use dashmap::DashMap;
use std::sync::Arc;

use crate::rebalance::RebalanceCounter;

const TOTAL_KEY: &str = "_total";

/// Tracks per-deployment request counts over the last 60 minutes.
///
/// Internally a DashMap of deployment_id → RebalanceCounter (60-bucket ring buffer).
/// A special `_total` key aggregates all deployments.
pub struct RequestRateTracker {
    counters: DashMap<String, Arc<RebalanceCounter>>,
}

impl RequestRateTracker {
    pub fn new() -> Self {
        let counters = DashMap::new();
        counters.insert(TOTAL_KEY.to_string(), Arc::new(RebalanceCounter::new()));
        Self { counters }
    }

    /// Record one request for the given deployment_id (also increments _total).
    pub fn record(&self, deployment_id: &str) {
        // Ensure deployment counter exists.
        self.counters
            .entry(deployment_id.to_string())
            .or_insert_with(|| Arc::new(RebalanceCounter::new()))
            .record();

        // Always increment total.
        if let Some(total) = self.counters.get(TOTAL_KEY) {
            total.record();
        }
    }

    /// Snapshot 60 data points for a single deployment.
    pub fn snapshot(&self, deployment_id: &str) -> Vec<(String, u64)> {
        self.counters
            .get(deployment_id)
            .map(|c| c.snapshot())
            .unwrap_or_default()
    }

    /// Snapshot all deployments: returns Vec of (deployment_id, snapshot_data).
    /// First entry is always `_total`, rest sorted by deployment_id.
    pub fn snapshot_all(&self) -> Vec<(String, Vec<(String, u64)>)> {
        let mut result = Vec::new();

        // Total first.
        if let Some(total) = self.counters.get(TOTAL_KEY) {
            result.push((TOTAL_KEY.to_string(), total.snapshot()));
        }

        // Per-deployment, sorted.
        let mut deps: Vec<String> = self
            .counters
            .iter()
            .filter(|e| e.key() != TOTAL_KEY)
            .map(|e| e.key().to_string())
            .collect();
        deps.sort();

        for dep in deps {
            if let Some(counter) = self.counters.get(&dep) {
                result.push((dep, counter.snapshot()));
            }
        }

        result
    }

    /// List all tracked deployment IDs (excluding _total).
    pub fn deployment_ids(&self) -> Vec<String> {
        self.counters
            .iter()
            .filter(|e| e.key() != TOTAL_KEY)
            .map(|e| e.key().to_string())
            .collect()
    }
}
