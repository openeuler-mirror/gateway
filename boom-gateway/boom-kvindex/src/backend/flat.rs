use boom_core::kv_event::{DynamoKvEvent, KvMatchResult, LoadState, StorageTier};
use dashmap::DashMap;
use std::collections::HashSet;

use super::KvIndexBackend;

/// Flat hash-based KV-cache prefix index.
///
/// Data structures:
/// - `blocks`: `(model, local_hash)` → `HashSet<worker_id>` — forward lookup
/// - `reverse`: `worker_id` → `HashSet<(model, local_hash)>` — reverse for cleanup
/// - `block_tiers`: `(model, local_hash, worker_id)` → `StorageTier` — tier tracking
/// - `loads`: `worker_id` → `LoadState` — load metrics per worker
pub struct FlatIndex {
    /// Forward index: (model, local_hash) → set of worker IDs that hold this block.
    blocks: DashMap<(String, u64), HashSet<String>>,
    /// Reverse index: worker_id → set of (model, local_hash) pairs owned by this worker.
    reverse: DashMap<String, HashSet<(String, u64)>>,
    /// Tier tracking: (model, local_hash, worker_id) → storage tier.
    block_tiers: DashMap<(String, u64, String), StorageTier>,
    /// Load metrics per worker.
    loads: DashMap<String, LoadState>,
    /// Weight for cache hit score in combined scoring.
    cache_weight: f64,
    /// Weight for tier score in combined scoring.
    tier_weight: f64,
    /// Weight for load score in combined scoring.
    load_weight: f64,
}

impl FlatIndex {
    pub fn new(cache_weight: f64, tier_weight: f64, load_weight: f64) -> Self {
        Self {
            blocks: DashMap::new(),
            reverse: DashMap::new(),
            block_tiers: DashMap::new(),
            loads: DashMap::new(),
            cache_weight,
            tier_weight,
            load_weight,
        }
    }

    /// Compute the longest consecutive prefix match depth for a single worker.
    ///
    /// Walks block_hashes in order (block_index 0, 1, 2, ...) and counts
    /// how many consecutive blocks are present for this worker.
    fn consecutive_prefix_depth(
        &self,
        model: &str,
        block_hashes: &[(usize, u64)],
        worker_id: &str,
    ) -> (u64, StorageTier) {
        let mut depth = 0u64;
        let mut best_tier = StorageTier::Remote;

        for (i, &(_block_idx, local_hash)) in block_hashes.iter().enumerate() {
            let key = (model.to_string(), local_hash);
            if let Some(workers) = self.blocks.get(&key) {
                if workers.contains(worker_id) {
                    depth = (i + 1) as u64;
                    // Track best tier for this prefix.
                    let tier_key = (model.to_string(), local_hash, worker_id.to_string());
                    if let Some(tier) = self.block_tiers.get(&tier_key) {
                        if tier.priority_score() > best_tier.priority_score() {
                            best_tier = *tier;
                        }
                    }
                    continue;
                }
            }
            // Gap found — stop counting consecutive prefix.
            break;
        }

        (depth, best_tier)
    }

    /// Compute load score for a worker (1.0 = idle, 0.0 = fully loaded).
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
            // No load info — assume neutral.
            0.5
        }
    }
}

impl KvIndexBackend for FlatIndex {
    fn apply_event(&self, event: &DynamoKvEvent) {
        match event {
            DynamoKvEvent::Store {
                worker_id,
                sequence_hash: _,
                prefix_hash: _,
                local_hash,
                block_index: _,
                storage_tier,
            } => {
                // We use local_hash as the key. In a per-model index we'd use
                // model as part of the key, but events don't carry model info
                // directly — the worker maps to a single model deployment.
                // For MVP we use an empty model key; the model will be resolved
                // at query time via the deployment's model_name.
                // TODO: add model to DynamoKvEvent::Store if workers serve multiple models.
                let model = ""; // Will be matched by deployment_id at query time.

                // Forward index.
                self.blocks
                    .entry((model.to_string(), *local_hash))
                    .or_default()
                    .insert(worker_id.clone());

                // Reverse index.
                self.reverse
                    .entry(worker_id.clone())
                    .or_default()
                    .insert((model.to_string(), *local_hash));

                // Tier tracking.
                self.block_tiers.insert(
                    (model.to_string(), *local_hash, worker_id.clone()),
                    *storage_tier,
                );
            }

            DynamoKvEvent::Remove {
                worker_id,
                sequence_hash: _,
                storage_tier: _tier_filter,
            } => {
                // Remove all blocks for this worker/sequence via reverse index.
                // Note: sequence-level removal would require a per-sequence reverse index.
                // For MVP, we remove ALL blocks for this worker (worker-level eviction).
                if let Some((_, hashes)) = self.reverse.remove(worker_id) {
                    for (model, local_hash) in hashes {
                        self.block_tiers
                            .remove(&(model.clone(), local_hash, worker_id.clone()));
                        if let Some(mut workers) = self.blocks.get_mut(&(model.clone(), local_hash))
                        {
                            workers.remove(worker_id);
                            if workers.is_empty() {
                                drop(workers);
                                self.blocks.remove(&(model, local_hash));
                            }
                        }
                    }
                }
            }

            DynamoKvEvent::LoadMetrics {
                worker_id,
                queue_depth,
                kv_utilization,
                running_requests,
                tier_usage: _,
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
        block_hashes: &[(usize, u64)],
        candidate_worker_ids: &[String],
    ) -> Vec<KvMatchResult> {
        if block_hashes.is_empty() || candidate_worker_ids.is_empty() {
            return Vec::new();
        }

        let total_blocks = block_hashes.len() as f64;
        let mut results: Vec<KvMatchResult> = Vec::with_capacity(candidate_worker_ids.len());

        for worker_id in candidate_worker_ids {
            let (match_depth, best_tier) =
                self.consecutive_prefix_depth(model, block_hashes, worker_id);

            if match_depth == 0 {
                continue;
            }

            let hit_ratio = match_depth as f64 / total_blocks;
            let tier_score = best_tier.priority_score();
            let load_score = self.load_score_for(worker_id);

            let combined_score = self.cache_weight * hit_ratio
                + self.tier_weight * tier_score
                + self.load_weight * load_score;

            results.push(KvMatchResult {
                worker_id: worker_id.clone(),
                match_depth,
                hit_ratio,
                best_tier,
                tier_score,
                load_score,
                combined_score,
            });
        }

        // Sort by combined_score descending.
        results.sort_by(|a, b| {
            b.combined_score
                .partial_cmp(&a.combined_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        results
    }

    fn remove_worker(&self, worker_id: &str) {
        if let Some((_, hashes)) = self.reverse.remove(worker_id) {
            for (model, local_hash) in hashes {
                self.block_tiers
                    .remove(&(model.clone(), local_hash, worker_id.to_string()));
                if let Some(mut workers) = self.blocks.get_mut(&(model.clone(), local_hash)) {
                    workers.remove(worker_id);
                    if workers.is_empty() {
                        drop(workers);
                        self.blocks.remove(&(model, local_hash));
                    }
                }
            }
        }
        self.loads.remove(worker_id);
    }

    fn block_count(&self) -> usize {
        // Sum of unique (model, hash) entries.
        self.blocks.len()
    }

    fn model_names(&self) -> HashSet<String> {
        self.blocks
            .iter()
            .map(|entry| entry.key().0.clone())
            .filter(|m| !m.is_empty())
            .collect()
    }
}
