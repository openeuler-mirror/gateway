//! kvc-aware routing orchestration (self-contained, zero external dependency).
//!
//! All kvc business logic lives here — prefix serialization, trie query,
//! routing decision, DFX computation, and trie learning (record). The handler
//! (routes.rs) calls `KvcOrchestrator::route()` and gets back a `RouteResult`
//! or `None` (kvc disabled / no match), then calls
//! [`KvcOrchestrator::record_success`] on the provider's Ok path — learning
//! happens ONLY after admission success (a failed prefix must never be
//! pinned to the worker that rejected it; see design doc §7). The handler
//! never touches the trie directly.
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
    /// Serialized request prefix (tools + messages). Non-empty only when the
    /// orchestrator computed it (kvc_aware + multi-candidate). The handler
    /// passes it to [`KvcOrchestrator::record_success`] AFTER the provider
    /// accepts the request — recording is deferred to the Ok path so failed
    /// prefixes never learn (tie trap, design doc §7). Empty = nothing to
    /// record (kvc disabled / single candidate — matches the old skip).
    pub prefix_bytes: Vec<u8>,
}

/// Orchestrates all kvc-aware logic: prefix serialization → trie query → select
/// provider → DFX → record (learn). Created once at startup, stored in
/// AppState. Each request calls `route()`.
#[derive(Clone)]
pub struct KvcOrchestrator {
    kv_index: Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
    router: Arc<Router>,
    /// Bounds the number of concurrent best-effort record tasks so a request
    /// burst can't spawn an unbounded flock of trie-writers. Record work is
    /// pure synchronous CPU (hash + trie walk); concurrency beyond the CPU
    /// count only adds lock contention, and each task runs on the blocking
    /// pool (spawn_blocking) so tokio's async workers are never occupied by it.
    record_semaphore: Arc<Semaphore>,
}

/// Record tasks are throughput-bound sync CPU — cap concurrency at the CPU
/// count (further tasks just contend on trie shard locks), with a small
/// ceiling to leave headroom in the shared blocking pool for real I/O.
fn record_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8)
}

impl KvcOrchestrator {
    pub fn new(
        kv_index: Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
        router: Arc<Router>,
    ) -> Self {
        Self {
            kv_index,
            router,
            record_semaphore: Arc::new(Semaphore::new(record_concurrency())),
        }
    }

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
        // Fixed prefix (tools) MUST come before the per-turn suffix (messages):
        // across turns, only `messages` grows. Continuous trie matching breaks at
        // the first differing block (the messages-growth point), so anything
        // placed AFTER that point — including a fixed tools block — can never be
        // reached and is permanently counted as a miss, even though it was
        // recorded. Putting tools first keeps the fixed segment ahead of the
        // divergence point, so it matches every turn. This also matches the
        // chat-template render order of every served model (GLM/Qwen3/MiniMax:
        // tools live in the system segment, before messages), so the gateway's
        // prefix ordering is at least consistent with vLLM's prompt layout.
        if let Some(tools) = &req.tools {
            if !tools.is_empty() {
                if let Ok(s) = serde_json::to_string(tools) {
                    bytes.extend_from_slice(s.as_bytes());
                }
            }
        }
        if let Ok(s) = serde_json::to_string(&req.messages) {
            bytes.extend_from_slice(s.as_bytes());
        }
        bytes
    }

    /// Prefix-serialize → trie query → select provider → DFX.
    /// Returns None when kvc is disabled or no provider matches; the handler
    /// falls back to the default routing path.
    ///
    /// Does NOT record — learning is deferred to the handler's Ok path via
    /// [`KvcOrchestrator::record_success`] (admission success is the "it
    /// fits" proof; Err prefixes must never learn).
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
        // Policy label is uniformly "kvc" (degraded sub-modes — gate lift,
        // empty prefix — are visible in the selection log, not the label).
        let schedule_policy = match self.router.policy_name().as_str() {
            "kvc_aware" => "kvc".to_string(),
            other => other.to_string(),
        };
        let kv_hit_blocks = selection.kv_hit_blocks;
        let kv_input_blocks = selection.kv_input_blocks;
        let trie_max_blocks = config.router_settings.kvc_aware.max_blocks.max(0) as i64;
        let trie_blocks = Some(kv_index.block_count() as i64);

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
            prefix_bytes,
        })
    }

    /// Record the routed prefix under the winning worker — call ONLY after
    /// the provider accepted the request (`chat`/`chat_stream` returned Ok).
    /// Admission success is the "it fits" proof; recording on the Err path
    /// would pin failed prefixes to the rejected worker — with add-only
    /// tries both sides then tie at hit_ratio 1.0 and round-robin keeps
    /// re-hitting the rejecting worker (tie trap, design doc §7).
    ///
    /// Best-effort and off the request path (semaphore-bounded
    /// spawn_blocking — the trie walk is synchronous CPU). No-op when
    /// `prefix_bytes` is empty (kvc disabled / single candidate) or the
    /// provider has no kv_worker_id.
    pub fn record_success(&self, model: &str, worker_id: Option<&str>, prefix_bytes: &[u8]) {
        // Flat guard: nothing to learn, or nowhere to attribute it.
        let (Some(worker_id), false) = (worker_id, prefix_bytes.is_empty()) else {
            return;
        };
        let kv_index = self.kv_index.clone();
        let model = model.to_string();
        let worker_id = worker_id.to_string();
        let prefix_bytes = prefix_bytes.to_vec();
        // try_acquire: if too many record tasks are in flight, drop this
        // one (best-effort — a missed record just means one extra cold
        // request later, not a correctness issue).
        if let Ok(permit) = self.record_semaphore.clone().try_acquire_owned() {
            // spawn_blocking, NOT tokio::spawn: record_request_prefix is
            // synchronous CPU work. On an async worker it occupies a
            // runtime thread without ever yielding, and enough concurrent
            // records starve the reactor (observed as DB-pool timeouts and
            // ~19x latency inflation under load). The blocking pool keeps
            // async workers free; the semaphore still bounds total tasks.
            tokio::task::spawn_blocking(move || {
                let _permit = permit; // held until record completes
                Self::record_prefix_now(&kv_index, &model, &worker_id, &prefix_bytes);
            });
        }
    }

    /// Synchronous record core, split out so unit tests drive it directly
    /// without the async semaphore/bounding. Flat: load → clone → record.
    fn record_prefix_now(
        kv_index: &Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
        model: &str,
        worker_id: &str,
        prefix_bytes: &[u8],
    ) {
        let g = kv_index.load();
        let idx = match (**g).as_ref() {
            Some(i) => i.clone(),
            None => return,
        };
        drop(g);
        idx.record_request_prefix(model, worker_id, prefix_bytes, StorageTier::Gpu);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boom_core::kv_event::KvMatchResult;
    use std::sync::Mutex;

    /// Shared record sink — the test keeps a clone, the fake index pushes
    /// into it. No downcast needed (KvIndexBackend has no as_any).
    #[derive(Clone, Default)]
    struct RecordSink(std::sync::Arc<Mutex<Vec<(String, String, Vec<u8>)>>>);

    struct RecordingIndex {
        sink: RecordSink,
    }

    impl KvIndexBackend for RecordingIndex {
        fn apply_event(&self, _event: &boom_core::kv_event::GatewayKvEvent) {}
        fn find_matches(
            &self,
            _model: &str,
            _prefix_bytes: &[u8],
            candidate_worker_ids: &[String],
        ) -> Vec<KvMatchResult> {
            candidate_worker_ids
                .iter()
                .map(|w| KvMatchResult {
                    worker_id: w.clone(),
                    match_depth: 0,
                    total_blocks: 0,
                    hit_ratio: 0.0,
                    load_score: 0.0,
                    combined_score: 0.0,
                })
                .collect()
        }
        fn remove_worker(&self, _worker_id: &str) {}
        fn block_count(&self) -> usize { 0 }
        fn model_names(&self) -> std::collections::HashSet<String> {
            std::collections::HashSet::new()
        }
        fn debug_dump(
            &self,
        ) -> Vec<(String, u64, Vec<String>, boom_core::kv_event::StorageTier, u64)> {
            Vec::new()
        }
        fn record_request_prefix(
            &self,
            model: &str,
            worker_id: &str,
            prefix_bytes: &[u8],
            _storage_tier: StorageTier,
        ) {
            // Mirrors the real trie's guard (token_prefix.rs): empty prefix
            // records nothing. Combined with record_success's own let-else
            // guard, empty prefixes never learn on any path.
            if prefix_bytes.is_empty() {
                return;
            }
            self.sink
                .0
                .lock()
                .unwrap()
                .push((model.to_string(), worker_id.to_string(), prefix_bytes.to_vec()));
        }
    }

    fn handle_with(sink: &RecordSink) -> Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>> {
        Arc::new(ArcSwap::from_pointee(Some(
            Arc::new(RecordingIndex { sink: sink.clone() }) as Arc<dyn KvIndexBackend>
        )))
    }

    #[test]
    fn record_prefix_now_records_model_worker_prefix() {
        let sink = RecordSink::default();
        let handle = handle_with(&sink);
        KvcOrchestrator::record_prefix_now(&handle, "m", "w1", b"abcdef");
        assert_eq!(
            sink.0.lock().unwrap().clone(),
            vec![("m".to_string(), "w1".to_string(), b"abcdef".to_vec())]
        );
    }

    #[test]
    fn record_prefix_now_noop_when_index_absent() {
        // Empty trie slot (kvc not initialized): must not panic, no record.
        let handle: Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>> =
            Arc::new(ArcSwap::from_pointee(None));
        KvcOrchestrator::record_prefix_now(&handle, "m", "w1", b"abcdef");
    }

    #[test]
    fn record_prefix_now_noop_on_empty_prefix() {
        let sink = RecordSink::default();
        let handle = handle_with(&sink);
        KvcOrchestrator::record_prefix_now(&handle, "m", "w1", b"");
        assert!(sink.0.lock().unwrap().is_empty(), "empty prefix must not record");
    }

    // ════════════════════════════════════════════════════════════════
    // PR2 回归闭环：真实 trie + 真实 kvc_aware 策略 + 内存 Router。
    // 钉死三条不变量（docs/context-rollback-design.md §7）：
    //   1. route() 不学习（路由时记录已移除）
    //   2. provider Ok 后 record_success 学习 → 下一同前缀请求亲和命中
    //   3. 未记录（= 失败）的前缀保持冷（失败不学习）
    // ════════════════════════════════════════════════════════════════

    /// Identity-only provider：策略只读 deployment_id / kv_worker_id。
    struct IdentityProvider {
        dep: &'static str,
        worker: &'static str,
    }

    #[async_trait::async_trait]
    impl Provider for IdentityProvider {
        async fn chat(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<boom_core::types::ChatCompletionResponse, boom_core::GatewayError> {
            unimplemented!("closed-loop tests only inspect identity/routing")
        }
        async fn chat_stream(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<boom_core::types::ChatStream, boom_core::GatewayError> {
            unimplemented!()
        }
        fn name(&self) -> &str { "fake" }
        fn models(&self) -> &[String] { &[] }
        fn deployment_id(&self) -> Option<&str> { Some(self.dep) }
        fn kv_worker_id(&self) -> Option<&str> { Some(self.worker) }
    }

    /// 真实组件 rig：TokenPrefixIndex（512B 块）+ 内存 DeploymentStore（2 实例）
    /// + KvcAwarePolicy（rebalance 默认 100=禁用，无负载干扰）+ Router。
    fn closed_loop_rig() -> (
        KvcOrchestrator,
        Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
        Arc<Router>,
    ) {
        let index: Arc<dyn KvIndexBackend> =
            Arc::new(boom_kvindex::TokenPrefixIndex::new(512, 1024));
        let handle = Arc::new(ArcSwap::from_pointee(Some(index.clone())));

        let store = Arc::new(boom_routing::DeploymentStore::new());
        store.add_deployment(
            "m",
            Arc::new(IdentityProvider { dep: "d1", worker: "w1" }) as Arc<dyn Provider>,
        );
        store.add_deployment(
            "m",
            Arc::new(IdentityProvider { dep: "d2", worker: "w2" }) as Arc<dyn Provider>,
        );

        let policy = boom_routing::KvcAwarePolicy::new(
            index,
            Arc::new(boom_routing::InFlightTracker::new()),
            None,
            None,
        );
        let router = Arc::new(Router::new(
            store,
            Arc::new(boom_routing::AliasStore::new()),
            Arc::new(policy),
        ));
        (KvcOrchestrator::new(handle.clone(), router.clone()), handle, router)
    }

    fn kvc_config() -> Config {
        serde_yaml::from_str(
            "server:\n  port: 4000\nrouter_settings:\n  schedule_policy: kvc_aware\n",
        )
        .unwrap()
    }

    /// >512B 的单消息请求（≥1 个 trie 块），marker 进前缀内容。
    fn req_with_marker(marker: &str) -> ChatCompletionRequest {
        serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": format!("{}-{}", marker, "x".repeat(1024))}
            ]
        }))
        .unwrap()
    }

    async fn route_once(
        orch: &KvcOrchestrator,
        router: &Router,
        req: &ChatCompletionRequest,
    ) -> RouteResult {
        let cands = router.candidates_for("m").expect("candidates");
        orch.route(&kvc_config(), "m", "k1", 100, req, &cands)
            .await
            .expect("route returns a selection")
    }

    /// Poll until the trie holds >= min blocks (record_success is
    /// spawn_blocking — bounded wait, no flaky sleep-only assertion).
    async fn wait_for_blocks(
        handle: &Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
        min: usize,
    ) -> bool {
        for _ in 0..400 {
            let n = (**handle.load())
                .as_ref()
                .map(|i| i.block_count())
                .unwrap_or(0);
            if n >= min {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        false
    }

    #[tokio::test]
    async fn route_does_not_record_the_trie() {
        // 不变量 1：route() 单独执行绝不写 trie（路由时学习已移除）。
        // 回归方向：若有人把记录搬回 route()，此测试立刻失败。
        let (orch, handle, router) = closed_loop_rig();
        let req = req_with_marker("alpha");
        let r1 = route_once(&orch, &router, &req).await;
        assert!(!r1.prefix_bytes.is_empty(), "prefix must be computed");
        let r2 = route_once(&orch, &router, &req).await;
        assert_eq!(r2.kv_hit_blocks, Some(0), "second route must stay cold");
        let blocks = (**handle.load()).as_ref().map(|i| i.block_count()).unwrap_or(0);
        assert_eq!(blocks, 0, "route() must NOT record (route-time learning removed)");
    }

    #[tokio::test]
    async fn record_success_learns_and_next_request_affinitizes() {
        // 不变量 2：provider Ok 后的 record_success 学习 → 下一同前缀请求
        // 命中并回到同一 worker（真实 trie + 真实策略的亲和闭环）。
        let (orch, handle, router) = closed_loop_rig();
        let req = req_with_marker("alpha");

        let r1 = route_once(&orch, &router, &req).await;
        // 模拟 handler 的 Ok 分支（routes.rs 六处接线的等价调用）：
        orch.record_success("m", r1.provider.kv_worker_id(), &r1.prefix_bytes);
        assert!(
            wait_for_blocks(&handle, 1).await,
            "record_success must land in the trie"
        );

        let r2 = route_once(&orch, &router, &req).await;
        assert!(
            r2.kv_hit_blocks.unwrap_or(0) > 0,
            "second same-prefix request must hit the learned trie"
        );
        assert_eq!(
            r2.provider.kv_worker_id(),
            r1.provider.kv_worker_id(),
            "affinity must return the recorded worker"
        );
    }

    #[tokio::test]
    async fn unrecorded_prefix_stays_cold() {
        // 不变量 3：没有 record_success（= provider Err / 失败路径）的前缀
        // 保持冷——失败不学习，修并列陷阱的单元级钉子。
        let (orch, handle, router) = closed_loop_rig();
        let req = req_with_marker("beta");

        let r1 = route_once(&orch, &router, &req).await;
        // 不调 record_success（模拟上游 400/Err）。
        let _ = r1;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let r2 = route_once(&orch, &router, &req).await;
        assert_eq!(
            r2.kv_hit_blocks,
            Some(0),
            "unrecorded (failed) prefix must stay cold — no tie, no re-hit"
        );
        let blocks = (**handle.load()).as_ref().map(|i| i.block_count()).unwrap_or(0);
        assert_eq!(blocks, 0);
    }
}
