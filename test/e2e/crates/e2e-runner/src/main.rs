//! e2e-runner：L4 套件入口。doctor / run / bind-check / clean / report。
//! 命令契约以 test/e2e/CLAUDE.md 为准。

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use e2e_impl::{all_cases, CaseEntry, EnvGroup};

#[derive(Parser)]
#[command(name = "e2e-runner")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// 环境自检：postgres 工具链、rust、网关/mock 编译
    Doctor,
    /// 跑套件或单用例
    Run {
        #[arg(long)]
        suite: Option<String>,
        #[arg(long = "case")]
        case: Vec<String>,
        /// 跑完保留环境进程现场（PG/网关/mock 不回收），供失败后 psql 取证；之后用 clean 清
        #[arg(long, default_value_t = false)]
        keep_env: bool,
    },
    /// 台账 ↔ 实现绑定双向校验
    BindCheck,
    /// 清残留进程与 runs/ 目录
    Clean,
    /// 查看某次运行的报告
    Report { run_id: String },
}

/// 仓库根：runner crate 目录上四级（crates/e2e-runner → e2e → test → gateway）
fn repo_root() -> PathBuf {
    if let Ok(p) = std::env::var("E2E_REPO_ROOT") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()
        .expect("定位仓库根失败")
}

/// test/e2e 根
fn e2e_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("定位 test/e2e 失败")
}

fn gateway_bin() -> PathBuf {
    repo_root().join("target/debug/boom-gateway")
}

fn mock_bin() -> PathBuf {
    repo_root()
        .join("test/mock-backend/target/debug/mock-backend")
}

fn build_gateway() -> anyhow::Result<()> {
    let manifest = repo_root().join("Cargo.toml");
    let st = std::process::Command::new("cargo")
        .arg("build")
        .arg("--manifest-path").arg(&manifest)
        .arg("--bin").arg("boom-gateway")
        .status()?;
    if !st.success() {
        anyhow::bail!("cargo build boom-gateway 失败");
    }
    Ok(())
}

fn build_mock() -> anyhow::Result<()> {
    let manifest = repo_root().join("test/mock-backend/Cargo.toml");
    if !manifest.exists() {
        anyhow::bail!("找不到 {}", manifest.display());
    }
    let st = std::process::Command::new("cargo")
        .arg("build")
        .arg("--manifest-path").arg(&manifest)
        .status()?;
    if !st.success() {
        anyhow::bail!("cargo build mock-backend 失败");
    }
    Ok(())
}

fn doctor() -> anyhow::Result<()> {
    println!("== e2e doctor ==");
    println!("repo root: {}", repo_root().display());

    match e2e_envs::postgres::find_pg_bin_dir() {
        Ok(dir) => println!("[ok] postgres 工具链: {}", dir.display()),
        Err(e) => {
            println!("[fail] postgres 工具链: {e}");
            anyhow::bail!("先安装 postgresql（见提示）再重试");
        }
    }

    if std::env::var("PATH").unwrap_or_default().contains("cargo") || which_cargo() {
        println!("[ok] cargo");
    } else {
        anyhow::bail!("cargo 不可用，先装 rustup");
    }

    println!("[..] 编译网关（首次较慢）…");
    build_gateway()?;
    println!("[ok] 网关二进制: {}", gateway_bin().display());

    println!("[..] 编译 mock-backend…");
    build_mock()?;
    println!("[ok] mock-backend: {}", mock_bin().display());

    println!("\n全部通过。跑冒烟：cargo run -p e2e-runner -- run --suite smoke");
    Ok(())
}

fn which_cargo() -> bool {
    std::process::Command::new("cargo")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------- 台账 ----------

#[derive(Debug, Clone)]
struct CaseMeta {
    id: String,
    status: String,
    decision_base_shared: bool,
}

fn scan_ledger(cases_dir: &Path) -> anyhow::Result<Vec<CaseMeta>> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<CaseMeta>) -> anyhow::Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let p = e.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if p.extension().map(|x| x == "md").unwrap_or(false) {
                let text = std::fs::read_to_string(&p)?;
                if let Some(fm) = frontmatter(&text) {
                    if let (Some(id), Some(status)) = (
                        yaml_str(&fm, "id"),
                        yaml_str(&fm, "status"),
                    ) {
                        out.push(CaseMeta {
                            id,
                            status,
                            decision_base_shared: fm.contains("decision_base_shared: true"),
                        });
                    } else {
                        anyhow::bail!("{} frontmatter 缺 id/status", p.display());
                    }
                }
            }
        }
        Ok(())
    }
    walk(cases_dir, &mut out)?;
    Ok(out)
}

fn frontmatter(text: &str) -> Option<String> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(rest[..end].to_string())
}

fn yaml_str(fm: &str, key: &str) -> Option<String> {
    for line in fm.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix(&format!("{key}:")) {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn bind_check() -> anyhow::Result<()> {
    let ledger = scan_ledger(&e2e_root().join("cases"))?;
    let impls: Vec<&CaseEntry> = all_cases().into_iter().collect();
    let mut problems = Vec::new();

    for m in &ledger {
        let implemented = impls.iter().any(|c| c.id == m.id);
        let needs_impl = m.status == "automated" || m.status == "specified";
        match (needs_impl, implemented) {
            (true, false) => problems.push(format!("{}: status={} 但无实现", m.id, m.status)),
            (false, true) => problems.push(format!(
                "{}: status={} 未要求实现却已实现（应转 automated）",
                m.id, m.status
            )),
            _ => {}
        }
        if let Some(c) = impls.iter().find(|c| c.id == m.id) {
            if c.decision_base_shared != m.decision_base_shared {
                problems.push(format!(
                    "{}: decision_base_shared 台账={} 实现={}",
                    m.id, m.decision_base_shared, c.decision_base_shared
                ));
            }
        }
    }
    for c in &impls {
        if !ledger.iter().any(|m| m.id == c.id) {
            problems.push(format!("{}: 实现引用的 ID 不在台账（孤儿实现）", c.id));
        }
    }

    if problems.is_empty() {
        println!(
            "bind-check 通过：台账 {} 条，实现 {} 条",
            ledger.len(),
            impls.len()
        );
        Ok(())
    } else {
        for p in &problems {
            println!("[fail] {p}");
        }
        anyhow::bail!("绑定校验失败 {} 项", problems.len());
    }
}

// ---------- run ----------

struct CaseOutcome {
    id: String,
    title: String,
    severity: String,
    group: EnvGroup,
    status: &'static str,
    secs: f64,
    evidence: Vec<String>,
    error: Option<String>,
}

const SMOKE: &[&str] = &["MI-001", "MI-002", "MI-004", "MI-005"];

async fn run(cmd: RunArgs) -> anyhow::Result<()> {
    // 有 cargo 则总是增量构建（秒级跳过无变化，杜绝旧二进制与源码行为漂移）；
    // 无 cargo 但二进制齐全时跳过构建继续（现场二进制直接跑的部署形态）
    if which_cargo() {
        println!("[..] 构建网关与 mock（增量）…");
        build_gateway()?;
        build_mock()?;
    } else if gateway_bin().exists() && mock_bin().exists() {
        println!("[warn] cargo 不可用，跳过构建，使用现有二进制：");
        println!("  网关: {}", gateway_bin().display());
        println!("  mock: {}", mock_bin().display());
    } else {
        anyhow::bail!(
            "cargo 不可用且二进制缺失: {} / {}",
            gateway_bin().display(),
            mock_bin().display()
        );
    }

    // 选择用例：--case 显式；否则 --suite（smoke 过滤注册表交集；nightly 全部已实现）
    let wanted: Vec<String> = if !cmd.case.is_empty() {
        cmd.case.clone()
    } else if cmd.suite.as_deref() == Some("smoke") {
        SMOKE.iter().map(|s| s.to_string()).collect()
    } else {
        // nightly / 缺省：全部已实现
        all_cases().iter().map(|c| c.id.to_string()).collect()
    };

    let selected: Vec<&CaseEntry> = all_cases()
        .into_iter()
        .filter(|c| wanted.iter().any(|w| w == c.id))
        .collect();
    let missing: Vec<String> = wanted
        .iter()
        .filter(|w| !all_cases().iter().any(|c| c.id == **w))
        .cloned()
        .collect();
    if !missing.is_empty() {
        anyhow::bail!("注册表中没有这些用例（台账 draft 或未实现）: {missing:?}");
    }
    if selected.is_empty() {
        println!("没有可执行的用例（当前注册表为空）");
        return Ok(());
    }

    // 按 (环境组, 判定口径) 分组：同组同口径共享一个环境串行执行
    let mut outcomes = Vec::new();
    for group in [EnvGroup::GBase, EnvGroup::GFresh, EnvGroup::GFault] {
        for shared in [false, true] {
            let group_cases: Vec<&&CaseEntry> = selected
                .iter()
                .filter(|c| c.env_group == group && c.decision_base_shared == shared)
                .collect();
            if group_cases.is_empty() {
                continue;
            }
            println!("== 起环境（{group:?} shared={shared}，{} 用例）==", group_cases.len());
            let env = start_group_env(group, shared).await?;
        for entry in group_cases {
            println!("[..] {}", entry.id);
            let t0 = Instant::now();
            let res = (entry.run)(&env).await;
            let secs = t0.elapsed().as_secs_f64();
            let outcome = match res {
                Ok(ev) => CaseOutcome {
                    id: entry.id.to_string(),
                    title: entry.title.to_string(),
                    severity: entry.severity.to_string(),
                    group,
                    status: "PASS",
                    secs,
                    evidence: ev,
                    error: None,
                },
                Err(e) => CaseOutcome {
                    id: entry.id.to_string(),
                    title: entry.title.to_string(),
                    severity: entry.severity.to_string(),
                    group,
                    status: "FAIL",
                    secs,
                    evidence: Vec::new(),
                    error: Some(format!("{e:#}")),
                },
            };
            println!("    => {} ({:.1}s)", outcome.status, outcome.secs);
            if let Some(err) = &outcome.error {
                for line in err.split('\n') {
                    println!("    | {line}");
                }
            }
            outcomes.push(outcome);
        }
        if cmd.keep_env {
            println!(
                "--keep-env：环境保留（取证后 clean 回收）\n  DSN: {}\n  日志: {}\n  配置: {}",
                env.dsn,
                env.root.join("logs").display(),
                env.root.join("configs").display()
            );
            // drop 会杀子进程（kill_on_drop）+ 停 PG（PostgresInstance::Drop）——现场必须泄漏
            std::mem::forget(env);
        } else {
            env.teardown();
        }
        }
    }

    write_report(&cmd, &outcomes)?;
    Ok(())
}

struct RunArgs {
    suite: Option<String>,
    case: Vec<String>,
    keep_env: bool,
}

async fn start_group_env(
    group: EnvGroup,
    decision_base_shared: bool,
) -> anyhow::Result<e2e_envs::TestEnv> {
    let root = e2e_root();
    let params = e2e_envs::EnvParams {
        gateway_bin: gateway_bin(),
        mock_bin: mock_bin(),
        master_key: "sk-e2e-master".to_string(),
        model_name: "e2e-model".to_string(),
        instances: vec![
            e2e_envs::InstanceSpec { name: "gw-a", instance_id: "gw-a" },
            e2e_envs::InstanceSpec { name: "gw-b", instance_id: "gw-b" },
        ],
        decision_base_shared,
        // G-fresh 特意并发首启（空库迁移竞态是用例靶子）；其余组顺序起，首实例先完成迁移
        boot_mode: match group {
            EnvGroup::GFresh => e2e_envs::BootMode::Concurrent,
            _ => e2e_envs::BootMode::Sequential,
        },
        // 定长响应：completion tokens 逐请求固定（240/4=60），精确剧本（MI-002 等）可做确定算术
        mock_min_chars: 240,
        mock_max_chars: 240,
    };
    e2e_envs::start_env(&root, &params).await
}

fn write_report(_cmd: &RunArgs, outcomes: &[CaseOutcome]) -> anyhow::Result<()> {
    let root = e2e_root();
    let report_dir = root.join("reports");
    std::fs::create_dir_all(&report_dir)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = report_dir.join(format!("report-{now}.md"));

    let mut md = String::new();
    md.push_str("# E2E 多实例运行报告\n\n");
    md.push_str(&format!("- 时间戳: {now}\n- 用例数: {}\n\n", outcomes.len()));

    md.push_str("## 结果\n\n| ID | 标题 | 级别 | 状态 | 时长 | 环境 |\n|---|---|---|---|---|---|\n");
    for o in outcomes {
        md.push_str(&format!(
            "| {} | {} | {} | {} | {:.1}s | {:?} |\n",
            o.id, o.title, o.severity, o.status, o.secs, o.group
        ));
    }
    let pass = outcomes.iter().filter(|o| o.status == "PASS").count();
    let fail = outcomes.len() - pass;
    md.push_str(&format!("\n**PASS {pass} / FAIL {fail}**\n\n"));

    md.push_str("## 证据\n\n");
    for o in outcomes {
        md.push_str(&format!("### {} {} ({:.1}s)\n\n", o.status, o.id, o.secs));
        for line in &o.evidence {
            md.push_str(&format!("- {line}\n"));
        }
        if let Some(err) = &o.error {
            md.push_str(&format!("\n```\n{err}\n```\n\n"));
        }
    }

    std::fs::write(&path, md)?;
    println!("报告: {}", path.display());
    if fail > 0 {
        anyhow::bail!("有 {fail} 个用例失败");
    }
    Ok(())
}

fn clean() -> anyhow::Result<()> {
    let root = e2e_root();
    // 1. 杀命令行含 runs/ 路径的残留进程（网关/mock 均以该路径作为参数启动）
    let runs = root.join("runs");
    let mut killed = 0;
    if let Ok(out) = std::process::Command::new("ps").arg("-eo").arg("pid=,command=").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let line = line.trim_start();
            let Some((pid, cmdline)) = line.split_once(' ') else { continue };
            let cmdline = cmdline.trim();
            if cmdline.contains(runs.to_str().unwrap_or("\u{0}nope")) {
                let pid: i32 = pid.trim().parse().unwrap_or(0);
                if pid > 1 {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    killed += 1;
                }
            }
        }
    }
    println!("清理进程: {killed} 个");
    // 2. pg_ctl stop 各 run 的 pg 实例 + 删目录
    if runs.exists() {
        for entry in std::fs::read_dir(&runs)? {
            let entry = entry?;
            let pg_data = entry.path().join("pg/data");
            if pg_data.exists() {
                if let Ok(dir) = e2e_envs::postgres::find_pg_bin_dir() {
                    let _ = std::process::Command::new(dir.join("pg_ctl"))
                        .arg("stop").arg("-D").arg(&pg_data)
                        .arg("-m").arg("immediate").arg("-t").arg("3")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        }
        std::fs::remove_dir_all(&runs)?;
        println!("已删除 {}", runs.display());
    }
    Ok(())
}

fn report(run_id: String) -> anyhow::Result<()> {
    let path = e2e_root().join("reports").join(&run_id);
    let text = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        let p2 = e2e_root().join("reports").join(format!("report-{run_id}.md"));
        std::fs::read_to_string(&p2)?
    };
    println!("{text}");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Doctor => doctor(),
        Cmd::BindCheck => bind_check(),
        Cmd::Clean => clean(),
        Cmd::Report { run_id } => report(run_id),
        Cmd::Run { suite, case, keep_env } => {
            run(RunArgs { suite, case, keep_env }).await
        }
    }
}
