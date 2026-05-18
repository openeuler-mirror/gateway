use boom_core::provider::{DeploymentQueueInfo, Provider};
use boom_flowcontrol::FlowController;
use boom_kvindex::KvIndexBackend;
use boom_routing::inflight::InFlightTracker;
use boom_routing::policy::SchedulePolicy;
use std::sync::Arc;

/// KV-cache aware scheduling policy.
///
/// Routes requests to the deployment with the highest KV-cache prefix overlap.
/// Falls back to lowest-load selection when no prefix match is found or when
/// no prefix block hashes are provided.
pub struct KvcAwarePolicy {
    /// Reference to the KV-cache prefix index.
    kv_index: Arc<dyn KvIndexBackend>,
    /// In-flight tracker for load-based fallback.
    tracker: Arc<InFlightTracker>,
    /// Flow controller for total load (in-flight + queued).
    flow_controller: Arc<FlowController>,
}

impl KvcAwarePolicy {
    pub fn new(
        kv_index: Arc<dyn KvIndexBackend>,
        tracker: Arc<InFlightTracker>,
        flow_controller: Arc<FlowController>,
    ) -> Self {
        Self {
            kv_index,
            tracker,
            flow_controller,
        }
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
        select_lowest_load(&self.tracker, &self.flow_controller, model, candidates)
    }

    fn select_with_context(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        _key_hash: Option<&str>,
        _input_chars: u64,
        prefix_block_hashes: &[(usize, u64)],
    ) -> Option<Arc<dyn Provider>> {
        if candidates.is_empty() {
            return None;
        }
        if candidates.len() == 1 {
            return Some(candidates[0].clone());
        }

        // No prefix hashes — fall back to lowest-load.
        if prefix_block_hashes.is_empty() {
            return select_lowest_load(&self.tracker, &self.flow_controller, model, candidates);
        }

        // Collect deployment IDs as worker IDs for KV index lookup.
        let worker_ids: Vec<String> = candidates
            .iter()
            .filter_map(|c| c.deployment_id().map(|id| id.to_string()))
            .collect();

        if worker_ids.is_empty() {
            return select_lowest_load(&self.tracker, &self.flow_controller, model, candidates);
        }

        // Query KV index for best prefix match.
        let matches = self.kv_index.find_matches(model, prefix_block_hashes, &worker_ids);

        if let Some(best) = matches.first() {
            // Find the provider matching the best worker.
            if let Some(provider) = candidates.iter().find(|c| {
                c.deployment_id()
                    .map(|id| id == best.worker_id.as_str())
                    .unwrap_or(false)
            }) {
                return Some(provider.clone());
            }
        }

        // No match found — fall back to lowest-load.
        select_lowest_load(&self.tracker, &self.flow_controller, model, candidates)
    }

    fn name(&self) -> &str {
        "kv_aware"
    }
}

/// Select the candidate with the fewest total load (in-flight + queued).
fn select_lowest_load(
    tracker: &InFlightTracker,
    flow_controller: &FlowController,
    model: &str,
    candidates: &[Arc<dyn Provider>],
) -> Option<Arc<dyn Provider>> {
    let (_min_load, provider) = min_load_candidate(tracker, flow_controller, model, candidates);
    Some(provider)
}

/// Find the candidate with the lowest total load.
fn min_load_candidate(
    tracker: &InFlightTracker,
    flow_controller: &FlowController,
    model: &str,
    candidates: &[Arc<dyn Provider>],
) -> (u64, Arc<dyn Provider>) {
    let mut best = candidates[0].clone();
    let mut best_load = u64::MAX;

    for candidate in candidates {
        let load = deployment_load(tracker, flow_controller, model, candidate.as_ref());

        if load < best_load {
            best_load = load;
            best = candidate.clone();
        }
    }

    (best_load, best)
}

/// Get the total load for a deployment: max(in-flight, FC total_load).
fn deployment_load(
    tracker: &InFlightTracker,
    flow_controller: &FlowController,
    model: &str,
    provider: &dyn Provider,
) -> u64 {
    match provider.deployment_id() {
        Some(id) => {
            let inflight = tracker.get_deployment_count(model, id);
            let queued = flow_controller.total_load(id);
            std::cmp::max(inflight, queued)
        }
        None => 0,
    }
}
