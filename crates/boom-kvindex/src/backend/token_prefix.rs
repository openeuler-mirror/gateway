use boom_core::kv_event::{GatewayKvEvent, KvMatchResult, StorageTier};
use dashmap::DashMap;
use lru::LruCache;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use super::KvIndexBackend;

// ── TTL prune timers (priority-heap, O(k log n) pop) ─────────────────────

struct PruneTimers {
    timers: HashMap<(String, String, u64), std::time::Instant>,
    expirations: std::collections::BinaryHeap<
        std::cmp::Reverse<(std::time::Instant, (String, String, u64))>,
    >,
}

impl PruneTimers {
    fn new() -> Self {
        Self {
            timers: HashMap::new(),
            expirations: std::collections::BinaryHeap::new(),
        }
    }
    fn insert(&mut self, key: (String, String, u64), now: std::time::Instant) {
        self.timers.insert(key.clone(), now);
        self.expirations.push(std::cmp::Reverse((now, key)));
    }
    fn remove(&mut self, key: &(String, String, u64)) {
        self.timers.remove(key);
    }
    fn remove_worker(&mut self, worker_id: &str) {
        self.timers.retain(|k, _| k.1 != worker_id);
    }
    fn pop_expired(&mut self, ttl: std::time::Duration) -> Vec<(String, String, u64)> {
        let now = std::time::Instant::now();
        let mut expired = Vec::new();
        while let Some(std::cmp::Reverse((store_time, _))) = self.expirations.peek() {
            if now.duration_since(*store_time) < ttl {
                break;
            }
            let std::cmp::Reverse((store_time, key)) = self.expirations.pop().unwrap();
            if self.timers.get(&key) == Some(&store_time) {
                self.timers.remove(&key);
                expired.push(key);
            }
        }
        expired
    }
}

// ── Trie node (per-node Arc<RwLock>) ──────────────────────────────────────

type SharedBlock = Arc<RwLock<Block>>;

#[derive(Default)]
struct Block {
    children: HashMap<u64, SharedBlock>,
    workers: HashMap<String, StorageTier>,
}

// ── TokenPrefixIndex ──────────────────────────────────────────────────────

pub struct TokenPrefixIndex {
    /// Per-model trie root. Each root is an Arc<RwLock<Block>> — per-node
    /// locking, not per-model. find_matches on model A doesn't block
    /// StoreBatch on model A's sibling branches.
    tries: DashMap<String, SharedBlock>,
    /// Per-block reverse lookup: (model, worker_id, hash) → SharedBlock.
    /// O(1) eviction — no BFS needed. Mirrors dynamo's WorkerLookup.
    block_lookup: DashMap<(String, String, u64), SharedBlock>,
    /// LRU capacity management. Key = (model, worker_id, effective_hash).
    lru_queue: Mutex<LruCache<(String, String, u64), ()>>,
    /// TTL prune timers (priority-heap).
    prune_timers: Mutex<PruneTimers>,
    /// O(1) mirror of block_lookup.len() — read on every routing selection,
    /// so iterating a 300k-entry DashMap per request is not acceptable.
    blocks: AtomicUsize,
    /// Live trie NODE count (roots + interior + leaves). Diagnostic: unlike
    /// `blocks` (claims, capped by max_blocks LRU), node count is NOT capped
    /// by configuration — shells reclaimed by sweep_stale decrement it. The
    /// gap `nodes - blocks` is the shell population, the memory signal hidden
    /// from Trie Fill.
    nodes: AtomicUsize,
    /// Stale-sweep throttle state (millis since construction).
    sweep_clock: std::time::Instant,
    last_sweep_ms: AtomicU64,
    /// Set by every shell-producing path (evict_single_block / remove_worker;
    /// TTL prune goes through evict_single_block). sweep_stale consumes it as
    /// an O(1) gate: with no eviction since the last completed sweep there is
    /// provably nothing to reclaim, so the full-tree BFS is skipped entirely
    /// (steady-state tries used to pay a whole-tree walk every tick).
    sweep_pending: AtomicBool,
    /// How many full-tree sweeps actually ran (gate passed). Diagnostic:
    /// with the gate this stays flat in steady state and only moves after
    /// eviction bursts.
    sweep_attempts: AtomicU64,
    block_size: usize,
}

/// Minimum spacing between stale sweeps. Sweeps only unlink leaf nodes, so
/// they are cheap; the throttle keeps a giant burst of evictions from
/// turning into a continuous full-tree walk.
const SWEEP_INTERVAL_MS: u64 = 60 * 1000;

/// Compute xxhash3-64 of a block's raw bytes (trie edge key).
#[inline]
fn hash_block_bytes(bytes: &[u8]) -> u64 {
    twox_hash::xxhash3_64::Hasher::oneshot(bytes)
}

/// Chain-dependent block key: hash(parent_effective ‖ content_hash). The SAME
/// block content at two trie positions (repeated chunks are common in long
/// contexts) must map to DIFFERENT block_lookup keys — otherwise the second
/// insert overwrites the first's lookup entry and the first node's claim
/// becomes unreachable by eviction, leaving an immortal shell (sweep's
/// workers-empty gate never passes). Mirrors vLLM's chain-scoped block hash.
#[inline]
fn chain_block_hash(parent_effective: u64, content_hash: u64) -> u64 {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&parent_effective.to_le_bytes());
    buf[8..].copy_from_slice(&content_hash.to_le_bytes());
    hash_block_bytes(&buf)
}

impl TokenPrefixIndex {
    pub fn new(block_size: usize, max_blocks: usize) -> Self {
        let cap = NonZeroUsize::new(max_blocks).unwrap_or(NonZeroUsize::new(500_000).unwrap());
        Self {
            tries: DashMap::new(),
            block_lookup: DashMap::new(),
            lru_queue: Mutex::new(LruCache::new(cap)),
            prune_timers: Mutex::new(PruneTimers::new()),
            blocks: AtomicUsize::new(0),
            nodes: AtomicUsize::new(0),
            sweep_clock: std::time::Instant::now(),
            // u64::MAX = "never swept" sentinel: the FIRST sweep runs
            // immediately (the gate skips the interval check for the
            // sentinel); after that, throttled to SWEEP_INTERVAL_MS.
            // NOTE: the gate must compare against the sentinel explicitly —
            // `now_ms.saturating_sub(u64::MAX)` saturates to 0, which would
            // make every call return early (sweep never runs).
            last_sweep_ms: AtomicU64::new(u64::MAX),
            sweep_pending: AtomicBool::new(false),
            sweep_attempts: AtomicU64::new(0),
            block_size,
        }
    }

    // ── Eviction: O(1) via block_lookup, NON-CASCADING ─────────────────────
    //
    // Eviction never touches children. Removing the worker from a node may
    // leave an empty shell linked in the trie; `sweep_stale` reclaims shells
    // later, one leaf at a time (drop depth is always 1). This mirrors
    // dynamo's `apply_removed` ("does NOT cascade to descendants") — the old
    // `children.clear()` cascaded into a recursive Drop whose depth equals
    // the prefix-chain length (thousands of blocks for long contexts) and
    // overflowed the 2 MiB tokio worker stack.

    fn lru_evict_block(&self, model: &str, worker_id: &str, hash: u64) {
        if hash == 0 {
            return;
        }
        self.evict_single_block(model, worker_id, hash);
        let key = (model.to_string(), worker_id.to_string(), hash);
        self.prune_timers.lock().remove(&key);
        if let Some(mut lru) = self.lru_queue.try_lock() {
            lru.pop(&key);
        }
    }

    /// O(1) eviction via block_lookup: remove the worker's claim on the node.
    /// Does NOT drop or unlink children — stale empty shells are reclaimed by
    /// `sweep_stale`.
    fn evict_single_block(&self, model: &str, worker_id: &str, hash: u64) {
        let key = (model.to_string(), worker_id.to_string(), hash);
        let node = match self.block_lookup.remove(&key) {
            Some((_, n)) => n,
            None => return, // already evicted or never recorded
        };
        // Claim removed → a shell may be left behind. Arm the sweep gate.
        self.sweep_pending.store(true, Ordering::Release);
        // Blocking write is safe here: node locks are only ever held briefly
        // and always parent-before-child, and this path holds no other lock
        // while acquiring the node, so no lock-order cycle is possible.
        {
            let mut guard = node.write();
            guard.workers.remove(worker_id);
        }
        self.blocks.fetch_sub(1, Ordering::Relaxed);
    }
}

// ── Iterative Drop — avoid recursive Arc drop stack overflow ──────────────

impl Drop for TokenPrefixIndex {
    fn drop(&mut self) {
        // STEP 1: release every secondary reference FIRST. block_lookup holds
        // an Arc to every interior node; while those exist, the try_unwrap
        // below always fails and the "iterative" drain is a no-op — the real
        // release then happens during field drops as a recursive chain
        // cascade (stack overflow on deep prefix chains). Clearing the lookup
        // drops each entry's Arc one at a time (the trie edge still holds a
        // reference, so no node actually drops here — depth stays 1).
        self.block_lookup.clear();
        self.lru_queue.lock().clear();
        // STEP 2: now the trie edges are the only owners, so try_unwrap
        // succeeds and the drain is genuinely iterative.
        for entry in self.tries.iter() {
            let root = entry.value().clone();
            let mut stack: Vec<SharedBlock> = {
                let mut guard = root.write();
                guard.children.drain().map(|(_, v)| v).collect()
            };
            while let Some(block) = stack.pop() {
                match Arc::try_unwrap(block) {
                    Ok(rwlock) => {
                        let mut inner = rwlock.into_inner();
                        stack.extend(inner.children.drain().map(|(_, v)| v));
                    }
                    Err(_) => { /* still referenced — let Arc handle it */ }
                }
            }
        }
    }
}

// ── KvIndexBackend impl ───────────────────────────────────────────────────

impl KvIndexBackend for TokenPrefixIndex {
    fn apply_event(&self, event: &GatewayKvEvent) {
        match event {
            GatewayKvEvent::Store {
                model,
                worker_id,
                local_hash,
                block_bytes,
                parent_hash,
                storage_tier,
                block_size: _,
                ..
            } => {
                if block_bytes.is_empty() {
                    return;
                }
                let trie_key = hash_block_bytes(block_bytes);
                let effective_hash = if *local_hash == 0 { trie_key } else { *local_hash };
                // Get root Arc — scope-limit the DashMap RefMut to avoid holding
                // the shard write lock during LRU eviction (which calls tries.get).
                let root_arc = {
                    let entry = self
                        .tries
                        .entry(model.to_string())
                        .or_insert_with(|| {
                            self.nodes.fetch_add(1, Ordering::Relaxed);
                            Arc::new(RwLock::new(Block::default()))
                        });
                    entry.clone()
                }; // RefMut dropped — shard lock released
                // Find parent via block_lookup (O(1)) or fall back to root
                let parent = match parent_hash {
                    None => root_arc.clone(),
                    Some(ph) => {
                        let key = (model.to_string(), worker_id.clone(), *ph);
                        self.block_lookup.get(&key).map(|r| r.clone()).unwrap_or_else(|| root_arc.clone())
                    }
                };
                let child = {
                    let mut guard = parent.write();
                    if !guard.children.contains_key(&trie_key) {
                        self.nodes.fetch_add(1, Ordering::Relaxed);
                    }
                    guard
                        .children
                        .entry(trie_key)
                        .or_insert_with(|| Arc::new(RwLock::new(Block::default())))
                        .clone()
                };
                {
                    let mut cguard = child.write();
                    cguard.workers.insert(worker_id.clone(), *storage_tier);
                }
                // Register in block_lookup for O(1) eviction
                if self
                    .block_lookup
                    .insert(
                        (model.to_string(), worker_id.clone(), effective_hash),
                        child.clone(),
                    )
                    .is_none()
                {
                    self.blocks.fetch_add(1, Ordering::Relaxed);
                }
                // LRU + prune_timers
                let key = (model.to_string(), worker_id.clone(), effective_hash);
                let now = std::time::Instant::now();
                let mut lru_evicted_key: Option<(String, String, u64)> = None;
                {
                    let mut timers = self.prune_timers.lock();
                    let mut lru = self.lru_queue.lock();
                    timers.insert(key.clone(), now);
                    if lru.get(&key).is_none() {
                        if let Some(((m, w, h), _)) = lru.push(key, ()) {
                            lru_evicted_key = Some((m, w, h));
                        }
                    }
                } // timers + lru dropped
                if let Some((m, w, h)) = lru_evicted_key {
                    self.lru_evict_block(&m, &w, h);
                }
            }

            GatewayKvEvent::Remove { worker_id, .. } => {
                self.remove_worker(worker_id);
            }

            GatewayKvEvent::EvictBlocks {
                model,
                worker_id,
                block_hashes,
                ..
            } => {
                for &hash in block_hashes {
                    if hash == 0 {
                        continue;
                    }
                    self.evict_single_block(model, worker_id, hash);
                    let key = (model.to_string(), worker_id.to_string(), hash);
                    self.prune_timers.lock().remove(&key);
                }
                {
                    let mut lru = self.lru_queue.lock();
                    for &hash in block_hashes {
                        if hash == 0 {
                            continue;
                        }
                        lru.pop(&(model.to_string(), worker_id.to_string(), hash));
                    }
                }
            }

            GatewayKvEvent::StoreBatch {
                model,
                worker_id,
                blocks,
            } => {
                self.apply_store_batch(model, worker_id, blocks.clone());
            }
        }
    }

    fn find_matches(
        &self,
        model: &str,
        prefix_bytes: &[u8],
        candidate_worker_ids: &[String],
    ) -> Vec<KvMatchResult> {
        if prefix_bytes.is_empty() || candidate_worker_ids.is_empty() {
            return Vec::new();
        }
        let n_full = prefix_bytes.len() / self.block_size;
        if n_full == 0 || self.block_size == 0 {
            return Vec::new();
        }
        let request_hashes: Vec<u64> = (0..n_full)
            .map(|i| {
                hash_block_bytes(
                    &prefix_bytes[i * self.block_size..(i + 1) * self.block_size],
                )
            })
            .collect();

        let candidate_set: HashSet<String> =
            candidate_worker_ids.iter().cloned().collect();

        let root = match self.tries.get(model) {
            Some(r) => r.clone(),
            None => return Vec::new(),
        };

        let mut current = root;
        let mut matched_workers = candidate_set.clone();
        let mut worker_depth: HashMap<String, u64> = HashMap::new();

        for (depth, &hash_key) in request_hashes.iter().enumerate() {
            // Hand-over-hand read: lock current → clone child Arc → release
            let next = {
                let guard = current.read();
                guard.children.get(&hash_key).cloned()
            };
            let Some(child) = next else { break };

            let mut still_matched = HashSet::new();
            {
                let child_guard = child.read();
                for w in child_guard.workers.keys() {
                    if matched_workers.contains(w) {
                        still_matched.insert(w.clone());
                        worker_depth.insert(w.clone(), (depth + 1) as u64);
                    }
                }
            }
            if still_matched.is_empty() {
                break;
            }
            matched_workers = still_matched;
            current = child;
        }

        let total_blocks = request_hashes.len() as f64;
        let mut results: Vec<KvMatchResult> = Vec::new();
        for (wid, depth) in &worker_depth {
            if *depth == 0 {
                continue;
            }
            let hit_ratio = *depth as f64 / total_blocks;
            results.push(KvMatchResult {
                worker_id: wid.clone(),
                match_depth: *depth,
                total_blocks: total_blocks as u64,
                hit_ratio,
                load_score: 0.0,
                // Pure affinity signal: the policy scores on hit_ratio alone
                // (load balancing is handled outside the index).
                combined_score: hit_ratio,
            });
        }
        results.sort_by(|a, b| {
            b.combined_score
                .partial_cmp(&a.combined_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results
    }

    /// Remove a worker's claims by walking its block_lookup entries — no trie
    /// traversal, no children.clear() (aligned with dynamo's
    /// remove_worker_blocks). Empty shells left behind are reclaimed by
    /// `sweep_stale`.
    fn remove_worker(&self, worker_id: &str) {
        // Collect the worker's lookup entries first (pin the nodes), then
        // remove each claim under the node's write lock. Only ever a single
        // node lock held at a time — no lock-order hazard. The counter tracks
        // actual lookup removals (the same node can be filed under two keys,
        // so nodes.len() would over-subtract).
        let (nodes, removed): (Vec<SharedBlock>, usize) = {
            let mut nodes = Vec::new();
            let mut to_remove = Vec::new();
            for entry in self.block_lookup.iter() {
                if entry.key().1 == worker_id {
                    nodes.push(entry.value().clone());
                    to_remove.push(entry.key().clone());
                }
            }
            let mut removed = 0usize;
            for key in to_remove {
                if self.block_lookup.remove(&key).is_some() {
                    removed += 1;
                }
            }
            (nodes, removed)
        };
        for node in nodes {
            let mut guard = node.write();
            guard.workers.remove(worker_id);
        }
        self.blocks.fetch_sub(removed, Ordering::Relaxed);
        if removed > 0 {
            // Claims removed → shells may remain under every touched node.
            // Arm the sweep gate (remove_worker doesn't go through
            // evict_single_block, so it must set the flag itself).
            self.sweep_pending.store(true, Ordering::Release);
        }
        // Clean LRU + prune_timers for this worker
        self.prune_timers.lock().remove_worker(worker_id);
        {
            let mut lru = self.lru_queue.lock();
            let keys_to_remove: Vec<(String, String, u64)> = lru
                .iter()
                .filter(|((_, wid, _), _)| wid == worker_id)
                .map(|(k, _)| k.clone())
                .collect();
            for key in keys_to_remove {
                lru.pop(&key);
            }
        }
    }

    fn node_count(&self) -> usize {
        self.nodes.load(Ordering::Relaxed)
    }

    fn block_count(&self) -> usize {
        self.blocks.load(Ordering::Relaxed)
    }

    fn prefix_block_count(&self, prefix_bytes: &[u8]) -> u64 {
        if prefix_bytes.is_empty() || self.block_size == 0 {
            return 0;
        }
        (prefix_bytes.len() / self.block_size) as u64
    }

    fn record_request_prefix(
        &self,
        model: &str,
        worker_id: &str,
        prefix_bytes: &[u8],
        storage_tier: StorageTier,
    ) {
        if self.block_size == 0 || prefix_bytes.is_empty() {
            return;
        }
        let n_full = prefix_bytes.len() / self.block_size;
        let mut prepared: Vec<(u64, u64, StorageTier)> = Vec::with_capacity(n_full);
        // Track the chain-scoped effective hash: eff(0) = content_hash(0),
        // eff(i) = chain_block_hash(eff(i-1), content_hash(i)). Identical
        // content at different positions gets different lookup keys, so
        // block_lookup stays 1:1 with trie nodes and eviction (TTL/LRU) is
        // exact. The trie edge key stays the CONTENT hash (position-blind,
        // shapes the tree); the lookup/LRU key is the position-scoped eff.
        //
        // Prepared tuples go straight into apply_prepared — no BatchBlock
        // staging, no per-block byte copy, and no second hash pass (the
        // event path re-hashes block_bytes because ZMQ events carry raw
        // bytes; the record path already has both hashes).
        let mut parent_eff: Option<u64> = None;
        for i in 0..n_full {
            let chunk = &prefix_bytes[i * self.block_size..(i + 1) * self.block_size];
            let content = hash_block_bytes(chunk);
            let eff = match parent_eff {
                None => content,
                Some(p) => chain_block_hash(p, content),
            };
            prepared.push((content, eff, storage_tier));
            parent_eff = Some(eff);
        }
        self.apply_prepared(model, worker_id, prepared);
    }

    fn prune_expired(&self, ttl: std::time::Duration) {
        let expired: Vec<(String, String, u64)> = self.prune_timers.lock().pop_expired(ttl);
        if expired.is_empty() {
            return;
        }
        let mut by_worker: HashMap<(String, String), Vec<u64>> = HashMap::new();
        for (model, worker, hash) in expired {
            by_worker.entry((model, worker)).or_default().push(hash);
        }
        for ((model, worker), hashes) in by_worker {
            for hash in &hashes {
                self.evict_single_block(&model, &worker, *hash);
            }
            let mut lru = self.lru_queue.lock();
            for hash in &hashes {
                lru.pop(&(model.clone(), worker.clone(), *hash));
            }
        }
    }

    /// Reclaim stale shells left by non-cascading eviction: nodes with no
    /// workers and no children, unlinked from their parent one at a time so
    /// a node's Drop never cascades (port of dynamo kv-router's
    /// sweep_stale_children; simplified to Arc pinning instead of Weaks).
    ///
    /// Concurrency: parent write lock → child try_write, always
    /// ancestor-before-descendant (same order as apply/find). The
    /// strong_count == 2 gate (parent edge + our pinned ref) skips any node a
    /// concurrent find_matches is traversing; skipped edges are retried on a
    /// later sweep. No global lock (DashMap/Mutex) is held while a node lock
    /// is held.
    fn sweep_stale(&self) {
        // O(1) gate: shells can ONLY be created by eviction or TTL prune
        // (both funnel through evict_single_block) or by remove_worker — every
        // such path sets sweep_pending. If no such event happened since the
        // last completed sweep, there is provably nothing to reclaim: skip
        // the full-tree BFS entirely instead of walking a steady-state trie
        // (dynamo arms its cleanup the same way — from removal paths only).
        // Load-only check FIRST: consuming the flag before the throttle below
        // would lose it on an early throttle return (shells would then wait
        // for the NEXT eviction to be seen). The flag is consumed only after
        // this thread wins the throttle — a new eviction arriving after that
        // swap re-arms it for the next tick, so nothing is lost.
        if !self.sweep_pending.load(Ordering::Acquire) {
            return;
        }
        // Throttle: at most one sweep per SWEEP_INTERVAL_MS.
        let now_ms = self.sweep_clock.elapsed().as_millis() as u64;
        let last = self.last_sweep_ms.load(Ordering::Relaxed);
        // `last == u64::MAX` is the "never swept" sentinel — run immediately.
        // For any real timestamp, throttle to one sweep per SWEEP_INTERVAL_MS.
        if last != u64::MAX && now_ms.saturating_sub(last) < SWEEP_INTERVAL_MS {
            return;
        }
        if self
            .last_sweep_ms
            .compare_exchange(last, now_ms, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return; // another thread is sweeping
        }
        // This thread owns the sweep slot — NOW consume the gate. Evictions
        // arriving after this swap re-arm the flag for the next tick.
        self.sweep_pending.store(false, Ordering::Release);
        self.sweep_attempts.fetch_add(1, Ordering::Relaxed);

        // Collect model roots FIRST and release the DashMap shard read lock
        // before walking: holding `tries.iter()`'s Ref across the BFS/sort/
        // lock phases below would block `tries.entry()` (model-root creation
        // on the record path) on the same shard for the whole sweep.
        let roots: Vec<SharedBlock> = self.tries.iter().map(|e| e.value().clone()).collect();
        for root in roots {
            // Phase 1: BFS under read locks, pinning (parent, key, child)
            // edges with depth. Pinning Arcs is intentional: it makes the
            // strong_count gate below exact.
            let mut edges: Vec<(SharedBlock, u64, SharedBlock, u64)> = Vec::new();
            let mut queue: Vec<(SharedBlock, u64)> = vec![(root, 0)];
            while let Some((node, depth)) = queue.pop() {
                let children: Vec<(u64, SharedBlock)> = {
                    let guard = node.read();
                    guard
                        .children
                        .iter()
                        .map(|(k, v)| (*k, v.clone()))
                        .collect()
                };
                for (k, child) in children {
                    queue.push((child.clone(), depth + 1));
                    edges.push((node.clone(), k, child, depth + 1));
                }
            }
            // Phase 2: deepest-first, so a chain of shells unwinds bottom-up
            // within a single sweep.
            edges.sort_by(|a, b| b.3.cmp(&a.3));
            for (parent, key, child, _) in edges {
                let mut pguard = parent.write();
                let still_child = pguard
                    .children
                    .get(&key)
                    .is_some_and(|c| Arc::ptr_eq(c, &child));
                if !still_child {
                    continue; // edge already gone / replaced
                }
                let Some(cguard) = child.try_write() else {
                    continue; // contended — retry next sweep
                };
                if !cguard.workers.is_empty() || !cguard.children.is_empty() {
                    continue;
                }
                if Arc::strong_count(&child) != 2 {
                    continue; // parent edge + our pinned ref only — else in use
                }
                pguard.children.remove(&key);
                self.nodes.fetch_sub(1, Ordering::Relaxed);
                drop(cguard);
                drop(pguard);
                // `child` (our pinned ref) drops at the end of this
                // iteration: it is a leaf, so its Drop releases exactly one
                // node — no cascade.
            }
        }
    }

    fn model_names(&self) -> HashSet<String> {
        self.tries.iter().map(|e| e.key().clone()).collect()
    }

    fn debug_dump(&self) -> Vec<(String, u64, Vec<String>, StorageTier, u64)> {
        let mut result = Vec::new();
        // Collect roots first so the DashMap shard read lock is NOT held
        // during the tree walk (same pattern as sweep_stale).
        let roots: Vec<(String, SharedBlock)> = self
            .tries
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        for (model, root) in roots {
            // Iterative DFS with per-node read locks. EVERY node is popped and
            // reported uniformly (root excluded — it carries no trie edge).
            // The previous shape only reported the direct children of stack
            // nodes and pushed grandchildren at depth+2, silently skipping
            // every even-depth level.
            let mut stack: Vec<(SharedBlock, u64, Option<u64>)> = vec![(root, 0, None)];
            while let Some((node, depth, trie_key)) = stack.pop() {
                let (workers_list, tier, children): (Vec<String>, StorageTier, Vec<(u64, SharedBlock)>) = {
                    let guard = node.read();
                    let workers: Vec<String> = guard.workers.keys().cloned().collect();
                    let tier = guard
                        .workers
                        .values()
                        .max_by_key(|t| t.priority_score().to_bits())
                        .copied()
                        .unwrap_or(StorageTier::Gpu);
                    let cs: Vec<(u64, SharedBlock)> =
                        guard.children.iter().map(|(&k, v)| (k, v.clone())).collect();
                    (workers, tier, cs)
                };
                if let Some(tk) = trie_key {
                    if !workers_list.is_empty() {
                        result.push((model.clone(), tk, workers_list, tier, depth));
                    }
                }
                for (child_key, child) in children {
                    stack.push((child, depth + 1, Some(child_key)));
                }
            }
        }
        result
    }

    fn block_capacity(&self) -> usize {
        self.lru_queue.lock().cap().get()
    }

    fn set_block_capacity(&self, max_blocks: usize) {
        // Mirror `new`'s fallback for 0 (callers validate > 0, this is
        // defense in depth).
        let cap = NonZeroUsize::new(max_blocks).unwrap_or(NonZeroUsize::new(500_000).unwrap());
        // Pop the excess LRU entries ourselves BEFORE resizing: `resize`
        // silently drops overflow entries without letting us run the trie
        // cleanup, which would leave claims the LRU no longer backs.
        // Lock discipline mirrors the insert path (:301-313): collect under
        // the lock, drop it, then evict — `lru_evict_block` re-acquires
        // locks itself.
        let mut evicted: Vec<(String, String, u64)> = Vec::new();
        {
            let mut lru = self.lru_queue.lock();
            while lru.len() > cap.get() {
                match lru.pop_lru() {
                    Some(((m, w, h), _)) => evicted.push((m, w, h)),
                    None => break,
                }
            }
            lru.resize(cap);
        }
        for (m, w, h) in evicted {
            self.lru_evict_block(&m, &w, h);
        }
    }
}

// ── apply_store_batch (private, hand-over-hand write) ─────────────────────

impl TokenPrefixIndex {
    fn apply_store_batch(
        &self,
        model: &str,
        worker_id: &str,
        blocks: Vec<boom_core::kv_event::BatchBlock>,
    ) {
        if blocks.is_empty() {
            return;
        }
        // Event path: ZMQ StoreBatch carries raw block bytes (no precomputed
        // hashes), so hash each block ONCE here and delegate. Blocks with
        // local_hash == 0 fall back to the content hash as the lookup key.
        let prepared: Vec<(u64, u64, StorageTier)> = blocks
            .iter()
            .map(|b| {
                let trie_key = hash_block_bytes(&b.block_bytes);
                let effective_hash = if b.local_hash == 0 { trie_key } else { b.local_hash };
                (trie_key, effective_hash, b.storage_tier)
            })
            .collect();
        self.apply_prepared(model, worker_id, prepared);
    }

    /// Shared insert core for both entry paths. `prepared` carries
    /// `(trie_edge_key, effective/lookup hash, tier)` per block — the record
    /// path computes both hashes inline (single pass, zero copies); the event
    /// path re-derives them from raw block bytes in apply_store_batch.
    ///
    /// Trie walk FIRST: insert ALL blocks into the trie structure.
    fn apply_prepared(
        &self,
        model: &str,
        worker_id: &str,
        prepared: Vec<(u64, u64, StorageTier)>,
    ) {
        if prepared.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        // Trie walk FIRST: insert ALL blocks into the trie structure.
        // Then LRU push + eviction — so evict_single_block can find the
        // nodes in the trie (they exist by this point). If eviction ran
        // before the trie walk, the nodes wouldn't exist yet and eviction
        // would silently no-op (trie grows unbounded).
        //
        // CRITICAL: scope-limit the DashMap entry RefMut — clone the Arc
        // out and drop the guard BEFORE the trie walk. Otherwise the shard
        // write lock is held for the entire walk + LRU eviction, blocking
        // find_matches (tries.get → shard read lock) on the same model →
        // gateway freeze.
        let root_arc = {
            let entry = self
                .tries
                .entry(model.to_string())
                .or_insert_with(|| {
                    self.nodes.fetch_add(1, Ordering::Relaxed);
                    Arc::new(RwLock::new(Block::default()))
                });
            entry.clone()
        }; // RefMut dropped — shard write lock released
        let mut current = root_arc;

        for &(trie_key, effective_hash, storage_tier) in &prepared {
            // Phase A: lock current, find-or-create child, clone Arc
            let child = {
                let mut guard = current.write();
                if !guard.children.contains_key(&trie_key) {
                    self.nodes.fetch_add(1, Ordering::Relaxed);
                }
                guard
                    .children
                    .entry(trie_key)
                    .or_insert_with(|| Arc::new(RwLock::new(Block::default())))
                    .clone()
            }; // current write released

            // Phase B: lock child, insert worker claim + block_lookup
            {
                let mut child_guard = child.write();
                child_guard.workers.insert(worker_id.to_string(), storage_tier);
            } // child write released

            // Phase C: register in block_lookup for O(1) eviction
            if self
                .block_lookup
                .insert(
                    (model.to_string(), worker_id.to_string(), effective_hash),
                    child.clone(),
                )
                .is_none()
            {
                self.blocks.fetch_add(1, Ordering::Relaxed);
            }

            current = child;
        }

        // LRU push + prune_timers — AFTER trie walk so evict_single_block
        // can find nodes in the trie when processing evictions.
        let mut lru_evicted: Vec<((String, String, u64), ())> = Vec::new();
        {
            let mut timers = self.prune_timers.lock();
            let mut lru = self.lru_queue.lock();
            for &(_trie_key, effective_hash, _) in &prepared {
                let key = (model.to_string(), worker_id.to_string(), effective_hash);
                timers.insert(key.clone(), now);
                if lru.get(&key).is_none() {
                    if let Some(evicted) = lru.push(key, ()) {
                        lru_evicted.push(evicted);
                    }
                }
            }
        }
        // Process LRU evictions (trie nodes exist now → evict_single_block works)
        for ((m, w, h), _) in lru_evicted {
            self.lru_evict_block(&m, &w, h);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn store_event(model: &str, worker: &str, _hash: u64, parent_hash: Option<u64>, bytes: Vec<u8>) -> GatewayKvEvent {
        // local_hash=0 → effective_hash = trie_key = hash_block_bytes(bytes)
        // This makes parent_hash refer to the parent's trie_key consistently.
        GatewayKvEvent::Store {
            model: model.to_string(),
            worker_id: worker.to_string(),
            sequence_hash: String::new(),
            prefix_hash: String::new(),
            local_hash: 0,
            parent_hash,
            block_index: 0,
            block_bytes: bytes,
            block_size: 4,
            storage_tier: StorageTier::Gpu,
        }
    }

    #[test]
    fn test_single_root_block_match() {
        let idx = TokenPrefixIndex::new(4, 500_000);
        idx.apply_event(&store_event("m", "w0", 0, None, vec![1, 2, 3, 4]));
        let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 1);
        assert!((matches[0].hit_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_chained_blocks_with_parent() {
        let idx = TokenPrefixIndex::new(4, 500_000);
        let b1 = vec![1, 2, 3, 4];
        let b2 = vec![5, 6, 7, 8];
        let b3 = vec![9, 10, 11, 12];
        let h1 = hash_block_bytes(&b1);
        let h2 = hash_block_bytes(&b2);
        idx.apply_event(&store_event("m", "w0", 0, None, b1.clone()));
        idx.apply_event(&store_event("m", "w0", 0, Some(h1), b2.clone()));
        idx.apply_event(&store_event("m", "w0", 0, Some(h2), b3.clone()));
        let matches = idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 3);
    }

    #[test]
    fn test_multi_worker_prefix_reuse() {
        let idx = TokenPrefixIndex::new(4, 500_000);
        idx.apply_event(&store_event("m", "w0", 0, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m", "w1", 0, None, vec![1, 2, 3, 4]));
        let m0 = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        let m1 = idx.find_matches("m", &[1, 2, 3, 4], &["w1".to_string()]);
        assert_eq!(m0.len(), 1);
        assert_eq!(m0[0].worker_id, "w0");
        assert_eq!(m1.len(), 1);
        assert_eq!(m1[0].worker_id, "w1");
    }

    #[test]
    fn test_evict_single_block() {
        let idx = TokenPrefixIndex::new(4, 500_000);
        // Use record_request_prefix for chain (reliable chain building)
        idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], StorageTier::Gpu);
        // Lookup key of block 2 is chain-scoped: chain(eff₁, content₂).
        let h2 = chain_block_hash(hash_block_bytes(&[1, 2, 3, 4]), hash_block_bytes(&[5, 6, 7, 8]));
        idx.apply_event(&GatewayKvEvent::EvictBlocks {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            block_hashes: vec![h2],
            storage_tier: None,
        });
        let matches = idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 1);
    }

    #[test]
    fn test_remove_worker() {
        let idx = TokenPrefixIndex::new(4, 500_000);
        idx.apply_event(&store_event("m", "w0", 0, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m2", "w0", 0, None, vec![10, 20, 30, 40]));
        idx.apply_event(&GatewayKvEvent::Remove {
            worker_id: "w0".to_string(),
            sequence_hash: String::new(),
            storage_tier: None,
        });
        assert_eq!(idx.block_count(), 0);
        assert!(idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]).is_empty());
        assert!(idx.find_matches("m2", &[10, 20, 30, 40], &["w0".to_string()]).is_empty());
    }

    #[test]
    fn test_record_request_prefix_self_learning() {
        let idx = TokenPrefixIndex::new(4, 500_000);
        let prefix: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        idx.record_request_prefix("m", "w0", prefix, StorageTier::Gpu);
        let matches = idx.find_matches("m", prefix, &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].worker_id, "w0");
        assert_eq!(matches[0].match_depth, 3);
        assert!((matches[0].hit_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_prune_expired_removes_old_blocks() {
        let idx = TokenPrefixIndex::new(4, 500_000);
        idx.record_request_prefix("m", "w0", &[1, 2, 3, 4], StorageTier::Gpu);
        assert!(idx.block_count() > 0);
        idx.prune_expired(std::time::Duration::ZERO);
        assert_eq!(idx.block_count(), 0);
    }

    #[test]
    fn test_lru_capacity_limits_trie() {
        // max_blocks=4: inserting 8 blocks should evict the oldest 4
        let idx = TokenPrefixIndex::new(4, 4);
        for i in 0..8u64 {
            let chunk: Vec<u8> = vec![i as u8, i as u8, i as u8, i as u8];
            idx.apply_event(&store_event("m", "w0", 0, None, chunk));
        }
        // Only the last 4 blocks should survive (LRU capacity)
        assert!(idx.block_count() <= 4);
    }

    #[test]
    fn test_set_block_capacity_grow_keeps_entries() {
        let idx = TokenPrefixIndex::new(4, 4);
        for i in 0..4u64 {
            let chunk: Vec<u8> = vec![i as u8, i as u8, i as u8, i as u8];
            idx.apply_event(&store_event("m", "w0", 0, None, chunk));
        }
        assert_eq!(idx.block_count(), 4);
        idx.set_block_capacity(100);
        assert_eq!(idx.block_capacity(), 100);
        // Growing must not evict anything.
        assert_eq!(idx.block_count(), 4);
    }

    #[test]
    fn test_set_block_capacity_shrink_evicts_lru_and_cleans_trie() {
        let idx = TokenPrefixIndex::new(4, 8);
        for i in 0..8u64 {
            let chunk: Vec<u8> = vec![i as u8, i as u8, i as u8, i as u8];
            idx.apply_event(&store_event("m", "w0", 0, None, chunk));
        }
        assert_eq!(idx.block_count(), 8);
        idx.set_block_capacity(3);
        assert_eq!(idx.block_capacity(), 3);
        // Shrunk to 3: LRU eviction must also remove the trie claims, not
        // just the LRU entries (blocks counter drops in lockstep).
        assert!(idx.block_count() <= 3);
        // Insert-time eviction still works at the new capacity.
        idx.apply_event(&store_event("m", "w0", 0, None, vec![9u8; 4]));
        assert!(idx.block_count() <= 3);
    }

    #[test]
    fn test_set_block_capacity_zero_falls_back_to_default() {
        let idx = TokenPrefixIndex::new(4, 4);
        idx.set_block_capacity(0);
        assert_eq!(idx.block_capacity(), 500_000);
    }

    // ── Regression: stack overflow via recursive chain Drop ──────────────
    //
    // Before the non-cascading eviction fix, TTL-pruning a long chain
    // (batch removal of lookup entries followed by the first real Drop)
    // recursed once per chain depth and overflowed a 2 MiB worker stack at
    // a few thousand blocks. This test builds a 50_000-deep chain (1M-token
    // context scale), expires it wholesale, sweeps, and drops the index —
    // all three previously-recursive paths.

    #[test]
    fn test_deep_chain_prune_and_drop_no_stack_overflow() {
        let block_size = 64;
        // 50k blocks × 64B = 3.2MB prefix ≈ long-context chain
        let depth = 50_000;
        // Deterministic unique blocks: block i = i.to_le_bytes() padded to
        // block_size, so every block hashes differently (a real chain).
        let mut prefix: Vec<u8> = Vec::with_capacity(depth * block_size);
        for i in 0..depth as u32 {
            let b = i.to_le_bytes();
            prefix.extend_from_slice(&b);
            prefix.resize(prefix.len() + block_size - b.len(), 0);
        }
        let idx = TokenPrefixIndex::new(block_size, 1_000_000);
        idx.record_request_prefix("m", "w0", &prefix, StorageTier::Gpu);
        assert_eq!(idx.block_count(), depth);

        // Wholesale TTL expiry: every lookup entry removed in one batch,
        // then the first node drop must not cascade down the chain.
        idx.prune_expired(std::time::Duration::ZERO);
        assert_eq!(idx.block_count(), 0);

        // Sweep reclaims the empty shells one leaf at a time.
        idx.sweep_stale();

        // Whole-index drop (hot-reload path) exercises the iterative Drop.
        drop(idx);
    }

    #[test]
    fn test_deep_chain_remove_worker_no_stack_overflow() {
        let block_size = 64;
        let depth = 50_000;
        // Deterministic unique blocks: block i = i.to_le_bytes() padded to
        // block_size, so every block hashes differently (a real chain).
        let mut prefix: Vec<u8> = Vec::with_capacity(depth * block_size);
        for i in 0..depth as u32 {
            let b = i.to_le_bytes();
            prefix.extend_from_slice(&b);
            prefix.resize(prefix.len() + block_size - b.len(), 0);
        }
        let idx = TokenPrefixIndex::new(block_size, 1_000_000);
        idx.record_request_prefix("m", "w0", &prefix, StorageTier::Gpu);
        // Worker removal previously BFS'd the tree with write locks and
        // cascaded children.clear() — must now be lookup-driven, no cascade.
        idx.remove_worker("w0");
        assert_eq!(idx.block_count(), 0);
        idx.sweep_stale();
        drop(idx);
    }

    #[test]
    fn test_sweep_reclaims_empty_shells() {
        let idx = TokenPrefixIndex::new(4, 1_000_000);
        idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8], StorageTier::Gpu);
        assert_eq!(idx.block_count(), 2);
        // root + 2 block nodes.
        assert_eq!(idx.node_count(), 3);
        // Evict only the FIRST block of the chain — non-cascading eviction
        // leaves the deeper block as a shell still linked in the trie.
        let h1 = hash_block_bytes(&[1, 2, 3, 4]);
        idx.apply_event(&GatewayKvEvent::EvictBlocks {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            block_hashes: vec![h1],
            storage_tier: None,
        });
        assert_eq!(idx.block_count(), 1);
        // find_matches must no longer report a hit: the walk breaks at the
        // evicted head block (no worker claims it anymore).
        let m = idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8], &["w0".to_string()]);
        assert!(m.is_empty(), "evicted head must break the chain: {m:?}");
        // Before sweeping, the level-2 node (still holding w0's claim) must be
        // visible in debug_dump — this also guards the dump's level coverage
        // (the old dump shape skipped every even-depth level, which made an
        // earlier version of this test vacuously pass).
        let dump = idx.debug_dump();
        assert_eq!(dump.len(), 1, "level-2 claim must be dumped: {dump:?}");
        assert_eq!(dump[0].4, 2, "surviving block sits at depth 2: {dump:?}");
        // Evict the second block too — now BOTH nodes are empty shells, and
        // the deeper one is a leaf, so a single deepest-first sweep reclaims
        // the whole chain (bottom-up). (With only the head evicted, level-2
        // still holds w0's claim and level-1 still has a child — correctly
        // retained; sweep only unlinks EMPTY leaves.) Second block's lookup
        // key is chain-scoped: chain(eff₁, content₂), eff₁ = content₁.
        let h2 = chain_block_hash(h1, hash_block_bytes(&[5, 6, 7, 8]));
        idx.apply_event(&GatewayKvEvent::EvictBlocks {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            block_hashes: vec![h2],
            storage_tier: None,
        });
        assert_eq!(idx.block_count(), 0);
        // Assert on the trie structure directly (private field, same module):
        // the root must have no children left after the sweep.
        // (The pre-fix sweep gate made sweep_stale a no-op; this assertion
        // would have caught it, unlike the old dump-only check.)
        idx.sweep_stale();
        let root = idx.tries.get("m").expect("model root").value().clone();
        assert!(
            root.read().children.is_empty(),
            "shells should be swept"
        );
        assert!(idx.debug_dump().is_empty());
        // Only the root survives the sweep.
        assert_eq!(idx.node_count(), 1);
    }

    #[test]
    fn test_sweep_gate_skips_steady_state() {
        let idx = TokenPrefixIndex::new(4, 1_000_000);
        idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8], StorageTier::Gpu);
        assert_eq!(idx.block_count(), 2);
        // Recording alone creates no shells — the gate must skip the BFS.
        idx.sweep_stale();
        assert_eq!(
            idx.sweep_attempts.load(Ordering::Relaxed),
            0,
            "no eviction since construction → sweep must be skipped"
        );
        // Eviction arms the gate; the next sweep runs once and reclaims.
        let h1 = hash_block_bytes(&[1, 2, 3, 4]);
        let h2 = chain_block_hash(h1, hash_block_bytes(&[5, 6, 7, 8]));
        idx.evict_single_block("m", "w0", h1);
        idx.evict_single_block("m", "w0", h2);
        idx.sweep_stale();
        assert_eq!(idx.sweep_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(idx.node_count(), 1, "both shells reclaimed");
        // Gate was consumed → back to O(1) skip.
        idx.sweep_stale();
        assert_eq!(idx.sweep_attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_sweep_gate_armed_by_remove_worker() {
        let idx = TokenPrefixIndex::new(4, 1_000_000);
        idx.record_request_prefix("m", "w0", &[1, 2, 3, 4], StorageTier::Gpu);
        idx.sweep_stale();
        assert_eq!(idx.sweep_attempts.load(Ordering::Relaxed), 0);
        // remove_worker strips claims without going through
        // evict_single_block — it must arm the gate itself.
        idx.remove_worker("w0");
        idx.sweep_stale();
        assert_eq!(idx.sweep_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(idx.node_count(), 1, "shell reclaimed after worker removal");
    }

    #[test]
    fn test_deep_chain_sweep_gate_ttl_reclaim_no_stack_overflow() {
        let block_size = 64;
        // Same shape as test_deep_chain_prune_and_drop_no_stack_overflow, but
        // asserting the GATE: skip before TTL prune, one full iterative sweep
        // after. 50k-deep shell chain must unwind without recursion.
        let depth = 50_000;
        let mut prefix: Vec<u8> = Vec::with_capacity(depth * block_size);
        for i in 0..depth as u32 {
            let b = i.to_le_bytes();
            prefix.extend_from_slice(&b);
            prefix.resize(prefix.len() + block_size - b.len(), 0);
        }
        let idx = TokenPrefixIndex::new(block_size, 1_000_000);
        idx.record_request_prefix("m", "w0", &prefix, StorageTier::Gpu);
        assert_eq!(idx.block_count(), depth);

        // Steady state: no eviction yet → gate skips, no BFS.
        idx.sweep_stale();
        assert_eq!(idx.sweep_attempts.load(Ordering::Relaxed), 0);

        // Wholesale TTL expiry arms the gate (prune funnels through
        // evict_single_block). The next sweep must run and unwind the whole
        // 50k-shell chain iteratively (deepest-first edge list, drop depth 1).
        idx.prune_expired(std::time::Duration::ZERO);
        assert_eq!(idx.block_count(), 0);
        idx.sweep_stale();
        assert_eq!(idx.sweep_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(idx.node_count(), 1, "all 50k shells reclaimed in one gated sweep");
    }

    #[test]
    fn test_repeated_block_content_never_leaks_nodes() {
        // Reproduces the immortal-shell leak: the SAME block content at two
        // trie positions (any repeated 512B chunk in a long context) maps to
        // ONE block_lookup key (model, worker, content_hash). The second
        // insert overwrites the lookup entry, so TTL/LRU eviction can only
        // remove the claim from the LAST node — the first keeps workers={w}
        // forever and sweep_stale can never unlink it (gate requires empty
        // workers). Observed in production as trie_blocks=0 with 561k nodes
        // stuck after TTL expiry.
        let idx = TokenPrefixIndex::new(4, 1_000_000);
        // Prefix: A, B, A — block A content appears at depth 1 AND depth 3.
        idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8, 1, 2, 3, 4], StorageTier::Gpu);
        // root + A1 + B + A2 = 4 nodes; with chain-scoped lookup keys all
        // three block positions are separately counted (previously the
        // duplicate A overwrote the first lookup entry).
        assert_eq!(idx.node_count(), 4);
        assert_eq!(idx.block_count(), 3);
        // Full TTL expiry (what prune_expired does) must reach EVERY claim,
        // including the duplicated content at both positions.
        idx.prune_expired(std::time::Duration::ZERO);
        assert_eq!(idx.block_count(), 0);
        idx.sweep_stale();
        // Pre-fix: the first A node kept w0's claim (lookup pointed at the
        // second A) → sweep couldn't unlink → node_count stayed > 1.
        assert_eq!(
            idx.node_count(),
            1,
            "all shells must be reclaimed after full eviction"
        );
    }

    #[test]
    fn test_block_count_tracks_insert_and_evict() {
        let idx = TokenPrefixIndex::new(4, 1_000_000);
        idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8], StorageTier::Gpu);
        assert_eq!(idx.block_count(), 2);
        // Re-recording the same prefix must not double-count.
        idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8], StorageTier::Gpu);
        assert_eq!(idx.block_count(), 2);
        let h1 = hash_block_bytes(&[1, 2, 3, 4]);
        idx.apply_event(&GatewayKvEvent::EvictBlocks {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            block_hashes: vec![h1],
            storage_tier: None,
        });
        assert_eq!(idx.block_count(), 1);
        idx.remove_worker("w0");
        assert_eq!(idx.block_count(), 0);
    }

    // ── Record-path perf refactor guards ─────────────────────────────────
    //
    // record_request_prefix now feeds apply_prepared directly (precomputed
    // (content_hash, eff, tier) tuples) instead of staging BatchBlocks with
    // per-block byte copies that apply_store_batch re-hashed. The event path
    // (StoreBatch) maps through the same core. These tests pin the two paths
    // to identical observable state.

    #[test]
    fn test_record_path_equivalence_with_event_batch() {
        // Prefix with repeated content (A,B,A) so both the trie edge key
        // (content, position-blind) and the lookup key (eff, position-scoped)
        // are exercised with non-trivial divergence.
        let prefix: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 1, 2, 3, 4];

        // Path A: self-learning record (direct hash-passing core).
        let idx_record = TokenPrefixIndex::new(4, 1_000_000);
        idx_record.record_request_prefix("m", "w0", prefix, StorageTier::Gpu);

        // Path B: event replay — same eff chain the record path computes,
        // carried in BatchBlocks with raw bytes (apply_store_batch re-hashes).
        let mut blocks: Vec<boom_core::kv_event::BatchBlock> = Vec::new();
        let mut parent_eff: Option<u64> = None;
        for chunk in prefix.chunks(4) {
            let content = hash_block_bytes(chunk);
            let eff = parent_eff.map_or(content, |p| chain_block_hash(p, content));
            blocks.push(boom_core::kv_event::BatchBlock {
                local_hash: eff,
                parent_hash: parent_eff,
                block_bytes: chunk.to_vec(),
                block_size: 4,
                storage_tier: StorageTier::Gpu,
            });
            parent_eff = Some(eff);
        }
        let idx_event = TokenPrefixIndex::new(4, 1_000_000);
        idx_event.apply_store_batch("m", "w0", blocks);

        // Identical observable state on every surface.
        assert_eq!(idx_record.block_count(), idx_event.block_count());
        assert_eq!(idx_record.node_count(), idx_event.node_count());
        assert_eq!(idx_record.debug_dump(), idx_event.debug_dump());
        let m_rec = idx_record.find_matches("m", prefix, &["w0".to_string()]);
        let m_evt = idx_event.find_matches("m", prefix, &["w0".to_string()]);
        assert_eq!(m_rec.len(), m_evt.len());
        assert_eq!(m_rec.first().map(|m| m.match_depth), m_evt.first().map(|m| m.match_depth));
        // And eviction sees the same chain-scoped lookup keys on both paths.
        idx_record.prune_expired(std::time::Duration::ZERO);
        idx_event.prune_expired(std::time::Duration::ZERO);
        idx_record.sweep_stale();
        idx_event.sweep_stale();
        assert_eq!(idx_record.node_count(), idx_event.node_count());
        assert_eq!(idx_record.node_count(), 1);
    }

    #[test]
    fn test_deep_chain_record_and_match_100k_no_stack_overflow() {
        let block_size = 64;
        // 100k blocks × 64B = 6.4MB prefix — beyond any realistic context,
        // stressing record (new direct path), find_matches (query walk),
        // wholesale TTL prune, gated sweep, and whole-index drop in sequence.
        let depth = 100_000;
        let mut prefix: Vec<u8> = Vec::with_capacity(depth * block_size);
        for i in 0..depth as u32 {
            let b = i.to_le_bytes();
            prefix.extend_from_slice(&b);
            prefix.resize(prefix.len() + block_size - b.len(), 0);
        }
        let idx = TokenPrefixIndex::new(block_size, 10_000_000);
        idx.record_request_prefix("m", "w0", &prefix, StorageTier::Gpu);
        assert_eq!(idx.block_count(), depth);
        assert_eq!(idx.node_count(), depth + 1);

        // Query walk over the full chain: exact hit at max depth.
        let m = idx.find_matches("m", &prefix, &["w0".to_string()]);
        let best = m.first().expect("full-chain match");
        assert_eq!(best.match_depth, depth as u64);
        assert_eq!(best.total_blocks, depth as u64);
        assert!((best.hit_ratio - 1.0).abs() < 1e-9);

        idx.prune_expired(std::time::Duration::ZERO);
        assert_eq!(idx.block_count(), 0);
        idx.sweep_stale();
        assert_eq!(idx.node_count(), 1);
        drop(idx);
    }

    #[test]
    fn test_deep_chain_duplicate_content_no_stack_overflow() {
        let block_size = 64;
        // 50k-deep chain whose block contents cycle through only 256 distinct
        // values: every content hash repeats ~195 times, so the trie is a
        // 50k-deep chain over 256 edge values while the lookup keys (eff) are
        // all position-distinct. Deep-direction dedup of chain-scoped keys +
        // full reclaim, all iterative.
        let depth = 50_000;
        let mut prefix: Vec<u8> = Vec::with_capacity(depth * block_size);
        for i in 0..depth {
            prefix.extend(std::iter::repeat((i % 256) as u8).take(block_size));
        }
        let idx = TokenPrefixIndex::new(block_size, 10_000_000);
        idx.record_request_prefix("m", "w0", &prefix, StorageTier::Gpu);
        // All positions claimable → lookup stayed 1:1 with chain positions.
        assert_eq!(idx.block_count(), depth);
        assert_eq!(idx.node_count(), depth + 1);

        idx.prune_expired(std::time::Duration::ZERO);
        assert_eq!(idx.block_count(), 0, "repeated content must be fully evictable at depth");
        idx.sweep_stale();
        assert_eq!(idx.node_count(), 1, "no immortal shells from repeated content");
        drop(idx);
    }

    #[test]
    fn test_concurrent_record_and_find_no_deadlock() {
        // spawn_blocking raises real cross-thread concurrency on the record
        // path: hand-over-hand write locks (record) must interleave safely
        // with read traversals (find_matches) with no deadlock and no
        // counter drift. 8 writers × distinct workers over the SAME prefix +
        // 4 readers hammering find_matches concurrently.
        let block_size = 4;
        let n_blocks = 256;
        let mut prefix: Vec<u8> = Vec::with_capacity(n_blocks * block_size);
        for i in 0..n_blocks as u32 {
            let b = i.to_le_bytes();
            prefix.extend_from_slice(&b);
            prefix.resize(prefix.len() + block_size - b.len(), 0);
        }
        let idx = std::sync::Arc::new(TokenPrefixIndex::new(block_size, 1_000_000));
        let workers: Vec<String> = (0..8).map(|i| format!("w{i}")).collect();

        std::thread::scope(|s| {
            for w in &workers {
                let idx = idx.clone();
                let prefix = prefix.as_slice();
                let w = w.clone();
                s.spawn(move || {
                    // Re-record the same prefix: idempotent inserts under
                    // concurrent writers must not double-count.
                    for _ in 0..10 {
                        idx.record_request_prefix("m", &w, prefix, StorageTier::Gpu);
                    }
                });
            }
            let worker_ids = workers.clone();
            for _ in 0..4 {
                let idx = idx.clone();
                let prefix = prefix.as_slice();
                let worker_ids = worker_ids.clone();
                s.spawn(move || {
                    for _ in 0..1_000 {
                        let m = idx.find_matches("m", prefix, &worker_ids);
                        assert!(!m.is_empty());
                    }
                });
            }
        });

        // Shared trie shape: one chain of n_blocks nodes; every worker claims
        // each node, and each (worker, position) pair is one lookup entry.
        assert_eq!(idx.block_count(), workers.len() * n_blocks, "exact claim count, no drift");
        assert_eq!(idx.node_count(), n_blocks + 1);
        for w in &workers {
            let m = idx.find_matches("m", &prefix, std::slice::from_ref(w));
            assert_eq!(m[0].match_depth, n_blocks as u64);
        }
        // Reclaim under per-worker removal also stays exact.
        for w in &workers {
            idx.remove_worker(w);
        }
        assert_eq!(idx.block_count(), 0);
        idx.sweep_stale();
        assert_eq!(idx.node_count(), 1);
    }

    #[test]
    fn test_eviction_storm_gated_sweep_reclaims_all_shells() {
        // Sustained LRU-eviction storm: 100 sequential prefix recordings into
        // a tiny max_blocks=8 trie, so every record evicts older chains and
        // keeps the sweep gate armed throughout. One gated sweep afterwards
        // must reclaim every empty chain (deepest-first, single pass) leaving
        // exactly the live blocks.
        let block_size = 4;
        let cap = 8;
        let idx = TokenPrefixIndex::new(block_size, cap);
        let mut prefix_counter: u32 = 0;
        for _ in 0..100 {
            // Each prefix: 4 globally-unique block contents (own chain, no
            // sharing with other prefixes).
            let mut prefix: Vec<u8> = Vec::with_capacity(4 * block_size);
            for _ in 0..4 {
                let b = prefix_counter.to_le_bytes();
                prefix.extend_from_slice(&b);
                prefix_counter += 1;
            }
            idx.record_request_prefix("m", "w0", &prefix, StorageTier::Gpu);
        }
        let live = idx.block_count();
        assert_eq!(live, cap, "steady-state capacity: last 2 prefixes survive");
        assert!(
            idx.sweep_pending.load(Ordering::Acquire),
            "eviction storm must keep the gate armed"
        );

        idx.sweep_stale();
        assert_eq!(idx.sweep_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(
            idx.node_count(),
            live + 1,
            "one deepest-first pass reclaims all evicted chains, leaving root + live blocks"
        );
        // Gate consumed and no further evictions → back to O(1) skip.
        idx.sweep_stale();
        assert_eq!(idx.sweep_attempts.load(Ordering::Relaxed), 1);
    }
}
