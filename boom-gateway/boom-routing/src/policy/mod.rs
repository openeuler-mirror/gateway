pub mod key_affinity;
pub mod kvc_aware;
pub mod load_helpers;
pub mod round_robin;

use boom_core::provider::Provider;
use std::sync::Arc;

/// Trait for scheduling policies. Each policy decides which provider
/// to select from a list of candidates for a given model.
pub trait SchedulePolicy: Send + Sync {
    /// Select one provider from the candidates list for the given model.
    fn select(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        input_chars: u64,
    ) -> Option<Arc<dyn Provider>>;

    /// Select a provider using token IDs for KV-cache prefix-aware routing.
    ///
    /// Default implementation delegates to `select()` (no KV-cache awareness).
    fn select_with_context(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        input_chars: u64,
        _token_ids: &[u32],
    ) -> Option<Arc<dyn Provider>> {
        self.select(model, candidates, key_hash, input_chars)
    }

    /// Policy name (for display / config validation).
    fn name(&self) -> &str;
}
