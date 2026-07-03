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
    /// Raw hit blocks (matched prefix blocks) for DFX. 0 when no KV match.
    pub kv_hit_blocks: u64,
    /// Raw input blocks (total prefix blocks in the request) for DFX.
    /// 0 when the request wasn't tokenized / no KV query.
    pub kv_input_blocks: u64,
    /// Whether the policy actually queried the KV index (`find_matches`) for
    /// this request. The gateway only requests a full KV-cache report from
    /// vLLM when this is `true` AND `kv_hit_ratio` is below threshold.
    ///
    /// `false` for all paths where KV-aware routing was a no-op: single
    /// candidate (no routing decision), missing tokenizer (no token_ids),
    /// candidates without a `kv_worker_id`, and non-KV policies. In a single-
    /// deployment setup the gateway must NOT ask vLLM for a full report —
    /// there is only one node, prefix affinity is pointless, and internal
    /// disaggregation scheduling (e.g. 2P1D) handles reuse on its own.
    pub kv_match_attempted: bool,
    /// Whether kvc_aware degraded to key_affinity for this request (hit below
    /// threshold, or empty token_ids). Used by the DFX log/dashboard to show
    /// the actual routing behavior (e.g. "kvc→key") rather than just the
    /// configured policy name.
    pub degraded: bool,
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
            .map(|provider| Selection { provider, kv_hit_ratio: 0.0, kv_hit_blocks: 0, kv_input_blocks: 0, kv_match_attempted: false, degraded: false })
    }

    /// Policy name (for display / config validation).
    fn name(&self) -> &str;
}
