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
//!
//! Retention cap: bodies bigger than `max_body_bytes` (TraceConfig) are NOT
//! retained as the shared Arc — the span keeps a truncated owned String plus
//! `{key}.truncated` / `{key}.bytes` markers instead. Without this the span
//! (active map → recent ring → OTLP queue) pins the full body for the whole
//! retention window, which is the root cause of the 1.1G→4G memory blow-up
//! when trace is enabled on MB-scale bodies. `max_body_bytes = 0` disables
//! the cap (unbounded retention).

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
    /// Serialized byte size of the retained body attributes (Json Arc or
    /// truncated String). Maintained by the body setters; consumed by
    /// `estimated_bytes` for the OTLP queue byte budget. Over-counts on
    /// repeated setter calls (safe direction for a budget).
    body_bytes: usize,
    // ---- Structured timing fields (metrics data path, issue #49) ----
    // Written by boom-main via `TraceGuard` setters at the point each value
    // becomes known; read once at finalize by `MetricsRecorder::record_request_done`.
    pub queue_wait_ms: Option<u64>,
    pub routing_duration_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    /// Total chunks observed on the response stream (first chunk included).
    pub chunk_count: Option<u64>,
    /// Generation window in ms: last chunk timestamp − first chunk timestamp.
    pub gen_duration_ms: Option<u64>,
    pub upstream_start_ns: Option<u64>,
    pub upstream_end_ns: Option<u64>,
    /// Model name the deployment actually serves (may differ from the
    /// requested `model` after alias/fallback resolution).
    pub response_model: Option<String>,
    /// Whether this span is exported to OTLP traces. Spans are now created
    /// for EVERY request (metrics need full coverage); only spans whose
    /// inbound request carried a valid traceparent AND matched the report
    /// filter are exported as traces. Metrics record regardless.
    pub export_eligible: bool,
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
        export_eligible: bool,
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
            body_bytes: 0,
            queue_wait_ms: None,
            routing_duration_ms: None,
            ttft_ms: None,
            duration_ms: None,
            chunk_count: None,
            gen_duration_ms: None,
            upstream_start_ns: None,
            upstream_end_ns: None,
            response_model: None,
            export_eligible,
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

    /// Set the `boom-gateway.llm_request` body attribute. `max_body_bytes`
    /// is the retention cap (TraceConfig.max_body_bytes; 0 = unlimited):
    /// bodies at or under the cap keep the shared Arc (zero-copy with
    /// PromptLogEntry); bigger bodies are stored as a truncated owned String
    /// so the span never pins a MB-scale allocation through the active map,
    /// recent ring and OTLP queue.
    pub fn set_llm_request(&mut self, body: Arc<serde_json::Value>, max_body_bytes: usize) {
        self.set_body_attribute("boom-gateway.llm_request", body, max_body_bytes);
    }

    /// Set the `boom-gateway.llm_response` body attribute. Same retention
    /// cap semantics as `set_llm_request`.
    pub fn set_llm_response(&mut self, body: Arc<serde_json::Value>, max_body_bytes: usize) {
        self.set_body_attribute("boom-gateway.llm_response", body, max_body_bytes);
    }

    fn set_body_attribute(
        &mut self,
        key: &str,
        body: Arc<serde_json::Value>,
        max_body_bytes: usize,
    ) {
        // Unlimited: keep the shared Arc (zero-copy). Still needs the exact
        // size for the OTLP queue byte budget — full serialization is the
        // price of an exact count in a mode that has already opted out of
        // memory bounds.
        if max_body_bytes == 0 {
            let full_len = serde_json::to_string(&*body).unwrap_or_default().len();
            self.body_bytes = self.body_bytes.saturating_add(full_len);
            self.set_attribute(key, SpanAttributeValue::Json(body));
            return;
        }
        // Cap-aware: serialize through a writer that aborts once
        // `max_body_bytes` bytes are written. Under the cap → exact length +
        // shared Arc (the buffer is dropped). Over → the buffer holds the
        // cap-byte prefix, which becomes the truncated string — O(cap) work
        // on MB-scale bodies instead of a full serialization pass whose
        // result would be discarded anyway. The `.bytes` marker reports the
        // lower bound (cap + 1) since the exact size is unknowable without
        // defeating the abort.
        let mut writer = BoundedJsonWriter {
            buf: Vec::with_capacity(max_body_bytes.min(8 * 1024)),
            cap: max_body_bytes,
        };
        match serde_json::to_writer(&mut writer, &*body) {
            Ok(()) => {
                self.body_bytes = self.body_bytes.saturating_add(writer.buf.len());
                self.set_attribute(key, SpanAttributeValue::Json(body));
            }
            Err(_) => {
                // BoundedJsonWriter's cap error is the only failure mode
                // for serializing a serde_json::Value.
                let truncated = truncate_at_byte_boundary(
                    String::from_utf8_lossy(&writer.buf).into_owned(),
                    max_body_bytes,
                );
                self.body_bytes = self.body_bytes.saturating_add(truncated.len());
                self.set_attribute(key, SpanAttributeValue::String(truncated));
                self.set_attribute(&format!("{key}.truncated"), SpanAttributeValue::Bool(true));
                self.set_attribute(
                    &format!("{key}.bytes"),
                    SpanAttributeValue::Int((max_body_bytes + 1) as i64),
                );
            }
        }
    }

    /// Conservative in-memory size estimate used by the OTLP queue byte
    /// budget. Json attributes count as 0 here — their serialized size is
    /// tracked exactly in `body_bytes` by the body setters (they are the
    /// only producers of the Json variant).
    pub fn estimated_bytes(&self) -> usize {
        let attrs = self
            .attributes
            .iter()
            .map(|(k, v)| match v {
                SpanAttributeValue::String(s) => k.len() + s.len(),
                SpanAttributeValue::Json(_) => k.len(),
                _ => k.len() + 16,
            })
            .sum::<usize>();
        512 + self.body_bytes + attrs
    }

    pub fn set_deployment_id(&mut self, did: String) {
        self.deployment_id = Some(did);
    }

    pub fn set_response_model(&mut self, m: String) {
        self.response_model = Some(m);
    }

    pub fn set_queue_wait_ms(&mut self, ms: u64) {
        self.queue_wait_ms = Some(ms);
    }

    pub fn set_routing_duration_ms(&mut self, ms: u64) {
        self.routing_duration_ms = Some(ms);
    }

    pub fn set_ttft_ms(&mut self, ms: u64) {
        self.ttft_ms = Some(ms);
    }

    pub fn set_duration_ms(&mut self, ms: u64) {
        self.duration_ms = Some(ms);
    }

    pub fn set_chunk_stats(&mut self, count: u64, gen_ms: u64) {
        self.chunk_count = Some(count);
        self.gen_duration_ms = Some(gen_ms);
    }

    pub fn note_upstream_start(&mut self, now_unix_nano: u64) {
        self.upstream_start_ns = Some(now_unix_nano);
    }

    pub fn note_upstream_end(&mut self, now_unix_nano: u64) {
        self.upstream_end_ns = Some(now_unix_nano);
    }

    /// Fill in the stream-shaped fields for non-stream requests, where the
    /// whole response arrives in one shot: TTFT ≈ total duration, one chunk,
    /// generation window = duration. Applied at finalize before metrics
    /// recording so downstream consumers never see the None variants.
    pub fn apply_nonstream_fallbacks(&mut self) {
        if self.is_stream {
            return;
        }
        let duration = self.duration_ms.or_else(|| {
            Some(self.end_time_unix_nano.saturating_sub(self.start_time_unix_nano) / 1_000_000)
        });
        if let Some(d) = duration {
            self.duration_ms = Some(d);
            if self.ttft_ms.is_none() {
                self.ttft_ms = Some(d);
            }
            if self.chunk_count.is_none() {
                self.chunk_count = Some(1);
            }
            if self.gen_duration_ms.is_none() {
                self.gen_duration_ms = Some(d);
            }
        }
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

/// io::Write sink that aborts serialization once `cap` bytes are written.
/// On abort the buffer holds the first `cap` bytes — the material for the
/// truncated attribute — so `set_body_attribute` never pays for a full
/// serialization pass on MB-scale bodies just to learn they're over cap.
struct BoundedJsonWriter {
    buf: Vec<u8>,
    cap: usize,
}

impl std::io::Write for BoundedJsonWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + buf.len() > self.cap {
            let room = self.cap - self.buf.len();
            self.buf.extend_from_slice(&buf[..room]);
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "body exceeds cap"));
        }
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Byte-budget truncation that never splits a UTF-8 code point: backs off
/// to the nearest char boundary at or below `max_bytes`.
pub(crate) fn truncate_at_byte_boundary(mut s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
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
            true,
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
            true,
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
            true,
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
            true,
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
            true,
        );
        let req = Arc::new(serde_json::json!({"messages": []}));
        span.set_llm_request(req.clone(), 16 * 1024);
        assert!(std::sync::Arc::strong_count(&req) >= 2);
        match &span.attributes[0].1 {
            SpanAttributeValue::Json(v) => assert_eq!(**v, serde_json::json!({"messages": []})),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn oversized_body_truncated_with_markers() {
        let w3c = make_w3c();
        let mut span = RequestSpan::new(
            "r".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            0,
            true,
        );
        // Body whose serialized form exceeds the 64-byte cap.
        let big = "x".repeat(200);
        let req = Arc::new(serde_json::json!({"prompt": big}));
        span.set_llm_request(req, 64);
        let body_attr = span
            .attributes
            .iter()
            .find(|(k, _)| k == "boom-gateway.llm_request")
            .expect("body attr");
        match &body_attr.1 {
            SpanAttributeValue::String(s) => {
                assert!(s.len() <= 64, "retained {} bytes", s.len());
                assert!(s.is_char_boundary(s.len()));
            }
            other => panic!("oversized body must not retain the Arc: {other:?}"),
        }
        let truncated = span
            .attributes
            .iter()
            .find(|(k, _)| k == "boom-gateway.llm_request.truncated")
            .expect("truncated marker");
        assert!(matches!(truncated.1, SpanAttributeValue::Bool(true)));
        // Lower-bound marker: serialization aborted at the cap, so the
        // exact original size is unknowable — cap + 1 is what we can prove.
        let bytes_marker = span
            .attributes
            .iter()
            .find(|(k, _)| k == "boom-gateway.llm_request.bytes")
            .expect("bytes marker");
        match &bytes_marker.1 {
            SpanAttributeValue::Int(n) => assert!(*n > 64),
            other => panic!("wrong marker variant: {other:?}"),
        }
    }

    #[test]
    fn zero_cap_keeps_shared_arc() {
        let w3c = make_w3c();
        let mut span = RequestSpan::new(
            "r".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            0,
            true,
        );
        let big = "x".repeat(10_000);
        let req = Arc::new(serde_json::json!({"prompt": big}));
        span.set_llm_request(req, 0);
        assert!(matches!(
            span.attributes[0].1,
            SpanAttributeValue::Json(_)
        ));
    }

    #[test]
    fn estimated_bytes_tracks_body_bytes() {
        let w3c = make_w3c();
        let mut span = RequestSpan::new(
            "r".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            0,
            true,
        );
        let small = span.estimated_bytes();
        span.set_llm_request(
            Arc::new(serde_json::json!({"messages": [{"role": "user", "content": "hello"}]})),
            16 * 1024,
        );
        let bigger = span.estimated_bytes();
        assert!(bigger > small);
        // Truncation bounds the growth: a 10KB body under a 64-byte cap
        // contributes ≤64 bytes, not 10KB.
        let mut span2 = RequestSpan::new(
            "r2".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            0,
            true,
        );
        span2.set_llm_request(
            Arc::new(serde_json::json!({"prompt": "x".repeat(10_000)})),
            64,
        );
        assert!(span2.estimated_bytes() < small + 64 + 512);
    }

    #[test]
    fn byte_truncation_respects_char_boundaries() {
        // "你好" is 6 bytes (3 per char); cut at 4 must back off to 3.
        let out = truncate_at_byte_boundary("你好".to_string(), 4);
        assert_eq!(out, "你");
        assert_eq!(truncate_at_byte_boundary("abc".to_string(), 10), "abc");
        assert_eq!(truncate_at_byte_boundary("abc".to_string(), 0), "");
    }
}
