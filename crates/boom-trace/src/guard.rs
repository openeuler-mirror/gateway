//! `TraceGuard` — RAII wrapper that finalizes the active span on Drop.
//!
//! The handler creates a guard at request ingress (after `should_trace`
//! decides a span is wanted). The guard holds a strong ref to the registry
//! and the request_id; when dropped it calls `finalize_ok` (or
//! `finalize_error` if `mark_error` was called) — so handlers don't need to
//! remember to finalize on every return path, including client disconnect.
//!
//! When tracing is disabled (filter didn't match, or channel off), `None`
//! is the natural representation — handlers call `TraceGuard::start_opt`
//! which returns `Option<TraceGuard>` and the `None` case short-circuits
//! all subsequent attribute setters.

use crate::context::W3cContext;
use crate::registry::TraceRegistry;
use crate::span::RequestSpan;
use boom_core::trace::SpanAttributeValue;
use std::sync::Arc;

/// Decide whether to trace this request. True when EITHER (a) the
/// tracestate carries any of `tracestate_keys`, OR (b) the trace_id
/// matches `trace_id_regex` (simple prefix-or-substring match — when the
/// pattern starts with `^`, prefix match; else `contains`). Both empty
/// ⇒ trace every request. This is a deliberate simplification to avoid
/// pulling in the `regex` crate; if the user needs full regex semantics,
/// file an issue and we'll wire up `regex` as an optional dep.
pub fn trace_filter_matches(
    tracestate_keys: &[String],
    trace_id_regex: &Option<String>,
    w3c: &W3cContext,
) -> bool {
    if tracestate_keys.is_empty() && trace_id_regex.is_none() {
        return true;
    }
    if let Some(key) = tracestate_keys
        .iter()
        .find(|k| w3c.trace_state.contains(&format!("{k}=")))
    {
        let _ = key;
        return true;
    }
    if let Some(pattern) = trace_id_regex {
        let trace_hex = hex::encode(w3c.trace_id);
        let matched = if let Some(rest) = pattern.strip_prefix('^') {
            trace_hex.starts_with(rest)
        } else {
            trace_hex.contains(pattern.as_str())
        };
        if matched {
            return true;
        }
    }
    false
}

/// RAII guard. Drop finalizes the span (OK or ERROR) and removes it from
/// the active map. Clone is intentionally NOT derived — exactly one
/// `TraceGuard` per span; the registry keeps its own clone under the
/// request_id key for `with_span_mut` access.
pub struct TraceGuard {
    registry: Arc<TraceRegistry>,
    request_id: String,
    /// Set when the handler signals an error path. Drop reads this to
    /// pick between `finalize_ok` / `finalize_error`. Default = OK.
    errored: bool,
    /// Set when the handler wants to override the error message on Drop.
    error_message: Option<String>,
}

impl TraceGuard {
    /// Start a new in-flight span. Metrics need full coverage (issue #49),
    /// so the span is created for EVERY request whenever the trace channel
    /// is enabled — the caller passes a synthetic W3C context when the
    /// inbound request carries no `traceparent`. `should_trace` (inbound
    /// traceparent parsed + report filter matched) only decides whether the
    /// span is *exported* as an OTLP trace; metrics record regardless.
    /// Returns `None` when the registry is disabled — the handler treats
    /// `None` as a no-op for all subsequent setter calls.
    ///
    /// `now_unix_nano` is the caller's time basis (so the queue-wait
    /// attribute can use the same clock).
    pub fn start(
        registry: Arc<TraceRegistry>,
        request_id: String,
        w3c: &W3cContext,
        model: String,
        api_path: String,
        is_stream: bool,
        now_unix_nano: u64,
        should_trace: bool,
    ) -> Option<Self> {
        if !registry.enabled() {
            return None;
        }
        registry.start_request(
            request_id.clone(),
            w3c,
            model,
            api_path,
            is_stream,
            now_unix_nano,
            should_trace,
        );
        Some(Self {
            registry,
            request_id,
            errored: false,
            error_message: None,
        })
    }

    /// Stamp an attribute on the active span. No-op when the guard is
    /// `None` (see `start` — handler should call via `Option::as_mut`).
    pub fn set_attribute(&self, key: &str, value: SpanAttributeValue) {
        let _ = self.registry.with_span_mut(&self.request_id, |s: &mut RequestSpan| {
            s.set_attribute(key, value);
        });
    }

    /// Convenience: set the `boom-gateway.llm_request` body attribute.
    /// The Arc is shared with `PromptLogEntry.request` — zero-copy.
    pub fn set_llm_request(&self, body: Arc<serde_json::Value>) {
        let _ = self.registry.with_span_mut(&self.request_id, |s: &mut RequestSpan| {
            s.set_llm_request(body);
        });
    }

    /// Convenience: set the `boom-gateway.llm_response` body attribute.
    /// The Arc is shared with `PromptLogEntry.response` — zero-copy.
    pub fn set_llm_response(&self, body: Arc<serde_json::Value>) {
        let _ = self.registry.with_span_mut(&self.request_id, |s: &mut RequestSpan| {
            s.set_llm_response(body);
        });
    }

    /// Mark the span as ending in error. Drop will call `finalize_error`
    /// with this message.
    pub fn mark_error(&mut self, message: String) {
        self.errored = true;
        self.error_message = Some(message);
    }

    // ---- Metrics data setters (issue #49) ----
    // Thin write-throughs onto the span's structured timing fields; the
    // values are consumed once at finalize by `MetricsRecorder`.

    pub fn set_queue_wait_ms(&self, ms: u64) {
        let _ = self.registry.with_span_mut(&self.request_id, |s| s.set_queue_wait_ms(ms));
    }

    pub fn set_routing_duration_ms(&self, ms: u64) {
        let _ = self
            .registry
            .with_span_mut(&self.request_id, |s| s.set_routing_duration_ms(ms));
    }

    pub fn set_ttft_ms(&self, ms: u64) {
        let _ = self.registry.with_span_mut(&self.request_id, |s| s.set_ttft_ms(ms));
    }

    pub fn set_duration_ms(&self, ms: u64) {
        let _ = self.registry.with_span_mut(&self.request_id, |s| s.set_duration_ms(ms));
    }

    pub fn set_chunk_stats(&self, count: u64, gen_ms: u64) {
        let _ = self
            .registry
            .with_span_mut(&self.request_id, |s| s.set_chunk_stats(count, gen_ms));
    }

    /// Stamp the deployment chosen by routing plus the model it actually
    /// serves (post alias/fallback resolution).
    pub fn set_deployment(&self, deployment_id: Option<String>, response_model: Option<String>) {
        let _ = self.registry.with_span_mut(&self.request_id, |s| {
            if let Some(did) = deployment_id {
                s.set_deployment_id(did);
            }
            if let Some(m) = response_model {
                s.set_response_model(m);
            }
        });
    }

    /// Upstream dispatch marker: stamps the span and bumps
    /// `http.client.active_requests`.
    pub fn note_upstream_start(&self) {
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
        self.registry.note_upstream_started(&self.request_id, now);
    }

    /// Upstream completion marker (stream end or non-stream response).
    /// Error paths that return between start/end are reconciled at finalize.
    pub fn note_upstream_end(&self) {
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
        self.registry.note_upstream_ended(&self.request_id, now);
    }

    /// The 8-byte gateway span_id, suitable for use as the parent_span_id in
    /// a child traceparent minted for the upstream LLM call. Returns `None`
    /// only if the active span was already evicted from the registry (which
    /// shouldn't happen in normal operation — the guard is supposed to keep
    /// the span alive via `with_span_mut`).
    pub fn child_parent_span_id(&self) -> [u8; 8] {
        self.registry
            .with_span_mut(&self.request_id, |s: &mut RequestSpan| s.span_id)
            .unwrap_or([0u8; 8])
    }

    /// The request_id this guard owns. Used by handlers to correlate
    /// with prompt log entries (same id appears in both channels).
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
}

impl Drop for TraceGuard {
    fn drop(&mut self) {
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
        if self.errored {
            let msg = self.error_message.take().unwrap_or_default();
            self.registry.finalize_error(&self.request_id, now, msg);
        } else {
            self.registry.finalize_ok(&self.request_id, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::parse_traceparent;
    use boom_core::TraceApi;

    fn make_w3c() -> W3cContext {
        let mut ctx = parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01",
        )
        .unwrap();
        ctx.trace_state = "opencode_user_id=alice".to_string();
        ctx
    }

    #[tokio::test]
    async fn guard_none_when_registry_disabled() {
        let reg = TraceRegistry::new();
        // Default enabled = false.
        let w3c = make_w3c();
        let g = TraceGuard::start(
            reg.clone(),
            "req-1".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            1_000,
            true, // should_trace=true but registry.enabled()=false → None
        );
        assert!(g.is_none());
        assert_eq!(reg.active_count(), 0);
    }

    #[tokio::test]
    async fn guard_starts_span_and_finalizes_on_drop() {
        let reg = TraceRegistry::new();
        reg.set_enabled_for_test(true);
        let w3c = make_w3c();
        {
            let _g = TraceGuard::start(
                reg.clone(),
                "req-2".to_string(),
                &w3c,
                "m".to_string(),
                "/p".to_string(),
                false,
                1_000,
                true,
            );
            assert_eq!(reg.active_count(), 1);
        }
        assert_eq!(reg.active_count(), 0);
        let snap = reg.snapshot().await;
        assert_eq!(snap.total_spans_started, 1);
        assert_eq!(snap.total_spans_finalized, 1);
        assert_eq!(snap.total_spans_error, 0);
    }

    #[tokio::test]
    async fn guard_mark_error_routes_to_finalize_error() {
        let reg = TraceRegistry::new();
        reg.set_enabled_for_test(true);
        let w3c = make_w3c();
        {
            let mut g = TraceGuard::start(
                reg.clone(),
                "req-3".to_string(),
                &w3c,
                "m".to_string(),
                "/p".to_string(),
                false,
                1_000,
                true,
            )
            .expect("started");
            g.mark_error("upstream 500".to_string());
        }
        let snap = reg.snapshot().await;
        assert_eq!(snap.total_spans_error, 1);
    }

    #[tokio::test]
    async fn guard_set_llm_request_stores_attribute() {
        let reg = TraceRegistry::new();
        reg.set_enabled_for_test(true);
        let w3c = make_w3c();
        let body = Arc::new(serde_json::json!({"messages": []}));
        {
            let g = TraceGuard::start(
                reg.clone(),
                "req-4".to_string(),
                &w3c,
                "m".to_string(),
                "/p".to_string(),
                false,
                1_000,
                true,
            )
            .expect("started");
            g.set_llm_request(body.clone());
            // Verify attribute was set on the live span.
            let has_attr = reg
                .with_span_mut("req-4", |s: &mut RequestSpan| {
                    s.attributes
                        .iter()
                        .any(|(k, _)| k == "boom-gateway.llm_request")
                })
                .unwrap_or(false);
            assert!(has_attr);
        }
        // After drop, span is finalized but should be in the slow ring
        // (slow_threshold_ms=0 by default in test). The attribute was
        // set on the live span — the snapshot may or may not carry
        // attributes depending on `to_snapshot`. We just verify no panic.
        assert_eq!(reg.active_count(), 0);
    }
}
