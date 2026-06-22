pub mod key_affinity;
pub mod kvc_aware;
pub mod load_helpers;
pub mod round_robin;

use boom_core::provider::Provider;
use std::sync::Arc;

/// Result of provider selection, carrying the KV-cache prefix hit ratio
/// for the request so the gateway can decide full vs incremental reporting.
pub struct Selection {
    pub provider: Arc<dyn Provider>,
    /// KV-cache prefix hit ratio (matched_blocks / total_blocks) for this
    /// request. `0.0` when the policy has no KV context (e.g. round_robin)
    /// or no prefix match was found.
    pub kv_hit_ratio: f64,
}

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
    /// Returns a [`Selection`] whose `kv_hit_ratio` reflects how much of the
    /// request's prefix the KV index already covers — the gateway uses it to
    /// decide full vs incremental reporting.
    ///
    /// Default implementation delegates to `select()` (no KV-cache awareness),
    /// reporting `kv_hit_ratio = 0.0`.
    fn select_with_context(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        input_chars: u64,
        _token_ids: &[u32],
    ) -> Option<Selection> {
        self.select(model, candidates, key_hash, input_chars)
            .map(|provider| Selection { provider, kv_hit_ratio: 0.0 })
    }

    /// Policy name (for display / config validation).
    fn name(&self) -> &str;
}
