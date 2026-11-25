use serde::{Deserialize, Serialize};
// `OtlpConfig` lives in boom-core now so boom-trace can share it without
// depending on this crate. Re-export for back-compat with anything that
// was importing `boom_promptlog::OtlpConfig`.
pub use boom_core::OtlpConfig;

// Built-in behavior constants. Formerly YAML knobs, retired after the
// operator-facing surface was cut down to what users can actually feel
// (seal size + retention). Changing any of these is a code change, not
// a config change.
//
// - fsync: group commit — the tick fsyncs at most once per interval, so
//   power loss costs at most the last interval's writes.
// - zstd level 1: compression ratio is flat across 1..3 while speed drops
//   3-5x, and the zstd-1 window is far smaller than any block we write.
// - channel/spill budgets: backpressure tiers, only relevant when the
//   writer is already stalled.
// - retention scan cadence: internal housekeeping rhythm.
pub(crate) const BUILTIN_FSYNC_INTERVAL_MS: u64 = 1000;
pub(crate) const BUILTIN_ZSTD_LEVEL: i32 = 1;
pub(crate) const BUILTIN_CHANNEL_MAX_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const BUILTIN_SPILL_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const BUILTIN_RETENTION_SCAN_SECS: u64 = 600;

/// Prompt logging configuration.
///
/// ```yaml
/// prompt_log:
///   enabled: false
///   dir: "/data/prompt_logs"
///   capture_raw_upstream: false
///   excluded_keys: []
///   excluded_teams: []
///   record_headers: []
///   segment:
///     max_segment_mb: 512        # seal threshold (uncompressed bytes)
///     max_segment_age_secs: 86400 # seal threshold (time), whichever first
///     block_bytes: 10485760      # compression/IO unit (~10MB)
///   retention:                   # DB-authoritative (global domain): this
///     enabled: false             #   YAML section is the first-boot seed
///     max_age_days: 7            # delete sealed segments older than this
///     max_total_gb: 0            # 0 = unlimited total size cap
///     disk_pct_watermark: 90     # emergency delete-oldest when disk use exceeds
///   otlp:
///     enabled: false
///     endpoint: "http://otel-collector:4318"
/// ```
///
/// Storage knobs are process/host-local by the §7 three-question test
/// (paths, memory budgets) — YAML private domain, never broadcast,
/// instances may legitimately diverge. `retention` is the exception: an
/// operator business policy enforced uniformly fleet-wide (each instance
/// ages its own local directory under the shared DB-authoritative policy).
/// Retention ships **disabled** (`enabled: false`) — the store grows
/// unboundedly until the operator opts in, matching the pre-refactor
/// JSONL behavior of never deleting anything.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PromptLogConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_dir")]
    pub dir: String,
    /// When true, record the raw gateway↔upstream exchange on every
    /// Response-phase entry: `bg_request` (exact body the gateway assembled
    /// and sent upstream) and `raw_response` (literal upstream body — SSE
    /// frames joined for streaming, including upstream error bodies and
    /// frames that failed parsing). Currently implemented by the OpenAI
    /// provider; other providers record nothing for these fields.
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
    #[serde(default)]
    pub segment: SegmentConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    /// Optional OTLP exporter — when `enabled=true`, entries are also pushed
    /// to a remote OpenTelemetry collector (HTTP/protobuf). Local segments
    /// are still written; this is a second sink, not a replacement.
    #[serde(default)]
    pub otlp: OtlpConfig,
}

/// Segment file layout knobs. Sealing happens on whichever threshold is hit
/// first; sealed segments are immutable and deletable as a whole (retention).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SegmentConfig {
    #[serde(default = "default_max_segment_mb")]
    pub max_segment_mb: u64,
    #[serde(default = "default_max_segment_age_secs")]
    pub max_segment_age_secs: u64,
    #[serde(default = "default_block_bytes")]
    pub block_bytes: usize,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        Self {
            max_segment_mb: default_max_segment_mb(),
            max_segment_age_secs: default_max_segment_age_secs(),
            block_bytes: default_block_bytes(),
        }
    }
}

impl SegmentConfig {
    /// Clamp operator input into the safe working range: a segment below
    /// ~16MB rotates once per minute at modest traffic (segment flood), a
    /// block below 64KB loses the cross-record compression win that is
    /// the whole point of block framing.
    pub fn clamped(&self) -> Self {
        Self {
            max_segment_mb: self.max_segment_mb.clamp(16, 1024 * 64),
            max_segment_age_secs: self.max_segment_age_secs.clamp(60, 24 * 3600),
            block_bytes: self.block_bytes.clamp(64 * 1024, 32 * 1024 * 1024),
        }
    }

    pub fn max_segment_bytes(&self) -> u64 {
        self.max_segment_mb * 1024 * 1024
    }
}

fn default_max_segment_mb() -> u64 {
    512
}
fn default_max_segment_age_secs() -> u64 {
    24 * 3600
}
fn default_block_bytes() -> usize {
    10 * 1024 * 1024
}

/// Retention / aging policy. Evaluated against sealed segments only; the
/// active segment is never a deletion candidate.
///
/// Domain (§7): **global, DB-authoritative** — the retention policy is an
/// operator business decision ("how long do we keep prompt logs") that must
/// be uniform across instances, so it lives in the `boom_config` KV row
/// `prompt_log_retention` (CATEGORY_PROMPTLOG broadcast). Each instance
/// enforces the shared policy against its own local segment directory.
/// The YAML `retention:` section is first-boot seed only.
///
/// `enabled: false` (the default) skips aging entirely — no scan, no
/// deletion, the other three knobs are inert. This matches the
/// pre-refactor JSONL store, which never deleted anything; operators opt
/// in per fleet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetentionConfig {
    /// Master switch: when false, no aging at all (unbounded growth).
    #[serde(default)]
    pub enabled: bool,
    /// Delete a sealed segment when its newest record is older than this.
    /// 0 disables time-based aging (today's behavior).
    #[serde(default = "default_max_age_days")]
    pub max_age_days: u64,
    /// Keep at most this many GB of sealed segments (delete oldest beyond
    /// the cap). 0 = unlimited.
    #[serde(default)]
    pub max_total_gb: u64,
    /// When filesystem use above the store's filesystem exceeds this
    /// percentage, delete oldest sealed segments until below. 0 disables.
    #[serde(default = "default_disk_pct_watermark")]
    pub disk_pct_watermark: u8,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_age_days: default_max_age_days(),
            max_total_gb: 0,
            disk_pct_watermark: default_disk_pct_watermark(),
        }
    }
}

fn default_max_age_days() -> u64 {
    7
}
fn default_disk_pct_watermark() -> u8 {
    90
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

fn default_dir() -> String {
    "/data/prompt_logs".to_string()
}

// derive(Default) ignores the serde default functions; spell it out so
// `PromptLogConfig::default()` matches what an empty YAML section parses to.
impl Default for PromptLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_dir(),
            capture_raw_upstream: false,
            excluded_keys: Vec::new(),
            excluded_teams: Vec::new(),
            record_headers: Vec::new(),
            segment: SegmentConfig::default(),
            retention: RetentionConfig::default(),
            otlp: OtlpConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe_and_complete() {
        let c = PromptLogConfig::default();
        assert_eq!(c.dir, "/data/prompt_logs");
        assert_eq!(c.segment.max_segment_mb, 512);
        assert_eq!(c.segment.max_segment_age_secs, 24 * 3600);
        assert_eq!(c.segment.block_bytes, 10 * 1024 * 1024);
        // Retention ships OFF: unbounded growth unless explicitly enabled.
        assert!(!c.retention.enabled);
        assert_eq!(c.retention.max_age_days, 7);
    }

    #[test]
    fn clamps_out_of_range_knobs() {
        let c = SegmentConfig {
            max_segment_mb: 0,
            block_bytes: 10,
            ..Default::default()
        };
        let clamped = c.clamped();
        assert_eq!(clamped.max_segment_mb, 16);
        assert_eq!(clamped.block_bytes, 64 * 1024);

        let c = SegmentConfig {
            block_bytes: 512 * 1024 * 1024,
            ..Default::default()
        };
        assert_eq!(c.clamped().block_bytes, 32 * 1024 * 1024);
    }

    #[test]
    fn parses_yaml_sections_and_retired_fields_are_ignored() {
        let v: serde_json::Value = serde_json::json!({
            "enabled": true,
            "dir": "/tmp/pl",
            "max_file_size_mb": 50,
            // retired knobs — whole sections/keys are unknown now and
            // silently ignored, so old configs keep parsing
            "retention": {"max_age_days": 3, "dry_run": true, "scan_interval_secs": 60},
            "durability": {"fsync": "sync"},
            "spill": {"max_bytes": 1},
            "channel": {"max_bytes": 1},
            "segment": {"zstd_level": 9},
        });
        let c: PromptLogConfig = serde_json::from_value(v).unwrap();
        assert!(c.enabled);
        assert_eq!(c.retention.max_age_days, 3);
        assert!(!c.retention.enabled);
        assert_eq!(c.segment.max_segment_mb, 512);
    }
}
