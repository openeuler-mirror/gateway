//! DT 用例 — DT harness 自身（`dt-test/src/lib.rs` 的测试基础设施）。
//!
//! 覆盖：
//! - `free_port`：返回非零端口，且立刻回 bind 成功（确实空闲）
//! - `chat_request`：JSON 反序列化字段正确、未知字段进 `extra`、
//!   类型错误按文档 panic（should_panic）
//! - `simple_chat_request`：最小请求 = 单条 user 消息
//! - `MockUpstream::start_chat_ok` + `uri()`：POST /v1/chat/completions
//!   返回 200 + 指定 content + 固定 usage
//! - `MockUpstream::start_empty` + `server()` + `remount_chat_ok`：
//!   空 server 无 Mock → 404；自挂错误 Mock → 500；remount 正常 Mock
//!   覆盖错误 Mock（wiremock 后挂的 Mock 优先）→ 200
//! - `TestServer::serve` + `url()` + `client()`：axum Router 起随机端口、
//!   GET/POST 走通；Drop 后端口关闭（连接被拒）

use boom_core::types::MessageContent;
use boom_dt::{chat_request, free_port, simple_chat_request, MockUpstream, TestServer};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

// ───────────────────────── helpers ─────────────────────────

/// 向 mock 上游发一条 chat 请求，返回状态码。
async fn post_chat_status(client: &reqwest::Client, base: &str) -> u16 {
    let body = serde_json::to_value(simple_chat_request("dt-mock-model", "hi")).unwrap();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("request reaches mock upstream");
    resp.status().as_u16()
}

/// 向 mock 上游发一条 chat 请求，返回 (status, body JSON)（要求响应是 JSON）。
async fn post_chat(client: &reqwest::Client, base: &str) -> (u16, serde_json::Value) {
    let body = serde_json::to_value(simple_chat_request("dt-mock-model", "hi")).unwrap();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("request reaches mock upstream");
    let status = resp.status().as_u16();
    let value = resp.json().await.expect("json body");
    (status, value)
}

// ═════════════════════════════════════════════════════════════════
// free_port
// ═════════════════════════════════════════════════════════════════

/// DT-HAR-01：free_port 返回非零端口，且立刻回 bind 成功（释放后确实空闲）。
#[test]
fn free_port_returns_bindable_port() {
    let port = free_port();
    assert!(port != 0, "ephemeral port must be non-zero");
    // bind 后立即释放，端口应可立刻复用（loopback 上竞态窗口极小）
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .expect("port returned by free_port is bindable");
    assert_eq!(listener.local_addr().unwrap().port(), port);
    // 连续分配的两个端口通常不同（ephemeral 区间轮转）
    let another = free_port();
    assert!(another != 0);
}

// ═════════════════════════════════════════════════════════════════
// chat_request / simple_chat_request
// ═════════════════════════════════════════════════════════════════

/// DT-HAR-02：chat_request 反序列化已知字段，未知字段落入 extra（flatten）。
#[test]
fn chat_request_deserializes_fields_and_extra() {
    let req = chat_request(json!({
        "model": "gpt-dt",
        "messages": [
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "hello"}
        ],
        "temperature": 0.7,
        "max_tokens": 128,
        "stream": false,
        "x-vendor-field": {"nested": true}
    }));
    assert_eq!(req.model, "gpt-dt");
    assert_eq!(req.messages.len(), 2);
    assert!(matches!(&req.messages[1].content, MessageContent::Text(t) if t == "hello"));
    assert_eq!(req.temperature, Some(0.7));
    assert_eq!(req.max_tokens, Some(128));
    assert_eq!(req.stream, Some(false));
    // 未知字段不丢，进 extra（provider 层透传给上游用）
    assert_eq!(req.extra["x-vendor-field"]["nested"], true);
}

/// DT-HAR-03：chat_request 类型不符按文档 panic（反序列化 expect 路径）。
#[test]
#[should_panic(expected = "deserialize ChatCompletionRequest")]
fn chat_request_panics_on_type_mismatch() {
    let _ = chat_request(json!({
        "model": "gpt-dt",
        "messages": "not-an-array"
    }));
}

/// DT-HAR-04：simple_chat_request = 指定 model + 单条 user 消息，其余字段默认。
#[test]
fn simple_chat_request_builds_minimal_request() {
    let req = simple_chat_request("qwen-max", "你好");
    assert_eq!(req.model, "qwen-max");
    assert_eq!(req.messages.len(), 1);
    assert!(matches!(req.messages[0].role, boom_core::types::MessageRole::User));
    assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "你好"));
    // 其余字段保持默认（未设置）
    assert_eq!(req.temperature, None);
    assert_eq!(req.stream, None);
    assert!(req.extra.is_empty());
}

// ═════════════════════════════════════════════════════════════════
// MockUpstream
// ═════════════════════════════════════════════════════════════════

/// DT-HAR-05：start_chat_ok 起的 mock 上游返回 200 + 指定 content + 固定 usage。
#[tokio::test]
async fn mock_upstream_start_chat_ok_serves_200() {
    let upstream = MockUpstream::start_chat_ok("dt-hello").await;
    let base = upstream.uri();
    assert!(base.starts_with("http://127.0.0.1:"));

    let client = reqwest::Client::new();
    let (status, body) = post_chat(&client, &base).await;
    assert_eq!(status, 200);
    assert_eq!(body["choices"][0]["message"]["content"], "dt-hello");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["total_tokens"], 18);
}

/// DT-HAR-06：start_empty 不挂默认 Mock（404）；server() 自挂错误 Mock（500）；
/// remount_chat_ok 后挂的正常 Mock 优先于先挂的错误 Mock（200 + 新 content）。
#[tokio::test]
async fn mock_upstream_empty_then_remount_overrides() {
    let upstream = MockUpstream::start_empty().await;
    let client = reqwest::Client::new();

    // 1) 空 server：无 Mock 匹配 → wiremock 404（响应体为空，只看状态码）
    let status = post_chat_status(&client, &upstream.uri()).await;
    assert_eq!(status, 404, "empty upstream has no mock mounted");

    // 2) 拿 server() 自挂一个 500 错误 Mock（覆盖"错误场景自行 mount"用法）
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream exploded"))
        .mount(upstream.server())
        .await;
    let status = post_chat_status(&client, &upstream.uri()).await;
    assert_eq!(status, 500);

    // 3) remount 正常 Mock：后挂的 Mock 优先 → 200，且 content 已替换
    upstream.remount_chat_ok("recovered").await;
    let (status, body) = post_chat(&client, &upstream.uri()).await;
    assert_eq!(status, 200);
    assert_eq!(body["choices"][0]["message"]["content"], "recovered");
}

// ═════════════════════════════════════════════════════════════════
// TestServer
// ═════════════════════════════════════════════════════════════════

async fn ping() -> &'static str {
    "pong"
}

async fn echo(axum::Json(body): axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
    axum::Json(body)
}

/// DT-HAR-07：TestServer 把 axum Router 起在随机端口 —— url() 拼路径、
/// client() 带超时可访问；Drop 后端口关闭（后续连接被拒）。
#[tokio::test]
async fn test_server_serves_router_and_stops_on_drop() {
    let router = axum::Router::new()
        .route("/ping", axum::routing::get(ping))
        .route("/echo", axum::routing::post(echo));
    let server = TestServer::serve(router).await;

    let ping_url = server.url("/ping");
    let echo_url = server.url("/echo");
    assert!(ping_url.starts_with("http://127.0.0.1:") && ping_url.ends_with("/ping"));

    let client = server.client();
    // GET 路由
    let resp = client.get(&ping_url).send().await.expect("GET /ping");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "pong");
    // POST + JSON 往返
    let resp = client
        .post(&echo_url)
        .json(&json!({"key": "value", "n": 42}))
        .send()
        .await
        .expect("POST /echo");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["key"], "value");
    assert_eq!(body["n"], 42);

    // Drop → 任务 abort、监听关闭 → 同一 client 再连被拒
    drop(server);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let err = client.get(&ping_url).send().await.err().expect("connection closed after drop");
    assert!(err.is_connect(), "expected connect error, got {err:?}");
}
