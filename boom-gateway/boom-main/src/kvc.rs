//! kvc-aware routing orchestration (self-contained, zero external dependency).
//!
//! All kvc business logic lives here — prefix serialization, trie query,
//! routing decision, DFX computation, and trie learning (record). The handler
//! (routes.rs) calls `KvcOrchestrator::route()` and gets back a `RouteResult`
//! or `None` (kvc disabled / no match). The handler never touches the trie
//! directly.
//!
//! No vLLM `/tokenize` HTTP and no ZMQ event stream: the trie is learned from
//! the gateway's own routed traffic. Each request's prefix (system+tools+
//! messages) is serialized to bytes, chunked, and xxhash'd into trie edge
//! keys. After routing, the chosen worker is recorded as having seen that
//! prefix so the next request with a matching prefix routes there. Eviction
//! is gateway-side only (LRU + TTL).
//!
//! Rate limiting (plan_charge) stays in the handler scope — it is NOT passed
//! into this module. The two-phase commit (peek → commit) is preserved.

use boom_core::kv_event::{KvIndexBackend, StorageTier};
use boom_core::provider::Provider;
use boom_core::types::ChatCompletionRequest;
use boom_config::Config;
use boom_routing::Router;
use std::sync::Arc;
use arc_swap::ArcSwap;
use tokio::sync::Semaphore;

/// Result of kvc-aware routing. When kvc is disabled or no match found,
/// the handler falls back to the default routing path.
pub struct RouteResult {
    pub provider: Arc<dyn Provider>,
    pub deployment_id: Option<String>,
    pub inflight_model: Option<String>,
    // DFX fields — None when kvc didn't produce them.
    pub schedule_policy: Option<String>,
    pub kv_hit_blocks: Option<i64>,
    pub kv_input_blocks: Option<i64>,
    pub trie_blocks: Option<i64>,
    pub trie_max_blocks: Option<i64>,
    pub request_bytes: Option<i64>,
}

/// Orchestrates all kvc-aware logic: prefix serialization → trie query → select
/// provider → DFX → record (learn). Created once at startup, stored in
/// AppState. Each request calls `route()`.
#[derive(Clone)]
pub struct KvcOrchestrator {
    kv_index: Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
    router: Arc<Router>,
    /// Bounds the number of concurrent best-effort record tasks so a request
    /// burst can't spawn an unbounded flock of trie-writers.
    record_semaphore: Arc<Semaphore>,
}

impl KvcOrchestrator {
    pub fn new(
        kv_index: Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
        router: Arc<Router>,
    ) -> Self {
        Self {
            kv_index,
            router,
            record_semaphore: Arc::new(Semaphore::new(32)),
        }
    }

    /// Serialize the request's prefix-relevant content (messages + tools) to a
    /// Serialize the request's prefix-relevant content (messages + tools) to a
    /// deterministic byte buffer. `serde_json::to_string` is deterministic for a
    /// given value (struct field order), so record and query — both derived
    /// from the same request — produce identical bytes.
    ///
    /// Claude Code's dynamic attribution block (`x-anthropic-billing-header`)
    /// is stripped by `strip_claude_code_attribution` BEFORE conversion to
    /// OpenAI format, so by the time we serialize here the system prompt is
    /// stable across turns — include it for maximum prefix depth.
    fn compute_prefix_bytes(req: &ChatCompletionRequest) -> Vec<u8> {
        let mut bytes = Vec::new();
        if let Ok(s) = serde_json::to_string(&req.messages) {
            bytes.extend_from_slice(s.as_bytes());
        }
        if let Some(tools) = &req.tools {
            if !tools.is_empty() {
                if let Ok(s) = serde_json::to_string(tools) {
                    bytes.extend_from_slice(s.as_bytes());
                }
            }
        }
        bytes
    }

    /// Prefix-serialize → trie query → select provider → DFX → record (learn).
    /// Returns None when kvc is disabled or no provider matches; the handler
    /// falls back to the default routing path.
    ///
    /// Each step is flat (no nested awaits in a handler state machine); the
    /// record step is spawned off the request path (best-effort, semaphore-
    /// bounded) so it never blocks the response.
    pub async fn route(
        &self,
        config: &Config,
        resolved_model: &str,
        key_hash: &str,
        input_chars: u64,
        req: &ChatCompletionRequest,
        candidates: &[Arc<dyn Provider>],
    ) -> Option<RouteResult> {
        // ── Step 0: kvc enabled? ──
        if config.router_settings.schedule_policy != "kvc_aware" {
            return None;
        }

        // ── Step 0.5: single candidate — no routing decision to make, and the
        //    trie will never be queried (select_with_context short-circuits on
        //    len==1). Skip prefix computation and record to save CPU + memory.
        //    Return None so the handler's default routing path handles it.
        if candidates.len() <= 1 {
            return None;
        }

        // ── Step 1: get kv_index (None = kvc not initialized) ──
        let kv_guard = self.kv_index.load();
        let kv_index = (**kv_guard).as_ref()?.clone();
        drop(kv_guard);

        // ── Step 2: serialize the request prefix to bytes (always — even on a
        // cold trie, so we can record and learn from this very request). ──
        let prefix_bytes = Self::compute_prefix_bytes(req);
        let request_bytes = if prefix_bytes.is_empty() {
            None
        } else {
            Some(prefix_bytes.len() as i64)
        };

        // ── Step 3: select provider (queries the trie with prefix_bytes). ──
        let selection = self.router.select_with_candidates(
            resolved_model,
            candidates,
            Some(key_hash),
            input_chars,
            &prefix_bytes,
        )?;
        let provider = selection.provider;
        let deployment_id = provider.deployment_id().map(|s| s.to_string());
        let inflight_model = self.router.resolve_model_name(resolved_model);

        // ── Step 4: compute DFX ──
        let schedule_policy = {
            let base = self.router.policy_name();
            if selection.degraded && base == "kvc_aware" {
                "kvc_aware→lowest_load".to_string()
            } else {
                base
            }
        };
        let kv_hit_blocks = selection.kv_hit_blocks;
        let kv_input_blocks = selection.kv_input_blocks;
        let trie_max_blocks = config.router_settings.kvc_aware.max_blocks.max(0) as i64;
        let trie_blocks = Some(kv_index.block_count() as i64);

        // ── Step 5: record (learn) — best-effort, off the request path. ──
        // Route the prefix under the chosen worker so the next request with a
        // matching prefix hits. On a capacity rebalance handoff (W→W') the
        // post-rebalance provider is recorded here, so the trie attribution
        // follows the actual routing (add-only; old worker's entry stays and
        // ages out via LRU/TTL — aligned with dynamo).
        if let Some(worker_id) = provider.kv_worker_id().map(|s| s.to_string()) {
            let kv_index = self.kv_index.clone();
            let model = resolved_model.to_string();
            // try_acquire: if too many record tasks are in flight, drop this
            // one (best-effort — a missed record just means one extra cold
            // request later, not a correctness issue).
            if let Ok(permit) = self.record_semaphore.clone().try_acquire_owned() {
                tokio::spawn(async move {
                    let _permit = permit; // held until record completes
                    let g = kv_index.load();
                    let idx = match (**g).as_ref() {
                        Some(i) => i.clone(),
                        None => return,
                    };
                    drop(g);
                    idx.record_request_prefix(&model, &worker_id, &prefix_bytes, StorageTier::Gpu);
                });
            }
        }

        Some(RouteResult {
            provider,
            deployment_id,
            inflight_model: Some(inflight_model),
            schedule_policy: Some(schedule_policy),
            kv_hit_blocks: Some(kv_hit_blocks as i64),
            kv_input_blocks: Some(kv_input_blocks as i64),
            trie_blocks,
            trie_max_blocks: Some(trie_max_blocks),
            request_bytes,
        })
    }
}
