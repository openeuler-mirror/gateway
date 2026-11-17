//! 环境编排器：initdb 临时 Postgres + mock-backend + N 个网关实例的全生命周期管理。
//! 只做编排（进程/端口/探活/teardown），用例逻辑在 e2e-impl。

pub mod config;
pub mod fixture;
pub mod ports;
pub mod postgres;

use std::path::PathBuf;

use e2e_tools::{DbProbe, HttpTestClient, ManagedChild};
use postgres::PostgresInstance;

pub const QUOTA_SYNC_INTERVAL_SECS: u32 = 30;

/// 实例启动方式：Sequential = 逐个 spawn+探活（首实例先完成迁移）；
/// Concurrent = 全部 spawn 再探活（空库并发首启竞态语义，G-fault/G-fresh 用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootMode {
    Sequential,
    Concurrent,
}

#[derive(Clone, Debug)]
pub struct InstanceSpec {
    pub name: &'static str,
    pub instance_id: &'static str,
}

pub struct EnvParams {
    pub gateway_bin: PathBuf,
    pub mock_bin: PathBuf,
    pub master_key: String,
    pub model_name: String,
    pub instances: Vec<InstanceSpec>,
    /// quota 判定口径：true = shared（全局基数+本地增量）。seed 写 boom_config KV + bump。
    pub decision_base_shared: bool,
    /// G-base 顺序启动避开空库并发首启迁移竞态（已知产品遗留，G-fresh 用例的靶子）
    pub boot_mode: BootMode,
    /// mock 响应长度区间（min==max 时 completion tokens 逐请求确定，精确剧本用）
    pub mock_min_chars: u32,
    pub mock_max_chars: u32,
}

pub struct GatewayHandle {
    pub name: String,
    pub instance_id: String,
    pub port: u16,
    pub base_url: String,
    pub config_path: PathBuf,
    pub child: ManagedChild,
}

impl GatewayHandle {
    pub fn client_with(&self, key: &str) -> HttpTestClient {
        HttpTestClient::new(&self.base_url, key)
    }
}

pub struct MockHandle {
    pub port: u16,
    pub base_url: String,
    pub child: ManagedChild,
}

pub struct TestEnv {
    pub run_id: String,
    pub root: PathBuf,
    pub dsn: String,
    pub master_key: String,
    pub model_name: String,
    pub pg: PostgresInstance,
    pub mock: MockHandle,
    pub gateways: Vec<GatewayHandle>,
    pub db: DbProbe,
}

impl TestEnv {
    pub fn gateway(&self, name: &str) -> &GatewayHandle {
        self.gateways
            .iter()
            .find(|g| g.name == name)
            .unwrap_or_else(|| panic!("环境里没有网关实例 {name}"))
    }

    pub fn gateway_mut(&mut self, name: &str) -> &mut GatewayHandle {
        self.gateways
            .iter_mut()
            .find(|g| g.name == name)
            .unwrap_or_else(|| panic!("环境里没有网关实例 {name}"))
    }

    /// teardown：杀网关/mock（进程组）、pg_ctl stop immediate。目录保留供取证，clean 命令统一删。
    pub fn teardown(mut self) {
        for gw in &mut self.gateways {
            let _ = gw.child.kill9();
        }
        let _ = self.mock.child.kill9();
        self.pg.stop_immediate();
        for gw in &mut self.gateways {
            let _ = gw.child.try_wait();
        }
        let _ = self.mock.child.try_wait();
    }
}

/// 拉起一套环境：postgres → mock → 生成 config → 并发 spawn 网关 → 探活 → fixture。
pub async fn start_env(root: &std::path::Path, params: &EnvParams) -> anyhow::Result<TestEnv> {
    let run_id = ports::alloc_run_id();
    let run_root = root.join("runs").join(&run_id);
    std::fs::create_dir_all(run_root.join("logs"))?;
    std::fs::create_dir_all(run_root.join("configs"))?;

    // 1. postgres
    let pg = PostgresInstance::init(&run_root)?;
    pg.start()?;

    // 2. mock backend
    let mock_port = ports::alloc_tcp();
    let mock_log = run_root.join("logs").join("mock.log");
    let mock_child = ManagedChild::spawn(
        "mock",
        params.mock_bin.to_str().expect("mock bin path utf8"),
        &[
            "--bind",
            &format!("127.0.0.1:{mock_port}"),
            "--min-chars",
            &params.mock_min_chars.to_string(),
            "--max-chars",
            &params.mock_max_chars.to_string(),
        ],
        &mock_log,
        &[],
    )?;
    let mock_base = format!("http://127.0.0.1:{mock_port}");
    // OpenAI provider 拼 {api_base}/chat/completions，mock 的端点在 /v1/ 下
    let mock_api_base = format!("{mock_base}/v1");

    // 3. 生成每实例 config + spawn + 探活。
    //    Sequential：逐实例 spawn→探活（首实例先完成迁移，避开空库并发首启竞态）。
    //    Concurrent：全部 spawn 后统一探活（G-fresh 竞态语义）。
    let mut gateways = Vec::new();
    for (idx, inst) in params.instances.iter().enumerate() {
        let port = ports::alloc_tcp();
        let cfg_path = run_root.join("configs").join(format!("{}.yaml", inst.name));
        config::render_to_file(
            &cfg_path,
            &config::RenderConfig {
                instance_id: inst.instance_id,
                port,
                dsn: pg.dsn.clone(),
                master_key: params.master_key.clone(),
                model_name: params.model_name.clone(),
                mock_url: mock_api_base.clone(),
                quota_sync_interval_secs: QUOTA_SYNC_INTERVAL_SECS,
            },
        )?;
        let log = run_root.join("logs").join(format!("{}.log", inst.name));
        let child = ManagedChild::spawn(
            inst.name,
            params.gateway_bin.to_str().expect("gateway bin path utf8"),
            &["--config", cfg_path.to_str().expect("config path utf8")],
            &log,
            &[],
        )?;
        let handle = GatewayHandle {
            name: inst.name.to_string(),
            instance_id: inst.instance_id.to_string(),
            port,
            base_url: format!("http://127.0.0.1:{port}"),
            config_path: cfg_path,
            child,
        };

        if params.boot_mode == BootMode::Sequential {
            probe_health(&handle.base_url, &handle.instance_id, &handle.name)
                .await
                .map_err(|e| anyhow::anyhow!("网关 {} 探活失败: {e}", handle.name))?;
            gateways.push(handle);
        } else {
            gateways.push(handle);
            if idx + 1 == params.instances.len() {
                for gw in &gateways {
                    probe_health(&gw.base_url, &gw.instance_id, &gw.name)
                        .await
                        .map_err(|e| anyhow::anyhow!("网关 {} 探活失败: {e}", gw.name))?;
                }
            }
        }
    }

    // 5. fixture（keys/plans）+ 只读探针
    fixture::seed(&pg, &run_root, params)?;
    let db = DbProbe::connect(&pg.dsn).await?;

    Ok(TestEnv {
        run_id,
        root: run_root,
        dsn: pg.dsn.clone(),
        master_key: params.master_key.clone(),
        model_name: params.model_name.clone(),
        pg,
        mock: MockHandle {
            port: mock_port,
            base_url: mock_base,
            child: mock_child,
        },
        gateways,
        db,
    })
}

async fn probe_health(base_url: &str, instance_id: &str, name: &str) -> anyhow::Result<()> {
    let client = health_client();
    let timeout = std::time::Duration::from_secs(60);
    let step = std::time::Duration::from_millis(500);
    e2e_tools::poll::poll_until(timeout, step, || {
        let url = format!("{base_url}/health");
        let client = client.clone();
        let instance_id = instance_id.to_string();
        Box::pin(async move {
            let resp = client.get(&url).send().await?;
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            if status == 200 && body.contains(&instance_id) {
                Ok(Some(()))
            } else {
                Ok(None)
            }
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("{name}: {e}"))
}

fn health_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("reqwest client")
}
