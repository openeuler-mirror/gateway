use boom_core::kv_event::{GatewayKvEvent, KvMatchResult, StorageTier};
use dashmap::DashMap;
use lru::LruCache;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
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
    tokens: Vec<u8>,
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
    block_size: usize,
    cache_weight: f64,
    #[allow(dead_code)]
    load_weight: f64,
}

/// Compute xxhash3-64 of a block's raw bytes (trie edge key).
#[inline]
fn hash_block_bytes(bytes: &[u8]) -> u64 {
    twox_hash::xxhash3_64::Hasher::oneshot(bytes)
}

impl TokenPrefixIndex {
    pub fn new(block_size: usize, cache_weight: f64, load_weight: f64, max_blocks: usize) -> Self {
        let cap = NonZeroUsize::new(max_blocks).unwrap_or(NonZeroUsize::new(500_000).unwrap());
        Self {
            tries: DashMap::new(),
            block_lookup: DashMap::new(),
            lru_queue: Mutex::new(LruCache::new(cap)),
            prune_timers: Mutex::new(PruneTimers::new()),
            block_size,
            cache_weight,
            load_weight,
        }
    }

    // ── Eviction: O(1) via block_lookup ────────────────────────────────────

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

    /// O(1) eviction via block_lookup. Removes the worker from the node.
    /// If workers becomes empty, clears children (lazy GC via Arc).
    fn evict_single_block(&self, model: &str, worker_id: &str, hash: u64) {
        let key = (model.to_string(), worker_id.to_string(), hash);
        let node = match self.block_lookup.get(&key) {
            Some(n) => n.clone(),
            None => return, // already evicted or never recorded
        };
        drop(key);
        let mut guard = node.write();
        guard.workers.remove(worker_id);
        if guard.workers.is_empty() {
            guard.children.clear();
        }
        drop(guard);
        self.block_lookup.remove(&(model.to_string(), worker_id.to_string(), hash));
    }
}

// ── Iterative Drop — avoid recursive Arc drop stack overflow ──────────────

impl Drop for TokenPrefixIndex {
    fn drop(&mut self) {
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
                block_size,
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
                        .or_insert_with(|| Arc::new(RwLock::new(Block::default())));
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
                    guard
                        .children
                        .entry(trie_key)
                        .or_insert_with(|| Arc::new(RwLock::new(Block::default())))
                        .clone()
                };
                {
                    let mut cguard = child.write();
                    cguard.workers.insert(worker_id.clone(), *storage_tier);
                    if cguard.tokens.is_empty() {
                        cguard.tokens = block_bytes.clone();
                    }
                }
                // Register in block_lookup for O(1) eviction
                self.block_lookup.insert(
                    (model.to_string(), worker_id.clone(), effective_hash),
                    child.clone(),
                );
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
                combined_score: self.cache_weight * hit_ratio,
            });
        }
        results.sort_by(|a, b| {
            b.combined_score
                .partial_cmp(&a.combined_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results
    }

    fn remove_worker(&self, worker_id: &str) {
        for entry in &self.tries {
            let root = entry.value().clone();
            // Iterative BFS: clone Arc → write → remove worker → collect children
            let mut stack: Vec<SharedBlock> = vec![root];
            while let Some(node) = stack.pop() {
                let children: Vec<SharedBlock> = {
                    let mut guard = node.write();
                    guard.workers.remove(worker_id);
                    if guard.workers.is_empty() {
                        // Lazy GC: clear children so Arc drops them
                        let cs: Vec<_> = guard.children.values().cloned().collect();
                        guard.children.clear();
                        cs
                    } else {
                        guard.children.values().cloned().collect()
                    }
                };
                stack.extend(children);
            }
        }
        // Clean LRU + prune_timers + block_lookup for this worker
        self.prune_timers.lock().remove_worker(worker_id);
        self.block_lookup.retain(|k, _| k.1 != worker_id);
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

    fn block_count(&self) -> usize {
        self.block_lookup.iter().count()
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
        let mut blocks: Vec<boom_core::kv_event::BatchBlock> = Vec::with_capacity(n_full);
        let mut parent_hash: Option<u64> = None;
        for i in 0..n_full {
            let chunk = &prefix_bytes[i * self.block_size..(i + 1) * self.block_size];
            let h = hash_block_bytes(chunk);
            blocks.push(boom_core::kv_event::BatchBlock {
                local_hash: 0,
                parent_hash,
                block_bytes: chunk.to_vec(),
                block_size: self.block_size as u32,
                storage_tier,
            });
            parent_hash = Some(h);
        }
        self.apply_store_batch(model, worker_id, blocks);
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

    fn model_names(&self) -> HashSet<String> {
        self.tries.iter().map(|e| e.key().clone()).collect()
    }

    fn debug_dump(&self) -> Vec<(String, u64, Vec<String>, StorageTier, u64)> {
        let mut result = Vec::new();
        for entry in &self.tries {
            let model = entry.key().clone();
            let root = entry.value().clone();
            // Iterative DFS with per-node read locks
            let mut stack: Vec<(SharedBlock, u64)> = vec![(root, 0)];
            while let Some((node, depth)) = stack.pop() {
                let children: Vec<(u64, SharedBlock)> = {
                    let guard = node.read();
                    for (&trie_key, _) in &guard.children {
                        // Will check child below
                    }
                    // Collect children info under read lock
                    let mut cs: Vec<(u64, SharedBlock)> = Vec::new();
                    for (&tk, child) in &guard.children {
                        cs.push((tk, child.clone()));
                    }
                    cs
                };
                for (trie_key, child) in &children {
                    let (workers_list, tier, grandchildren): (Vec<String>, StorageTier, Vec<SharedBlock>) = {
                        let cguard = child.read();
                        let workers: Vec<String> = cguard.workers.keys().cloned().collect();
                        let tier = cguard
                            .workers
                            .values()
                            .max_by_key(|t| t.priority_score().to_bits())
                            .copied()
                            .unwrap_or(StorageTier::Gpu);
                        let gc: Vec<_> = cguard.children.values().cloned().collect();
                        (workers, tier, gc)
                    };
                    if !workers_list.is_empty() {
                        result.push((
                            model.clone(),
                            *trie_key,
                            workers_list,
                            tier,
                            depth + 1,
                        ));
                    }
                    for gc in grandchildren {
                        stack.push((gc, depth + 2));
                    }
                }
            }
        }
        result
    }

    fn block_capacity(&self) -> usize {
        self.lru_queue.lock().cap().get()
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

        // Pre-compute per-block data
        let now = std::time::Instant::now();
        let prepared: Vec<(u64, u64, Vec<u8>, StorageTier)> = blocks
            .iter()
            .map(|b| {
                let trie_key = hash_block_bytes(&b.block_bytes);
                let effective_hash = if b.local_hash == 0 { trie_key } else { b.local_hash };
                (trie_key, effective_hash, b.block_bytes.clone(), b.storage_tier)
            })
            .collect();

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
                .or_insert_with(|| Arc::new(RwLock::new(Block::default())));
            entry.clone()
        }; // RefMut dropped — shard write lock released
        let mut current = root_arc;

        for &(trie_key, effective_hash, ref block_bytes, storage_tier) in &prepared {
            // Phase A: lock current, find-or-create child, clone Arc
            let child = {
                let mut guard = current.write();
                guard
                    .children
                    .entry(trie_key)
                    .or_insert_with(|| Arc::new(RwLock::new(Block::default())))
                    .clone()
            }; // current write released

            // Phase B: lock child, insert worker + tokens + block_lookup
            {
                let mut child_guard = child.write();
                child_guard.workers.insert(worker_id.to_string(), storage_tier);
                if child_guard.tokens.is_empty() {
                    child_guard.tokens = block_bytes.clone();
                }
            } // child write released

            // Phase C: register in block_lookup for O(1) eviction
            self.block_lookup.insert(
                (model.to_string(), worker_id.to_string(), effective_hash),
                child.clone(),
            );

            current = child;
        }

        // LRU push + prune_timers — AFTER trie walk so evict_single_block
        // can find nodes in the trie when processing evictions.
        let mut lru_evicted: Vec<((String, String, u64), ())> = Vec::new();
        {
            let mut timers = self.prune_timers.lock();
            let mut lru = self.lru_queue.lock();
            for &(_trie_key, effective_hash, _, _) in &prepared {
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
        let idx = TokenPrefixIndex::new(4, 0.5, 0.2, 500_000);
        idx.apply_event(&store_event("m", "w0", 0, None, vec![1, 2, 3, 4]));
        let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 1);
        assert!((matches[0].hit_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_chained_blocks_with_parent() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.2, 500_000);
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
        let idx = TokenPrefixIndex::new(4, 0.5, 0.2, 500_000);
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
        let idx = TokenPrefixIndex::new(4, 0.5, 0.2, 500_000);
        // Use record_request_prefix for chain (reliable chain building)
        idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], StorageTier::Gpu);
        // trie_key of block 2 = hash_block_bytes([5,6,7,8])
        let h2 = hash_block_bytes(&[5, 6, 7, 8]);
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
        let idx = TokenPrefixIndex::new(4, 0.5, 0.2, 500_000);
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
        let idx = TokenPrefixIndex::new(4, 0.5, 0.2, 500_000);
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
        let idx = TokenPrefixIndex::new(4, 0.5, 0.2, 500_000);
        idx.record_request_prefix("m", "w0", &[1, 2, 3, 4], StorageTier::Gpu);
        assert!(idx.block_count() > 0);
        idx.prune_expired(std::time::Duration::ZERO);
        assert_eq!(idx.block_count(), 0);
    }

    #[test]
    fn test_lru_capacity_limits_trie() {
        // max_blocks=4: inserting 8 blocks should evict the oldest 4
        let idx = TokenPrefixIndex::new(4, 0.5, 0.2, 4);
        for i in 0..8u64 {
            let chunk: Vec<u8> = vec![i as u8, i as u8, i as u8, i as u8];
            idx.apply_event(&store_event("m", "w0", 0, None, chunk));
        }
        // Only the last 4 blocks should survive (LRU capacity)
        assert!(idx.block_count() <= 4);
    }
}
