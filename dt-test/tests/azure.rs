//! DT 用例 — boom-provider::azure：Azure OpenAI Provider。
//!
//! 覆盖（内部 wiremock 测试在依赖编译下不参与，此处等价复现 + 补齐）：
//! - chat：成功（model 改写为 deployment、api-key 头、api-version 查询参数、
//!   gateway_headers 透传 + 非法头丢弃）/ 非 2xx UpstreamError / 连接失败
//! - chat_stream：SSE 成功（data: 无空格、[DONE]、坏行跳过、usage chunk）、
//!   流体 stream=true + stream_options.include_usage 注入、非 2xx 报错、
//!   提前 Drop 不挂起
//! - 访问器：name/protocol/models/deployment_id/kv_worker_id/client_type_header/
//!   with_custom_headers

use boom_core::provider::Provider;
use boom_core::types::*;
use boom_provider::azure::AzureProvider;
use futures::StreamExt;
use reqwest::Client;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ───────────────────────── helpers ─────────────────────────

const DEP: &str = "test-dep";
const AZURE_PATH: &str = "/openai/deployments/test-dep/chat/completions";

fn provider(server_uri: &str) -> AzureProvider {
    AzureProvider::new(
        Client::new(),
        Some("azure-key".to_string()),
        Some(server_uri.to_string()),
        DEP,
        "", // 默认 api-version=2024-02-01
        Some("dep-id-1".to_string()),
        true,
    )
}

fn request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "whatever-model".to_string(),
        from_anthropic_protocol: false,
        messages: vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".to_string()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: None,
        stop: None,
        max_tokens: None,
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        seed: None,
        logprobs: None,
        top_logprobs: None,
        logit_bias: None,
        extra: Default::default(),
        gateway_headers: Default::default(),
        kv_cache_report_full: false,
        raw_capture: None,
    }
}

fn completion_json() -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1,
        "model": DEP,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hey"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
    })
}

fn chat_mock() -> wiremock::Mock {
    Mock::given(method("POST")).and(path(AZURE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion_json()))
}

fn sse_mock(body: String) -> wiremock::Mock {
    Mock::given(method("POST")).and(path(AZURE_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
}

// ═════════════════════════════════════════════════════════════════
// 访问器
// ═════════════════════════════════════════════════════════════════

/// DT-AZ-01：访问器 + 默认 api-version + kv_worker_id 派生自 api_base host。
#[tokio::test]
async fn azure_accessors() {
    let server = MockServer::start().await;
    let p = provider(&server.uri());
    assert_eq!(p.name(), "azure");
    assert_eq!(p.protocol(), boom_core::provider::ProviderProtocol::OpenAiCompatible);
    assert_eq!(p.models(), &[DEP.to_string()]);
    assert_eq!(p.deployment_id(), Some("dep-id-1"));
    // kv_worker_id 派生自 api_base（去 scheme 的 host[:port]）
    let kv = p.kv_worker_id().expect("kv from api_base");
    assert!(kv.contains("127.0.0.1"), "{kv}");
    assert!(p.client_type_header());
    assert!(p.custom_headers().is_empty());

    let with_headers = p.with_custom_headers(vec![("X-Custom".into(), "v".into())]);
    assert_eq!(with_headers.custom_headers().len(), 1);
    assert_eq!(with_headers.custom_headers()[0].0, "X-Custom");
}

// ═════════════════════════════════════════════════════════════════
// chat — 非流式
// ═════════════════════════════════════════════════════════════════

/// DT-AZ-02：chat 成功 —— model 改写为 deployment、api-key 头、
/// 默认 api-version 查询参数、usage 透传。
#[tokio::test]
async fn azure_chat_success() {
    let server = MockServer::start().await;
    chat_mock().mount(&server).await;
    let p = provider(&server.uri());

    let resp = p.chat(request()).await.expect("chat ok");
    assert_eq!(resp.model, DEP);
    assert_eq!(resp.usage.unwrap().total_tokens, 3);
    assert!(matches!(&resp.choices[0].message.content, MessageContent::Text(t) if t == "hey"));

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    // api-key 头
    assert!(reqs[0].headers.keys().any(|k| k.as_str() == "api-key"));
    // 默认 api-version
    assert!(reqs[0].url.query().unwrap().contains("api-version=2024-02-01"));
    // model 被改写为 deployment 名
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["model"], DEP);
}

/// DT-AZ-03：自定义 api_version 出现在查询参数。
#[tokio::test]
async fn azure_chat_custom_api_version() {
    let server = MockServer::start().await;
    chat_mock().mount(&server).await;
    let p = AzureProvider::new(
        Client::new(),
        None,
        Some(server.uri()),
        DEP,
        "2025-01-01",
        None,
        false,
    );
    assert!(p.chat(request()).await.is_ok());
    let reqs = server.received_requests().await.unwrap();
    assert!(reqs[0].url.query().unwrap().contains("api-version=2025-01-01"));
}

/// DT-AZ-04：gateway_headers 透传（合法）+ 非法头项丢弃不 panic。
#[tokio::test]
async fn azure_chat_gateway_headers_forwarded() {
    let server = MockServer::start().await;
    chat_mock().mount(&server).await;
    let p = provider(&server.uri());
    let mut req = request();
    req.gateway_headers.insert("X-Gateway-Priority".to_string(), "100".to_string());
    req.gateway_headers.insert("bad header".to_string(), "x".to_string()); // 非法名 → 丢弃

    assert!(p.chat(req).await.is_ok());
    let reqs = server.received_requests().await.unwrap();
    assert!(reqs[0].headers.keys().any(|k| k.as_str() == "x-gateway-priority"));
    assert!(!reqs[0].headers.keys().any(|k| k.as_str().contains("bad")));
}

/// DT-AZ-05：chat 非 2xx → UpstreamError（状态 + body 文本）。
#[tokio::test]
async fn azure_chat_non_2xx_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(AZURE_PATH))
        .respond_with(ResponseTemplate::new(429).set_body_string("quota exceeded"))
        .mount(&server).await;
    let p = provider(&server.uri());
    let err = p.chat(request()).await.err().expect("429 fails");
    match err {
        boom_core::GatewayError::UpstreamError { status, message } => {
            assert_eq!(status, 429);
            assert_eq!(message, "quota exceeded");
        }
        other => panic!("expected UpstreamError, got {other:?}"),
    }
}

/// DT-AZ-06：chat 连接失败 → ProviderError。
#[tokio::test]
async fn azure_chat_connection_refused() {
    let p = provider("http://127.0.0.1:1");
    let err = p.chat(request()).await.err().expect("refused");
    assert!(matches!(err, boom_core::GatewayError::ProviderError(m) if m.contains("unavailable")));
}

// ═════════════════════════════════════════════════════════════════
// chat_stream — 流式
// ═════════════════════════════════════════════════════════════════

/// DT-AZ-07：流式成功 —— data: 无空格解析、[DONE] 终止、usage chunk 透传、
/// 请求体注入 stream=true + stream_options.include_usage。
#[tokio::test]
async fn azure_stream_success() {
    let sse = concat!(
        // data: 无空格（SPE 允许）
        "data:{\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        // 坏 JSON 行 → 跳过不中断
        "data:not-json\n\n",
        // usage chunk（choices 空）
        "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\n",
        "data:[DONE]\n\n",
    );
    let server = MockServer::start().await;
    sse_mock(sse.to_string()).mount(&server).await;
    let p = provider(&server.uri());

    let stream = p.chat_stream(request()).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;
    // content chunk + usage chunk（坏行被跳过）
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("hi"));
    assert_eq!(chunks[1].usage.as_ref().unwrap().total_tokens, Some(3));

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
}

/// DT-AZ-08：流式非 2xx → UpstreamError。
#[tokio::test]
async fn azure_stream_non_2xx_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(AZURE_PATH))
        .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
        .mount(&server).await;
    let p = provider(&server.uri());
    let err = p.chat_stream(request()).await.err().expect("503 fails");
    match err {
        boom_core::GatewayError::UpstreamError { status, message } => {
            assert_eq!(status, 503);
            assert_eq!(message, "overloaded");
        }
        other => panic!("expected UpstreamError, got {other:?}"),
    }
}

/// DT-AZ-09：流式连接失败 → ProviderError。
#[tokio::test]
async fn azure_stream_connection_refused() {
    let p = provider("http://127.0.0.1:1");
    let err = p.chat_stream(request()).await.err().expect("refused");
    assert!(matches!(err, boom_core::GatewayError::ProviderError(m) if m.contains("unavailable")));
}

/// DT-AZ-10：消费一个 chunk 后提前 Drop 流 → 后台任务经 tx.closed() 退出，不挂起。
#[tokio::test]
async fn azure_stream_dropped_early_terminates_pump() {
    let sse = "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\n\n";
    let server = MockServer::start().await;
    sse_mock(sse.to_string()).mount(&server).await;
    let p = provider(&server.uri());

    let mut stream = p.chat_stream(request()).await.expect("stream starts");
    let _first = stream.next().await;
    drop(stream);
    // 给后台 pump 一点时间观察到关闭并退出（无需断言，不挂起即通过）
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}
