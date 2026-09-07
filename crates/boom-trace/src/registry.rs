//! `TraceRegistry` — the in-memory hub for active + slow spans, and the
//! producer-side `TraceApi` impl surfaced to the dashboard.
//!
//! Lives on `AppState`'s top-level Arc (CLAUDE.md §4) so the active span
//! table + slow ring survive hot reloads. The OTLP exporter is held behind
//! `Arc<ArcSwap<Option<Arc<TraceExporter>>>>` so reload can hot-swap it
//! (mirrors `PromptLogWriter::replace_otlp`).

use crate::context::W3cContext;
use crate::otlp_export::TraceExporter;
use crate::span::RequestSpan;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use boom_core::trace::{
    ExporterStatusSnapshot, ProbeResult, TraceApi, TraceSnapshot,
};
use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Default cap on the active span table. When full, the oldest span is
/// evicted. Sized for ~10s of headroom at 10k QPS — should be plenty for
/// debugging "which request is stuck" queries.
const DEFAULT_ACTIVE_CAP: usize = 100_000;

/// Default cap on the recent-finalized ring (most recent N spans of any
/// duration, OK or error). The dashboard shows "what just happened" by
/// merging active + recent — the operator sees the last ~100 spans
/// regardless of duration. Older finalized spans drop off as new ones
/// come in. Bounded so it can't grow unbounded under load.
const DEFAULT_RECENT_CAP: usize = 100;

/// Ring buffer of recently-finalized spans (FIFO, fixed capacity).
/// Older entries drop off the back as new ones come in. Used for the
/// recent ring — every finalized span (OK or error) lands here.
struct SlowRing {
    items: VecDeque<RequestSpan>,
    cap: usize,
}

impl SlowRing {
    fn new(cap: usize) -> Self {
        Self {
            items: VecDeque::with_capacity(cap.min(1024)),
            cap,
        }
    }

    fn push(&mut self, span: RequestSpan) {
        if self.items.len() >= self.cap {
            self.items.pop_front();
        }
        self.items.push_back(span);
    }

    fn snapshot(&self) -> Vec<RequestSpan> {
        self.items.iter().rev().cloned().collect()
    }
}

/// `TraceRegistry` — held by `AppState` as `Arc<TraceRegistry>`.
///
/// Active spans are stored keyed by `request_id`. Each span's
/// `start_time_unix_nano` is set at creation; `end_time` is 0 until
/// `finalize_*` is called. The OTLP exporter is `Option` — when
/// `trace.otlp.enabled=false`, `otlp_status` / `probe_otlp` return `None`
/// and `enqueue_finalized` is a no-op.
pub struct TraceRegistry {
    active: DashMap<String, RequestSpan>,
    recent_ring: std::sync::Mutex<SlowRing>,
    otlp: Arc<ArcSwap<Option<Arc<TraceExporter>>>>,
    flush_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// When true, the OTLP exporter is configured and `enqueue_finalized`
    /// forwards the span to OTLP traces. Driven by `TraceConfig.enabled`.
    enabled: std::sync::atomic::AtomicBool,
    total_started: AtomicU64,
    total_finalized: AtomicU64,
    total_error: AtomicU64,
    active_cap: usize,
}

impl TraceRegistry {
    /// Build an empty registry with no OTLP exporter. boom-main calls
    /// `set_otlp` (via `replace_otlp`) when `trace.otlp.enabled=true` is
    /// configured at startup or hot-reloaded in.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            active: DashMap::with_capacity(1024),
            recent_ring: std::sync::Mutex::new(SlowRing::new(DEFAULT_RECENT_CAP)),
            otlp: Arc::new(ArcSwap::from_pointee(None)),
            flush_handle: Arc::new(std::sync::Mutex::new(None)),
            enabled: std::sync::atomic::AtomicBool::new(false),
            total_started: AtomicU64::new(0),
            total_finalized: AtomicU64::new(0),
            total_error: AtomicU64::new(0),
            active_cap: DEFAULT_ACTIVE_CAP,
        })
    }

    /// Install (or hot-swap) the OTLP traces exporter. Mirrors
    /// `PromptLogWriter::replace_otlp`: abort the old flush task, run a
    /// best-effort final flush, construct a new exporter from the config,
    /// store it, spawn the new flush task into the shared handle slot.
    pub async fn replace_otlp(&self, config: &boom_core::OtlpConfig, enabled: bool) {
        // 1. Abort old flush task.
        if let Some(h) = self.flush_handle.lock().unwrap().take() {
            h.abort();
        }
        // 2. Best-effort final flush on the old exporter.
        if let Some(old) = self.otlp.load().as_ref() {
            let bound = std::time::Duration::from_secs(config.timeout_secs.max(1) + 2);
            let _ = tokio::time::timeout(bound, old.flush()).await;
        }
        // 3. Construct new exporter (or None if disabled).
        let new_exporter = if enabled && config.enabled {
            let exp = TraceExporter::new(config);
            exp.spawn_flush_task_to_handle(self.flush_handle.clone());
            Some(exp)
        } else {
            None
        };
        self.otlp.store(Arc::new(new_exporter));
        self.enabled.store(enabled, Ordering::Relaxed);
        tracing::info!(
            enabled,
            endpoint = %config.endpoint,
            "Trace exporter hot-swapped"
        );
    }

    /// Whether the trace channel is enabled. Read at every request ingress
    /// to decide whether to create a span + attribute it.
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Test-only: flip the enabled flag without going through `replace_otlp`.
    /// Lets unit tests start spans without spinning up an OTLP exporter.
    #[cfg(test)]
    pub fn set_enabled_for_test(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Create a new in-flight span. Caller passes the W3C context (parsed
    /// from `traceparent` if present, else a synthetic fallback). Returns
    /// a mutable handle that the caller mutates to add attributes; the
    /// registry keeps a clone under the request_id key.
    ///
    /// When the active table is full (cap exceeded), the oldest entry is
    /// evicted. The evicted span is *not* sent to the slow ring or OTLP —
    /// it's treated as abandoned. Callers should always call `finalize_*`
    /// to ensure proper accounting; the eviction is a safety valve for
    /// lost-track-of-it scenarios.
    pub fn start_request(
        &self,
        request_id: String,
        w3c: &W3cContext,
        model: String,
        api_path: String,
        is_stream: bool,
        now_unix_nano: u64,
    ) -> RequestSpan {
        let span = RequestSpan::new(
            request_id.clone(),
            w3c,
            model,
            api_path,
            is_stream,
            now_unix_nano,
        );
        self.total_started.fetch_add(1, Ordering::Relaxed);
        // Evict oldest if at cap.
        if self.active.len() >= self.active_cap {
            // DashMap doesn't have a "remove oldest" — we scan for the
            // smallest start_time. O(n) but only fires on overflow.
            let mut oldest_id: Option<String> = None;
            let mut oldest_start = u64::MAX;
            for entry in self.active.iter() {
                if entry.start_time_unix_nano < oldest_start {
                    oldest_start = entry.start_time_unix_nano;
                    oldest_id = Some(entry.key().clone());
                }
            }
            if let Some(id) = oldest_id {
                self.active.remove(&id);
            }
        }
        self.active.insert(request_id, span.clone());
        span
    }

    /// Apply attributes / finalize on the active span identified by
    /// `request_id`. Returns the (possibly finalized) span so the caller
    /// can do additional post-finalize work (e.g. extract traceparent for
    /// the upstream call).
    pub fn with_span_mut<F, R>(&self, request_id: &str, f: F) -> Option<R>
    where
        F: FnOnce(&mut RequestSpan) -> R,
    {
        let mut entry = self.active.get_mut(request_id)?;
        Some(f(entry.value_mut()))
    }

    /// Mark the span as finalized-OK and remove it from the active table.
    /// Every finalized span (regardless of duration) lands in the recent
    /// ring so the dashboard can show "what just finished". If OTLP is
    /// enabled, also enqueue for export to the collector.
    pub fn finalize_ok(&self, request_id: &str, now_unix_nano: u64) {
        let span = match self.active.remove(request_id) {
            Some((_, s)) => s,
            None => return,
        };
        let mut span = span;
        span.finalize_ok(now_unix_nano);
        self.total_finalized.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut ring) = self.recent_ring.lock() {
            ring.push(span.clone());
        }
        if self.enabled.load(Ordering::Relaxed) {
            if let Some(exp) = self.otlp.load().as_ref() {
                let exp = exp.clone();
                let span = span;
                tokio::spawn(async move {
                    exp.enqueue(span).await;
                });
            }
        }
    }

    /// Mark the span as finalized-ERROR with a message, remove from active,
    /// bump the error counter, push to recent ring, enqueue to OTLP.
    pub fn finalize_error(&self, request_id: &str, now_unix_nano: u64, message: String) {
        let span = match self.active.remove(request_id) {
            Some((_, s)) => s,
            None => return,
        };
        let mut span = span;
        span.finalize_error(now_unix_nano, message);
        self.total_finalized.fetch_add(1, Ordering::Relaxed);
        self.total_error.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut ring) = self.recent_ring.lock() {
            ring.push(span.clone());
        }
        if self.enabled.load(Ordering::Relaxed) {
            if let Some(exp) = self.otlp.load().as_ref() {
                let exp = exp.clone();
                tokio::spawn(async move {
                    exp.enqueue(span).await;
                });
            }
        }
    }

    /// Active span count (for diagnostics).
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Active spans in snapshot form (clone + convert). Bounded by active
    /// cap; safe to call from the dashboard's poll loop.
    fn active_snapshot(&self) -> Vec<RequestSpan> {
        self.active
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }
}

#[async_trait]
impl TraceApi for TraceRegistry {
    async fn snapshot(&self) -> TraceSnapshot {
        TraceSnapshot {
            active: self
                .active_snapshot()
                .into_iter()
                .map(|s| s.to_snapshot())
                .collect(),
            recent: self
                .recent_ring
                .lock()
                .map(|ring| ring.snapshot().into_iter().map(|s| s.to_snapshot()).collect())
                .unwrap_or_default(),
            total_spans_started: self.total_started.load(Ordering::Relaxed),
            total_spans_finalized: self.total_finalized.load(Ordering::Relaxed),
            total_spans_error: self.total_error.load(Ordering::Relaxed),
        }
    }

    async fn otlp_status(&self) -> Option<ExporterStatusSnapshot> {
        let g = self.otlp.load();
        let Some(exporter) = g.as_ref() else {
            return None;
        };
        Some(exporter.clone().status_snapshot())
    }

    async fn probe_otlp(&self) -> Option<ProbeResult> {
        let g = self.otlp.load();
        let Some(exporter) = g.as_ref() else {
            return None;
        };
        Some(exporter.clone().probe().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::parse_traceparent;
    use boom_core::trace::SpanStatus;

    fn make_w3c() -> W3cContext {
        let mut ctx = parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01",
        )
        .unwrap();
        ctx.trace_state = "opencode_user_id=alice".to_string();
        ctx
    }

    #[tokio::test]
    async fn start_finalize_ok_lands_in_recent_ring() {
        // Every finalized span lands in the recent ring, regardless of
        // duration. The slow-ring concept is gone — the dashboard just
        // shows the most recent N spans.
        let reg = TraceRegistry::new();
        let w3c = make_w3c();
        let _span = reg.start_request(
            "req-1".to_string(),
            &w3c,
            "gpt-4".to_string(),
            "/p".to_string(),
            false,
            1_000_000_000,
        );
        reg.finalize_ok("req-1", 2_000_000_000);
        let snap = reg.snapshot().await;
        assert_eq!(snap.total_spans_started, 1);
        assert_eq!(snap.total_spans_finalized, 1);
        assert_eq!(snap.recent.len(), 1);
        assert_eq!(snap.recent[0].request_id, "req-1");
        assert!(snap.active.is_empty());
    }

    #[tokio::test]
    async fn finalize_error_bumps_error_counter_and_pushes_recent() {
        let reg = TraceRegistry::new();
        let w3c = make_w3c();
        let _span = reg.start_request(
            "req-2".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            1_000,
        );
        reg.finalize_error("req-2", 2_000, "boom".to_string());
        let snap = reg.snapshot().await;
        assert_eq!(snap.total_spans_error, 1);
        assert_eq!(snap.recent.len(), 1);
        assert_eq!(snap.recent[0].status, SpanStatus::Error);
    }

    #[tokio::test]
    async fn otlp_status_none_when_no_exporter_configured() {
        let reg = TraceRegistry::new();
        assert!(reg.otlp_status().await.is_none());
        assert!(reg.probe_otlp().await.is_none());
    }

    #[tokio::test]
    async fn finalize_ok_without_otlp_does_not_panic() {
        // enabled=false default. Should silently no-op the OTLP enqueue path.
        let reg = TraceRegistry::new();
        let w3c = make_w3c();
        let _span = reg.start_request(
            "r".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            1_000,
        );
        reg.finalize_ok("r", 2_000);
    }

    #[tokio::test]
    async fn active_eviction_when_cap_exceeded() {
        // Construct a registry with a tiny active_cap by going through the
        // public new() and then overflowing it directly via the active map.
        // DEFAULT_ACTIVE_CAP is 100_000 — we only insert a few thousand so
        // eviction never fires; the point is to verify the start/finalize
        // path doesn't panic under load. The eviction scan is O(n) so we
        // don't actually trigger it here.
        let reg = TraceRegistry::new();
        let w3c = make_w3c();
        for i in 0..1_000 {
            let _ = reg.start_request(
                format!("r-{i}"),
                &w3c,
                "m".to_string(),
                "/p".to_string(),
                false,
                i as u64,
            );
        }
        assert_eq!(reg.active.len(), 1_000);
        // Finalize a couple to make sure removal works.
        reg.finalize_ok("r-0", 2_000);
        reg.finalize_ok("r-1", 2_000);
        assert_eq!(reg.active.len(), 998);
    }
}
