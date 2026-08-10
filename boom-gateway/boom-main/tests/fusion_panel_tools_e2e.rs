use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures::{stream, StreamExt};
use serde_json::{json, Value};
use std::fs;
use std::io;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const MASTER_KEY: &str = "fusion-e2e-master-key";

#[derive(Clone, Copy)]
enum MockBehavior {
    Success {
        model: &'static str,
        content: &'static str,
    },
    Fail {
        status: u16,
        message: &'static str,
    },
    FailOnceThenSuccess {
        model: &'static str,
        content: &'static str,
    },
    Invalid {
        model: &'static str,
    },
    ToolAggregator,
    StreamingError,
    StreamingSlowBeforeFirst,
    StreamingSlowAfterFirst,
    DelayedPanel,
}

struct MockState {
    behavior: MockBehavior,
    calls: Arc<Mutex<Vec<Value>>>,
    attempts: AtomicUsize,
    disconnects: Arc<AtomicUsize>,
}

struct MockUpstream {
    address: SocketAddr,
    calls: Arc<Mutex<Vec<Value>>>,
    disconnects: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl MockUpstream {
    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn requests(&self) -> Vec<Value> {
        self.calls.lock().unwrap().clone()
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct DropCountStream {
    inner: Pin<Box<dyn futures::Stream<Item = Result<Bytes, io::Error>> + Send>>,
    disconnects: Arc<AtomicUsize>,
}

impl futures::Stream for DropCountStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(context)
    }
}

impl Drop for DropCountStream {
    fn drop(&mut self) {
        self.disconnects.fetch_add(1, Ordering::SeqCst);
    }
}

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
    prompt_log_dir: PathBuf,
}

impl TempConfig {
    fn create_with_options(
        panel_a: SocketAddr,
        panel_b: SocketAddr,
        aggregator: SocketAddr,
        panel_timeout_secs: Option<u64>,
    ) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "boom-gateway-fusion-e2e-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("failed to create temporary config directory");
        let path = directory.join("config.yaml");
        let prompt_log_dir = directory.join("prompt-logs");
        let panel_timeout = panel_timeout_secs
            .map(|seconds| format!("      panel_timeout_secs: {seconds}\n"))
            .unwrap_or_default();
        let content = format!(
            r#"
model_list:
  - model_name: panel-a
    model_info:
      id: panel-a-deployment
    litellm_params:
      model: openai/panel-a-upstream
      api_key: mock-key
      api_base: http://{panel_a}/v1
      timeout: 30
  - model_name: panel-b
    model_info:
      id: panel-b-deployment
    litellm_params:
      model: openai/panel-b-upstream
      api_key: mock-key
      api_base: http://{panel_b}/v1
      timeout: 30
  - model_name: aggregator
    model_info:
      id: aggregator-deployment
    litellm_params:
      model: openai/aggregator-upstream
      api_key: mock-key
      api_base: http://{aggregator}/v1
      timeout: 60

workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: panel-a
            temperature: 0.3
          - model: panel-b
            temperature: 0.5
        aggregator:
          model: aggregator
          temperature: 0
{panel_timeout}

general_settings:
  master_key: {MASTER_KEY}

router_settings:
  routing_strategy: round_robin

server:
  host: 127.0.0.1
  port: 4000
  workers: 1

rate_limit:
  enabled: false

prompt_log:
  enabled: true
  dir: {prompt_log_dir}
  max_file_size_mb: 10

deployment_health_check:
  auto_offline_enabled: false
  auto_recovery_enabled: false
  request_failure_auto_offline_enabled: false
"#,
            panel_a = panel_a,
            panel_b = panel_b,
            aggregator = aggregator,
            prompt_log_dir = prompt_log_dir.display(),
        );
        fs::write(&path, content).expect("failed to write temporary config");
        Self {
            path,
            prompt_log_dir,
        }
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        if let Some(directory) = self.path.parent() {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

struct TestGateway {
    process: GatewayProcess,
    config: TempConfig,
    client: reqwest::Client,
    base_url: String,
}

impl TestGateway {
    async fn start(
        panel_a: &MockUpstream,
        panel_b: &MockUpstream,
        aggregator: &MockUpstream,
    ) -> Self {
        Self::start_with_timeout(panel_a, panel_b, aggregator, None).await
    }

    async fn start_with_timeout(
        panel_a: &MockUpstream,
        panel_b: &MockUpstream,
        aggregator: &MockUpstream,
        panel_timeout_secs: Option<u64>,
    ) -> Self {
        let config = TempConfig::create_with_options(
            panel_a.address,
            panel_b.address,
            aggregator.address,
            panel_timeout_secs,
        );
        let port = unused_port();
        let mut process = GatewayProcess::spawn(&config.path, port);
        let client = reqwest::Client::new();
        let base_url = format!("http://127.0.0.1:{port}");
        wait_until_ready(&client, &base_url, &mut process).await;
        Self {
            process,
            config,
            client,
            base_url,
        }
    }

    async fn post(&self, body: &Value) -> reqwest::Response {
        self.client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(MASTER_KEY)
            .json(body)
            .send()
            .await
            .expect("Fusion request failed")
    }
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        let _ = self.process.try_wait();
    }
}

async fn mock_chat(State(state): State<Arc<MockState>>, Json(request): Json<Value>) -> Response {
    state.calls.lock().unwrap().push(request);
    let attempt = state.attempts.fetch_add(1, Ordering::SeqCst) + 1;
    match state.behavior {
        MockBehavior::Success { model, content } => text_completion(model, content),
        MockBehavior::Fail { status, message } => (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({"error": {"message": message}})),
        )
            .into_response(),
        MockBehavior::FailOnceThenSuccess { .. } if attempt == 1 => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "temporary panel failure"}})),
        )
            .into_response(),
        MockBehavior::FailOnceThenSuccess { model, content } => text_completion(model, content),
        MockBehavior::Invalid { model } => Json(json!({
            "id": format!("chatcmpl-{model}"),
            "object": "chat.completion",
            "created": 1,
            "model": model,
            "choices": [],
            "usage": {
                "prompt_tokens": 2,
                "completion_tokens": 0,
                "total_tokens": 2
            }
        }))
        .into_response(),
        MockBehavior::ToolAggregator => Json(json!({
            "id": "chatcmpl-aggregator-tool",
            "object": "chat.completion",
            "created": 1,
            "model": "aggregator-upstream",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call-aggregator",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"date\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls",
                "logprobs": null
            }],
            "usage": {
                "prompt_tokens": 2,
                "completion_tokens": 1,
                "total_tokens": 3
            }
        }))
        .into_response(),
        MockBehavior::StreamingError => streaming_error(),
        MockBehavior::StreamingSlowBeforeFirst => streaming_slow_before_first(),
        MockBehavior::StreamingSlowAfterFirst => {
            streaming_slow_after_first(state.disconnects.clone())
        }
        MockBehavior::DelayedPanel => delayed_panel(state.disconnects.clone()),
    }
}

fn text_completion(model: &str, content: &str) -> Response {
    Json(json!({
        "id": format!("chatcmpl-{model}"),
        "object": "chat.completion",
        "created": 1,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
            "logprobs": null
        }],
        "usage": {
            "prompt_tokens": 2,
            "completion_tokens": 1,
            "total_tokens": 3
        }
    }))
    .into_response()
}

fn streaming_error() -> Response {
    let first = concat!(
        "data: {\"id\":\"chatcmpl-stream-error\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"aggregator-upstream\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"role\":\"assistant\",\"content\":\"partial aggregate\"},",
        "\"finish_reason\":null}]}\n\n"
    );
    let stream = stream::unfold(0, move |state| async move {
        match state {
            0 => Some((
                Ok::<Bytes, io::Error>(Bytes::from_static(first.as_bytes())),
                1,
            )),
            1 => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Some((
                    Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "mock aggregator stream reset",
                    )),
                    2,
                ))
            }
            _ => None,
        }
    });
    sse_response(Body::from_stream(stream))
}

fn streaming_slow_before_first() -> Response {
    let data = concat!(
        "data: {\"id\":\"chatcmpl-slow\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"aggregator-upstream\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"role\":\"assistant\",\"content\":\"late aggregate\"},",
        "\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,",
        "\"completion_tokens\":1,\"total_tokens\":3}}\n\n",
        "data: [DONE]\n\n"
    );
    let stream = stream::once(async move {
        tokio::time::sleep(Duration::from_secs(31)).await;
        Ok::<Bytes, io::Error>(Bytes::from_static(data.as_bytes()))
    });
    sse_response(Body::from_stream(stream))
}

fn streaming_slow_after_first(disconnects: Arc<AtomicUsize>) -> Response {
    let first = concat!(
        "data: {\"id\":\"chatcmpl-cancel\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"aggregator-upstream\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"role\":\"assistant\",\"content\":\"first aggregate chunk\"},",
        "\"finish_reason\":null}]}\n\n"
    );
    let second = concat!(
        "data: {\"id\":\"chatcmpl-cancel\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"aggregator-upstream\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"content\":\"late aggregate chunk\"},\"finish_reason\":\"stop\"}],",
        "\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4}}\n\n",
        "data: [DONE]\n\n"
    );
    let stream = stream::unfold(0, move |state| async move {
        match state {
            0 => Some((
                Ok::<Bytes, io::Error>(Bytes::from_static(first.as_bytes())),
                1,
            )),
            1 => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Some((
                    Ok::<Bytes, io::Error>(Bytes::from_static(second.as_bytes())),
                    2,
                ))
            }
            _ => None,
        }
    });
    let stream = DropCountStream {
        inner: Box::pin(stream),
        disconnects,
    };
    sse_response(Body::from_stream(stream))
}

fn delayed_panel(disconnects: Arc<AtomicUsize>) -> Response {
    let body = json!({
        "id": "chatcmpl-delayed-panel",
        "object": "chat.completion",
        "created": 1,
        "model": "panel-a-upstream",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "delayed panel answer"},
            "finish_reason": "stop",
            "logprobs": null
        }],
        "usage": {
            "prompt_tokens": 2,
            "completion_tokens": 1,
            "total_tokens": 3
        }
    })
    .to_string();
    let stream = stream::once(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        Ok::<Bytes, io::Error>(Bytes::from(body))
    });
    let stream = DropCountStream {
        inner: Box::pin(stream),
        disconnects,
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn sse_response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(body)
        .unwrap()
}

async fn start_mock(behavior: MockBehavior) -> MockUpstream {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind mock upstream");
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let disconnects = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(MockState {
        behavior,
        calls: calls.clone(),
        attempts: AtomicUsize::new(0),
        disconnects: disconnects.clone(),
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat))
        .with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock upstream failed");
    });
    MockUpstream {
        address,
        calls,
        disconnects,
        task,
    }
}

fn unused_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("failed to reserve gateway test port")
        .local_addr()
        .unwrap()
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

async fn wait_for_prompt_logs(config: &TempConfig, expected: usize) -> Vec<Value> {
    wait_for_prompt_logs_with_timeout(config, expected, Duration::from_secs(5)).await
}

async fn wait_for_prompt_logs_with_timeout(
    config: &TempConfig,
    expected: usize,
    timeout: Duration,
) -> Vec<Value> {
    let started = tokio::time::Instant::now();
    loop {
        let entries = read_prompt_logs(config);
        if entries.len() >= expected {
            return entries;
        }
        if started.elapsed() >= timeout {
            panic!(
                "prompt log did not contain {expected} entries within {} ms",
                timeout.as_millis()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn read_prompt_logs(config: &TempConfig) -> Vec<Value> {
    let path = config
        .prompt_log_dir
        .join("_no_team")
        .join("master")
        .join("log_000001.jsonl");
    fs::read_to_string(path)
        .ok()
        .into_iter()
        .flat_map(|content| {
            content
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn fusion_request(message: &str, stream: bool, tools: Option<Vec<Value>>) -> Value {
    let mut body = json!({
        "model": "fusion",
        "messages": [{"role": "user", "content": message}],
        "stream": stream
    });
    if let Some(tools) = tools {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = Value::String("auto".to_string());
    }
    body
}

fn bash_tools() -> Vec<Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": "bash",
            "parameters": {"type": "object"}
        }
    })]
}

fn sse_json_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("SSE data event was not JSON"))
        .collect()
}

fn fusion_calls(prompt_log: &Value) -> &[Value] {
    prompt_log["fusion"]["calls"]
        .as_array()
        .expect("prompt log fusion.calls must be an array")
}

fn role_call<'a>(calls: &'a [Value], role: &str) -> &'a Value {
    calls
        .iter()
        .find(|call| call["role"] == role)
        .unwrap_or_else(|| panic!("prompt trace did not contain role {role}"))
}

async fn admin_session_cookie(client: &reqwest::Client, base_url: &str) -> String {
    let response = client
        .post(format!("{base_url}/dashboard/api/auth/login"))
        .json(&json!({
            "user_id": "admin",
            "api_key": MASTER_KEY
        }))
        .send()
        .await
        .expect("admin login request failed");
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .expect("admin login response did not set a session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn wait_for_disconnect(upstream: &MockUpstream) {
    for _ in 0..100 {
        if upstream.disconnects.load(Ordering::SeqCst) > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("Gateway did not close the mock upstream response body");
}

#[tokio::test]
async fn tools_reach_every_child_and_prompt_log_is_queryable_by_request_id() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "panel A answer",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Success {
        model: "panel-b-upstream",
        content: "panel B answer",
    })
    .await;
    let aggregator = start_mock(MockBehavior::ToolAggregator).await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;

    let response = gateway
        .post(&fusion_request(
            "use a tool after synthesis",
            false,
            Some(bash_tools()),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"command\":\"date\"}"
    );
    assert!(body.get("fusion_diagnostics").is_none());

    for upstream in [&panel_a, &panel_b, &aggregator] {
        let requests = upstream.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(requests[0]["stream"], false);
    }

    let logs = wait_for_prompt_logs(&gateway.config, 1).await;
    let calls = fusion_calls(&logs[0]);
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|call| {
        call["status"] == "succeeded"
            && call["duration_ms"].as_u64().is_some()
            && call["request"]["tools"].as_array().map(Vec::len) == Some(1)
            && !call["response"].is_null()
    }));

    let request_id = logs[0]["request_id"].as_str().unwrap();
    let unauthenticated = gateway
        .client
        .get(format!(
            "{}/dashboard/api/admin/prompt-log/entry/{request_id}",
            gateway.base_url
        ))
        .query(&[("key_hash", "master")])
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let cookie = admin_session_cookie(&gateway.client, &gateway.base_url).await;
    let api_response = gateway
        .client
        .get(format!(
            "{}/dashboard/api/admin/prompt-log/entry/{request_id}",
            gateway.base_url
        ))
        .header(reqwest::header::COOKIE, cookie)
        .query(&[("key_hash", "master")])
        .send()
        .await
        .unwrap();
    assert_eq!(api_response.status(), StatusCode::OK);
    let api_log: Value = api_response.json().await.unwrap();
    assert_eq!(api_log["request_id"], request_id);
    assert_eq!(fusion_calls(&api_log).len(), 3);
}

#[tokio::test]
async fn zero_valid_panels_retry_the_entire_panel_once_then_aggregate() {
    let panel_a = start_mock(MockBehavior::FailOnceThenSuccess {
        model: "panel-a-upstream",
        content: "panel A recovered",
    })
    .await;
    let panel_b = start_mock(MockBehavior::FailOnceThenSuccess {
        model: "panel-b-upstream",
        content: "panel B recovered",
    })
    .await;
    let aggregator = start_mock(MockBehavior::Success {
        model: "aggregator-upstream",
        content: "aggregated answer",
    })
    .await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;

    let response = gateway
        .post(&fusion_request("retry every panel", false, None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "aggregated answer"
    );
    assert_eq!(panel_a.call_count(), 2);
    assert_eq!(panel_b.call_count(), 2);
    assert_eq!(aggregator.call_count(), 1);

    let logs = wait_for_prompt_logs(&gateway.config, 1).await;
    let calls = fusion_calls(&logs[0]);
    assert_eq!(calls.len(), 5);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["status"] == "failed")
            .count(),
        2
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["status"] == "succeeded")
            .count(),
        3
    );
}

#[tokio::test]
async fn one_valid_panel_with_tools_returns_directly_without_retry_or_aggregator() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "plain panel answer",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Fail {
        status: 503,
        message: "panel B unavailable",
    })
    .await;
    let aggregator = start_mock(MockBehavior::Success {
        model: "aggregator-upstream",
        content: "must not be called",
    })
    .await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;

    let response = gateway
        .post(&fusion_request(
            "one panel is enough when tools are available",
            false,
            Some(bash_tools()),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "panel-a-upstream");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "plain panel answer"
    );
    assert_eq!(panel_a.call_count(), 1);
    assert_eq!(panel_b.call_count(), 1);
    assert_eq!(aggregator.call_count(), 0);

    let logs = wait_for_prompt_logs(&gateway.config, 1).await;
    let calls = fusion_calls(&logs[0]);
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["status"] == "failed")
            .count(),
        1
    );
}

#[tokio::test]
async fn one_valid_panel_without_tools_is_a_detailed_error_without_retry() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "only valid answer",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Fail {
        status: 503,
        message: "panel B unavailable",
    })
    .await;
    let aggregator = start_mock(MockBehavior::Success {
        model: "aggregator-upstream",
        content: "must not be called",
    })
    .await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;

    let response = gateway
        .post(&fusion_request("quorum is required", false, None))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body: Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert_eq!(body["error"]["type"], "provider_error");
    assert!(message.contains("only 1 valid panel answer"));
    assert!(message.contains("panel[1]"));
    assert!(message.contains("upstream_status=503"));
    assert_eq!(panel_a.call_count(), 1);
    assert_eq!(panel_b.call_count(), 1);
    assert_eq!(aggregator.call_count(), 0);

    let logs = wait_for_prompt_logs(&gateway.config, 1).await;
    assert_eq!(logs[0]["status_code"], 502);
    assert_eq!(fusion_calls(&logs[0]).len(), 2);
}

#[tokio::test]
async fn all_panel_failures_are_detailed_for_json_and_sse() {
    let panel_a = start_mock(MockBehavior::Fail {
        status: 503,
        message: "panel A unavailable",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Invalid {
        model: "panel-b-upstream",
    })
    .await;
    let aggregator = start_mock(MockBehavior::Success {
        model: "aggregator-upstream",
        content: "must not be called",
    })
    .await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;

    let json_response = gateway
        .post(&fusion_request("all panels fail json", false, None))
        .await;
    assert_eq!(json_response.status(), StatusCode::BAD_GATEWAY);
    let json_body: Value = json_response.json().await.unwrap();
    let message = json_body["error"]["message"].as_str().unwrap();
    assert!(message.contains("after 2 attempt(s)"));
    assert!(message.contains("panel[0]"));
    assert!(message.contains("upstream_status=503"));
    assert!(message.contains("panel[1]"));
    assert!(message.contains("invalid_response"));

    let stream_response = gateway
        .post(&fusion_request("all panels fail stream", true, None))
        .await;
    assert_eq!(stream_response.status(), StatusCode::OK);
    let stream_body = stream_response.text().await.unwrap();
    let error = sse_json_events(&stream_body)
        .into_iter()
        .find(|event| event["error"]["type"] == "provider_error")
        .expect("stream did not contain a provider_error event");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("invalid_response"));

    assert_eq!(panel_a.call_count(), 4);
    assert_eq!(panel_b.call_count(), 4);
    assert_eq!(aggregator.call_count(), 0);
    let logs = wait_for_prompt_logs(&gateway.config, 2).await;
    assert_eq!(
        logs.iter()
            .map(fusion_calls)
            .map(<[Value]>::len)
            .sum::<usize>(),
        8
    );
}

#[tokio::test]
async fn aggregator_header_failure_falls_back_for_json_and_stream() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "panel A fallback",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Success {
        model: "panel-b-upstream",
        content: "panel B answer",
    })
    .await;
    let aggregator = start_mock(MockBehavior::Fail {
        status: 502,
        message: "aggregator unavailable",
    })
    .await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;

    let json_response = gateway
        .post(&fusion_request("fallback json", false, None))
        .await;
    assert_eq!(json_response.status(), StatusCode::OK);
    let json_body: Value = json_response.json().await.unwrap();
    assert_eq!(json_body["model"], "panel-a-upstream");

    let stream_response = gateway
        .post(&fusion_request("fallback stream", true, None))
        .await;
    assert_eq!(stream_response.status(), StatusCode::OK);
    let stream_body = stream_response.text().await.unwrap();
    assert!(stream_body.contains("panel A fallback"));
    assert!(!stream_body.contains("provider_error"));

    assert_eq!(aggregator.call_count(), 2);
    let logs = wait_for_prompt_logs(&gateway.config, 2).await;
    for log in logs {
        let call = role_call(fusion_calls(&log), "aggregator");
        assert_eq!(call["status"], "failed");
        assert_eq!(call["error"]["upstream_status"], 502);
    }
}

#[tokio::test]
async fn aggregator_midstream_error_is_emitted_and_not_replaced_by_a_panel() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "panel A fallback",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Success {
        model: "panel-b-upstream",
        content: "panel B answer",
    })
    .await;
    let aggregator = start_mock(MockBehavior::StreamingError).await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;

    let response = gateway
        .post(&fusion_request("stream until reset", true, None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("partial aggregate"));
    assert!(!body.contains("panel A fallback"));
    let error = sse_json_events(&body)
        .into_iter()
        .find(|event| event["error"]["type"] == "provider_error")
        .expect("stream did not contain a provider_error event");
    assert!(error["error"].is_object());
    assert_eq!(error["error"]["code"], 502);
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("direct_synthesis"));
    assert!(message.contains("aggregator"));
    assert!(message.contains("Upstream stream error"));

    let logs = wait_for_prompt_logs(&gateway.config, 1).await;
    let call = role_call(fusion_calls(&logs[0]), "aggregator");
    assert_eq!(call["status"], "failed");
    assert_eq!(call["response"]["event_count"], 1);
    assert_eq!(
        call["response"]["last_chunk"]["choices"][0]["delta"]["content"],
        "partial aggregate"
    );
    assert!(call["response"].get("chunks").is_none());
}

#[tokio::test]
async fn stream_keep_alive_precedes_slow_first_token_and_trace_stays_compact() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "panel A answer",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Success {
        model: "panel-b-upstream",
        content: "panel B answer",
    })
    .await;
    let aggregator = start_mock(MockBehavior::StreamingSlowBeforeFirst).await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;

    let response = gateway
        .post(&fusion_request("slow first token", true, None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(20), stream.next())
        .await
        .expect("Gateway did not send the default keep-alive before the 30-second timeout")
        .expect("stream ended before keep-alive")
        .unwrap();
    let first = String::from_utf8_lossy(&first);
    assert!(first.lines().any(|line| line.starts_with(':')));
    assert!(!first.contains("late aggregate"));

    let mut remainder = String::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(chunk) = stream.next().await {
            remainder.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    })
    .await
    .expect("slow upstream response did not finish after keep-alive");
    assert!(remainder.lines().any(|line| line.starts_with(':')));
    assert!(remainder.contains("late aggregate"));
    let events = sse_json_events(&remainder);
    assert_eq!(events.last().unwrap()["usage"]["prompt_tokens"], 6);
    assert_eq!(events.last().unwrap()["usage"]["completion_tokens"], 3);
    assert_eq!(events.last().unwrap()["usage"]["total_tokens"], 9);

    let logs = wait_for_prompt_logs(&gateway.config, 1).await;
    let call = role_call(fusion_calls(&logs[0]), "aggregator");
    assert_eq!(call["status"], "succeeded");
    assert_eq!(call["response"]["event_count"], 1);
    assert!(call["response"]["last_chunk"].is_object());
    assert!(call["response"].get("chunks").is_none());
}

#[tokio::test]
async fn client_disconnect_cancels_the_real_upstream_and_marks_trace_cancelled() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "panel A answer",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Success {
        model: "panel-b-upstream",
        content: "panel B answer",
    })
    .await;
    let aggregator = start_mock(MockBehavior::StreamingSlowAfterFirst).await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;

    let response = gateway
        .post(&fusion_request("disconnect after first token", true, None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains("first aggregate chunk"));
    drop(stream);

    wait_for_disconnect(&aggregator).await;
    let logs = wait_for_prompt_logs_with_timeout(&gateway.config, 1, Duration::from_secs(8)).await;
    assert_eq!(logs[0]["status_code"], 200);
    let call = role_call(fusion_calls(&logs[0]), "aggregator");
    assert_eq!(call["status"], "cancelled");
    assert_eq!(call["response"]["event_count"], 1);
    assert!(call["duration_ms"].as_u64().is_some());
}

#[tokio::test]
async fn explicit_empty_tools_are_omitted_from_every_child_request() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "panel A answer",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Success {
        model: "panel-b-upstream",
        content: "panel B answer",
    })
    .await;
    let aggregator = start_mock(MockBehavior::Success {
        model: "aggregator-upstream",
        content: "aggregated answer",
    })
    .await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;
    let body = json!({
        "model": "fusion",
        "messages": [{"role": "user", "content": "omit empty tools"}],
        "stream": false,
        "tools": [],
        "tool_choice": "required"
    });

    let response = gateway.post(&body).await;
    assert_eq!(response.status(), StatusCode::OK);
    for upstream in [&panel_a, &panel_b, &aggregator] {
        let requests = upstream.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].get("tools").is_none());
        assert!(requests[0].get("tool_choice").is_none());
    }
}

#[tokio::test]
async fn multiple_choices_are_rejected_before_any_child_request() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "panel A answer",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Success {
        model: "panel-b-upstream",
        content: "panel B answer",
    })
    .await;
    let aggregator = start_mock(MockBehavior::Success {
        model: "aggregator-upstream",
        content: "aggregated answer",
    })
    .await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;
    let mut body = fusion_request("reject n greater than one", false, None);
    body["n"] = json!(2);

    let response = gateway.post(&body).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "unsupported_mode_error");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("supports only n=1"));
    assert_eq!(panel_a.call_count(), 0);
    assert_eq!(panel_b.call_count(), 0);
    assert_eq!(aggregator.call_count(), 0);
}

#[tokio::test]
async fn successful_empty_aggregator_response_is_not_revalidated() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "panel A answer",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Success {
        model: "panel-b-upstream",
        content: "panel B answer",
    })
    .await;
    let aggregator = start_mock(MockBehavior::Invalid {
        model: "aggregator-upstream",
    })
    .await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;

    let response = gateway
        .post(&fusion_request("trust successful aggregator", false, None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "aggregator-upstream");
    assert_eq!(body["choices"].as_array().map(Vec::len), Some(0));
    let logs = wait_for_prompt_logs(&gateway.config, 1).await;
    assert_eq!(
        role_call(fusion_calls(&logs[0]), "aggregator")["status"],
        "succeeded"
    );
}

#[tokio::test]
async fn panel_timeout_can_degrade_to_one_tool_capable_panel_and_is_traced() {
    let panel_a = start_mock(MockBehavior::DelayedPanel).await;
    let panel_b = start_mock(MockBehavior::Success {
        model: "panel-b-upstream",
        content: "fast panel answer",
    })
    .await;
    let aggregator = start_mock(MockBehavior::Success {
        model: "aggregator-upstream",
        content: "must not be called",
    })
    .await;
    let gateway = TestGateway::start_with_timeout(&panel_a, &panel_b, &aggregator, Some(1)).await;

    let response = gateway
        .post(&fusion_request(
            "one panel times out",
            false,
            Some(bash_tools()),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "panel-b-upstream");
    assert_eq!(panel_a.call_count(), 1);
    assert_eq!(panel_b.call_count(), 1);
    assert_eq!(aggregator.call_count(), 0);

    wait_for_disconnect(&panel_a).await;
    let logs = wait_for_prompt_logs(&gateway.config, 1).await;
    let calls = fusion_calls(&logs[0]);
    let delayed = calls
        .iter()
        .find(|call| call["model"] == "panel-a")
        .expect("delayed panel call missing from trace");
    assert_eq!(delayed["status"], "cancelled");
    assert!(delayed["duration_ms"].as_u64().is_some());
}

#[tokio::test]
async fn concurrent_requests_keep_prompt_traces_isolated() {
    let panel_a = start_mock(MockBehavior::Success {
        model: "panel-a-upstream",
        content: "panel A answer",
    })
    .await;
    let panel_b = start_mock(MockBehavior::Success {
        model: "panel-b-upstream",
        content: "panel B answer",
    })
    .await;
    let aggregator = start_mock(MockBehavior::Success {
        model: "aggregator-upstream",
        content: "aggregated answer",
    })
    .await;
    let gateway = TestGateway::start(&panel_a, &panel_b, &aggregator).await;
    let markers = ["trace-alpha", "trace-beta", "trace-gamma"];

    let bodies = markers
        .iter()
        .map(|marker| fusion_request(marker, false, Some(bash_tools())))
        .collect::<Vec<_>>();
    let responses = futures::future::join_all(bodies.iter().map(|body| gateway.post(body))).await;
    assert!(responses
        .iter()
        .all(|response| response.status() == StatusCode::OK));

    let logs = wait_for_prompt_logs(&gateway.config, markers.len()).await;
    assert_eq!(logs.len(), markers.len());
    for log in logs {
        let marker = log["request"]["messages"][0]["content"].as_str().unwrap();
        let trace = serde_json::to_string(&log["fusion"]).unwrap();
        assert!(trace.contains(marker));
        for other in markers.iter().filter(|other| **other != marker) {
            assert!(!trace.contains(other));
        }
    }
}
