use boom_core::provider::{DeploymentQueueInfo, Provider};
use std::sync::Arc;

use crate::inflight::InFlightTracker;

/// Select the candidate with the fewest total load (in-flight + queued).
pub fn select_lowest_load(
    tracker: &InFlightTracker,
    queue_info: &Option<Arc<dyn DeploymentQueueInfo>>,
    model: &str,
    candidates: &[Arc<dyn Provider>],
) -> Option<Arc<dyn Provider>> {
    let (_min_load, provider) = min_load_candidate(tracker, queue_info, model, candidates);
    Some(provider)
}

/// Find the candidate with the lowest total load (O(1) per candidate).
pub fn min_load_candidate(
    tracker: &InFlightTracker,
    queue_info: &Option<Arc<dyn DeploymentQueueInfo>>,
    model: &str,
    candidates: &[Arc<dyn Provider>],
) -> (u64, Arc<dyn Provider>) {
    let mut best = candidates[0].clone();
    let mut best_load = u64::MAX;

    for candidate in candidates {
        let load = deployment_load(tracker, queue_info, model, candidate.as_ref());

        if load < best_load {
            best_load = load;
            best = candidate.clone();
        }
    }

    (best_load, best)
}

/// Rebalance decision: should the preferred candidate hand off to the
/// least-loaded one? True when the preferred's load_pct exceeds the minimum
/// by more than `threshold` (all in 0..100 percentage points). Shared by
/// kvc_aware (per-request, at scoring stage) and key_affinity (per-key
/// migration) so both use identical rebalance semantics.
pub fn should_rebalance(preferred_load: u64, min_load: u64, threshold: u64) -> bool {
    preferred_load > min_load + threshold
}

/// Get the normalized utilization for a deployment (percentage, 0..100).
///
/// Formula: `raw_load * 100 / max_inflight`
/// - `raw_load` = max(in-flight from tracker, FC queue total), avoiding double-counting.
/// - `max_inflight` = 0 means unlimited capacity → returns `raw_load` as-is (no normalization).
pub fn deployment_load(
    tracker: &InFlightTracker,
    queue_info: &Option<Arc<dyn DeploymentQueueInfo>>,
    model: &str,
    provider: &dyn Provider,
) -> u64 {
    match provider.deployment_id() {
        Some(id) => {
            let inflight = tracker.get_deployment_count(model, id);
            let queued = queue_info.as_ref().map(|q| q.total_load(id)).unwrap_or(0);
            // queued already includes current_inflight from FlowControl,
            // which ≈ inflight. Use max to avoid double-counting.
            let raw_load = std::cmp::max(inflight, queued);

            let capacity = queue_info
                .as_ref()
                .map(|q| q.max_capacity(id))
                .unwrap_or(0);

            if capacity > 0 && raw_load > 0 {
                // Normalize to percentage: raw_load * 100 / capacity.
                // Both sides are then in 0..100 range, comparable across
                // deployments with different max_inflight values.
                raw_load * 100 / (capacity as u64)
            } else {
                raw_load
            }
        }
        None => 0,
    }
}
