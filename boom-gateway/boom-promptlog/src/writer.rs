use crate::config::PromptLogConfig;
use crate::entry::PromptLogEntry;
use arc_swap::ArcSwap;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// Handle to the background prompt log writer.
///
/// Clone-safe handle that checks `should_capture()` against the live config.
/// The actual file I/O happens in a background tokio task.
#[derive(Clone)]
pub struct PromptLogWriter {
    config: Arc<ArcSwap<PromptLogConfig>>,
    sender: mpsc::UnboundedSender<PromptLogEntry>,
}

impl PromptLogWriter {
    /// Spawn the background writer task and return (writer_handle, sender).
    ///
    /// Boom-main keeps the writer in AppState and passes the sender to route handlers.
    /// The sender can also be cloned freely.
    pub fn spawn(config: PromptLogConfig) -> Self {
        let config = Arc::new(ArcSwap::from_pointee(config));
        let (sender, receiver) = mpsc::unbounded_channel();

        let config_clone = config.clone();
        tokio::spawn(async move {
            background_writer(receiver, config_clone).await;
        });

        Self { config, sender }
    }

    /// Check if this key/team should be captured.
    /// Call this BEFORE cloning the request body to avoid unnecessary work.
    pub fn should_capture(&self, key_hash: &str, team_id: Option<&str>) -> bool {
        self.config.load().should_capture(key_hash, team_id)
    }

    /// Get a clone of the sender for passing to stream wrappers.
    pub fn sender(&self) -> mpsc::UnboundedSender<PromptLogEntry> {
        self.sender.clone()
    }

    /// Send an entry to the background writer (non-blocking, fire-and-forget).
    pub fn send(&self, entry: PromptLogEntry) {
        if let Err(e) = self.sender.send(entry) {
            tracing::warn!("Prompt log channel closed, dropping entry: {}", e.0.request_id);
        }
    }

    /// Update config at runtime (hot-reload).
    pub fn update_config(&self, new_config: PromptLogConfig) {
        self.config.store(Arc::new(new_config));
    }

    /// Read a snapshot of the current config.
    pub fn config(&self) -> PromptLogConfig {
        self.config.load().as_ref().clone()
    }
}

/// State for an open log file.
struct OpenFile {
    file: tokio::fs::File,
    size: u64,
    sequence: u64,
}

/// Background writer loop.
async fn background_writer(
    mut receiver: mpsc::UnboundedReceiver<PromptLogEntry>,
    config: Arc<ArcSwap<PromptLogConfig>>,
) {
    // "{team_alias}/{key_hash}" → open file state
    let mut open_files: HashMap<String, OpenFile> = HashMap::new();

    while let Some(entry) = receiver.recv().await {
        let cfg = config.load();
        let base_dir = PathBuf::from(&cfg.dir);
        let max_bytes = cfg.max_file_size_mb * 1024 * 1024;
        drop(cfg); // release config guard

        // Directory layout: {dir}/{team_alias}/{key_hash}/
        // If no team_alias, use "_no_team" as fallback.
        let team_dir_name = entry.team_alias.as_deref().unwrap_or("_no_team");
        let key_dir = base_dir.join(team_dir_name).join(&entry.key_hash);
        // Map key for open_files: use team_alias/key_hash as composite key.
        let file_key = format!("{}/{}", team_dir_name, entry.key_hash);

        // Ensure directory exists.
        if let Err(e) = tokio::fs::create_dir_all(&key_dir).await {
            tracing::error!("Failed to create prompt log dir {:?}: {}", key_dir, e);
            continue;
        }

        // Serialize entry to a single JSON line.
        let json_line = match serde_json::to_string(&entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to serialize prompt log entry: {}", e);
                continue;
            }
        };
        let line_bytes = json_line.len() as u64;

        // Get or create open file for this team/key.
        let of = match open_files.entry(file_key.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                // Scan directory for existing files to find max sequence.
                let (seq, stale) = find_max_sequence_and_stale(&key_dir).await;
                let path = key_dir.join(format!("log_{:06}.jsonl", seq));

                // Compress stale .jsonl files left over from a crash.
                if !stale.is_empty() {
                    let stale_paths: Vec<PathBuf> = stale.iter()
                        .map(|s| key_dir.join(format!("log_{:06}.jsonl", s)))
                        .collect();
                    tokio::spawn(async move {
                        for p in stale_paths {
                            if let Err(e) = compress_file(&p).await {
                                tracing::warn!("Failed to compress stale file {:?}: {}", p, e);
                            }
                        }
                    });
                }

                match tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await
                {
                    Ok(file) => {
                        // Get current file size for append mode.
                        let size = match tokio::fs::metadata(&path).await {
                            Ok(m) => m.len(),
                            Err(_) => 0,
                        };
                        e.insert(OpenFile { file, size, sequence: seq })
                    }
                    Err(err) => {
                        tracing::error!("Failed to open prompt log file {:?}: {}", path, err);
                        continue;
                    }
                }
            }
        };

        // Check if writing this line would exceed max file size.
        // If current file is non-empty and would overflow, rotate to a new file.
        if of.size > 0 && of.size + line_bytes > max_bytes {
            let old_path = key_dir.join(format!("log_{:06}.jsonl", of.sequence));
            let new_seq = of.sequence + 1;
            let new_path = key_dir.join(format!("log_{:06}.jsonl", new_seq));
            match tokio::fs::File::create(&new_path).await {
                Ok(file) => {
                    tracing::info!(
                        path = %new_path.display(),
                        key_hash = %entry.key_hash,
                        "Rotated prompt log file"
                    );
                    *of = OpenFile { file, size: 0, sequence: new_seq };
                    // Compress old file in background — fire-and-forget.
                    tokio::spawn(async move {
                        if let Err(e) = compress_file(&old_path).await {
                            tracing::warn!("Failed to compress {:?}: {}", old_path, e);
                        }
                    });
                }
                Err(err) => {
                    tracing::error!("Failed to create new prompt log file {:?}: {}", new_path, err);
                    continue;
                }
            }
        }

        // Write the line.
        if let Err(e) = of.file.write_all(json_line.as_bytes()).await {
            tracing::error!("Failed to write prompt log entry: {}", e);
        }
        if let Err(e) = of.file.write_all(b"\n").await {
            tracing::error!("Failed to write prompt log newline: {}", e);
        }
        of.size += line_bytes + 1; // +1 for newline
    }

    tracing::info!("Prompt log writer channel closed, exiting background task");
}

/// Scan a directory for existing log files.
/// Returns (max_sequence_to_use, stale_uncompressed_sequences).
///
/// - max_sequence_to_use: the highest sequence found + 1 (next file to write).
///   If only `.jsonl` files exist (no `.gz`), the max `.jsonl` seq is the current
///   file, so we return max+1. If a `.gz` exists with same seq, that `.jsonl` is stale.
/// - stale_uncompressed_sequences: sequences that have `.jsonl` but no matching `.gz`
///   and are NOT the newest `.jsonl` file (left over from a crash).
async fn find_max_sequence_and_stale(dir: &std::path::Path) -> (u64, Vec<u64>) {
    let mut jsonl_seqs: Vec<u64> = Vec::new();
    let mut gz_seqs: std::collections::HashSet<u64> = std::collections::HashSet::new();

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return (1, Vec::new()),
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(seq_str) = name_str
            .strip_prefix("log_")
            .and_then(|s| s.strip_suffix(".jsonl"))
        {
            if let Ok(seq) = seq_str.parse::<u64>() {
                jsonl_seqs.push(seq);
            }
        } else if let Some(seq_str) = name_str
            .strip_prefix("log_")
            .and_then(|s| s.strip_suffix(".jsonl.gz"))
        {
            if let Ok(seq) = seq_str.parse::<u64>() {
                gz_seqs.insert(seq);
            }
        }
    }

    let overall_max = jsonl_seqs.iter().copied().chain(gz_seqs.iter().copied()).max().unwrap_or(0);
    let next_seq = overall_max + 1;

    // Stale: .jsonl files that are NOT the newest and don't have a .gz counterpart.
    let newest_jsonl = jsonl_seqs.iter().copied().max().unwrap_or(0);
    let stale: Vec<u64> = jsonl_seqs
        .into_iter()
        .filter(|&s| s < newest_jsonl && !gz_seqs.contains(&s))
        .collect();

    (next_seq, stale)
}

/// Compress a file to `.gz` and delete the original on success.
async fn compress_file(path: &std::path::Path) -> std::io::Result<()> {
    let data = tokio::fs::read(path).await?;
    let gz_path = PathBuf::from(format!("{}.gz", path.display()));

    // Compress in a blocking task to avoid starving the async runtime.
    let gz_path_clone = gz_path.clone();
    let compressed = tokio::task::spawn_blocking(move || {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        use std::io::Write;
        encoder.write_all(&data)?;
        encoder.finish()
    })
    .await
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))??;

    tokio::fs::write(&gz_path_clone, &compressed).await?;
    tokio::fs::remove_file(path).await?;
    tracing::info!(
        original = %path.display(),
        compressed = %gz_path_clone.display(),
        "Compressed prompt log file"
    );
    Ok(())
}
