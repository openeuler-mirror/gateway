pub mod flat;

use boom_core::kv_event::{DynamoKvEvent, KvMatchResult};
use std::collections::HashSet;

/// Backend interface for the KV-cache prefix index.
pub trait KvIndexBackend: Send + Sync {
    /// Apply a single KV event to the index.
    fn apply_event(&self, event: &DynamoKvEvent);

    /// Find workers with the best KV-cache prefix match for the given block hashes.
    ///
    /// Returns matches sorted by combined_score descending.
    fn find_matches(
        &self,
        model: &str,
        block_hashes: &[(usize, u64)],
        candidate_worker_ids: &[String],
    ) -> Vec<KvMatchResult>;

    /// Remove all index entries for a worker (e.g. on worker shutdown).
    fn remove_worker(&self, worker_id: &str);

    /// Total number of indexed blocks across all workers and models.
    fn block_count(&self) -> usize;

    /// Return all model names currently tracked in the index.
    fn model_names(&self) -> HashSet<String>;
}
