//! DT 用例 — boom-core：网关核心类型与纯逻辑。
//!
//! 覆盖范围（全部纯内存，无网络无 DB）：
//! - error.rs：GatewayError 18 变体的 status_code / should_log_to_db /
//!   should_dedup_log / is_deployment_failure / error_type / raw_upstream_body
//! - normalize.rs：ensure_role_alternation / convert_tool_choice_for_anthropic /
//!   convert_image_source / host_of_api_base 全分支
//! - debug_store.rs：DebugErrorStore 启停 / record / FIFO 驱逐 / 查询
//! - key_format.rs：is_valid_prefix 边界
//! - provider.rs：ProviderCost / ProviderBilling（含 nested prompt_tokens_details）/
//!   RawCaptureChannel（first-writer-wins / 帧 join）
//! - types.rs：is_hard_blocked_header / AuthIdentity 谓词 /
//!   Message::normalize_reasoning_for_openai / ContentPart 手写 serde /
//!   WindowLimit::is_empty / deserialize_window_limit_vec（Array vs Object）/
//!   CompletionRequest::into_chat_request / lenient 反序列化
//! - kv_event.rs：StorageTier::priority_score + serde lowercase
//! - otlp_config.rs：OtlpConfig::default 字段默认值

use boom_core::normalize::{
    convert_image_source, convert_tool_choice_for_anthropic, ensure_role_alternation,
    host_of_api_base,
};
use boom_core::provider::{ProviderBilling, ProviderCost, RawCaptureChannel};
use boom_core::types::{
    deserialize_window_limit_vec, is_hard_blocked_header, AuthIdentity, CompletionPrompt,
    CompletionRequest, ContentPart, ImageUrl, Message, MessageContent, MessageRole, Usage,
    WindowLimit,
};
use boom_core::kv_event::StorageTier;
use boom_core::{DebugErrorEntry, DebugErrorStore, GatewayError, OtlpConfig, is_valid_prefix};

use rust_decimal::Decimal;

// ───────────────────────── helpers ─────────────────────────

/// 构造一个最小合法的 AuthIdentity，按需覆盖字段。
fn auth_identity() -> AuthIdentity {
    AuthIdentity {
        key_hash: "h".to_string(),
        key_name: None,
        key_alias: None,
        user_id: None,
        team_id: None,
        team_alias: None,
        models: vec![],
        team_models: vec![],
        rpm_limit: None,
        tpm_limit: None,
        max_budget: None,
        spend: 0.0,
        blocked: false,
        expires_at: None,
        metadata: serde_json::Value::Null,
    }
}

fn rate_limit_exceeded() -> GatewayError {
    GatewayError::RateLimitExceeded {
        retry_after_secs: None,
        message: "rpm".into(),
        limit_type: "rpm_limit",
        scope: Some("key"),
        scope_id: None,
        plan_name: None,
    }
}

fn debug_entry(request_id: &str, key_hash: &str) -> DebugErrorEntry {
    DebugErrorEntry {
        request_id: request_id.to_string(),
        key_hash: key_hash.to_string(),
        key_alias: None,
        model: "gpt-4".to_string(),
        api_path: "/v1/chat/completions".to_string(),
        is_stream: false,
        created_at: "2026-09-28T00:00:00Z".to_string(),
        status_code: 502,
        error_type: "upstream_error".to_string(),
        error_message: "boom".to_string(),
        upstream_status: Some(500),
        upstream_body: Some("oops".to_string()),
        request_body: Some("{}".to_string()),
    }
}

// ═════════════════════════════════════════════════════════════
// error.rs — status_code（18 变体全覆盖）
// ═════════════════════════════════════════════════════════════

/// DT-CORE-01：status_code 每个变体映射到正确的 HTTP 状态码。
#[test]
fn error_status_code_all_variants() {
    assert_eq!(GatewayError::AuthError("x".into()).status_code(), 401);
    assert_eq!(rate_limit_exceeded().status_code(), 429);
    assert_eq!(
        GatewayError::ConcurrencyExceeded { limit: 1, message: "x".into() }.status_code(),
        429
    );
    assert_eq!(GatewayError::ModelNotFound("m".into()).status_code(), 404);
    assert_eq!(GatewayError::ProviderError("p".into()).status_code(), 502);
    assert_eq!(
        GatewayError::UpstreamParseError { parse_error: "e".into(), raw_body: "b".into() }
            .status_code(),
        502
    );
    assert_eq!(GatewayError::BudgetExceeded.status_code(), 402);
    assert_eq!(GatewayError::ConfigError("c".into()).status_code(), 500);
    assert_eq!(GatewayError::KeyExpired.status_code(), 401);
    assert_eq!(GatewayError::KeyBlocked.status_code(), 403);
    assert_eq!(GatewayError::ModelNotAllowed("m".into()).status_code(), 403);
    assert_eq!(GatewayError::UpstreamTimeout.status_code(), 504);
    assert_eq!(
        GatewayError::UpstreamError { status: 500, message: "x".into() }.status_code(),
        502
    );
    assert_eq!(GatewayError::NotSupported("e".into()).status_code(), 404);
    assert_eq!(GatewayError::UnsupportedMode("m".into()).status_code(), 400);
    assert_eq!(
        GatewayError::FlowControlQueueTimeout {
            deployment_id: "d".into(),
            waiters: 0,
            message: "x".into()
        }
        .status_code(),
        503
    );
    assert_eq!(GatewayError::InternalError("i".into()).status_code(), 500);
}

/// DT-CORE-02：UpstreamError 的 status 字段不影响 status_code（恒 502）。
#[test]
fn error_upstream_status_code_constant_regardless_of_status() {
    for s in [400u16, 401, 403, 429, 500, 502, 503] {
        assert_eq!(
            GatewayError::UpstreamError { status: s, message: "x".into() }.status_code(),
            502
        );
    }
}

// ── should_log_to_db ──

/// DT-CORE-03：should_log_to_db 对预期拒绝类返回 false，其余返回 true。
#[test]
fn error_should_log_to_db_excludes_expected_rejections() {
    // false — 太频繁不单独审计
    assert!(!rate_limit_exceeded().should_log_to_db());
    assert!(!GatewayError::ConcurrencyExceeded { limit: 1, message: "x".into() }.should_log_to_db());
    assert!(!GatewayError::BudgetExceeded.should_log_to_db());
    assert!(!GatewayError::FlowControlQueueTimeout {
        deployment_id: "d".into(),
        waiters: 0,
        message: "x".into()
    }
    .should_log_to_db());

    // true — 逐请求审计
    assert!(GatewayError::AuthError("x".into()).should_log_to_db());
    assert!(GatewayError::ModelNotFound("m".into()).should_log_to_db());
    assert!(GatewayError::ProviderError("p".into()).should_log_to_db());
    assert!(GatewayError::UpstreamParseError { parse_error: "e".into(), raw_body: "b".into() }
        .should_log_to_db());
    assert!(GatewayError::ConfigError("c".into()).should_log_to_db());
    assert!(GatewayError::KeyExpired.should_log_to_db());
    assert!(GatewayError::KeyBlocked.should_log_to_db());
    assert!(GatewayError::ModelNotAllowed("m".into()).should_log_to_db());
    assert!(GatewayError::UpstreamTimeout.should_log_to_db());
    assert!(GatewayError::UpstreamError { status: 500, message: "x".into() }.should_log_to_db());
    assert!(GatewayError::NotSupported("e".into()).should_log_to_db());
    assert!(GatewayError::UnsupportedMode("m".into()).should_log_to_db());
    assert!(GatewayError::InternalError("i".into()).should_log_to_db());
}

// ── should_dedup_log ──

/// DT-CORE-04：should_dedup_log 8 变体白名单返回 true，其余 false。
#[test]
fn error_should_dedup_log_whitelist() {
    // 白名单成员
    assert!(rate_limit_exceeded().should_dedup_log());
    assert!(GatewayError::ConcurrencyExceeded { limit: 1, message: "x".into() }.should_dedup_log());
    assert!(GatewayError::BudgetExceeded.should_dedup_log());
    assert!(GatewayError::FlowControlQueueTimeout {
        deployment_id: "d".into(),
        waiters: 0,
        message: "x".into()
    }
    .should_dedup_log());
    assert!(GatewayError::ModelNotFound("gpt-x".into()).should_dedup_log());
    assert!(GatewayError::ModelNotAllowed("gpt-x".into()).should_dedup_log());
    assert!(GatewayError::KeyExpired.should_dedup_log());
    assert!(GatewayError::KeyBlocked.should_dedup_log());

    // 非白名单 — 保留全量日志
    assert!(!GatewayError::AuthError("bad key".into()).should_dedup_log());
    assert!(!GatewayError::ProviderError("upstream".into()).should_dedup_log());
    assert!(!GatewayError::UpstreamParseError { parse_error: "e".into(), raw_body: "b".into() }
        .should_dedup_log());
    assert!(!GatewayError::UpstreamTimeout.should_dedup_log());
    assert!(!GatewayError::UpstreamError { status: 500, message: "x".into() }.should_dedup_log());
    assert!(!GatewayError::ConfigError("cfg".into()).should_dedup_log());
    assert!(!GatewayError::InternalError("boom".into()).should_dedup_log());
    assert!(!GatewayError::NotSupported("embeddings".into()).should_dedup_log());
    assert!(!GatewayError::UnsupportedMode("x".into()).should_dedup_log());
}

/// DT-CORE-05：should_dedup_log 是 !should_log_to_db 的超集——
/// 不进 DB 的 4 类必然 dedup，额外 4 类（NotFound/NotAllowed/Expired/Blocked）
/// dedup 但仍进 DB（窗口内首条写库）。
#[test]
fn error_dedup_superset_of_non_db() {
    let non_db = [
        rate_limit_exceeded(),
        GatewayError::ConcurrencyExceeded { limit: 1, message: "x".into() },
        GatewayError::BudgetExceeded,
        GatewayError::FlowControlQueueTimeout {
            deployment_id: "d".into(),
            waiters: 0,
            message: "x".into(),
        },
    ];
    for e in &non_db {
        assert!(e.should_dedup_log());
        assert!(!e.should_log_to_db());
    }
    let dedup_but_db = [
        GatewayError::ModelNotFound("gpt-x".into()),
        GatewayError::ModelNotAllowed("gpt-x".into()),
        GatewayError::KeyExpired,
        GatewayError::KeyBlocked,
    ];
    for e in &dedup_but_db {
        assert!(e.should_dedup_log());
        assert!(e.should_log_to_db());
    }
}

// ── is_deployment_failure ──

/// DT-CORE-06：is_deployment_failure — ProviderError/UpstreamParseError 恒 true；
/// UpstreamError 仅 401/403 为 true；其余 false。
#[test]
fn error_is_deployment_failure_classification() {
    // 确定性部署失败
    assert!(GatewayError::ProviderError("up".into()).is_deployment_failure());
    assert!(GatewayError::UpstreamParseError { parse_error: "e".into(), raw_body: "b".into() }
        .is_deployment_failure());
    assert!(GatewayError::UpstreamError { status: 401, message: "x".into() }.is_deployment_failure());
    assert!(GatewayError::UpstreamError { status: 403, message: "x".into() }.is_deployment_failure());

    // UpstreamError 非 401/403 → false（可自愈）
    assert!(!GatewayError::UpstreamError { status: 500, message: "x".into() }.is_deployment_failure());
    assert!(!GatewayError::UpstreamError { status: 429, message: "x".into() }.is_deployment_failure());
    assert!(!GatewayError::UpstreamError { status: 502, message: "x".into() }.is_deployment_failure());

    // 其余全部 false
    assert!(!GatewayError::AuthError("x".into()).is_deployment_failure());
    assert!(!rate_limit_exceeded().is_deployment_failure());
    assert!(!GatewayError::ModelNotFound("m".into()).is_deployment_failure());
    assert!(!GatewayError::BudgetExceeded.is_deployment_failure());
    assert!(!GatewayError::UpstreamTimeout.is_deployment_failure());
    assert!(!GatewayError::ConfigError("c".into()).is_deployment_failure());
    assert!(!GatewayError::InternalError("i".into()).is_deployment_failure());
}

// ── error_type ──

/// DT-CORE-07：error_type 每变体的 OpenAI 风格类型串。
#[test]
fn error_type_all_variants() {
    assert_eq!(GatewayError::AuthError("x".into()).error_type(), "authentication_error");
    assert_eq!(rate_limit_exceeded().error_type(), "rpm_limit");
    assert_eq!(
        GatewayError::ConcurrencyExceeded { limit: 1, message: "x".into() }.error_type(),
        "concurrency_exceeded"
    );
    assert_eq!(GatewayError::ModelNotFound("m".into()).error_type(), "model_not_found");
    assert_eq!(GatewayError::ProviderError("p".into()).error_type(), "provider_error");
    assert_eq!(
        GatewayError::UpstreamParseError { parse_error: "e".into(), raw_body: "b".into() }
            .error_type(),
        "upstream_parse_error"
    );
    assert_eq!(GatewayError::BudgetExceeded.error_type(), "budget_exceeded");
    assert_eq!(GatewayError::ConfigError("c".into()).error_type(), "internal_error");
    assert_eq!(GatewayError::KeyExpired.error_type(), "key_expired");
    assert_eq!(GatewayError::KeyBlocked.error_type(), "key_blocked");
    assert_eq!(GatewayError::ModelNotAllowed("m".into()).error_type(), "model_not_allowed");
    assert_eq!(GatewayError::UpstreamTimeout.error_type(), "timeout");
    assert_eq!(
        GatewayError::UpstreamError { status: 500, message: "x".into() }.error_type(),
        "upstream_error"
    );
    assert_eq!(GatewayError::NotSupported("e".into()).error_type(), "not_supported");
    assert_eq!(GatewayError::UnsupportedMode("m".into()).error_type(), "unsupported_mode_error");
    assert_eq!(
        GatewayError::FlowControlQueueTimeout {
            deployment_id: "d".into(),
            waiters: 0,
            message: "x".into()
        }
        .error_type(),
        "flow_control_timeout"
    );
    assert_eq!(GatewayError::InternalError("i".into()).error_type(), "internal_error");
}

/// DT-CORE-08：RateLimitExceeded.error_type 透传 limit_type 字段（非恒定串）。
#[test]
fn error_type_rate_limit_passes_through_limit_type() {
    let e = GatewayError::RateLimitExceeded {
        retry_after_secs: None,
        message: "tpm".into(),
        limit_type: "team_tpm_limit",
        scope: Some("team"),
        scope_id: None,
        plan_name: None,
    };
    assert_eq!(e.error_type(), "team_tpm_limit");
}

// ── raw_upstream_body ──

/// DT-CORE-09：raw_upstream_body 仅 UpstreamParseError 返回 Some，其余 None。
#[test]
fn error_raw_upstream_body_only_for_parse_error() {
    assert_eq!(
        GatewayError::UpstreamParseError { parse_error: "e".into(), raw_body: "BODY".into() }
            .raw_upstream_body(),
        Some("BODY")
    );
    assert_eq!(GatewayError::ProviderError("p".into()).raw_upstream_body(), None);
    assert_eq!(
        GatewayError::UpstreamError { status: 500, message: "x".into() }.raw_upstream_body(),
        None
    );
    assert_eq!(GatewayError::InternalError("i".into()).raw_upstream_body(), None);
}

// ═════════════════════════════════════════════════════════════
// normalize.rs — ensure_role_alternation
// ═════════════════════════════════════════════════════════════

fn msg(role: MessageRole, text: &str) -> Message {
    Message {
        role,
        content: MessageContent::Text(text.to_string()),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    }
}

/// DT-CORE-10：连续同角色（user/user、assistant/assistant）插入空 user 分隔符。
#[test]
fn normalize_alternation_consecutive_same_role() {
    let mut m = vec![
        msg(MessageRole::User, "a"),
        msg(MessageRole::User, "b"),
        msg(MessageRole::Assistant, "c"),
        msg(MessageRole::Assistant, "d"),
    ];
    ensure_role_alternation(&mut m);
    assert_eq!(m.len(), 6);
    // [U, <sep U>, U, A, <sep U>, A]
    assert!(matches!(m[0].role, MessageRole::User));
    assert!(matches!(m[1].role, MessageRole::User)); // 插入的空分隔
    assert!(matches!(m[1].content, MessageContent::Text(ref t) if t.is_empty()));
    assert!(matches!(m[2].role, MessageRole::User));
    assert!(matches!(m[3].role, MessageRole::Assistant));
    assert!(matches!(m[4].role, MessageRole::User)); // 插入的空分隔
    assert!(matches!(m[5].role, MessageRole::Assistant));
}

/// DT-CORE-11：无触发对的序列不被改动。注意 `(Assistant, User)` 是触发对
/// （Anthropic 严格要求 user 起头），故真正"已交替"的序列用 `(User, Assistant)`
/// 与 System 搭配——这些相邻对均不在触发列表中。
#[test]
fn normalize_alternation_already_alternating_unchanged() {
    let mut m = vec![
        msg(MessageRole::System, "s"),
        msg(MessageRole::User, "u"),
        msg(MessageRole::Assistant, "a"),
    ];
    ensure_role_alternation(&mut m);
    assert_eq!(m.len(), 3);
}

/// DT-CORE-12：user/tool 混合对的 4 种触发组合都插分隔符。
#[test]
fn normalize_alternation_tool_user_pairs_trigger_separator() {
    // (User, Tool), (Assistant, User), (Tool, Tool), (Tool, User)
    let cases: Vec<(MessageRole, MessageRole)> = vec![
        (MessageRole::User, MessageRole::Tool),
        (MessageRole::Assistant, MessageRole::User),
        (MessageRole::Tool, MessageRole::Tool),
        (MessageRole::Tool, MessageRole::User),
    ];
    for (a, b) in cases {
        let pair_desc = format!("{a:?},{b:?}");
        let mut m = vec![msg(a, "1"), msg(b, "2")];
        ensure_role_alternation(&mut m);
        assert_eq!(m.len(), 3, "pair ({pair_desc}) should insert separator");
        assert!(matches!(m[1].role, MessageRole::User), "separator must be user");
    }
}

/// DT-CORE-13：System 角色不触发分隔（与 System/System/System 搭配均不插）。
#[test]
fn normalize_alternation_system_does_not_trigger() {
    let mut m = vec![
        msg(MessageRole::System, "s"),
        msg(MessageRole::System, "s2"),
        msg(MessageRole::User, "u"),
    ];
    ensure_role_alternation(&mut m);
    assert_eq!(m.len(), 3, "System pairs never trigger separators");
}

/// DT-CORE-14：空向量与单元素向量安全不变。
#[test]
fn normalize_alternation_empty_and_single_unchanged() {
    let mut empty: Vec<Message> = vec![];
    ensure_role_alternation(&mut empty);
    assert!(empty.is_empty());

    let mut single = vec![msg(MessageRole::User, "only")];
    ensure_role_alternation(&mut single);
    assert_eq!(single.len(), 1);
}

// ═════════════════════════════════════════════════════════════
// normalize.rs — convert_tool_choice_for_anthropic
// ═════════════════════════════════════════════════════════════

/// DT-CORE-15：tool_choice=None + parallel_tool_calls=None → (None, false)。
#[test]
fn convert_tool_choice_none_input() {
    let (tc, strip) = convert_tool_choice_for_anthropic(&None, None);
    assert!(tc.is_none());
    assert!(!strip);
}

/// DT-CORE-16：tool_choice=None + parallel=false → auto + disable_parallel，不 strip。
#[test]
fn convert_tool_choice_none_input_parallel_false() {
    let (tc, strip) = convert_tool_choice_for_anthropic(&None, Some(false));
    let v = tc.expect("some");
    assert_eq!(v["type"], "auto");
    assert_eq!(v["disable_parallel_tool_use"], true);
    assert!(!strip);
}

/// DT-CORE-17：type=none → (None, strip=true)。
#[test]
fn convert_tool_choice_none_type_strips_tools() {
    let (tc, strip) =
        convert_tool_choice_for_anthropic(&Some(serde_json::json!({"type": "none"})), None);
    assert!(tc.is_none());
    assert!(strip);
}

/// DT-CORE-18：type=auto → auto；parallel=false 加 disable_parallel。
#[test]
fn convert_tool_choice_auto() {
    let (tc, strip) =
        convert_tool_choice_for_anthropic(&Some(serde_json::json!({"type": "auto"})), None);
    assert_eq!(tc.unwrap()["type"], "auto");
    assert!(!strip);

    let (tc2, _) =
        convert_tool_choice_for_anthropic(&Some(serde_json::json!({"type": "auto"})), Some(false));
    let v = tc2.unwrap();
    assert_eq!(v["disable_parallel_tool_use"], true);
}

/// DT-CORE-19：type=required → 映射为 Anthropic "any"。
#[test]
fn convert_tool_choice_required_maps_to_any() {
    let (tc, strip) =
        convert_tool_choice_for_anthropic(&Some(serde_json::json!({"type": "required"})), None);
    let v = tc.unwrap();
    assert_eq!(v["type"], "any");
    assert!(!strip);

    // parallel=false 同样加 disable_parallel
    let (tc2, _) =
        convert_tool_choice_for_anthropic(&Some(serde_json::json!({"type": "required"})), Some(false));
    assert_eq!(tc2.unwrap()["disable_parallel_tool_use"], true);
}

/// DT-CORE-20：type=function → tool + name；parallel=false 加 disable_parallel。
#[test]
fn convert_tool_choice_function_maps_to_tool() {
    let input = serde_json::json!({"type": "function", "function": {"name": "get_weather"}});
    let (tc, strip) = convert_tool_choice_for_anthropic(&Some(input), None);
    let v = tc.unwrap();
    assert_eq!(v["type"], "tool");
    assert_eq!(v["name"], "get_weather");
    assert!(!strip);

    let input2 = serde_json::json!({"type": "function", "function": {"name": "f"}});
    let (tc2, _) = convert_tool_choice_for_anthropic(&Some(input2), Some(false));
    assert_eq!(tc2.unwrap()["disable_parallel_tool_use"], true);
}

/// DT-CORE-21：function 无 function.name → 空字符串 name。
#[test]
fn convert_tool_choice_function_missing_name_defaults_empty() {
    let input = serde_json::json!({"type": "function"});
    let (tc, _) = convert_tool_choice_for_anthropic(&Some(input), None);
    assert_eq!(tc.unwrap()["name"], "");
}

/// DT-CORE-22：未知 type → 原样透传。
#[test]
fn convert_tool_choice_unknown_type_passthrough() {
    let input = serde_json::json!({"type": "weird", "extra": 1});
    let (tc, strip) = convert_tool_choice_for_anthropic(&Some(input.clone()), None);
    assert_eq!(tc.unwrap(), input);
    assert!(!strip);
}

/// DT-CORE-23：type 缺失默认 "auto"。
#[test]
fn convert_tool_choice_missing_type_defaults_auto() {
    let input = serde_json::json!({}); // 无 type
    let (tc, strip) = convert_tool_choice_for_anthropic(&Some(input), None);
    assert_eq!(tc.unwrap()["type"], "auto");
    assert!(!strip);
}

// ═════════════════════════════════════════════════════════════
// normalize.rs — convert_image_source
// ═════════════════════════════════════════════════════════════

/// DT-CORE-24：url 类型直接返回 url 字段。
#[test]
fn convert_image_source_url() {
    let s = serde_json::json!({"type": "url", "url": "https://example.com/i.png"});
    assert_eq!(convert_image_source(&s), "https://example.com/i.png");
}

/// DT-CORE-25：base64 类型构造 data URI，默认/指定 media_type。
#[test]
fn convert_image_source_base64() {
    let s = serde_json::json!({"type": "base64", "media_type": "image/jpeg", "data": "SGVsbG8="});
    let r = convert_image_source(&s);
    assert_eq!(r, "data:image/jpeg;base64,SGVsbG8=");

    // 缺 media_type → 默认 image/png；缺 data → 空
    let s2 = serde_json::json!({"type": "base64"});
    assert_eq!(convert_image_source(&s2), "data:image/png;base64,");
}

/// DT-CORE-26：未知 type / 缺 type → 原始 JSON 串。
#[test]
fn convert_image_source_unknown_type() {
    let s = serde_json::json!({"type": "weird", "x": 1});
    assert_eq!(convert_image_source(&s), r#"{"type":"weird","x":1}"#);

    let s2 = serde_json::json!({"no_type": true});
    assert_eq!(convert_image_source(&s2), r#"{"no_type":true}"#);
}

// ═════════════════════════════════════════════════════════════
// normalize.rs — host_of_api_base
// ═════════════════════════════════════════════════════════════

/// DT-CORE-27：IPv4 + 端口 + 路径 / 主机名 / 裸 host。
#[test]
fn host_of_api_base_ipv4_and_hostname() {
    assert_eq!(host_of_api_base("http://7.150.7.202:8000/v1"), Some("7.150.7.202".into()));
    assert_eq!(host_of_api_base("http://worker-0/v1"), Some("worker-0".into()));
    assert_eq!(host_of_api_base("http://host:8000"), Some("host".into()));
    assert_eq!(host_of_api_base("7.150.7.202"), Some("7.150.7.202".into()));
    assert_eq!(host_of_api_base("https://api.example.com:443/v1/chat"), Some("api.example.com".into()));
}

/// DT-CORE-28：IPv6 字面量（带/不带端口）去括号返回裸 host。
#[test]
fn host_of_api_base_ipv6_literal() {
    assert_eq!(host_of_api_base("http://[2001:db8::1]:8000/v1"), Some("2001:db8::1".into()));
    assert_eq!(host_of_api_base("http://[2001:db8::1]/v1"), Some("2001:db8::1".into()));
    assert_eq!(host_of_api_base("http://[::1]:8080"), Some("::1".into()));
}

/// DT-CORE-29：空/纯空白输入返回 None。
#[test]
fn host_of_api_base_empty_returns_none() {
    assert_eq!(host_of_api_base(""), None);
    assert_eq!(host_of_api_base("   "), None);
}

/// DT-CORE-30：空 IPv6 括号 `[]` 返回 None（边界）。
#[test]
fn host_of_api_base_empty_ipv6_brackets_returns_none() {
    assert_eq!(host_of_api_base("http://[]:8000/v1"), None);
}

// ═════════════════════════════════════════════════════════════
// debug_store.rs — DebugErrorStore
// ═════════════════════════════════════════════════════════════

/// DT-CORE-31：new 默认禁用，record 无效；Default 等价 new。
#[test]
fn debug_store_new_disabled_ignores_records() {
    let s = DebugErrorStore::new();
    assert!(!s.is_enabled());
    s.record(debug_entry("r1", "k1"));
    assert_eq!(s.len(), 0);
    assert!(s.get("r1").is_none());
    assert!(s.list_for_key("k1").is_empty());

    // Default 等价
    let d = DebugErrorStore::default();
    assert!(!d.is_enabled());
}

/// DT-CORE-32：启用后 record 入库，get/list_for_key 可查。
#[test]
fn debug_store_enabled_records_and_lookup() {
    let s = DebugErrorStore::new();
    s.set_enabled(true);
    assert!(s.is_enabled());

    s.record(debug_entry("r1", "k1"));
    s.record(debug_entry("r2", "k1"));
    assert_eq!(s.len(), 2);
    assert!(s.get("r1").is_some());
    let list = s.list_for_key("k1");
    assert_eq!(list.len(), 2);
    // list_for_key 未知 key 返回空
    assert!(s.list_for_key("unknown").is_empty());
}

/// DT-CORE-33：单 key 超过 3 条触发 FIFO 驱逐最旧。
#[test]
fn debug_store_fifo_eviction_per_key() {
    let s = DebugErrorStore::new();
    s.set_enabled(true);
    for i in 0..5 {
        s.record(debug_entry(&format!("r{i}"), "k1"));
    }
    // 超过 MAX_ENTRIES_PER_KEY(3)，最旧的 r0/r1 被驱逐，保留 r2/r3/r4
    assert_eq!(s.len(), 3);
    assert!(s.get("r0").is_none());
    assert!(s.get("r1").is_none());
    assert!(s.get("r2").is_some());
    assert!(s.get("r4").is_some());
    let list = s.list_for_key("k1");
    assert_eq!(list.len(), 3);
}

/// DT-CORE-34：不同 key 各自独立计数 3 条上限。
#[test]
fn debug_store_per_key_limit_independent() {
    let s = DebugErrorStore::new();
    s.set_enabled(true);
    for i in 0..4 {
        s.record(debug_entry(&format!("a{i}"), "ka"));
    }
    for i in 0..4 {
        s.record(debug_entry(&format!("b{i}"), "kb"));
    }
    assert_eq!(s.len(), 6); // 3 + 3
}

/// DT-CORE-35：set_enabled(false) 清空全部条目，之后 record 无效。
#[test]
fn debug_store_disable_clears_and_blocks() {
    let s = DebugErrorStore::new();
    s.set_enabled(true);
    s.record(debug_entry("r1", "k1"));
    s.record(debug_entry("r2", "k2"));
    assert_eq!(s.len(), 2);

    s.set_enabled(false);
    assert!(!s.is_enabled());
    assert_eq!(s.len(), 0);
    // 禁用后 record 无效
    s.record(debug_entry("r3", "k1"));
    assert_eq!(s.len(), 0);
}

/// DT-CORE-36：clear() 清空但保持 enabled 状态。
#[test]
fn debug_store_clear_preserves_enabled() {
    let s = DebugErrorStore::new();
    s.set_enabled(true);
    s.record(debug_entry("r1", "k1"));
    s.clear();
    assert_eq!(s.len(), 0);
    assert!(s.is_enabled());
    // 仍可继续 record
    s.record(debug_entry("r2", "k1"));
    assert_eq!(s.len(), 1);
}

// ═════════════════════════════════════════════════════════════
// key_format.rs — is_valid_prefix
// ═════════════════════════════════════════════════════════════

/// DT-CORE-37：合法前缀（1–50 ASCII 字母数字，大小写均可）。
#[test]
fn key_format_valid_prefixes() {
    assert!(is_valid_prefix("a"));
    assert!(is_valid_prefix("abc123"));
    assert!(is_valid_prefix("TeamA"));
    assert!(is_valid_prefix(&"a".repeat(50)));
}

/// DT-CORE-38：非法前缀（空、超长、含下划线/连字符/点）。
#[test]
fn key_format_invalid_prefixes() {
    assert!(!is_valid_prefix(""));
    assert!(!is_valid_prefix(&"a".repeat(51)));
    assert!(!is_valid_prefix("team_a"));
    assert!(!is_valid_prefix("team-a"));
    assert!(!is_valid_prefix("team.a"));
    assert!(!is_valid_prefix("带中文"));
}

// ═════════════════════════════════════════════════════════════
// provider.rs — ProviderCost / ProviderBilling
// ═════════════════════════════════════════════════════════════

/// DT-CORE-39：ProviderCost::total 三项求和；add 累加。
#[test]
fn provider_cost_total_and_add() {
    let mut a = ProviderCost {
        regular_input: Decimal::from(10),
        cached_input: Decimal::from(2),
        output: Decimal::from(5),
    };
    assert_eq!(a.total(), Decimal::from(17));

    let b = ProviderCost {
        regular_input: Decimal::from(1),
        cached_input: Decimal::from(1),
        output: Decimal::from(1),
    };
    a.add(&b);
    assert_eq!(a.regular_input, Decimal::from(11));
    assert_eq!(a.cached_input, Decimal::from(3));
    assert_eq!(a.output, Decimal::from(6));
    assert_eq!(a.total(), Decimal::from(20));

    // default 全零
    assert_eq!(ProviderCost::default().total(), Decimal::from(0));
}

/// DT-CORE-40：ProviderBilling 累加实际成本，多次 add_actual_cost 叠加。
#[test]
fn provider_billing_accumulates_cost() {
    let b = ProviderBilling::default();
    assert_eq!(b.actual_cost(), None); // 初始 None

    b.add_actual_cost(&ProviderCost {
        regular_input: Decimal::from(3),
        cached_input: Decimal::from(0),
        output: Decimal::from(0),
    });
    b.add_actual_cost(&ProviderCost {
        regular_input: Decimal::from(7),
        cached_input: Decimal::from(0),
        output: Decimal::from(0),
    });
    let cost = b.actual_cost().expect("set");
    assert_eq!(cost.regular_input, Decimal::from(10));
    assert_eq!(cost.total(), Decimal::from(10));
}

/// DT-CORE-41：ProviderBilling 累加 usage，含嵌套 prompt_tokens_details.cached_tokens
/// 与可选 cache_creation/cache_read 字段的饱和累加。
#[test]
fn provider_billing_accumulates_usage_nested() {
    let b = ProviderBilling::default();
    assert!(b.actual_usage().is_none());

    b.add_actual_usage(&Usage {
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
        cache_creation_input_tokens: Some(20),
        cache_read_input_tokens: Some(30),
        prompt_tokens_details: Some(boom_core::types::PromptTokensDetails {
            cached_tokens: Some(40),
        }),
    });
    b.add_actual_usage(&Usage {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        cache_creation_input_tokens: None, // None 不影响累加
        cache_read_input_tokens: Some(5),
        prompt_tokens_details: Some(boom_core::types::PromptTokensDetails {
            cached_tokens: Some(60),
        }),
    });

    let u = b.actual_usage().expect("set");
    assert_eq!(u.prompt_tokens, 110);
    assert_eq!(u.completion_tokens, 55);
    assert_eq!(u.total_tokens, 165);
    assert_eq!(u.cache_creation_input_tokens, Some(20)); // None 第二次不增
    assert_eq!(u.cache_read_input_tokens, Some(35));
    assert_eq!(u.prompt_tokens_details.unwrap().cached_tokens, Some(100)); // 40 + 60
}

/// DT-CORE-42：ProviderBilling.usage 的 None 字段不增、Some(0) 不增、无 details 不构造。
#[test]
fn provider_billing_usage_none_details() {
    let b = ProviderBilling::default();
    b.add_actual_usage(&Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        prompt_tokens_details: None,
    });
    let u = b.actual_usage().expect("set");
    assert_eq!(u.prompt_tokens, 0);
    assert!(u.prompt_tokens_details.is_none());
}

// ═════════════════════════════════════════════════════════════
// provider.rs — RawCaptureChannel
// ═════════════════════════════════════════════════════════════

/// DT-CORE-43：request_body 首写者胜出，再次 record 不覆盖；take 取出后清空。
#[test]
fn raw_capture_request_body_first_writer_wins() {
    let c = RawCaptureChannel::default();
    c.record_request_body("first");
    c.record_request_body("second"); // 忽略
    assert_eq!(c.take_request_body().as_deref(), Some("first"));
    assert_eq!(c.take_request_body(), None); // take 后清空
}

/// DT-CORE-44：响应帧 join "\n\n"，单帧原样，空 channel 返回 None。
#[test]
fn raw_capture_response_frames_join() {
    let c = RawCaptureChannel::default();
    c.push_response_frame("data: {\"a\":1}".to_string());
    c.push_response_frame("data: [DONE]".to_string());
    assert_eq!(c.take_response_body().as_deref(), Some("data: {\"a\":1}\n\ndata: [DONE]"));
    assert_eq!(c.take_response_body(), None); // take 后清空

    let c2 = RawCaptureChannel::default();
    c2.push_response_frame("{\"id\":\"x\"}".to_string());
    assert_eq!(c2.take_response_body().as_deref(), Some("{\"id\":\"x\"}"));

    let c3 = RawCaptureChannel::default();
    assert_eq!(c3.take_request_body(), None);
    assert_eq!(c3.take_response_body(), None);
}

// ═════════════════════════════════════════════════════════════
// types.rs — is_hard_blocked_header
// ═════════════════════════════════════════════════════════════

/// DT-CORE-45：网关控制前缀 + 认证/会话/KV 精确名被硬阻断。
#[test]
fn hard_blocked_header_policy() {
    // 前缀阻断
    assert!(is_hard_blocked_header("x-gateway-priority"));
    assert!(is_hard_blocked_header("x-gateway-anything"));
    assert!(is_hard_blocked_header("x-boom-client-type"));
    assert!(is_hard_blocked_header("x-boom-foo"));
    // 精确阻断
    for n in [
        "authorization",
        "x-api-key",
        "api-key",
        "cookie",
        "set-cookie",
        "surrogate-key",
    ] {
        assert!(is_hard_blocked_header(n), "{n} must be blocked");
    }
    // 大小写敏感：阻断名均小写，大写变体放行
    assert!(!is_hard_blocked_header("Authorization"));
    assert!(!is_hard_blocked_header("X-Gateway-Priority"));
}

/// DT-CORE-46：普通自定义 header 放行。
#[test]
fn hard_blocked_header_allows_ordinary() {
    for n in ["x-request-id", "user-agent", "anthropic-beta", "x-custom", "content-type"] {
        assert!(!is_hard_blocked_header(n), "{n} must be allowed");
    }
}

// ═════════════════════════════════════════════════════════════
// types.rs — AuthIdentity 谓词
// ═════════════════════════════════════════════════════════════

/// DT-CORE-47：can_call_model — 空模型列表全放行；非空列表精确匹配。
#[test]
fn auth_identity_can_call_model() {
    let mut id = auth_identity();
    assert!(id.can_call_model("anything")); // 空 → true

    id.models = vec!["gpt-4".to_string(), "claude".to_string()];
    assert!(id.can_call_model("gpt-4"));
    assert!(id.can_call_model("claude"));
    assert!(!id.can_call_model("gpt-3.5"));
}

/// DT-CORE-48：is_expired — None 不过期；过期时间在过去 → true；未来 → false。
#[test]
fn auth_identity_is_expired() {
    let mut id = auth_identity();
    assert!(!id.is_expired()); // None → false

    id.expires_at = Some(chrono::Utc::now().naive_utc() - chrono::Duration::seconds(1));
    assert!(id.is_expired()); // 过去 → true

    id.expires_at = Some(chrono::Utc::now().naive_utc() + chrono::Duration::days(1));
    assert!(!id.is_expired()); // 未来 → false
}

/// DT-CORE-49：is_budget_exceeded — None 不超；spend >= budget → true；< → false。
#[test]
fn auth_identity_is_budget_exceeded() {
    let mut id = auth_identity();
    id.max_budget = None;
    assert!(!id.is_budget_exceeded());

    id.max_budget = Some(100.0);
    id.spend = 50.0;
    assert!(!id.is_budget_exceeded());

    id.spend = 100.0; // 恰好等于 → true（>=）
    assert!(id.is_budget_exceeded());

    id.spend = 150.0;
    assert!(id.is_budget_exceeded());
}

// ═════════════════════════════════════════════════════════════
// types.rs — Message::normalize_reasoning_for_openai
// ═════════════════════════════════════════════════════════════

/// DT-CORE-50：Reasoning 内容提取到 reasoning_content，剩余纯 Text 折叠为 Text(String)。
#[test]
fn message_normalize_reasoning_extracts_and_collapses_text() {
    let mut m = Message {
        role: MessageRole::Assistant,
        content: MessageContent::Parts(vec![
            ContentPart::Reasoning { reasoning: "think1".into() },
            ContentPart::Text { text: "hello".into() },
            ContentPart::Reasoning { reasoning: "think2".into() },
        ]),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    };
    m.normalize_reasoning_for_openai();
    assert_eq!(m.reasoning_content.as_deref(), Some("think1think2"));
    // 仅剩一个 Text → 折叠成 Text(String)
    assert!(matches!(m.content, MessageContent::Text(ref t) if t == "hello"));
}

/// DT-CORE-51：仅 Reasoning 部分 → reasoning_content 有值，content 退化为空 Text。
#[test]
fn message_normalize_reasoning_only_parts() {
    let mut m = Message {
        role: MessageRole::Assistant,
        content: MessageContent::Parts(vec![
            ContentPart::Reasoning { reasoning: "only".into() },
        ]),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    };
    m.normalize_reasoning_for_openai();
    assert_eq!(m.reasoning_content.as_deref(), Some("only"));
    assert!(matches!(m.content, MessageContent::Text(ref t) if t.is_empty()));
}

/// DT-CORE-52：多个非 Text 部分保留为 Parts；含 Text 与 ImageUrl 混合。
#[test]
fn message_normalize_reasoning_keeps_multipart() {
    let mut m = Message {
        role: MessageRole::Assistant,
        content: MessageContent::Parts(vec![
            ContentPart::Reasoning { reasoning: "r".into() },
            ContentPart::Text { text: "t".into() },
            ContentPart::ImageUrl {
                image_url: ImageUrl { url: "u".into(), detail: None },
            },
        ]),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    };
    m.normalize_reasoning_for_openai();
    assert_eq!(m.reasoning_content.as_deref(), Some("r"));
    assert!(matches!(m.content, MessageContent::Parts(ref p) if p.len() == 2));
}

/// DT-CORE-53：无 Reasoning 部分 → reasoning_content 保持 None，content 不变（Text 原样）。
#[test]
fn message_normalize_reasoning_no_reasoning_unchanged() {
    let mut m = Message {
        role: MessageRole::User,
        content: MessageContent::Parts(vec![ContentPart::Text { text: "t".into() }]),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    };
    m.normalize_reasoning_for_openai();
    assert!(m.reasoning_content.is_none());
    // 单个 Text → 折叠为 Text(String)
    assert!(matches!(m.content, MessageContent::Text(ref t) if t == "t"));

    // 非 Parts 的 content（如 Text(String)）直接 return 不动
    let mut m2 = Message {
        role: MessageRole::User,
        content: MessageContent::Text("raw".into()),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    };
    m2.normalize_reasoning_for_openai();
    assert!(matches!(m2.content, MessageContent::Text(ref t) if t == "raw"));
}

// ═════════════════════════════════════════════════════════════
// types.rs — ContentPart 手写 serde
// ═════════════════════════════════════════════════════════════

/// DT-CORE-54：ContentPart 各变体序列化/反序列化往返一致。
#[test]
fn content_part_serde_roundtrip() {
    let cases = vec![
        ContentPart::Text { text: "hi".into() },
        ContentPart::ImageUrl {
            image_url: ImageUrl { url: "https://x/i.png".into(), detail: Some("high".into()) },
        },
        ContentPart::Reasoning { reasoning: "think".into() },
    ];
    for c in &cases {
        let json = serde_json::to_string(c).expect("serialize");
        let back: ContentPart = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    // Unknown 透传任意 JSON
    let raw = serde_json::json!({"type": "audio", "data": "xyz"});
    let unknown: ContentPart = serde_json::from_value(raw.clone()).expect("deserialize unknown");
    let re = serde_json::to_value(&unknown).expect("serialize unknown");
    assert_eq!(re, raw);
}

/// DT-CORE-55：ContentPart 反序列化 — text 缺字段默认空，image_url 非法退化为 Unknown。
#[test]
fn content_part_deserialize_edge_cases() {
    // text 无 text 字段 → 空串
    let v = serde_json::json!({"type": "text"});
    let p: ContentPart = serde_json::from_value(v).unwrap();
    assert!(matches!(p, ContentPart::Text { ref text } if text.is_empty()));

    // image_url 但 image_url 非对象 → Unknown
    let v = serde_json::json!({"type": "image_url", "image_url": "not-an-object"});
    let p: ContentPart = serde_json::from_value(v).unwrap();
    assert!(matches!(p, ContentPart::Unknown(_)));

    // image_url 缺 image_url 字段 → Unknown
    let v = serde_json::json!({"type": "image_url"});
    let p: ContentPart = serde_json::from_value(v).unwrap();
    assert!(matches!(p, ContentPart::Unknown(_)));

    // 无 type → Unknown
    let v = serde_json::json!({"foo": 1});
    let p: ContentPart = serde_json::from_value(v.clone()).unwrap();
    assert!(matches!(p, ContentPart::Unknown(_)));
}

// ═════════════════════════════════════════════════════════════
// types.rs — WindowLimit::is_empty / deserialize_window_limit_vec
// ═════════════════════════════════════════════════════════════

/// DT-CORE-56：WindowLimit::is_empty — 三维全 None 为空，任一有值非空。
#[test]
fn window_limit_is_empty() {
    assert!(WindowLimit::default().is_empty());

    let mut w = WindowLimit::default();
    w.counts = Some(10);
    assert!(!w.is_empty());

    let mut w = WindowLimit::default();
    w.tokens = Some(100);
    assert!(!w.is_empty());

    let mut w = WindowLimit::default();
    w.costs = Some(Decimal::from(1));
    assert!(!w.is_empty());

    // default window_secs = 60 但不影响 is_empty
    assert_eq!(WindowLimit::default().window_secs, 60);
}

/// DT-CORE-57：deserialize_window_limit_vec 接受紧凑数组形式 [counts, tokens, costs, window_secs]。
#[test]
fn deserialize_window_limit_vec_array_form() {
    let json = serde_json::json!([
        [10, 100, "0.5", 60],
        [null, null, null, 120]
    ]);
    let v: Vec<WindowLimit> = serde_json::from_value(json).unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].counts, Some(10));
    assert_eq!(v[0].tokens, Some(100));
    assert_eq!(v[0].costs, Some(Decimal::try_from(0.5f64).unwrap()));
    assert_eq!(v[0].window_secs, 60);

    assert!(v[1].is_empty());
    assert_eq!(v[1].window_secs, 120);
}

/// DT-CORE-58：deserialize_window_limit_vec 接受对象形式 {counts, tokens, costs, window_secs}。
#[test]
fn deserialize_window_limit_vec_object_form() {
    let json = serde_json::json!([
        {"counts": 5, "window_secs": 30},
        {"tokens": 200, "costs": "1.25", "window_secs": 60}
    ]);
    let v: Vec<WindowLimit> = serde_json::from_value(json).unwrap();
    assert_eq!(v[0].counts, Some(5));
    assert!(v[0].tokens.is_none());
    assert_eq!(v[0].window_secs, 30);

    assert_eq!(v[1].tokens, Some(200));
    assert_eq!(v[1].costs, Some(Decimal::try_from(1.25f64).unwrap()));
    assert_eq!(v[1].window_secs, 60);
}

// ═════════════════════════════════════════════════════════════
// types.rs — CompletionRequest::into_chat_request
// ═════════════════════════════════════════════════════════════

/// DT-CORE-59：String prompt 包装为单条 user 消息；Strings 用 "\n" join。
#[test]
fn completion_request_into_chat_request() {
    let r = CompletionRequest {
        model: "gpt-4".into(),
        prompt: CompletionPrompt::String("hello".into()),
        max_tokens: Some(100),
        temperature: Some(0.5),
        top_p: None,
        stop: None,
        n: None,
        suffix: None,
        stream: Some(true),
        extra: serde_json::Map::new(),
    };
    let chat = r.into_chat_request();
    assert_eq!(chat.model, "gpt-4");
    assert_eq!(chat.messages.len(), 1);
    assert!(matches!(chat.messages[0].role, MessageRole::User));
    assert!(matches!(&chat.messages[0].content, MessageContent::Text(t) if t == "hello"));
    assert_eq!(chat.max_tokens, Some(100));
    assert_eq!(chat.temperature, Some(0.5));
    assert_eq!(chat.stream, Some(true));

    // Strings 形式 join 换行
    let r2 = CompletionRequest {
        model: "m".into(),
        prompt: CompletionPrompt::Strings(vec!["a".into(), "b".into()]),
        max_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        n: None,
        suffix: None,
        stream: None,
        extra: serde_json::Map::new(),
    };
    let chat2 = r2.into_chat_request();
    assert!(matches!(&chat2.messages[0].content, MessageContent::Text(t) if t == "a\nb"));
}

// ═════════════════════════════════════════════════════════════
// types.rs — lenient 反序列化（Usage / ChatCompletionResponse 容错）
// ═════════════════════════════════════════════════════════════

/// DT-CORE-60：Usage 对 missing/null/数字 字段容错（lenient）。
#[test]
fn usage_lenient_deserialize() {
    // 全 null → 0
    let u: Usage = serde_json::from_str(
        r#"{"prompt_tokens":null,"completion_tokens":null,"total_tokens":null}"#,
    )
    .unwrap();
    assert_eq!(u.prompt_tokens, 0);
    assert_eq!(u.completion_tokens, 0);
    assert_eq!(u.total_tokens, 0);

    // 浮点数 → 截断为 u32
    let u: Usage = serde_json::from_str(r#"{"prompt_tokens":12.9,"completion_tokens":3.0,"total_tokens":15.7}"#).unwrap();
    assert_eq!(u.prompt_tokens, 12);
    assert_eq!(u.completion_tokens, 3);
    assert_eq!(u.total_tokens, 15);

    // 缺字段 → default 0
    let u: Usage = serde_json::from_str(r#"{}"#).unwrap();
    assert_eq!(u.prompt_tokens, 0);
}

/// DT-CORE-61：ChatCompletionResponse 对缺失 identity 字段容错。
#[test]
fn chat_completion_response_lenient_identity() {
    let json = r#"{"choices":[]}"#; // 无 id/object/created/model
    let r: boom_core::types::ChatCompletionResponse = serde_json::from_str(json).unwrap();
    assert_eq!(r.id, "");
    assert_eq!(r.object, "");
    assert_eq!(r.created, 0);
    assert_eq!(r.model, "");
    assert!(r.choices.is_empty());
    assert!(r.usage.is_none());
}

/// DT-CORE-62：ToolCall 对缺失 id/type 用空串容错（lenient::string_lenient）。
#[test]
fn tool_call_lenient_string_fields() {
    let json = r#"{"function":{}}"#; // 缺 id/type
    let tc: boom_core::types::ToolCall = serde_json::from_str(json).unwrap();
    assert_eq!(tc.id, "");
    assert_eq!(tc.call_type, "");
    assert_eq!(tc.function.name, "");
    assert_eq!(tc.function.arguments, "");

    // null 字段 → 空串；function 字段有 `#[serde(default)]` 只容缺字段不容显式 null，
    // 故此处用显式 null 的 id/type + 在场 function 验证 lenient::string_lenient 路径。
    let json2 = r#"{"id":null,"type":null,"function":{"name":"f","arguments":"a"}}"#;
    let tc2: boom_core::types::ToolCall = serde_json::from_str(json2).unwrap();
    assert_eq!(tc2.id, "");
    assert_eq!(tc2.call_type, "");
    assert_eq!(tc2.function.name, "f");
    assert_eq!(tc2.function.arguments, "a");

    // 边界：function 字段仅有 `#[serde(default)]`（容缺失）不容显式 null —— 显式 null 应报错。
    let bad = r#"{"id":null,"type":null,"function":null}"#;
    assert!(serde_json::from_str::<boom_core::types::ToolCall>(bad).is_err());
}

// ═════════════════════════════════════════════════════════════
// types.rs — MessageRole / StopSequence serde
// ═════════════════════════════════════════════════════════════

/// DT-CORE-63：MessageRole 序列化为小写；未知角色回退到 Unknown。
#[test]
fn message_role_serde_lowercase_and_unknown_fallback() {
    assert_eq!(serde_json::to_string(&MessageRole::User).unwrap(), r#""user""#);
    assert_eq!(serde_json::to_string(&MessageRole::Assistant).unwrap(), r#""assistant""#);
    assert_eq!(serde_json::to_string(&MessageRole::System).unwrap(), r#""system""#);
    assert_eq!(serde_json::to_string(&MessageRole::Tool).unwrap(), r#""tool""#);

    // 未知角色 → Unknown（#[serde(other)]）
    let r: MessageRole = serde_json::from_str(r#""developer""#).unwrap();
    assert!(matches!(r, MessageRole::Unknown));
    let r: MessageRole = serde_json::from_str(r#""function""#).unwrap();
    assert!(matches!(r, MessageRole::Unknown));
    assert_eq!(serde_json::to_string(&MessageRole::Unknown).unwrap(), r#""unknown""#);
}

/// DT-CORE-64：StopSequence 单字符串与数组两种 untagged 形式。
#[test]
fn stop_sequence_untagged_forms() {
    use boom_core::types::StopSequence;
    let s: StopSequence = serde_json::from_str(r#""<stop>""#).unwrap();
    assert!(matches!(s, StopSequence::Single(ref t) if t == "<stop>"));

    let s: StopSequence = serde_json::from_str(r#"["a","b"]"#).unwrap();
    assert!(matches!(s, StopSequence::Multiple(ref v) if v == &vec!["a".to_string(), "b".to_string()]));
}

// ═════════════════════════════════════════════════════════════
// kv_event.rs — StorageTier
// ═════════════════════════════════════════════════════════════

/// DT-CORE-65：StorageTier::priority_score 递减 Gpu > Cpu > Ssd > Remote。
#[test]
fn storage_tier_priority_score_ordered() {
    assert_eq!(StorageTier::Gpu.priority_score(), 1.0);
    assert_eq!(StorageTier::Cpu.priority_score(), 0.7);
    assert_eq!(StorageTier::Ssd.priority_score(), 0.4);
    assert_eq!(StorageTier::Remote.priority_score(), 0.2);
    assert!(StorageTier::Gpu.priority_score() > StorageTier::Cpu.priority_score());
    assert!(StorageTier::Cpu.priority_score() > StorageTier::Ssd.priority_score());
    assert!(StorageTier::Ssd.priority_score() > StorageTier::Remote.priority_score());
}

/// DT-CORE-66：StorageTier serde rename_all = "lowercase" 往返。
#[test]
fn storage_tier_serde_lowercase() {
    for (tier, name) in [
        (StorageTier::Gpu, "gpu"),
        (StorageTier::Cpu, "cpu"),
        (StorageTier::Ssd, "ssd"),
        (StorageTier::Remote, "remote"),
    ] {
        let s = serde_json::to_string(&tier).unwrap();
        assert_eq!(s, format!("\"{name}\""));
        let back: StorageTier = serde_json::from_str(&s).unwrap();
        assert_eq!(back, tier);
    }
}

// ═════════════════════════════════════════════════════════════
// otlp_config.rs — OtlpConfig::default
// ═════════════════════════════════════════════════════════════

/// DT-CORE-67：OtlpConfig::default 各字段默认值与历史 boom-promptlog 默认对齐。
#[test]
fn otlp_config_default_values() {
    let c = OtlpConfig::default();
    assert!(!c.enabled);
    assert_eq!(c.endpoint, "");
    assert_eq!(c.service_name, "boom-gateway");
    assert!(c.service_version.is_none());
    assert_eq!(c.timeout_secs, 10);
    assert_eq!(c.batch_size, 512);
    assert_eq!(c.flush_interval_secs, 5);
    assert_eq!(c.max_attribute_bytes, 4096);
    assert_eq!(c.max_queue_size, 10000);
    assert!(c.headers.is_empty());
}

/// DT-CORE-68：OtlpConfig serde 往返保留自定义值。
#[test]
fn otlp_config_serde_roundtrip() {
    let yaml = "
enabled: true
endpoint: http://otel:4318
service_name: svc
service_version: v1
timeout_secs: 30
batch_size: 64
flush_interval_secs: 2
max_attribute_bytes: 2048
max_queue_size: 100
headers:
  x-auth: token
";
    let c: OtlpConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(c.enabled);
    assert_eq!(c.endpoint, "http://otel:4318");
    assert_eq!(c.service_name, "svc");
    assert_eq!(c.service_version.as_deref(), Some("v1"));
    assert_eq!(c.timeout_secs, 30);
    assert_eq!(c.batch_size, 64);
    assert_eq!(c.max_queue_size, 100);
    assert_eq!(c.headers.get("x-auth").unwrap(), "token");

    // 缺字段时走 default 函数
    let c2: OtlpConfig = serde_yaml::from_str("enabled: false\n").unwrap();
    assert_eq!(c2.service_name, "boom-gateway");
    assert_eq!(c2.batch_size, 512);
}

// ═════════════════════════════════════════════════════════════
// types.rs — PlanType / MessageContent 默认
// ═════════════════════════════════════════════════════════════

/// DT-CORE-69：PlanType::default() == Key；serde lowercase 往返。
#[test]
fn plan_type_default_and_serde() {
    use boom_core::types::PlanType;
    assert!(matches!(PlanType::default(), PlanType::Key));
    assert_eq!(serde_json::to_string(&PlanType::Key).unwrap(), r#""key""#);
    assert_eq!(serde_json::to_string(&PlanType::Team).unwrap(), r#""team""#);
    let t: PlanType = serde_json::from_str(r#""team""#).unwrap();
    assert!(matches!(t, PlanType::Team));
}

/// DT-CORE-70：MessageContent::default() == Text("")；untagged 三形态可互转。
#[test]
fn message_content_default_and_untagged() {
    assert!(matches!(MessageContent::default(), MessageContent::Text(ref t) if t.is_empty()));

    // Text
    let c: MessageContent = serde_json::from_str(r#""hi""#).unwrap();
    assert!(matches!(c, MessageContent::Text(ref t) if t == "hi"));

    // Parts（数组）
    let c: MessageContent =
        serde_json::from_str(r#"[{"type":"text","text":"a"}]"#).unwrap();
    assert!(matches!(c, MessageContent::Parts(ref p) if p.len() == 1));

    // Null
    let c: MessageContent = serde_json::from_str(r#"null"#).unwrap();
    assert!(matches!(c, MessageContent::Null));
}
