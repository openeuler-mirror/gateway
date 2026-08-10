//! OTLP/HTTP exporter for prompt log entries.
//!
//! The exporter has one job: take `PromptLogEntry`s, batch them up, and POST
//! them as `ExportLogsServiceRequest` protobuf to `{endpoint}/v1/logs`. Two
//! design rules:
//!
//! 1. **`convert_entry_to_log_records` is a pure function** — no client, no
//!    state. The offline replayer (see `replay.rs`) reuses it so the runtime
//!    and replay paths produce byte-identical LogRecords from the same entry.
//!    "What you stored locally is what gets pushed" is the contract.
//! 2. **Never block the gateway.** The exporter owns a bounded Vec; overflow
//!    drops oldest with a `tracing::warn!`. HTTP failures retry a couple
//!    times with exponential backoff and then drop the batch with a warn.
//!    Local JSONL is the source of truth; OTLP is best-effort.

use crate::config::OtlpConfig;
#[cfg(test)]
use crate::entry::error_code;
use crate::entry::{LogPhase, PromptLogEntry};
use chrono::DateTime;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    any_value::Value as OtlpAnyValue, AnyValue, InstrumentationScope, KeyValue as OtlpKeyValue,
    KeyValueList,
};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{interval_at, Instant};

/// Severity numbers (OTel spec): INFO=9, WARN=13, ERROR=17. The producer maps
/// HTTP status → severity; missing status defaults to INFO.
fn severity_for(status: Option<i32>, phase: LogPhase) -> i32 {
    match phase {
        LogPhase::Request => 9, // INFO — ingress is always informational
        LogPhase::Response => match status {
            Some(s) if (200..300).contains(&s) => 9,  // INFO
            Some(s) if (400..500).contains(&s) => 13, // WARN
            Some(_) => 17,                              // ERROR (5xx)
            None => 9, // unknown status — assume OK
        },
    }
}

fn severity_text(status: Option<i32>, phase: LogPhase) -> &'static str {
    match severity_for(status, phase) {
        s if s <= 9 => "INFO",
        s if s <= 13 => "WARN",
        _ => "ERROR",
    }
}

/// Map a W3C traceparent header value to a 16-byte (32 hex) trace id.
/// Returns `None` if the value isn't a parseable traceparent. We extract the
/// 32-hex segment between the version and span id, parse to bytes, return.
pub fn parse_traceparent(value: &str) -> Option<[u8; 16]> {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 4 || parts[0].len() != 2 || parts[1].len() != 32 || parts[2].len() != 16 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte_str) in parts[1].as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(byte_str).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

/// Hash a request_id into a deterministic 16-byte trace id when no upstream
/// traceparent was supplied. SHA-256 then take the first 16 bytes.
fn synthetic_trace_id(request_id: &str) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(request_id.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Hash a request_id + phase into a deterministic 8-byte span id. The phase
/// discriminates request vs response spans so they're not identical.
fn synthetic_span_id(request_id: &str, phase: LogPhase) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(request_id.as_bytes());
    match phase {
        LogPhase::Request => hasher.update(b"request"),
        LogPhase::Response => hasher.update(b"response"),
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

/// Truncate a value to fit within `max_bytes` (UTF-8 boundary aware). Returns
/// the (possibly-truncated) string. Caller increments a dropped-bytes
/// counter when truncation occurred.
fn truncate_to_bytes(s: String, max_bytes: usize) -> (String, usize) {
    if s.len() <= max_bytes {
        return (s, 0);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), s.len() - end)
}

/// Build the OTLP `Resource` for all records from this gateway.
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

fn otlp_kv_kvlist(key: &str, pairs: Vec<OtlpKeyValue>) -> OtlpKeyValue {
    OtlpKeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(OtlpAnyValue::KvlistValue(KeyValueList { values: pairs })),
        }),
        ..Default::default()
    }
}

/// Convert one prompt log entry into a single OTLP LogRecord wrapped in the
/// outer ResourceLogs/ScopeLogs envelopes so the caller just packs many of
/// them into an `ExportLogsServiceRequest`.
///
/// Pure: no I/O, no time-dependent side effects (timestamps come from the
/// entry's RFC3339 field). Both the runtime exporter and the offline
/// replayer call this, so their wire output is identical.
#[allow(clippy::too_many_lines)]
pub fn convert_entry_to_log_records(
    entry: &PromptLogEntry,
    resource: &Resource,
    max_attribute_bytes: usize,
) -> ResourceLogs {
    let trace_id = entry
        .trace_id
        .as_deref()
        .and_then(parse_traceparent)
        .unwrap_or_else(|| synthetic_trace_id(&entry.request_id));
    let span_id = synthetic_span_id(&entry.request_id, entry.phase);

    // Body — one-line summary. Easy to skim in a log viewer.
    let body_summary = match entry.phase {
        LogPhase::Request => format!(
            "REQ {} {} model={}",
            entry.api_path, entry.request_id, entry.model
        ),
        LogPhase::Response => {
            let sc = entry
                .status_code
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".into());
            format!(
                "RESP {} {} model={} ({}ms, {})",
                entry.api_path,
                entry.request_id,
                entry.model,
                entry.duration_ms.unwrap_or(0),
                sc,
            )
        }
    };

    // Convert timestamp RFC3339 → unix nanos.
    let time_unix_nano = entry
        .timestamp
        .parse::<DateTime<chrono::Utc>>()
        .ok()
        .and_then(|dt| dt.timestamp_nanos_opt())
        .map(|n| n as u64)
        .unwrap_or(0);

    // Build attributes. Some are large (request/response/raw_upstream);
    // truncate them and count drops.
    let mut attrs: Vec<OtlpKeyValue> = Vec::new();
    let mut dropped_attributes_count: u32 = 0;

    // Standard semantic-convention HTTP fields.
    attrs.push(otlp_kv_string("url.path", &entry.api_path));
    attrs.push(otlp_kv_string("http.request.method", "POST"));
    if let Some(sc) = entry.status_code {
        attrs.push(otlp_kv_int("http.response.status_code", sc as i64));
    }
    if let Some(ip) = &entry.client_ip {
        attrs.push(otlp_kv_string("client.address", ip));
    }
    if let Some(d) = entry.duration_ms {
        attrs.push(otlp_kv_int("duration_ms", d as i64));
    }

    // Custom prompt_log.* fields.
    attrs.push(otlp_kv_string(
        "prompt_log.phase",
        match entry.phase {
            LogPhase::Request => "request",
            LogPhase::Response => "response",
        },
    ));
    attrs.push(otlp_kv_string("prompt_log.request_id", &entry.request_id));
    attrs.push(otlp_kv_string("prompt_log.key_hash", &entry.key_hash));
    if let Some(t) = &entry.team_alias {
        attrs.push(otlp_kv_string("prompt_log.team_alias", t));
    }
    attrs.push(otlp_kv_string("prompt_log.model", &entry.model));
    attrs.push(otlp_kv_bool("prompt_log.is_stream", entry.is_stream));
    if let Some(c) = &entry.error_code {
        attrs.push(otlp_kv_string("prompt_log.error_code", c));
    }
    if let Some(m) = &entry.error_message {
        attrs.push(otlp_kv_string("prompt_log.error_message", m));
    }

    // Large bodies — truncate if needed.
    if let Some(req) = &entry.request {
        let s = serde_json::to_string(req).unwrap_or_default();
        let (s, dropped) = truncate_to_bytes(s, max_attribute_bytes);
        if dropped > 0 {
            dropped_attributes_count += 1;
        }
        attrs.push(otlp_kv_string("prompt_log.request", &s));
    }
    if let Some(resp) = &entry.response {
        let s = serde_json::to_string(resp).unwrap_or_default();
        let (s, dropped) = truncate_to_bytes(s, max_attribute_bytes);
        if dropped > 0 {
            dropped_attributes_count += 1;
        }
        attrs.push(otlp_kv_string("prompt_log.response", &s));
    }
    if let Some(raw) = &entry.raw_upstream_response {
        let s = serde_json::to_string(raw).unwrap_or_default();
        let (s, dropped) = truncate_to_bytes(s, max_attribute_bytes);
        if dropped > 0 {
            dropped_attributes_count += 1;
        }
        attrs.push(otlp_kv_string("prompt_log.raw_upstream_response", &s));
    }

    // Headers — emit as a nested kvlist attribute. Each header value is also
    // subject to truncation.
    if let Some(headers) = &entry.headers {
        let mut pairs: Vec<OtlpKeyValue> = Vec::with_capacity(headers.len());
        for (k, v) in headers.iter() {
            let (v, _) = truncate_to_bytes(v.clone(), max_attribute_bytes);
            pairs.push(otlp_kv_string(k, &v));
        }
        attrs.push(otlp_kv_kvlist("prompt_log.headers", pairs));
    }

    let record = LogRecord {
        time_unix_nano,
        observed_time_unix_nano: time_unix_nano,
        severity_number: severity_for(entry.status_code, entry.phase),
        severity_text: severity_text(entry.status_code, entry.phase).to_string(),
        body: Some(AnyValue {
            value: Some(OtlpAnyValue::StringValue(body_summary)),
        }),
        attributes: attrs,
        dropped_attributes_count,
        flags: 1, // trace flags: sampled
        trace_id: trace_id.to_vec(),
        span_id: span_id.to_vec(),
        ..Default::default()
    };

    ResourceLogs {
        resource: Some(resource.clone()),
        scope_logs: vec![ScopeLogs {
            scope: Some(InstrumentationScope {
                name: "boom-promptlog".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            }),
            log_records: vec![record],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Pack many `ResourceLogs` into an `ExportLogsServiceRequest`. Entries from
/// the same gateway share one resource, so the caller typically pre-computes
/// the resource once and calls `convert_entry_to_log_records` in a loop.
pub fn build_export_request(resource_logs: Vec<ResourceLogs>) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest { resource_logs }
}

/// Batched OTLP exporter. Owns the HTTP client and the in-memory queue.
/// Always wrapped in `Arc` — the writer holds `Arc<Self>` so the background
/// task and the enqueue path can both reach the same shared batch.
pub struct OtelExporter {
    client: reqwest::Client,
    endpoint: String,
    extra_headers: Vec<(String, String)>,
    resource: Resource,
    batch: Arc<Mutex<Vec<PromptLogEntry>>>,
    batch_size: usize,
    flush_interval: Duration,
    max_attribute_bytes: usize,
    max_queue_size: usize,
    /// How many entries we've dropped since startup due to queue overflow.
    /// Saturation here means OTLP backend is slow.
    dropped_count: Arc<std::sync::atomic::AtomicU64>,
}

impl OtelExporter {
    /// Construct an exporter from config. Caller is responsible for spawning
    /// the flush task via `spawn_flush_task` and triggering a final flush
    /// via `flush()` at shutdown.
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
            batch: Arc::new(Mutex::new(Vec::with_capacity(config.batch_size))),
            batch_size: config.batch_size,
            flush_interval: Duration::from_secs(config.flush_interval_secs.max(1)),
            max_attribute_bytes: config.max_attribute_bytes,
            max_queue_size: config.max_queue_size,
            dropped_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// Build an exporter without an Arc wrapper, for tests where the borrow
    /// outlives the exporter.
    #[cfg(test)]
    pub fn new_owned(config: &OtlpConfig) -> Self {
        let arc = Self::new(config);
        Arc::try_unwrap(arc).unwrap_or_else(|_| panic!("unexpected Arc refs"))
    }

    /// Get a clone of the dropped-counter handle (for metrics surfacing).
    pub fn dropped_count_handle(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.dropped_count.clone()
    }

    /// Access to the shared resource (for replayer / tests).
    pub fn resource(&self) -> &Resource {
        &self.resource
    }

    pub fn max_attribute_bytes(&self) -> usize {
        self.max_attribute_bytes
    }

    /// Spawn the periodic flush task. Returns a JoinHandle the caller can
    /// await at shutdown. Drops of the handle do NOT abort the task — the
    /// task runs until the runtime is dropped or the process exits.
    pub fn spawn_flush_task(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        let start = Instant::now();
        let mut ticker = interval_at(start, me.flush_interval);
        tokio::spawn(async move {
            loop {
                ticker.tick().await;
                me.flush().await;
            }
        })
    }

    /// Push an entry into the in-memory queue. Bounded; if full, drop oldest.
    /// When the batch hits `batch_size`, flush eagerly so a steady load
    /// doesn't wait for the periodic tick.
    ///
    /// Async because the batch is held in a `tokio::sync::Mutex` and we can't
    /// `blocking_lock` from inside an async runtime.
    pub async fn enqueue(self: &Arc<Self>, entry: PromptLogEntry, _max_attribute_bytes: usize) {
        let should_flush = {
            let mut batch = self.batch.lock().await;
            if batch.len() >= self.max_queue_size {
                let dropped = batch.remove(0);
                self.dropped_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    request_id = %dropped.request_id,
                    phase = ?dropped.phase,
                    queue_size = self.max_queue_size,
                    "OTLP exporter queue full, dropping oldest entry"
                );
            }
            batch.push(entry);
            batch.len() >= self.batch_size
        };
        if should_flush {
            let me = self.clone();
            tokio::spawn(async move {
                me.flush().await;
            });
        }
    }

    /// Force a flush of the current batch right now, regardless of size.
    /// Used at shutdown and by the periodic task.
    pub async fn flush(&self) {
        let drained: Vec<PromptLogEntry> = {
            let mut batch = self.batch.lock().await;
            if batch.is_empty() {
                return;
            }
            std::mem::take(&mut *batch)
        };

        let resource_logs: Vec<ResourceLogs> = drained
            .iter()
            .map(|e| convert_entry_to_log_records(e, &self.resource, self.max_attribute_bytes))
            .collect();

        let req = build_export_request(resource_logs);
        let body = req.encode_to_vec();

        let url = format!("{}/v1/logs", self.endpoint.trim_end_matches('/'));
        let mut req_builder = self
            .client
            .post(&url)
            .header("Content-Type", "application/x-protobuf")
            .body(body);
        for (k, v) in &self.extra_headers {
            req_builder = req_builder.header(k, v);
        }

        // Retry up to 3 times with exponential backoff.
        let mut attempt = 0u32;
        loop {
            let result = req_builder
                .try_clone()
                .expect("clone req")
                .send()
                .await;
            match result {
                Ok(resp) if resp.status().is_success() => return,
                Ok(resp) => {
                    let status = resp.status();
                    tracing::warn!(
                        attempt = attempt + 1,
                        status = %status,
                        "OTLP collector returned non-success status"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        error = %e,
                        "OTLP push failed"
                    );
                }
            }
            attempt += 1;
            if attempt >= 3 {
                tracing::warn!(
                    attempts = attempt,
                    entries = drained.len(),
                    "OTLP push failed 3× — dropping batch"
                );
                self.dropped_count
                    .fetch_add(drained.len() as u64, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            tokio::time::sleep(Duration::from_millis(100u64 * (1 << attempt))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::PromptLogEntry;
    use std::collections::HashMap;

    fn make_request_entry(id: &str, trace_id: Option<String>) -> PromptLogEntry {
        PromptLogEntry::new_request(
            id,
            trace_id,
            "kh",
            Some("acct bob"),
            Some("team-a"),
            "gpt-4",
            "/v1/chat/completions",
            true,
            serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
            Some("127.0.0.1"),
            Some(HashMap::from([
                ("x-trace-id".to_string(), "trace-1".to_string()),
            ])),
        )
    }

    #[test]
    fn convert_request_entry_emits_info_severity_with_trace_id_and_span_id() {
        let entry = make_request_entry("req-1", None);
        let cfg = OtlpConfig {
            enabled: true,
            endpoint: "http://x".to_string(),
            service_name: "boom-gateway".to_string(),
            service_version: Some("0.1.0".to_string()),
            ..OtlpConfig::default()
        };
        let resource = build_resource(&cfg);
        let rl = convert_entry_to_log_records(&entry, &resource, 4096);
        let scope = &rl.scope_logs[0];
        let rec = &scope.log_records[0];

        assert_eq!(rec.severity_number, 9);
        assert_eq!(rec.severity_text, "INFO");
        // Body should mention REQ + api_path + request_id.
        let body = rec.body.as_ref().unwrap();
        let OtlpAnyValue::StringValue(s) = body.value.as_ref().unwrap() else {
            panic!("body should be a string");
        };
        assert!(s.contains("REQ"));
        assert!(s.contains("req-1"));

        // Synthetic trace_id: 16 bytes.
        assert_eq!(rec.trace_id.len(), 16);
        assert_eq!(rec.span_id.len(), 8);

        // Check attribute keys include prompt_log.phase and url.path.
        let keys: Vec<&str> = rec.attributes.iter().map(|a| a.key.as_str()).collect();
        assert!(keys.contains(&"prompt_log.phase"));
        assert!(keys.contains(&"prompt_log.request_id"));
        assert!(keys.contains(&"url.path"));
        assert!(keys.contains(&"prompt_log.headers"));

        // Resource carries service.name and service.version.
        let res = rl.resource.as_ref().unwrap();
        let res_keys: Vec<&str> = res.attributes.iter().map(|a| a.key.as_str()).collect();
        assert!(res_keys.contains(&"service.name"));
        assert!(res_keys.contains(&"service.version"));
    }

    #[test]
    fn convert_response_entry_uses_severity_from_status() {
        let req = make_request_entry("req-2", None);
        let mut resp = PromptLogEntry::new_response_from(&req);
        resp.set_status(500, 1500);
        resp.set_response(serde_json::json!({"choices": []}));
        resp.set_error(error_code::UPSTREAM_ERROR, "boom".to_string());

        let cfg = OtlpConfig::default();
        let resource = build_resource(&cfg);
        let rl = convert_entry_to_log_records(&resp, &resource, 4096);
        let rec = &rl.scope_logs[0].log_records[0];

        // 5xx → ERROR=17
        assert_eq!(rec.severity_number, 17);
        assert_eq!(rec.severity_text, "ERROR");
        // Trace id matches between request and response (same request_id).
        let req_rl = convert_entry_to_log_records(&req, &resource, 4096);
        let req_rec = &req_rl.scope_logs[0].log_records[0];
        assert_eq!(req_rec.trace_id, rec.trace_id);
        // Span ids differ (phase-scoped).
        assert_ne!(req_rec.span_id, rec.span_id);
    }

    #[test]
    fn traceparent_parsed_correctly() {
        let tp = "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01";
        let bytes = parse_traceparent(tp).unwrap();
        assert_eq!(bytes.len(), 16);
        // First byte should be 0x0a.
        assert_eq!(bytes[0], 0x0a);
    }

    #[test]
    fn invalid_traceparent_returns_none() {
        assert!(parse_traceparent("garbage").is_none());
        assert!(parse_traceparent("00-short").is_none());
    }

    #[test]
    fn large_request_body_is_truncated_with_dropped_count_increment() {
        let mut entry = make_request_entry("req-3", None);
        // Build a request body way over 64-byte budget.
        let big = "x".repeat(5000);
        entry.request = Some(serde_json::json!({"big": big}));
        let cfg = OtlpConfig::default();
        let resource = build_resource(&cfg);
        let rl = convert_entry_to_log_records(&entry, &resource, 64);
        let rec = &rl.scope_logs[0].log_records[0];
        assert!(rec.dropped_attributes_count >= 1);
        let req_attr = rec
            .attributes
            .iter()
            .find(|a| a.key == "prompt_log.request")
            .unwrap();
        let OtlpAnyValue::StringValue(s) =
            req_attr.value.as_ref().unwrap().value.as_ref().unwrap()
        else {
            panic!("expected string");
        };
        assert!(s.len() <= 64);
    }

    #[test]
    fn trace_id_reused_when_entry_carries_traceparent() {
        // Entry with an explicit traceparent — bytes are extracted from the
        // middle segment.
        let entry = make_request_entry(
            "req-4",
            Some("00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01".to_string()),
        );
        let cfg = OtlpConfig::default();
        let resource = build_resource(&cfg);
        let rl = convert_entry_to_log_records(&entry, &resource, 4096);
        let rec = &rl.scope_logs[0].log_records[0];
        // First byte matches the traceparent's trace-id segment.
        assert_eq!(rec.trace_id[0], 0x0a);
        assert_eq!(rec.trace_id[1], 0xf7);
        assert_eq!(rec.trace_id.len(), 16);

        // Two entries without trace_id (synthetic fallback) but same
        // request_id should produce the same trace id (correlation by id).
        let entry2 = make_request_entry("req-4", None);
        let rl2 = convert_entry_to_log_records(&entry2, &resource, 4096);
        let rec2 = &rl2.scope_logs[0].log_records[0];
        let entry3 = make_request_entry("req-4", None);
        let rl3 = convert_entry_to_log_records(&entry3, &resource, 4096);
        let rec3 = &rl3.scope_logs[0].log_records[0];
        assert_eq!(rec2.trace_id, rec3.trace_id);

        // Different request_id → different synthetic trace_id.
        let entry4 = make_request_entry("req-5", None);
        let rl4 = convert_entry_to_log_records(&entry4, &resource, 4096);
        let rec4 = &rl4.scope_logs[0].log_records[0];
        assert_ne!(rec2.trace_id, rec4.trace_id);
    }
}
