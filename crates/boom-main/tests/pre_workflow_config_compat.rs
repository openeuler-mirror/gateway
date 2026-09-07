use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::fs;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const LEGACY_CONFIG: &str = include_str!("fixtures/pre_workflow_config.yaml");
const MASTER_KEY: &str = "legacy-master-key";

struct GatewayProcess(Child);

impl GatewayProcess {
    fn spawn(config_path: &PathBuf, port: u16) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_boom-gateway"))
            .arg("--config")
            .arg(config_path)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to start boom-gateway");
        Self(child)
    }

    fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.0.try_wait().expect("failed to poll boom-gateway")
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct TempConfig {
    path: PathBuf,
}

impl TempConfig {
    fn create(upstream_port: u16) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "boom-gateway-pre-workflow-config-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("failed to create temporary config directory");
        let path = directory.join("config.yaml");
        let content = LEGACY_CONFIG.replace("__UPSTREAM_PORT__", &upstream_port.to_string());
        fs::write(&path, content).expect("failed to write temporary config");
        Self { path }
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        if let Some(directory) = self.path.parent() {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

async fn mock_chat(Json(request): Json<Value>) -> Json<Value> {
    assert_eq!(request["model"], "legacy-upstream-model");
    Json(json!({
        "id": "chatcmpl-legacy-compat",
        "object": "chat.completion",
        "created": 1,
        "model": "legacy-upstream-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "legacy config works"
            },
            "finish_reason": "stop",
            "logprobs": null
        }],
        "usage": {
            "prompt_tokens": 3,
            "completion_tokens": 4,
            "total_tokens": 7
        }
    }))
}

async fn start_mock_upstream() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind mock upstream");
    let address = listener
        .local_addr()
        .expect("failed to read mock upstream address");
    let app = Router::new().route("/v1/chat/completions", post(mock_chat));
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock upstream failed");
    });
    (address, task)
}

fn unused_port() -> u16 {
    let listener =
        StdTcpListener::bind("127.0.0.1:0").expect("failed to reserve gateway test port");
    listener
        .local_addr()
        .expect("failed to read gateway test port")
        .port()
}

async fn wait_until_ready(client: &reqwest::Client, base_url: &str, gateway: &mut GatewayProcess) {
    for _ in 0..100 {
        if let Some(status) = gateway.try_wait() {
            panic!("boom-gateway exited before becoming ready: {status}");
        }
        if client
            .get(format!("{base_url}/health"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("boom-gateway did not become ready within 5 seconds");
}

#[tokio::test]
async fn pre_workflow_config_starts_and_serves_normal_chat_requests() {
    assert!(
        !LEGACY_CONFIG.contains("workflow_settings"),
        "fixture must represent a config written before workflow_settings existed"
    );

    let (upstream_address, upstream_task) = start_mock_upstream().await;
    let config = TempConfig::create(upstream_address.port());
    let parsed = boom_config::load_config(
        config
            .path
            .to_str()
            .expect("temporary config path is not valid UTF-8"),
    )
    .expect("pre-workflow config must still parse and validate");
    assert!(parsed.workflow_settings.models.is_empty());
    assert!(parsed.workflow_settings.workflows.is_empty());
    assert_eq!(
        parsed
            .router_settings
            .flow_control_queue_timeout_secs(),
        1200
    );

    let gateway_port = unused_port();
    let mut gateway = GatewayProcess::spawn(&config.path, gateway_port);
    let base_url = format!("http://127.0.0.1:{gateway_port}");
    let client = reqwest::Client::new();
    wait_until_ready(&client, &base_url, &mut gateway).await;

    let health: Value = client
        .get(format!("{base_url}/health"))
        .send()
        .await
        .expect("health request failed")
        .error_for_status()
        .expect("health request returned an error")
        .json()
        .await
        .expect("health response was not JSON");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["db_connected"], false);
    assert_eq!(health["models_count"], 1);

    let models: Value = client
        .get(format!("{base_url}/v1/models"))
        .bearer_auth(MASTER_KEY)
        .send()
        .await
        .expect("models request failed")
        .error_for_status()
        .expect("models request returned an error")
        .json()
        .await
        .expect("models response was not JSON");
    let model_ids = models["data"]
        .as_array()
        .expect("models response did not contain a data array")
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(model_ids, vec!["legacy-model"]);

    let response: Value = client
        .post(format!("{base_url}/v1/chat/completions"))
        .bearer_auth(MASTER_KEY)
        .json(&json!({
            "model": "legacy-model",
            "messages": [{"role": "user", "content": "compatibility check"}]
        }))
        .send()
        .await
        .expect("chat request failed")
        .error_for_status()
        .expect("chat request returned an error")
        .json()
        .await
        .expect("chat response was not JSON");
    assert_eq!(
        response["choices"][0]["message"]["content"],
        "legacy config works"
    );
    assert_eq!(response["usage"]["total_tokens"], 7);
    assert!(response.get("fusion_usage").is_none());

    upstream_task.abort();
}
