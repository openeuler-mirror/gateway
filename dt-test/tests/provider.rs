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
