use boom_core::provider::Provider;
use dashmap::DashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::SchedulePolicy;

/// Round-robin scheduling: cycles through providers sequentially per model.
pub struct RoundRobinPolicy {
    counters: DashMap<String, AtomicUsize>,
}

impl RoundRobinPolicy {
    pub fn new() -> Self {
        Self {
            counters: DashMap::new(),
        }
    }
}

impl SchedulePolicy for RoundRobinPolicy {
    fn select(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        _key_hash: Option<&str>,
        _input_chars: u64,
    ) -> Option<Arc<dyn Provider>> {
        if candidates.is_empty() {
            return None;
        }
        if candidates.len() == 1 {
            return Some(candidates[0].clone());
        }

        let counter = self
            .counters
            .entry(model.to_string())
            .or_insert_with(|| AtomicUsize::new(0));
        let idx = counter.fetch_add(1, Ordering::Relaxed);
        Some(candidates[idx % candidates.len()].clone())
    }

    fn name(&self) -> &str {
        "round_robin"
    }
}

impl Default for RoundRobinPolicy {
    fn default() -> Self {
        Self::new()
    }
}
