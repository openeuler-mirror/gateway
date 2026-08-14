//! OTLP/HTTP exporter for prompt log entries.
//!
//! The exporter has one job: take `PromptLogEntry`s, batch them up, and POST
//! them as `ExportLogsServiceRequest` protobuf to `{endpoint}/v1/logs`. The
//! design rests on three principles:
//!
//! 1. **`convert_entry_to_log_records` is a pure function** — no client, no
//!    state. The offline replayer (see `replay.rs`) reuses it so the runtime
//!    and replay paths produce byte-identical LogRecords from the same entry.
//!    "What you stored locally is what gets pushed" is the contract.
//! 2. **Local JSONL is the source of truth; OTLP is best-effort.** The writer
//!    writes JSONL first and forks a copy to the exporter. Any failure on the
//!    OTLP side never blocks or stalls the local sink.
//! 3. **Result-oriented state machine.** When the OTLP backend is
//!    unreachable, the exporter enters `Offline` and skips all further pushes
//!    — no point burning CPU/network on a known-dead endpoint. Recovery is
//!    detected by a periodic lightweight probe (an empty
//!    `ExportLogsServiceRequest`). The "能推则推，不能推则丢弃" contract: we
//!    do not buffer offline entries for later replay; the JSONL file is the
//!    durable record.
//!
//! # State machine
//!
//! ```text
//!                ┌─────────────┐
//!     startup → │   Online    │
//!                │ push → batch│←────────────┐
//!                │ tick → flush│             │
//!                └──────┬──────┘             │
//!                       │                    │
//!                       │ flush 3× retry     │
//!                       │ all failed         │
//!                       ▼                    │
//!                ┌─────────────┐             │
//!                │   Offline   │── tick (5s) ─┘
//!                │ skip push   │   probe ok
//!                │ tick→probe  │
//!                └─────────────┘
//!                       │
//!                       │ probe fail (stay Offline)
//!                       ▼
//!                  (stay Offline)
//! ```
//!
//! Transitions are atomic (AtomicU8 for status, AtomicU64 for counters) so
//! the hot path (`enqueue`) reads status without locking.

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
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{interval_at, Instant};

/// Status byte for `OtelExporter::status`. Using integer constants (rather
/// than an enum) so reads on the hot path are lock-free via `AtomicU8`.
const STATUS_ONLINE: u8 = 0;
const STATUS_OFFLINE: u8 = 1;

/// Severity numbers (OTel spec): INFO=9, WARN=13, ERROR=17. The producer maps
/// HTTP status → severity; missing status defaults to INFO.
fn severity_for(status: Option<i32>, phase: LogPhase) -> i32 {
    match phase {
        LogPhase::Request => 9, // INFO — ingress is always informational
        LogPhase::Response => match status {
            Some(s) if (200..300).contains(&s) => 9,  // INFO
            Some(s) if (400..500).contains(&s) => 13, // WARN
            Some(_) => 17, // ERROR (5xx)
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
    if let Some(d) = &entry.duration_ms {
        attrs.push(otlp_kv_int("duration_ms", *d as i64));
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

/// Read-only snapshot of the OTLP exporter's runtime state.
///
/// Returned by [`OtelExporter::status_snapshot`]. The dashboard surfaces this
/// through `GET /admin/prompt-log/otlp-status` so the connectivity indicator
/// reflects the live state machine (Online/Offline) rather than a one-shot
/// manual ping. Timestamps are unix epoch seconds; `None` means "never".
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExporterStatusSnapshot {
    /// `"online"` or `"offline"`. Push is skipped while offline.
    pub status: String,
    /// The endpoint URL the exporter is currently committed to (from YAML
    /// via `replace_otlp`). Differs from the live config endpoint only
    /// briefly during a reload race window.
    pub endpoint: String,
    pub last_failure_ts: Option<u64>,
    pub last_recovery_ts: Option<u64>,
    /// Probe failures since the last successful probe. Only accumulates while
    /// Offline — Online flush successes reset this implicitly (the value is
    /// only meaningful when status == offline).
    pub consecutive_probe_failures: u64,
    /// Lifetime counter of how many times the exporter entered Offline.
    pub total_offline_episodes: u64,
    /// Entries dropped at enqueue time because the exporter was Offline.
    /// Does NOT include entries dropped due to batch overflow while Online —
    /// those are in `dropped_count`.
    pub total_dropped_during_offline: u64,
    /// Total entries ever dropped (offline skip + batch overflow + flush
    /// failure drops). Monotonic across reloads within a process lifetime.
    pub dropped_count: u64,
}

/// Result of a manual probe via [`OtelExporter::probe`].
#[derive(Debug, Clone)]
pub enum ProbeResult {
    /// Probe succeeded. If the exporter was Offline it has been transitioned
    /// back to Online.
    Ok { latency_ms: u64 },
    /// Probe failed. If the exporter was Online it remains Online (the
    /// manual probe does NOT cause a transition to Offline — only repeated
    /// flush failures do that). If it was already Offline, the failure is
    /// recorded in `consecutive_probe_failures`.
    Fail { error: String },
}

/// Batched OTLP exporter with a result-oriented state machine.
///
/// Owns the HTTP client, an in-memory batch, and the Online/Offline status.
/// Always wrapped in `Arc` — the writer holds `Arc<ArcSwap<Option<Arc<Self>>>>`
/// so the whole exporter can be hot-swapped at runtime via `replace_otlp`
/// (called by boom-main when the OTLP sub-config changes).
///
/// # Memory bounds
///
/// - `batch` is bounded by `max_queue_size` (default 10000). Overflow drops
///   oldest (O(1) via VecDeque pop_front).
/// - In-flight `flush` tasks are bounded to **1** via a `Semaphore(1)`. While
///   one flush is in retry backoff, additional batch-full triggers do NOT
///   spawn more flush tasks — they accumulate in `batch` and are picked up
///   by the next periodic tick or by the next successful flush.
/// - While Offline, `enqueue` returns immediately without touching `batch`
///   or spawning any task — zero memory growth.
///
/// # State transitions
///
/// - **Online → Offline**: a flush's three retry attempts all fail (network
///   error, HTTP non-2xx, or timeout — result-oriented, all treated the same).
/// - **Offline → Online**: a periodic probe (or manual `probe()`) returns
///   HTTP 2xx.
pub struct OtelExporter {
    client: reqwest::Client,
    endpoint: String,
    extra_headers: Vec<(String, String)>,
    resource: Resource,
    batch: Arc<Mutex<VecDeque<PromptLogEntry>>>,
    batch_size: usize,
    flush_interval: Duration,
    max_attribute_bytes: usize,
    max_queue_size: usize,
    /// Total entries ever dropped (offline skip + batch overflow + flush
    /// failure). Surfaced to dashboard via `status_snapshot`.
    dropped_count: Arc<AtomicU64>,
    /// Status byte (STATUS_ONLINE / STATUS_OFFLINE). Atomic so `enqueue`'s
    /// hot-path read is lock-free.
    status: AtomicU8,
    /// Probe failures since the last successful probe. Only meaningful while
    /// Offline — Online flush successes reset this implicitly.
    consecutive_probe_failures: AtomicU64,
    /// Unix epoch secs of the last failure (flush or probe). 0 = never.
    last_failure_ts: AtomicU64,
    /// Unix epoch secs of the last recovery (Offline → Online transition). 0 = never.
    last_recovery_ts: AtomicU64,
    /// Lifetime counter of Offline episodes.
    total_offline_episodes: AtomicU64,
    /// Entries dropped at enqueue time because the exporter was Offline.
    total_dropped_during_offline: AtomicU64,
    /// Serializes in-flight flush tasks to at most 1. Acquired before spawn,
    /// held until the flush returns.
    flush_permit: Arc<Semaphore>,
}

impl OtelExporter {
    /// Construct an exporter from config. Caller is responsible for spawning
    /// the flush task via `spawn_flush_task` and triggering a final flush
    /// via `flush()` at shutdown. The exporter starts in Online status.
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
            max_attribute_bytes: config.max_attribute_bytes,
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

    /// Build an exporter without an Arc wrapper, for tests where the borrow
    /// outlives the exporter.
    #[cfg(test)]
    pub fn new_owned(config: &OtlpConfig) -> Self {
        let arc = Self::new(config);
        Arc::try_unwrap(arc).unwrap_or_else(|_| panic!("unexpected Arc refs"))
    }

    /// Get a clone of the dropped-counter handle (for metrics surfacing).
    pub fn dropped_count_handle(&self) -> Arc<AtomicU64> {
        self.dropped_count.clone()
    }

    /// Access to the shared resource (for replayer / tests).
    pub fn resource(&self) -> &Resource {
        &self.resource
    }

    pub fn max_attribute_bytes(&self) -> usize {
        self.max_attribute_bytes
    }

    /// Read-only snapshot of the exporter's runtime state. Cheap (atomic
    /// loads only), safe to call from any thread.
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

    /// `true` when the exporter is in Offline status (push skipped).
    #[inline]
    pub fn is_offline(&self) -> bool {
        self.status.load(Ordering::Relaxed) == STATUS_OFFLINE
    }

    /// Push an entry into the in-memory queue. Behavior depends on state:
    ///
    /// - **Online**: push to `batch`. If batch is full, drop the oldest
    ///   (FIFO, O(1)). If `batch.len() >= batch_size`, try to acquire the
    ///   flush permit; if acquired, spawn a flush task; if not, the entries
    ///   wait for the next periodic tick.
    /// - **Offline**: drop immediately + bump `dropped_count` and
    ///   `total_dropped_during_offline`. Zero memory growth.
    ///
    /// Never blocks on the network. The Mutex is held only for the brief
    /// batch push; the HTTP round-trip happens in a spawned task (or the
    /// periodic tick) holding the flush permit, not the batch lock.
    pub async fn enqueue(self: &Arc<Self>, entry: PromptLogEntry) {
        // Hot path: lock-free status read. Offline → immediate drop, no batch
        // touch, no spawn. This is the contract that bounds memory under
        // backend outage.
        if self.is_offline() {
            self.dropped_count.fetch_add(1, Ordering::Relaxed);
            self.total_dropped_during_offline
                .fetch_add(1, Ordering::Relaxed);
            return;
        }

        let should_flush = {
            let mut batch = self.batch.lock().await;
            if batch.len() >= self.max_queue_size {
                let dropped = batch.pop_front().expect("batch non-empty when len>=cap");
                self.dropped_count.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    request_id = %dropped.request_id,
                    queue_size = self.max_queue_size,
                    "OTLP batch full, dropping oldest entry"
                );
            }
            batch.push_back(entry);
            batch.len() >= self.batch_size
        };

        if should_flush {
            // Try to acquire the flush permit. If acquired, spawn a flush
            // task that holds the permit until it returns. If not acquired,
            // a flush is already in flight — the entries accumulated above
            // will be picked up by the next periodic tick or by the in-flight
            // flush's next iteration. This bounds in-flight flush tasks to 1.
            if let Ok(permit) = self.flush_permit.clone().try_acquire_owned() {
                let me = self.clone();
                tokio::spawn(async move {
                    me.flush().await;
                    drop(permit);
                });
            }
        }
    }

    /// Force a flush of the current batch right now, regardless of size.
    /// Used by the periodic task and by shutdown. Also used by `probe()` to
    /// drain pending entries after a successful probe (so they don't sit in
    /// the batch until the next tick).
    pub async fn flush(&self) {
        let drained: VecDeque<PromptLogEntry> = {
            let mut batch = self.batch.lock().await;
            if batch.is_empty() {
                return;
            }
            std::mem::take(&mut *batch)
        };

        match self.send_with_retry(&drained).await {
            Ok(()) => { /* stay Online */ }
            Err(()) => {
                // Result-oriented: any failure (network, HTTP non-2xx,
                // timeout) drives the exporter Offline. The drained batch's
                // entries are counted as dropped — they're still in JSONL,
                // so not truly lost, but the OTLP copy is gone.
                self.transition_to_offline(drained.len() as u64);
            }
        }
    }

    /// Build the HTTP request body for a batch and send it, retrying up to 3
    /// times with exponential backoff (100ms, 200ms, 400ms). Returns `Err(())`
    /// if all attempts fail — caller drives the state-machine transition.
    async fn send_with_retry(&self, drained: &VecDeque<PromptLogEntry>) -> Result<(), ()> {
        let resource_logs: Vec<ResourceLogs> = drained
            .iter()
            .map(|e| convert_entry_to_log_records(e, &self.resource, self.max_attribute_bytes))
            .collect();
        let req = build_export_request(resource_logs);
        let body = req.encode_to_vec();
        let url = format!("{}/v1/logs", self.endpoint.trim_end_matches('/'));

        let mut attempt = 0u32;
        loop {
            let result = self
                .client
                .post(&url)
                .header("Content-Type", "application/x-protobuf")
                .body(body.clone())
                .send()
                .await;
            match result {
                Ok(resp) if resp.status().is_success() => return Ok(()),
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
                return Err(());
            }
            tokio::time::sleep(Duration::from_millis(100u64 * (1 << attempt))).await;
        }
    }

    /// Manual probe. Sends an empty `ExportLogsServiceRequest` to the
    /// endpoint using the exporter's own HTTP client (so the probe goes
    /// through the same connection pool / headers / timeout as a real push).
    ///
    /// - Success → if was Offline, transition to Online. Returns `Ok { latency_ms }`.
    /// - Failure → if was Offline, bump `consecutive_probe_failures`. If was
    ///   Online, leave it Online (manual probe failure does NOT drive the
    ///   exporter Offline — only repeated flush failures do that).
    ///
    /// This is the API the dashboard's "Test" button calls through
    /// `PromptLogWriter::probe_otlp`. The periodic tick uses the internal
    /// `run_probe_cycle` instead, which logs differently.
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

    /// Internal: send an empty ExportLogsServiceRequest, return latency on
    /// success or a one-line error on failure. Does NOT touch the state
    /// machine — the caller decides what to do with the result.
    async fn probe_internal(&self) -> Result<u64, String> {
        let req = build_export_request(Vec::new());
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
        let started = std::time::Instant::now();
        match req_builder.send().await {
            Ok(resp) if resp.status().is_success() => {
                Ok(started.elapsed().as_millis() as u64)
            }
            Ok(resp) => Err(format!("HTTP {}", resp.status())),
            Err(e) => Err(format!("{e}")),
        }
    }

    /// Periodic probe cycle: probe, and on success transition to Online and
    /// drain any entries that accumulated in `batch` while Offline (none
    /// should exist since Offline enqueue drops, but a race between
    /// transition_to_offline and an in-flight enqueue could leave a few). On
    /// failure, record and stay Offline.
    async fn run_probe_cycle(&self) {
        match self.probe_internal().await {
            Ok(latency_ms) => {
                self.transition_to_online();
                tracing::info!(
                    latency_ms,
                    "OTLP probe succeeded — back online, resuming push"
                );
                // Drain anything that raced into the batch during the
                // Offline→Online transition window. Best-effort: a concurrent
                // enqueue might still be in flight; those will be picked up
                // by the next periodic tick.
                self.flush().await;
            }
            Err(e) => {
                self.record_probe_failure();
                tracing::debug!(error = %e, "OTLP probe still failing");
            }
        }
    }

    // ── State machine transitions ─────────────────────────────────────────

    /// Transition to Offline. Called when a flush's three retry attempts
    /// all failed. Records timestamps and counters, logs a warn.
    fn transition_to_offline(&self, dropped_in_batch: u64) {
        let prev = self.status.swap(STATUS_OFFLINE, Ordering::Relaxed);
        let now = now_epoch_secs();
        self.last_failure_ts.store(now, Ordering::Relaxed);
        // Only bump episodes when actually transitioning (not when already
        // Offline and another flush somehow ran — defensive).
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
            "OTLP offline — entering skip-push mode (push will resume after a probe succeeds)"
        );
    }

    /// Transition to Online. Called when a probe succeeds. Only logs when
    /// the previous status was Offline (no log spam if already Online).
    fn transition_to_online(&self) {
        let prev = self.status.swap(STATUS_ONLINE, Ordering::Relaxed);
        if prev == STATUS_OFFLINE {
            let now = now_epoch_secs();
            self.last_recovery_ts.store(now, Ordering::Relaxed);
            self.consecutive_probe_failures.store(0, Ordering::Relaxed);
            tracing::info!(
                endpoint = %self.endpoint,
                "OTLP recovered — resuming push"
            );
        }
    }

    /// Record a probe failure (probe couldn't reach the backend). Bumps
    /// `consecutive_probe_failures` and `last_failure_ts`. Status is left
    /// unchanged — if we were Offline we stay Offline; if we were Online
    /// (e.g. manual probe failed while flushes are still succeeding) we
    /// don't punish the exporter for a single flaky probe.
    fn record_probe_failure(&self) {
        self.consecutive_probe_failures
            .fetch_add(1, Ordering::Relaxed);
        self.last_failure_ts.store(now_epoch_secs(), Ordering::Relaxed);
    }

    // ── Periodic task ─────────────────────────────────────────────────────

    /// Spawn the periodic flush task. Returns a JoinHandle the caller can
    /// await at shutdown. Drops of the handle do NOT abort the task — the
    /// task runs until the runtime is dropped or the process exits.
    ///
    /// For hot-reload paths prefer `spawn_flush_task_to_handle`, which writes
    /// the JoinHandle into an externally-managed `Mutex<Option<JoinHandle>>`
    /// so the caller can `abort()` it before constructing a replacement.
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

    /// Same as `spawn_flush_task` but writes the JoinHandle into the caller-
    /// supplied `flush_handle` slot instead of returning it. Used by
    /// `PromptLogWriter::replace_otlp` so the reload path can abort the
    /// previous flush task before spawning a fresh one against the rebuilt
    /// exporter — same pattern as `AppState::kv_prune_handle`.
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

/// Read an `AtomicU64` as `Option<u64>` — `0` means "never set" → `None`.
fn read_atomic_opt(a: &AtomicU64) -> Option<u64> {
    let v = a.load(Ordering::Relaxed);
    if v == 0 {
        None
    } else {
        Some(v)
    }
}

/// Current unix epoch in seconds. Used for `last_failure_ts` /
/// `last_recovery_ts` in the status snapshot.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Probe a remote OTLP/HTTP collector by sending an empty
/// `ExportLogsServiceRequest`. A real collector returns 200 OK for an empty
/// batch (no LogRecords), which lets us distinguish "the collector is up and
/// speaking OTLP" from "network unreachable". Returns the round-trip latency
/// in milliseconds on success, or a one-line error string on failure.
///
/// This is the **standalone** probe — it constructs its own reqwest::Client
/// each call. It's used by the dashboard's "Test" button to validate an
/// endpoint the operator just typed into the form (not yet saved to YAML).
/// For probing the live exporter's committed endpoint, use
/// `PromptLogWriter::probe_otlp` instead — that goes through the exporter's
/// own client and drives the state machine.
///
/// Single attempt — no retry. The dashboard polls every 5s and would just
/// compound backoff if this retried internally.
pub async fn ping_endpoint(config: &OtlpConfig) -> Result<u64, String> {
    if config.endpoint.trim().is_empty() {
        return Err("endpoint not configured".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs.max(1)))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let req = build_export_request(Vec::new());
    let body = req.encode_to_vec();
    let url = format!("{}/v1/logs", config.endpoint.trim_end_matches('/'));
    let mut req_builder = client
        .post(&url)
        .header("Content-Type", "application/x-protobuf")
        .body(body);
    for (k, v) in &config.headers {
        req_builder = req_builder.header(k, v);
    }
    let started = std::time::Instant::now();
    match req_builder.send().await {
        Ok(resp) if resp.status().is_success() => {
            Ok(started.elapsed().as_millis() as u64)
        }
        Ok(resp) => Err(format!("HTTP {}", resp.status())),
        Err(e) => Err(format!("{e}")),
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

    // ── State machine unit tests ──────────────────────────────────────────

    #[test]
    fn new_exporter_starts_online() {
        let cfg = OtlpConfig::default();
        let exp = OtelExporter::new_owned(&cfg);
        assert!(!exp.is_offline());
        let snap = exp.status_snapshot();
        assert_eq!(snap.status, "online");
        assert_eq!(snap.total_offline_episodes, 0);
        assert_eq!(snap.total_dropped_during_offline, 0);
        assert_eq!(snap.dropped_count, 0);
        assert_eq!(snap.last_failure_ts, None);
        assert_eq!(snap.last_recovery_ts, None);
    }

    #[test]
    fn transition_to_offline_then_back_records_timestamps_and_episodes() {
        let cfg = OtlpConfig::default();
        let exp = OtelExporter::new_owned(&cfg);

        // Online → Offline: 3 entries dropped in the failed flush.
        exp.transition_to_offline(3);
        assert!(exp.is_offline());
        let snap = exp.status_snapshot();
        assert_eq!(snap.status, "offline");
        assert_eq!(snap.total_offline_episodes, 1);
        assert_eq!(snap.dropped_count, 3);
        assert!(snap.last_failure_ts.is_some());
        assert!(snap.last_recovery_ts.is_none()); // not recovered yet

        // Offline → Online.
        exp.transition_to_online();
        assert!(!exp.is_offline());
        let snap = exp.status_snapshot();
        assert_eq!(snap.status, "online");
        assert_eq!(snap.total_offline_episodes, 1); // monotonic
        assert!(snap.last_recovery_ts.is_some());

        // Online → Offline again: episode count increments.
        exp.transition_to_offline(0);
        let snap = exp.status_snapshot();
        assert_eq!(snap.total_offline_episodes, 2);
    }

    #[test]
    fn transition_to_online_when_already_online_is_noop() {
        let cfg = OtlpConfig::default();
        let exp = OtelExporter::new_owned(&cfg);

        // Already Online — calling transition_to_online should NOT set
        // last_recovery_ts (no episode to recover from).
        exp.transition_to_online();
        let snap = exp.status_snapshot();
        assert_eq!(snap.status, "online");
        assert_eq!(snap.last_recovery_ts, None);
        assert_eq!(snap.total_offline_episodes, 0);
    }

    #[test]
    fn record_probe_failure_bumps_consecutive_counter_and_last_failure() {
        let cfg = OtlpConfig::default();
        let exp = OtelExporter::new_owned(&cfg);
        // Manually push Offline so record_probe_failure is the right path.
        exp.transition_to_offline(0);

        exp.record_probe_failure();
        exp.record_probe_failure();
        exp.record_probe_failure();

        let snap = exp.status_snapshot();
        assert_eq!(snap.consecutive_probe_failures, 3);
        assert!(snap.last_failure_ts.is_some());
        assert!(exp.is_offline());

        // transition_to_online resets consecutive_probe_failures.
        exp.transition_to_online();
        let snap = exp.status_snapshot();
        assert_eq!(snap.consecutive_probe_failures, 0);
    }

    #[tokio::test]
    async fn enqueue_while_offline_drops_immediately_and_grows_no_batch() {
        let cfg = OtlpConfig::default();
        let exp = OtelExporter::new(&cfg);

        // Force Offline.
        exp.transition_to_offline(0);
        assert!(exp.is_offline());

        // Push 5 entries — all should be dropped at enqueue, batch stays empty.
        for i in 0..5 {
            let entry = make_request_entry(&format!("off-{i}"), None);
            exp.enqueue(entry).await;
        }

        let batch_len = exp.batch.lock().await.len();
        assert_eq!(batch_len, 0, "batch must stay empty while Offline");

        let snap = exp.status_snapshot();
        assert_eq!(snap.total_dropped_during_offline, 5);
        assert_eq!(snap.dropped_count, 5);
    }

    #[tokio::test]
    async fn enqueue_while_online_pushes_to_batch() {
        let cfg = OtlpConfig::default();
        let exp = OtelExporter::new(&cfg);

        // Online by default.
        for i in 0..3 {
            let entry = make_request_entry(&format!("on-{i}"), None);
            exp.enqueue(entry).await;
        }

        let batch_len = exp.batch.lock().await.len();
        assert_eq!(batch_len, 3);
        let snap = exp.status_snapshot();
        assert_eq!(snap.total_dropped_during_offline, 0);
        assert_eq!(snap.dropped_count, 0);
    }

    #[tokio::test]
    async fn enqueue_overflow_drops_oldest_from_batch() {
        // max_queue_size=2 → push 3, oldest dropped.
        let cfg = OtlpConfig {
            enabled: true,
            endpoint: "http://x".to_string(),
            max_queue_size: 2,
            batch_size: 10, // large so we don't trigger flush spawn
            ..OtlpConfig::default()
        };
        let exp = OtelExporter::new(&cfg);

        // First two fill the batch.
        exp.enqueue(make_request_entry("a", None)).await;
        exp.enqueue(make_request_entry("b", None)).await;
        // Third overflows — "a" should be dropped.
        exp.enqueue(make_request_entry("c", None)).await;

        let batch = exp.batch.lock().await;
        assert_eq!(batch.len(), 2);
        // The first-in ("a") is gone; "b" and "c" remain.
        let ids: Vec<&str> = batch.iter().map(|e| e.request_id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c"]);
        drop(batch);

        let snap = exp.status_snapshot();
        assert_eq!(snap.dropped_count, 1);
    }

    #[tokio::test]
    async fn flush_empty_batch_is_noop() {
        // Empty batch → flush returns immediately, no state change.
        let cfg = OtlpConfig::default();
        let exp = OtelExporter::new(&cfg);
        exp.flush().await;
        assert!(!exp.is_offline());
        let snap = exp.status_snapshot();
        assert_eq!(snap.dropped_count, 0);
    }

    #[test]
    fn record_probe_failure_while_online_does_not_flip_status() {
        // Per spec: a probe failure while Online must NOT drive the exporter
        // Offline — only repeated flush failures do that. Otherwise a flaky
        // single probe could push the exporter Offline and cause drops.
        let cfg = OtlpConfig::default();
        let exp = OtelExporter::new_owned(&cfg);
        assert!(!exp.is_offline());
        exp.record_probe_failure();
        exp.record_probe_failure();
        assert!(!exp.is_offline(), "probe failure must not flip Online");
        let snap = exp.status_snapshot();
        assert_eq!(snap.consecutive_probe_failures, 2);
        assert!(snap.last_failure_ts.is_some());
    }

    #[test]
    fn status_snapshot_endpoint_matches_config() {
        // Sanity: the snapshot's endpoint reflects the exporter's committed
        // config — the dashboard's polling indicator shows this so the
        // operator can see which URL is currently being pushed to.
        let cfg = OtlpConfig {
            enabled: true,
            endpoint: "http://collector.local:4318".to_string(),
            ..OtlpConfig::default()
        };
        let exp = OtelExporter::new_owned(&cfg);
        let snap = exp.status_snapshot();
        assert_eq!(snap.endpoint, "http://collector.local:4318");
    }

    #[test]
    fn transition_to_offline_idempotent_does_not_double_count_episodes() {
        // Calling transition_to_offline twice in a row must only bump the
        // episode counter once — the transition_to_offline body uses
        // `prev == STATUS_ONLINE` to gate the bump.
        let cfg = OtlpConfig::default();
        let exp = OtelExporter::new_owned(&cfg);
        exp.transition_to_offline(0);
        exp.transition_to_offline(0);
        let snap = exp.status_snapshot();
        assert_eq!(snap.total_offline_episodes, 1);
    }
}
