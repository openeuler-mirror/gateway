use serde::Deserialize;

/// Prompt logging configuration.
///
/// ```yaml
/// prompt_log:
///   enabled: false
///   dir: "/data/prompt_logs"
///   max_file_size_mb: 50
///   capture_raw_upstream: false
///   excluded_keys: []
///   excluded_teams: []
///   record_headers: []
///   otlp:
///     enabled: false
///     endpoint: "http://otel-collector:4318"
///     service_name: "boom-gateway"
///     timeout_secs: 10
///     batch_size: 512
///     flush_interval_secs: 5
///     max_attribute_bytes: 4096
///     headers: {}
///     max_queue_size: 10000
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PromptLogConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_dir")]
    pub dir: String,
    #[serde(default = "default_max_file_size")]
    pub max_file_size_mb: u64,
    /// When true, record the raw upstream response (before any format conversion)
    /// alongside the converted response. Only applies to endpoints that perform
    /// format conversion (e.g., `/v1/messages` where OpenAI → Anthropic).
    #[serde(default)]
    pub capture_raw_upstream: bool,
    #[serde(default)]
    pub excluded_keys: Vec<String>,
    #[serde(default)]
    pub excluded_teams: Vec<String>,
    /// Whitelist of request header names (lowercase) to capture into each prompt
    /// log entry. Empty list = no headers recorded. Only listed headers are
    /// stored — never the full HeaderMap — to avoid leaking sensitive headers
    /// (Authorization, Cookie, etc.).
    #[serde(default)]
    pub record_headers: Vec<String>,
    /// Optional OTLP exporter — when `enabled=true`, entries are also pushed
    /// to a remote OpenTelemetry collector (HTTP/protobuf). Local JSONL files
    /// are still written; this is a second sink, not a replacement.
    #[serde(default)]
    pub otlp: OtlpConfig,
}

/// OTLP export configuration. When `enabled=false` (the default) the gateway
/// runs purely on local JSONL files; flipping it on spawns a background
/// exporter that batches entries and POSTs them as `ExportLogsServiceRequest`
/// protobuf to `{endpoint}/v1/logs`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct OtlpConfig {
    #[serde(default)]
    pub enabled: bool,
    /// OTLP/HTTP base URL, e.g. `http://otel-collector:4318`. The exporter
    /// appends `/v1/logs` itself.
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
    /// Per-attribute byte budget. Request/response bodies that exceed this get
    /// truncated and the record's `dropped_attributes_count` is incremented.
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

impl Default for PromptLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_dir(),
            max_file_size_mb: default_max_file_size(),
            capture_raw_upstream: false,
            excluded_keys: Vec::new(),
            excluded_teams: Vec::new(),
            record_headers: Vec::new(),
            otlp: OtlpConfig::default(),
        }
    }
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

fn default_dir() -> String {
    "/data/prompt_logs".to_string()
}

fn default_max_file_size() -> u64 {
    50
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

impl PromptLogConfig {
    /// Check if a request from this key/team should be logged.
    pub fn should_capture(&self, key_hash: &str, team_id: Option<&str>) -> bool {
        if !self.enabled {
            return false;
        }
        if self.excluded_keys.iter().any(|k| k == key_hash) {
            return false;
        }
        if let Some(tid) = team_id {
            if self.excluded_teams.iter().any(|t| t == tid) {
                return false;
            }
        }
        true
    }

    /// Builder: return a copy with the global enabled flag changed.
    pub fn with_enabled(&self, enabled: bool) -> Self {
        let mut c = self.clone();
        c.enabled = enabled;
        c
    }

    /// Builder: return a copy with capture_raw_upstream changed.
    pub fn with_capture_raw_upstream(&self, capture: bool) -> Self {
        let mut c = self.clone();
        c.capture_raw_upstream = capture;
        c
    }

    /// Builder: return a copy with a key added to or removed from the exclusion list.
    pub fn with_key_excluded(&self, key_hash: &str, excluded: bool) -> Self {
        let mut c = self.clone();
        if excluded {
            if !c.excluded_keys.iter().any(|k| k == key_hash) {
                c.excluded_keys.push(key_hash.to_string());
            }
        } else {
            c.excluded_keys.retain(|k| k != key_hash);
        }
        c
    }

    /// Builder: return a copy with a team added to or removed from the exclusion list.
    pub fn with_team_excluded(&self, team_id: &str, excluded: bool) -> Self {
        let mut c = self.clone();
        if excluded {
            if !c.excluded_teams.iter().any(|t| t == team_id) {
                c.excluded_teams.push(team_id.to_string());
            }
        } else {
            c.excluded_teams.retain(|t| t != team_id);
        }
        c
    }
}
