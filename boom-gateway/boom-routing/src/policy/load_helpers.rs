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

/// Get the total load for a deployment: max(in-flight, FC queue depth).
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
            std::cmp::max(inflight, queued)
        }
        None => 0,
    }
}
