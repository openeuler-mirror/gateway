use boom_core::kv_event::{GatewayKvEvent, KvMatchResult, LoadState, StorageTier};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};

use super::KvIndexBackend;

/// A node in the token-block trie.
///
/// Each transition represents a block of `block_size` token IDs.
/// Workers that have cached the full prefix up to this node are stored
/// in `workers`.
#[derive(Default)]
struct TrieNode {
    children: HashMap<Vec<u32>, Box<TrieNode>>,
    workers: HashSet<String>,
}

/// Token-prefix index using a per-model trie.
///
/// Matches incoming requests against cached KV prefixes by walking the trie
/// following the request's token blocks. No hash computation needed — matching
/// is done on raw token_ids, compatible with any vLLM configuration.
pub struct TokenPrefixIndex {
    /// Per-model trie root (model → trie root).
    tries: DashMap<String, RwLock<TrieNode>>,
    /// Per (model, worker) → ordered block token sequences (for removal).
    worker_sequences: DashMap<(String, String), Vec<Vec<u32>>>,
    /// Per (model, worker, hash) → block token_ids (for EvictBlocks lookup).
    hash_to_tokens: DashMap<(String, String, u64), Vec<u32>>,
    /// Tier tracking per (model, worker).
    tiers: DashMap<(String, String), StorageTier>,
    /// Load metrics per worker.
    loads: DashMap<String, LoadState>,
    /// Tokens per block (must match vLLM's block_size, typically 16).
    block_size: usize,
    cache_weight: f64,
    tier_weight: f64,
    load_weight: f64,
}

impl TokenPrefixIndex {
    pub fn new(block_size: usize, cache_weight: f64, tier_weight: f64, load_weight: f64) -> Self {
        Self {
            tries: DashMap::new(),
            worker_sequences: DashMap::new(),
            hash_to_tokens: DashMap::new(),
            tiers: DashMap::new(),
            loads: DashMap::new(),
            block_size,
            cache_weight,
            tier_weight,
            load_weight,
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

impl KvIndexBackend for TokenPrefixIndex {
    fn apply_event(&self, event: &GatewayKvEvent) {
        match event {
            GatewayKvEvent::Store {
                model,
                worker_id,
                local_hash,
                token_ids,
                storage_tier,
                ..
            } => {
                if token_ids.is_empty() {
                    return;
                }
                let block_tokens = token_ids.clone();

                // Read existing sequence BEFORE acquiring write lock.
                let seq_key = (model.clone(), worker_id.clone());
                let existing: Vec<Vec<u32>> = self
                    .worker_sequences
                    .get(&seq_key)
                    .map(|s| s.value().clone())
                    .unwrap_or_default();

                // Insert into trie: walk from root, add worker to the new node.
                let trie = self
                    .tries
                    .entry(model.clone())
                    .or_insert_with(|| RwLock::new(TrieNode::default()));
                {
                    let mut root = trie.write();

                    // Walk to the current end of this worker's path.
                    let mut node: &mut TrieNode = &mut root;
                    for block in &existing {
                        node = node
                            .children
                            .entry(block.clone())
                            .or_insert_with(|| Box::default())
                            .as_mut();
                    }

                    // Add new block node.
                    node.children
                        .entry(block_tokens.clone())
                        .or_insert_with(|| Box::default())
                        .workers
                        .insert(worker_id.clone());
                }

                // Append to worker's sequence (no write lock held).
                self.worker_sequences
                    .entry(seq_key)
                    .or_default()
                    .push(block_tokens.clone());

                // Store hash → tokens mapping.
                self.hash_to_tokens.insert(
                    (model.clone(), worker_id.clone(), *local_hash),
                    block_tokens,
                );

                // Update tier.
                self.tiers
                    .insert((model.clone(), worker_id.clone()), *storage_tier);
            }

            GatewayKvEvent::Remove { worker_id, .. } => {
                self.remove_worker(worker_id);
            }

            GatewayKvEvent::EvictBlocks {
                model,
                worker_id,
                block_hashes,
            } => {
                // Collect all hashes to remove from hash_to_tokens.
                // We process the first hash to find the drain position, then
                // clean up all orphaned hash entries for blocks after that position.
                let seq_key = (model.clone(), worker_id.clone());
                let mut drained_blocks: Vec<Vec<u32>> = Vec::new();

                for &hash in block_hashes {
                    let key = (model.clone(), worker_id.clone(), hash);
                    if let Some((_, tokens)) = self.hash_to_tokens.remove(&key) {
                        if let Some(mut seq) = self.worker_sequences.get_mut(&seq_key) {
                            if let Some(pos) = seq.iter().position(|b| *b == tokens) {
                                let removed: Vec<Vec<u32>> = seq.drain(pos..).collect();
                                drained_blocks.extend(removed);
                                break; // drain already removed everything after pos
                            }
                        }
                    }
                }

                if !drained_blocks.is_empty() {
                    // Remove worker from trie nodes for all drained blocks.
                    self.remove_worker_from_trie(model, worker_id, &drained_blocks);

                    // Clean up orphaned hash_to_tokens entries for the drained blocks.
                    // (The first hash was already removed above, but blocks after it
                    // still have entries that need cleanup.)
                    for block_tokens in &drained_blocks {
                        // We don't know the exact hash for each drained block,
                        // so scan and remove entries whose tokens match.
                        self.hash_to_tokens.retain(|k, v| {
                            !(k.1 == worker_id.as_str() && v == block_tokens)
                        });
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
        if token_ids.is_empty() || candidate_worker_ids.is_empty() {
            return Vec::new();
        }

        // Split into blocks.
        let request_blocks: Vec<Vec<u32>> = token_ids
            .chunks(self.block_size)
            .map(|c| c.to_vec())
            .collect();
        if request_blocks.is_empty() {
            return Vec::new();
        }

        let candidate_set: HashSet<&str> = candidate_worker_ids.iter().map(|s| s.as_str()).collect();

        // Walk trie, tracking match depth per candidate worker.
        let mut worker_depth: HashMap<String, u64> = HashMap::new();

        let trie_roots = self.tries.get(model);
        if let Some(root_lock) = trie_roots {
            let root = root_lock.read();

            // BFS: at each depth, track which trie nodes we're at and which workers matched.
            let mut current_nodes: Vec<&TrieNode> = vec![&root];
            let mut matched_workers: HashSet<&str> = candidate_set;

            for (depth, block) in request_blocks.iter().enumerate() {
                let mut next_nodes = Vec::new();
                let mut next_workers = HashSet::new();

                for node in &current_nodes {
                    if let Some(child) = node.children.get(block) {
                        next_nodes.push(child.as_ref());
                        for w in &child.workers {
                            if matched_workers.contains(w.as_str()) {
                                next_workers.insert(w.as_str());
                                worker_depth.insert(w.to_string(), (depth + 1) as u64);
                            }
                        }
                    }
                }

                if next_nodes.is_empty() || next_workers.is_empty() {
                    break;
                }
                current_nodes = next_nodes;
                matched_workers = next_workers;
            }
        }
        // DashMap guard dropped here — worker_depth owns its data.

        let total_blocks = request_blocks.len() as f64;
        let mut results: Vec<KvMatchResult> = Vec::new();

        for (wid, depth) in &worker_depth {
            if *depth == 0 {
                continue;
            }
            let hit_ratio = *depth as f64 / total_blocks;
            let tier = self
                .tiers
                .get(&(model.to_string(), wid.clone()))
                .map(|t| *t)
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
        let models: Vec<String> = self
            .worker_sequences
            .iter()
            .filter(|e| e.key().1 == worker_id)
            .map(|e| e.key().0.clone())
            .collect();

        for model in &models {
            let seq_key = (model.clone(), worker_id.to_string());
            if let Some((_, blocks)) = self.worker_sequences.remove(&seq_key) {
                self.remove_worker_from_trie(model, worker_id, &blocks);
            }
        }

        let hash_keys: Vec<(String, String, u64)> = self
            .hash_to_tokens
            .iter()
            .filter(|e| e.key().1 == worker_id)
            .map(|e| e.key().clone())
            .collect();
        for key in hash_keys {
            self.hash_to_tokens.remove(&key);
        }

        self.tiers.retain(|k, _| k.1 != worker_id);
        self.loads.remove(worker_id);
    }

    fn block_count(&self) -> usize {
        self.worker_sequences.iter().map(|e| e.value().len()).sum()
    }

    fn model_names(&self) -> HashSet<String> {
        self.tries.iter().map(|e| e.key().clone()).collect()
    }

    fn debug_dump(&self) -> Vec<(String, Vec<u32>, Vec<String>, StorageTier)> {
        let mut result = Vec::new();
        for entry in &self.tries {
            let model = entry.key().clone();
            let root = entry.value().read();
            Self::walk_trie(&root, &model, &mut result, &self.tiers);
        }
        result
    }
}

impl TokenPrefixIndex {
    /// Remove a worker from specific trie nodes along a path (iterative).
    fn remove_worker_from_trie(&self, model: &str, worker_id: &str, blocks: &[Vec<u32>]) {
        if let Some(root) = self.tries.get(model) {
            let mut guard = root.write();
            let mut node: &mut TrieNode = &mut guard;
            for block in blocks {
                match node.children.get_mut(block) {
                    Some(child) => {
                        child.workers.remove(worker_id);
                        node = child;
                    }
                    None => break,
                }
            }
        }
    }

    fn walk_trie(
        root: &TrieNode,
        model: &str,
        result: &mut Vec<(String, Vec<u32>, Vec<String>, StorageTier)>,
        tiers: &DashMap<(String, String), StorageTier>,
    ) {
        // Iterative DFS using an explicit stack to avoid stack overflow.
        let mut stack: Vec<&TrieNode> = vec![root];
        while let Some(node) = stack.pop() {
            for (block_tokens, child) in &node.children {
                if !child.workers.is_empty() {
                    let workers: Vec<String> = child.workers.iter().cloned().collect();
                    let tier = workers
                        .first()
                        .and_then(|w| tiers.get(&(model.to_string(), w.clone())).map(|t| *t))
                        .unwrap_or(StorageTier::Gpu);
                    result.push((model.to_string(), block_tokens.clone(), workers, tier));
                }
                stack.push(child);
            }
        }
    }
}
