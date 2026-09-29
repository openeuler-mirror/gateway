//! DT 用例 — boom-routing::ml_service_client：外置 ML 分类服务客户端。
//!
//! 覆盖（内部 mockito 测试在依赖编译下不参与，等价场景用 wiremock 复现）：
//! - try_new 校验：timeout=0 / 非法 URL / 非 http(s) scheme / 合法入参
//! - new（panic 包装）+ stats() + name()
//! - classify：合法 tier 直用 / 未知 tier 回退 TierClassifier /
//!   非 2xx 回退 / 坏 JSON 回退 / 连接失败回退（failures+fallbacks 计数）
//! - 请求体携带 messages / tools / default_tier

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use boom_core::types::{Message, MessageContent, MessageRole};
use boom_routing::auto_router::{ClassifyRequest, ClassificationStrategy};
use boom_routing::MlServiceClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ───────────────────────── helpers ─────────────────────────

/// MlServiceClient 无 Debug，用 match 取 Err 信息。
fn unwrap_err(result: Result<MlServiceClient, String>) -> String {
    match result {
        Ok(_) => panic!("expected Err, got Ok"),
        Err(msg) => msg,
    }
}

fn valid_tiers() -> HashSet<String> {
    HashSet::from(["small".to_string(), "medium".to_string(), "large".to_string()])
}

fn msgs(text: &str) -> Vec<Message> {
    vec![Message {
        role: MessageRole::User,
        content: MessageContent::Text(text.to_string()),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    }]
}

fn classify_req(ms: &[Message]) -> ClassifyRequest<'_> {
    ClassifyRequest { messages: ms, tools: &None, default_tier: "medium" }
}

fn classify_ok() -> Mock {
    Mock::given(method("POST")).and(path("/classify"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"tier":"large"}"#))
}

// ═════════════════════════════════════════════════════════════════
// try_new 校验
// ═════════════════════════════════════════════════════════════════

/// DT-ML-01：try_new —— timeout=0 / 坏 URL / 非 http scheme / 合法 URL。
#[test]
fn try_new_validation_matrix() {
    let err = unwrap_err(MlServiceClient::try_new("http://127.0.0.1:2345", 0, valid_tiers()));
    assert!(err.contains("timeout_ms"), "{err}");

    let err = unwrap_err(MlServiceClient::try_new("not a url", 100, valid_tiers()));
    assert!(err.contains("valid URL"), "{err}");

    let err = unwrap_err(MlServiceClient::try_new("ftp://127.0.0.1:2345", 100, valid_tiers()));
    assert!(err.contains("http"), "{err}");

    assert!(MlServiceClient::try_new("http://127.0.0.1:2345", 100, valid_tiers()).is_ok());
    assert!(MlServiceClient::try_new("https://ml.example.com/", 100, valid_tiers()).is_ok());
}

/// DT-ML-02：new（合法入参不 panic）+ name + stats 初始为零。
#[test]
fn new_stats_and_name() {
    let client = MlServiceClient::new("http://127.0.0.1:2345", 100, valid_tiers());
    assert_eq!(ClassificationStrategy::name(&client), "ml_service");
    let s = client.stats();
    assert_eq!(s.successes.load(Ordering::Relaxed), 0);
    assert_eq!(s.failures.load(Ordering::Relaxed), 0);
    assert_eq!(s.fallbacks.load(Ordering::Relaxed), 0);
}

// ═════════════════════════════════════════════════════════════════
// classify — 成功与回退
// ═════════════════════════════════════════════════════════════════

/// DT-ML-03：合法 tier → 直接采用；successes=1；请求体带 messages/default_tier。
#[tokio::test]
async fn classify_valid_tier_used_directly() {
    let server = MockServer::start().await;
    classify_ok().mount(&server).await;
    let client = MlServiceClient::new(&server.uri(), 500, valid_tiers());

    let ms = msgs("hello");
    let tier = client.classify(&classify_req(&ms)).await;
    assert_eq!(tier, "large");
    assert_eq!(client.stats().successes.load(Ordering::Relaxed), 1);
    assert_eq!(client.stats().failures.load(Ordering::Relaxed), 0);

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["default_tier"], "medium");
    assert_eq!(body["messages"][0]["content"], "hello");
    assert_eq!(body["tools"], serde_json::Value::Null);
}

/// DT-ML-04：未知 tier → TierClassifier 回退（"hi" → small）；failure+fallback 计数。
#[tokio::test]
async fn classify_unknown_tier_falls_back() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/classify"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"tier":"x-large"}"#))
        .mount(&server).await;
    let client = MlServiceClient::new(&server.uri(), 500, valid_tiers());

    let ms = msgs("hi");
    let tier = client.classify(&classify_req(&ms)).await;
    assert_eq!(tier, "small");
    assert_eq!(client.stats().failures.load(Ordering::Relaxed), 1);
    assert_eq!(client.stats().fallbacks.load(Ordering::Relaxed), 1);
}

/// DT-ML-05：非 2xx → 回退；failures=1。
#[tokio::test]
async fn classify_non_2xx_falls_back() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/classify"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server).await;
    let client = MlServiceClient::new(&server.uri(), 500, valid_tiers());

    let ms = msgs("hi");
    let tier = client.classify(&classify_req(&ms)).await;
    assert_eq!(tier, "small");
    assert_eq!(client.stats().failures.load(Ordering::Relaxed), 1);
    assert_eq!(client.stats().fallbacks.load(Ordering::Relaxed), 1);
}

/// DT-ML-06：200 但坏 JSON → 回退。
#[tokio::test]
async fn classify_malformed_json_falls_back() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/classify"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
        .mount(&server).await;
    let client = MlServiceClient::new(&server.uri(), 500, valid_tiers());

    let ms = msgs("hi");
    let tier = client.classify(&classify_req(&ms)).await;
    assert_eq!(tier, "small");
    assert_eq!(client.stats().failures.load(Ordering::Relaxed), 1);
}

/// DT-ML-07：连接失败（端口关闭）→ 回退，请求不因 ML 服务故障而失败。
#[tokio::test]
async fn classify_connection_refused_falls_back() {
    let client = MlServiceClient::new("http://127.0.0.1:1", 100, valid_tiers());
    let ms = msgs("hi");
    let tier = client.classify(&classify_req(&ms)).await;
    assert_eq!(tier, "small");
    assert_eq!(client.stats().failures.load(Ordering::Relaxed), 1);
    assert_eq!(client.stats().fallbacks.load(Ordering::Relaxed), 1);
}

/// DT-ML-08：带 tools 的请求 → 请求体 tools 序列化透传。
#[tokio::test]
async fn classify_forwards_tools_in_body() {
    let server = MockServer::start().await;
    classify_ok().mount(&server).await;
    let client = MlServiceClient::new(&server.uri(), 500, valid_tiers());

    let ms = msgs("weather?");
    let tools = Some(vec![boom_core::types::Tool {
        tool_type: "function".into(),
        function: boom_core::types::ToolFunction {
            name: "get_weather".into(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
        },
    }]);
    let req = ClassifyRequest { messages: &ms, tools: &tools, default_tier: "medium" };
    let tier = client.classify(&req).await;
    assert_eq!(tier, "large");

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
}
