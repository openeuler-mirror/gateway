//! OTLP exporter configuration — shared between boom-promptlog (logs) and
//! boom-trace (traces) so the two channels share one config type. Lives
//! here in boom-core (rather than boom-config) because it's a runtime
//! shape carried inside `PromptLogConfig` / `TraceConfig`, not a top-level
//! YAML section, and both leaf crates already depend on boom-core.
//!
//! Default values are picked to match `boom_promptlog`'s historical
//! defaults so existing YAML configs keep working.

use serde::{Deserialize, Serialize};

/// OTLP/HTTP exporter config. Used as `prompt_log.otlp` and `trace.otlp`.
/// Endpoint is OTLP/HTTP base (e.g. `http://otel-collector:4318`); the
/// exporter appends `/v1/logs` or `/v1/traces` itself.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct OtlpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_service_name")]
    pub service_name: String,
    /// Defaults to the gateway's CARGO_PKG_VERSION when None.
    #[serde(default)]
    pub service_version: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_flush_interval_secs")]
    pub flush_interval_secs: u64,
    /// Per-attribute byte budget. Bodies that exceed this get truncated and
    /// the record's `dropped_attributes_count` is incremented.
    #[serde(default = "default_max_attribute_bytes")]
    pub max_attribute_bytes: usize,
    /// Extra HTTP headers to attach to OTLP POSTs (e.g. SaaS backend auth).
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    /// In-memory queue cap. When full, oldest entries are dropped with a
    /// `tracing::warn!` — the gateway must never block on OTLP.
    #[serde(default = "default_max_queue_size")]
    pub max_queue_size: usize,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            service_name: default_service_name(),
            service_version: None,
            timeout_secs: default_timeout_secs(),
            batch_size: default_batch_size(),
            flush_interval_secs: default_flush_interval_secs(),
            max_attribute_bytes: default_max_attribute_bytes(),
            headers: std::collections::HashMap::new(),
            max_queue_size: default_max_queue_size(),
        }
    }
}

fn default_service_name() -> String {
    "boom-gateway".to_string()
}

fn default_timeout_secs() -> u64 {
    10
}

fn default_batch_size() -> usize {
    512
}

fn default_flush_interval_secs() -> u64 {
    5
}

fn default_max_attribute_bytes() -> usize {
    4096
}

fn default_max_queue_size() -> usize {
    10000
}
