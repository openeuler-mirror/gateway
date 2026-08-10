use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which phase of the request lifecycle this entry records.
///
/// `Request` entries are written the moment a request enters the gateway —
/// before the upstream is contacted. `Response` entries are written at
/// completion (success, error, or disconnect) and reference the same
/// `request_id` so the two halves can be correlated downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogPhase {
    Request,
    Response,
}

/// A single prompt log entry — one line in a JSONL file.
///
/// Entries come in two phases (see [`LogPhase`]):
/// - `Request` phase: `request` is set, response fields are `None`. Written
///   the moment the request enters the gateway so it's available even if the
///   upstream hangs or the client disconnects.
/// - `Response` phase: `response` is set (assembled full body for streams, not
///   chunk arrays), plus `status_code` / `duration_ms` / `error_code`. The
///   shared identity fields (`request_id` / `key_hash` / `team_alias` / `model`
///   / `api_path` / `is_stream` / `client_ip` / `domain_account` / `headers`)
///   are duplicated into both phases so each phase can be indexed independently.
///
/// `trace_id` links the two phases for downstream correlation. If the inbound
/// request carried a W3C `traceparent` header, that trace id is reused;
/// otherwise the gateway mints one at request entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptLogEntry {
    pub phase: LogPhase,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    pub timestamp: String,
    pub key_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_alias: Option<String>,
    pub model: String,
    pub api_path: String,
    pub is_stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_ip: Option<String>,
    /// Domain account derived from key_alias (last space-separated segment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_account: Option<String>,
    /// Whitelisted request headers snapshot (keys lowercased). Populated only
    /// when `prompt_log.record_headers` is non-empty. Carried in BOTH phases
    /// so a response entry can be correlated by user-supplied headers (e.g.
    /// `X-Trace-Id`) without joining back to the request entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    // ── Request phase ─────────────────────────────────────────────
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<serde_json::Value>,
    // ── Response phase ───────────────────────────────────────────
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Assembled full response body. For streaming responses this is the
    /// reconstructed ChatCompletion-style JSON (content concatenated, not a
    /// chunk array). For non-streaming responses it's the literal body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<serde_json::Value>,
    /// Raw upstream response before any gateway format conversion.
    /// Only populated when `capture_raw_upstream` is enabled and the endpoint
    /// performs format conversion (e.g., `/v1/messages`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_upstream_response: Option<serde_json::Value>,
    /// Standardized error code for failed/interrupted requests. See
    /// `StreamErrorCode` for the canonical values. `None` on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Canonical error codes used in `PromptLogEntry::error_code`. Stored as
/// strings (not an enum) so the JSONL schema stays open for new codes without
/// a migration. Keep this list in sync with `boom_core::GatewayError` variants.
pub mod error_code {
    pub const CLIENT_DISCONNECTED: &str = "CLIENT_DISCONNECTED";
    pub const UPSTREAM_ERROR: &str = "UPSTREAM_ERROR";
    pub const TIMEOUT: &str = "TIMEOUT";
    pub const MODEL_NOT_ALLOWED: &str = "MODEL_NOT_ALLOWED";
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    pub const AUTH_FAILED: &str = "AUTH_FAILED";
    pub const BAD_REQUEST: &str = "BAD_REQUEST";
    pub const INTERNAL_ERROR: &str = "INTERNAL_ERROR";
}

impl PromptLogEntry {
    /// Build the Request-phase entry at request ingress.
    ///
    /// `trace_id` should be the W3C traceparent-derived id when available;
    /// pass `None` and the gateway will mint one upstream. The returned entry
    /// only carries request-phase fields — response fields stay `None`.
    #[allow(clippy::too_many_arguments)]
    pub fn new_request(
        request_id: &str,
        trace_id: Option<String>,
        key_hash: &str,
        key_alias: Option<&str>,
        team_alias: Option<&str>,
        model: &str,
        api_path: &str,
        is_stream: bool,
        request_body: serde_json::Value,
        client_ip: Option<&str>,
        headers: Option<HashMap<String, String>>,
    ) -> Self {
        let domain_account = key_alias
            .and_then(|a| a.rsplit_once(' ').map(|(_, last)| last.to_string()));
        Self {
            phase: LogPhase::Request,
            request_id: request_id.to_string(),
            trace_id,
            timestamp: chrono::Utc::now().to_rfc3339(),
            key_hash: key_hash.to_string(),
            team_alias: team_alias.map(|s| s.to_string()),
            model: model.to_string(),
            api_path: api_path.to_string(),
            is_stream,
            client_ip: client_ip.map(|s| s.to_string()),
            domain_account,
            headers,
            request: Some(request_body),
            status_code: None,
            duration_ms: None,
            response: None,
            raw_upstream_response: None,
            error_code: None,
            error_message: None,
        }
    }

    /// Build the Response-phase entry by cloning the shared identity fields
    /// from a Request-phase entry. Caller then sets response body / status /
    /// duration / error via the setters below.
    pub fn new_response_from(request_entry: &PromptLogEntry) -> Self {
        Self {
            phase: LogPhase::Response,
            request_id: request_entry.request_id.clone(),
            trace_id: request_entry.trace_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            key_hash: request_entry.key_hash.clone(),
            team_alias: request_entry.team_alias.clone(),
            model: request_entry.model.clone(),
            api_path: request_entry.api_path.clone(),
            is_stream: request_entry.is_stream,
            client_ip: request_entry.client_ip.clone(),
            domain_account: request_entry.domain_account.clone(),
            headers: request_entry.headers.clone(),
            request: None,
            status_code: None,
            duration_ms: None,
            response: None,
            raw_upstream_response: None,
            error_code: None,
            error_message: None,
        }
    }

    pub fn set_response(&mut self, response: serde_json::Value) {
        self.response = Some(response);
    }

    pub fn set_raw_upstream_response(&mut self, raw: serde_json::Value) {
        self.raw_upstream_response = Some(raw);
    }

    pub fn set_status(&mut self, status_code: i32, duration_ms: u64) {
        self.status_code = Some(status_code);
        self.duration_ms = Some(duration_ms);
    }

    pub fn set_error(&mut self, error_code: &str, error_message: String) {
        self.error_code = Some(error_code.to_string());
        self.error_message = Some(error_message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_request_marks_phase_and_leaves_response_none() {
        let entry = PromptLogEntry::new_request(
            "req-1",
            Some("trace-abc".to_string()),
            "keyhash",
            Some("team foo"),
            Some("team-a"),
            "gpt-4",
            "/v1/chat/completions",
            false,
            serde_json::json!({"messages": []}),
            Some("127.0.0.1"),
            None,
        );
        assert_eq!(entry.phase, LogPhase::Request);
        assert_eq!(entry.request_id, "req-1");
        assert_eq!(entry.trace_id.as_deref(), Some("trace-abc"));
        assert_eq!(entry.domain_account.as_deref(), Some("foo"));
        assert!(entry.request.is_some());
        assert!(entry.response.is_none());
        assert!(entry.status_code.is_none());
        assert!(entry.error_code.is_none());
    }

    #[test]
    fn new_response_from_clones_shared_identity_and_sets_phase() {
        let req = PromptLogEntry::new_request(
            "req-2",
            Some("trace-xyz".to_string()),
            "kh",
            Some("acct bar"),
            Some("team-b"),
            "claude-3",
            "/v1/messages",
            true,
            serde_json::json!({}),
            None,
            Some(HashMap::from([("x-trace-id".to_string(), "t1".to_string())])),
        );
        let mut resp = PromptLogEntry::new_response_from(&req);
        assert_eq!(resp.phase, LogPhase::Response);
        assert_eq!(resp.request_id, "req-2");
        assert_eq!(resp.trace_id.as_deref(), Some("trace-xyz"));
        assert_eq!(resp.key_hash, "kh");
        assert_eq!(resp.team_alias.as_deref(), Some("team-b"));
        assert_eq!(resp.model, "claude-3");
        assert!(resp.is_stream);
        assert_eq!(
            resp.headers.as_ref().and_then(|h| h.get("x-trace-id")),
            Some(&"t1".to_string())
        );
        // Response-phase should not carry the request body.
        assert!(resp.request.is_none());
        assert!(resp.response.is_none());

        // Setters mutate response-phase fields.
        resp.set_status(200, 1234);
        resp.set_response(serde_json::json!({"id": "resp-2"}));
        assert_eq!(resp.status_code, Some(200));
        assert_eq!(resp.duration_ms, Some(1234));
        assert!(resp.response.is_some());

        resp.set_error(error_code::CLIENT_DISCONNECTED, "client gone".to_string());
        assert_eq!(resp.error_code.as_deref(), Some(error_code::CLIENT_DISCONNECTED));
        assert_eq!(resp.error_message.as_deref(), Some("client gone"));
    }

    #[test]
    fn phase_serializes_as_lowercase_string() {
        let req = PromptLogEntry::new_request(
            "r",
            None,
            "k",
            None,
            None,
            "m",
            "/p",
            false,
            serde_json::json!({}),
            None,
            None,
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["phase"], "request");

        let resp = PromptLogEntry::new_response_from(&req);
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["phase"], "response");
    }
}
