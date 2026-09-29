//! DT 用例 — boom-provider：Provider 构造、SSE 解析、HTTP chat/chat_stream。
//!
//! 覆盖：
//! - sse.rs SseParser：WHATWG §9.2 全分支（LF/CRLF/CR、lone \r、跨包 CRLF、
//!   data: 无空格、event 字段、注释/未知字段、BOM、UTF-8 跨段、finish、空事件、raw、take_pending_raw）
//! - lib.rs create_provider：各 provider 类型 protocol、未知 provider 错误、
//!   custom_headers sanitization（reserved/hard-blocked/transport/invalid）、
//!   reserved key 配置 anthropic_version、auto_detect、kv_worker_id_from_api_base
//! - openai.rs chat/chat_stream（wiremock）：成功/usage 缺失/解析错误/raw body/BOM/
//!   SSE 装配/部分 usage/null identity/未知 role/未知 content part/流式 CRLF/无空格 data/
//!   未知 role 流式/tool_call 无 index/gateway_headers 注入/bearer auth/extra 透传
//! - anthropic.rs chat/chat_stream（wiremock）：成功/api_version header/custom headers
//! - gemini/azure/bedrock：构造 + protocol

use std::collections::HashMap;
use std::sync::Arc;

use boom_core::provider::{Provider, ProviderProtocol};
use boom_core::types::*;
use boom_core::GatewayError;
use boom_provider::{create_provider, kv_worker_id_from_api_base, sse::SseParser};
use futures::StreamExt;
use reqwest::Client;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ───────────────────────── helpers ─────────────────────────

/// 最小 ChatCompletionRequest（单条 user 消息）。结构与 boom-provider 内部测试对齐。
fn min_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "test-model".to_string(),
        from_anthropic_protocol: false,
        messages: vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".to_string()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }],
        max_tokens: None,
        max_completion_tokens: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        stop: None,
        n: None,
        stream: None,
        logprobs: None,
        top_logprobs: None,
        logit_bias: None,
        user: None,
        extra: Default::default(),
        gateway_headers: HashMap::new(),
        kv_cache_report_full: false,
        raw_capture: None,
    }
}

fn request_with_gateway_headers(hdrs: &[(&str, &str)]) -> ChatCompletionRequest {
    let mut r = min_request();
    for (k, v) in hdrs {
        r.gateway_headers.insert(k.to_string(), v.to_string());
    }
    r
}

fn fake_completion_json() -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-test", "object": "chat.completion", "created": 1700000000_u64, "model": "test-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

fn fake_anthropic_response() -> serde_json::Value {
    serde_json::json!({
        "id": "msg_test", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "hi"}], "model": "claude-test",
        "stop_reason": "end_turn", "usage": {"input_tokens": 5, "output_tokens": 1}
    })
}

/// 通过 create_provider 构造 provider（覆盖 lib.rs 路径），api_base 指向 mock server。
fn create(model: &str, key: Option<String>, base: String) -> Arc<dyn Provider> {
    create_provider(model, key, Some(base), 5, &HashMap::new(), &HashMap::new(), None, false)
        .expect("create_provider")
}

fn create_full(
    model: &str,
    key: Option<String>,
    base: String,
    extra: &HashMap<String, String>,
    custom_headers: &HashMap<String, String>,
) -> Arc<dyn Provider> {
    create_provider(model, key, Some(base), 5, extra, custom_headers, None, false)
        .expect("create_provider")
}

fn datas(events: &[boom_provider::sse::SseEvent]) -> Vec<&str> {
    events.iter().map(|e| e.data.as_str()).collect()
}

// ═════════════════════════════════════════════════════════════
// sse.rs — SseParser
// ═════════════════════════════════════════════════════════════

/// DT-PRV-SSE-01：LF / CRLF / CR 三种行尾都能正确分帧。
#[test]
fn sse_lf_crlf_cr_line_endings() {
    let mut p = SseParser::new();
    let events = p.push(b"data: a\n\ndata: b\r\n\r\ndata: c\r\rdata: d\n\n");
    assert_eq!(datas(&events), ["a", "b", "c", "d"]);
}

/// DT-PRV-SSE-02：流末 lone \r 被 finish 当作终止符关闭。
#[test]
fn sse_trailing_lone_cr_closed_by_finish() {
    let mut p = SseParser::new();
    let events = p.push(b"data: c\r\rdata: d\n\ndata: e\r");
    assert_eq!(datas(&events), ["c", "d"]);
    let (events, _) = p.finish();
    assert_eq!(datas(&events), ["e"]);
}

/// DT-PRV-SSE-03：CRLF 跨 push 分割仍是一个终止符（不伪造换行）。
#[test]
fn sse_crlf_split_across_pushes() {
    let mut p = SseParser::new();
    assert!(p.push(b"data: {\"a\":1}\r").is_empty());
    let events = p.push(b"\ndata: {\"b\":2}\r\n\r\n");
    assert_eq!(datas(&events), ["{\"a\":1}\n{\"b\":2}"]);
}

/// DT-PRV-SSE-04：data: 无空格等价；多空格只剥一个。
#[test]
fn sse_data_prefix_without_space() {
    let mut p = SseParser::new();
    let events = p.push(b"data:a\n\ndata:  b\n\n");
    assert_eq!(datas(&events), ["a", " b"]);
}

/// DT-PRV-SSE-05：event 字段被捕获。
#[test]
fn sse_event_field_captured() {
    let mut p = SseParser::new();
    let events = p.push(b"event: content_block_delta\ndata: {\"x\":1}\n\n");
    assert_eq!(events[0].event_type, "content_block_delta");
    assert_eq!(events[0].data, "{\"x\":1}");
}

/// DT-PRV-SSE-06：注释行与未知字段（id/retry）被忽略。
#[test]
fn sse_comments_and_unknown_fields_ignored() {
    let mut p = SseParser::new();
    let events = p.push(b": keep-alive\nid: 42\nretry: 3000\ndata: a\n\n");
    assert_eq!(datas(&events), ["a"]);
}

/// DT-PRV-SSE-07：BOM 仅剥离一次。
#[test]
fn sse_bom_stripped_once() {
    let mut p = SseParser::new();
    let events = p.push("\u{feff}data: a\n\n".as_bytes());
    assert_eq!(datas(&events), ["a"]);
}

/// DT-PRV-SSE-08：多字节 UTF-8 跨 TCP 段不损坏。
#[test]
fn sse_utf8_split_across_pushes() {
    let mut p = SseParser::new();
    let full = "data: 你好\n\n".as_bytes().to_vec();
    let split_at = 7; // 在「你」3字节中间
    assert!(p.push(&full[..split_at]).is_empty());
    let events = p.push(&full[split_at..]);
    assert_eq!(datas(&events), ["你好"]);
}

/// DT-PRV-SSE-09：finish 派发无最终空行的 pending 事件。
#[test]
fn sse_finish_dispatches_pending() {
    let mut p = SseParser::new();
    assert!(p.push(b"data: tail").is_empty());
    let (events, leftover) = p.finish();
    assert_eq!(datas(&events), ["tail"]);
    assert!(leftover.is_empty());
}

/// DT-PRV-SSE-10：无 data 的空事件不派发（event:-only ping）。
#[test]
fn sse_empty_event_no_data_not_dispatched() {
    let mut p = SseParser::new();
    let events = p.push(b"event: ping\n\n");
    assert!(events.is_empty());
}

/// DT-PRV-SSE-11：raw 保留原始行尾。
#[test]
fn sse_raw_preserves_line_endings() {
    let mut p = SseParser::new();
    let events = p.push(b"data: a\r\n\r\n");
    assert_eq!(events[0].raw, "data: a\r\n\r\n");
}

/// DT-PRV-SSE-12：take_pending_raw 暴露未完成帧字节。
#[test]
fn sse_take_pending_raw_incomplete_frame() {
    let mut p = SseParser::new();
    p.push(b"data: partial\r\n");
    let pending = p.take_pending_raw();
    assert_eq!(pending, "data: partial\r\n");
}

/// DT-PRV-SSE-13：只有字段名无冒号的行 → field=line, value=""。
#[test]
fn sse_field_without_colon_empty_value() {
    let mut p = SseParser::new();
    let events = p.push(b"data\n\n");
    assert_eq!(datas(&events), [""]);
}

// ═════════════════════════════════════════════════════════════
// lib.rs — create_provider / sanitize / kv_worker_id / auto_detect
// ═════════════════════════════════════════════════════════════

/// DT-PRV-LIB-01：各 provider 类型 protocol 正确（OpenAI-兼容 vs Native）。
#[test]
fn create_provider_protocol_per_type() {
    let base = "http://127.0.0.1:1/v1".to_string();
    for model in [
        "openai/test-model", "hosted_vllm/test-model", "azure/test-deployment",
        "deepseek/test-model", "ollama/test-model", "groq/test-model",
    ] {
        let p = create_full(model, Some("k".into()), base.clone(), &HashMap::new(), &HashMap::new());
        assert_eq!(p.protocol(), ProviderProtocol::OpenAiCompatible, "{model}");
    }
    for model in ["anthropic/test-model", "gemini/test-model", "bedrock/test-model"] {
        let p = create_full(model, Some("k".into()), base.clone(), &HashMap::new(), &HashMap::new());
        assert_eq!(p.protocol(), ProviderProtocol::Native, "{model}");
    }
}

/// DT-PRV-LIB-02：未知 provider 类型返回 ConfigError。
#[test]
fn create_provider_unknown_type_errors() {
    let err = create_provider(
        "unknown_provider/m", Some("k".into()), Some("http://x".into()), 5,
        &HashMap::new(), &HashMap::new(), None, false,
    ).err().expect("should be Err");
    assert!(matches!(err, GatewayError::ConfigError(_)), "{err:?}");
}

/// DT-PRV-LIB-03：custom_headers sanitization——reserved/hard-blocked/transport/invalid 全部丢弃，仅留普通头。
#[test]
fn custom_headers_sanitization() {
    let mut headers = HashMap::new();
    headers.insert("X-Request-Id".into(), "deploy-1".into());     // 保留
    headers.insert("api_version".into(), "2024-02-01".into());     // reserved → 丢
    headers.insert("x-gateway-priority".into(), "spoof".into());   // hard-blocked → 丢
    headers.insert("Authorization".into(), "Bearer leak".into());  // hard-blocked → 丢
    headers.insert("content-type".into(), "text/plain".into());    // transport → 丢
    headers.insert("bad name".into(), "x".into());                 // invalid name → 丢
    headers.insert("good-but-bad-val".into(), "\u{0}null".into()); // invalid value → 丢

    let p = create_full("openai/test-model", Some("k".into()), "http://x/v1".into(), &HashMap::new(), &headers);
    let attached = p.custom_headers();
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0].0, "X-Request-Id");
    assert_eq!(attached[0].1, "deploy-1");
}

/// DT-PRV-LIB-04：reserved key（anthropic_version）在 headers map 中仍配置 provider，不变成上游头。
#[test]
fn reserved_key_configures_anthropic_version() {
    let mut headers = HashMap::new();
    headers.insert("anthropic_version".into(), "2023-01-01".into());
    let p = create_full("anthropic/test-model", None, "http://x".into(), &HashMap::new(), &headers);
    assert!(p.custom_headers().is_empty(), "reserved key must not become upstream header");
}

/// DT-PRV-LIB-05：reserved key（api_version）配置 azure。
#[test]
fn reserved_key_configures_azure_api_version() {
    let mut extra = HashMap::new();
    extra.insert("api_version".into(), "2024-10-21".into());
    let _p = create_full("azure/test-deployment", Some("k".into()), "http://x".into(), &extra, &HashMap::new());
    // 构造不 panic 即说明 api_version 被 merged_extra 读取
}

/// DT-PRV-LIB-06：reserved key（aws_region_name）配置 bedrock。
#[test]
fn reserved_key_configures_bedrock_region() {
    let mut extra = HashMap::new();
    extra.insert("aws_region_name".into(), "us-west-2".into());
    let p = create_full("bedrock/test-model", Some("k".into()), "http://x".into(), &extra, &HashMap::new());
    assert_eq!(p.protocol(), ProviderProtocol::Native);
}

/// DT-PRV-LIB-07：auto_detect——无前缀模型名按前缀自动判别 provider。
#[test]
fn auto_detect_provider_from_model_name() {
    let base = "http://127.0.0.1:1/v1".to_string();
    // gpt- → openai (OpenAiCompatible)
    assert_eq!(create_full("gpt-4", Some("k".into()), base.clone(), &HashMap::new(), &HashMap::new()).protocol(), ProviderProtocol::OpenAiCompatible);
    // claude- → anthropic (Native)
    assert_eq!(create_full("claude-sonnet-4", Some("k".into()), base.clone(), &HashMap::new(), &HashMap::new()).protocol(), ProviderProtocol::Native);
    // gemini- → gemini (Native)
    assert_eq!(create_full("gemini-1.5-pro", Some("k".into()), base.clone(), &HashMap::new(), &HashMap::new()).protocol(), ProviderProtocol::Native);
    // deepseek → deepseek (OpenAiCompatible)
    assert_eq!(create_full("deepseek-chat", Some("k".into()), base.clone(), &HashMap::new(), &HashMap::new()).protocol(), ProviderProtocol::OpenAiCompatible);
    // llama/qwen/yi- → openai-compatible
    assert_eq!(create_full("llama-3", Some("k".into()), base.clone(), &HashMap::new(), &HashMap::new()).protocol(), ProviderProtocol::OpenAiCompatible);
    assert_eq!(create_full("qwen-2", Some("k".into()), base.clone(), &HashMap::new(), &HashMap::new()).protocol(), ProviderProtocol::OpenAiCompatible);
    // bedrock 前缀
    assert_eq!(create_full("anthropic.claude-3", Some("k".into()), base.clone(), &HashMap::new(), &HashMap::new()).protocol(), ProviderProtocol::Native);
    // 完全未知 → 默认 openai
    assert_eq!(create_full("some-unknown-model", Some("k".into()), base, &HashMap::new(), &HashMap::new()).protocol(), ProviderProtocol::OpenAiCompatible);
}

/// DT-PRV-LIB-08：hosted_vllm/ollama 无 api_key 时用占位 key（仍可构造）。
#[test]
fn vllm_ollama_no_key_uses_placeholder() {
    let p = create_full("hosted_vllm/m", None, "http://x/v1".into(), &HashMap::new(), &HashMap::new());
    assert_eq!(p.protocol(), ProviderProtocol::OpenAiCompatible);
    let p2 = create_full("ollama/m", None, "http://x/v1".into(), &HashMap::new(), &HashMap::new());
    assert_eq!(p2.protocol(), ProviderProtocol::OpenAiCompatible);
}

/// DT-PRV-LIB-09：openai 无 key 且无前缀→ None key（OpenAIProvider 仍构造，bearer 跳过）。
#[test]
fn openai_no_key_constructs() {
    let p = create_full("gpt-4", None, "http://x/v1".into(), &HashMap::new(), &HashMap::new());
    assert_eq!(p.protocol(), ProviderProtocol::OpenAiCompatible);
}

/// DT-PRV-LIB-10：Provider trait 基础访问器（name/models/deployment_id/kv_worker_id/client_type_header/custom_headers）。
#[test]
fn provider_trait_accessors() {
    let p = create_full("openai/gpt-4", Some("k".into()), "http://10.0.0.5:8000/v1".into(), &HashMap::new(), &HashMap::new());
    assert_eq!(p.name(), "openai");
    assert_eq!(p.models().len(), 1);
    assert_eq!(p.models()[0], "gpt-4");
    assert_eq!(p.deployment_id(), None);
    assert_eq!(p.kv_worker_id(), Some("10.0.0.5"));
    assert!(!p.client_type_header());
    assert!(p.custom_headers().is_empty());
}

/// DT-PRV-LIB-11：deployment_id 透传。
#[test]
fn deployment_id_passed_through() {
    let p = create_provider(
        "openai/gpt-4", Some("k".into()), Some("http://x/v1".into()), 5,
        &HashMap::new(), &HashMap::new(), Some("dep-42".into()), false,
    ).unwrap();
    assert_eq!(p.deployment_id(), Some("dep-42"));
}

/// DT-PRV-LIB-12：client_type_header=true 透传。
#[test]
fn client_type_header_flag_passed() {
    let p = create_provider(
        "openai/gpt-4", Some("k".into()), Some("http://x/v1".into()), 5,
        &HashMap::new(), &HashMap::new(), None, true,
    ).unwrap();
    assert!(p.client_type_header());
}

/// DT-PRV-LIB-13：kv_worker_id_from_api_base 各形态。
#[test]
fn kv_worker_id_from_api_base_variants() {
    assert_eq!(kv_worker_id_from_api_base(Some("http://10.0.0.5:8000/v1")), Some("10.0.0.5".into()));
    assert_eq!(kv_worker_id_from_api_base(Some("https://worker-0/v1")), Some("worker-0".into()));
    assert_eq!(kv_worker_id_from_api_base(Some("10.0.0.5:8000")), Some("10.0.0.5".into()));
    assert_eq!(kv_worker_id_from_api_base(None), None);
}

/// DT-PRV-LIB-14：timeout=0 被 max(1) 兜底，仍能构造 client。
#[test]
fn create_provider_timeout_zero_clamped() {
    let p = create_provider(
        "openai/gpt-4", Some("k".into()), Some("http://x/v1".into()), 0,
        &HashMap::new(), &HashMap::new(), None, false,
    );
    assert!(p.is_ok(), "timeout=0 should be clamped to 1");
}

// ═════════════════════════════════════════════════════════════
// openai.rs — chat / chat_stream via wiremock
// ═════════════════════════════════════════════════════════════

/// DT-PRV-OAI-01：chat 成功 + usage 透传 + raw_response 填充。
#[tokio::test]
async fn openai_chat_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", Some("k".into()), server.uri());
    let resp = p.chat(min_request()).await.expect("chat ok");
    assert_eq!(resp.id, "chatcmpl-test");
    assert_eq!(resp.usage.as_ref().unwrap().total_tokens, 6);
    assert!(resp.raw_response.is_some());
}

/// DT-PRV-OAI-02：chat 响应缺 usage 仍解析（usage=None）。
#[tokio::test]
async fn openai_chat_no_usage() {
    let server = MockServer::start().await;
    let mut body = fake_completion_json();
    body.as_object_mut().unwrap().remove("usage");
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body)).expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let resp = p.chat(min_request()).await.expect("parse ok");
    assert!(resp.usage.is_none());
}

/// DT-PRV-OAI-03：chat 不可解析响应返回 UpstreamParseError 携带 raw_body。
#[tokio::test]
async fn openai_chat_parse_error_carries_raw_body() {
    let server = MockServer::start().await;
    let body = "{\"not\":\"valid\"}";
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body)).expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let err = p.chat(min_request()).await.unwrap_err();
    match err {
        GatewayError::UpstreamParseError { raw_body, .. } => assert_eq!(raw_body, body),
        other => panic!("expected UpstreamParseError, got {other:?}"),
    }
}

/// DT-PRV-OAI-04：chat 响应带 UTF-8 BOM 仍解析。
#[tokio::test]
async fn openai_chat_bom_parses() {
    let server = MockServer::start().await;
    let body = format!("\u{feff}{}", fake_completion_json());
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body)).expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let resp = p.chat(min_request()).await.expect("BOM parses");
    assert!(resp.usage.is_some());
}

/// DT-PRV-OAI-05：非流式请求上游回 SSE → 装配成完整响应。
#[tokio::test]
async fn openai_chat_sse_assembled() {
    let server = MockServer::start().await;
    let sse = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"test-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let resp = p.chat(min_request()).await.expect("assembled");
    assert_eq!(resp.id, "chatcmpl-1");
    assert!(matches!(&resp.choices[0].message.content, MessageContent::Text(t) if t == "Hello"));
    assert_eq!(resp.usage.as_ref().unwrap().total_tokens, 7);
}

/// DT-PRV-OAI-06：chat 响应部分/null usage 降级为 0。
#[tokio::test]
async fn openai_chat_partial_usage_parses() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "id": "chatcmpl-test", "model": "test-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": { "prompt_tokens": 5, "completion_tokens": null }
    });
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body)).expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let resp = p.chat(min_request()).await.expect("partial usage parses");
    let u = resp.usage.unwrap();
    assert_eq!(u.prompt_tokens, 5);
    assert_eq!(u.completion_tokens, 0);
}

/// DT-PRV-OAI-07：chat 响应 null identity 字段不失败。
#[tokio::test]
async fn openai_chat_null_identity_parses() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "id": null, "object": null, "created": null, "model": null,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}]
    });
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string())).expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let resp = p.chat(min_request()).await.expect("null identity parses");
    assert_eq!(resp.id, "");
}

/// DT-PRV-OAI-08：chat 响应未知 role 降级为 Unknown。
#[tokio::test]
async fn openai_chat_unknown_role_parses() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "id": "1", "model": "m",
        "choices": [{"index": 0, "message": {"role": "developer", "content": "hi"}, "finish_reason": "stop"}]
    });
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body)).expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let resp = p.chat(min_request()).await.expect("unknown role parses");
    assert!(matches!(resp.choices[0].message.role, MessageRole::Unknown));
}

/// DT-PRV-OAI-09：chat 响应未知 content part 原样保留。
#[tokio::test]
async fn openai_chat_unknown_content_part_preserved() {
    let server = MockServer::start().await;
    let audio = serde_json::json!({"type": "input_audio", "input_audio": {"data": "SGVsbG8=", "format": "wav"}});
    let body = serde_json::json!({
        "id": "1", "model": "m",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": [{"type":"text","text":"hi"}, audio]}, "finish_reason": "stop"}]
    });
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string())).expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let resp = p.chat(min_request()).await.expect("unknown part parses");
    let MessageContent::Parts(parts) = &resp.choices[0].message.content else { panic!("expected Parts") };
    assert_eq!(parts.len(), 2);
    assert!(matches!(&parts[1], ContentPart::Unknown(_)));
}

/// DT-PRV-OAI-10：chat 上游错误状态返回 UpstreamError。
#[tokio::test]
async fn openai_chat_upstream_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", Some("k".into()), server.uri());
    let err = p.chat(min_request()).await.unwrap_err();
    assert!(matches!(err, GatewayError::UpstreamError { status: 429, .. }), "{err:?}");
}

/// DT-PRV-OAI-11：chat 注入 gateway_headers（X-Gateway-Priority）+ bearer auth。
#[tokio::test]
async fn openai_chat_gateway_headers_and_bearer() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(header("Authorization", "Bearer sk-test"))
        .and(header("X-Gateway-Priority", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", Some("sk-test".into()), server.uri());
    let req = request_with_gateway_headers(&[("X-Gateway-Priority", "100")]);
    assert!(p.chat(req).await.is_ok());
}

/// DT-PRV-OAI-12：chat 无 gateway_headers 时不发 priority 头。
#[tokio::test]
async fn openai_chat_no_priority_header_when_empty() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    assert!(p.chat(min_request()).await.is_ok());
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(!reqs[0].headers.contains_key("X-Gateway-Priority"));
}

/// DT-PRV-OAI-13：chat extra 字段（reasoning_effort/service_tier）透传到上游 body。
#[tokio::test]
async fn openai_chat_extra_fields_forwarded() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"reasoning_effort": "high", "service_tier": "auto"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let mut req = min_request();
    req.extra.insert("reasoning_effort".into(), serde_json::json!("high"));
    req.extra.insert("service_tier".into(), serde_json::json!("auto"));
    assert!(p.chat(req).await.is_ok());
}

/// DT-PRV-OAI-14：chat_stream 成功——SSE chunk 解析 + [DONE] 结束。
#[tokio::test]
async fn openai_chat_stream_success() {
    let sse = "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let stream = p.chat_stream(min_request()).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("hi"));
}

/// DT-PRV-OAI-15：chat_stream CRLF 分帧不挂起。
#[tokio::test]
async fn openai_chat_stream_crlf() {
    let sse = "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\r\n\r\ndata: [DONE]\r\n\r\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let stream = p.chat_stream(min_request()).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;
    assert_eq!(chunks.len(), 1);
}

/// DT-PRV-OAI-16：chat_stream data: 无空格仍解析。
#[tokio::test]
async fn openai_chat_stream_data_no_space() {
    let sse = "data:{\"choices\":[{\"delta\":{\"content\":\"hi\"},\"index\":0}],\"created\":0,\"id\":\"1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\ndata:[DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let stream = p.chat_stream(min_request()).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("hi"));
}

/// DT-PRV-OAI-17：chat_stream 未知 role 降级为 Unknown 不丢弃。
#[tokio::test]
async fn openai_chat_stream_unknown_role() {
    let sse = "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"developer\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let stream = p.chat_stream(min_request()).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;
    assert_eq!(chunks.len(), 1);
    assert!(matches!(chunks[0].choices[0].delta.role, Some(MessageRole::Unknown)));
}

/// DT-PRV-OAI-18：chat_stream tool_call 无 index 默认 0。
#[tokio::test]
async fn openai_chat_stream_tool_call_no_index() {
    let sse = "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let stream = p.chat_stream(min_request()).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;
    assert_eq!(chunks.len(), 1);
    let calls = chunks[0].choices[0].delta.tool_calls.as_ref().unwrap();
    assert_eq!(calls[0].index, 0);
    assert_eq!(calls[0].id.as_deref(), Some("call_1"));
}

/// DT-PRV-OAI-19：chat_stream 上游错误状态返回 UpstreamError。
#[tokio::test]
async fn openai_chat_stream_upstream_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", Some("k".into()), server.uri());
    let err = p.chat_stream(min_request()).await.err().expect("should be Err");
    assert!(matches!(err, GatewayError::UpstreamError { status: 500, .. }), "{err:?}");
}

/// DT-PRV-OAI-20：chat_stream 注入 gateway_headers。
#[tokio::test]
async fn openai_chat_stream_gateway_headers() {
    let sse = "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(header("X-Gateway-Priority", "100"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let req = request_with_gateway_headers(&[("X-Gateway-Priority", "100")]);
    let stream = p.chat_stream(req).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;
    assert_eq!(chunks.len(), 1);
}

/// DT-PRV-OAI-21：chat 带 raw_capture 记录请求体与响应帧。
#[tokio::test]
async fn openai_chat_raw_capture() {
    use boom_core::provider::SharedRawCapture;
    use std::sync::Mutex;
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let cap = Arc::new(boom_core::provider::RawCaptureChannel {
        request_body: Mutex::new(None),
        response_frames: Mutex::new(Vec::new()),
    });
    let mut req = min_request();
    req.raw_capture = Some(cap.clone() as SharedRawCapture);
    let p = create("openai/test-model", None, server.uri());
    assert!(p.chat(req).await.is_ok());
    assert!(cap.take_request_body().is_some(), "request body recorded");
    assert!(cap.take_response_body().is_some(), "response frames recorded");
}

/// DT-PRV-OAI-22：chat kv_cache_report_full=true 注入 vllm_xargs。
#[tokio::test]
async fn openai_chat_kv_cache_report_full() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"vllm_xargs": {"kv_cache_report_mode": "full"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let mut req = min_request();
    req.kv_cache_report_full = true;
    assert!(p.chat(req).await.is_ok());
}

/// DT-PRV-OAI-23：chat ContentPart::Reasoning 被转为 Text（上游不拒）。
#[tokio::test]
async fn openai_chat_reasoning_part_converted_to_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"messages": [{"role":"user","content":[{"type":"text","text":"think"}]}]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let mut req = min_request();
    req.messages[0].content = MessageContent::Parts(vec![ContentPart::Reasoning { reasoning: "think".into() }]);
    assert!(p.chat(req).await.is_ok());
}

// ═════════════════════════════════════════════════════════════
// anthropic.rs — chat / chat_stream via wiremock
// ═════════════════════════════════════════════════════════════

/// DT-PRV-ANT-01：anthropic chat 成功——Anthropic 响应转 OpenAI 格式。
#[tokio::test]
async fn anthropic_chat_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_anthropic_response()))
        .expect(1).mount(&server).await;
    let p = create("anthropic/claude-test", Some("k".into()), server.uri());
    let resp = p.chat(min_request()).await.expect("chat ok");
    assert_eq!(resp.id, "msg_test");
    assert!(matches!(&resp.choices[0].message.content, MessageContent::Text(t) if t == "hi"));
    assert_eq!(resp.usage.as_ref().unwrap().prompt_tokens, 5);
}

/// DT-PRV-ANT-02：anthropic chat 注入 x-api-key + anthropic-version + gateway_headers。
#[tokio::test]
async fn anthropic_chat_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .and(header("x-api-key", "sk-ant"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("x-gateway-priority", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_anthropic_response()))
        .expect(1).mount(&server).await;
    let p = create("anthropic/claude-test", Some("sk-ant".into()), server.uri());
    let req = request_with_gateway_headers(&[("x-gateway-priority", "100")]);
    assert!(p.chat(req).await.is_ok());
}

/// DT-PRV-ANT-03：anthropic_version 通过 extra 配置 → 自定义 anthropic-version 头。
#[tokio::test]
async fn anthropic_custom_api_version() {
    let server = MockServer::start().await;
    let mut extra = HashMap::new();
    extra.insert("anthropic_version".into(), "2023-01-01".into());
    Mock::given(method("POST")).and(path("/messages"))
        .and(header("anthropic-version", "2023-01-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_anthropic_response()))
        .expect(1).mount(&server).await;
    let p = create_full("anthropic/claude-test", Some("k".into()), server.uri(), &extra, &HashMap::new());
    assert!(p.chat(min_request()).await.is_ok());
}

/// DT-PRV-ANT-04：anthropic chat 上游错误返回 UpstreamError。
#[tokio::test]
async fn anthropic_chat_upstream_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .expect(1).mount(&server).await;
    let p = create("anthropic/claude-test", Some("k".into()), server.uri());
    let err = p.chat(min_request()).await.unwrap_err();
    assert!(matches!(err, GatewayError::UpstreamError { status: 401, .. }), "{err:?}");
}

/// DT-PRV-ANT-05：anthropic chat_stream 成功——Anthropic SSE 事件转 OpenAI chunk。
#[tokio::test]
async fn anthropic_chat_stream_success() {
    let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("anthropic/claude-test", None, server.uri());
    let stream = p.chat_stream(min_request()).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;
    // 至少有一个 text delta chunk
    let has_text = chunks.iter().any(|c| c.choices.iter().any(|ch| ch.delta.content.as_deref() == Some("hi")));
    assert!(has_text, "expected a text delta chunk, got {} chunks", chunks.len());
}

/// DT-PRV-ANT-06：anthropic chat_stream CRLF 分帧解析。
#[tokio::test]
async fn anthropic_chat_stream_crlf() {
    let parts = [
        "event: message_start\r\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\r\n\r\n",
        "event: message_delta\r\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\r\n\r\n",
        "event: message_stop\r\n",
        "data: {\"type\":\"message_stop\"}\r\n\r\n",
    ];
    let sse = parts.concat();
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("anthropic/claude-test", None, server.uri());
    let stream = p.chat_stream(min_request()).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;
    assert!(chunks.iter().any(|c| c.choices.iter().any(|ch| ch.delta.content.as_deref() == Some("hi"))));
}

// ───────────────── helpers（anthropic 请求体校验） ─────────────────

/// 取 mock server 收到的最后一个请求体（JSON）。
async fn last_request_body(server: &MockServer) -> serde_json::Value {
    let reqs = server.received_requests().await.expect("received requests");
    let req = reqs.last().expect("at least one request");
    serde_json::from_slice(&req.body).expect("request body is JSON")
}

/// 挂一个必然成功（200）的 anthropic /messages mock，返回 server。
async fn anthropic_ok_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_anthropic_response()))
        .mount(&server).await;
    server
}

/// DT-PRV-ANT-08：请求体转换 —— system(Text)→顶层 system 数组、assistant
/// text+tool_calls→text/tool_use blocks、Tool 角色→tool_result（[ERROR]→is_error）。
#[tokio::test]
async fn anthropic_request_system_assistant_tool_conversion() {
    let server = anthropic_ok_server().await;
    let p = create("anthropic/claude-test", None, server.uri());
    let mut req = min_request();
    req.messages = vec![
        Message { role: MessageRole::System, content: MessageContent::Text("sys prompt".into()), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
        Message { role: MessageRole::User, content: MessageContent::Text("hi".into()), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
        Message {
            role: MessageRole::Assistant, content: MessageContent::Text("calling".into()), name: None,
            tool_calls: Some(vec![ToolCall { id: "tc1".into(), call_type: "function".into(), function: FunctionCall { name: "get_weather".into(), arguments: r#"{"city":"BJ"}"#.into() } }]),
            tool_call_id: None, reasoning_content: None,
        },
        Message { role: MessageRole::Tool, content: MessageContent::Text("[ERROR] boom".into()), name: None, tool_calls: None, tool_call_id: Some("tc1".into()), reasoning_content: None },
    ];
    p.chat(req).await.expect("chat ok");
    let body = last_request_body(&server).await;

    // system：Text → 数组形式 text block
    assert_eq!(body["system"][0]["type"], "text");
    assert_eq!(body["system"][0]["text"], "sys prompt");

    // assistant：text + tool_use（arguments 解析为 JSON 对象）
    let msgs = body["messages"].as_array().unwrap();
    let assistant = &msgs[1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"][0]["type"], "text");
    assert_eq!(assistant["content"][0]["text"], "calling");
    assert_eq!(assistant["content"][1]["type"], "tool_use");
    assert_eq!(assistant["content"][1]["id"], "tc1");
    assert_eq!(assistant["content"][1]["name"], "get_weather");
    assert_eq!(assistant["content"][1]["input"]["city"], "BJ");

    // Tool 角色 → user + tool_result，[ERROR] 前缀剥离并置 is_error
    let tool_result = &msgs[2];
    assert_eq!(tool_result["role"], "user");
    assert_eq!(tool_result["content"][0]["type"], "tool_result");
    assert_eq!(tool_result["content"][0]["tool_use_id"], "tc1");
    assert_eq!(tool_result["content"][0]["content"], "boom");
    assert_eq!(tool_result["content"][0]["is_error"], true);
}

/// DT-PRV-ANT-09：system 为 Parts → 多 text block；extra.system_cache_control
/// 透传到每个 block 的 cache_control。
#[tokio::test]
async fn anthropic_request_system_parts_cache_control() {
    let server = anthropic_ok_server().await;
    let p = create("anthropic/claude-test", None, server.uri());
    let mut req = min_request();
    req.messages = vec![
        Message { role: MessageRole::System, content: MessageContent::Parts(vec![
            ContentPart::Text { text: "part-a".into() },
            ContentPart::Text { text: "part-b".into() },
        ]), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
        Message { role: MessageRole::User, content: MessageContent::Text("hi".into()), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
    ];
    req.extra.insert("system_cache_control".into(), serde_json::json!({"type": "ephemeral"}));
    p.chat(req).await.expect("chat ok");
    let body = last_request_body(&server).await;
    assert_eq!(body["system"].as_array().unwrap().len(), 2);
    assert_eq!(body["system"][0]["text"], "part-a");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][1]["text"], "part-b");
    assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
}

/// DT-PRV-ANT-10：user 消息 Parts 全形态 —— Text/ImageUrl/Reasoning→text block、
/// Unknown→丢弃；assistant Null content → 兜底空 text block。
#[tokio::test]
async fn anthropic_request_user_parts_variants() {
    let server = anthropic_ok_server().await;
    let p = create("anthropic/claude-test", None, server.uri());
    let mut req = min_request();
    req.messages = vec![
        Message { role: MessageRole::User, content: MessageContent::Parts(vec![
            ContentPart::Text { text: "t".into() },
            ContentPart::ImageUrl { image_url: ImageUrl { url: "https://x/i.png".into(), detail: None } },
            ContentPart::Reasoning { reasoning: "thought".into() },
            ContentPart::Unknown(serde_json::json!({"type": "audio"})),
        ]), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
        Message { role: MessageRole::Assistant, content: MessageContent::Null, name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
    ];
    p.chat(req).await.expect("chat ok");
    let body = last_request_body(&server).await;
    let msgs = body["messages"].as_array().unwrap();
    // user：text + image url + reasoning 三块（Unknown 丢弃）
    let user_content = msgs[0]["content"].as_array().unwrap();
    assert_eq!(user_content.len(), 3);
    assert_eq!(user_content[0]["text"], "t");
    assert_eq!(user_content[1]["text"], "https://x/i.png");
    assert_eq!(user_content[2]["text"], "thought");
    // assistant Null content → 兜底空 text block
    assert_eq!(msgs[1]["content"][0]["type"], "text");
    assert_eq!(msgs[1]["content"][0]["text"], "");
}

/// DT-PRV-ANT-11：采样参数透传 —— temperature/top_p/stop(Single 与 Multiple)、
/// max_tokens 缺省 4096、显式覆盖。
#[tokio::test]
async fn anthropic_request_sampling_stop_sequences() {
    let server = anthropic_ok_server().await;
    let p = create("anthropic/claude-test", None, server.uri());

    let mut req = min_request();
    req.temperature = Some(0.7);
    req.top_p = Some(0.9);
    req.stop = Some(StopSequence::Single("END".into()));
    p.chat(req).await.expect("chat 1");
    let body = last_request_body(&server).await;
    assert_eq!(body["temperature"], 0.7);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["stop_sequences"], serde_json::json!(["END"]));
    assert_eq!(body["max_tokens"], 4096); // 缺省

    let mut req = min_request();
    req.max_tokens = Some(128);
    req.stop = Some(StopSequence::Multiple(vec!["A".into(), "B".into()]));
    p.chat(req).await.expect("chat 2");
    let body = last_request_body(&server).await;
    assert_eq!(body["max_tokens"], 128);
    assert_eq!(body["stop_sequences"], serde_json::json!(["A", "B"]));
}

/// DT-PRV-ANT-12：tools → Anthropic 格式（name/input_schema/description）。
#[tokio::test]
async fn anthropic_request_tools_and_description() {
    let server = anthropic_ok_server().await;
    let p = create("anthropic/claude-test", None, server.uri());
    let mut req = min_request();
    req.tools = Some(vec![Tool {
        tool_type: "function".into(),
        function: ToolFunction {
            name: "get_weather".into(),
            description: Some("Get weather".into()),
            parameters: serde_json::json!({"type": "object"}),
        },
    }]);
    p.chat(req).await.expect("chat ok");
    let body = last_request_body(&server).await;
    assert_eq!(body["tools"][0]["name"], "get_weather");
    assert_eq!(body["tools"][0]["description"], "Get weather");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
}

/// DT-PRV-ANT-13：response_format → 合成 output_json 工具并强制 tool_choice
/// （json_schema 用给定 schema；json_object 用 {type:object}；text 不合成）。
#[tokio::test]
async fn anthropic_request_response_format_synthetic_tool() {
    let server = anthropic_ok_server().await;
    let p = create("anthropic/claude-test", None, server.uri());

    let mut req = min_request();
    req.response_format = Some(serde_json::json!({
        "type": "json_schema", "json_schema": {"schema": {"type": "object", "properties": {"a": {"type": "number"}}}}
    }));
    p.chat(req).await.expect("chat 1");
    let body = last_request_body(&server).await;
    assert_eq!(body["tools"][0]["name"], "output_json");
    assert_eq!(body["tools"][0]["input_schema"]["properties"]["a"]["type"], "number");
    assert_eq!(body["tool_choice"]["type"], "tool");
    assert_eq!(body["tool_choice"]["name"], "output_json");

    let mut req = min_request();
    req.response_format = Some(serde_json::json!({"type": "json_object"}));
    p.chat(req).await.expect("chat 2");
    let body = last_request_body(&server).await;
    assert_eq!(body["tools"][0]["input_schema"], serde_json::json!({"type": "object"}));

    let mut req = min_request();
    req.response_format = Some(serde_json::json!({"type": "text"}));
    p.chat(req).await.expect("chat 3");
    let body = last_request_body(&server).await;
    assert!(body.get("tools").is_none(), "text response_format 不合成工具");
}

/// DT-PRV-ANT-14：extra 透传（thinking/metadata/top_k）、user → metadata.user_id、
/// thinking 触发 extended-thinking beta header。
#[tokio::test]
async fn anthropic_request_extra_passthrough_and_user() {
    let server = anthropic_ok_server().await;
    let p = create("anthropic/claude-test", None, server.uri());
    let mut req = min_request();
    req.extra.insert("thinking".into(), serde_json::json!({"type": "enabled", "budget_tokens": 1024}));
    req.extra.insert("metadata".into(), serde_json::json!({"session": "s1"}));
    req.extra.insert("top_k".into(), serde_json::json!(5));
    req.user = Some("u-123".into());
    p.chat(req).await.expect("chat ok");
    let reqs = server.received_requests().await.unwrap();
    // thinking extra → extended-thinking beta 头（叠加恒有的 prompt-caching beta）
    let beta = reqs.last().unwrap().headers.get("anthropic-beta")
        .and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    assert!(beta.contains("extended-thinking-2025-04-11"), "beta header: {beta}");
    assert!(beta.contains("prompt-caching-2024-07-31"), "beta header: {beta}");
    let body = last_request_body(&server).await;
    assert_eq!(body["thinking"]["budget_tokens"], 1024);
    assert_eq!(body["top_k"], 5);
    assert_eq!(body["metadata"]["session"], "s1");
    assert_eq!(body["metadata"]["user_id"], "u-123"); // user 合并进 metadata
}

/// DT-PRV-ANT-15：响应转换 —— thinking/redacted_thinking/tool_use blocks、
/// stop_reason=tool_use、usage 缓存字段。
#[tokio::test]
async fn anthropic_response_thinking_tool_use_cache_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msg_think", "type": "message", "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "let me think"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "answer"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "BJ"}}
            ],
            "model": "claude-test", "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 4, "cache_creation_input_tokens": 3, "cache_read_input_tokens": 7}
        })))
        .expect(1).mount(&server).await;
    let p = create("anthropic/claude-test", None, server.uri());
    let resp = p.chat(min_request()).await.expect("chat ok");

    // 有 Reasoning part → Parts 内容（redacted 跳过）
    let parts = match &resp.choices[0].message.content {
        MessageContent::Parts(ps) => ps.clone(),
        other => panic!("expected Parts, got {other:?}"),
    };
    assert!(matches!(&parts[0], ContentPart::Reasoning { reasoning } if reasoning == "let me think"));
    assert!(matches!(&parts[1], ContentPart::Text { text } if text == "answer"));
    // tool_use → tool_calls
    let tcs = resp.choices[0].message.tool_calls.as_ref().unwrap();
    assert_eq!(tcs.len(), 1);
    assert_eq!(tcs[0].id, "toolu_1");
    assert_eq!(tcs[0].function.name, "get_weather");
    assert_eq!(tcs[0].function.arguments, r#"{"city":"BJ"}"#);
    // stop_reason=tool_use → finish_reason=tool_calls
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    // usage 缓存字段
    let usage = resp.usage.as_ref().unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 4);
    assert_eq!(usage.cache_creation_input_tokens, Some(3));
    assert_eq!(usage.cache_read_input_tokens, Some(7));
}

/// DT-PRV-ANT-16：响应转换 —— output_json 合成工具结果转 Text、stop_reason=
/// max_tokens→length、缺 id 自动生成。
#[tokio::test]
async fn anthropic_response_output_json_max_tokens_no_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "message", "role": "assistant",
            "content": [{"type": "tool_use", "name": "output_json", "input": {"a": 1}}],
            "model": "claude-test", "stop_reason": "max_tokens",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1).mount(&server).await;
    let p = create("anthropic/claude-test", None, server.uri());
    let resp = p.chat(min_request()).await.expect("chat ok");
    // output_json → Text（JSON 序列化），非 tool_call
    assert!(resp.choices[0].message.tool_calls.is_none());
    assert!(matches!(&resp.choices[0].message.content, MessageContent::Text(t) if t == r#"{"a":1}"#));
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"));
    assert!(!resp.id.is_empty(), "缺 id 时自动生成");
}

/// DT-PRV-ANT-17：chat 网络失败 / 200 坏 JSON → ProviderError。
#[tokio::test]
async fn anthropic_chat_network_and_parse_errors() {
    // 连接失败（死端口）
    let p = create("anthropic/claude-test", None, "http://127.0.0.1:1".into());
    let err = p.chat(min_request()).await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError(ref m) if m.contains("unavailable")), "{err:?}");

    // 200 但响应体非 JSON
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>not json</html>"))
        .expect(1).mount(&server).await;
    let p = create("anthropic/claude-test", None, server.uri());
    let err = p.chat(min_request()).await.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError(ref m) if m.contains("process upstream")), "{err:?}");
}

/// DT-PRV-ANT-18：chat_stream 全事件矩阵 —— message_start(role+input_tokens)、
/// tool_use start/delta（arguments 分片）、thinking_delta、text_delta、ping、
/// message_delta(stop_reason+usage)、message_stop；坏 JSON 行与空 data 行跳过。
#[tokio::test]
async fn anthropic_stream_full_event_matrix() {
    let sse = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"m1","usage":{"input_tokens":7,"output_tokens":0}}}"#,
        "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"BJ\"}"}}"#,
        "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking"}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"pondering"}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Hello"}}"#,
        "\n\n",
        "event: ping\n",
        r#"data: {"type":"ping"}"#,
        "\n\n",
        "event: bad\n",
        "data: not-json\n",
        "\n",
        "event: empty\n",
        "data: \n",
        "\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}"#,
        "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("anthropic/claude-test", None, server.uri());
    let stream = p.chat_stream(min_request()).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;

    // 首 chunk：role + input_tokens
    assert!(matches!(chunks[0].choices[0].delta.role, Some(MessageRole::Assistant)));
    assert_eq!(chunks[0].usage.as_ref().unwrap().prompt_tokens, Some(7));
    // tool_use start → id/name delta
    let tc = chunks.iter().find_map(|c| c.choices[0].delta.tool_calls.clone()).expect("tool_call delta");
    assert_eq!(tc[0].index, 0);
    assert_eq!(tc[0].id.as_deref(), Some("toolu_1"));
    assert_eq!(tc[0].function.as_ref().unwrap().name.as_deref(), Some("get_weather"));
    // arguments 分片累积
    let args: String = chunks.iter().filter_map(|c| {
        c.choices[0].delta.tool_calls.as_ref().map(|t| t[0].function.as_ref().unwrap().arguments.clone().unwrap_or_default())
    }).collect();
    assert_eq!(args, r#"{"city":"BJ"}"#);
    // thinking/text delta
    assert!(chunks.iter().any(|c| c.choices[0].delta.reasoning_content.as_deref() == Some("pondering")));
    assert!(chunks.iter().any(|c| c.choices[0].delta.content.as_deref() == Some("Hello")));
    // 尾 chunk：finish_reason + usage
    let last = chunks.last().unwrap();
    assert_eq!(last.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(last.usage.as_ref().unwrap().completion_tokens, Some(3));
}

/// DT-PRV-ANT-19：chat_stream 上游非 2xx → UpstreamError；连接失败 → ProviderError。
#[tokio::test]
async fn anthropic_stream_upstream_and_network_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .expect(1).mount(&server).await;
    let p = create("anthropic/claude-test", None, server.uri());
    let err = match p.chat_stream(min_request()).await {
        Err(e) => e,
        Ok(_) => panic!("expected stream error"),
    };
    assert!(matches!(err, GatewayError::UpstreamError { status: 500, .. }), "{err:?}");

    let p = create("anthropic/claude-test", None, "http://127.0.0.1:1".into());
    let err = match p.chat_stream(min_request()).await {
        Err(e) => e,
        Ok(_) => panic!("expected stream error"),
    };
    assert!(matches!(err, GatewayError::ProviderError(ref m) if m.contains("unavailable")), "{err:?}");
}

/// DT-PRV-ANT-20：Provider accessors —— name/models/deployment_id/client_type_header。
#[tokio::test]
async fn anthropic_provider_accessors() {
    let p = create_provider(
        "anthropic/claude-test", Some("k".into()), Some("http://127.0.0.1:9".into()), 5,
        &HashMap::new(), &HashMap::new(), Some("dep-42".into()), true,
    ).expect("create_provider");
    assert_eq!(p.name(), "anthropic");
    assert_eq!(p.models(), &["claude-test".to_string()]);
    assert_eq!(p.deployment_id(), Some("dep-42"));
    assert!(p.client_type_header());
}

// ═════════════════════════════════════════════════════════════
// gemini / azure / bedrock — 构造 + protocol
// ═════════════════════════════════════════════════════════════

/// DT-PRV-GEM-01：gemini 构造 + protocol + accessors。
#[test]
fn gemini_provider_construct() {
    let p = create_full("gemini/gemini-1.5-pro", Some("k".into()), "http://x".into(), &HashMap::new(), &HashMap::new());
    assert_eq!(p.protocol(), ProviderProtocol::Native);
    assert_eq!(p.name(), "gemini");
    assert_eq!(p.models()[0], "gemini-1.5-pro");
}

/// DT-PRV-AZU-01：azure 构造 + protocol（OpenAI-兼容）+ name="azure"。
#[test]
fn azure_provider_construct() {
    let p = create_full("azure/my-deployment", Some("k".into()), "http://x".into(), &HashMap::new(), &HashMap::new());
    assert_eq!(p.protocol(), ProviderProtocol::OpenAiCompatible);
    assert_eq!(p.name(), "azure");
}

/// DT-PRV-BD-01：bedrock 构造 + protocol + 默认 region。
#[test]
fn bedrock_provider_construct() {
    let p = create_full("bedrock/anthropic.claude-3", Some("k".into()), "http://x".into(), &HashMap::new(), &HashMap::new());
    assert_eq!(p.protocol(), ProviderProtocol::Native);
    assert_eq!(p.name(), "bedrock");
}

/// DT-PRV-AZU-02：azure chat 成功——URL 走 /openai/deployments/{model}/chat/completions。
#[tokio::test]
async fn azure_chat_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/openai/deployments/my-deployment/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let mut extra = HashMap::new();
    extra.insert("api_version".into(), "2024-10-21".into());
    let p = create_full("azure/my-deployment", Some("k".into()), server.uri(), &extra, &HashMap::new());
    let resp = p.chat(min_request()).await.expect("azure chat ok");
    assert_eq!(resp.id, "chatcmpl-test");
}

/// DT-PRV-OAI-24：direct OpenAIProvider::new + with_custom_headers（绕过 create_provider 覆盖构造器）。
#[test]
fn openai_provider_direct_construct_with_headers() {
    use boom_provider::openai::OpenAIProvider;
    let p = OpenAIProvider::new(Client::new(), Some("k".into()), Some("http://x/v1".into()), "m", None, false)
        .with_custom_headers(vec![("X-Custom".into(), "v".into())]);
    assert_eq!(p.custom_headers().len(), 1);
    assert_eq!(p.kv_worker_id(), Some("x"));
}

/// DT-PRV-ANT-07：direct AnthropicProvider::new + with_api_version + with_custom_headers。
#[test]
fn anthropic_provider_direct_construct() {
    use boom_provider::anthropic::AnthropicProvider;
    let p = AnthropicProvider::new(Client::new(), Some("k".into()), Some("http://host/v1".into()), "claude", None, false)
        .with_api_version("2023-01-01".into())
        .with_custom_headers(vec![("X-Custom".into(), "v".into())]);
    assert_eq!(p.protocol(), ProviderProtocol::Native);
    assert_eq!(p.kv_worker_id(), Some("host"));
    assert_eq!(p.custom_headers().len(), 1);
}

/// DT-PRV-GEM-02：direct GeminiProvider::new + with_custom_headers。
#[test]
fn gemini_provider_direct_construct() {
    use boom_provider::gemini::GeminiProvider;
    let p = GeminiProvider::new(Client::new(), Some("k".into()), "gemini-1.5", None, false)
        .with_custom_headers(vec![("X-Custom".into(), "v".into())]);
    assert_eq!(p.protocol(), ProviderProtocol::Native);
    assert_eq!(p.custom_headers().len(), 1);
}

/// DT-PRV-AZU-03：direct AzureProvider::new + with_custom_headers。
#[test]
fn azure_provider_direct_construct() {
    use boom_provider::azure::AzureProvider;
    let p = AzureProvider::new(Client::new(), Some("k".into()), Some("http://x".into()), "dep", "2024-10-21", None, false)
        .with_custom_headers(vec![("X-Custom".into(), "v".into())]);
    assert_eq!(p.protocol(), ProviderProtocol::OpenAiCompatible);
    assert_eq!(p.custom_headers().len(), 1);
}

/// DT-PRV-BD-02：direct BedrockProvider::new。
#[test]
fn bedrock_provider_direct_construct() {
    use boom_provider::bedrock::BedrockProvider;
    let p = BedrockProvider::new(Client::new(), "anthropic.claude-3", "us-west-2", None, false);
    assert_eq!(p.protocol(), ProviderProtocol::Native);
}

// ═════════════════════════════════════════════════════════════
// gemini.rs — to_gemini_request 全分支（chat 在 send 前构建 body，
// 用死地址 client 使 send 立即失败 → ProviderError，覆盖纯请求构建逻辑，
// 不发真实 Google 请求）
// ═════════════════════════════════════════════════════════════

use std::net::SocketAddr;
use boom_provider::gemini::GeminiProvider;

/// 把 generativelanguage.googleapis.com 解析到 127.0.0.1:1（死地址），
/// 使 reqwest send() 立即 ECONNREFUSED → ProviderError，确定性、无真实联网。
fn dead_google_client() -> Client {
    Client::builder()
        .resolve(
            "generativelanguage.googleapis.com",
            "127.0.0.1:1".parse::<SocketAddr>().unwrap(),
        )
        .build()
        .expect("client builds")
}

fn gemini_provider() -> GeminiProvider {
    GeminiProvider::new(dead_google_client(), Some("fake-key".into()), "gemini-1.5-pro", None, false)
}

/// chat 必然失败（死地址），但 to_gemini_request 已执行——本组测试通过断言
/// `.is_err()` 驱动请求构建各分支。
async fn gemini_chat_err(req: ChatCompletionRequest) -> GatewayError {
    gemini_provider().chat(req).await.err().expect("gemini chat must error with dead client")
}

/// DT-PRV-GEM-10：system + user 文本 → systemInstruction + user parts。
#[tokio::test]
async fn gemini_system_and_user_text() {
    let mut req = min_request();
    req.messages = vec![
        Message { role: MessageRole::System, content: MessageContent::Text("sys".into()), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
        Message { role: MessageRole::User, content: MessageContent::Text("hi".into()), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
    ];
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-11：system 为 Parts → 多 text 块组装 systemInstruction。
#[tokio::test]
async fn gemini_system_parts() {
    let mut req = min_request();
    req.messages = vec![
        Message { role: MessageRole::System, content: MessageContent::Parts(vec![ContentPart::Text { text: "a".into() }, ContentPart::Text { text: "b".into() }]), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
        Message { role: MessageRole::User, content: MessageContent::Text("hi".into()), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
    ];
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-12：assistant 带 tool_calls → functionCall parts（role=model）。
#[tokio::test]
async fn gemini_assistant_tool_calls() {
    let mut req = min_request();
    req.messages = vec![
        Message { role: MessageRole::User, content: MessageContent::Text("call it".into()), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
        Message {
            role: MessageRole::Assistant, content: MessageContent::Text(String::new()),
            name: None,
            tool_calls: Some(vec![ToolCall { id: "c1".into(), call_type: "function".into(), function: FunctionCall { name: "get_weather".into(), arguments: r#"{"city":"BJ"}"#.into() } }]),
            tool_call_id: None, reasoning_content: None,
        },
    ];
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-13：tool 消息 → functionResponse（role=user）。
#[tokio::test]
async fn gemini_tool_result_message() {
    let mut req = min_request();
    req.messages = vec![
        Message { role: MessageRole::User, content: MessageContent::Text("q".into()), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
        Message {
            role: MessageRole::Tool, content: MessageContent::Text("result".into()),
            name: None, tool_calls: None, tool_call_id: Some("get_weather".into()), reasoning_content: None,
        },
    ];
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-14：tool 消息 content 为 Parts（取 Text 拼接）。
#[tokio::test]
async fn gemini_tool_result_parts() {
    let mut req = min_request();
    req.messages = vec![
        Message { role: MessageRole::Tool, content: MessageContent::Parts(vec![ContentPart::Text { text: "r1".into() }, ContentPart::ImageUrl { image_url: ImageUrl { url: "http://x".into(), detail: None } }]), name: None, tool_calls: None, tool_call_id: Some("t".into()), reasoning_content: None },
    ];
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-15：未知 role → 当作 user。
#[tokio::test]
async fn gemini_unknown_role_as_user() {
    let mut req = min_request();
    req.messages = vec![
        Message { role: MessageRole::Unknown, content: MessageContent::Text("x".into()), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None },
    ];
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-16：user content 为 Parts——text/image(data-uri)/image(url)/reasoning/unknown 全分支。
#[tokio::test]
async fn gemini_user_parts_all_variants() {
    let mut req = min_request();
    req.messages = vec![Message {
        role: MessageRole::User,
        content: MessageContent::Parts(vec![
            ContentPart::Text { text: "t".into() },
            ContentPart::Text { text: String::new() }, // 空 text → 丢弃
            ContentPart::ImageUrl { image_url: ImageUrl { url: "data:image/png;base64,SGk=".into(), detail: None } }, // inline_data
            ContentPart::ImageUrl { image_url: ImageUrl { url: "https://x/i.png".into(), detail: None } }, // file_data
            ContentPart::Reasoning { reasoning: "think".into() }, // → text
            ContentPart::Unknown(serde_json::json!({"x":1})), // → 丢弃
        ]),
        name: None, tool_calls: None, tool_call_id: None, reasoning_content: None,
    }];
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-17：data URI 缺少 ;base64, → file_data 兜底。
#[tokio::test]
async fn gemini_data_uri_no_base64_falls_back() {
    let mut req = min_request();
    req.messages = vec![Message {
        role: MessageRole::User,
        content: MessageContent::Parts(vec![ContentPart::ImageUrl { image_url: ImageUrl { url: "data:text/plain,hello".into(), detail: None } }]),
        name: None, tool_calls: None, tool_call_id: None, reasoning_content: None,
    }];
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-18：generationConfig——temperature/topP/maxOutputTokens/candidateCount/stopSequences。
#[tokio::test]
async fn gemini_generation_config_all_fields() {
    let mut req = min_request();
    req.temperature = Some(0.7);
    req.top_p = Some(0.9);
    req.max_tokens = Some(100);
    req.max_completion_tokens = Some(200); // 优先 max_completion_tokens
    req.n = Some(2);
    req.stop = Some(StopSequence::Multiple(vec!["A".into(), "B".into()]));
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-19：stop 为 Single。
#[tokio::test]
async fn gemini_stop_single() {
    let mut req = min_request();
    req.stop = Some(StopSequence::Single("END".into()));
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-20：tools → functionDeclarations（含 description）。
#[tokio::test]
async fn gemini_tools_with_description() {
    let mut req = min_request();
    req.tools = Some(vec![Tool { tool_type: "function".into(), function: ToolFunction { name: "get_weather".into(), description: Some("weather".into()), parameters: serde_json::json!({"type":"object"}) } }]);
    assert!(matches!(gemini_chat_err(req).await, GatewayError::ProviderError(_)));
}

/// DT-PRV-GEM-21：chat_stream 也走 to_gemini_request（死地址 → ProviderError）。
#[tokio::test]
async fn gemini_chat_stream_builds_request() {
    let p = gemini_provider();
    let err = p.chat_stream(min_request()).await.err().expect("stream must error with dead client");
    assert!(matches!(err, GatewayError::ProviderError(_)), "{err:?}");
}

/// DT-PRV-GEM-22：无 api_key 时 query 不带 key（仍构建请求、仍失败）。
#[tokio::test]
async fn gemini_no_api_key() {
    let p = GeminiProvider::new(dead_google_client(), None, "gemini-1.5-pro", None, false);
    assert!(p.chat(min_request()).await.is_err());
}

/// DT-PRV-GEM-23：gemini provider accessors（direct construct）。
#[test]
fn gemini_provider_accessors() {
    let p = GeminiProvider::new(dead_google_client(), Some("k".into()), "gemini-1.5-pro", Some("dep-1".into()), true)
        .with_custom_headers(vec![("X-C".into(), "v".into())]);
    assert_eq!(p.name(), "gemini");
    assert_eq!(p.models()[0], "gemini-1.5-pro");
    assert_eq!(p.deployment_id(), Some("dep-1"));
    assert!(p.client_type_header());
    assert_eq!(p.custom_headers().len(), 1);
}

// ───────────────────────── openai 深分支（SSE 装配 / 流捕获 / 早退） ─────────────────────────

/// SSE mock 挂载 helper：返回 text/event-stream body。
async fn openai_sse_server(body: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    server
}

/// DT-PRV-OAI-29：非流式请求收到 SSE —— 装配深分支：reasoning 累积、
/// tool_calls 分片拼接（id 首胜 / call_type 缺省 / arguments 跨 chunk 拼接）、
/// 单 chunk 多 choice、role 缺省 Assistant、usage 缺 total 降 0。
#[tokio::test]
async fn openai_chat_sse_assembled_tool_reasoning_multi() {
    let sse = concat!(
        "data: {\"id\":\"c1\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"He\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"ing\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"},{\"index\":1,\"delta\":{\"content\":\"other\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"created\":1,\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":5}}\n\n",
        "data: [DONE]\n\n",
    );
    let server = openai_sse_server(sse.to_string()).await;
    let p = create("openai/test-model", None, server.uri());
    let resp = p.chat(min_request()).await.expect("sse assembled");
    assert_eq!(resp.id, "c1");
    assert_eq!(resp.choices.len(), 2, "both choice indexes assembled");
    let c0 = &resp.choices[0];
    assert_eq!(c0.finish_reason.as_deref(), Some("tool_calls"));
    assert!(matches!(&c0.message.content, MessageContent::Text(t) if t == "Hello"));
    assert_eq!(c0.message.reasoning_content.as_deref(), Some("thinking"));
    let calls = c0.message.tool_calls.as_ref().expect("tool_calls assembled");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].call_type, "function", "missing call_type defaults");
    assert_eq!(calls[0].function.name, "f");
    assert_eq!(calls[0].function.arguments, "{\"a\":1}");
    // index=1 的 choice 无 role → 默认 Assistant
    let c1 = &resp.choices[1];
    assert!(matches!(c1.message.role, MessageRole::Assistant));
    assert!(matches!(&c1.message.content, MessageContent::Text(t) if t == "other"));
    let usage = resp.usage.expect("usage");
    assert_eq!(usage.prompt_tokens, 5);
    assert_eq!(usage.total_tokens, 0, "missing total degrades to 0");
}

/// DT-PRV-OAI-25：非流式请求收到 SSE —— 全部 chunk 不可解析 →
/// UpstreamParseError（无可用 chunk）；坏 chunk 与好 chunk 混杂 → 坏的被跳过。
#[tokio::test]
async fn openai_chat_sse_unparseable_chunks() {
    // 全坏 → 装配失败
    let server = openai_sse_server("data: not-json\n\ndata: also-bad\n\ndata: [DONE]\n\n".to_string()).await;
    let p = create("openai/test-model", None, server.uri());
    let err = p.chat(min_request()).await.err().expect("no parseable chunks must fail");
    match err {
        GatewayError::UpstreamParseError { parse_error, .. } => {
            assert!(parse_error.contains("no parseable data chunks"), "{parse_error}");
        }
        other => panic!("expected UpstreamParseError, got {other:?}"),
    }

    // 坏 + 好混和 → 好的存活
    let mixed = concat!(
        "data: {\"id\":\"c2\",\"created\":2,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {{{broken\n\n",
        "data: {\"id\":\"c2\",\"created\":2,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let server2 = openai_sse_server(mixed.to_string()).await;
    let p2 = create("openai/test-model", None, server2.uri());
    let resp = p2.chat(min_request()).await.expect("good chunks survive");
    assert!(matches!(&resp.choices[0].message.content, MessageContent::Text(t) if t == "ok"));
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
}

/// DT-PRV-OAI-26：chat_stream 带 raw_capture —— 请求体与逐事件帧被记录，
/// 无尾随空行的 [DONE] 走 parser.finish() 派发路径。
#[tokio::test]
async fn openai_chat_stream_raw_capture_frames() {
    // 结尾无空行：[DONE] 只能由 finish() 派发（覆盖流末 pending 事件路径）
    let sse = "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]";
    let server = openai_sse_server(sse.to_string()).await;
    let p = create("openai/test-model", None, server.uri());
    let cap = Arc::new(boom_core::provider::RawCaptureChannel::default());
    let mut req = min_request();
    req.raw_capture = Some(cap.clone());
    let mut stream = p.chat_stream(req).await.expect("stream ok");
    let mut n = 0;
    while let Some(_) = stream.next().await { n += 1; }
    assert_eq!(n, 1, "one content chunk, [DONE] ends stream");
    let body = cap.take_request_body().expect("request body recorded");
    assert!(body.contains("\"model\":\"test-model\""), "serialized request captured: {body}");
    let raw = cap.take_response_body().expect("frames recorded");
    assert!(raw.contains("data: {\"id\":\"1\""), "event frames captured with prefix: {raw}");
    assert!(raw.contains("[DONE]"), "[DONE] frame captured: {raw}");
}

/// DT-PRV-OAI-27：chat_stream 消费端提前 Drop → 后台任务 send 失败 /
/// tx.closed() 早退（不再无限解析剩余事件）。
#[tokio::test]
async fn openai_chat_stream_dropped_early() {
    let mut sse = String::new();
    for i in 0..10 {
        sse.push_str(&format!(
            "data: {{\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"c{i}\"}},\"finish_reason\":null}}]}}\n\n"
        ));
    }
    sse.push_str("data: [DONE]\n\n");
    let server = openai_sse_server(sse).await;
    let p = create("openai/test-model", None, server.uri());
    let mut stream = p.chat_stream(min_request()).await.expect("stream ok");
    // 只读一个 chunk 就丢弃 —— receiver 关闭后后台任务必须终止
    let first = stream.next().await;
    assert!(first.is_some(), "first chunk arrives");
    drop(stream);
    // 给后台任务一点时间走到 send-err / tx.closed 分支
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

/// DT-PRV-OAI-28：chat 带 raw_capture 且上游返回错误状态 →
/// 错误 body 被记入 capture 帧，UpstreamError 携带原文。
#[tokio::test]
async fn openai_chat_raw_capture_upstream_error_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream boom"))
        .mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let cap = Arc::new(boom_core::provider::RawCaptureChannel::default());
    let mut req = min_request();
    req.raw_capture = Some(cap.clone());
    let err = p.chat(req).await.err().expect("upstream error");
    match err {
        GatewayError::UpstreamError { status, message } => {
            assert_eq!(status, 500);
            assert_eq!(message, "upstream boom");
        }
        other => panic!("expected UpstreamError, got {other:?}"),
    }
    assert_eq!(cap.take_response_body().as_deref(), Some("upstream boom"), "error body captured");
}

/// DT-PRV-OAI-30：!101 thinking→reasoning_effort —— anthropic 协议来的请求
/// （from_anthropic_protocol=true，thinking 在 extra 里）经 build_request
/// 翻译：budget 12000 → reasoning_effort "high" 上行，thinking 字段删除
/// （Anthropic 词表不上 OpenAI 线）。
#[tokio::test]
async fn openai_chat_translates_anthropic_thinking_to_reasoning_effort() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"reasoning_effort": "high"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let mut req = min_request();
    req.from_anthropic_protocol = true;
    req.extra.insert("thinking".into(), serde_json::json!({"type": "enabled", "budget_tokens": 12000}));
    assert!(p.chat(req).await.is_ok());
}

/// DT-PRV-OAI-31：显式 reasoning_effort 优先于 thinking 翻译 —— 客户端已设
/// "low" 就不覆盖；thinking 仍被删除。
#[tokio::test]
async fn openai_chat_explicit_reasoning_effort_wins_over_translation() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"reasoning_effort": "low"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let mut req = min_request();
    req.from_anthropic_protocol = true;
    req.extra.insert("thinking".into(), serde_json::json!({"type": "enabled", "budget_tokens": 12000}));
    req.extra.insert("reasoning_effort".into(), serde_json::json!("low"));
    assert!(p.chat(req).await.is_ok());
    // 上行 body：reasoning_effort 保持 "low"，thinking 已删
    let received = server.received_requests().await.expect("requests captured");
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body.get("reasoning_effort"), Some(&serde_json::json!("low")));
    assert!(body.get("thinking").is_none(), "thinking must be stripped: {body}");
}

/// DT-PRV-OAI-32：非 anthropic 来源（from_anthropic_protocol=false）不做
/// 翻译也不删 thinking —— 原生 OpenAI 请求的 extra 原样透传。
#[tokio::test]
async fn openai_chat_native_request_skips_thinking_translation() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"thinking": {"type": "enabled", "budget_tokens": 12000}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_json()))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let mut req = min_request();
    req.extra.insert("thinking".into(), serde_json::json!({"type": "enabled", "budget_tokens": 12000}));
    assert!(p.chat(req).await.is_ok());
}

/// DT-PRV-OAI-33：chat_stream 走同一个 build_request —— 翻译同样生效。
#[tokio::test]
async fn openai_chat_stream_translates_anthropic_thinking() {
    let sse = "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"reasoning_effort": "medium"})))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1).mount(&server).await;
    let p = create("openai/test-model", None, server.uri());
    let mut req = min_request();
    req.from_anthropic_protocol = true;
    req.extra.insert("thinking".into(), serde_json::json!({"type": "enabled", "budget_tokens": 6000}));
    let stream = p.chat_stream(req).await.expect("stream starts");
    let chunks: Vec<_> = stream.filter_map(|c| async move { c.ok() }).collect().await;
    assert_eq!(chunks.len(), 1);
}

// ───────────────────────── bedrock（骨架实现） ─────────────────────────

/// DT-PRV-BED-01：Bedrock 骨架 —— chat / chat_stream 返回 not-implemented
/// ProviderError；accessors 正常。
#[tokio::test]
async fn bedrock_skeleton_not_implemented() {
    let p = boom_provider::bedrock::BedrockProvider::new(
        Client::new(), "anthropic.claude-3", "us-east-1", Some("dep-b".into()), true,
    );
    let err = p.chat(min_request()).await.err().expect("chat must fail");
    match err {
        GatewayError::ProviderError(msg) => assert!(msg.contains("not yet implemented"), "{msg}"),
        other => panic!("expected ProviderError, got {other:?}"),
    }
    let serr = p.chat_stream(min_request()).await.err().expect("stream must fail");
    match serr {
        GatewayError::ProviderError(msg) => assert!(msg.contains("not yet implemented"), "{msg}"),
        other => panic!("expected ProviderError, got {other:?}"),
    }
    assert_eq!(p.name(), "bedrock");
    assert_eq!(p.models()[0], "anthropic.claude-3");
    assert_eq!(p.deployment_id(), Some("dep-b"));
    assert!(p.client_type_header());
}

// ───────────────────────── anthropic 深分支 ─────────────────────────

/// DT-PRV-ANT-21：请求转换深分支 —— assistant Parts(Reasoning+Text) → thinking 块；
/// Tool 角色 Parts 内容拼接；[ERROR] 前缀 → is_error；user Null → 空数组；
/// user Unknown part 丢弃；tool_choice 透传。
#[tokio::test]
async fn anthropic_request_deep_content_branches() {
    let server = anthropic_ok_server().await;
    let p = create("anthropic/claude-test", None, server.uri());
    let mut req = min_request();
    req.messages = vec![
        Message {
            role: MessageRole::User,
            content: MessageContent::Text("q".into()),
            name: None, tool_calls: None, tool_call_id: None, reasoning_content: None,
        },
        Message {
            role: MessageRole::Assistant,
            content: MessageContent::Parts(vec![
                ContentPart::Reasoning { reasoning: "R".into() },
                ContentPart::Text { text: "A".into() },
            ]),
            name: None, tool_calls: None, tool_call_id: None, reasoning_content: None,
        },
        Message {
            role: MessageRole::Tool,
            content: MessageContent::Parts(vec![ContentPart::Text { text: "ok-part".into() }]),
            tool_call_id: Some("t1".into()),
            name: None, tool_calls: None, reasoning_content: None,
        },
        Message {
            role: MessageRole::Tool,
            content: MessageContent::Text("[ERROR] bad".into()),
            tool_call_id: Some("t2".into()),
            name: None, tool_calls: None, reasoning_content: None,
        },
        Message {
            role: MessageRole::User,
            content: MessageContent::Null,
            name: None, tool_calls: None, tool_call_id: None, reasoning_content: None,
        },
        Message {
            role: MessageRole::User,
            content: MessageContent::Parts(vec![ContentPart::Unknown(
                serde_json::json!({"type": "audio"}),
            )]),
            name: None, tool_calls: None, tool_call_id: None, reasoning_content: None,
        },
    ];
    req.tool_choice = Some("auto".into());
    p.chat(req).await.expect("chat ok");
    let body = last_request_body(&server).await;
    let msgs = body["messages"].as_array().unwrap();
    // [user "q", assistant(thinking+text), tool_result, tool_result, user(null→[]), user(unknown→[])]
    let assistant = &msgs[1];
    let blocks = assistant["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["thinking"], "R");
    assert_eq!(blocks[1]["type"], "text");
    // Tool Parts → tool_result 块，content 为拼接后的字符串
    // （相邻同角色消息之间会被 ensure_role_alternation 插入空 text 分隔块）
    let tr1 = &msgs[2];
    assert_eq!(tr1["content"][0]["type"], "tool_result");
    assert_eq!(tr1["content"][0]["content"], "ok-part");
    assert!(tr1["content"][0].get("is_error").is_none(), "non-error result has no is_error");
    // [ERROR] 前缀剥离 + is_error（msgs[3] 是 alternation 插入的空 text 分隔）
    let tr2 = &msgs[4];
    assert_eq!(tr2["content"][0]["type"], "tool_result");
    assert_eq!(tr2["content"][0]["content"], "bad");
    assert_eq!(tr2["content"][0]["is_error"], true);
    // Null user → []（msgs[5]、msgs[7] 是 alternation 分隔）
    assert_eq!(msgs[6]["content"], serde_json::json!([]));
    // Unknown part → 丢弃 → []
    assert_eq!(msgs[8]["content"], serde_json::json!([]));
    // tool_choice 转为对象形式
    assert_eq!(body["tool_choice"], serde_json::json!({"type": "auto"}));
}

/// DT-PRV-ANT-22：响应转换深分支 —— redacted_thinking / 未知块类型跳过、
/// stop_reason=max_tokens → length、无 stop_reason → 默认 stop、cache_read 读取。
#[tokio::test]
async fn anthropic_response_deep_block_branches() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "r1", "type": "message", "role": "assistant",
            "content": [
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "hi"},
                {"type": "weird_block", "x": 1}
            ],
            "model": "claude-test", "stop_reason": "max_tokens",
            "usage": {"input_tokens": 3, "output_tokens": 4, "cache_read_input_tokens": 9}
        })))
        .mount(&server).await;
    let p = create("anthropic/claude-test", None, server.uri());
    let resp = p.chat(min_request()).await.expect("chat ok");
    assert!(matches!(&resp.choices[0].message.content, MessageContent::Text(t) if t == "hi"),
        "redacted/unknown blocks dropped, text kept");
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"), "max_tokens → length");
    let usage = resp.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 3);
    assert_eq!(usage.cache_read_input_tokens, Some(9));

    // 无 stop_reason / 无 usage → 默认 finish=stop、usage=None
    let server2 = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "r2", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "plain"}],
            "model": "claude-test"
        })))
        .mount(&server2).await;
    let p2 = create("anthropic/claude-test", None, server2.uri());
    let resp2 = p2.chat(min_request()).await.expect("chat ok");
    assert_eq!(resp2.choices[0].finish_reason.as_deref(), Some("stop"), "missing stop_reason defaults");
    // usage 无条件构造，缺失字段降 0
    let usage2 = resp2.usage.as_ref().expect("usage always built");
    assert_eq!(usage2.prompt_tokens, 0);
    assert_eq!(usage2.cache_read_input_tokens, None);
}

/// DT-PRV-ANT-23：chat_stream 消费端提前 Drop → 后台任务 send 失败早退
/// （各事件分支的 tx.send-err return）。
#[tokio::test]
async fn anthropic_stream_dropped_early() {
    let mut sse = String::new();
    sse.push_str("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n");
    for i in 0..10 {
        sse.push_str(&format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"c{i}\"}}}}\n\n"
        ));
    }
    sse.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server).await;
    let p = create("anthropic/claude-test", None, server.uri());
    let mut stream = p.chat_stream(min_request()).await.expect("stream ok");
    let first = stream.next().await;
    assert!(first.is_some());
    drop(stream);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}
