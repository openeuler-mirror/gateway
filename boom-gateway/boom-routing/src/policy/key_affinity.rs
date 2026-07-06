use boom_core::provider::{DeploymentQueueInfo, Provider};
use dashmap::DashMap;
use std::sync::Arc;

use crate::inflight::InFlightTracker;
use crate::rebalance::RebalanceMoveTracker;
use super::SchedulePolicy;

/// Key-affinity scheduling: route requests from the same API key to the same
/// provider deployment for a given model, with load-aware rebalancing.
///
/// Affinity key: `{key_hash}:{model}` → `deployment_id`
///
/// Logic:
///   1. Single candidate → direct return
///   2. No key_hash provided → fall back to lowest-load
///   3. Below context_threshold → lowest-load (warm-up phase)
///   4. Affinity lookup → validate deployment still exists → rebalance check
///   5. First time or affinity miss → lowest-load → write affinity map
pub struct KeyAffinityPolicy {
    /// Reference to the in-flight tracker for load queries.
    tracker: Arc<InFlightTracker>,
    /// Optional flow control queue info for total load (in-flight + queued).
    queue_info: Option<Arc<dyn DeploymentQueueInfo>>,
    /// Affinity map: `{key_hash}:{model}` → `deployment_id`
    affinity: DashMap<String, String>,
    /// Context threshold: below this total input_chars across all providers,
    /// always pick lowest-load (warm-up to distribute initial load).
    /// 0 means always use affinity (no warm-up).
    context_threshold: u64,
    /// Rebalance threshold: if the preferred provider's utilization exceeds
    /// the minimum by more than this percentage (1..=100), reassign to the
    /// least loaded provider.
    rebalance_threshold: u8,
    /// Optional tracker for per-deployment rebalance move counts (in/out).
    rebalance_move_tracker: Option<Arc<RebalanceMoveTracker>>,
}

impl KeyAffinityPolicy {
    pub fn new(
        tracker: Arc<InFlightTracker>,
        context_threshold: u64,
        rebalance_threshold: u8,
        rebalance_move_tracker: Option<Arc<RebalanceMoveTracker>>,
    ) -> Self {
        Self {
            tracker,
            queue_info: None,
            affinity: DashMap::new(),
            context_threshold,
            rebalance_threshold,
            rebalance_move_tracker,
        }
    }

    /// Inject flow control queue info for total load queries.
    pub fn set_queue_info(&mut self, info: Arc<dyn DeploymentQueueInfo>) {
        self.queue_info = Some(info);
    }
}

impl SchedulePolicy for KeyAffinityPolicy {
    fn select(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        _input_chars: u64,
    ) -> Option<Arc<dyn Provider>> {
        if candidates.is_empty() {
            return None;
        }
        if candidates.len() == 1 {
            return Some(candidates[0].clone());
        }

        // No key context → fall back to lowest-load.
        let key_hash = match key_hash {
            Some(k) => k,
            None => return select_lowest_load(&self.tracker, &self.queue_info, model, candidates),
        };

        let affinity_key = format!("{}:{}", key_hash, model);

        // Check if total in-flight is below context_threshold (warm-up).
        if self.context_threshold > 0 {
            let total_input = self.tracker.get_model_input_chars(model);

            if total_input < self.context_threshold {
                // Warm-up: pick lowest-load and record affinity.
                let provider = select_lowest_load(&self.tracker, &self.queue_info, model, candidates);
                if let Some(ref p) = provider {
                    if let Some(did) = p.deployment_id() {
                        self.affinity.insert(affinity_key, did.to_string());
                    }
                }
                return provider;
            }
        }

        // Look up existing affinity.
        // Clone the value immediately to release the DashMap read guard
        // before any subsequent insert() — holding a Ref while inserting
        // into the same shard causes a parking_lot::RwLock self-deadlock.
        let preferred_id = self.affinity.get(&affinity_key)
            .map(|r| r.value().clone());

        if let Some(ref preferred_id) = preferred_id {
            // Find the preferred provider in candidates.
            if let Some(provider) = candidates.iter().find(|c| {
                c.deployment_id()
                    .map(|id| id == preferred_id.as_str())
                    .unwrap_or(false)
            }) {
                // Rebalance check: if the preferred provider is significantly
                // more loaded than the least-loaded candidate, reassign.
                let load_preferred = load_for_deployment(&self.tracker, &self.queue_info, model, provider.as_ref());
                let (min_load, least_loaded) = min_load_candidate(&self.tracker, &self.queue_info, model, candidates);

                if load_preferred > min_load + self.rebalance_threshold as u64 {
                    // Rebalance to least loaded.
                    if let Some(ref tracker) = self.rebalance_move_tracker {
                        tracker.record_move(
                            provider.deployment_id(),
                            least_loaded.deployment_id(),
                        );
                    }
                    if let Some(did) = least_loaded.deployment_id() {
                        self.affinity.insert(affinity_key, did.to_string());
                    }
                    return Some(least_loaded);
                }

                return Some(provider.clone());
            }
            // Preferred deployment no longer in candidates — fall through.
        }

        // First time or affinity miss: pick lowest-load and record.
        let provider = select_lowest_load(&self.tracker, &self.queue_info, model, candidates);
        if let Some(ref p) = provider {
            if let Some(did) = p.deployment_id() {
                self.affinity.insert(affinity_key, did.to_string());
            }
        }
        provider
    }

    fn name(&self) -> &str {
        "key_affinity"
    }
}

use super::load_helpers::{deployment_load, min_load_candidate, select_lowest_load};

/// Get the load for a specific deployment (used in rebalance check).
fn load_for_deployment(
    tracker: &InFlightTracker,
    queue_info: &Option<Arc<dyn DeploymentQueueInfo>>,
    model: &str,
    provider: &dyn Provider,
) -> u64 {
    deployment_load(tracker, queue_info, model, provider)
}
