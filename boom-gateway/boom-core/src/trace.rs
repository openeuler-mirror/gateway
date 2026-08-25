//! Request-level trace types — shared between boom-trace (producer) and
//! boom-dashboard (consumer). Defined here so the dashboard can read the
//! snapshot via `Arc<dyn TraceApi>` without depending on boom-trace
//! (CLAUDE.md §5 trait-over-concrete-type pattern, same shape as
//! `StressmonApi`).
//!
//! Distinct from boom-promptlog's `PromptLogEntry`: that struct records the
//! full request/response body for audit/replay; this module records only
//! per-request span metadata (start/end time, status, attributes) for
//! latency-distribution and trace-correlation use. The two channels share
//! the body `Arc<serde_json::Value>` so memory is single-sourced.

use async_trait::async_trait;
use serde::Serialize;
use std::sync::Arc;

/// Status of a `RequestSpan`. Matches OpenTelemetry's `Span::Status` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpanStatus {
    /// Default state — span has not been finalized.
    Unset,
    /// The operation completed successfully.
    Ok,
    /// The operation failed. `error_message` on the span carries details.
    Error,
}

/// A single key-value attribute on a span. Mirrors OTLP's AnyValue::Value but
/// trimmed to the variants we actually emit (string / int / bool / json).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SpanAttributeValue {
    String(String),
    Int(i64),
    Bool(bool),
    /// Raw JSON body (request/response). Serialized as-is; the producer
    /// holds the original `Arc<serde_json::Value>` and we deref to serialize.
    Json(Arc<serde_json::Value>),
}

/// One finalized or in-flight request span. The concrete struct lives in
/// boom-trace; this is the serializable form surfaced to the dashboard via
/// `TraceSnapshot`. Body fields are `Option<Arc<...>>` so the snapshot is
/// cheap to clone even when the body is large.
#[derive(Debug, Clone, Serialize)]
pub struct RequestSpan {
    pub request_id: String,
    /// W3C trace id (32 hex chars) — reuses the inbound `traceparent`'s
    /// trace id when present, so OTLP logs and OTLP traces share the id
    /// and the backend can JOIN.
    pub trace_id: String,
    /// 16 hex chars. New per gateway span; differs from parent_span_id.
    pub span_id: String,
    /// 16 hex chars. The inbound `traceparent`'s span id (the agent's span),
    /// or empty when no traceparent was supplied.
    pub parent_span_id: String,
    /// Unix nanos. Set at span creation.
    pub start_time_unix_nano: u64,
    /// Unix nanos. Set when finalized (success / error / disconnect).
    /// 0 while in flight.
    pub end_time_unix_nano: u64,
    pub status: SpanStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Model name (public-facing, from the request).
    pub model: String,
    /// Deployment id (if known) — same value as `boom_request_log`'s
    /// deployment_id column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    /// `true` for `chat_stream`, `false` for `chat`.
    pub is_stream: bool,
    /// Api path (`/v1/chat/completions` or `/v1/messages`).
    pub api_path: String,
    /// Attribute bag. Includes `boom-gateway.request.id`,
    /// `boom-gateway.time_in_queue`, `boom-gateway.user.id`,
    /// `boom-gateway.llm_request`, `boom-gateway.llm_response` per the issue.
    pub attributes: Vec<(String, SpanAttributeValue)>,
}

/// Snapshot returned by `TraceApi::snapshot`. Read-only, point-in-time.
#[derive(Debug, Clone, Serialize)]
pub struct TraceSnapshot {
    /// Currently in-flight spans (start_time set, end_time=0). Bounded by
    /// the registry's active-span cap; oldest evicted on overflow.
    pub active: Vec<RequestSpan>,
    /// Most recent N finalized slow spans (duration > slow_threshold).
    /// Bounded by the registry's slow-ring capacity.
    pub slow: Vec<RequestSpan>,
    /// Lifetime counters (monotonic across reloads — the registry is on
    /// AppState's top-level Arc per CLAUDE.md §4).
    pub total_spans_started: u64,
    pub total_spans_finalized: u64,
    pub total_spans_error: u64,
}

/// Read-only trace API surfaced to the dashboard. Producer is
/// `boom_trace::TraceRegistry`; dashboard holds `Arc<dyn TraceApi>` without
/// depending on the boom-trace crate (§5 trait-over-concrete-type pattern).
///
/// The OTLP methods are `Option`-returning: when the OTLP traces exporter
/// is not configured (`TraceConfig.otlp.enabled=false`), they return `None`
/// and the dashboard surfaces "disabled".
#[async_trait]
pub trait TraceApi: Send + Sync + 'static {
    /// Point-in-time snapshot of active + slow spans + lifetime counters.
    async fn snapshot(&self) -> TraceSnapshot;

    /// Read-only state of the OTLP traces exporter. Returns `None` when
    /// traces exporter is not configured. Shape mirrors
    /// `boom_promptlog::ExporterStatusSnapshot` (Online/Offline state
    /// machine) so the dashboard can reuse the same indicator widget.
    async fn otlp_status(&self) -> Option<ExporterStatusSnapshot>;

    /// Manual probe of the live traces exporter. Drives the state machine
    /// (Offline → Online on success). Returns `None` when no exporter is
    /// configured. Shape mirrors `boom_promptlog::ProbeResult`.
    async fn probe_otlp(&self) -> Option<ProbeResult>;
}

/// Status snapshot of the OTLP traces exporter. Shape mirrors
/// `boom_promptlog::ExporterStatusSnapshot` so the dashboard can reuse the
/// same rendering code for logs + traces indicators.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExporterStatusSnapshot {
    pub status: String,
    pub endpoint: String,
    pub last_failure_ts: Option<u64>,
    pub last_recovery_ts: Option<u64>,
    pub consecutive_probe_failures: u64,
    pub total_offline_episodes: u64,
    pub total_dropped_during_offline: u64,
    pub dropped_count: u64,
}

/// Result of a manual probe via `TraceApi::probe_otlp`. Mirrors
/// `boom_promptlog::ProbeResult`.
#[derive(Debug, Clone)]
pub enum ProbeResult {
    Ok { latency_ms: u64 },
    Fail { error: String },
}
