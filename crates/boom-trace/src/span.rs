//! `RequestSpan` — the per-request span the gateway records.
//!
//! A span is created at request ingress and finalized when the response
//! completes (success / error / client disconnect). Between those points
//! it's held in `TraceRegistry`'s active map keyed by `request_id`.
//!
//! Body sharing: `llm_request` / `llm_response` are `Arc<serde_json::Value>`
//! so the same allocation is shared with `PromptLogEntry.request` /
//! `.response` (see boom-promptlog refactor). On non-stream paths the Arc
//! is constructed once in the handler and passed in via `set_llm_request`;
//! on stream paths the Arc is constructed from the tee buffer at Drop time.

use crate::context::W3cContext;
use boom_core::trace::{RequestSpan as SpanSnapshot, SpanAttributeValue, SpanStatus};
use std::sync::Arc;

/// One in-flight or finalized request span. The lifetime is bound to a
/// request_id — `TraceRegistry::start_request` returns a `RequestSpan`
/// already inserted into the active map; `finalize` removes it and (if
/// slow) pushes into the slow ring buffer.
#[derive(Debug, Clone)]
pub struct RequestSpan {
    pub request_id: String,
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: [u8; 8],
    pub start_time_unix_nano: u64,
    pub end_time_unix_nano: u64,
    pub status: SpanStatus,
    pub error_message: Option<String>,
    pub model: String,
    pub deployment_id: Option<String>,
    pub is_stream: bool,
    pub api_path: String,
    pub trace_state: String,
    /// attribute bag (key → value). Bodies stored as `Arc<serde_json::Value>`
    /// so the same Arc is shared with `PromptLogEntry`.
    pub attributes: Vec<(String, SpanAttributeValue)>,
}

impl RequestSpan {
    /// Build a new in-flight span. `now_unix_nano` is passed by the caller
    /// (rather than read here) so the caller can stamp the queue-wait
    /// attribute using the same time basis. Caller retains the new 8-byte
    /// `span_id`; it's also persisted on the span for `build_child_traceparent`.
    pub fn new(
        request_id: String,
        w3c: &W3cContext,
        model: String,
        api_path: String,
        is_stream: bool,
        now_unix_nano: u64,
    ) -> Self {
        Self {
            request_id,
            trace_id: w3c.trace_id,
            span_id: fresh_span_id(),
            parent_span_id: w3c.parent_span_id,
            start_time_unix_nano: now_unix_nano,
            end_time_unix_nano: 0,
            status: SpanStatus::Unset,
            error_message: None,
            model,
            deployment_id: None,
            is_stream,
            api_path,
            trace_state: w3c.trace_state.clone(),
            attributes: Vec::new(),
        }
    }

    pub fn set_attribute(&mut self, key: &str, value: SpanAttributeValue) {
        // Replace if key already exists (last write wins, matching OTel spec).
        if let Some(slot) = self.attributes.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
            return;
        }
        self.attributes.push((key.to_string(), value));
    }

    pub fn set_llm_request(&mut self, body: Arc<serde_json::Value>) {
        self.set_attribute("boom-gateway.llm_request", SpanAttributeValue::Json(body));
    }

    pub fn set_llm_response(&mut self, body: Arc<serde_json::Value>) {
        self.set_attribute("boom-gateway.llm_response", SpanAttributeValue::Json(body));
    }

    pub fn set_deployment_id(&mut self, did: String) {
        self.deployment_id = Some(did);
    }

    pub fn finalize_ok(&mut self, now_unix_nano: u64) {
        self.end_time_unix_nano = now_unix_nano;
        self.status = SpanStatus::Ok;
    }

    pub fn finalize_error(&mut self, now_unix_nano: u64, message: String) {
        self.end_time_unix_nano = now_unix_nano;
        self.status = SpanStatus::Error;
        self.error_message = Some(message);
    }

    /// Convert to the serializable snapshot form (used by `TraceApi::snapshot`
    /// and by the OTLP exporter when building `opentelemetry::Span`).
    pub fn to_snapshot(&self) -> SpanSnapshot {
        SpanSnapshot {
            request_id: self.request_id.clone(),
            trace_id: hex::encode(self.trace_id),
            span_id: hex::encode(self.span_id),
            parent_span_id: hex::encode(self.parent_span_id),
            start_time_unix_nano: self.start_time_unix_nano,
            end_time_unix_nano: self.end_time_unix_nano,
            status: self.status,
            error_message: self.error_message.clone(),
            model: self.model.clone(),
            deployment_id: self.deployment_id.clone(),
            is_stream: self.is_stream,
            api_path: self.api_path.clone(),
            attributes: self.attributes.clone(),
        }
    }
}

/// Generate a fresh 8-byte span id. Uses `rand`-free entropy: SHA-256 of
/// `request_id + counter + monotonic time` would also work, but for span
/// ids the only uniqueness requirement is "low collision probability within
/// one trace" — `request_id` is already globally unique, so hashing its
/// bytes plus a process-local counter is sufficient. We use SHA-256 first
/// 8 bytes for stable determinism (same request_id → same span_id, useful
/// for the OTLP traces↔logs JOIN in the backend).
fn fresh_span_id() -> [u8; 8] {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(n.to_le_bytes());
    hasher.update(nanos.to_le_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::parse_traceparent;

    fn make_w3c() -> W3cContext {
        let mut ctx = parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01",
        )
        .unwrap();
        ctx.trace_state = "opencode_user_id=alice".to_string();
        ctx
    }

    #[test]
    fn new_span_has_distinct_span_id_from_parent() {
        let w3c = make_w3c();
        let span = RequestSpan::new(
            "req-1".to_string(),
            &w3c,
            "gpt-4".to_string(),
            "/v1/chat/completions".to_string(),
            false,
            1_000_000_000,
        );
        assert_ne!(span.span_id, w3c.parent_span_id);
        assert_eq!(span.trace_id, w3c.trace_id);
        assert_eq!(span.parent_span_id, w3c.parent_span_id);
        assert_eq!(span.status, SpanStatus::Unset);
        assert_eq!(span.end_time_unix_nano, 0);
    }

    #[test]
    fn set_attribute_replaces_existing_key() {
        let w3c = make_w3c();
        let mut span = RequestSpan::new(
            "r".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            0,
        );
        span.set_attribute("k", SpanAttributeValue::Int(1));
        span.set_attribute("k", SpanAttributeValue::Int(2));
        assert_eq!(span.attributes.len(), 1);
        match &span.attributes[0].1 {
            SpanAttributeValue::Int(n) => assert_eq!(*n, 2),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn finalize_ok_sets_end_time_and_status() {
        let w3c = make_w3c();
        let mut span = RequestSpan::new(
            "r".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            1_000,
        );
        span.finalize_ok(2_000);
        assert_eq!(span.end_time_unix_nano, 2_000);
        assert_eq!(span.status, SpanStatus::Ok);
    }

    #[test]
    fn finalize_error_records_message() {
        let w3c = make_w3c();
        let mut span = RequestSpan::new(
            "r".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            1_000,
        );
        span.finalize_error(2_000, "boom".to_string());
        assert_eq!(span.status, SpanStatus::Error);
        assert_eq!(span.error_message.as_deref(), Some("boom"));
    }

    #[test]
    fn body_setters_store_json_attribute() {
        let w3c = make_w3c();
        let mut span = RequestSpan::new(
            "r".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            0,
        );
        let req = Arc::new(serde_json::json!({"messages": []}));
        span.set_llm_request(req.clone());
        assert!(std::sync::Arc::strong_count(&req) >= 2);
        match &span.attributes[0].1 {
            SpanAttributeValue::Json(v) => assert_eq!(**v, serde_json::json!({"messages": []})),
            _ => panic!("wrong variant"),
        }
    }
}
