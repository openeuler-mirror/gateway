//! Background writer — the producer-facing pipeline around `SegmentStore`.
//!
//! No-loss backpressure chain (per request):
//!
//! 1. **Memory channel** with a built-in byte budget (256MB). Producers
//!    never block: `try_send` atomically claims budget, sends, and the
//!    writer returns the budget once the entry is appended to the
//!    segment's in-memory tail.
//! 2. **Spill file** (`{dir}/spill.log`, built-in 2GB budget): when
//!    the channel is full or over budget (writer stalled, slow disk), the
//!    producer appends the record frame straight to disk — sequential I/O,
//!    tens of µs. The writer drains the spill back into the store whenever
//!    the channel goes idle.
//! 3. **Drop + count + warn** — only when the spill itself cannot take
//!    more (disk full / IO error / budget exhausted).
//!
//! Durability: group commit — the tick fsyncs at most once per
//! [`BUILTIN_FSYNC_INTERVAL_MS`], so power loss costs at most the last
//! interval. Sealing always fsyncs.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use tokio::sync::{mpsc, oneshot};

use crate::config::{
    BUILTIN_CHANNEL_MAX_BYTES, BUILTIN_FSYNC_INTERVAL_MS, BUILTIN_RETENTION_SCAN_SECS,
    BUILTIN_SPILL_MAX_BYTES, PromptLogConfig,
};
#[cfg(feature = "otlp")]
use crate::config::OtlpConfig;
use crate::entry::PromptLogEntry;
use crate::index::id_hash;
use crate::segment;
use crate::jsonl_export::JsonlExport;
use crate::spill::SpillStore;
use crate::store::SegmentStore;

#[cfg(feature = "otlp")]
use crate::otlp::{ExporterStatusSnapshot, OtelExporter, ProbeResult};

/// Entry-count cap on the channel. The real bound is the byte budget;
/// this only stops unbounded metadata when producers send tiny entries
/// faster than the writer drains them.
const CHANNEL_CAPACITY: usize = 4096;

/// Maintenance tick: flush/fsync cadence, spill drain, dir-change check.
/// Every tick action is a no-op or a cheap syscall in the healthy path.
const TICK: Duration = Duration::from_millis(250);

/// Bound for the shutdown handshake — a wedged flush must not block the
/// gateway exit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Counting, budget-aware handle to the writer channel. ALL producers —
/// handler paths, the error path, the stream wrapper's Drop — go through
/// this so `dropped_on_full()` reflects every entry that failed to reach
/// ANY persistence tier, not just whichever path happened to count.
#[derive(Clone)]
pub struct PromptLogSender {
    sender: mpsc::Sender<PromptLogEntry>,
    /// Remaining byte budget of the in-memory channel (may transiently go
    /// negative under races; it is a damper, not an accountant).
    budget: Arc<AtomicI64>,
    dropped_on_full: Arc<AtomicU64>,
    spill: Arc<SpillStore>,
}

impl PromptLogSender {
    /// Non-blocking send. Over budget or channel-full → spill file;
    /// spill failure → drop + count + warn. Only `Closed` is returned to
    /// the caller (each site logs it at its own level) — everything else
    /// has been parked somewhere durable.
    // The error must carry the entry back so the caller can spill it.
    #[allow(clippy::result_large_err)]
    pub fn try_send(
        &self,
        entry: PromptLogEntry,
    ) -> Result<(), mpsc::error::TrySendError<PromptLogEntry>> {
        let bytes = entry.estimated_bytes() as i64;
        let remaining = self.budget.fetch_sub(bytes, Ordering::Relaxed) - bytes;
        if remaining < 0 {
            self.budget.fetch_add(bytes, Ordering::Relaxed);
            self.spill_or_drop(entry);
            return Ok(());
        }
        match self.sender.try_send(entry) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(e)) => {
                self.budget.fetch_add(bytes, Ordering::Relaxed);
                self.spill_or_drop(e);
                Ok(())
            }
            Err(e @ mpsc::error::TrySendError::Closed(_)) => {
                self.budget.fetch_add(bytes, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    fn spill_or_drop(&self, entry: PromptLogEntry) {
        let request_id = entry.request_id.clone();
        match segment::encode_record_frame(&entry) {
            Ok(frame) => match self.spill.try_write(&frame) {
                Ok(()) => {}
                Err(e) => self.count_drop(&request_id, &e.to_string()),
            },
            Err(e) => self.count_drop(&request_id, &e.to_string()),
        }
    }

    fn count_drop(&self, request_id: &str, error: &str) {
        let n = self.dropped_on_full.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(
            request_id,
            error,
            total_dropped = n,
            "prompt log entry dropped: channel over budget AND spill failed"
        );
    }

    /// Test-only constructor: unlimited budget, 1-byte spill (any spill
    /// attempt fails into the drop counter — exactly what stream tests
    /// want to observe). Production code clones the writer's real sender
    /// via [`PromptLogWriter::sender`].
    #[doc(hidden)]
    pub fn for_tests(sender: mpsc::Sender<PromptLogEntry>) -> Self {
        Self {
            sender,
            budget: Arc::new(AtomicI64::new(i64::MAX / 2)),
            dropped_on_full: Arc::new(AtomicU64::new(0)),
            spill: Arc::new(SpillStore::new(&std::env::temp_dir(), 1)),
        }
    }
}

/// Handle to the background prompt log writer.
///
/// Clone-safe handle that checks `should_capture()` against the live
/// config. Segment file I/O happens in the background task; the segment
/// store is shared with the query path through `store()`.
#[derive(Clone)]
pub struct PromptLogWriter {
    config: Arc<ArcSwap<PromptLogConfig>>,
    sender: PromptLogSender,
    dropped_on_full: Arc<AtomicU64>,
    /// `None` = degraded boot (store failed to open): capture is hard-off,
    /// no background task runs; the config handles stay live so the
    /// dashboard renders the committed state. A restart re-opens the store.
    store: Option<Arc<SegmentStore>>,
    shutdown_tx: mpsc::Sender<oneshot::Sender<()>>,
    #[cfg(feature = "otlp")]
    otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>>,
    #[cfg(feature = "otlp")]
    flush_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

/// Everything the background task owns.
struct WriterTask {
    receiver: mpsc::Receiver<PromptLogEntry>,
    config: Arc<ArcSwap<PromptLogConfig>>,
    store: Arc<SegmentStore>,
    spill: Arc<SpillStore>,
    budget: Arc<AtomicI64>,
    shutdown_rx: mpsc::Receiver<oneshot::Sender<()>>,
    jsonl_export: JsonlExport,
    #[cfg(feature = "otlp")]
    otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>>,
}

impl PromptLogWriter {
    /// Spawn the background writer with no OTLP exporter — local
    /// segments only. Recovers or creates the segment store under
    /// `{cfg.dir}/segments/`; opening the store is fatal to bubble
    /// (silently dropping every entry would defeat the no-loss goal —
    /// boom-main logs and continues without prompt logging).
    pub fn spawn(config: PromptLogConfig, instance_id: &str) -> std::io::Result<Self> {
        #[cfg(feature = "otlp")]
        let otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>> = Arc::new(ArcSwap::from_pointee(None));
        let (writer, task) = Self::build(
            config,
            instance_id,
            #[cfg(feature = "otlp")]
            otlp,
            BUILTIN_CHANNEL_MAX_BYTES,
        )?;
        tokio::spawn(background_writer(task));
        Ok(writer)
    }

    /// Test seam: spawn with a shrunken channel budget so the spill path
    /// can be exercised without manufacturing 256MB entries.
    #[doc(hidden)]
    pub fn spawn_with_channel_budget_for_tests(
        config: PromptLogConfig,
        instance_id: &str,
        channel_bytes: u64,
    ) -> std::io::Result<Self> {
        #[cfg(feature = "otlp")]
        let otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>> = Arc::new(ArcSwap::from_pointee(None));
        let (writer, task) = Self::build(
            config,
            instance_id,
            #[cfg(feature = "otlp")]
            otlp,
            channel_bytes,
        )?;
        tokio::spawn(background_writer(task));
        Ok(writer)
    }

    /// Spawn with an OTLP exporter; also starts its periodic flush task.
    #[cfg(feature = "otlp")]
    pub fn spawn_with_otlp(
        config: PromptLogConfig,
        instance_id: &str,
        exporter: Arc<OtelExporter>,
    ) -> std::io::Result<Self> {
        let otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>> =
            Arc::new(ArcSwap::from_pointee(Some(exporter.clone())));
        let (writer, task) =
            Self::build(config, instance_id, otlp.clone(), BUILTIN_CHANNEL_MAX_BYTES)?;
        exporter.spawn_flush_task_to_handle(writer.flush_handle.clone());
        tokio::spawn(background_writer(task));
        Ok(writer)
    }

    fn build(
        config: PromptLogConfig,
        instance_id: &str,
        #[cfg(feature = "otlp")] otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>>,
        channel_bytes: u64,
    ) -> std::io::Result<(Self, WriterTask)> {
        let config = Arc::new(ArcSwap::from_pointee(config));
        let store = SegmentStore::open_sync(config.clone(), instance_id)?;
        let dir = Path::new(&config.load().dir).to_path_buf();
        let spill = Arc::new(SpillStore::new(&dir, BUILTIN_SPILL_MAX_BYTES));
        // Fixed at boot: hot-adjusting a live budget means reconciling
        // in-flight claims — restart changes it, everything else
        // (dir/segment/retention) is hot.
        let budget = Arc::new(AtomicI64::new(channel_bytes as i64));
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let dropped_on_full = Arc::new(AtomicU64::new(0));
        let sender = PromptLogSender {
            sender: tx,
            budget: budget.clone(),
            dropped_on_full: dropped_on_full.clone(),
            spill: spill.clone(),
        };
        let writer = Self {
            config: config.clone(),
            sender,
            dropped_on_full,
            store: Some(store.clone()),
            shutdown_tx,
            #[cfg(feature = "otlp")]
            otlp: otlp.clone(),
            #[cfg(feature = "otlp")]
            flush_handle: Arc::new(std::sync::Mutex::new(None)),
        };
        let task = WriterTask {
            receiver: rx,
            config,
            store,
            spill,
            budget,
            shutdown_rx,
            jsonl_export: JsonlExport::default(),
            #[cfg(feature = "otlp")]
            otlp,
        };
        Ok((writer, task))
    }

    /// Degraded writer for an unbootable store (bad `dir`, full disk,
    /// unreadable recovery). Capture is hard-disabled — `should_capture`
    /// returns false regardless of config — no background task runs, and
    /// the config handles stay live so the dashboard renders the committed
    /// YAML state (and the OTLP exporter still answers status/probes).
    #[allow(unused_variables)]
    pub fn degraded(
        config: PromptLogConfig,
        #[cfg(feature = "otlp")] otlp: Arc<ArcSwap<Option<Arc<OtelExporter>>>>,
    ) -> Self {
        let config = Arc::new(ArcSwap::from_pointee(config));
        let (tx, _rx) = mpsc::channel(1); // receiver dropped — Closed if ever sent to
        let (shutdown_tx, _shutdown_rx) = mpsc::channel(1);
        let dropped_on_full = Arc::new(AtomicU64::new(0));
        let sender = PromptLogSender {
            sender: tx,
            budget: Arc::new(AtomicI64::new(0)),
            dropped_on_full: dropped_on_full.clone(),
            // Last-resort backstop only: capture is hard-off, so nothing
            // reaches the spill anyway.
            spill: Arc::new(SpillStore::new(Path::new("/nonexistent"), 0)),
        };
        tracing::error!(
            "prompt log writer degraded: the segment store is unavailable; \
             prompt logging is off until a restart re-opens it"
        );
        Self {
            config,
            sender,
            dropped_on_full,
            store: None,
            shutdown_tx,
            #[cfg(feature = "otlp")]
            otlp,
            #[cfg(feature = "otlp")]
            flush_handle: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// The segment store shared with the query path. `None` in degraded
    /// mode — the query handle must fall back to the disabled impl.
    pub fn store(&self) -> Option<Arc<SegmentStore>> {
        self.store.clone()
    }

    /// Check if this key/team should be captured.
    /// Call this BEFORE cloning the request body to avoid unnecessary work.
    /// Hard-off in degraded mode — a broken store cannot honor capture.
    pub fn should_capture(&self, key_hash: &str, team_id: Option<&str>) -> bool {
        self.store.is_some() && self.config.load().should_capture(key_hash, team_id)
    }

    /// Get a clone of the counting sender for passing to stream wrappers.
    pub fn sender(&self) -> PromptLogSender {
        self.sender.clone()
    }

    /// Cumulative entries dropped because every persistence tier refused
    /// them (channel over budget AND spill failed). Surfaced for
    /// diagnostics.
    pub fn dropped_on_full(&self) -> u64 {
        self.dropped_on_full.load(Ordering::Relaxed)
    }

    /// Send an entry to the background writer (non-blocking,
    /// fire-and-forget). Over-budget entries spill to disk; only a closed
    /// channel (shutdown) drops in memory. No-op in degraded mode.
    pub fn send(&self, entry: PromptLogEntry) {
        if self.store.is_none() {
            return;
        }
        if let Err(mpsc::error::TrySendError::Closed(e)) = self.sender.try_send(entry) {
            tracing::warn!("Prompt log channel closed, dropping entry: {}", e.request_id);
        }
    }

    /// Update config at runtime (hot-reload). The writer picks up segment
    /// thresholds, retention, durability, spill budget and `dir` changes
    /// on its next tick; capture toggles apply immediately. Does NOT touch
    /// the running exporter — use `replace_otlp` for those.
    pub fn update_config(&self, new_config: PromptLogConfig) {
        self.config.store(Arc::new(new_config));
    }

    /// Overlay only the retention policy onto the live config, leaving the
    /// YAML-home fields untouched. This is the DB-adopt path (retention is
    /// the one global-domain prompt-log field, CATEGORY_PROMPTLOG): the KV
    /// row carries retention alone, so the merge must not clobber
    /// instance-local values with stale ones.
    pub fn set_retention(&self, retention: crate::config::RetentionConfig) {
        let mut cfg = self.config();
        if cfg.retention != retention {
            cfg.retention = retention;
            self.config.store(Arc::new(cfg));
        }
    }

    /// Hot-swap the OTLP exporter. Aborts the old flush task, runs a
    /// best-effort final flush on the old exporter to drain its queue,
    /// constructs a new exporter from `new_otlp`, stores it, and spawns a
    /// fresh flush task.
    #[cfg(feature = "otlp")]
    pub async fn replace_otlp(&self, new_otlp: &OtlpConfig) {
        if let Some(h) = self.flush_handle.lock().unwrap().take() {
            h.abort();
        }
        if let Some(old) = self.otlp.load().as_ref() {
            let bound = std::time::Duration::from_secs(new_otlp.timeout_secs.max(1) + 2);
            let _ = tokio::time::timeout(bound, old.flush()).await;
        }
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

    /// Read-only snapshot of the live OTLP exporter's state machine.
    /// `None` = OTLP not configured (the dashboard renders that as
    /// "disabled").
    #[cfg(feature = "otlp")]
    pub async fn otlp_status(&self) -> Option<ExporterStatusSnapshot> {
        let g = self.otlp.load();
        let Some(exporter) = g.as_ref() else {
            return None;
        };
        Some(exporter.clone().status_snapshot())
    }

    #[cfg(not(feature = "otlp"))]
    pub async fn otlp_status(&self) -> Option<()> {
        None
    }

    /// Manual probe of the live OTLP exporter (dashboard "Probe now").
    #[cfg(feature = "otlp")]
    pub async fn probe_otlp(&self) -> Option<ProbeResult> {
        let g = self.otlp.load();
        let Some(exporter) = g.as_ref() else {
            return None;
        };
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

    /// Read-only config handle, for use by query paths.
    pub fn config_handle(&self) -> Arc<ArcSwap<PromptLogConfig>> {
        self.config.clone()
    }

    /// Drain the channel, flush and fsync the active segment, flush the
    /// OTLP batch. Bounded by `SHUTDOWN_TIMEOUT` so a wedged disk cannot
    /// block the gateway exit. Called once during shutdown.
    pub async fn shutdown_flush(&self) {
        #[cfg(feature = "otlp")]
        if let Some(exporter) = self.otlp.load().as_ref() {
            let bound = std::time::Duration::from_secs(exporter_timeout_secs(exporter));
            let _ = tokio::time::timeout(bound, exporter.flush()).await;
        }
        #[cfg(feature = "otlp")]
        if let Some(h) = self.flush_handle.lock().unwrap().take() {
            h.abort();
        }
        if self.store.is_none() {
            return; // degraded: no writer task, no store to flush
        }
        let (tx, rx) = oneshot::channel();
        if self.shutdown_tx.send(tx).await.is_ok() {
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, rx).await;
        } else {
            // The writer task is gone — flush what we can directly. The
            // store's flush is safe here precisely because the single
            // writer no longer exists.
            if let Some(store) = &self.store {
                let _ = store.flush_active_block().await;
                let _ = store.sync_active();
            }
        }
    }
}

#[cfg(feature = "otlp")]
fn exporter_timeout_secs(exporter: &OtelExporter) -> u64 {
    // Best-effort bound for the final OTLP flush; the exporter's own
    // timeout config is private, so use a generous fixed cap.
    let _ = exporter;
    30
}

/// The background loop. Exactly one task runs this: it is the sole
/// appender, flusher and sealer of the active segment.
async fn background_writer(mut task: WriterTask) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_fsync = Instant::now();
    let mut last_retention = Instant::now();
    task.jsonl_export
        .maintain(&task.config.load(), task.store.instance_slug());

    let done = loop {
        tokio::select! {
            entry = task.receiver.recv() => {
                let Some(entry) = entry else {
                    break None;
                };
                process_entry(entry, &mut task).await;
            }
            _ = interval.tick() => {
                maintain(
                    &mut task,
                    &mut last_fsync,
                    &mut last_retention,
                )
                .await;
            }
            cmd = task.shutdown_rx.recv() => {
                task.receiver.close();
                while let Some(entry) = task.receiver.recv().await {
                    process_entry(entry, &mut task).await;
                }
                break cmd;
            }
        }
    };

    // Exit path (channel closed or shutdown): flush the tail and make it
    // durable. A crash before this point loses at most the last fsync
    // interval — the documented durability semantics.
    drain_spill(&task.store, &task.spill, &mut task.jsonl_export).await;
    task.jsonl_export.close();
    if let Err(e) = task.store.flush_active_block().await {
        tracing::warn!(error = %e, "final block flush failed during writer exit");
    }
    if let Err(e) = task.store.sync_active() {
        tracing::warn!(error = %e, "final fsync failed during writer exit");
    }
    tracing::info!("Prompt log writer exiting");
    if let Some(done) = done {
        let _ = done.send(());
    }
}

/// Append one entry: OTLP fork first (local store is the source of truth
/// and must never be skipped by an exporter failure), then the segment
/// append with block cutting and seal checks.
async fn process_entry(entry: PromptLogEntry, task: &mut WriterTask) {
    #[cfg(feature = "otlp")]
    {
        let (otlp_enabled, exporter) = {
            let cfg = task.config.load();
            let enabled = cfg.otlp.enabled;
            let exporter = task.otlp.load().clone();
            (enabled, exporter)
        };
        if otlp_enabled {
            if let Some(exporter) = exporter.as_ref() {
                exporter.enqueue(entry.clone()).await;
            }
        }
    }

    let bytes = entry.estimated_bytes() as i64;
    let frame = match segment::encode_record_frame(&entry) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(request_id = %entry.request_id, error = %e, "prompt log entry serialization failed");
            task.budget.fetch_add(bytes, Ordering::Relaxed);
            return;
        }
    };
    let hash = id_hash(&entry.request_id);
    let phase = segment::phase_to_u8(entry.phase);
    let ts = segment::entry_ts_ms(&entry);
    match task.store.append_frame(&frame, hash, phase, ts) {
        Ok(()) => task.jsonl_export.append(&frame[4..]),
        Err(e) => tracing::error!(request_id = %entry.request_id, error = %e, "segment append failed"),
    }
    task.budget.fetch_add(bytes, Ordering::Relaxed);

    if task.store.wants_flush() {
        if let Err(e) = task.store.flush_active_block().await {
            tracing::warn!(error = %e, "block flush failed");
        }
    }
    if task.store.should_seal() {
        seal_and_age(&task.store, &task.config).await;
    }
}

/// Periodic maintenance, every TICK: spill drain, dir change, tail flush,
/// group-commit fsync, seal, retention.
async fn maintain(
    task: &mut WriterTask,
    last_fsync: &mut Instant,
    last_retention: &mut Instant,
) {
    let WriterTask {
        store,
        spill,
        config,
        jsonl_export,
        ..
    } = task;
    let dir = {
        let cfg = config.load();
        jsonl_export.maintain(&cfg, store.instance_slug());
        cfg.dir.clone()
    };

    // 1. Replay any spilled frames (no-op in the healthy path).
    drain_spill(store, spill, jsonl_export).await;

    // 2. Config `dir` change: seal, move the store, follow with the spill.
    if store.dir_changed() {
        if let Err(e) = store.seal_active().await {
            tracing::warn!(error = %e, "seal before dir change failed; will retry next tick");
            return;
        }
        match store.update_dir() {
            Ok(()) => spill.update_dir(Path::new(&dir)),
            Err(e) => tracing::error!(error = %e, "segment store dir change failed"),
        }
    }

    // 3. Flush the tail so the crash window and the unqueryable-tail stay
    //    bounded.
    if let Err(e) = store.flush_active_block().await {
        tracing::warn!(error = %e, "periodic block flush failed");
    }

    // 4. Group-commit fsync (built-in cadence — retired as a knob).
    let every = Duration::from_millis(BUILTIN_FSYNC_INTERVAL_MS.max(100));
    if last_fsync.elapsed() >= every {
        let _ = store.sync_active();
        *last_fsync = Instant::now();
    }

    // 5. Seal when a threshold is crossed; otherwise run retention on its
    //    own timer (seal_and_age runs it immediately after every seal).
    if store.should_seal() {
        seal_and_age(store, config).await;
    } else if last_retention.elapsed() >= Duration::from_secs(BUILTIN_RETENTION_SCAN_SECS) {
        run_retention(store, config).await;
        *last_retention = Instant::now();
    }
}

async fn seal_and_age(store: &Arc<SegmentStore>, config: &Arc<ArcSwap<PromptLogConfig>>) {
    match store.seal_active().await {
        Ok(()) => run_retention(store, config).await,
        Err(e) => tracing::warn!(error = %e, "segment seal failed; retried next tick"),
    }
}

async fn run_retention(store: &Arc<SegmentStore>, config: &Arc<ArcSwap<PromptLogConfig>>) {
    let cfg = config.load().retention.clone();
    let store = store.clone();
    let report = match tokio::task::spawn_blocking(move || crate::aging::enforce_once(&store, &cfg))
        .await
    {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(io::Error::other(format!("retention task: {e}"))),
    };
    match report {
        Ok(r) if r.deleted_segments > 0 => {
            tracing::info!(
                deleted = r.deleted_segments,
                freed_bytes = r.freed_bytes,
                "retention pass"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "retention pass failed"),
    }
}

/// Replay the spill into the segment store. Frame decode failures skip
/// the frame (undecodable data is unrecoverable by definition); an append
/// failure stops the drain and leaves the frame for the next pass.
async fn drain_spill(
    store: &Arc<SegmentStore>,
    spill: &Arc<SpillStore>,
    jsonl_export: &mut JsonlExport,
) {
    if spill.bytes() == 0 {
        return;
    }
    let store = store.clone();
    let spill = spill.clone();
    let mut export = std::mem::take(jsonl_export);
    let result = match tokio::task::spawn_blocking(move || {
        let result = spill.drain(|body| {
            let entry = match segment::decode_record(body) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "spill frame undecodable; skipping");
                    return Ok(());
                }
            };
            let mut frame = Vec::with_capacity(4 + body.len());
            frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
            frame.extend_from_slice(body);
            store.append_frame(
                &frame,
                id_hash(&entry.request_id),
                segment::phase_to_u8(entry.phase),
                segment::entry_ts_ms(&entry),
            )?;
            export.append(body);
            Ok(())
        });
        (result, export)
    })
    .await
    {
        Ok((result, export)) => {
            *jsonl_export = export;
            result
        }
        Err(e) => Err(io::Error::other(format!("spill drain task: {e}"))),
    };
    match result {
        Ok(0) => {}
        Ok(n) => tracing::info!(replayed = n, "spill drained into segment store"),
        Err(e) => tracing::warn!(error = %e, "spill drain failed; retried next tick"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::LogPhase;
    use crate::store::EntryPair;

    fn test_config(dir: &std::path::Path) -> PromptLogConfig {
        PromptLogConfig {
            dir: dir.to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    fn entry(id: &str, payload: usize) -> PromptLogEntry {
        let body = Arc::new(serde_json::Value::String("x".repeat(payload)));
        PromptLogEntry::new_request(
            id,
            None,
            "keyhash",
            None,
            Some("team1"),
            "gpt-4o",
            "/v1/chat/completions",
            false,
            body,
            None,
            None,
        )
    }

    async fn wait_for<F>(mut f: F)
    where
        F: FnMut() -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the writer to catch up"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn send_then_queryable_via_store() {
        let dir = tempfile::tempdir().unwrap();
        let writer = PromptLogWriter::spawn(test_config(dir.path()), "gw-test")
            .unwrap();

        let e = entry("r1", 64);
        let resp = {
            let mut r = PromptLogEntry::new_response_from(&e);
            r.set_response(Arc::new(serde_json::json!({"a": 1})));
            r
        };
        writer.send(e);
        writer.send(resp);

        let store = writer.store().expect("healthy writer in tests");
        wait_for(|| store.find_pair_sync("r1").complete()).await;
        assert!(store.find_pair_sync("absent").request.is_none());
        writer.shutdown_flush().await;
    }

    #[tokio::test]
    async fn over_budget_entries_spill_not_drop() {
        let dir = tempfile::tempdir().unwrap();
        // 1-byte channel budget: every entry is over budget → spill path.
        let writer = PromptLogWriter::spawn_with_channel_budget_for_tests(
            test_config(dir.path()),
            "gw-test",
            1,
        )
        .unwrap();

        writer.send(entry("spilled-1", 512));
        writer.send(entry("spilled-2", 512));
        assert_eq!(writer.dropped_on_full(), 0);

        let store = writer.store().expect("healthy writer in tests");
        wait_for(|| store.find_sync("spilled-1", LogPhase::Request).is_some()).await;
        wait_for(|| store.find_sync("spilled-2", LogPhase::Request).is_some()).await;
        assert_eq!(writer.dropped_on_full(), 0);
        writer.shutdown_flush().await;
    }

    #[tokio::test]
    async fn shutdown_flush_drains_pending() {
        let dir = tempfile::tempdir().unwrap();
        let writer = PromptLogWriter::spawn(test_config(dir.path()), "gw-test")
            .unwrap();
        writer.send(entry("r-final", 64));
        // Shutdown drains the queue and flushes before returning.
        writer.shutdown_flush().await;

        let store = writer.store().expect("healthy writer in tests");
        assert!(
            store.find_sync("r-final", LogPhase::Request).is_some(),
            "entry sent before shutdown must be flushed"
        );
    }

    #[tokio::test]
    async fn jsonl_exports_normal_and_spilled_records_through_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.jsonl_export.enabled = true;
        let normal = entry("normal", 32);
        let spilled = entry("spilled", 4096);
        let writer = PromptLogWriter::spawn_with_channel_budget_for_tests(
            config,
            "gw-test",
            normal.estimated_bytes() as u64,
        )
        .unwrap();
        writer.send(normal);
        writer.send(spilled);
        writer.shutdown_flush().await;

        let store = writer.store().unwrap();
        let dir = dir.path().join("export");
        let mut ids = Vec::new();
        for file in std::fs::read_dir(dir).unwrap() {
            let path = file.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                for line in std::fs::read_to_string(path).unwrap().lines() {
                    let entry = segment::decode_record(line.as_bytes()).unwrap();
                    assert!(store.find_sync(&entry.request_id, entry.phase).is_some());
                    ids.push(entry.request_id);
                }
            }
        }
        ids.sort();
        assert_eq!(ids, ["normal", "spilled"]);
    }

    #[tokio::test]
    async fn jsonl_hot_reload_and_bad_directory_leave_segment_writes_available() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path());
        let writer = PromptLogWriter::spawn(config.clone(), "gw-test").unwrap();
        let store = writer.store().unwrap();
        let export_dir = dir.path().join("export");
        writer.send(entry("disabled", 32));
        wait_for(|| store.find_sync("disabled", LogPhase::Request).is_some()).await;
        assert!(!export_dir.exists());

        config.jsonl_export.enabled = true;
        config.jsonl_export.dir = dir.path().join("segments").to_string_lossy().into_owned();
        writer.update_config(config.clone());
        writer.send(entry("bad-directory", 32));
        wait_for(|| store.find_sync("bad-directory", LogPhase::Request).is_some()).await;

        config.jsonl_export.dir.clear();
        writer.update_config(config.clone());
        wait_for(|| export_dir.join(".prompt-gw-test.lock").exists()).await;
        writer.send(entry("enabled", 32));
        wait_for(|| store.find_sync("enabled", LogPhase::Request).is_some()).await;

        config.jsonl_export.enabled = false;
        writer.update_config(config);
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .open(export_dir.join(".prompt-gw-test.lock"))
            .unwrap();
        wait_for(|| lock.try_lock().is_ok()).await;
        writer.send(entry("disabled-again", 32));
        writer.shutdown_flush().await;
        assert!(store.find_sync("disabled-again", LogPhase::Request).is_some());
        let jsonl = std::fs::read_dir(export_dir)
            .unwrap()
            .map(|file| file.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .unwrap();
        let content = std::fs::read_to_string(jsonl).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert_eq!(
            segment::decode_record(content.trim_end().as_bytes())
                .unwrap()
                .request_id,
            "enabled"
        );
    }

    #[tokio::test]
    async fn entry_pair_from_writer_store() {
        let dir = tempfile::tempdir().unwrap();
        let writer = PromptLogWriter::spawn(test_config(dir.path()), "gw-test")
            .unwrap();
        let e = entry("pair-check", 32);
        let resp = {
            let mut r = PromptLogEntry::new_response_from(&e);
            r.set_response(Arc::new(serde_json::json!({"ok": true})));
            r
        };
        writer.send(e);
        writer.send(resp);
        let store = writer.store().expect("healthy writer in tests");
        wait_for(|| store.find_pair_sync("pair-check").complete()).await;
        let EntryPair { request, response } = store.find_pair_sync("pair-check");
        assert_eq!(request.unwrap().team_alias.as_deref(), Some("team1"));
        assert!(response.is_some());
        writer.shutdown_flush().await;
    }

    #[tokio::test]
    async fn sender_drops_when_spill_also_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx_keep_open) = mpsc::channel::<PromptLogEntry>(1);
        let spill = Arc::new(SpillStore::new(dir.path(), 1)); // 1-byte spill
        let dropped = Arc::new(AtomicU64::new(0));
        let sender = PromptLogSender {
            sender: tx,
            budget: Arc::new(AtomicI64::new(1024)),
            dropped_on_full: dropped.clone(),
            spill,
        };

        assert!(sender.try_send(entry("ok-1", 8)).is_ok());
        // Channel (capacity 1) is now full → second send spills → spill
        // budget (1 byte) rejects → dropped + counted, not an Err.
        assert!(sender.try_send(entry("too-big", 64)).is_ok());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }
}
