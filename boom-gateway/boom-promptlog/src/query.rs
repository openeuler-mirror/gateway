//! Read-only query API for the prompt log.
//!
//! Dashboard uses this trait to look up entries and to read the live config
//! snapshot — it must NOT hold a `PromptLogWriter` handle directly (that
//! handle is owned by boom-main; dashboard shouldn't depend on the writer's
//! type or its file-scan internals).
//!
//! The file-backed implementation here lives in boom-promptlog because the
//! JSONL layout is this crate's private detail. A future storage backend
//! (DB / object store) just swaps the implementation behind this trait.

use crate::config::PromptLogConfig;
use crate::entry::{LogPhase, PromptLogEntry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Snapshot of the parts of `PromptLogConfig` that the dashboard needs to
/// render its admin UI. We don't hand back the whole config to keep the
/// schema narrow and explicit — new private fields won't silently leak into
/// the dashboard.
#[derive(Debug, Clone)]
pub struct PromptLogConfigSnapshot {
    pub enabled: bool,
    pub dir: String,
    pub capture_raw_upstream: bool,
    pub excluded_keys: Vec<String>,
    pub excluded_teams: Vec<String>,
    pub record_headers: Vec<String>,
    pub otlp_enabled: bool,
    pub otlp_endpoint: String,
}

impl From<&PromptLogConfig> for PromptLogConfigSnapshot {
    fn from(c: &PromptLogConfig) -> Self {
        Self {
            enabled: c.enabled,
            dir: c.dir.clone(),
            capture_raw_upstream: c.capture_raw_upstream,
            excluded_keys: c.excluded_keys.clone(),
            excluded_teams: c.excluded_teams.clone(),
            record_headers: c.record_headers.clone(),
            otlp_enabled: c.otlp.enabled,
            otlp_endpoint: c.otlp.endpoint.clone(),
        }
    }
}

/// Read-only window into the prompt log store. Implementations live in
/// boom-main (which owns the writer) and just forward to the file scan here.
pub trait PromptLogQueryApi: Send + Sync {
    /// Look up a single entry by (request_id, key_hash, team_alias, phase).
    /// `team_alias=None` maps to the `_no_team` directory.
    fn find_entry(
        &self,
        request_id: &str,
        key_hash: &str,
        team_alias: Option<&str>,
        phase: LogPhase,
    ) -> Option<PromptLogEntry>;

    /// Snapshot of the live config at the moment of the call. Used by the
    /// dashboard to render the admin page (excluded lists, record_headers,
    /// otlp toggle) without holding the writer handle.
    fn config_snapshot(&self) -> PromptLogConfigSnapshot;

    /// Full live config clone. Used by the dashboard's toggle handlers —
    /// they read-modify-write via `PromptLogConfig::with_*` builders, then
    /// push the result back through `AdminCommand::UpdatePromptLogConfig`.
    /// The narrow `config_snapshot()` is for rendering; this is for mutation.
    fn full_config(&self) -> PromptLogConfig;
}

/// File-backed `PromptLogQueryApi` implementation. Holds an `Arc<ArcSwap<_>>`
/// config so hot-reloads are visible without rebuilding the trait object.
pub struct FilePromptLogQuery {
    config: Arc<arc_swap::ArcSwap<PromptLogConfig>>,
}

impl FilePromptLogQuery {
    pub fn new(config: Arc<arc_swap::ArcSwap<PromptLogConfig>>) -> Self {
        Self { config }
    }
}

impl PromptLogQueryApi for FilePromptLogQuery {
    fn find_entry(
        &self,
        request_id: &str,
        key_hash: &str,
        team_alias: Option<&str>,
        phase: LogPhase,
    ) -> Option<PromptLogEntry> {
        let cfg = self.config.load();
        // The layout matches writer.rs: {dir}/{team}/{key_hash}/{phase}.jsonl{,.gz}
        let team_dir = team_alias.unwrap_or("_no_team");
        let key_dir: PathBuf = Path::new(&cfg.dir).join(team_dir).join(key_hash);
        scan_dir_for_entry(&key_dir, request_id, phase)
    }

    fn config_snapshot(&self) -> PromptLogConfigSnapshot {
        PromptLogConfigSnapshot::from(self.config.load().as_ref())
    }

    fn full_config(&self) -> PromptLogConfig {
        self.config.load().as_ref().clone()
    }
}

/// Scan a per-key directory for the first entry whose `request_id` matches
/// the requested phase.
///
/// Reads `.jsonl` files newest-first (matching writer.rs's
/// `{phase}_{seq:06}.jsonl` naming). Older `.jsonl.gz` files are decompressed
/// on the fly. The scan is O(total entry count across the key's history) —
/// fine for the dashboard's "drill into one request" path, but not for bulk
/// queries. A manifest index would be the next step if that ever becomes a
/// bottleneck.
fn scan_dir_for_entry(
    key_dir: &Path,
    request_id: &str,
    phase: LogPhase,
) -> Option<PromptLogEntry> {
    let phase_prefix = match phase {
        LogPhase::Request => "request_",
        LogPhase::Response => "response_",
    };
    let entries = match std::fs::read_dir(key_dir) {
        Ok(rd) => rd,
        Err(_) => return None,
    };

    // Collect (sequence, path, compressed) triples; sort by sequence desc.
    let mut files: Vec<(u64, PathBuf, bool)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let (seq_str, compressed) = if let Some(s) = name_str
            .strip_prefix(phase_prefix)
            .and_then(|s| s.strip_suffix(".jsonl"))
        {
            (s, false)
        } else if let Some(s) = name_str
            .strip_prefix(phase_prefix)
            .and_then(|s| s.strip_suffix(".jsonl.gz"))
        {
            (s, true)
        } else {
            continue;
        };
        let seq: u64 = match seq_str.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        files.push((seq, entry.path(), compressed));
    }
    files.sort_by_key(|(seq, _, _)| std::cmp::Reverse(*seq));

    for (_seq, path, compressed) in files {
        let data = if compressed {
            match read_gz(&path) {
                Ok(d) => d,
                Err(_) => continue,
            }
        } else {
            match std::fs::read_to_string(&path) {
                Ok(d) => d,
                Err(_) => continue,
            }
        };
        for line in data.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: PromptLogEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.request_id == request_id {
                return Some(entry);
            }
        }
    }
    None
}

/// Decompress a `.jsonl.gz` file to a string. Synchronous because this is a
/// dashboard one-shot lookup, not a hot path.
fn read_gz(path: &Path) -> std::io::Result<String> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let bytes = std::fs::read(path)?;
    let mut dec = GzDecoder::new(&bytes[..]);
    let mut out = String::new();
    dec.read_to_string(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::PromptLogEntry;
    use arc_swap::ArcSwap;
    use std::fs;
    use std::sync::Arc;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmp dir")
    }

    fn write_jsonl(path: &Path, lines: &[String]) {
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn find_entry_scans_newest_first_and_matches_phase() {
        let dir = tmp_dir();
        let cfg = PromptLogConfig {
            enabled: true,
            dir: dir.path().to_string_lossy().to_string(),
            ..PromptLogConfig::default()
        };
        let query = FilePromptLogQuery::new(Arc::new(ArcSwap::from_pointee(cfg)));

        let key_dir = dir.path().join("_no_team").join("kh");
        fs::create_dir_all(&key_dir).unwrap();

        // Older file: seq 1, contains request entry for req-old.
        let req_old = PromptLogEntry::new_request(
            "req-old",
            None,
            "kh",
            None,
            None,
            "gpt-4",
            "/v1/chat/completions",
            false,
            serde_json::json!({}),
            None,
            None,
        );
        write_jsonl(
            &key_dir.join("request_000001.jsonl"),
            &[serde_json::to_string(&req_old).unwrap()],
        );

        // Newer file: seq 2, contains request entry for req-new.
        let req_new = PromptLogEntry::new_request(
            "req-new",
            None,
            "kh",
            None,
            None,
            "gpt-4",
            "/v1/chat/completions",
            false,
            serde_json::json!({}),
            None,
            None,
        );
        let mut resp_new = PromptLogEntry::new_response_from(&req_new);
        resp_new.set_status(200, 100);
        write_jsonl(
            &key_dir.join("request_000002.jsonl"),
            &[serde_json::to_string(&req_new).unwrap()],
        );
        write_jsonl(
            &key_dir.join("response_000002.jsonl"),
            &[serde_json::to_string(&resp_new).unwrap()],
        );

        // Lookups should find the right entry by phase.
        let found_req = query.find_entry("req-new", "kh", None, LogPhase::Request);
        assert!(found_req.is_some());
        assert_eq!(found_req.unwrap().phase, LogPhase::Request);

        let found_resp = query.find_entry("req-new", "kh", None, LogPhase::Response);
        assert!(found_resp.is_some());
        assert_eq!(found_resp.unwrap().status_code, Some(200));

        // Older entry still reachable.
        let found_old = query.find_entry("req-old", "kh", None, LogPhase::Request);
        assert!(found_old.is_some());

        // Missing key returns None — no panic.
        let missing = query.find_entry("nope", "kh", None, LogPhase::Request);
        assert!(missing.is_none());

        // Missing team dir returns None.
        let missing_team = query.find_entry("x", "kh", Some("phantom"), LogPhase::Request);
        assert!(missing_team.is_none());
    }

    #[test]
    fn config_snapshot_reflects_live_state() {
        let dir = tmp_dir();
        let cfg = PromptLogConfig {
            enabled: true,
            dir: dir.path().to_string_lossy().to_string(),
            excluded_keys: vec!["deadbeef".to_string()],
            excluded_teams: vec!["team-x".to_string()],
            record_headers: vec!["x-trace-id".to_string()],
            otlp: crate::config::OtlpConfig {
                enabled: true,
                endpoint: "http://collector:4318".to_string(),
                ..crate::config::OtlpConfig::default()
            },
            ..PromptLogConfig::default()
        };
        let cfg_arc = Arc::new(ArcSwap::from_pointee(cfg));
        let query = FilePromptLogQuery::new(cfg_arc.clone());

        let snap = query.config_snapshot();
        assert!(snap.enabled);
        assert!(snap.otlp_enabled);
        assert_eq!(snap.otlp_endpoint, "http://collector:4318");
        assert_eq!(snap.excluded_keys, vec!["deadbeef".to_string()]);
        assert_eq!(snap.record_headers, vec!["x-trace-id".to_string()]);

        // Hot-reload: store a new config, snapshot reflects it immediately.
        let mut new_cfg = PromptLogConfig::default();
        new_cfg.enabled = false;
        cfg_arc.store(Arc::new(new_cfg));
        let snap2 = query.config_snapshot();
        assert!(!snap2.enabled);
    }
}
