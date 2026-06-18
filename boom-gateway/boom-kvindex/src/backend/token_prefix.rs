use boom_core::kv_event::{GatewayKvEvent, KvMatchResult, LoadState, StorageTier};
use dashmap::DashMap;
use lru::LruCache;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;

use super::KvIndexBackend;

/// A node in the token-block trie.
///
/// Each transition represents a block of `block_size` token IDs, keyed by
/// the xxhash3-64 hash of those tokens (u64, 8 bytes).
/// Workers that have cached the full prefix up to this node are stored
/// in `workers`.
#[derive(Default)]
struct TrieNode {
    children: HashMap<u64, Box<TrieNode>>,
    /// Workers that have cached the full prefix up to this node,
    /// mapped to their storage tier for this specific block.
    workers: HashMap<String, StorageTier>,
}

/// Token-prefix index using a per-model trie.
///
/// Matches incoming requests against cached KV prefixes by hashing each
/// request block with xxhash3-64 and walking the trie. This saves ~98% key
/// storage compared to raw Vec<u32> keys (8 bytes vs 512 bytes per block at
/// block_size=128).
///
/// Block parent-child relationships are tracked via `block_parent` so that
/// blocks are inserted at the correct trie depth (respecting `parent_block_hash`
/// from vLLM events), supporting multiple concurrent request chains per worker.
pub struct TokenPrefixIndex {
    /// Per-model trie root (model → trie root).
    tries: DashMap<String, RwLock<TrieNode>>,
    /// Per-block parent tracking: (model, worker, block_hash) → parent_hash.
    /// `None` means root block (first block of a new sequence).
    block_parent: DashMap<(String, String, u64), Option<u64>>,
    /// Reverse edge of `block_parent`: (model, worker, parent_hash) → child
    /// hashes. The `None` bucket holds root blocks. Kept in lock-step with
    /// `block_parent` on every Store/Evict/Remove so `find_children` and
    /// `collect_descendants` are O(1)/O(|subtree|) instead of O(n) full-table
    /// scans on the hot Store/Evict paths. Empty buckets are dropped
    /// immediately to bound memory under high block churn.
    reverse_children: DashMap<(String, String, Option<u64>), HashSet<u64>>,
    /// Maps vLLM block_hash → trie key (xxhash3-64 of token_ids).
    /// Used by build_path to walk the trie during eviction / repositioning.
    block_trie_key: DashMap<(String, String, u64), u64>,
    /// Per (model, worker, hash) → block token_ids (for debug_dump and logging).
    hash_to_tokens: DashMap<(String, String, u64), Vec<u32>>,
    /// Load metrics per worker.
    loads: DashMap<String, LoadState>,
    /// LRU tracking for block capacity limit. Key = (model, worker_id, block_hash).
    /// When capacity is reached, the least recently stored block is evicted.
    lru_queue: Mutex<LruCache<(String, String, u64), ()>>,
    /// Tokens per block (must match vLLM's block_size, typically 128).
    block_size: usize,
    cache_weight: f64,
    tier_weight: f64,
    load_weight: f64,
    /// Records the temporary trie path where a block was placed when
    /// chain_complete=false. Used by reinsert_block_tree for precise removal
    /// instead of the global remove_worker_from_edges which can corrupt other chains.
    /// Key: (model, worker_id, block_hash) → Value: full trie path [key0, …, trie_key]
    temp_trie_path: DashMap<(String, String, u64), Vec<u64>>,
}

/// Compute a xxhash3-64 hash of a token block.
/// This is the trie edge key — same tokens always produce the same u64.
#[inline]
fn hash_block_tokens(tokens: &[u32]) -> u64 {
    // On little-endian (all x86/ARM), reinterpret u32 slice as bytes — zero-copy.
    #[cfg(target_endian = "little")]
    {
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(tokens.as_ptr() as *const u8, tokens.len() * 4)
        };
        twox_hash::xxhash3_64::Hasher::oneshot(bytes)
    }
    #[cfg(not(target_endian = "little"))]
    {
        let mut bytes = Vec::with_capacity(tokens.len() * 4);
        for &t in tokens {
            bytes.extend_from_slice(&t.to_le_bytes());
        }
        twox_hash::xxhash3_64::Hasher::oneshot(&bytes)
    }
}

impl TokenPrefixIndex {
    pub fn new(
        block_size: usize,
        cache_weight: f64,
        tier_weight: f64,
        load_weight: f64,
        max_blocks: usize,
    ) -> Self {
        let cap = NonZeroUsize::new(max_blocks).unwrap_or(NonZeroUsize::new(500_000).unwrap());
        Self {
            tries: DashMap::new(),
            block_parent: DashMap::new(),
            reverse_children: DashMap::new(),
            block_trie_key: DashMap::new(),
            hash_to_tokens: DashMap::new(),
            loads: DashMap::new(),
            lru_queue: Mutex::new(LruCache::new(cap)),
            block_size,
            cache_weight,
            tier_weight,
            load_weight,
            temp_trie_path: DashMap::new(),
        }
    }

    fn load_score_for(&self, worker_id: &str) -> f64 {
        if let Some(load) = self.loads.get(worker_id) {
            let kv_avail = 1.0 - load.kv_utilization;
            let queue_factor = if load.queue_depth == 0 {
                1.0
            } else {
                1.0 / (1.0 + load.queue_depth as f64)
            };
            kv_avail * queue_factor
        } else {
            0.5
        }
    }
}

impl TokenPrefixIndex {
    /// Walk the parent chain from `hash` back to root, returning the
    /// root→hash trie key path (u64 hashes).
    ///
    /// `hash = None` returns an empty path (used when inserting root blocks).
    /// `hash = Some(h)` returns `[root_trie_key, ..., h_trie_key]`.
    ///
    /// Returns `(path, chain_complete)` where `chain_complete` indicates whether
    /// the entire chain from root to `hash` was successfully reconstructed.
    fn build_path(&self, model: &str, worker_id: &str, hash: Option<u64>) -> (Vec<u64>, bool) {
        let mut path = Vec::new();
        let mut current = hash;
        let mut visited = HashSet::new();
        let mut reached_root = hash.is_none();
        while let Some(h) = current {
            if !visited.insert(h) {
                tracing::warn!(hash = h, "cycle detected in parent chain, breaking");
                break;
            }
            let key = (model.to_string(), worker_id.to_string(), h);
            if let Some(trie_key) = self.block_trie_key.get(&key) {
                let parent = self
                    .block_parent
                    .get(&key)
                    .map(|p| *p)
                    .unwrap_or(None);
                path.push(*trie_key.value());
                if parent.is_none() {
                    reached_root = true;
                    current = None;
                } else {
                    current = parent;
                }
            } else {
                // Parent block not yet received — chain is incomplete.
                break;
            }
        }
        path.reverse();
        (path, reached_root)
    }

    /// Find all blocks in `block_parent` that have `parent_hash` pointing to
    /// `target_hash` for this (model, worker).
    fn find_children(
        &self,
        model: &str,
        worker_id: &str,
        target_hash: u64,
    ) -> Vec<u64> {
        // O(1) reverse-index lookup. `block_parent` is a forest (one parent per
        // child), so a precomputed parent→children map replaces the prior
        // O(n) full-table scan that ran on every Store.
        self.reverse_children
            .get(&(model.to_string(), worker_id.to_string(), Some(target_hash)))
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Remove `child` from the `(model, worker, parent)` reverse-children
    /// bucket, dropping the bucket when it becomes empty. This is the only
    /// detach entry point for the reverse index; Store uses `entry().or_default()`
    /// for the attach side and calls this only when a block is re-parented.
    /// The RefMut is released before the conditional `remove` to avoid a shard
    /// deadlock.
    fn reverse_detach(
        &self,
        model: &str,
        worker_id: &str,
        parent: Option<u64>,
        child: u64,
    ) {
        let bucket_key = (model.to_string(), worker_id.to_string(), parent);
        let now_empty = if let Some(mut set) = self.reverse_children.get_mut(&bucket_key) {
            set.remove(&child);
            set.is_empty()
        } else {
            return;
        };
        if now_empty {
            let _ = self.reverse_children.remove(&bucket_key);
        }
    }

    /// Compute the full root→block trie path for every block in `to_evict`
    /// (eviction roots + their descendants, any order, duplicates tolerated).
    ///
    /// Incremental construction: a block whose parent is also in the set gets
    /// `parent_path + own_trie_key` (O(1)); a block whose parent is outside the
    /// set (an eviction root) is resolved once via `build_path`. This replaces
    /// the prior O(|to_evict| × depth) scheme that called `build_path` per
    /// block and re-walked shared prefixes each time — the dominant cost on the
    /// frequent Evict path under high throughput.
    fn build_evict_paths(
        &self,
        model: &str,
        worker_id: &str,
        to_evict: &[u64],
    ) -> Vec<Vec<u64>> {
        // Snapshot parent + trie_key for every block (one read each) and the
        // set membership used to distinguish boundary vs. incremental nodes.
        let mut parent_of: HashMap<u64, Option<u64>> = HashMap::new();
        let mut trie_key_of: HashMap<u64, Option<u64>> = HashMap::new();
        let mut in_set: HashSet<u64> = HashSet::new();
        for &h in to_evict {
            in_set.insert(h);
            let key = (model.to_string(), worker_id.to_string(), h);
            let p = self.block_parent.get(&key).map(|v| *v).unwrap_or(None);
            let tk = self.block_trie_key.get(&key).map(|v| *v);
            parent_of.entry(h).or_insert(p);
            trie_key_of.entry(h).or_insert(tk);
        }

        let mut paths: HashMap<u64, Vec<u64>> = HashMap::new();
        for &h in to_evict {
            if paths.contains_key(&h) {
                continue;
            }
            // Walk up from h collecting the chain until we leave the set or hit
            // an already-resolved node. `chain_set` guards against cycles
            // (defensive; build_path guards too) in O(1) per step.
            let mut chain: Vec<u64> = vec![h];
            let mut chain_set: HashSet<u64> = HashSet::new();
            chain_set.insert(h);
            let mut cur = parent_of[&h];
            loop {
                match cur {
                    Some(p) if paths.contains_key(&p) => break, // memoized base above
                    Some(p) if in_set.contains(&p) && chain_set.insert(p) => {
                        chain.push(p);
                        cur = parent_of[&p];
                    }
                    _ => break, // parent outside set (boundary) or cycle
                }
            }
            // Resolve parent-first so each node's parent path is already known.
            chain.reverse();
            for &node in &chain {
                if paths.contains_key(&node) {
                    continue;
                }
                let path = match (parent_of[&node], trie_key_of[&node]) {
                    (Some(p), Some(tk)) if paths.contains_key(&p) => {
                        let mut v = paths[&p].clone();
                        v.push(tk);
                        v
                    }
                    _ => self.build_path(model, worker_id, Some(node)).0,
                };
                paths.insert(node, path);
            }
        }

        // Emit deduplicated, non-empty paths in to_evict order. Duplicates in
        // `to_evict` (e.g. one eviction root being an ancestor of another) are
        // collapsed; empty paths mean the block was never inserted into the trie.
        let mut emitted: HashSet<u64> = HashSet::new();
        let mut out: Vec<Vec<u64>> = Vec::with_capacity(to_evict.len());
        for &h in to_evict {
            if !emitted.insert(h) {
                continue;
            }
            if let Some(p) = paths.get(&h) {
                if !p.is_empty() {
                    out.push(p.clone());
                }
            }
        }
        out
    }

    /// Re-insert a block and its descendants into the trie at the correct
    /// position. Used when a delayed parent block arrives and previously
    /// orphaned children need to be repositioned.
    ///
    /// Uses `temp_trie_path` to remove the worker from the old (temporary)
    /// position only, avoiding the global `remove_worker_from_edges` which
    /// would corrupt other request chains sharing the same trie_key.
    fn reinsert_block_tree(
        &self,
        model: &str,
        worker_id: &str,
        block_hash: u64,
    ) {
        let (correct_path, complete) = self.build_path(model, worker_id, Some(block_hash));
        if !complete || correct_path.is_empty() {
            return;
        }

        let key = (model.to_string(), worker_id.to_string(), block_hash);
        let trie_key = match self.block_trie_key.get(&key) {
            Some(k) => *k.value(),
            None => return,
        };

        let trie = match self.tries.get(model) {
            Some(t) => t,
            None => return,
        };
        {
            let mut root = trie.write();

            // Step 1: Capture the worker's current tier for this trie_key
            // before any removal.
            let tier = Self::find_worker_tier_for_key(&root, worker_id, trie_key)
                .unwrap_or(StorageTier::Gpu);

            // Step 2: Precisely remove worker from the old temporary position only.
            // temp_trie_path was recorded when chain_complete=false during Store.
            let lookup_key = (model.to_string(), worker_id.to_string(), block_hash);
            if let Some((_, old_path)) = self.temp_trie_path.remove(&lookup_key) {
                Self::remove_worker_at_leaf(&mut root, worker_id, &old_path);
            }

            // Step 3: Insert at the correct position with preserved tier.
            let mut node: &mut TrieNode = &mut root;
            for &tk in &correct_path[..correct_path.len() - 1] {
                node = node
                    .children
                    .entry(tk)
                    .or_insert_with(|| Box::default())
                    .as_mut();
            }
            node.children
                .entry(trie_key)
                .or_insert_with(|| Box::default())
                .workers
                .insert(worker_id.to_string(), tier);
        }

        // Recursively reinsert children.
        let children = self.find_children(model, worker_id, block_hash);
        for child_hash in children {
            self.reinsert_block_tree(model, worker_id, child_hash);
        }
    }

    /// DFS find the tier of `worker_id` in the first trie node reachable via
    /// `target_key` edge. Returns None if the worker is not found.
    fn find_worker_tier_for_key(
        node: &TrieNode,
        worker_id: &str,
        target_key: u64,
    ) -> Option<StorageTier> {
        if let Some(child) = node.children.get(&target_key) {
            if let Some(tier) = child.workers.get(worker_id) {
                return Some(*tier);
            }
        }
        for child in node.children.values() {
            if let Some(tier) = Self::find_worker_tier_for_key(child, worker_id, target_key) {
                return Some(tier);
            }
        }
        None
    }

    /// Check if `worker_id`'s stored tier at the trie node reachable via
    /// `trie_key` matches `required_tier`. Returns false if the worker is
    /// found but the tier differs (swap protection).
    fn check_worker_tier_at_trie_key(
        node: &TrieNode,
        worker_id: &str,
        trie_key: u64,
        required_tier: StorageTier,
    ) -> bool {
        if let Some(child) = node.children.get(&trie_key) {
            if let Some(stored_tier) = child.workers.get(worker_id) {
                return *stored_tier == required_tier;
            }
        }
        // Worker not found at this key — check children (trie_key might
        // appear at a deeper level due to path structure).
        for child in node.children.values() {
            if Self::check_worker_tier_at_trie_key(child, worker_id, trie_key, required_tier) {
                return true;
            }
        }
        false
    }

    /// Check whether `worker_id`'s stored tier for the block identified by
    /// (`hash`, `trie_key`) equals `required_tier` — used by EvictBlocks swap
    /// protection.
    ///
    /// Walks the actual root→leaf trie path (O(depth)) reconstructed via the
    /// `block_parent` + `block_trie_key` reverse indexes, instead of
    /// DFS-scanning the whole subtree from the root (which was O(num_nodes) in
    /// the worst case — see `check_worker_tier_at_trie_key`). Falls back to the
    /// subtree scan whenever the indexes are inconsistent with the trie (e.g. a
    /// delayed event hasn't populated them yet), so behavior is unchanged.
    fn check_worker_tier(
        &self,
        model: &str,
        worker_id: &str,
        hash: u64,
        trie_key: u64,
        required_tier: StorageTier,
    ) -> bool {
        let root_lock = match self.tries.get(model) {
            Some(r) => r,
            None => return true, // no trie for this model → allow eviction
        };
        let root = root_lock.read();

        // Reconstruct the leaf→root path of trie_keys. block_parent maps
        // (model, worker, effective_hash) → parent effective_hash; block_trie_key
        // maps the same key → trie_key. Values are Copy, so each DashMap guard
        // is dropped immediately to avoid holding it across the next lookup.
        let mut trie_keys: Vec<u64> = vec![trie_key];
        let mut cur_hash = hash;
        let mut inconsistent = false;
        loop {
            let parent = self
                .block_parent
                .get(&(model.to_string(), worker_id.to_string(), cur_hash))
                .map(|v| *v);
            match parent {
                None => {
                    inconsistent = true;
                    break;
                }
                Some(None) => break,    // root block — path complete
                Some(Some(p)) if p == cur_hash => {
                    inconsistent = true; // defensive: refuse to chase a cycle
                    break;
                }
                Some(Some(p)) => {
                    let parent_trie_key = self
                        .block_trie_key
                        .get(&(model.to_string(), worker_id.to_string(), p))
                        .map(|v| *v);
                    match parent_trie_key {
                        None => {
                            inconsistent = true;
                            break;
                        }
                        Some(t) => {
                            trie_keys.push(t);
                            cur_hash = p;
                        }
                    }
                }
            }
        }
        if inconsistent {
            return Self::check_worker_tier_at_trie_key(
                &root, worker_id, trie_key, required_tier,
            );
        }
        trie_keys.reverse(); // root → leaf

        // Walk the trie along the reconstructed path.
        let mut node: &TrieNode = &root;
        for k in &trie_keys {
            match node.children.get(k) {
                Some(c) => node = c,
                None => {
                    // Path not present in the trie → fall back to subtree scan.
                    return Self::check_worker_tier_at_trie_key(
                        &root, worker_id, trie_key, required_tier,
                    );
                }
            }
        }
        match node.workers.get(worker_id) {
            Some(t) => *t == required_tier,
            None => Self::check_worker_tier_at_trie_key(
                &root, worker_id, trie_key, required_tier,
            ),
        }
    }

    /// Walk the trie path and remove the worker from the LEAF node only.
    /// Unlike remove_worker_from_edges which scans the entire trie, this only
    /// touches the single node at the end of the given path — safe for shared
    /// trie_keys that appear under different parent chains.
    fn remove_worker_at_leaf(node: &mut TrieNode, worker_id: &str, path: &[u64]) -> bool {
        if path.is_empty() {
            return false;
        }
        if path.len() == 1 {
            if let Some(child) = node.children.get_mut(&path[0]) {
                return child.workers.remove(worker_id).is_some();
            }
            return false;
        }
        if let Some(child) = node.children.get_mut(&path[0]) {
            Self::remove_worker_at_leaf(child, worker_id, &path[1..])
        } else {
            false
        }
    }

    /// Collect all transitive descendants of `target_hash` within a worker's
    /// block tree.
    fn collect_descendants(
        &self,
        model: &str,
        worker_id: &str,
        target_hash: u64,
        result: &mut Vec<u64>,
    ) {
        // Iterative BFS over the reverse index — O(|subtree|) instead of the
        // prior O(n) recursive full-table scan. No dedup is needed: the reverse
        // index guarantees each child lives in exactly one parent bucket
        // (forest invariant), so the subtree contains no duplicates.
        let mut stack: Vec<Option<u64>> = vec![Some(target_hash)];
        while let Some(parent) = stack.pop() {
            if let Some(set) = self
                .reverse_children
                .get(&(model.to_string(), worker_id.to_string(), parent))
            {
                for &child in set.iter() {
                    debug_assert!(
                        !result.contains(&child),
                        "duplicate child in subtree — reverse index invariant violated"
                    );
                    result.push(child);
                    stack.push(Some(child));
                }
            }
        }
    }

    /// Walk the trie along `path` (u64 keys), remove `worker_id` from the **last** node.
    fn remove_worker_at_path(node: &mut TrieNode, worker_id: &str, path: &[u64]) {
        if path.is_empty() {
            node.workers.remove(worker_id);
            return;
        }
        let mut current = node;
        for &key in path {
            match current.children.get_mut(&key) {
                Some(child) => {
                    current = child;
                }
                None => return,
            }
        }
        current.workers.remove(worker_id);
    }

    /// Recursively remove `worker_id` from all trie nodes (for AllBlocksCleared).
    fn remove_worker_recursive(node: &mut TrieNode, worker_id: &str) {
        node.workers.remove(worker_id);
        for child in node.children.values_mut() {
            Self::remove_worker_recursive(child, worker_id);
        }
    }

    /// Evict a single block and all its descendants from trie + all DashMaps + LRU queue.
    /// Used by LRU capacity eviction.
    fn lru_evict_block(&self, model: &str, worker_id: &str, hash: u64) {
        if hash == 0 {
            return;
        }

        // 1. Collect descendants (cascading eviction).
        let mut to_evict = Vec::new();
        self.collect_descendants(model, worker_id, hash, &mut to_evict);
        to_evict.push(hash);

        // 2. Build trie paths BEFORE removing metadata. Incremental: shared
        //    parent prefixes are walked via build_path at most once per root
        //    instead of once per block.
        let paths: Vec<Vec<u64>> = self.build_evict_paths(model, worker_id, &to_evict);

        // 3. Remove worker from trie nodes.
        if let Some(trie) = self.tries.get(model) {
            let mut root = trie.write();
            for path in &paths {
                Self::remove_worker_at_path(&mut root, worker_id, path);
            }
        }

        // 4. Clean up block metadata in DashMaps (+ reverse index).
        for &h in &to_evict {
            let key = (model.to_string(), worker_id.to_string(), h);
            if let Some((_, old_parent)) = self.block_parent.remove(&key) {
                self.reverse_detach(model, worker_id, old_parent, h);
            }
            self.block_trie_key.remove(&key);
            self.hash_to_tokens.remove(&key);
            self.temp_trie_path.remove(&key);
        }

        // 5. Remove all evicted entries (including descendants) from LRU queue.
        {
            let mut lru = self.lru_queue.lock();
            for &h in &to_evict {
                lru.pop(&(model.to_string(), worker_id.to_string(), h));
            }
        }

        tracing::debug!(
            model,
            worker_id,
            hash,
            evicted_count = to_evict.len(),
            "LRU eviction: block(s) evicted to maintain capacity"
        );
    }

    fn walk_trie(
        root: &TrieNode,
        model: &str,
        result: &mut Vec<(String, u64, Vec<String>, StorageTier, u64)>,
    ) {
        // Stack entries: (node reference, depth)
        let mut stack: Vec<(&TrieNode, u64)> = vec![(root, 0)];
        while let Some((node, depth)) = stack.pop() {
            for (&trie_key, child) in &node.children {
                if !child.workers.is_empty() {
                    let workers: Vec<String> = child.workers.keys().cloned().collect();
                    // Use the best (highest priority) tier among workers as representative.
                    let tier = child
                        .workers
                        .values()
                        .max_by_key(|t| t.priority_score().to_bits())
                        .copied()
                        .unwrap_or(StorageTier::Gpu);
                    result.push((model.to_string(), trie_key, workers, tier, depth + 1));
                }
                stack.push((child, depth + 1));
            }
        }
    }
}

impl KvIndexBackend for TokenPrefixIndex {
    fn apply_event(&self, event: &GatewayKvEvent) {
        match event {
            GatewayKvEvent::Store {
                model,
                worker_id,
                local_hash,
                token_ids,
                parent_hash,
                storage_tier,
                ..
            } => {
                if token_ids.is_empty() {
                    return;
                }

                // Handle the case where local_hash is 0 (vLLM sent empty block_hashes).
                let effective_hash = if *local_hash == 0 {
                    // Use xxhash3 of token_ids as synthetic hash.
                    let synthetic = hash_block_tokens(token_ids);
                    tracing::debug!(
                        model,
                        worker_id,
                        synthetic_hash = synthetic,
                        token_count = token_ids.len(),
                        "Store with local_hash=0, using synthetic hash"
                    );
                    synthetic
                } else {
                    *local_hash
                };

                // Compute the trie key (xxhash3-64 of token_ids).
                let trie_key = hash_block_tokens(token_ids);

                // 1. Record parent relationship, keeping the reverse index in
                //    sync. `block_parent.insert` returns the previous parent so
                //    we can detach the stale reverse edge on re-parenting (e.g.
                //    when a delayed parent_block_hash arrives later).
                let parent_key = (model.clone(), worker_id.clone(), effective_hash);
                let prev_parent = self.block_parent.insert(parent_key, *parent_hash);
                if let Some(old) = prev_parent {
                    if old != *parent_hash {
                        self.reverse_detach(model, worker_id, old, effective_hash);
                    }
                }
                self.reverse_children
                    .entry((model.clone(), worker_id.clone(), *parent_hash))
                    .or_default()
                    .insert(effective_hash);

                // 2. Record hash → trie key mapping.
                self.block_trie_key.insert(
                    (model.clone(), worker_id.clone(), effective_hash),
                    trie_key,
                );

                // 3. Record hash → token_ids mapping (for debug/logging).
                //    If already exists, verify consistency (detect tokenization drift).
                let token_key = (model.clone(), worker_id.clone(), effective_hash);
                if let Some(existing) = self.hash_to_tokens.get(&token_key) {
                    if existing.value() != token_ids {
                        let diff_pos = existing.iter().zip(token_ids.iter())
                            .enumerate()
                            .find(|(_, (a, b))| a != b)
                            .map(|(i, _)| i);
                        tracing::warn!(
                            model,
                            worker_id,
                            hash = effective_hash,
                            existing_len = existing.len(),
                            new_len = token_ids.len(),
                            first_diff_at = diff_pos,
                            existing_first_4 = ?&existing[..existing.len().min(4)],
                            new_first_4 = ?&token_ids[..token_ids.len().min(4)],
                            "[STORE] token_ids MISMATCH for same hash — tokenization drift detected!"
                        );
                    }
                }
                self.hash_to_tokens.insert(
                    token_key,
                    token_ids.clone(),
                );

                // 3.5 LRU tracking — evict oldest block if at capacity.
                let lru_evicted = {
                    let mut lru = self.lru_queue.lock();
                    let key = (model.clone(), worker_id.clone(), effective_hash);
                    if lru.get(&key).is_some() {
                        // Already tracked — get() already moved it to most recent.
                        None
                    } else {
                        // New block — push, may evict oldest if at capacity.
                        lru.push(key, ())
                    }
                };
                if let Some(((m, w, h), _)) = lru_evicted {
                    self.lru_evict_block(&m, &w, h);
                }

                // 4. Build path from root to parent (u64 trie keys).
                let (path, chain_complete) = self.build_path(model, worker_id, *parent_hash);

                // 5. Walk trie along path, then insert new block keyed by trie_key.
                {
                    let trie = self
                        .tries
                        .entry(model.clone())
                        .or_insert_with(|| RwLock::new(TrieNode::default()));
                    {
                        let mut root = trie.write();

                        let mut node: &mut TrieNode = &mut root;
                        for &tk in &path {
                            node = node
                                .children
                                .entry(tk)
                                .or_insert_with(|| Box::default())
                                .as_mut();
                        }

                        node.children
                            .entry(trie_key)
                            .or_insert_with(|| Box::default())
                            .workers
                            .insert(worker_id.clone(), *storage_tier);
                    }

                    // Always log trie insertion at debug level for depth diagnosis.
                    let first_4 = token_ids.iter().take(4).copied().collect::<Vec<_>>();
                    let last_4 = token_ids.iter().rev().take(4).copied().collect::<Vec<_>>();
                    tracing::debug!(
                        model,
                        worker_id,
                        depth = path.len(),
                        hash = effective_hash,
                        trie_key,
                        parent_hash = ?parent_hash,
                        token_count = token_ids.len(),
                        first_4_tokens = ?first_4,
                        last_4_tokens = ?last_4,
                        chain_complete,
                        "[STORE] block inserted into trie"
                    );
                }

                if !chain_complete {
                    tracing::debug!(
                        model,
                        worker_id,
                        hash = effective_hash,
                        parent_hash = ?parent_hash,
                        "block stored with incomplete parent chain, will be repositioned when parent arrives"
                    );
                    // Record the temporary trie path so reinsert_block_tree can
                    // remove the worker from this exact position later, instead of
                    // doing a global remove_worker_from_edges that may corrupt
                    // other chains sharing the same trie_key.
                    let mut temp_path = path.clone();
                    temp_path.push(trie_key);
                    self.temp_trie_path.insert(
                        (model.clone(), worker_id.clone(), effective_hash),
                        temp_path,
                    );
                }

                // 6. Reposition orphaned children.
                let orphaned_children = self.find_children(model, worker_id, effective_hash);
                if !orphaned_children.is_empty() {
                    tracing::debug!(
                        model,
                        worker_id,
                        hash = effective_hash,
                        orphaned_count = orphaned_children.len(),
                        "repositioning orphaned children after parent arrival"
                    );
                    for child_hash in orphaned_children {
                        self.reinsert_block_tree(model, worker_id, child_hash);
                    }
                }

                // 7. Tier is already stored in the trie node (per-block).
            }

            GatewayKvEvent::Remove { worker_id, .. } => {
                self.remove_worker(worker_id);
            }

            GatewayKvEvent::EvictBlocks {
                model,
                worker_id,
                block_hashes,
                storage_tier,
            } => {
                if block_hashes.is_empty() {
                    tracing::warn!(
                        model,
                        worker_id,
                        "EvictBlocks with empty block_hashes — vLLM should not emit this, skipping"
                    );
                    return;
                }

                let mut to_evict: Vec<u64> = Vec::new();

                for &hash in block_hashes {
                    if hash == 0 {
                        tracing::debug!(
                            model,
                            worker_id,
                            hash,
                            "EvictBlocks skipping hash=0"
                        );
                        continue;
                    }

                    // When a specific tier is required (swap scenario), check if this
                    // block's current tier matches. If the block has already been
                    // swapped to a different tier, skip it entirely — both trie and
                    // metadata must be preserved.
                    if let Some(required_tier) = storage_tier {
                        let key = (model.to_string(), worker_id.to_string(), hash);
                        let tier_matches = self
                            .block_trie_key
                            .get(&key)
                            .map(|trie_key| {
                                self.check_worker_tier(
                                    model,
                                    worker_id,
                                    hash,
                                    *trie_key.value(),
                                    *required_tier,
                                )
                            })
                            .unwrap_or(true); // If metadata missing, allow eviction.

                        if !tier_matches {
                            tracing::debug!(
                                worker_id,
                                hash,
                                ?required_tier,
                                "EvictBlocks tier mismatch, skipping block (swap protection)"
                            );
                            continue;
                        }
                    }

                    self.collect_descendants(model, worker_id, hash, &mut to_evict);
                    to_evict.push(hash);
                }

                if to_evict.is_empty() {
                    return;
                }

                // Build all paths BEFORE removing metadata (incremental).
                let paths: Vec<Vec<u64>> = self.build_evict_paths(model, worker_id, &to_evict);

                // Remove worker from trie nodes (unconditional — tier check done above).
                if let Some(trie) = self.tries.get(model) {
                    let mut root = trie.write();
                    for path in &paths {
                        Self::remove_worker_at_path(&mut root, worker_id, path);
                    }
                }

                // Clean up block metadata + reverse index + LRU queue.
                {
                    let mut lru = self.lru_queue.lock();
                    for &hash in &to_evict {
                        let key = (model.clone(), worker_id.clone(), hash);
                        if let Some((_, old_parent)) = self.block_parent.remove(&key) {
                            self.reverse_detach(model, worker_id, old_parent, hash);
                        }
                        self.block_trie_key.remove(&key);
                        self.hash_to_tokens.remove(&key);
                        self.temp_trie_path.remove(&key);
                        lru.pop(&key);
                    }
                }
            }

            GatewayKvEvent::LoadMetrics {
                worker_id,
                queue_depth,
                kv_utilization,
                running_requests,
                ..
            } => {
                let mut load = self.loads.entry(worker_id.clone()).or_default();
                load.queue_depth = *queue_depth;
                load.kv_utilization = *kv_utilization;
                load.running_requests = *running_requests;
            }
        }
    }

    fn find_matches(
        &self,
        model: &str,
        token_ids: &[u32],
        candidate_worker_ids: &[String],
    ) -> Vec<KvMatchResult> {
        tracing::debug!(
            model,
            token_count = token_ids.len(),
            candidates = candidate_worker_ids.len(),
            "find_matches called"
        );

        if token_ids.is_empty() || candidate_worker_ids.is_empty() {
            return Vec::new();
        }

        // Hash each request block into a u64 trie key.
        let request_hashes: Vec<u64> = token_ids
            .chunks(self.block_size)
            .map(|chunk| hash_block_tokens(chunk))
            .collect();
        if request_hashes.is_empty() {
            return Vec::new();
        }

        let candidate_set: HashSet<&str> =
            candidate_worker_ids.iter().map(|s| s.as_str()).collect();

        let mut worker_depth: HashMap<String, u64> = HashMap::new();
        // Per-worker worst (lowest priority) tier across all matched blocks.
        let mut worker_tiers: HashMap<String, StorageTier> = HashMap::new();

        let trie_roots = self.tries.get(model);
        if let Some(root_lock) = trie_roots {
            let root = root_lock.read();

            tracing::debug!(
                model,
                gateway_block_size = self.block_size,
                req_block0_trie_key = request_hashes[0],
                req_num_blocks = request_hashes.len(),
                root_num_children = root.children.len(),
                "[FIND_MATCHES] request hash[0] vs trie root children"
            );

            // BFS: at each depth, track which trie nodes we're at and which workers matched.
            let mut current_nodes: Vec<&TrieNode> = vec![&root];
            let mut matched_workers: HashSet<&str> = candidate_set;

            for (depth, &hash_key) in request_hashes.iter().enumerate() {
                let mut next_nodes = Vec::new();
                let mut next_workers = HashSet::new();

                let start = depth * self.block_size;
                let end = std::cmp::min(start + self.block_size, token_ids.len());

                for node in &current_nodes {
                    if let Some(child) = node.children.get(&hash_key) {
                        next_nodes.push(child.as_ref());
                        for (w, block_tier) in &child.workers {
                            if matched_workers.contains(w.as_str()) {
                                next_workers.insert(w.as_str());
                                worker_depth.insert(w.to_string(), (depth + 1) as u64);
                                // Track worst tier: if any matched block is on a slower
                                // tier, that determines the effective access cost.
                                let entry = worker_tiers.entry(w.to_string()).or_insert(*block_tier);
                                if block_tier.priority_score() < entry.priority_score() {
                                    *entry = *block_tier;
                                }
                            }
                        }
                    }
                }

                if next_nodes.is_empty() || next_workers.is_empty() {
                    // Normal: request prefix extends past the cached depth.
                    tracing::debug!(
                        depth,
                        token_range = ?(start, end),
                        req_hash = hash_key,
                        "[FIND_MATCHES] mismatch at depth — request hash not in trie children"
                    );
                    break;
                } else if depth < 3 || depth >= request_hashes.len().saturating_sub(2) {
                    // Log first few and last few matched depths for context.
                    tracing::debug!(
                        depth,
                        token_range = ?(start, end),
                        req_hash = hash_key,
                        matched_workers = next_workers.len(),
                        "[FIND_MATCHES] matched"
                    );
                }
                current_nodes = next_nodes;
                matched_workers = next_workers;
            }
        } else {
            tracing::debug!(
                model,
                "[FIND_MATCHES] no trie root found for model"
            );
        }

        let total_blocks = request_hashes.len() as f64;
        let mut results: Vec<KvMatchResult> = Vec::new();

        for (wid, depth) in &worker_depth {
            if *depth == 0 {
                continue;
            }
            let hit_ratio = *depth as f64 / total_blocks;
            let tier = worker_tiers
                .get(wid)
                .copied()
                .unwrap_or(StorageTier::Gpu);
            let tier_score = tier.priority_score();
            let load_score = self.load_score_for(wid);

            let combined_score = self.cache_weight * hit_ratio
                + self.tier_weight * tier_score
                + self.load_weight * load_score;

            results.push(KvMatchResult {
                worker_id: wid.clone(),
                match_depth: *depth,
                hit_ratio,
                best_tier: tier,
                tier_score,
                load_score,
                combined_score,
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
        let models: HashSet<String> = self
            .block_parent
            .iter()
            .filter(|e| e.key().1 == worker_id)
            .map(|e| e.key().0.clone())
            .collect();

        for model in &models {
            if let Some(trie) = self.tries.get(model) {
                let mut root = trie.write();
                Self::remove_worker_recursive(&mut root, worker_id);
            }
        }

        self.block_parent.retain(|k, _| k.1 != worker_id);
        self.reverse_children.retain(|k, _| k.1 != worker_id);
        self.block_trie_key.retain(|k, _| k.1 != worker_id);
        self.hash_to_tokens.retain(|k, _| k.1 != worker_id);
        self.temp_trie_path.retain(|k, _| k.1 != worker_id);
        self.loads.remove(worker_id);

        // Clean up LRU entries for this worker.
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
        self.hash_to_tokens.iter().count()
    }

    fn model_names(&self) -> HashSet<String> {
        self.tries.iter().map(|e| e.key().clone()).collect()
    }

    fn debug_dump(&self) -> Vec<(String, u64, Vec<String>, StorageTier, u64)> {
        let mut result = Vec::new();
        for entry in &self.tries {
            let model = entry.key().clone();
            let root = entry.value().read();
            Self::walk_trie(&root, &model, &mut result);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boom_core::kv_event::{GatewayKvEvent, StorageTier};

    /// Helper: create a Store event.
    fn store_event(
        model: &str,
        worker: &str,
        hash: u64,
        parent_hash: Option<u64>,
        tokens: Vec<u32>,
    ) -> GatewayKvEvent {
        GatewayKvEvent::Store {
            model: model.to_string(),
            worker_id: worker.to_string(),
            sequence_hash: String::new(),
            prefix_hash: String::new(),
            local_hash: hash,
            parent_hash,
            block_index: 0,
            token_ids: tokens,
            block_size: 4,
            storage_tier: StorageTier::Gpu,
        }
    }

    /// Helper: create an EvictBlocks event.
    fn evict_event(model: &str, worker: &str, hashes: Vec<u64>) -> GatewayKvEvent {
        GatewayKvEvent::EvictBlocks {
            model: model.to_string(),
            worker_id: worker.to_string(),
            block_hashes: hashes,
            storage_tier: None,
        }
    }

    /// Helper: create a Remove event.
    fn remove_event(worker: &str) -> GatewayKvEvent {
        GatewayKvEvent::Remove {
            worker_id: worker.to_string(),
            sequence_hash: String::new(),
            storage_tier: None,
        }
    }

    // ---- Test 1: Single root block ----
    #[test]
    fn test_single_root_block_match() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event(
            "m", "w0", 100, None, vec![1, 2, 3, 4],
        ));

        let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].worker_id, "w0");
        assert_eq!(matches[0].match_depth, 1);
        assert!((matches[0].hit_ratio - 1.0).abs() < f64::EPSILON);
    }

    // ---- Test 2: Two independent root blocks on same worker ----
    #[test]
    fn test_two_independent_root_blocks() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event(
            "m", "w0", 100, None, vec![1, 2, 3, 4],
        ));
        idx.apply_event(&store_event(
            "m", "w0", 200, None, vec![5, 6, 7, 8],
        ));

        let m1 = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        assert_eq!(m1.len(), 1);
        assert_eq!(m1[0].match_depth, 1);

        let m2 = idx.find_matches("m", &[5, 6, 7, 8], &["w0".to_string()]);
        assert_eq!(m2.len(), 1);
        assert_eq!(m2[0].match_depth, 1);
    }

    // ---- Test 3: Multi-turn with parent hash ----
    #[test]
    fn test_chained_blocks_with_parent() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event(
            "m", "w0", 100, None, vec![1, 2, 3, 4],
        ));
        idx.apply_event(&store_event(
            "m", "w0", 200, Some(100), vec![5, 6, 7, 8],
        ));
        idx.apply_event(&store_event(
            "m", "w0", 300, Some(200), vec![9, 10, 11, 12],
        ));

        let matches =
            idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 3);
    }

    // ---- Test 4: One-shot prefill with multiple blocks ----
    #[test]
    fn test_multi_block_root_prefill() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event(
            "m", "w0", 100, None, vec![1, 2, 3, 4],
        ));
        idx.apply_event(&store_event(
            "m", "w0", 200, Some(100), vec![5, 6, 7, 8],
        ));
        idx.apply_event(&store_event(
            "m", "w0", 300, Some(200), vec![9, 10, 11, 12],
        ));

        let matches =
            idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 3);

        let partial = idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8], &["w0".to_string()]);
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0].match_depth, 2);
    }

    // ---- Test 5: Multi-worker prefix reuse ----
    #[test]
    fn test_multi_worker_prefix_reuse() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event("m", "w0", 100, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m", "w0", 101, Some(100), vec![5, 6, 7, 8]));

        idx.apply_event(&store_event("m", "w1", 200, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m", "w1", 201, Some(200), vec![9, 10, 11, 12]));

        let matches = idx.find_matches(
            "m",
            &[1, 2, 3, 4, 9, 10, 11, 12],
            &["w0".to_string(), "w1".to_string()],
        );
        assert_eq!(matches.len(), 2);
        let w1_match = matches.iter().find(|m| m.worker_id == "w1").unwrap();
        let w0_match = matches.iter().find(|m| m.worker_id == "w0").unwrap();
        assert_eq!(w1_match.match_depth, 2);
        assert_eq!(w0_match.match_depth, 1);
    }

    // ---- Test 6: EvictBlocks removes correct block and descendants ----
    #[test]
    fn test_evict_single_block() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event("m", "w0", 100, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m", "w0", 200, Some(100), vec![5, 6, 7, 8]));
        idx.apply_event(&store_event("m", "w0", 300, Some(200), vec![9, 10, 11, 12]));

        idx.apply_event(&evict_event("m", "w0", vec![200]));

        let matches =
            idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 1);

        assert_eq!(idx.block_count(), 1);
    }

    // ---- Test 7: EvictBlocks with empty hashes ----
    #[test]
    fn test_evict_empty_hashes() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event("m", "w0", 100, None, vec![1, 2, 3, 4]));
        idx.apply_event(&evict_event("m", "w0", vec![]));

        let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(idx.block_count(), 1);
    }

    // ---- Test 8: AllBlocksCleared ----
    #[test]
    fn test_remove_worker() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event("m", "w0", 100, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m", "w0", 200, Some(100), vec![5, 6, 7, 8]));

        idx.apply_event(&store_event("m2", "w0", 300, None, vec![10, 20, 30, 40]));

        idx.apply_event(&remove_event("w0"));

        assert_eq!(idx.block_count(), 0);

        let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        assert!(matches.is_empty());

        let matches2 = idx.find_matches("m2", &[10, 20, 30, 40], &["w0".to_string()]);
        assert!(matches2.is_empty());
    }

    // ---- Test 9: Regression — two identical requests, second should hit ----
    #[test]
    fn test_regression_two_identical_requests() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        let tokens: Vec<u32> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

        idx.apply_event(&store_event("m", "w0", 100, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m", "w0", 200, Some(100), vec![5, 6, 7, 8]));
        idx.apply_event(&store_event("m", "w0", 300, Some(200), vec![9, 10, 11, 12]));

        idx.apply_event(&store_event("m", "w0", 400, None, vec![99, 98, 97, 96]));

        let matches = idx.find_matches("m", &tokens, &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 3);
        assert!((matches[0].hit_ratio - 1.0).abs() < f64::EPSILON);
    }

    // ---- Test 10: block_count accuracy ----
    #[test]
    fn test_block_count() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        assert_eq!(idx.block_count(), 0);

        idx.apply_event(&store_event("m", "w0", 100, None, vec![1, 2, 3, 4]));
        assert_eq!(idx.block_count(), 1);

        idx.apply_event(&store_event("m", "w0", 200, Some(100), vec![5, 6, 7, 8]));
        assert_eq!(idx.block_count(), 2);

        idx.apply_event(&store_event("m", "w1", 300, None, vec![1, 2, 3, 4]));
        assert_eq!(idx.block_count(), 3);

        idx.apply_event(&evict_event("m", "w0", vec![100]));
        assert_eq!(idx.block_count(), 1);

        idx.apply_event(&remove_event("w1"));
        assert_eq!(idx.block_count(), 0);
    }

    // ---- Test: branching chains on same worker ----
    #[test]
    fn test_branching_chains_same_worker() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event("m", "w0", 100, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m", "w0", 200, Some(100), vec![5, 6, 7, 8]));
        idx.apply_event(&store_event("m", "w0", 300, Some(100), vec![9, 10, 11, 12]));

        let ma = idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8], &["w0".to_string()]);
        assert_eq!(ma.len(), 1);
        assert_eq!(ma[0].match_depth, 2);

        let mb = idx.find_matches("m", &[1, 2, 3, 4, 9, 10, 11, 12], &["w0".to_string()]);
        assert_eq!(mb.len(), 1);
        assert_eq!(mb[0].match_depth, 2);

        idx.apply_event(&evict_event("m", "w0", vec![200]));
        assert_eq!(idx.block_count(), 2);

        let mb_after = idx.find_matches("m", &[1, 2, 3, 4, 9, 10, 11, 12], &["w0".to_string()]);
        assert_eq!(mb_after.len(), 1);
        assert_eq!(mb_after[0].match_depth, 2);
    }

    // ---- Test: store with local_hash=0 (empty block_hashes from vLLM) ----
    #[test]
    fn test_store_with_zero_hash() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&GatewayKvEvent::Store {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            sequence_hash: String::new(),
            prefix_hash: String::new(),
            local_hash: 0,
            parent_hash: None,
            block_index: 0,
            token_ids: vec![1, 2, 3, 4],
            block_size: 4,
            storage_tier: StorageTier::Gpu,
        });

        let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 1);
        assert_eq!(idx.block_count(), 1);

        idx.apply_event(&GatewayKvEvent::Store {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            sequence_hash: String::new(),
            prefix_hash: String::new(),
            local_hash: 0,
            parent_hash: None,
            block_index: 1,
            token_ids: vec![5, 6, 7, 8],
            block_size: 4,
            storage_tier: StorageTier::Gpu,
        });

        assert_eq!(idx.block_count(), 2);
    }

    // ---- Test: child arrives before parent (delayed parent) ----
    #[test]
    fn test_child_arrives_before_parent() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event(
            "m", "w0", 200, Some(100), vec![5, 6, 7, 8],
        ));

        let matches = idx.find_matches("m", &[5, 6, 7, 8], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 1);

        idx.apply_event(&store_event(
            "m", "w0", 100, None, vec![1, 2, 3, 4],
        ));

        let matches_full =
            idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8], &["w0".to_string()]);
        assert_eq!(matches_full.len(), 1);
        assert_eq!(matches_full[0].match_depth, 2);
    }

    // ---- Test: deep chain with multiple delayed ancestors ----
    #[test]
    fn test_deep_chain_delayed_ancestors() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        idx.apply_event(&store_event(
            "m", "w0", 300, Some(200), vec![9, 10, 11, 12],
        ));
        idx.apply_event(&store_event(
            "m", "w0", 200, Some(100), vec![5, 6, 7, 8],
        ));

        assert_eq!(idx.block_count(), 2);

        idx.apply_event(&store_event(
            "m", "w0", 100, None, vec![1, 2, 3, 4],
        ));

        let matches = idx.find_matches(
            "m",
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            &["w0".to_string()],
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 3);
    }

    // ---- Test: swap scenario — Store GPU, Store CPU, Remove GPU ----
    #[test]
    fn test_swap_gpu_to_cpu_block_survives() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        // Store block on GPU
        idx.apply_event(&GatewayKvEvent::Store {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            sequence_hash: String::new(),
            prefix_hash: String::new(),
            local_hash: 100,
            parent_hash: None,
            block_index: 0,
            token_ids: vec![1, 2, 3, 4],
            block_size: 4,
            storage_tier: StorageTier::Gpu,
        });

        // Swap: block moves to CPU — update tier
        idx.apply_event(&GatewayKvEvent::Store {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            sequence_hash: String::new(),
            prefix_hash: String::new(),
            local_hash: 100,
            parent_hash: None,
            block_index: 0,
            token_ids: vec![1, 2, 3, 4],
            block_size: 4,
            storage_tier: StorageTier::Cpu,
        });

        // Remove from GPU — tier mismatch (now CPU), block should survive
        idx.apply_event(&GatewayKvEvent::EvictBlocks {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            block_hashes: vec![100],
            storage_tier: Some(StorageTier::Gpu),
        });

        // Block should still be in trie
        let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 1);
        // Tier should be CPU (worst among matched blocks)
        assert_eq!(matches[0].best_tier, StorageTier::Cpu);
        assert_eq!(idx.block_count(), 1);
    }

    // ---- Test: swap scenario — Remove GPU before Store CPU ----
    #[test]
    fn test_swap_remove_before_store() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        // Store block on GPU
        idx.apply_event(&GatewayKvEvent::Store {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            sequence_hash: String::new(),
            prefix_hash: String::new(),
            local_hash: 100,
            parent_hash: None,
            block_index: 0,
            token_ids: vec![1, 2, 3, 4],
            block_size: 4,
            storage_tier: StorageTier::Gpu,
        });

        // Remove from GPU — tier matches (GPU = GPU), block removed
        idx.apply_event(&GatewayKvEvent::EvictBlocks {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            block_hashes: vec![100],
            storage_tier: Some(StorageTier::Gpu),
        });

        // Block removed from trie
        let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        assert_eq!(matches.len(), 0);
        assert_eq!(idx.block_count(), 0);

        // Later, Store on CPU arrives — re-inserts
        idx.apply_event(&GatewayKvEvent::Store {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            sequence_hash: String::new(),
            prefix_hash: String::new(),
            local_hash: 100,
            parent_hash: None,
            block_index: 0,
            token_ids: vec![1, 2, 3, 4],
            block_size: 4,
            storage_tier: StorageTier::Cpu,
        });

        let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].best_tier, StorageTier::Cpu);
    }

    // ---- Test: mixed tier scoring uses worst tier ----
    #[test]
    fn test_mixed_tier_scoring_uses_worst() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        // Block 0 on GPU
        idx.apply_event(&GatewayKvEvent::Store {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            sequence_hash: String::new(),
            prefix_hash: String::new(),
            local_hash: 100,
            parent_hash: None,
            block_index: 0,
            token_ids: vec![1, 2, 3, 4],
            block_size: 4,
            storage_tier: StorageTier::Gpu,
        });

        // Block 1 on CPU (child of block 0)
        idx.apply_event(&GatewayKvEvent::Store {
            model: "m".to_string(),
            worker_id: "w0".to_string(),
            sequence_hash: String::new(),
            prefix_hash: String::new(),
            local_hash: 200,
            parent_hash: Some(100),
            block_index: 1,
            token_ids: vec![5, 6, 7, 8],
            block_size: 4,
            storage_tier: StorageTier::Cpu,
        });

        let matches =
            idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8], &["w0".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].match_depth, 2);
        // Worst tier across matched blocks is CPU
        assert_eq!(matches[0].best_tier, StorageTier::Cpu);
        // CPU priority_score = 0.7
        assert!((matches[0].tier_score - 0.7).abs() < f64::EPSILON);
    }

    // ---- Reverse-index invariants ----

    /// Assert `reverse_children` is an exact mirror of `block_parent`: every
    /// block is in its parent's reverse bucket, no bucket is empty (the leak
    /// risk under high eviction churn), and every reverse entry points back to a
    /// real `block_parent` row with a matching parent. Cardinalities must match.
    fn assert_reverse_invariant(idx: &TokenPrefixIndex) {
        let mut forward = 0usize;
        for entry in idx.block_parent.iter() {
            let (m, w, hash) = entry.key();
            let parent = entry.value();
            forward += 1;
            let bucket = idx
                .reverse_children
                .get(&(m.clone(), w.clone(), *parent))
                .unwrap_or_else(|| {
                    panic!("reverse bucket missing for parent {parent:?} (child {hash})")
                });
            assert!(
                bucket.contains(hash),
                "child {hash} missing from reverse bucket of parent {parent:?}"
            );
        }

        let mut reverse = 0usize;
        for entry in idx.reverse_children.iter() {
            let (m, w, parent) = entry.key();
            let children = entry.value();
            assert!(
                !children.is_empty(),
                "empty reverse bucket leaked for ({m:?}, {w:?}, {parent:?})"
            );
            for &child in children.iter() {
                reverse += 1;
                let bp = idx
                    .block_parent
                    .get(&(m.clone(), w.clone(), child))
                    .expect("reverse child has no block_parent row");
                assert_eq!(
                    *bp, *parent,
                    "reverse parent {parent:?} != block_parent parent for child {child}"
                );
            }
        }
        assert_eq!(forward, reverse, "reverse index cardinality drift");
    }

    #[test]
    fn test_reverse_index_consistency_under_structural_ops() {
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 500_000);

        // Two branching chains on w0 + a chain on w1.
        //   w0: 10 → 11 → 12 , 10 → 13
        //   w1: 20 → 21
        idx.apply_event(&store_event("m", "w0", 10, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m", "w0", 11, Some(10), vec![5, 6, 7, 8]));
        idx.apply_event(&store_event("m", "w0", 12, Some(11), vec![9, 10, 11, 12]));
        idx.apply_event(&store_event("m", "w0", 13, Some(10), vec![13, 14, 15, 16]));
        idx.apply_event(&store_event("m", "w1", 20, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m", "w1", 21, Some(20), vec![5, 6, 7, 8]));
        assert_reverse_invariant(&idx);

        // Cascading subtree eviction: evicting 10 removes 11,12,13 too.
        idx.apply_event(&evict_event("m", "w0", vec![10]));
        assert_reverse_invariant(&idx);
        assert_eq!(
            idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]).len(),
            0
        );

        // Dedup path: evict two roots where one is an ancestor of the other.
        idx.apply_event(&store_event("m", "w0", 30, None, vec![21, 22, 23, 24]));
        idx.apply_event(&store_event("m", "w0", 31, Some(30), vec![25, 26, 27, 28]));
        idx.apply_event(&evict_event("m", "w0", vec![30, 31]));
        assert_reverse_invariant(&idx);

        // Re-parenting: child arrives before its parent (delayed parent_block_hash).
        idx.apply_event(&store_event("m", "w0", 50, Some(49), vec![30, 31, 32, 33]));
        assert_reverse_invariant(&idx);
        idx.apply_event(&store_event("m", "w0", 49, None, vec![26, 27, 28, 29]));
        assert_reverse_invariant(&idx);

        // Full worker teardown drops every w1 bucket.
        idx.apply_event(&remove_event("w1"));
        assert_reverse_invariant(&idx);
        for entry in idx.reverse_children.iter() {
            assert_eq!(entry.key().1, "w0", "w1 bucket survived removal");
        }
    }

    #[test]
    fn test_reverse_index_no_bucket_leak_under_lru_churn() {
        // Capacity 4 forces an LRU eviction on almost every Store — the worst
        // case for empty-bucket accumulation in the reverse index.
        let idx = TokenPrefixIndex::new(4, 0.5, 0.3, 0.2, 4);

        // Seed a small chain, then hammer root Stores across two workers so the
        // global LRU queue churns repeatedly (evicting subtrees + their buckets).
        idx.apply_event(&store_event("m", "w0", 1, None, vec![1, 2, 3, 4]));
        idx.apply_event(&store_event("m", "w0", 2, Some(1), vec![5, 6, 7, 8]));
        for i in 0..400u64 {
            let w = if i % 2 == 0 { "w0" } else { "w1" };
            idx.apply_event(&store_event(
                "m",
                w,
                1000 + i,
                None,
                vec![i as u32, i as u32 + 1, i as u32 + 2, i as u32 + 3],
            ));
            if i % 64 == 0 {
                assert_reverse_invariant(&idx);
            }
        }
        assert_reverse_invariant(&idx);

        // After heavy churn no empty bucket may remain anywhere.
        for entry in idx.reverse_children.iter() {
            assert!(!entry.value().is_empty(), "empty bucket leaked after churn");
        }
    }
}
