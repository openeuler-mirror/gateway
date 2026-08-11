use crate::config::{OtlpConfig, PromptLogConfig};
use crate::entry::{LogPhase, PromptLogEntry};
#[cfg(feature = "otlp")]
use crate::otlp::OtelExporter;
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
///
/// The OTLP exporter is held in an `Arc<ArcSwap<Option<Arc<OtelExporter>>>>`
/// so it can be hot-swapped at runtime via `replace_otlp()` — the background
/// writer reads `otlp.load()` on every entry, so the new exporter takes
/// effect immediately for new entries after a reload.
#[derive(Clone)]
pub struct PromptLogWriter {
    config: Arc<ArcSwap<PromptLogConfig>>,
    sender: mpsc::UnboundedSender<PromptLogEntry>,
    #[cfg(feature = "otlp")]
    otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>>,
    #[cfg(feature = "otlp")]
    flush_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl PromptLogWriter {
    /// Spawn the background writer with no OTLP exporter — local JSONL only.
    /// Available regardless of feature gate. When the `otlp` feature is on
    /// but the caller doesn't have a configured exporter, this is the right
    /// entrypoint: it just passes `None` through to the inner writer.
    pub fn spawn(config: PromptLogConfig) -> Self {
        let config = Arc::new(ArcSwap::from_pointee(config));
        let (sender, receiver) = mpsc::unbounded_channel();
        let config_clone = config.clone();
        #[cfg(feature = "otlp")]
        let otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>> =
            Arc::new(ArcSwap::from_pointee(None));
        #[cfg(feature = "otlp")]
        let otlp_clone = otlp.clone();
        #[cfg(feature = "otlp")]
        let flush_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
            Arc::new(std::sync::Mutex::new(None));
        tokio::spawn(async move {
            #[cfg(feature = "otlp")]
            background_writer(receiver, config_clone, otlp_clone).await;
            #[cfg(not(feature = "otlp"))]
            background_writer(receiver, config_clone).await;
        });
        Self {
            config,
            sender,
            #[cfg(feature = "otlp")]
            otlp,
            #[cfg(feature = "otlp")]
            flush_handle,
        }
    }

    /// Spawn the background writer with an OTLP exporter. Only available when
    /// the `otlp` feature is on. Caller also typically calls
    /// `exporter.spawn_flush_task_to_handle()` to start periodic flushes
    /// (writes the JoinHandle into the writer's `flush_handle` slot so
    /// `replace_otlp` can abort it on reload).
    #[cfg(feature = "otlp")]
    pub fn spawn_with_otlp(config: PromptLogConfig, exporter: Arc<OtelExporter>) -> Self {
        let config = Arc::new(ArcSwap::from_pointee(config));
        let (sender, receiver) = mpsc::unbounded_channel();
        let config_clone = config.clone();
        let flush_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let flush_handle_for_task = flush_handle.clone();
        // Spawn the flush task into the writer's handle slot.
        exporter.spawn_flush_task_to_handle(flush_handle_for_task.clone());
        let otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>> =
            Arc::new(ArcSwap::from_pointee(Some(exporter.clone())));
        let otlp_clone = otlp.clone();
        tokio::spawn(async move {
            background_writer(receiver, config_clone, otlp_clone).await;
        });
        Self {
            config,
            sender,
            otlp,
            flush_handle,
        }
    }

    #[cfg(not(feature = "otlp"))]
    pub fn spawn_with_otlp(config: PromptLogConfig, _exporter: ()) -> Self {
        Self::spawn(config)
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

    /// Update config at runtime (hot-reload). Updates the runtime config
    /// seen by the background writer — affects local sink (dir, max_size,
    /// capture flag, excluded_*, record_headers) and the `otlp.enabled`
    /// toggle. Does NOT touch the running exporter (endpoint / batch_size /
    /// timeout / etc.); use `replace_otlp` for those.
    pub fn update_config(&self, new_config: PromptLogConfig) {
        self.config.store(Arc::new(new_config));
    }

    /// Hot-swap the OTLP exporter. Aborts the old flush task, runs a
    /// best-effort final flush on the old exporter to drain its queue,
    /// constructs a new exporter from `new_otlp`, stores it, and spawns a
    /// fresh flush task. Called from boom-main reload path when the `otlp`
    /// sub-config differs from the running exporter's config.
    ///
    /// The best-effort flush is bounded by `new_otlp.timeout_secs + 2` so
    /// a slow/unreachable old endpoint can't stall the reload. Entries that
    /// arrive during the swap may briefly hit the old exporter (race window
    /// between abort and store) — those entries stay in the old queue and
    /// are dropped on overflow. Local JSONL is unaffected.
    #[cfg(feature = "otlp")]
    pub async fn replace_otlp(&self, new_otlp: &OtlpConfig) {
        // 1. Abort the old flush task. The task may be mid-.await (HTTP
        //    in-flight); abort() cancels cooperatively at the next .await.
        if let Some(h) = self.flush_handle.lock().unwrap().take() {
            h.abort();
        }
        // 2. Best-effort final flush on the old exporter to drain its
        //    in-memory batch. Bounded so a dead endpoint doesn't stall
        //    reload. Reuse the *new* timeout as the bound — it's a hint,
        //    not a contract.
        if let Some(old) = self.otlp.load().as_ref() {
            let bound = std::time::Duration::from_secs(new_otlp.timeout_secs.max(1) + 2);
            let _ = tokio::time::timeout(bound, old.flush()).await;
        }
        // 3. Construct the new exporter + spawn its flush task into the
        //    shared handle slot. The ArcSwap store is atomic — the
        //    background writer sees the new exporter on its next entry.
        let new = OtelExporter::new(new_otlp);
        new.spawn_flush_task_to_handle(self.flush_handle.clone());
        self.otlp.store(Arc::new(Some(new)));
        tracing::info!(
            endpoint = %new_otlp.endpoint,
            batch_size = new_otlp.batch_size,
            flush_interval_secs = new_otlp.flush_interval_secs,
            "OTLP exporter hot-swapped via reload"
        );
    }

    /// Read a snapshot of the current config.
    pub fn config(&self) -> PromptLogConfig {
        self.config.load().as_ref().clone()
    }

    /// Read-only config handle, for use by `FilePromptLogQuery`.
    pub fn config_handle(&self) -> Arc<ArcSwap<PromptLogConfig>> {
        self.config.clone()
    }

    /// Trigger final flush of any OTLP batch and wait for it to drain.
    /// Called once during shutdown — best-effort, never blocks the gateway
    /// exit longer than the configured timeout. Reads the current exporter
    /// via the ArcSwap so a `replace_otlp` mid-shutdown is honored.
    #[cfg(feature = "otlp")]
    pub async fn shutdown_flush(&self) {
        if let Some(exporter) = self.otlp.load().as_ref() {
            exporter.flush().await;
        }
        if let Some(h) = self.flush_handle.lock().unwrap().take() {
            h.abort();
        }
    }

    #[cfg(not(feature = "otlp"))]
    pub async fn shutdown_flush(&self) {}
}

/// State for an open log file, keyed by `(team, key_hash, phase)`.
struct OpenFile {
    file: tokio::fs::File,
    size: u64,
    sequence: u64,
}

/// Background writer loop. Each entry is written to one of two phase files
/// (`request.jsonl` / `response.jsonl`) under `{dir}/{team}/{key_hash}/`.
/// When the OTLP feature is on, a copy is also handed to the exporter — the
/// local file sink is never blocked by the remote backend.
#[cfg(feature = "otlp")]
async fn background_writer(
    receiver: mpsc::UnboundedReceiver<PromptLogEntry>,
    config: Arc<ArcSwap<PromptLogConfig>>,
    otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>>,
) {
    background_writer_impl(receiver, config, otlp).await
}

#[cfg(not(feature = "otlp"))]
async fn background_writer(
    receiver: mpsc::UnboundedReceiver<PromptLogEntry>,
    config: Arc<ArcSwap<PromptLogConfig>>,
) {
    background_writer_impl(receiver, config, ()).await
}

/// Background writer body. The third argument is `Arc<ArcSwap<Option<Arc<OtelExporter>>>>`
/// when the otlp feature is on, or `()` when off. Reading `otlp.load()` on
/// every entry lets `replace_otlp` hot-swap the exporter mid-run — new
/// entries are enqueued to whatever exporter is current.
#[cfg_attr(feature = "otlp", allow(clippy::type_complexity))]
async fn background_writer_impl(
    mut receiver: mpsc::UnboundedReceiver<PromptLogEntry>,
    config: Arc<ArcSwap<PromptLogConfig>>,
    #[cfg(feature = "otlp")] otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>>,
    #[cfg(not(feature = "otlp"))] _otlp: (),
) {
    // "{team_alias}/{key_hash}/{phase}" → open file state
    let mut open_files: HashMap<String, OpenFile> = HashMap::new();

    while let Some(entry) = receiver.recv().await {
        let cfg = config.load();
        let base_dir = PathBuf::from(&cfg.dir);
        let max_bytes = cfg.max_file_size_mb * 1024 * 1024;
        #[cfg(feature = "otlp")]
        let otlp_enabled = cfg.otlp.enabled;
        drop(cfg); // release config guard

        // Fork to OTLP exporter first (under feature gate). Failure here must
        // never skip the local file write — local JSONL is the source of truth.
        // Respect the live `otlp.enabled` toggle: the exporter may have been
        // disabled via dashboard — skip enqueuing in that case so the in-memory
        // queue drains instead of accumulating entries nobody will pick up.
        //
        // `otlp.load()` is read every entry, so a `replace_otlp` mid-run is
        // picked up immediately — new entries go to the new exporter. The
        // `max_attribute_bytes` is no longer passed here (the previous
        // parameter was silently ignored); the exporter uses its own frozen
        // value, which is the current config value because replace_otlp
        // rebuilds the exporter from the new config.
        #[cfg(feature = "otlp")]
        if otlp_enabled {
            if let Some(exporter) = otlp.load().as_ref() {
                exporter.enqueue(entry.clone()).await;
            }
        }

        // Directory layout: {dir}/{team_alias}/{key_hash}/{phase}.jsonl
        // If no team_alias, use "_no_team" as fallback.
        let team_dir_name = entry.team_alias.as_deref().unwrap_or("_no_team");
        let phase_name = match entry.phase {
            LogPhase::Request => "request",
            LogPhase::Response => "response",
        };
        let key_dir = base_dir.join(team_dir_name).join(&entry.key_hash);
        // Map key for open_files: use team_alias/key_hash/phase as composite key.
        let file_key = format!("{}/{}/{}", team_dir_name, entry.key_hash, phase_name);

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

        // Get or create open file for this team/key/phase.
        let of = match open_files.entry(file_key.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                // Scan directory for existing files to find max sequence.
                let (seq, stale) = find_max_sequence_and_stale(&key_dir, phase_name).await;
                let path = key_dir.join(format!("{}_{:06}.jsonl", phase_name, seq));

                // Compress stale .jsonl files left over from a crash.
                if !stale.is_empty() {
                    let stale_paths: Vec<PathBuf> = stale
                        .iter()
                        .map(|s| key_dir.join(format!("{}_{:06}.jsonl", phase_name, s)))
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
            let old_path = key_dir.join(format!("{}_{:06}.jsonl", phase_name, of.sequence));
            let new_seq = of.sequence + 1;
            let new_path = key_dir.join(format!("{}_{:06}.jsonl", phase_name, new_seq));
            match tokio::fs::File::create(&new_path).await {
                Ok(file) => {
                    tracing::info!(
                        path = %new_path.display(),
                        key_hash = %entry.key_hash,
                        phase = phase_name,
                        "Rotated prompt log file"
                    );
                    *of = OpenFile { file, size: 0, sequence: new_seq };
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

/// Scan a directory for existing log files of the given phase.
/// Returns (max_sequence_to_use, stale_uncompressed_sequences).
///
/// Files are named `{phase}_{seq:06}.jsonl` or `{phase}_{seq:06}.jsonl.gz`.
async fn find_max_sequence_and_stale(dir: &std::path::Path, phase: &str) -> (u64, Vec<u64>) {
    let prefix = format!("{}_", phase);
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
            .strip_prefix(prefix.as_str())
            .and_then(|s| s.strip_suffix(".jsonl"))
        {
            if let Ok(seq) = seq_str.parse::<u64>() {
                jsonl_seqs.push(seq);
            }
        } else if let Some(seq_str) = name_str
            .strip_prefix(prefix.as_str())
            .and_then(|s| s.strip_suffix(".jsonl.gz"))
        {
            if let Ok(seq) = seq_str.parse::<u64>() {
                gz_seqs.insert(seq);
            }
        }
    }

    let overall_max = jsonl_seqs.iter().copied().chain(gz_seqs.iter().copied()).max().unwrap_or(0);
    let next_seq = overall_max + 1;

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

    let gz_path_clone = gz_path.clone();
    let compressed = tokio::task::spawn_blocking(move || {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        use std::io::Write;
        encoder.write_all(&data)?;
        encoder.finish()
    })
    .await
    .map_err(std::io::Error::other)??;

    tokio::fs::write(&gz_path_clone, &compressed).await?;
    tokio::fs::remove_file(path).await?;
    tracing::info!(
        original = %path.display(),
        compressed = %gz_path_clone.display(),
        "Compressed prompt log file"
    );
    Ok(())
}
