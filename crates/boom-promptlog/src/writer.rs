use crate::config::{OtlpConfig, PromptLogConfig};
use crate::entry::{LogPhase, PromptLogEntry};
#[cfg(feature = "otlp")]
use crate::otlp::{ExporterStatusSnapshot, OtelExporter, ProbeResult};
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

    /// Read-only snapshot of the live OTLP exporter's state machine. Returns
    /// `None` when OTLP is not configured (no exporter in the ArcSwap) — the
    /// dashboard treats `None` as "disabled" (gray indicator).
    ///
    /// Takes an owned `Arc<OtelExporter>` out of the ArcSwap guard before
    /// returning, so callers can `.await` freely without holding the guard.
    #[cfg(feature = "otlp")]
    pub async fn otlp_status(&self) -> Option<ExporterStatusSnapshot> {
        let g = self.otlp.load();
        let Some(exporter) = g.as_ref() else {
            return None;
        };
        // Clone out of the ArcSwap guard before awaiting so we don't hold the
        // guard across the (potentially long) await.
        Some(exporter.clone().status_snapshot())
    }

    #[cfg(not(feature = "otlp"))]
    pub async fn otlp_status(&self) -> Option<()> {
        None
    }

    /// Manual probe of the live OTLP exporter. Drives the state machine: on
    /// success, transitions Offline → Online; on failure, records a probe
    /// failure (but does NOT drive Online → Offline — only repeated flush
    /// failures do that).
    ///
    /// Used by the dashboard's manual "Probe now" action when the operator
    /// wants to attempt recovery before the next periodic tick. The periodic
    /// tick calls `run_probe_cycle` internally; this is the user-triggered
    /// counterpart that returns a `ProbeResult` for UI feedback.
    #[cfg(feature = "otlp")]
    pub async fn probe_otlp(&self) -> Option<ProbeResult> {
        let g = self.otlp.load();
        let Some(exporter) = g.as_ref() else {
            return None;
        };
        // Clone out of the ArcSwap guard before awaiting so we don't hold
        // the guard across the network probe.
        Some(exporter.clone().probe().await)
    }

    #[cfg(not(feature = "otlp"))]
    pub async fn probe_otlp(&self) -> Option<()> {
        None
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

/// State for one open log file.
struct OpenFile {
    file: tokio::fs::File,
    size: u64,
}

/// One aligned generation for a key directory: the request and response
/// files share the same sequence number and rotate together, so a request
/// entry and the response entries of the same generation always land in
/// files with the same `{phase}_{seq:06}` suffix. Pairing them then only
/// ever needs to look inside one seq (at most two adjacent ones, when
/// rotation falls between a request and its response).
///
/// Phase files open lazily: a generation resumed after a restart (or a phase
/// not yet written) stays `None` until its first entry arrives.
struct Generation {
    seq: u64,
    request: Option<OpenFile>,
    response: Option<OpenFile>,
}

impl Generation {
    fn phase(&self, phase: LogPhase) -> &Option<OpenFile> {
        match phase {
            LogPhase::Request => &self.request,
            LogPhase::Response => &self.response,
        }
    }

    fn phase_mut(&mut self, phase: LogPhase) -> &mut Option<OpenFile> {
        match phase {
            LogPhase::Request => &mut self.request,
            LogPhase::Response => &mut self.response,
        }
    }
}

fn phase_file_name(phase: &str, seq: u64) -> String {
    format!("{}_{:06}.jsonl", phase, seq)
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
    // "{dir}/{team_alias}/{key_hash}" → aligned generation. The key carries
    // the full path (including dir) so a runtime `dir` change naturally
    // starts fresh generations in the new directory instead of rotating
    // files under paths that no longer match the open handles.
    let mut open_gens: HashMap<PathBuf, Generation> = HashMap::new();

    while let Some(entry) = receiver.recv().await {
        let cfg = config.load();
        let base_dir = PathBuf::from(&cfg.dir);
        // Clamp to >=1MB: an explicit `max_file_size_mb: 0` would satisfy the
        // rotation predicate on every write and rotate once per line (one
        // file + one compress spawn per request).
        let max_bytes = cfg.max_file_size_mb.max(1) * 1024 * 1024;
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

        // Get or create the aligned generation for this team/key. On first
        // sight of the directory, scan it to decide where to (re)start and
        // compress any stale .jsonl files left over from a crash.
        let gen = match open_gens.entry(key_dir.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let (seq, stale) = scan_generation_state(&key_dir).await;
                if !stale.is_empty() {
                    tokio::spawn(async move {
                        for p in stale {
                            if let Err(err) = compress_file(&p).await {
                                tracing::warn!("Failed to compress stale file {:?}: {}", p, err);
                            }
                        }
                    });
                }
                e.insert(Generation { seq, request: None, response: None })
            }
        };

        // Lazily open this phase's file at the generation seq. On a resumed
        // generation the file already exists and metadata restores its size,
        // so the threshold keeps counting from where the last run left off.
        if gen.phase_mut(entry.phase).is_none() {
            let path = key_dir.join(phase_file_name(phase_name, gen.seq));
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
                    *gen.phase_mut(entry.phase) = Some(OpenFile { file, size });
                }
                Err(err) => {
                    tracing::error!("Failed to open prompt log file {:?}: {}", path, err);
                    continue;
                }
            }
        }

        // Rotate when this write would overflow the phase file. The whole
        // generation rotates together — both phases — so request/response
        // seq suffixes stay aligned. Checking only the phase being written is
        // sufficient: files only grow on writes, and every write checks
        // itself first, so whichever phase crosses the threshold first seals
        // the generation.
        let should_rotate = {
            let of = gen.phase(entry.phase).as_ref().unwrap();
            of.size > 0 && of.size + line_bytes + 1 > max_bytes
        };
        if should_rotate {
            let old_seq = gen.seq;
            let new_seq = old_seq + 1;
            let new_path = key_dir.join(phase_file_name(phase_name, new_seq));
            match tokio::fs::File::create(&new_path).await {
                Ok(file) => {
                    tracing::info!(
                        path = %new_path.display(),
                        key_hash = %entry.key_hash,
                        phase = phase_name,
                        old_seq,
                        new_seq,
                        "Rotated prompt log generation"
                    );
                    // Seal both phases: drop open handles so no later write
                    // targets old_seq; the other phase reopens lazily at
                    // new_seq on its next entry.
                    gen.request = None;
                    gen.response = None;
                    gen.seq = new_seq;
                    *gen.phase_mut(entry.phase) = Some(OpenFile { file, size: 0 });
                    // Compress both sealed files that exist on disk — the
                    // phase not written this generation (e.g. resumed from a
                    // previous run with no handle open yet) may have none.
                    let old_req = key_dir.join(phase_file_name("request", old_seq));
                    let old_resp = key_dir.join(phase_file_name("response", old_seq));
                    tokio::spawn(async move {
                        for p in [old_req, old_resp] {
                            if matches!(tokio::fs::try_exists(&p).await, Ok(true)) {
                                if let Err(e) = compress_file(&p).await {
                                    tracing::warn!("Failed to compress {:?}: {}", p, e);
                                }
                            }
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
        let of = gen.phase_mut(entry.phase).as_mut().unwrap();
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

/// Scan a key directory and decide where to (re)start writing.
///
/// Files are `{phase}_{seq:06}.jsonl` / `{phase}_{seq:06}.jsonl.gz` with
/// request and response sharing one sequence space (a "generation"). Returns
/// `(start_seq, stale_paths)`:
///
/// - If the newest uncompressed generation is aligned — both phases agree on
///   the max `.jsonl` seq, or only one phase has files — and neither phase
///   has a `.gz` at that seq, writing resumes at that seq: a restart
///   mid-generation appends to the half-full pair instead of abandoning it
///   as an uncompressed orphan until the next restart.
/// - Otherwise (misaligned leftovers from pre-alignment runs, or a `.gz`
///   twin already present at the newest seq) a fresh generation starts at
///   `overall_max + 1`, and every leftover `.jsonl` without a `.gz` twin is
///   returned as stale for compression — including the previous newest,
///   which the old `s < newest_jsonl` filter always skipped.
async fn scan_generation_state(dir: &std::path::Path) -> (u64, Vec<PathBuf>) {
    let mut jsonl: Vec<(&'static str, u64)> = Vec::new();
    let mut gz: std::collections::HashSet<(&'static str, u64)> = std::collections::HashSet::new();

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return (1, Vec::new()),
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        for phase in ["request", "response"] {
            let Some(rest) = name.strip_prefix(&format!("{}_", phase)) else {
                continue;
            };
            if let Some(seq_str) = rest.strip_suffix(".jsonl") {
                if let Ok(seq) = seq_str.parse::<u64>() {
                    jsonl.push((phase, seq));
                }
            } else if let Some(seq_str) = rest.strip_suffix(".jsonl.gz") {
                if let Ok(seq) = seq_str.parse::<u64>() {
                    gz.insert((phase, seq));
                }
            }
        }
    }

    let (start, stale) = plan_generation_start(&jsonl, &gz);
    let stale_paths = stale
        .into_iter()
        .map(|(phase, seq)| dir.join(phase_file_name(phase, seq)))
        .collect();
    (start, stale_paths)
}

/// Pure decision core of [`scan_generation_state`], split out for testing.
/// Takes `(phase, seq)` pairs of uncompressed files and the set of files
/// that already have a `.gz` twin; returns the seq to write at and the
/// uncompressed `(phase, seq)` pairs to compress.
fn plan_generation_start(
    jsonl: &[(&'static str, u64)],
    gz: &std::collections::HashSet<(&'static str, u64)>,
) -> (u64, Vec<(&'static str, u64)>) {
    let phase_max = |p: &str| {
        jsonl
            .iter()
            .filter(|(ph, _)| *ph == p)
            .map(|(_, s)| *s)
            .max()
    };
    let overall_max = jsonl
        .iter()
        .map(|(_, s)| *s)
        .chain(gz.iter().map(|(_, s)| *s))
        .max()
        .unwrap_or(0);

    // Resume only when the newest jsonl generation is aligned and no phase
    // already has a compressed twin at that seq — a crash between writing
    // the .gz and deleting the .jsonl would otherwise duplicate content if
    // we appended to the jsonl.
    let aligned_seq = match (phase_max("request"), phase_max("response")) {
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(a), None) | (None, Some(a)) => Some(a),
        _ => None,
    };
    let start = match aligned_seq {
        Some(s) if !gz.iter().any(|(_, gs)| *gs == s) => s,
        _ => overall_max + 1,
    };

    let stale = jsonl
        .iter()
        .copied()
        .filter(|&(_, s)| s != start)
        .filter(|&(p, s)| !gz.contains(&(p, s)))
        .collect();

    (start, stale)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn gzset(pairs: &[(&'static str, u64)]) -> std::collections::HashSet<(&'static str, u64)> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn empty_dir_starts_at_one() {
        let (start, stale) = plan_generation_start(&[], &gzset(&[]));
        assert_eq!(start, 1);
        assert!(stale.is_empty());
    }

    #[test]
    fn aligned_generation_is_resumed() {
        let jsonl = [("request", 3), ("response", 3), ("request", 2), ("response", 1)];
        let (start, stale) = plan_generation_start(&jsonl, &gzset(&[("request", 1), ("request", 2)]));
        assert_eq!(start, 3);
        // seq 1/2 are old generations: request sides already gzipped,
        // response seq 1 has no twin yet → the only stale file.
        assert_eq!(stale, vec![("response", 1)]);
    }

    #[test]
    fn single_phase_generation_is_resumed() {
        let (start, stale) = plan_generation_start(&[("request", 7)], &gzset(&[]));
        assert_eq!(start, 7);
        assert!(stale.is_empty());
    }

    #[test]
    fn misaligned_generation_starts_fresh_and_compresses_all() {
        // Pre-alignment leftovers: request stopped at 7, response at 9.
        let (start, stale) = plan_generation_start(&[("request", 7), ("response", 9)], &gzset(&[]));
        assert_eq!(start, 10);
        assert_eq!(stale, vec![("request", 7), ("response", 9)]);
    }

    #[test]
    fn gz_twin_at_newest_prevents_resume() {
        // Crash between writing the .gz and deleting the .jsonl: resuming
        // would append to a file whose content is already in a .gz twin.
        let (start, stale) = plan_generation_start(
            &[("request", 5), ("response", 5)],
            &gzset(&[("response", 5)]),
        );
        assert_eq!(start, 6);
        // response 5 is skipped (twin exists — healed state); request 5 is
        // recompressed, which overwrites nothing but removes the jsonl.
        assert_eq!(stale, vec![("request", 5)]);
    }

    #[test]
    fn gz_only_dir_starts_after_max() {
        let (start, stale) = plan_generation_start(&[], &gzset(&[("request", 4), ("response", 4)]));
        assert_eq!(start, 5);
        assert!(stale.is_empty());
    }

    #[tokio::test]
    async fn lockstep_rotation_keeps_request_response_aligned() {
        use crate::config::PromptLogConfig;
        use std::time::{Duration, Instant};

        let tmp = tempfile::tempdir().unwrap();
        let cfg = PromptLogConfig {
            dir: tmp.path().to_string_lossy().into_owned(),
            max_file_size_mb: 1,
            ..Default::default()
        };
        let writer = PromptLogWriter::spawn(cfg);

        let entry = |id: &str, phase: LogPhase, payload: usize| {
            let body = Arc::new(serde_json::Value::String("x".repeat(payload)));
            let mut e = PromptLogEntry::new_request(
                id,
                None,
                "keyhash",
                Some("alice"),
                Some("team1"),
                "gpt-4o",
                "/v1/chat/completions",
                false,
                body,
                Some("127.0.0.1"),
                None,
            );
            e.phase = phase;
            e
        };

        writer.send(entry("r1", LogPhase::Request, 10));
        writer.send(entry("r1", LogPhase::Response, 10));
        // ~1.2MB request line on a non-empty file crosses the 1MB threshold
        // and must rotate the WHOLE generation to seq 2 — including the
        // half-empty response file.
        writer.send(entry("r2", LogPhase::Request, 1_200_000));
        writer.send(entry("r2", LogPhase::Response, 10));

        let key_dir = tmp.path().join("team1").join("keyhash");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(content) = tokio::fs::read_to_string(key_dir.join("response_000002.jsonl")).await {
                if content.contains("\"r2\"") {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for generation-2 files to appear"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Generation-1 files were sealed by the rotation and are compressed
        // asynchronously — accept either form, but both phases must survive.
        for phase in ["request", "response"] {
            let jsonl = tokio::fs::try_exists(key_dir.join(format!("{}_000001.jsonl", phase)))
                .await
                .unwrap();
            let gz = tokio::fs::try_exists(key_dir.join(format!("{}_000001.jsonl.gz", phase)))
                .await
                .unwrap();
            assert!(
                jsonl || gz,
                "generation-1 {phase} file must survive in some form"
            );
        }
        let req2 = tokio::fs::read_to_string(key_dir.join("request_000002.jsonl"))
            .await
            .unwrap();
        assert!(req2.contains("\"r2\""));
        let resp2 = tokio::fs::read_to_string(key_dir.join("response_000002.jsonl"))
            .await
            .unwrap();
        assert!(resp2.contains("\"r2\""));
    }
}
