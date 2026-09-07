//! OTLP/HTTP exporter for trace spans.
//!
//! Mirrors `boom_promptlog::otlp::OtelExporter`'s state machine (Online/Offline
//! + probe cycle, result-oriented transitions). The differences:
//!
//! - Drains a queue of `RequestSpan` (not `PromptLogEntry`).
//! - Emits `ExportTraceServiceRequest` to `{endpoint}/v1/traces` (not `/v1/logs`).
//! - Each `RequestSpan` → one OTel `Span` with start_time / end_time / status +
//!   attributes from the span's attribute bag (including `boom-gateway.llm_request`
//!   / `llm_response` bodies, shared via Arc with `PromptLogEntry`).
//!
//! The exporter is `Option`-wrapped in `TraceRegistry` — when `trace.otlp.enabled=false`
//! the registry returns `None` from `otlp_status` / `probe_otlp`, the dashboard
//! surfaces "disabled", and `enqueue` is a no-op.

use crate::span::RequestSpan;
use boom_core::trace::{ExporterStatusSnapshot, ProbeResult};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    any_value::Value as OtlpAnyValue, AnyValue, InstrumentationScope, KeyValue as OtlpKeyValue,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span as OtlpSpan, Status as OtlpStatus};
use prost::Message as ProstMessage;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{interval_at, Instant};

use boom_core::trace::SpanAttributeValue;

const STATUS_ONLINE: u8 = 0;
const STATUS_OFFLINE: u8 = 1;

/// Status code values per OTLP trace spec.
/// 0 = Unset, 1 = Ok, 2 = Error.
const STATUS_CODE_UNSET: i32 = 0;
const STATUS_CODE_OK: i32 = 1;
const STATUS_CODE_ERROR: i32 = 2;

/// OTLP traces exporter. State machine identical to promptlog's
/// `OtelExporter` (Online → push → tick → flush; Offline → skip + probe).
/// Always wrapped in `Arc` and held by `TraceRegistry` behind an
/// `Arc<ArcSwap<Option<Arc<...>>>>` so reloads can hot-swap it.
pub struct TraceExporter {
    client: reqwest::Client,
    endpoint: String,
    extra_headers: Vec<(String, String)>,
    resource: Resource,
    batch: Arc<Mutex<VecDeque<RequestSpan>>>,
    batch_size: usize,
    flush_interval: Duration,
    max_queue_size: usize,
    dropped_count: Arc<AtomicU64>,
    status: AtomicU8,
    consecutive_probe_failures: AtomicU64,
    last_failure_ts: AtomicU64,
    last_recovery_ts: AtomicU64,
    total_offline_episodes: AtomicU64,
    total_dropped_during_offline: AtomicU64,
    flush_permit: Arc<Semaphore>,
}

impl TraceExporter {
    pub fn new(config: &OtlpConfig) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs.max(1)))
            .build()
            .expect("reqwest client build");
        let extra_headers: Vec<(String, String)> = config
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let resource = build_resource(config);
        Arc::new(Self {
            client,
            endpoint: config.endpoint.clone(),
            extra_headers,
            resource,
            batch: Arc::new(Mutex::new(VecDeque::with_capacity(config.batch_size))),
            batch_size: config.batch_size,
            flush_interval: Duration::from_secs(config.flush_interval_secs.max(1)),
            max_queue_size: config.max_queue_size,
            dropped_count: Arc::new(AtomicU64::new(0)),
            status: AtomicU8::new(STATUS_ONLINE),
            consecutive_probe_failures: AtomicU64::new(0),
            last_failure_ts: AtomicU64::new(0),
            last_recovery_ts: AtomicU64::new(0),
            total_offline_episodes: AtomicU64::new(0),
            total_dropped_during_offline: AtomicU64::new(0),
            flush_permit: Arc::new(Semaphore::new(1)),
        })
    }

    pub fn dropped_count_handle(&self) -> Arc<AtomicU64> {
        self.dropped_count.clone()
    }

    #[inline]
    pub fn is_offline(&self) -> bool {
        self.status.load(Ordering::Relaxed) == STATUS_OFFLINE
    }

    pub fn status_snapshot(&self) -> ExporterStatusSnapshot {
        ExporterStatusSnapshot {
            status: if self.is_offline() {
                "offline".to_string()
            } else {
                "online".to_string()
            },
            endpoint: self.endpoint.clone(),
            last_failure_ts: read_atomic_opt(&self.last_failure_ts),
            last_recovery_ts: read_atomic_opt(&self.last_recovery_ts),
            consecutive_probe_failures: self.consecutive_probe_failures.load(Ordering::Relaxed),
            total_offline_episodes: self.total_offline_episodes.load(Ordering::Relaxed),
            total_dropped_during_offline: self.total_dropped_during_offline.load(Ordering::Relaxed),
            dropped_count: self.dropped_count.load(Ordering::Relaxed),
        }
    }

    pub async fn enqueue(self: &Arc<Self>, span: RequestSpan) {
        if self.is_offline() {
            self.dropped_count.fetch_add(1, Ordering::Relaxed);
            self.total_dropped_during_offline
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        let should_flush = {
            let mut batch = self.batch.lock().await;
            if batch.len() >= self.max_queue_size {
                let _dropped = batch.pop_front();
                self.dropped_count.fetch_add(1, Ordering::Relaxed);
            }
            batch.push_back(span);
            batch.len() >= self.batch_size
        };
        if should_flush {
            if let Ok(permit) = self.flush_permit.clone().try_acquire_owned() {
                let me = self.clone();
                tokio::spawn(async move {
                    me.flush().await;
                    drop(permit);
                });
            }
        }
    }

    pub async fn flush(&self) {
        let drained: VecDeque<RequestSpan> = {
            let mut batch = self.batch.lock().await;
            if batch.is_empty() {
                return;
            }
            std::mem::take(&mut *batch)
        };
        match self.send_with_retry(&drained).await {
            Ok(()) => {}
            Err(()) => self.transition_to_offline(drained.len() as u64),
        }
    }

    async fn send_with_retry(&self, drained: &VecDeque<RequestSpan>) -> Result<(), ()> {
        let resource_spans: Vec<ResourceSpans> = drained
            .iter()
            .map(|s| convert_span_to_resource_spans(s, &self.resource))
            .collect();
        let req = ExportTraceServiceRequest { resource_spans };
        let body = req.encode_to_vec();
        let url = format!("{}/v1/traces", self.endpoint.trim_end_matches('/'));

        let mut attempt = 0u32;
        loop {
            let mut req_builder = self
                .client
                .post(&url)
                .header("Content-Type", "application/x-protobuf")
                .body(body.clone());
            for (k, v) in &self.extra_headers {
                req_builder = req_builder.header(k, v);
            }
            match req_builder.send().await {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                Ok(resp) => {
                    let status = resp.status();
                    tracing::warn!(
                        attempt = attempt + 1,
                        status = %status,
                        "OTLP traces push returned non-success"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        error = %e,
                        "OTLP traces push failed"
                    );
                }
            }
            attempt += 1;
            if attempt >= 3 {
                return Err(());
            }
            tokio::time::sleep(Duration::from_millis(100u64 * (1 << attempt))).await;
        }
    }

    pub async fn probe(self: &Arc<Self>) -> ProbeResult {
        match self.probe_internal().await {
            Ok(latency_ms) => {
                self.transition_to_online();
                ProbeResult::Ok { latency_ms }
            }
            Err(e) => {
                self.record_probe_failure();
                ProbeResult::Fail { error: e }
            }
        }
    }

    async fn probe_internal(&self) -> Result<u64, String> {
        let req = ExportTraceServiceRequest { resource_spans: Vec::new() };
        let body = req.encode_to_vec();
        let url = format!("{}/v1/traces", self.endpoint.trim_end_matches('/'));
        let mut req_builder = self
            .client
            .post(&url)
            .header("Content-Type", "application/x-protobuf")
            .body(body);
        for (k, v) in &self.extra_headers {
            req_builder = req_builder.header(k, v);
        }
        let started = std::time::Instant::now();
        match req_builder.send().await {
            Ok(resp) if resp.status().is_success() => Ok(started.elapsed().as_millis() as u64),
            Ok(resp) => Err(format!("HTTP {}", resp.status())),
            Err(e) => Err(format!("{e}")),
        }
    }

    async fn run_probe_cycle(&self) {
        match self.probe_internal().await {
            Ok(latency_ms) => {
                self.transition_to_online();
                tracing::info!(latency_ms, "OTLP traces probe succeeded — back online");
                self.flush().await;
            }
            Err(e) => {
                self.record_probe_failure();
                tracing::debug!(error = %e, "OTLP traces probe still failing");
            }
        }
    }

    fn transition_to_offline(&self, dropped_in_batch: u64) {
        let prev = self.status.swap(STATUS_OFFLINE, Ordering::Relaxed);
        let now = now_epoch_secs();
        self.last_failure_ts.store(now, Ordering::Relaxed);
        if prev == STATUS_ONLINE {
            self.total_offline_episodes.fetch_add(1, Ordering::Relaxed);
        }
        if dropped_in_batch > 0 {
            self.dropped_count
                .fetch_add(dropped_in_batch, Ordering::Relaxed);
        }
        tracing::warn!(
            endpoint = %self.endpoint,
            entries_dropped = dropped_in_batch,
            episodes = self.total_offline_episodes.load(Ordering::Relaxed),
            "OTLP traces offline — entering skip-push mode"
        );
    }

    fn transition_to_online(&self) {
        let prev = self.status.swap(STATUS_ONLINE, Ordering::Relaxed);
        if prev == STATUS_OFFLINE {
            let now = now_epoch_secs();
            self.last_recovery_ts.store(now, Ordering::Relaxed);
            self.consecutive_probe_failures.store(0, Ordering::Relaxed);
            tracing::info!(endpoint = %self.endpoint, "OTLP traces recovered — resuming push");
        }
    }

    fn record_probe_failure(&self) {
        self.consecutive_probe_failures
            .fetch_add(1, Ordering::Relaxed);
        self.last_failure_ts.store(now_epoch_secs(), Ordering::Relaxed);
    }

    pub fn spawn_flush_task(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        let start = Instant::now();
        let mut ticker = interval_at(start, me.flush_interval);
        tokio::spawn(async move {
            loop {
                ticker.tick().await;
                if me.is_offline() {
                    me.run_probe_cycle().await;
                } else {
                    me.flush().await;
                }
            }
        })
    }

    pub fn spawn_flush_task_to_handle(
        self: &Arc<Self>,
        handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    ) {
        let me = self.clone();
        let start = Instant::now();
        let mut ticker = interval_at(start, me.flush_interval);
        let h = tokio::spawn(async move {
            loop {
                ticker.tick().await;
                if me.is_offline() {
                    me.run_probe_cycle().await;
                } else {
                    me.flush().await;
                }
            }
        });
        *handle.lock().unwrap() = Some(h);
    }
}

fn read_atomic_opt(a: &AtomicU64) -> Option<u64> {
    let v = a.load(Ordering::Relaxed);
    if v == 0 { None } else { Some(v) }
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build the OTLP `Resource` (service.name + service.version) attached to
/// every span pushed from this gateway. Same shape as promptlog's.
fn build_resource(config: &OtlpConfig) -> Resource {
    let service_version = config
        .service_version
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    Resource {
        attributes: vec![
            otlp_kv_string("service.name", &config.service_name),
            otlp_kv_string("service.version", &service_version),
        ],
        ..Default::default()
    }
}

fn otlp_kv_string(key: &str, value: &str) -> OtlpKeyValue {
    OtlpKeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(OtlpAnyValue::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

fn otlp_kv_int(key: &str, value: i64) -> OtlpKeyValue {
    OtlpKeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(OtlpAnyValue::IntValue(value)),
        }),
        ..Default::default()
    }
}

fn otlp_kv_bool(key: &str, value: bool) -> OtlpKeyValue {
    OtlpKeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(OtlpAnyValue::BoolValue(value)),
        }),
        ..Default::default()
    }
}

/// Convert a finalized `RequestSpan` into an OTLP `ResourceSpans` envelope.
/// Each gateway span is its own `ScopeSpans` so the InstrumentationScope
/// fields carry the gateway's name + version.
pub fn convert_span_to_resource_spans(span: &RequestSpan, resource: &Resource) -> ResourceSpans {
    let mut otlp_attrs: Vec<OtlpKeyValue> = Vec::with_capacity(span.attributes.len() + 6);
    // Standard semantic-convention fields.
    otlp_attrs.push(otlp_kv_string("url.path", &span.api_path));
    otlp_attrs.push(otlp_kv_string("http.request.method", "POST"));
    otlp_attrs.push(otlp_kv_string("boom-gateway.request.id", &span.request_id));
    otlp_attrs.push(otlp_kv_string("boom-gateway.model", &span.model));
    otlp_attrs.push(otlp_kv_bool("boom-gateway.is_stream", span.is_stream));
    if let Some(did) = &span.deployment_id {
        otlp_attrs.push(otlp_kv_string("boom-gateway.deployment_id", did));
    }
    if !span.trace_state.is_empty() {
        otlp_attrs.push(otlp_kv_string("boom-gateway.trace_state", &span.trace_state));
    }

    // Custom attributes from the span's attribute bag. Body attributes are
    // serialized as JSON string (the Arc<serde_json::Value> derefs). Skip
    // keys already added above (request.id / model / is_stream etc.).
    for (k, v) in &span.attributes {
        match v {
            SpanAttributeValue::String(s) => otlp_attrs.push(otlp_kv_string(k, s)),
            SpanAttributeValue::Int(n) => otlp_attrs.push(otlp_kv_int(k, *n)),
            SpanAttributeValue::Bool(b) => otlp_attrs.push(otlp_kv_bool(k, *b)),
            SpanAttributeValue::Json(v) => {
                let s = serde_json::to_string(v).unwrap_or_default();
                otlp_attrs.push(otlp_kv_string(k, &s));
            }
        }
    }

    let status_code = match span.status {
        boom_core::trace::SpanStatus::Unset => STATUS_CODE_UNSET,
        boom_core::trace::SpanStatus::Ok => STATUS_CODE_OK,
        boom_core::trace::SpanStatus::Error => STATUS_CODE_ERROR,
    };
    let status_msg = span.error_message.clone().unwrap_or_default();

    let otlp_span = OtlpSpan {
        trace_id: span.trace_id.to_vec(),
        span_id: span.span_id.to_vec(),
        trace_state: span.trace_state.clone(),
        parent_span_id: if span.parent_span_id.iter().any(|b| *b != 0) {
            span.parent_span_id.to_vec()
        } else {
            Vec::new()
        },
        flags: 1, // sampled
        name: "boom-gateway.request".to_string(),
        kind: 2, // SERVER
        start_time_unix_nano: span.start_time_unix_nano,
        end_time_unix_nano: span.end_time_unix_nano,
        attributes: otlp_attrs,
        dropped_attributes_count: 0,
        events: Vec::new(),
        dropped_events_count: 0,
        links: Vec::new(),
        dropped_links_count: 0,
        status: Some(OtlpStatus {
            code: status_code,
            message: status_msg,
            ..Default::default()
        }),
    };

    ResourceSpans {
        resource: Some(resource.clone()),
        scope_spans: vec![ScopeSpans {
            scope: Some(InstrumentationScope {
                name: "boom-trace".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            }),
            spans: vec![otlp_span],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Standalone probe for a one-shot endpoint check (used by the dashboard's
/// "Test" button on a not-yet-saved endpoint). Same shape as promptlog's
/// `ping_endpoint`.
pub async fn ping_endpoint(config: &OtlpConfig) -> Result<u64, String> {
    if config.endpoint.trim().is_empty() {
        return Err("endpoint not configured".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs.max(1)))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let req = ExportTraceServiceRequest { resource_spans: Vec::new() };
    let body = req.encode_to_vec();
    let url = format!("{}/v1/traces", config.endpoint.trim_end_matches('/'));
    let mut req_builder = client
        .post(&url)
        .header("Content-Type", "application/x-protobuf")
        .body(body);
    for (k, v) in &config.headers {
        req_builder = req_builder.header(k, v);
    }
    let started = std::time::Instant::now();
    match req_builder.send().await {
        Ok(resp) if resp.status().is_success() => Ok(started.elapsed().as_millis() as u64),
        Ok(resp) => Err(format!("HTTP {}", resp.status())),
        Err(e) => Err(format!("{e}")),
    }
}

// Re-export boom-core's OtlpConfig so boom-trace consumers don't need to
// import boom-core directly for the config type. Shared between promptlog
// (logs) and boom-trace (traces) — both channels read from the same shape.
pub use boom_core::OtlpConfig;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::parse_traceparent;
    use crate::span::RequestSpan;

    fn make_span(req_id: &str) -> RequestSpan {
        let w3c = parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01",
        )
        .unwrap();
        let mut span = RequestSpan::new(
            req_id.to_string(),
            &w3c,
            "gpt-4".to_string(),
            "/v1/chat/completions".to_string(),
            false,
            1_000_000_000,
        );
        span.finalize_ok(2_000_000_000);
        span
    }

    #[test]
    fn convert_span_emits_correct_trace_and_span_ids() {
        let span = make_span("r-1");
        let cfg = OtlpConfig::default();
        let resource = build_resource(&cfg);
        let rs = convert_span_to_resource_spans(&span, &resource);
        let scope = &rs.scope_spans[0];
        let otlp_span = &scope.spans[0];
        assert_eq!(otlp_span.trace_id.len(), 16);
        assert_eq!(otlp_span.trace_id[0], 0x0a);
        assert_eq!(otlp_span.span_id.len(), 8);
        assert!(!otlp_span.parent_span_id.is_empty());
        assert_eq!(otlp_span.start_time_unix_nano, 1_000_000_000);
        assert_eq!(otlp_span.end_time_unix_nano, 2_000_000_000);
        assert_eq!(otlp_span.status.as_ref().unwrap().code, STATUS_CODE_OK);
    }

    #[test]
    fn error_span_carries_status_message() {
        let w3c = parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01",
        )
        .unwrap();
        let mut span = RequestSpan::new(
            "r-2".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            1_000,
        );
        span.finalize_error(2_000, "boom".to_string());
        let cfg = OtlpConfig::default();
        let resource = build_resource(&cfg);
        let rs = convert_span_to_resource_spans(&span, &resource);
        let otlp_span = &rs.scope_spans[0].spans[0];
        assert_eq!(otlp_span.status.as_ref().unwrap().code, STATUS_CODE_ERROR);
        assert_eq!(otlp_span.status.as_ref().unwrap().message, "boom");
    }

    #[test]
    fn body_attribute_serialized_as_json_string() {
        let w3c = parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01",
        )
        .unwrap();
        let mut span = RequestSpan::new(
            "r-3".to_string(),
            &w3c,
            "m".to_string(),
            "/p".to_string(),
            false,
            0,
        );
        span.set_llm_request(Arc::new(serde_json::json!({"messages": []})));
        span.finalize_ok(1);
        let cfg = OtlpConfig::default();
        let resource = build_resource(&cfg);
        let rs = convert_span_to_resource_spans(&span, &resource);
        let otlp_span = &rs.scope_spans[0].spans[0];
        let req_attr = otlp_span
            .attributes
            .iter()
            .find(|a| a.key == "boom-gateway.llm_request")
            .expect("llm_request attribute present");
        let OtlpAnyValue::StringValue(s) =
            req_attr.value.as_ref().unwrap().value.as_ref().unwrap()
        else {
            panic!("body attribute should be string");
        };
        assert!(s.contains("messages"));
    }

    #[test]
    fn new_exporter_starts_online() {
        let cfg = OtlpConfig::default();
        let exp = TraceExporter::new(&cfg);
        assert!(!exp.is_offline());
        let snap = exp.status_snapshot();
        assert_eq!(snap.status, "online");
    }

    #[tokio::test]
    async fn enqueue_while_offline_drops_immediately() {
        let cfg = OtlpConfig::default();
        let exp = TraceExporter::new(&cfg);
        // Manually push Offline.
        exp.transition_to_offline(0);
        for i in 0..3 {
            exp.enqueue(make_span(&format!("r-{i}"))).await;
        }
        let batch_len = exp.batch.lock().await.len();
        assert_eq!(batch_len, 0);
        let snap = exp.status_snapshot();
        assert_eq!(snap.total_dropped_during_offline, 3);
    }
}
