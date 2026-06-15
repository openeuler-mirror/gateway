use boom_core::kv_event::KvIndexBackend;
use boom_core::provider::{DeploymentQueueInfo, Provider};
use std::sync::Arc;

use crate::inflight::InFlightTracker;
use super::{SchedulePolicy, Selection};

/// KV-cache aware scheduling policy.
///
/// Routes requests to the deployment with the highest KV-cache prefix overlap.
/// Falls back to lowest-load selection when no prefix match is found.
pub struct KvcAwarePolicy {
    /// Reference to the KV-cache prefix index.
    kv_index: Arc<dyn KvIndexBackend>,
    /// In-flight tracker for load-based fallback.
    tracker: Arc<InFlightTracker>,
    /// Optional flow control queue info for total load (in-flight + queued).
    queue_info: Option<Arc<dyn DeploymentQueueInfo>>,
}

impl KvcAwarePolicy {
    pub fn new(kv_index: Arc<dyn KvIndexBackend>, tracker: Arc<InFlightTracker>) -> Self {
        Self {
            kv_index,
            tracker,
            queue_info: None,
        }
    }

    /// Inject flow control queue info for total load queries.
    pub fn set_queue_info(&mut self, info: Arc<dyn DeploymentQueueInfo>) {
        self.queue_info = Some(info);
    }
}

impl SchedulePolicy for KvcAwarePolicy {
    fn select(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        _key_hash: Option<&str>,
        _input_chars: u64,
    ) -> Option<Arc<dyn Provider>> {
        // Without prefix context, fall back to lowest-load.
        select_lowest_load(&self.tracker, &self.queue_info, model, candidates)
    }

    fn select_with_context(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        _key_hash: Option<&str>,
        _input_chars: u64,
        token_ids: &[u32],
    ) -> Option<Selection> {
        if candidates.is_empty() {
            return None;
        }

        // Single candidate — no routing decision to make; skip the KV lookup.
        if candidates.len() == 1 {
            tracing::trace!(model, "single candidate, skip KVC lookup");
            return Some(Selection {
                provider: candidates[0].clone(),
                kv_hit_ratio: 0.0,
            });
        }

        // No token IDs — fall back to lowest-load.
        if token_ids.is_empty() {
            tracing::debug!(model, "no token_ids, fallback to lowest-load");
            return select_lowest_load(&self.tracker, &self.queue_info, model, candidates)
                .map(|provider| Selection { provider, kv_hit_ratio: 0.0 });
        }

        // Collect deployment IDs as worker IDs for KV index lookup.
        let worker_ids: Vec<String> = candidates
            .iter()
            .filter_map(|c| c.deployment_id().map(|id| id.to_string()))
            .collect();

        if worker_ids.is_empty() {
            tracing::debug!(model, "no deployment_id on candidates, fallback to lowest-load");
            return select_lowest_load(&self.tracker, &self.queue_info, model, candidates)
                .map(|provider| Selection { provider, kv_hit_ratio: 0.0 });
        }

        // Query KV index for best prefix match.
        let matches = self.kv_index.find_matches(model, token_ids, &worker_ids);

        if let Some(best) = matches.first() {
            tracing::info!(
                model,
                worker_id = %best.worker_id,
                match_depth = best.match_depth,
                hit_ratio = format!("{:.2}", best.hit_ratio),
                tier_score = format!("{:.2}", best.tier_score),
                load_score = format!("{:.2}", best.load_score),
                combined = format!("{:.3}", best.combined_score),
                candidates = worker_ids.len(),
                "KVC selected"
            );
            // Find the provider matching the best worker.
            if let Some(provider) = candidates.iter().find(|c| {
                c.deployment_id()
                    .map(|id| id == best.worker_id.as_str())
                    .unwrap_or(false)
            }) {
                // Carry the hit ratio out so the gateway can decide
                // full vs incremental reporting.
                return Some(Selection {
                    provider: provider.clone(),
                    kv_hit_ratio: best.hit_ratio,
                });
            }
        }

        // No match found — fall back to lowest-load. Hit ratio is 0, so the
        // gateway will request a full report to backfill the trie.
        tracing::debug!(
            model,
            token_count = token_ids.len(),
            candidates = worker_ids.len(),
            "no KV match, fallback to lowest-load"
        );
        select_lowest_load(&self.tracker, &self.queue_info, model, candidates)
            .map(|provider| Selection { provider, kv_hit_ratio: 0.0 })
    }

    fn name(&self) -> &str {
        "kvc_aware"
    }
}

use super::load_helpers::select_lowest_load;
