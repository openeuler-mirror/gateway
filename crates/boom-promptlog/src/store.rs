//! SegmentStore — the content layer's heart.
//!
//! Owns the on-disk layout under `{prompt_log.dir}/segments/`:
//!
//! ```text
//! {dir}/segments/{slug}-{start_ms}-{seq:06}.seg   one global write sequence
//! {dir}/segments/{slug}-{start_ms}-{seq:06}.idx   sorted id index + bloom
//! ```
//!
//! One active segment receives all records (every key, both phases) as
//! zstd-compressed blocks; size/age thresholds seal it immutably with a
//! finalized `.idx`. Sealed segments are whole-file deletable (retention) and
//! never rewritten. The store is self-describing: recovery rebuilds the
//! registry by scanning the directory — no persistent manifest exists, so
//! there is no manifest to corrupt. Crash mid-seal leaves a stream of valid
//! blocks with no footer; recovery salvages them (scan → truncate tail →
//! append footer → rebuild index).
//!
//! Concurrency model:
//! - Exactly one task (the background writer) appends, flushes, seals.
//!   Mutations take the write lock only for buffer/index updates; block
//!   compression + file I/O run outside it.
//! - Any number of concurrent readers take the read lock just long enough
//!   to snapshot what they need, then do file I/O lock-free.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use arc_swap::ArcSwap;
use tokio::sync::Notify;

use crate::config::PromptLogConfig;
use crate::entry::{LogPhase, PromptLogEntry};
use crate::index::{id_hash, locate_block, ActiveHit, ActiveIndex, SegmentIndex};
use crate::segment;
use crate::segment::{
    prepare_block, read_block_at, read_header, read_sealed_meta, scan_blocks, BlockLoc,
    SegmentFooter, BLOCK_HEADER_SIZE, IDX_EXT, SEG_EXT,
};

/// Resident `.idx` cache cap. One index of a 512MB segment is ~2MB; 64
/// entries bound the worst case around ~130MB. Eviction is harmless — a
/// miss reloads from disk.
const IDX_CACHE_CAPACITY: usize = 64;

/// Both halves of one request, as the dashboard wants them.
#[derive(Debug, Clone, Default)]
pub struct EntryPair {
    pub request: Option<PromptLogEntry>,
    pub response: Option<PromptLogEntry>,
}

impl EntryPair {
    fn has(&self, phase: LogPhase) -> bool {
        match phase {
            LogPhase::Request => self.request.is_some(),
            LogPhase::Response => self.response.is_some(),
        }
    }

    fn put(&mut self, entry: PromptLogEntry) {
        match entry.phase {
            LogPhase::Request => self.request = Some(entry),
            LogPhase::Response => self.response = Some(entry),
        }
    }

    pub fn complete(&self) -> bool {
        self.request.is_some() && self.response.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct SealedMeta {
    pub path: PathBuf,
    pub idx_path: PathBuf,
    pub slug: String,
    pub start_ms: i64,
    pub seq: u64,
    pub first_ts_ms: i64,
    pub last_ts_ms: i64,
    pub record_count: u64,
    pub uncomp_bytes: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct StoreStats {
    pub dir: String,
    pub sealed_segments: u64,
    pub sealed_bytes: u64,
    pub sealed_records: u64,
    pub active_records: u64,
    pub active_uncomp_bytes: u64,
    pub oldest_ts_ms: Option<i64>,
    pub newest_ts_ms: Option<i64>,
}

/// The active segment: single-writer mutable state behind the store's
/// RwLock. `inflight` holds record frames not yet flushed as a block;
/// flushed blocks live in `blocks` and on disk.
struct ActiveSegment {
    path: PathBuf,
    slug: String,
    start_ms: i64,
    seq: u64,
    /// Append-mode handle. Only the writer task writes through it (or a
    /// clone of it inside a blocking task); queries open their own fd so
    /// they never race the shared offset.
    file: std::fs::File,
    /// End-of-file offset after the last flushed block.
    file_end: u64,
    /// Uncompressed bytes already flushed into blocks.
    flushed_bytes: u64,
    /// Record frames appended since the last flush — complete frames only,
    /// so a flush always cuts on a frame boundary.
    inflight: Vec<u8>,
    pending_frames: u32,
    blocks: Vec<BlockLoc>,
    index: ActiveIndex,
    record_count: u64,
    first_ts_ms: Option<i64>,
    last_ts_ms: Option<i64>,
}

impl ActiveSegment {
    fn stream_len(&self) -> u64 {
        self.flushed_bytes + self.inflight.len() as u64
    }
}

struct StoreInner {
    dir: PathBuf,
    next_seq: u64,
    active: Option<ActiveSegment>,
    /// Oldest → newest by (start_ms, seq).
    sealed: Vec<SealedMeta>,
}

pub struct SegmentStore {
    config: Arc<ArcSwap<PromptLogConfig>>,
    instance_slug: String,
    inner: RwLock<StoreInner>,
    idx_cache: Mutex<lru::LruCache<PathBuf, Arc<SegmentIndex>>>,
    /// Bumped every time a segment seals; the retention task waits on it so
    /// enforcement runs promptly after rotation, not just on its timer.
    seal_epoch: AtomicU64,
    seal_notify: Notify,
}

impl SegmentStore {
    /// Recover-or-create the store under `{cfg.dir}/segments/`. Sync —
    /// callers run it in `spawn_blocking` (the writer does; the CLI has no
    /// runtime constraints).
    pub fn open_sync(
        config: Arc<ArcSwap<PromptLogConfig>>,
        instance_id: &str,
    ) -> io::Result<Arc<Self>> {
        let slug = segment::instance_slug(instance_id);
        let dir = segments_dir(&config.load().dir);
        std::fs::create_dir_all(&dir)?;
        let (sealed, next_seq) = recover_directory(&dir, &slug)?;
        let store = Arc::new(Self {
            config,
            instance_slug: slug,
            inner: RwLock::new(StoreInner {
                dir,
                next_seq,
                active: None,
                sealed,
            }),
            idx_cache: Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(IDX_CACHE_CAPACITY).unwrap(),
            )),
            seal_epoch: AtomicU64::new(0),
            seal_notify: Notify::new(),
        });
        store.open_active_sync()?;
        Ok(store)
    }

    // ── live config ────────────────────────────────────────────

    fn segment_cfg(&self) -> crate::config::SegmentConfig {
        self.config.load().segment.clamped()
    }

    pub fn current_dir(&self) -> PathBuf {
        self.inner.read().unwrap().dir.clone()
    }

    /// This instance's segment-name prefix. The retention validator
    /// re-checks it as defense in depth before any unlink.
    pub fn instance_slug(&self) -> &str {
        &self.instance_slug
    }

    /// The writer watches this; a config `dir` change means: seal, move to
    /// the new root, keep serving. Instances are private-domain config —
    /// each machine ages only its own segments.
    pub fn dir_changed(&self) -> bool {
        let new_dir = segments_dir(&self.config.load().dir);
        self.inner.read().unwrap().dir != new_dir
    }

    /// Point the store at the (changed) configured directory and open a
    /// fresh active segment there. The writer seals first. Segments left in
    /// the old directory stay in the registry so aging still reaches them.
    pub fn update_dir(&self) -> io::Result<()> {
        let new_dir = segments_dir(&self.config.load().dir);
        if self.inner.read().unwrap().dir == new_dir {
            return Ok(());
        }
        std::fs::create_dir_all(&new_dir)?;
        self.inner.write().unwrap().dir = new_dir;
        self.open_active_sync()
    }

    // ── open / recovery internals ──────────────────────────────

    fn open_active_sync(&self) -> io::Result<()> {
        let (dir, next_seq) = {
            let inner = self.inner.read().unwrap();
            (inner.dir.clone(), inner.next_seq)
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        let name = segment::segment_file_name(&self.instance_slug, now_ms, next_seq);
        let path = dir.join(&name);
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|e| io::Error::other(format!("create active segment {path:?}: {e}")))?;
        segment::write_header(&mut file, &self.instance_slug, now_ms)?;
        let file_end = file.metadata()?.len();
        let mut inner = self.inner.write().unwrap();
        inner.active = Some(ActiveSegment {
            path,
            slug: self.instance_slug.clone(),
            start_ms: now_ms,
            seq: next_seq,
            file,
            file_end,
            flushed_bytes: 0,
            inflight: Vec::new(),
            pending_frames: 0,
            blocks: Vec::new(),
            index: HashMap::new(),
            record_count: 0,
            first_ts_ms: None,
            last_ts_ms: None,
        });
        inner.next_seq = next_seq + 1;
        Ok(())
    }

    // ── append (single writer) ──────────────────────────────────

    /// Append one serialized record frame. The hash/phase/ts come from the
    /// caller (writer pipeline or spill drain) so neither path pays double
    /// serialization/parsing.
    pub fn append_frame(
        &self,
        frame: &[u8],
        hash: u64,
        phase: u8,
        ts_ms: Option<i64>,
    ) -> io::Result<()> {
        let mut inner = self.inner.write().unwrap();
        let Some(active) = inner.active.as_mut() else {
            return Err(io::Error::other("segment store has no active segment"));
        };
        let abs_off = active.stream_len();
        active
            .index
            .entry(hash)
            .or_default()
            .push(ActiveHit { phase, abs_off });
        active.inflight.extend_from_slice(frame);
        active.pending_frames += 1;
        if let Some(ts) = ts_ms {
            active.first_ts_ms = Some(active.first_ts_ms.map_or(ts, |t| t.min(ts)));
            active.last_ts_ms = Some(active.last_ts_ms.map_or(ts, |t| t.max(ts)));
        }
        active.record_count += 1;
        Ok(())
    }

    /// Append one entry (encode → index). Used by tests.
    pub fn append_entry(&self, entry: &PromptLogEntry) -> io::Result<()> {
        let frame = segment::encode_record_frame(entry)?;
        let ts = segment::entry_ts_ms(entry);
        self.append_frame(
            &frame,
            id_hash(&entry.request_id),
            segment::phase_to_u8(entry.phase),
            ts,
        )
    }

    /// Whether the unflushed tail reached the configured block size —
    /// the writer's signal to cut a block now.
    pub fn wants_flush(&self) -> bool {
        let inner = self.inner.read().unwrap();
        let Some(active) = inner.active.as_ref() else {
            return false;
        };
        active.inflight.len() as u64 >= self.segment_cfg().block_bytes as u64
    }

    /// Whether the active segment crossed a seal threshold (uncompressed
    /// stream size or segment age).
    pub fn should_seal(&self) -> bool {
        let cfg = self.segment_cfg();
        let inner = self.inner.read().unwrap();
        let Some(active) = inner.active.as_ref() else {
            return false;
        };
        if active.stream_len() >= cfg.max_segment_bytes() {
            return true;
        }
        active.start_ms + (cfg.max_segment_age_secs as i64) * 1000
            <= chrono::Utc::now().timestamp_millis()
    }

    // ── flush / seal ───────────────────────────────────────────

    /// Compress and write the current inflight tail as one block. No-op
    /// when everything is already flushed. Compression and the append run
    /// in the blocking pool; buffer/index mutations stay under the write
    /// lock only — and because the writer is the sole appender, the tail
    /// cannot change while the blocking work is in flight.
    pub async fn flush_active_block(&self) -> io::Result<()> {
        let (file, snapshot, file_pos, abs_start, frames) = {
            let inner = self.inner.read().unwrap();
            let Some(active) = inner.active.as_ref() else {
                return Ok(());
            };
            if active.inflight.is_empty() {
                return Ok(());
            }
            (
                active.file.try_clone()?,
                active.inflight.clone(),
                active.file_end,
                active.flushed_bytes,
                active.pending_frames,
            )
        };
        let (block_pos, comp_len, uncomp_len) =
            tokio::task::spawn_blocking(move || -> io::Result<(u64, u32, u32)> {
                let prepared =
                    prepare_block(&snapshot, frames, crate::config::BUILTIN_ZSTD_LEVEL)?;
                let comp_len = (prepared.len() - BLOCK_HEADER_SIZE) as u32;
                let uncomp_len =
                    u32::from_le_bytes(prepared[8..12].try_into().unwrap());
                let mut f = file;
                if let Err(e) = f.write_all(&prepared) {
                    // Roll back a partial append so the block stream stays
                    // scannable — a garbage tail would orphan every later
                    // block.
                    let _ = f.set_len(file_pos);
                    return Err(e);
                }
                Ok((file_pos, comp_len, uncomp_len))
            })
            .await
            .map_err(|e| io::Error::other(format!("block flush task: {e}")))??;
        let mut inner = self.inner.write().unwrap();
        let Some(active) = inner.active.as_mut() else {
            return Ok(());
        };
        active.blocks.push(BlockLoc {
            file_pos: block_pos,
            comp_len,
            uncomp_len,
            abs_start,
            count: frames,
        });
        active.flushed_bytes += uncomp_len as u64;
        active.file_end = block_pos + BLOCK_HEADER_SIZE as u64 + comp_len as u64;
        active.inflight.clear();
        active.pending_frames = 0;
        Ok(())
    }

    /// fsync the active segment file, per the durability policy. The fd is
    /// cloned under the read lock; the sync itself runs outside it.
    pub fn sync_active(&self) -> io::Result<()> {
        let file = {
            let inner = self.inner.read().unwrap();
            let Some(active) = inner.active.as_ref() else {
                return Ok(());
            };
            active.file.try_clone()?
        };
        file.sync_all()
    }

    /// Seal the active segment: flush the tail, append the footer (+
    /// fsync — sealing is the durability barrier), build and persist the
    /// `.idx`, rotate to a fresh active segment. On failure the active
    /// segment is restored (footer rolled back) and sealing is retried on
    /// the next writer cycle; every on-disk intermediate state is also
    /// recoverable by the next `open_sync`.
    pub async fn seal_active(&self) -> io::Result<()> {
        self.flush_active_block().await?;
        let active = {
            let mut inner = self.inner.write().unwrap();
            inner.active.take()
        };
        let Some(active) = active else {
            return Ok(());
        };
        // Empty actives never become sealed segments: an idle store would
        // rotate a stream of 0-record files on the age threshold. Drop the
        // file and rotate fresh — recovery does the same for crashed empties.
        if active.record_count == 0 {
            let path = active.path.clone();
            drop(active);
            let _ = std::fs::remove_file(&path);
            self.open_active_sync()?;
            return Ok(());
        }
        let idx_path = active.path.with_extension(IDX_EXT);
        let pre_seal_len = active.file_end;
        let joined = tokio::task::spawn_blocking(move || {
            let meta = seal_on_disk(&active, &idx_path);
            (active, meta)
        })
        .await;
        let (active, meta) = match joined {
            Ok(pair) => pair,
            Err(e) => {
                // The blocking task panicked with the active segment in
                // hand. Its blocks are all on disk, so leave the file for
                // recovery to salvage and rotate to a fresh segment now.
                tracing::error!(error = %e, "seal task panicked; rotating to a fresh segment");
                let _ = self.open_active_sync();
                return Err(io::Error::other(format!("seal task: {e}")));
            }
        };
        match meta {
            Ok(meta) => {
                self.inner.write().unwrap().sealed.push(meta);
                self.open_active_sync()?;
                self.seal_epoch.fetch_add(1, Ordering::Release);
                self.seal_notify.notify_waiters();
                Ok(())
            }
            Err(e) => {
                self.restore_active(active, pre_seal_len);
                Err(e)
            }
        }
    }

    fn restore_active(&self, mut active: ActiveSegment, pre_seal_len: u64) {
        let _ = active.file.set_len(pre_seal_len);
        active.file_end = pre_seal_len;
        let mut inner = self.inner.write().unwrap();
        if inner.active.is_none() {
            inner.active = Some(active);
        } else {
            // Unreachable with a single writer; if it ever happens, the
            // stale segment is fully flushed and footer-sealed on disk —
            // recovery re-registers it.
            tracing::warn!(path = ?active.path, "seal restore raced a replacement");
        }
    }

    // ── query ──────────────────────────────────────────────────

    /// Both phases of one request, active segment first, then sealed
    /// segments newest → oldest. A request and its response can straddle
    /// a seal boundary, so the search spans segments. Per-segment I/O
    /// errors degrade to a warn + skip; hash collisions are filtered by
    /// content verification (request_id equality).
    ///
    /// Sync — disk I/O inside; async callers wrap in `spawn_blocking`.
    pub fn find_pair_sync(&self, request_id: &str) -> EntryPair {
        let hash = id_hash(request_id);
        let mut pair = EntryPair::default();
        self.find_in_active(hash, request_id, &mut pair);
        if pair.complete() {
            return pair;
        }
        let sealed = self.inner.read().unwrap().sealed.clone();
        for meta in sealed.iter().rev() {
            if let Err(e) = self.find_in_sealed(meta, hash, request_id, &mut pair) {
                tracing::warn!(path = ?meta.path, error = %e, "sealed segment query failed");
                continue;
            }
            if pair.complete() {
                break;
            }
        }
        pair
    }

    pub fn find_sync(&self, request_id: &str, phase: LogPhase) -> Option<PromptLogEntry> {
        let EntryPair { request, response } = self.find_pair_sync(request_id);
        match phase {
            LogPhase::Request => request,
            LogPhase::Response => response,
        }
    }

    fn find_in_active(&self, hash: u64, request_id: &str, pair: &mut EntryPair) {
        // Phase 1 under the read lock: resolve unflushed hits directly from
        // the in-memory tail, collect flushed hits for lock-free file reads.
        let (block_hits, blocks, path) = {
            let inner = self.inner.read().unwrap();
            let Some(active) = inner.active.as_ref() else {
                return;
            };
            let Some(hits) = active.index.get(&hash) else {
                return;
            };
            let mut block_hits = Vec::new();
            for hit in hits {
                let Some(phase) = segment::phase_from_u8(hit.phase) else {
                    continue;
                };
                if pair.has(phase) {
                    continue;
                }
                if hit.abs_off < active.flushed_bytes {
                    block_hits.push(*hit);
                } else if let Some(frame) =
                    frame_at(&active.inflight, hit.abs_off - active.flushed_bytes)
                {
                    if let Ok(entry) = segment::decode_record(frame) {
                        if entry.request_id == request_id {
                            pair.put(entry);
                        }
                    }
                }
            }
            (block_hits, active.blocks.clone(), active.path.clone())
        };
        if block_hits.is_empty() {
            return;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            return;
        };
        let mut payloads: HashMap<u64, Vec<u8>> = HashMap::new();
        for hit in block_hits {
            let Some(phase) = segment::phase_from_u8(hit.phase) else {
                continue;
            };
            if pair.has(phase) {
                continue;
            }
            let Some(block) = locate_block(&blocks, hit.abs_off) else {
                continue;
            };
            let payload = match payloads.entry(block.file_pos) {
                std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => {
                    match read_block_at(&file, block.file_pos) {
                        Ok(p) => v.insert(p),
                        Err(e) => {
                            tracing::warn!(path = ?path, error = %e, "active block read failed");
                            continue;
                        }
                    }
                }
            };
            if let Some(frame) = frame_at(payload, hit.abs_off - block.abs_start) {
                if let Ok(entry) = segment::decode_record(frame) {
                    if entry.request_id == request_id {
                        pair.put(entry);
                    }
                }
            }
            let _ = phase;
        }
    }

    fn find_in_sealed(
        &self,
        meta: &SealedMeta,
        hash: u64,
        request_id: &str,
        pair: &mut EntryPair,
    ) -> io::Result<()> {
        let idx = self.load_idx(meta)?;
        if !idx.bloom.contains(hash) {
            return Ok(());
        }
        let file = std::fs::File::open(&meta.path)?;
        let mut payloads: HashMap<u64, Vec<u8>> = HashMap::new();
        for e in idx.lookup(hash) {
            let Some(phase) = segment::phase_from_u8(e.phase) else {
                continue;
            };
            if pair.has(phase) {
                continue;
            }
            let payload = match payloads.entry(e.block_file_pos) {
                std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(read_block_at(&file, e.block_file_pos)?)
                }
            };
            let Some(frame) = frame_at(payload, e.intra_off as u64) else {
                continue;
            };
            if let Ok(entry) = segment::decode_record(frame) {
                if entry.request_id == request_id {
                    pair.put(entry);
                }
            }
        }
        Ok(())
    }

    /// Load a segment's index through the LRU cache. A missing or invalid
    /// `.idx` self-heals: the index is rebuilt from the segment content and
    /// persisted (best effort), because the segment itself is the truth.
    fn load_idx(&self, meta: &SealedMeta) -> io::Result<Arc<SegmentIndex>> {
        if let Some(idx) = self.idx_cache.lock().unwrap().get(&meta.idx_path) {
            return Ok(idx.clone());
        }
        let idx = match SegmentIndex::load(&meta.idx_path) {
            Ok(Some(idx)) => Arc::new(idx),
            // Ok(None) = present but invalid; NotFound = missing entirely
            // (aging crashed mid-unlink, or never written). Both rebuild from
            // the segment — the segment is the truth, the idx is a cache.
            Ok(None) | Err(_) => {
                tracing::warn!(path = ?meta.idx_path, "idx missing or invalid; rebuilding from segment");
                let idx = Arc::new(rebuild_idx_from_segment(&meta.path)?);
                if let Err(e) = idx.write(&meta.idx_path) {
                    tracing::warn!(path = ?meta.idx_path, error = %e, "rebuilt idx persist failed");
                }
                idx
            }
        };
        self.idx_cache
            .lock()
            .unwrap()
            .put(meta.idx_path.clone(), idx.clone());
        Ok(idx)
    }

    // ── stats / retention hooks ────────────────────────────────

    pub fn stats(&self) -> StoreStats {
        let inner = self.inner.read().unwrap();
        let mut st = StoreStats {
            dir: inner.dir.to_string_lossy().into_owned(),
            sealed_segments: inner.sealed.len() as u64,
            sealed_bytes: inner.sealed.iter().map(|m| m.bytes).sum(),
            sealed_records: inner.sealed.iter().map(|m| m.record_count).sum(),
            ..Default::default()
        };
        let mut oldest = inner.sealed.iter().map(|m| m.first_ts_ms).min();
        let mut newest = inner.sealed.iter().map(|m| m.last_ts_ms).max();
        if let Some(a) = &inner.active {
            st.active_records = a.record_count;
            st.active_uncomp_bytes = a.stream_len();
            if let Some(t) = a.first_ts_ms {
                oldest = Some(oldest.map_or(t, |o: i64| o.min(t)));
            }
            if let Some(t) = a.last_ts_ms {
                newest = Some(newest.map_or(t, |n: i64| n.max(t)));
            }
        }
        st.oldest_ts_ms = oldest;
        st.newest_ts_ms = newest;
        st
    }

    /// Registry snapshot for the retention task (oldest → newest).
    pub fn sealed_metas(&self) -> Vec<SealedMeta> {
        self.inner.read().unwrap().sealed.clone()
    }

    /// Remove one sealed segment (both files) from the registry and disk.
    /// Only call with a registry-sourced meta after the retention
    /// validation chain — the registry itself was built from validated
    /// scans, so this path can never touch a file the gateway didn't write.
    pub fn remove_sealed(&self, meta: &SealedMeta) -> io::Result<()> {
        {
            let mut inner = self.inner.write().unwrap();
            inner.sealed.retain(|m| m.path != meta.path);
        }
        self.idx_cache.lock().unwrap().pop(&meta.idx_path);
        // idx first: a crash between the two unlinks leaves an orphan idx
        // (swept cheaply on recovery) instead of a sealed segment without
        // an idx (expensive full rescan to rebuild).
        match std::fs::remove_file(&meta.idx_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::remove_file(&meta.path)?;
        Ok(())
    }

    pub fn seal_epoch(&self) -> u64 {
        self.seal_epoch.load(Ordering::Acquire)
    }

    /// Retention waits on this so enforcement runs promptly after rotation.
    /// Create the `notified()` future *before* checking state, or a signal
    /// fired in between is lost.
    pub fn seal_notifier(&self) -> &Notify {
        &self.seal_notify
    }
}

// ─────────────────────────────────────────────────────────────────
// Free helpers
// ─────────────────────────────────────────────────────────────────

fn segments_dir(dir: &str) -> PathBuf {
    Path::new(dir).join("segments")
}

/// The frame bytes at `intra` inside an uncompressed block payload
/// (`[u32 len][json]`), or `None` when it runs off the end.
fn frame_at(payload: &[u8], intra: u64) -> Option<&[u8]> {
    let off = intra as usize;
    let len = u32::from_le_bytes(payload.get(off..off + 4)?.try_into().ok()?) as usize;
    payload.get(off + 4..off + 4 + len)
}

/// Append the footer, fsync, persist the index. The segment on disk is
/// complete the moment the footer lands — a crash after that point leaves
/// a sealed segment the next `open_sync` re-registers (idx loss self-heals
/// on query), so only footer/fsync failure is fatal to the seal.
fn seal_on_disk(active: &ActiveSegment, idx_path: &Path) -> io::Result<SealedMeta> {
    let mut file = active.file.try_clone()?;
    let footer = SegmentFooter {
        block_count: active.blocks.len() as u64,
        record_count: active.record_count,
        first_ts_ms: active.first_ts_ms.unwrap_or(active.start_ms),
        last_ts_ms: active.last_ts_ms.unwrap_or(active.start_ms),
        uncomp_bytes: active.flushed_bytes,
    };
    segment::write_footer(&mut file, &footer)?;
    file.sync_all()?;
    let idx = SegmentIndex::build(&active.index, &active.blocks, active.record_count);
    if let Err(e) = idx.write(idx_path) {
        tracing::warn!(path = ?idx_path, error = %e, "segment idx write failed; self-heals on query");
    }
    let bytes = file.metadata()?.len();
    Ok(SealedMeta {
        path: active.path.clone(),
        idx_path: idx_path.to_path_buf(),
        slug: active.slug.clone(),
        start_ms: active.start_ms,
        seq: active.seq,
        first_ts_ms: footer.first_ts_ms,
        last_ts_ms: footer.last_ts_ms,
        record_count: footer.record_count,
        uncomp_bytes: footer.uncomp_bytes,
        bytes,
    })
}

struct ScannedRecords {
    index: ActiveIndex,
    record_count: u64,
    first_ts_ms: Option<i64>,
    last_ts_ms: Option<i64>,
}

/// Walk flushed blocks, decoding every record into an active-style index.
/// Used by salvage and by the query-side idx self-heal.
fn decode_blocks(file: &std::fs::File, blocks: &[BlockLoc]) -> io::Result<ScannedRecords> {
    let mut out = ScannedRecords {
        index: HashMap::new(),
        record_count: 0,
        first_ts_ms: None,
        last_ts_ms: None,
    };
    for block in blocks {
        let payload = read_block_at(file, block.file_pos)?;
        segment::for_each_record(&payload, |frame, rel_off| {
            let entry = segment::decode_record(frame)?;
            out.record_count += 1;
            if let Some(ts) = segment::entry_ts_ms(&entry) {
                out.first_ts_ms = Some(out.first_ts_ms.map_or(ts, |t| t.min(ts)));
                out.last_ts_ms = Some(out.last_ts_ms.map_or(ts, |t| t.max(ts)));
            }
            out.index
                .entry(id_hash(&entry.request_id))
                .or_default()
                .push(ActiveHit {
                    phase: segment::phase_to_u8(entry.phase),
                    abs_off: block.abs_start + rel_off,
                });
            Ok(())
        })?;
    }
    Ok(out)
}

/// Rebuild a segment's index from its content (query self-heal path).
fn rebuild_idx_from_segment(path: &Path) -> io::Result<SegmentIndex> {
    let file = std::fs::File::open(path)?;
    let mut reader = &file;
    let Some(header) = read_header(&mut reader)? else {
        return Err(io::Error::other(format!("{path:?}: not a segment file")));
    };
    let (blocks, _) = scan_blocks(&file, header.data_offset)?;
    let scanned = decode_blocks(&file, &blocks)?;
    Ok(SegmentIndex::build(&scanned.index, &blocks, scanned.record_count))
}

/// Re-derive a segment's sealed form from its content: scan valid blocks,
/// truncate any garbage or partial footer, append a fresh footer, rebuild
/// and persist the index. Returns `None` when the file holds no records —
/// a crashed active segment that never got a flush — and removes the empty
/// file. Idempotent for already-sealed segments: the same bytes come out.
fn salvage_segment(path: &Path, idx_path: &Path, seq: u64) -> io::Result<Option<SealedMeta>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)?;
    let Some(header) = read_header(&mut file)? else {
        tracing::warn!(path = ?path, "unrecognized file with a segment name; ignoring");
        return Ok(None);
    };
    let (blocks, valid_end) = scan_blocks(&file, header.data_offset)?;
    if blocks.is_empty() {
        drop(file);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(idx_path);
        return Ok(None);
    }
    let scanned = decode_blocks(&file, &blocks)?;
    file.set_len(valid_end)?;
    let footer = SegmentFooter {
        block_count: blocks.len() as u64,
        record_count: scanned.record_count,
        first_ts_ms: scanned.first_ts_ms.unwrap_or(header.start_ts_ms),
        last_ts_ms: scanned.last_ts_ms.unwrap_or(header.start_ts_ms),
        uncomp_bytes: blocks.iter().map(|b| b.uncomp_len as u64).sum(),
    };
    segment::write_footer(&mut file, &footer)?;
    file.sync_all()?;
    let idx = SegmentIndex::build(&scanned.index, &blocks, scanned.record_count);
    if let Err(e) = idx.write(idx_path) {
        tracing::warn!(path = ?idx_path, error = %e, "salvage idx write failed; self-heals on query");
    }
    let bytes = file.metadata()?.len();
    Ok(Some(SealedMeta {
        path: path.to_path_buf(),
        idx_path: idx_path.to_path_buf(),
        slug: header.slug,
        start_ms: header.start_ts_ms,
        seq,
        first_ts_ms: footer.first_ts_ms,
        last_ts_ms: footer.last_ts_ms,
        record_count: footer.record_count,
        uncomp_bytes: footer.uncomp_bytes,
        bytes,
    }))
}

/// Rebuild the sealed registry from the directory — the recovery path and
/// the proof there is no manifest to corrupt. Foreign-instance segments and
/// unrecognized files are ignored, never modified: mis-deleting is worse
/// than leaving orphans (宁漏勿错).
fn recover_directory(dir: &Path, slug: &str) -> io::Result<(Vec<SealedMeta>, u64)> {
    let mut sealed = Vec::new();
    let mut next_seq = 0u64;
    let mut seg_stems: HashSet<String> = HashSet::new();
    let mut idx_paths: Vec<PathBuf> = Vec::new();

    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();

    for name in &names {
        // Leftover from an idx atomic rename — never referenced.
        if name.ends_with(".idx.tmp") {
            let _ = std::fs::remove_file(dir.join(name));
            continue;
        }
        if name.ends_with(&format!(".{IDX_EXT}")) {
            let stem = name.strip_suffix(&format!(".{IDX_EXT}")).unwrap();
            if segment::parse_segment_stem(stem).is_some() {
                idx_paths.push(dir.join(name));
            }
            continue;
        }
        let Some(parsed) = segment::parse_segment_file_name(name) else {
            continue;
        };
        seg_stems.insert(name.strip_suffix(&format!(".{SEG_EXT}")).unwrap().to_string());
        if parsed.slug != slug {
            tracing::warn!(%name, "foreign instance segment in directory; ignoring");
            continue;
        }
        next_seq = next_seq.max(parsed.seq + 1);
        let path = dir.join(name);
        let idx_path = path.with_extension(IDX_EXT);
        match read_sealed_meta(&path) {
            Err(e) => {
                tracing::warn!(%name, error = %e, "segment meta read failed; ignoring");
            }
            Ok(Some((header, footer, bytes))) => {
                if idx_path.exists() {
                    sealed.push(SealedMeta {
                        path: path.clone(),
                        idx_path: idx_path.clone(),
                        slug: header.slug,
                        start_ms: header.start_ts_ms,
                        seq: parsed.seq,
                        first_ts_ms: footer.first_ts_ms,
                        last_ts_ms: footer.last_ts_ms,
                        record_count: footer.record_count,
                        uncomp_bytes: footer.uncomp_bytes,
                        bytes,
                    });
                } else {
                    // Sealed but the idx was lost (crash between footer and
                    // idx write) — re-derive it.
                    match salvage_segment(&path, &idx_path, parsed.seq) {
                        Ok(Some(meta)) => sealed.push(meta),
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(%name, error = %e, "segment idx rebuild failed; ignoring")
                        }
                    }
                }
            }
            Ok(None) => {
                // Crash mid-write or mid-seal — salvage the valid prefix.
                match salvage_segment(&path, &idx_path, parsed.seq) {
                    Ok(Some(meta)) => sealed.push(meta),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(%name, error = %e, "segment salvage failed; ignoring")
                    }
                }
            }
        }
    }

    // Idx files whose segment is gone (aging crashed between the two
    // unlinks) are stale derived data — sweep them.
    for idx_path in idx_paths {
        if let Some(stem) = idx_path.file_stem().and_then(|s| s.to_str()) {
            if !seg_stems.contains(stem) {
                let _ = std::fs::remove_file(&idx_path);
            }
        }
    }

    sealed.sort_by_key(|m| (m.start_ms, m.seq));
    Ok((sealed, next_seq))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: &Path) -> Arc<ArcSwap<PromptLogConfig>> {
        let c = PromptLogConfig {
            dir: dir.to_string_lossy().into_owned(),
            ..Default::default()
        };
        Arc::new(ArcSwap::from_pointee(c))
    }

    fn req_entry(id: &str) -> PromptLogEntry {
        PromptLogEntry::new_request(
            id,
            None,
            "kh",
            None,
            None,
            "m",
            "/p",
            false,
            Arc::new(serde_json::json!({"q": id})),
            None,
            None,
        )
    }

    fn resp_entry(id: &str) -> PromptLogEntry {
        let mut e = PromptLogEntry::new_response_from(&req_entry(id));
        e.set_response(Arc::new(serde_json::json!({"a": id})));
        e
    }

    #[tokio::test]
    async fn find_in_active_before_and_after_flush() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentStore::open_sync(test_config(dir.path()), "gw-test").unwrap();
        store.append_entry(&req_entry("r1")).unwrap();
        store.append_entry(&resp_entry("r1")).unwrap();

        // Unflushed tail path (in-memory).
        let pair = store.find_pair_sync("r1");
        assert_eq!(pair.request.as_ref().unwrap().request_id, "r1");
        assert!(pair.response.is_some());
        assert!(store.find_pair_sync("absent").request.is_none());

        // Flushed block path (file read).
        store.flush_active_block().await.unwrap();
        let pair = store.find_pair_sync("r1");
        assert!(pair.complete());
        let resp = store.find_sync("r1", LogPhase::Response).unwrap();
        assert_eq!(resp.phase, LogPhase::Response);
        assert_eq!(store.stats().active_records, 2);
    }

    #[tokio::test]
    async fn seal_then_find_via_idx() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentStore::open_sync(test_config(dir.path()), "gw-test").unwrap();
        store.append_entry(&req_entry("r1")).unwrap();
        store.append_entry(&resp_entry("r1")).unwrap();
        store.seal_active().await.unwrap();

        let st = store.stats();
        assert_eq!(st.sealed_segments, 1);
        assert_eq!(st.sealed_records, 2);
        assert_eq!(st.active_records, 0);
        assert!(st.oldest_ts_ms.is_some());
        assert_eq!(store.seal_epoch(), 1);

        let pair = store.find_pair_sync("r1");
        assert!(pair.complete());
        assert!(store.find_pair_sync("absent").request.is_none());
    }

    #[tokio::test]
    async fn pair_across_seal_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentStore::open_sync(test_config(dir.path()), "gw-test").unwrap();
        store.append_entry(&req_entry("r1")).unwrap();
        store.seal_active().await.unwrap();
        store.append_entry(&resp_entry("r1")).unwrap();

        let pair = store.find_pair_sync("r1");
        assert!(pair.complete());
        let st = store.stats();
        assert_eq!(st.sealed_records, 1);
        assert_eq!(st.active_records, 1);
    }

    #[tokio::test]
    async fn remove_sealed_deletes_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentStore::open_sync(test_config(dir.path()), "gw-test").unwrap();
        store.append_entry(&req_entry("r1")).unwrap();
        store.seal_active().await.unwrap();
        store.append_entry(&req_entry("r2")).unwrap();
        store.seal_active().await.unwrap();
        assert_eq!(store.stats().sealed_segments, 2);

        let metas = store.sealed_metas();
        store.remove_sealed(&metas[0]).unwrap();
        assert!(!metas[0].path.exists());
        assert!(!metas[0].idx_path.exists());
        assert_eq!(store.stats().sealed_segments, 1);
        // The removed segment no longer answers.
        assert!(store.find_pair_sync("r1").request.is_none());
        assert!(store.find_pair_sync("r2").request.is_some());
    }

    #[tokio::test]
    async fn reopen_recovers_registry_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = SegmentStore::open_sync(test_config(dir.path()), "gw-test").unwrap();
            store.append_entry(&req_entry("r1")).unwrap();
            store.seal_active().await.unwrap();
            store.append_entry(&req_entry("r2")).unwrap();
            store.flush_active_block().await.unwrap();
            // Drop without sealing the second segment — the restart must
            // salvage it (blocks on disk, no footer).
        }
        let store2 = SegmentStore::open_sync(test_config(dir.path()), "gw-test").unwrap();
        let st = store2.stats();
        assert_eq!(st.sealed_segments, 2);
        assert_eq!(st.sealed_records, 2);
        assert!(store2.find_pair_sync("r1").request.is_some());
        assert!(store2.find_pair_sync("r2").request.is_some());
        // next_seq advanced past everything recovery saw. store2's active is
        // empty (both records were salvaged), so sealing it rotates the file
        // (seq 2 deleted, seq 3 created) instead of keeping a 0-record
        // segment — 2 sealed + 1 fresh active on disk, no filename collision
        // (create_new would have failed).
        store2.seal_active().await.unwrap();
        let names: Vec<String> = std::fs::read_dir(dir.path().join("segments"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "seg"))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        assert_eq!(names.len(), 3);
    }

    #[tokio::test]
    async fn recovery_salvages_crash_states() {
        let dir = tempfile::tempdir().unwrap();
        let seg_dir = dir.path().join("segments");
        std::fs::create_dir_all(&seg_dir).unwrap();

        // A crashed active segment: header + one valid block, no footer.
        let crashed = seg_dir.join(segment::segment_file_name("gw-test", 1000, 0));
        {
            let mut f = std::fs::OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(&crashed)
                .unwrap();
            segment::write_header(&mut f, "gw-test", 1000).unwrap();
            let mut payload = Vec::new();
            payload.extend_from_slice(&segment::encode_record_frame(&req_entry("r1")).unwrap());
            payload.extend_from_slice(&segment::encode_record_frame(&resp_entry("r1")).unwrap());
            let block = prepare_block(&payload, 2, 1).unwrap();
            f.write_all(&block).unwrap();
        }

        // An empty crashed active (header only) — must be deleted.
        let empty = seg_dir.join(segment::segment_file_name("gw-test", 2000, 1));
        {
            let mut f = std::fs::OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(&empty)
                .unwrap();
            segment::write_header(&mut f, "gw-test", 2000).unwrap();
        }

        // A foreign instance's segment — must survive untouched.
        let foreign = seg_dir.join(segment::segment_file_name("other-host", 3000, 5));
        {
            let mut f = std::fs::OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(&foreign)
                .unwrap();
            segment::write_header(&mut f, "other-host", 3000).unwrap();
        }

        // An orphan idx (aging crashed between unlinks) — must be swept.
        let orphan_idx = seg_dir.join(format!("{}{}", "gw-test-4000-000009", ".idx"));
        std::fs::write(&orphan_idx, b"junk").unwrap();

        let store = SegmentStore::open_sync(test_config(dir.path()), "gw-test").unwrap();
        let pair = store.find_pair_sync("r1");
        assert!(pair.complete(), "salvaged segment must be queryable");
        assert!(!empty.exists(), "empty crashed active must be removed");
        assert!(foreign.exists(), "foreign segment must be untouched");
        assert!(!orphan_idx.exists(), "orphan idx must be swept");
        assert_eq!(store.stats().sealed_segments, 1);
    }

    #[tokio::test]
    async fn should_seal_by_stream_size() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = {
            let c = PromptLogConfig {
                dir: dir.path().to_string_lossy().into_owned(),
                segment: crate::config::SegmentConfig {
                    max_segment_mb: 16, // clamped minimum
                    ..Default::default()
                },
                ..Default::default()
            };
            Arc::new(ArcSwap::from_pointee(c))
        };
        let store = SegmentStore::open_sync(cfg, "gw-test").unwrap();
        assert!(!store.should_seal());
        // Force the uncompressed stream past 16MB with raw frames.
        let frame = vec![0u8; 1024 * 1024];
        for _ in 0..16 {
            store.append_frame(&frame, 1, 0, None).unwrap();
        }
        assert!(store.should_seal());
    }

    #[tokio::test]
    async fn idx_self_heals_after_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentStore::open_sync(test_config(dir.path()), "gw-test").unwrap();
        store.append_entry(&req_entry("r1")).unwrap();
        store.seal_active().await.unwrap();
        let metas = store.sealed_metas();
        std::fs::remove_file(&metas[0].idx_path).unwrap();
        // Query must rebuild the index from the segment content.
        let pair = store.find_pair_sync("r1");
        assert!(pair.request.is_some());
        assert!(metas[0].idx_path.exists(), "rebuilt idx must be persisted");
    }
}
