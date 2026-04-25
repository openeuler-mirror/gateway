pub mod key_affinity;
pub mod round_robin;

use boom_core::provider::Provider;
use std::sync::Arc;

/// Trait for scheduling policies. Each policy decides which provider
/// to select from a list of candidates for a given model.
pub trait SchedulePolicy: Send + Sync {
    /// Select one provider from the candidates list for the given model.
    ///
    /// - `model`: the resolved model name (after alias resolution).
    /// - `candidates`: available provider deployments for this model.
    /// - `key_hash`: SHA-256 hash of the API key making the request (if available).
    /// - `input_chars`: estimated input character count for this request.
    fn select(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        input_chars: u64,
    ) -> Option<Arc<dyn Provider>>;

    /// Policy name (for display / config validation).
    fn name(&self) -> &str;
}
