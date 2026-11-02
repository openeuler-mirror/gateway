//! Prometheus pull channel — issue #49 semantic-convention metrics,
//! self-aggregated in-process and served from `GET /metrics`.
//!
//! ## Model
//!
//! Pull, not push: no exporter pipeline, no background flush task, no OTel
//! SDK. `TraceRegistry::finalize_*` is the single funnel — it reads the
//! structured timing fields off the finalized `RequestSpan` and records
//! every duration-family metric in one `record_request_done` call into
//! sharded `DashMap` histograms. A scrape (`MetricsRecorder::render`) walks
//! those maps on demand and encodes Prometheus text format 0.0.4. The only
//! work on the request path is the O(1) map update.
//!
//! boom-main only writes data into the span via `TraceGuard` setters — it
//! never touches the aggregator directly; the `/metrics` handler calls
//! `TraceRegistry::prometheus_text`.
//!
//! ## Metric names (issue #49, not litellm's namespace)
//!
//! OTel name → Prometheus exposition name (dots→underscores, unit "s" →
//! `_seconds` suffix):
//!
//! - `http.server.request.duration` → `http_server_request_duration_seconds`
//!   / `http.server.active_requests` → `http_server_active_requests` (gauge)
//! - `ai.gateway.queue.duration` / `ai.gateway.routing.duration` →
//!   `ai_gateway_{queue,routing}_duration_seconds`
//! - `gen_ai.client.operation.time_to_first_chunk` / `.time_per_output_chunk`
//!   → `gen_ai_client_operation_time_{to_first_chunk,per_output_chunk}_seconds`
//! - `http.client.request.duration` → `http_client_request_duration_seconds`
//!   / `http.client.active_requests` → `http_client_active_requests` (gauge)
//!
//! `http.client.open_connections` is NOT reported: reqwest does not expose
//! connection-pool statistics and wrapping the hyper connector is out of
//! scope this round.
//!
//! ## Cardinality
//!
//! Labels are restricted to a bounded set: `gen_ai_operation_name`
//! (chat|messages|responses), `gen_ai_request_model`,
//! `ai_gateway_target_deployment_id`, and `error_type` (truncated, error
//! series only). Never `user.id` / `request_id` / client IP.

use crate::span::RequestSpan;
use dashmap::DashMap;
use std::sync::Arc;

/// Cap for `error.type` label values — keeps the label space bounded even
/// when upstreams echo back full response bodies in error messages.
/// Shared with the traces channel (span attribute dual-write).
pub(crate) const ERROR_TYPE_MAX_CHARS: usize = 32;

/// Histogram bucket boundaries (seconds) for LLM-scale durations: server /
/// client duration, TTFT, time-per-output-chunk. The OTel SDK default
/// ([0, 5, 10, …, 10000]) is millisecond intuition — every LLM request
/// would land in the first bucket; that mismatch is part of why this
/// channel aggregates on its own instead of via the SDK.
const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// Millisecond-scale boundaries for queue wait and routing selection —
/// both are firmly sub-second.
const FAST_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
];

/// Bounded label-set key every series is aggregated under. Absent
/// dimensions are `None` (label omitted at render — sparse series, the same
/// semantics OTel attributes have).
#[derive(Hash, Eq, PartialEq, Clone)]
struct AttrKey {
    operation: String,
    model: String,
    deployment_id: Option<String>,
    error_type: Option<String>,
}

/// Per-series histogram state. `counts` holds per-bucket deltas (len ==
/// buckets.len() + 1, last slot is +Inf); the Prometheus `le` cumulative
/// counts are computed at render time by prefix-summing.
#[derive(Clone, Default)]
struct Histo {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histo {
    fn new(buckets_len: usize) -> Self {
        Self {
            counts: vec![0; buckets_len + 1],
            sum: 0.0,
            count: 0,
        }
    }

    /// Explicit-boundary buckets with inclusive upper bound (Prometheus
    /// `le` semantics): a value exactly on a boundary lands in that
    /// boundary's bucket.
    fn observe(&mut self, buckets: &[f64], v: f64) {
        let idx = buckets.partition_point(|&b| v > b);
        self.counts[idx] += 1;
        self.sum += v;
        self.count += 1;
    }
}

pub struct MetricsRecorder {
    server_duration: DashMap<AttrKey, Histo>,
    queue_duration: DashMap<AttrKey, Histo>,
    routing_duration: DashMap<AttrKey, Histo>,
    ttft: DashMap<AttrKey, Histo>,
    time_per_output_chunk: DashMap<AttrKey, Histo>,
    client_duration: DashMap<AttrKey, Histo>,
    server_active: DashMap<AttrKey, i64>,
    client_active: DashMap<AttrKey, i64>,
}

impl MetricsRecorder {
    /// Self-aggregating recorder — no exporter pipeline, no failure mode.
    /// Presence in the registry's `ArcSwap<Option<Arc<MetricsRecorder>>>`
    /// IS the enabled flag (see `set_metrics_enabled`).
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            server_duration: DashMap::new(),
            queue_duration: DashMap::new(),
            routing_duration: DashMap::new(),
            ttft: DashMap::new(),
            time_per_output_chunk: DashMap::new(),
            client_duration: DashMap::new(),
            server_active: DashMap::new(),
            client_active: DashMap::new(),
        })
    }

    /// Record every duration-family metric for a finalized span. One call,
    /// from `TraceRegistry::finalize_*` — the single funnel.
    pub fn record_request_done(&self, span: &RequestSpan) {
        let errored = matches!(span.status, boom_core::trace::SpanStatus::Error);
        let key = AttrKey {
            operation: operation_name(&span.api_path),
            model: span.model.clone(),
            deployment_id: span.deployment_id.clone(),
            error_type: errored
                .then(|| span.error_message.as_ref())
                .flatten()
                .map(|m| truncate_bounded(m, ERROR_TYPE_MAX_CHARS)),
        };

        // Server duration from span start/end: guard Drop (or LoggedStream
        // Drop on streaming) is the response-completion moment.
        if span.end_time_unix_nano > span.start_time_unix_nano {
            let secs = (span.end_time_unix_nano - span.start_time_unix_nano) as f64 / 1e9;
            observe(&self.server_duration, DURATION_BUCKETS, key.clone(), secs);
        }
        if let Some(qw) = span.queue_wait_ms {
            observe(&self.queue_duration, FAST_BUCKETS, key.clone(), qw as f64 / 1e3);
        }
        if let Some(rd) = span.routing_duration_ms {
            observe(&self.routing_duration, FAST_BUCKETS, key.clone(), rd as f64 / 1e3);
        }
        if let Some(ttft) = span.ttft_ms {
            observe(&self.ttft, DURATION_BUCKETS, key.clone(), ttft as f64 / 1e3);
        }
        if let (Some(count), Some(gen_ms)) = (span.chunk_count, span.gen_duration_ms) {
            if count > 0 && gen_ms > 0 {
                observe(
                    &self.time_per_output_chunk,
                    DURATION_BUCKETS,
                    key.clone(),
                    gen_ms as f64 / count as f64 / 1e3,
                );
            }
        }
        match (span.upstream_start_ns, span.upstream_end_ns) {
            (Some(s), Some(e)) if e > s => {
                observe(
                    &self.client_duration,
                    DURATION_BUCKETS,
                    key.clone(),
                    (e - s) as f64 / 1e9,
                );
            }
            // Dispatched but never completed (upstream error path returned
            // before note_upstream_end) — reconcile the in-flight gauge.
            (Some(_), None) => self.add_client_active(span, -1),
            _ => {}
        }
    }

    /// `http.server.active_requests` delta. Called with +1 at span start,
    /// −1 at finalize — the RAII pairing of `TraceGuard` makes this exact.
    /// Labels are only op + model (deployment is not yet chosen at start).
    pub fn add_server_active(&self, delta: i64, model: &str, api_path: &str) {
        let key = AttrKey {
            operation: operation_name(api_path),
            model: model.to_string(),
            deployment_id: None,
            error_type: None,
        };
        *self.server_active.entry(key).or_insert(0) += delta;
    }

    /// `http.client.active_requests` delta for the upstream call owned by
    /// this span (+1 at dispatch, −1 at completion or via finalize
    /// reconcile).
    pub fn add_client_active(&self, span: &RequestSpan, delta: i64) {
        let key = AttrKey {
            operation: operation_name(&span.api_path),
            model: span.model.clone(),
            deployment_id: span.deployment_id.clone(),
            error_type: None,
        };
        *self.client_active.entry(key).or_insert(0) += delta;
    }

    /// Render the full exposition in Prometheus text format 0.0.4.
    /// Deterministic: fixed family order, series sorted by canonical label
    /// order. Families with no series are omitted — an enabled-but-idle
    /// recorder renders an empty string. Runs only on the scrape path,
    /// never on the request path.
    pub fn render(&self) -> String {
        let mut out = String::new();
        render_histogram(
            &mut out,
            "http_server_request_duration_seconds",
            "Gateway server-side request duration (ingress to response completion).",
            DURATION_BUCKETS,
            &self.server_duration,
        );
        render_gauge(
            &mut out,
            "http_server_active_requests",
            "In-flight gateway requests.",
            &self.server_active,
        );
        render_histogram(
            &mut out,
            "ai_gateway_queue_duration_seconds",
            "Time spent waiting in per-deployment flow control before dispatch.",
            FAST_BUCKETS,
            &self.queue_duration,
        );
        render_histogram(
            &mut out,
            "ai_gateway_routing_duration_seconds",
            "Time spent selecting the target deployment (kvc-aware or fallback routing).",
            FAST_BUCKETS,
            &self.routing_duration,
        );
        render_histogram(
            &mut out,
            "gen_ai_client_operation_time_to_first_chunk_seconds",
            "Time from request start to the first response chunk. Non-stream requests fall back to total duration.",
            DURATION_BUCKETS,
            &self.ttft,
        );
        render_histogram(
            &mut out,
            "gen_ai_client_operation_time_per_output_chunk_seconds",
            "Mean inter-chunk generation time: (last chunk − first chunk) / chunk count.",
            DURATION_BUCKETS,
            &self.time_per_output_chunk,
        );
        render_histogram(
            &mut out,
            "http_client_request_duration_seconds",
            "Upstream provider call duration (request dispatch to last byte received).",
            DURATION_BUCKETS,
            &self.client_duration,
        );
        render_gauge(
            &mut out,
            "http_client_active_requests",
            "In-flight upstream provider calls.",
            &self.client_active,
        );
        out
    }
}

fn observe(map: &DashMap<AttrKey, Histo>, buckets: &'static [f64], key: AttrKey, value: f64) {
    let mut entry = map.entry(key).or_insert_with(|| Histo::new(buckets.len()));
    entry.observe(buckets, value);
}

/// Deterministic series ordering: canonical dimension order, `None` sorts
/// before `Some`. Stabilizes render output for tests and diffing.
fn label_set_order(a: &AttrKey, b: &AttrKey) -> std::cmp::Ordering {
    a.operation
        .cmp(&b.operation)
        .then_with(|| a.model.cmp(&b.model))
        .then_with(|| a.deployment_id.cmp(&b.deployment_id))
        .then_with(|| a.error_type.cmp(&b.error_type))
}

impl AttrKey {
    /// Canonical Prometheus label set: fixed dimension order, absent
    /// dimensions omitted.
    fn prom_label_string(&self) -> String {
        let mut s = format!(
            "gen_ai_operation_name=\"{}\",gen_ai_request_model=\"{}\"",
            escape_label_value(&self.operation),
            escape_label_value(&self.model)
        );
        if let Some(d) = &self.deployment_id {
            s.push_str(&format!(
                ",ai_gateway_target_deployment_id=\"{}\"",
                escape_label_value(d)
            ));
        }
        if let Some(e) = &self.error_type {
            s.push_str(&format!(",error_type=\"{}\"", escape_label_value(e)));
        }
        s
    }
}

fn render_histogram(
    out: &mut String,
    name: &str,
    help: &str,
    buckets: &'static [f64],
    map: &DashMap<AttrKey, Histo>,
) {
    if map.is_empty() {
        return;
    }
    let mut series: Vec<(AttrKey, Histo)> = map
        .iter()
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    series.sort_by(|a, b| label_set_order(&a.0, &b.0));
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} histogram\n"));
    for (key, h) in &series {
        let labels = key.prom_label_string();
        // Per-bucket deltas → cumulative `le` counts via prefix sum.
        let mut cumulative: u64 = 0;
        for (i, b) in buckets.iter().enumerate() {
            cumulative += h.counts[i];
            out.push_str(&format!("{name}_bucket{{{labels},le=\"{b}\"}} {cumulative}\n"));
        }
        cumulative += h.counts[buckets.len()];
        out.push_str(&format!("{name}_bucket{{{labels},le=\"+Inf\"}} {cumulative}\n"));
        out.push_str(&format!("{name}_sum{{{labels}}} {}\n", h.sum));
        out.push_str(&format!("{name}_count{{{labels}}} {}\n", h.count));
    }
}

fn render_gauge(out: &mut String, name: &str, help: &str, map: &DashMap<AttrKey, i64>) {
    if map.is_empty() {
        return;
    }
    let mut series: Vec<(AttrKey, i64)> = map
        .iter()
        .map(|e| (e.key().clone(), *e.value()))
        .collect();
    series.sort_by(|a, b| label_set_order(&a.0, &b.0));
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} gauge\n"));
    for (key, v) in &series {
        out.push_str(&format!("{name}{{{}}} {v}\n", key.prom_label_string()));
    }
}

/// Prometheus text-format label value escaping: backslash, newline, double
/// quote. Everything else passes through verbatim.
fn escape_label_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// Map the gateway API path onto the bounded `gen_ai.operation.name` value
/// set. Unknown paths fall back to the raw path (still a small bounded set
/// of configured routes). Shared with the traces channel's dual-write.
pub(crate) fn operation_name(api_path: &str) -> String {
    if api_path.starts_with("/v1/responses") {
        "responses".to_string()
    } else if api_path == "/v1/messages" {
        "messages".to_string()
    } else {
        // /v1/chat/completions, /v1/completions (legacy), anything else.
        "chat".to_string()
    }
}

pub(crate) fn truncate_bounded(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

/// Stable per-process instance identity. HOSTNAME is set in most container
/// runtimes; /etc/hostname covers bare Linux; the pid suffix guarantees
/// uniqueness even when both are absent. Shared with the traces channel's
/// Resource builder.
pub(crate) fn service_instance_id() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());
    format!("{host}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::parse_traceparent;
    use crate::span::RequestSpan;

    #[test]
    fn operation_name_maps_paths_onto_bounded_set() {
        assert_eq!(operation_name("/v1/chat/completions"), "chat");
        assert_eq!(operation_name("/v1/completions"), "chat");
        assert_eq!(operation_name("/v1/messages"), "messages");
        assert_eq!(operation_name("/v1/responses"), "responses");
    }

    #[test]
    fn truncate_bounded_limits_by_chars() {
        assert_eq!(truncate_bounded("short", 32), "short");
        let long = "x".repeat(64);
        assert_eq!(truncate_bounded(&long, 32).chars().count(), 32);
    }

    #[test]
    fn service_instance_id_is_stable_within_process() {
        assert_eq!(service_instance_id(), service_instance_id());
        assert!(service_instance_id().contains('-'));
    }

    #[test]
    fn label_values_escape_prometheus_specials() {
        assert_eq!(escape_label_value("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        // Through the full pipeline: an upstream error message is the one
        // label value that realistically contains specials.
        let recorder = MetricsRecorder::new();
        let mut span = make_span("/v1/chat/completions", false);
        span.finalize_error(2_000_000_000, "quote \" backslash \\ newline \n".to_string());
        recorder.record_request_done(&span);
        let text = recorder.render();
        assert!(text.contains("error_type=\"quote \\\" backslash \\\\ newline \\n\""));
    }

    // ---- Recorder-level render assertions ----

    fn make_w3c() -> crate::context::W3cContext {
        parse_traceparent("00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01").unwrap()
    }

    fn make_span(api_path: &str, is_stream: bool) -> RequestSpan {
        RequestSpan::new(
            "r-metrics".to_string(),
            &make_w3c(),
            "gpt-4".to_string(),
            api_path.to_string(),
            is_stream,
            1_000_000_000,
            true,
        )
    }

    #[test]
    fn record_request_done_records_all_duration_metrics() {
        let recorder = MetricsRecorder::new();
        let mut span = make_span("/v1/chat/completions", true);
        span.set_deployment_id("dep-1".to_string());
        span.set_queue_wait_ms(100);
        span.set_routing_duration_ms(5);
        span.set_ttft_ms(250);
        span.set_chunk_stats(10, 900);
        span.note_upstream_start(1_500_000_000);
        span.note_upstream_end(2_000_000_000);
        span.finalize_ok(2_000_000_000); // 1s server duration
        recorder.record_request_done(&span);

        let text = recorder.render();
        let labels = "gen_ai_operation_name=\"chat\",gen_ai_request_model=\"gpt-4\",ai_gateway_target_deployment_id=\"dep-1\"";
        assert!(text.contains("# TYPE http_server_request_duration_seconds histogram\n"));
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_count{{{labels}}} 1\n"
        )));
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_sum{{{labels}}} 1\n"
        )));
        assert!(text.contains(&format!(
            "ai_gateway_queue_duration_seconds_sum{{{labels}}} 0.1\n"
        )));
        assert!(text.contains(&format!(
            "ai_gateway_routing_duration_seconds_sum{{{labels}}} 0.005\n"
        )));
        assert!(text.contains(&format!(
            "gen_ai_client_operation_time_to_first_chunk_seconds_sum{{{labels}}} 0.25\n"
        )));
        assert!(text.contains(&format!(
            "gen_ai_client_operation_time_per_output_chunk_seconds_sum{{{labels}}} 0.09\n"
        )));
        assert!(text.contains(&format!(
            "http_client_request_duration_seconds_sum{{{labels}}} 0.5\n"
        )));
    }

    #[test]
    fn histogram_le_lines_are_cumulative() {
        let recorder = MetricsRecorder::new();
        // 4ms and 6ms server durations: le=0.005 → 1, le=0.01 → 2.
        for end in [1_004_000_000u64, 1_006_000_000] {
            let mut span = make_span("/v1/chat/completions", false);
            span.finalize_ok(end);
            recorder.record_request_done(&span);
        }
        let text = recorder.render();
        let labels = "gen_ai_operation_name=\"chat\",gen_ai_request_model=\"gpt-4\"";
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_bucket{{{labels},le=\"0.005\"}} 1\n"
        )));
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_bucket{{{labels},le=\"0.01\"}} 2\n"
        )));
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} 2\n"
        )));
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_count{{{labels}}} 2\n"
        )));
    }

    #[test]
    fn value_exactly_on_boundary_lands_in_that_bucket() {
        let recorder = MetricsRecorder::new();
        let mut span = make_span("/v1/chat/completions", false);
        span.finalize_ok(1_005_000_000); // exactly 5ms == the le="0.005" boundary
        recorder.record_request_done(&span);
        let text = recorder.render();
        let labels = "gen_ai_operation_name=\"chat\",gen_ai_request_model=\"gpt-4\"";
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_bucket{{{labels},le=\"0.005\"}} 1\n"
        )));
        assert!(text.contains(&format!(
            "http_server_request_duration_seconds_bucket{{{labels},le=\"0.01\"}} 1\n"
        )));
    }

    #[test]
    fn error_span_labels_carry_bounded_error_type() {
        let recorder = MetricsRecorder::new();
        let mut span = make_span("/v1/messages", false);
        span.finalize_error(2_000_000_000, "x".repeat(64));
        recorder.record_request_done(&span);
        let text = recorder.render();
        assert!(text.contains(&format!("error_type=\"{}\"", "x".repeat(32))));
        assert!(!text.contains(&format!("error_type=\"{}\"", "x".repeat(33))));
        assert!(text.contains("gen_ai_operation_name=\"messages\""));
    }

    #[test]
    fn upstream_error_path_reconciles_client_active() {
        let recorder = MetricsRecorder::new();
        let mut span = make_span("/v1/chat/completions", false);
        // Dispatch happened (+1 via note_upstream_started semantics) but the
        // error path returned before note_upstream_end.
        span.note_upstream_start(1_500_000_000);
        recorder.add_client_active(&span, 1);
        span.finalize_error(2_000_000_000, "upstream 500".to_string());
        recorder.record_request_done(&span);
        let text = recorder.render();
        let labels = "gen_ai_operation_name=\"chat\",gen_ai_request_model=\"gpt-4\"";
        assert!(text.contains(&format!(
            "http_client_active_requests{{{labels}}} 0\n"
        )));
    }

    #[test]
    fn server_active_pairs_to_zero() {
        let recorder = MetricsRecorder::new();
        recorder.add_server_active(1, "gpt-4", "/v1/chat/completions");
        recorder.add_server_active(-1, "gpt-4", "/v1/chat/completions");
        let text = recorder.render();
        assert!(text.contains("# TYPE http_server_active_requests gauge\n"));
        assert!(text.contains(
            "http_server_active_requests{gen_ai_operation_name=\"chat\",gen_ai_request_model=\"gpt-4\"} 0\n"
        ));
    }

    #[test]
    fn enabled_but_idle_renders_empty_body() {
        let recorder = MetricsRecorder::new();
        assert_eq!(recorder.render(), "");
    }

    #[test]
    fn render_golden_text_for_one_nonstream_span() {
        let recorder = MetricsRecorder::new();
        let mut span = make_span("/v1/chat/completions", false);
        span.finalize_ok(2_000_000_000); // 1s
        recorder.record_request_done(&span);
        let l = "gen_ai_operation_name=\"chat\",gen_ai_request_model=\"gpt-4\"";
        let expected = format!(
            concat!(
                "# HELP http_server_request_duration_seconds Gateway server-side request duration (ingress to response completion).\n",
                "# TYPE http_server_request_duration_seconds histogram\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"0.005\"}} 0\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"0.01\"}} 0\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"0.025\"}} 0\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"0.05\"}} 0\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"0.1\"}} 0\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"0.25\"}} 0\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"0.5\"}} 0\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"1\"}} 1\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"2.5\"}} 1\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"5\"}} 1\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"10\"}} 1\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"30\"}} 1\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"60\"}} 1\n",
                "http_server_request_duration_seconds_bucket{{{l},le=\"+Inf\"}} 1\n",
                "http_server_request_duration_seconds_sum{{{l}}} 1\n",
                "http_server_request_duration_seconds_count{{{l}}} 1\n",
            ),
            l = l,
        );
        assert_eq!(recorder.render(), expected);
    }

    #[test]
    fn nonstream_fallback_fills_stream_shaped_fields() {
        let mut span = make_span("/v1/chat/completions", false);
        span.finalize_ok(3_000_000_000); // 2s total
        span.apply_nonstream_fallbacks();
        assert_eq!(span.ttft_ms, Some(2_000));
        assert_eq!(span.chunk_count, Some(1));
        assert_eq!(span.gen_duration_ms, Some(2_000));
    }
}
