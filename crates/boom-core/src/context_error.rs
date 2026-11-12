//! Context-length error identification — rollback L1 (design doc §4).
//!
//! Identifies upstream "context too long" rejections so the rollback chain
//! (PR4) can fall back to a larger-capability tier. Conservative by design:
//! an unmatched error is NEVER a context error — a false positive would
//! retry a non-context failure on the fallback tier.
//!
//! Input contract: `GatewayError::UpstreamError.message` carries the raw
//! upstream response body (boom-provider puts `resp.text()` there verbatim),
//! so JSON bodies are parsed for `error.code` / `error.message` first;
//! non-JSON bodies fall back to substring matching on the raw text.
//!
//! Matching rules (per `router_settings.rollback.context.patterns`):
//! - `code:<CODE>` — exact match against the JSON `error.code` field;
//! - anything else — substring match against the error message text
//!   (the JSON inner message when parseable, the raw body otherwise).
//! - Empty pattern list = identification off (never matches) — an explicit
//!   way to disable via config; the serde default list is non-empty.
//!
//! Numeric extraction (for PR4's smallest-sufficient-tier selection):
//! - requested tokens: `<digits>` after `requested`, suffix ` token`
//!   (GLM "you requested 210625 tokens", vLLM same phrasing);
//! - max tokens: after `length is ` (GLM "maximum input length is 204799",
//!   vLLM "maximum context length is 262144") or the Anthropic shape
//!   `<n> tokens > <m> maximum`.
//! All scanners are flat `while` loops over byte slices — no recursion, no
//! regex engine, bounded stack by construction.

use crate::error::GatewayError;

/// Identification result. `matched` is the decision; the token numbers are
/// best-effort hints (present on the empirically verified GLM / vLLM /
/// Anthropic error shapes, `None` on anything else — the match still holds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextErrorMatch {
    pub matched: bool,
    /// Tokens the request asked for (parsed from the error text).
    pub requested_tokens: Option<u64>,
    /// The rejecting instance's actual context limit (parsed from the text).
    pub max_tokens: Option<u64>,
}

impl ContextErrorMatch {
    pub fn is_match(&self) -> bool {
        self.matched
    }
}

/// Built-in pattern table (serde default for
/// `router_settings.rollback.context.patterns`). Covers the empirically
/// verified GLM / vLLM / Anthropic stacks.
pub const DEFAULT_CONTEXT_ERROR_PATTERNS: &[&str] = &[
    "code:InputTokenExceeded",      // GLM (error.code, exact)
    "code:context_length_exceeded", // OpenAI / Azure (error.code, exact)
    "maximum input length",         // GLM (message substring)
    "maximum context length",       // vLLM / OpenAI (message substring)
    "prompt is too long",           // Anthropic (message substring)
];

/// Identify a context-length error from an upstream rejection.
///
/// Only HTTP 400 can match: every verified stack rejects over-long prompts
/// at admission with 400; treating other statuses as context errors would
/// be guesswork (and 401/403 have their own failure paths).
pub fn identify(status: u16, upstream_body: &str, patterns: &[String]) -> ContextErrorMatch {
    if status != 400 {
        return ContextErrorMatch::default();
    }

    // ── Surface extraction: JSON first, raw text fallback. Flat. ──
    // The parsed Value stays alive for the whole function so the borrowed
    // message slice remains valid through the numeric scans below.
    // A JSON body exposes ONLY its structured surfaces (error.code /
    // error.message) — falling back to the raw text would let substring
    // patterns accidentally match JSON structure (code/type field names).
    // Non-JSON bodies match on the raw text.
    let parsed: Option<serde_json::Value> = serde_json::from_str(upstream_body).ok();
    let mut code: Option<&str> = None;
    let mut message_text: &str = if parsed.is_some() { "" } else { upstream_body };
    if let Some(v) = parsed.as_ref() {
        // Conventional shapes, tried in order: {"error":{"code","message"}},
        // {"error":"plain"}, {"message":"plain"}.
        if let Some(err) = v.get("error") {
            if let Some(c) = err.get("code").and_then(|c| c.as_str()) {
                code = Some(c);
            }
            if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
                message_text = m;
            } else if let Some(m) = err.as_str() {
                message_text = m;
            }
        } else if let Some(m) = v.get("message").and_then(|m| m.as_str()) {
            message_text = m;
        }
    }

    // ── Pattern evaluation: flat loop, first hit wins. ──
    let mut matched = false;
    for pattern in patterns {
        if let Some(code_pat) = pattern.strip_prefix("code:") {
            if let Some(code) = code {
                if code == code_pat {
                    matched = true;
                    break;
                }
            }
        } else if !pattern.is_empty() && message_text.contains(pattern.as_str()) {
            matched = true;
            break;
        }
    }
    if !matched {
        return ContextErrorMatch::default();
    }

    // ── Numeric hints: flat scanners, best-effort. ──
    ContextErrorMatch {
        matched: true,
        // requested: GLM/vLLM put it after "requested"; Anthropic puts it
        // BEFORE " tokens > " — try forward, then backward.
        requested_tokens: number_after(message_text, "requested", " token")
            .or_else(|| number_before(message_text, " tokens > ")),
        max_tokens: number_after(message_text, "length is ", " token")
            .or_else(|| number_after(message_text, " tokens > ", " maximum")),
    }
}

impl GatewayError {
    /// L1 identification on a gateway error. Only
    /// [`GatewayError::UpstreamError`] carries an upstream status + body;
    /// every other variant (auth, timeout, parse, …) is a non-match.
    pub fn context_error_match(&self, patterns: &[String]) -> ContextErrorMatch {
        match self {
            Self::UpstreamError { status, message } => identify(*status, message, patterns),
            _ => ContextErrorMatch::default(),
        }
    }
}

/// Scan `text` for `key`, then parse the first digit-run after it; the
/// number is accepted only when `required_suffix` immediately follows the
/// digits (conservative: "requested 210625 tokens" yes, "requested by the
/// client" no). Digit-runs that fail the suffix or overflow u64 are skipped
/// and the scan continues at the next key occurrence. Flat loop, no
/// recursion.
fn number_after(text: &str, key: &str, required_suffix: &str) -> Option<u64> {
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(key) {
        let key_end = from + rel + key.len();
        let bytes = text.as_bytes();
        // Skip whitespace between the key and the number.
        let mut i = key_end;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i > digits_start {
            let parsed = text[digits_start..i].parse::<u64>().ok();
            if let Some(n) = parsed {
                if text[i..].starts_with(required_suffix) {
                    return Some(n);
                }
            }
        }
        // No acceptable number at this occurrence — continue scanning.
        from = key_end;
    }
    None
}

/// Mirror of [`number_after`] for the Anthropic shape, where the requested
/// count sits BEFORE the key ("210000 tokens > 200000 maximum"): find the
/// key, walk back over whitespace, parse the digit-run ending there. Flat
/// loop, no recursion.
fn number_before(text: &str, key: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(key) {
        let key_start = from + rel;
        let mut i = key_start;
        while i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
            i -= 1;
        }
        let digits_end = i;
        while i > 0 && bytes[i - 1].is_ascii_digit() {
            i -= 1;
        }
        if i < digits_end {
            if let Ok(n) = text[i..digits_end].parse::<u64>() {
                return Some(n);
            }
        }
        from = key_start + key.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_patterns() -> Vec<String> {
        DEFAULT_CONTEXT_ERROR_PATTERNS
            .iter()
            .map(|p| p.to_string())
            .collect()
    }

    // ── Real-world samples (empirically verified shapes) ──

    const GLM_BODY: &str = r#"{"error":{"code":"InputTokenExceeded","type":"invalid_request_error","message":"This model's maximum input length is 204799 tokens. However, you requested 210625 tokens. Please reduce the length of the prompt or the messages.","param":null}}"#;

    const VLLM_BODY: &str = r#"{"object":"error","message":"This model's maximum context length is 262144 tokens. However, you requested 262145 tokens. ","type":"BadRequestError","param":null,"code":400}"#;

    const ANTHROPIC_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#;

    #[test]
    fn glm_sample_matches_with_numbers() {
        let m = identify(400, GLM_BODY, &default_patterns());
        assert!(m.is_match());
        assert_eq!(m.requested_tokens, Some(210625));
        assert_eq!(m.max_tokens, Some(204799));
    }

    #[test]
    fn vllm_sample_matches_with_numbers() {
        let m = identify(400, VLLM_BODY, &default_patterns());
        assert!(m.is_match());
        assert_eq!(m.requested_tokens, Some(262145));
        assert_eq!(m.max_tokens, Some(262144));
    }

    const OPENAI_BODY: &str = r#"{"error":{"message":"This model's maximum context length is 16385 tokens. However, you requested 17285 tokens, which is the length of the messages and tools you submitted. Please reduce the length of the messages or tools.","type":"invalid_request_error","param":"messages","code":"context_length_exceeded"}}"#;

    #[test]
    fn openai_sample_matches_with_numbers() {
        let m = identify(400, OPENAI_BODY, &default_patterns());
        assert!(m.is_match());
        assert_eq!(m.requested_tokens, Some(17285));
        assert_eq!(m.max_tokens, Some(16385));
        // code-only 命中路径：message 换成无关措辞，code 仍命中
        let code_only = r#"{"error":{"code":"context_length_exceeded","message":"nope"}}"#;
        assert!(identify(400, code_only, &default_patterns()).is_match());
    }

    #[test]
    fn anthropic_sample_matches_with_numbers() {
        let m = identify(400, ANTHROPIC_BODY, &default_patterns());
        assert!(m.is_match());
        assert_eq!(m.requested_tokens, Some(210000));
        assert_eq!(m.max_tokens, Some(200000));
    }

    // ── Matching semantics ──

    #[test]
    fn code_exact_match_beats_odd_message() {
        // code 命中：即使 message 是非上下文措辞
        let body = r#"{"error":{"code":"InputTokenExceeded","message":"nope"}}"#;
        assert!(identify(400, body, &default_patterns()).is_match());
    }

    #[test]
    fn non_400_never_matches() {
        for status in [401u16, 403, 404, 429, 500, 502, 503] {
            let m = identify(status, GLM_BODY, &default_patterns());
            assert!(!m.is_match(), "status {} must not match", status);
        }
    }

    #[test]
    fn unrelated_400_does_not_match() {
        let body = r#"{"error":{"code":"InvalidModel","message":"model not found"}}"#;
        assert!(!identify(400, body, &default_patterns()).is_match());
        assert!(!identify(400, "bad request", &default_patterns()).is_match());
    }

    #[test]
    fn non_json_body_falls_back_to_substring() {
        // 非 JSON 原文命中 message 子串
        let raw = "BadRequestError: This model's maximum context length is 4096 tokens. However, you requested 5000 tokens.";
        let m = identify(400, raw, &default_patterns());
        assert!(m.is_match());
        assert_eq!(m.requested_tokens, Some(5000));
        assert_eq!(m.max_tokens, Some(4096));
    }

    #[test]
    fn code_prefix_is_exact_message_pattern_is_substring() {
        // code:InputTokenExceeded 精确匹配 code 字段
        let body = r#"{"error":{"code":"InputTokenExceeded"}}"#;
        let code_pat = vec!["code:InputTokenExceeded".to_string()];
        assert!(identify(400, body, &code_pat).is_match());
        // 不带 code: 前缀的同名模式只做 message 子串——code-only body 不命中
        let msg_pat = vec!["InputTokenExceeded".to_string()];
        assert!(!identify(400, body, &msg_pat).is_match());
        // 但 message 里包含该词时命中
        let body2 = r#"{"error":{"message":"error InputTokenExceeded occurred"}}"#;
        assert!(identify(400, body2, &msg_pat).is_match());
    }

    #[test]
    fn custom_patterns_and_empty_list() {
        let body = r#"{"error":{"code":"CtxTooLong","message":"whatever"}}"#;
        let custom = vec!["code:CtxTooLong".to_string()];
        assert!(identify(400, body, &custom).is_match());
        // 空模式表 = 识别关闭
        assert!(!identify(400, GLM_BODY, &[]).is_match());
    }

    #[test]
    fn malformed_numbers_dont_panic_or_block_match() {
        // 溢出 u64 的数字串：跳过，不 panic，匹配保持
        let raw = "maximum context length is 99999999999999999999999999 tokens. you requested 12 tokens";
        let m = identify(400, raw, &default_patterns());
        assert!(m.is_match());
        assert_eq!(m.requested_tokens, Some(12));
        assert_eq!(m.max_tokens, None, "overflowing max run must be skipped");
        // requested 无 token 后缀 → None，匹配保持
        let raw2 = "maximum context length is 100 tokens. requested by the client";
        let m2 = identify(400, raw2, &default_patterns());
        assert!(m2.is_match());
        assert_eq!(m2.requested_tokens, None);
        assert_eq!(m2.max_tokens, Some(100));
    }

    #[test]
    fn gateway_error_helper_delegates_and_gates() {
        let e = GatewayError::UpstreamError {
            status: 400,
            message: GLM_BODY.to_string(),
        };
        let m = e.context_error_match(&default_patterns());
        assert!(m.is_match());
        assert_eq!(m.requested_tokens, Some(210625));

        // 非 UpstreamError 变体：直接 no-match
        let other = GatewayError::ProviderError("x".to_string());
        assert!(!other.context_error_match(&default_patterns()).is_match());
        // UpstreamError 但非 400：no-match
        let e401 = GatewayError::UpstreamError {
            status: 401,
            message: GLM_BODY.to_string(),
        };
        assert!(!e401.context_error_match(&default_patterns()).is_match());
    }

    #[test]
    fn number_after_scanner_basics() {
        assert_eq!(number_after("you requested 42 tokens", "requested", " token"), Some(42));
        assert_eq!(number_after("requested 7 tokens twice: requested 8 tokens", "requested", " token"), Some(7));
        assert_eq!(number_after("requested 9 people", "requested", " token"), None);
        assert_eq!(number_after("nothing here", "requested", " token"), None);
        assert_eq!(number_after("a tokens > 5 maximum", " tokens > ", " maximum"), Some(5));
        assert_eq!(number_after(" tokens >  maximum", " tokens > ", " maximum"), None);
        assert_eq!(number_before("210000 tokens > 200000 maximum", " tokens > "), Some(210000));
        assert_eq!(number_before("no digits tokens > 5 maximum", " tokens > "), None);
        assert_eq!(number_before("nothing", " tokens > "), None);
    }
}

// ═══════════════════════════════════════════════════════════════════
// RollbackStats — global counters for the debug page (design §10)
// ═══════════════════════════════════════════════════════════════════

/// Process-lifetime rollback counters, mirroring the KvcSchedulerStats
/// pattern: held at AppState top level (survives config reloads — the
/// module's functions are stateless, so per-module fields would reset).
///
/// Two invariants hold by construction (the debug pane displays them):
///   identify_total == identify_miss + pool_inert + no_target + fallback_selected
///   fallback_selected == retry_success + retry_error + guard_error
#[derive(Default)]
pub struct RollbackStats {
    identify_total: std::sync::atomic::AtomicU64,
    identify_miss: std::sync::atomic::AtomicU64,
    pool_inert: std::sync::atomic::AtomicU64,
    no_target: std::sync::atomic::AtomicU64,
    fallback_selected: std::sync::atomic::AtomicU64,
    retry_success: std::sync::atomic::AtomicU64,
    retry_error: std::sync::atomic::AtomicU64,
    guard_error: std::sync::atomic::AtomicU64,
    passthrough_400: std::sync::atomic::AtomicU64,
}

/// Serializable point-in-time snapshot. Field names are the dashboard API
/// contract (`GET /admin/stats/rollback`).
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct RollbackStatsSnapshot {
    pub identify_total: u64,
    pub identify_miss: u64,
    pub pool_inert: u64,
    pub no_target: u64,
    pub fallback_selected: u64,
    pub retry_success: u64,
    pub retry_error: u64,
    pub guard_error: u64,
    pub passthrough_400: u64,
}

impl RollbackStats {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(field: &std::sync::atomic::AtomicU64) {
        field.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Observability-only: classify an error on a NON-capable pool (the
    /// handler's else branch — no retry will be attempted). Only context
    /// errors are interesting: they indicate a pool that COULD have rolled
    /// back but isn't configured for it.
    pub fn record_identify_total(&self) {
        Self::bump(&self.identify_total);
    }

    pub fn record_identify_miss(&self) {
        Self::bump(&self.identify_miss);
    }

    pub fn record_pool_inert(&self) {
        Self::bump(&self.pool_inert);
    }

    pub fn record_no_target(&self) {
        Self::bump(&self.no_target);
    }

    pub fn record_fallback_selected(&self) {
        Self::bump(&self.fallback_selected);
    }

    pub fn record_pool_inert_if_context_error(&self, error: &GatewayError, patterns: &[String]) {
        if error.context_error_match(patterns).matched {
            Self::bump(&self.pool_inert);
        }
    }

    pub fn record_retry_success(&self) {
        Self::bump(&self.retry_success);
    }

    pub fn record_retry_error(&self) {
        Self::bump(&self.retry_error);
    }

    pub fn record_guard_error(&self) {
        Self::bump(&self.guard_error);
    }

    pub fn record_passthrough_400(&self) {
        Self::bump(&self.passthrough_400);
    }

    pub fn snapshot(&self) -> RollbackStatsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        RollbackStatsSnapshot {
            identify_total: self.identify_total.load(Relaxed),
            identify_miss: self.identify_miss.load(Relaxed),
            pool_inert: self.pool_inert.load(Relaxed),
            no_target: self.no_target.load(Relaxed),
            fallback_selected: self.fallback_selected.load(Relaxed),
            retry_success: self.retry_success.load(Relaxed),
            retry_error: self.retry_error.load(Relaxed),
            guard_error: self.guard_error.load(Relaxed),
            passthrough_400: self.passthrough_400.load(Relaxed),
        }
    }
}

