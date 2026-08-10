//! Offline replay tool — read JSONL files and push to OTLP.
//!
//! The runtime exporter and this replayer share `OtelExporter` and
//! `convert_entry_to_log_records`. The contract is: an entry written to disk
//! and then replayed produces a byte-identical `ExportLogsServiceRequest` to
//! the entry pushed live at runtime. If you change the conversion, change it
//! in one place.
//!
//! Usage from the example binary (`examples/replay.rs`):
//! ```sh
//! cargo run --example replay -- --dir /data/prompt_logs --otlp-endpoint http://collector:4318
//! cargo run --example replay -- --dir /data/prompt_logs/_no_team/kh --otlp-endpoint http://collector:4318
//! ```

use crate::config::OtlpConfig;
use crate::entry::{LogPhase, PromptLogEntry};
use crate::otlp::OtelExporter;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Replay JSONL files into an OTLP backend. Holds an `Arc<OtelExporter>` so
/// the same conversion / batching / retry logic is reused — this is not a
/// re-implementation, it's the runtime path pointed at file input.
pub struct OtelReplayer {
    exporter: Arc<OtelExporter>,
}

impl OtelReplayer {
    pub fn new(config: &OtlpConfig) -> Self {
        let exporter = OtelExporter::new(config);
        Self { exporter }
    }

    /// Spawns the periodic flush task — callers MUST call this if they want
    /// auto-flush. For a one-shot replay you can also just push all entries
    /// and then call `flush()` once at the end.
    pub fn spawn_flush(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        self.exporter.spawn_flush_task()
    }

    /// Read a single phase file (request.jsonl or response.jsonl) line by
    /// line and enqueue each entry. Returns the count of entries enqueued.
    /// Silently skips lines that fail to parse (warns).
    pub async fn replay_file(&self, path: &Path, _phase: LogPhase) -> Result<usize, ReplayError> {
        let data = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ReplayError::Read(path.to_path_buf(), e.to_string()))?;
        let mut count = 0;
        let max_attribute_bytes = self.exporter_max_attribute_bytes();
        for (i, line) in data.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<PromptLogEntry>(line) {
                Ok(entry) => {
                    self.exporter.enqueue(entry, max_attribute_bytes).await;
                    count += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        file = %path.display(),
                        line = i + 1,
                        error = %e,
                        "Skipping malformed entry in replay file"
                    );
                }
            }
        }
        Ok(count)
    }

    /// Walk a directory (recursively) and replay every `*.jsonl` and
    /// `*.jsonl.gz` file found. Files are processed sequentially to keep
    /// memory bounded — re-ingestion shouldn't compete with the live gateway
    /// for resources. Returns the total entry count.
    pub async fn replay_dir(&self, dir: &Path) -> Result<usize, ReplayError> {
        let mut walker = WalkFiles::new(dir)?;
        let mut total = 0;
        while let Some(path) = walker.next_file() {
            // Skip files that don't match our naming — leaves room for future
            // index files / sidecars.
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if !(name.starts_with("request_") || name.starts_with("response_")) {
                continue;
            }
            let phase = if name.starts_with("request_") {
                LogPhase::Request
            } else {
                LogPhase::Response
            };

            if name.ends_with(".jsonl") {
                total += self.replay_file(&path, phase).await.unwrap_or_else(|e| {
                    tracing::warn!(file = %path.display(), error = %e, "Skipping file");
                    0
                });
            } else if name.ends_with(".jsonl.gz") {
                let temp_path = match decompress_to_temp(&path).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(file = %path.display(), error = %e, "Skipping gz file");
                        continue;
                    }
                };
                total += self.replay_file(&temp_path, phase).await.unwrap_or_else(|e| {
                    tracing::warn!(file = %path.display(), error = %e, "Skipping decompressed file");
                    0
                });
                let _ = tokio::fs::remove_file(&temp_path).await;
            }
        }
        Ok(total)
    }

    /// Force a flush of whatever's queued. Call at the end of a replay run.
    pub async fn flush(&self) {
        self.exporter.flush().await;
    }

    fn exporter_max_attribute_bytes(&self) -> usize {
        self.exporter.max_attribute_bytes()
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ReplayError {
    #[error("failed to read {0}: {1}")]
    Read(PathBuf, String),
    #[error("failed to walk {0}: {1}")]
    Walk(PathBuf, String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Recursively yields the file paths under `dir`. Uses a Vec as a stack;
/// processes one directory at a time, draining all its entries before
/// recursing into the next directory. For a one-shot replay tool, sync walk
/// is fine.
struct WalkFiles {
    /// Directories still to walk. Pushed when we encounter them; popped when
    /// we exhaust the current directory's entries.
    stack: Vec<PathBuf>,
    /// Pending files at the current directory level. Drained one at a time by
    /// `next_file()` so we can interleave with deeper directory entries
    /// without losing them on the early-return path.
    pending_files: Vec<PathBuf>,
}

impl WalkFiles {
    fn new(root: &Path) -> Result<Self, ReplayError> {
        Ok(Self {
            stack: vec![root.to_path_buf()],
            pending_files: Vec::new(),
        })
    }

    fn next_file(&mut self) -> Option<PathBuf> {
        loop {
            // Hand out any pending files first.
            if let Some(f) = self.pending_files.pop() {
                return Some(f);
            }
            // No pending files — pop a directory and read its entries.
            let dir = self.stack.pop()?;
            let entries = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) => {
                    tracing::warn!(dir = %dir.display(), error = %e, "Skipping unreadable dir");
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    self.stack.push(path);
                } else {
                    self.pending_files.push(path);
                }
            }
            // Loop back: next iteration will pop from pending_files.
        }
    }
}

/// Decompress a `.jsonl.gz` to a temp file path. The temp file is owned by
/// the caller (which should delete it after replay).
async fn decompress_to_temp(gz_path: &Path) -> Result<PathBuf, std::io::Error> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let bytes = tokio::fs::read(gz_path).await?;
    let path = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
        let mut dec = GzDecoder::new(&bytes[..]);
        let mut out = String::new();
        dec.read_to_string(&mut out)?;
        let (file, path) = tempfile::Builder::new()
            .prefix("promptlog-replay-")
            .tempfile()?
            .keep()
            .map_err(std::io::Error::other)?;
        drop(file); // we have the path
        std::fs::write(&path, out)?;
        Ok(path)
    })
    .await
    .map_err(std::io::Error::other)??;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    #[tokio::test]
    async fn replay_file_enqueues_entries() {
        // We don't have a live collector — we just verify enqueue doesn't
        // panic and counts match.
        let cfg = OtlpConfig {
            enabled: true,
            endpoint: "http://localhost:9".to_string(), // unreachable but unused here
            ..OtlpConfig::default()
        };
        let replayer = OtelReplayer::new(&cfg);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let entry1 = PromptLogEntry::new_request(
            "r1",
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
        let entry2 = PromptLogEntry::new_request(
            "r2",
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
        let lines = vec![
            serde_json::to_string(&entry1).unwrap(),
            serde_json::to_string(&entry2).unwrap(),
            "".to_string(), // blank line, should be skipped
            "not-json".to_string(), // bad line, should be skipped
        ];
        fs::write(&path, lines.join("\n") + "\n").unwrap();

        let count = replayer
            .replay_file(&path, LogPhase::Request)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn replay_dir_walks_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let team_dir = dir.path().join("_no_team").join("kh");
        fs::create_dir_all(&team_dir).unwrap();

        let entry = PromptLogEntry::new_request(
            "r1",
            None,
            "kh",
            None,
            None,
            "gpt-4",
            "/v1/chat/completions",
            false,
            serde_json::json!({}),
            None,
            Some(HashMap::new()),
        );
        let line = serde_json::to_string(&entry).unwrap();
        fs::write(team_dir.join("request_000001.jsonl"), format!("{}\n", line)).unwrap();
        fs::write(team_dir.join("response_000001.jsonl"), format!("{}\n", line)).unwrap();
        // A file with the wrong prefix should be skipped.
        fs::write(team_dir.join("README.txt"), "hi\n").unwrap();

        let cfg = OtlpConfig {
            enabled: true,
            endpoint: "http://localhost:9".to_string(),
            ..OtlpConfig::default()
        };
        let replayer = OtelReplayer::new(&cfg);
        let count = replayer.replay_dir(dir.path()).await.unwrap();
        assert_eq!(count, 2);
    }
}
