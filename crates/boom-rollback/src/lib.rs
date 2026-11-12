//! Context rollback — standalone crate (design doc §6).
//!
//! Submodules:
//! - [`decision`]: L1+L2 gating (identify → pool gates → tier selection →
//!   second select through the router). Pure — no side effects.
//! - [`attempt`]: single retry orchestration (guard swap, verbatim re-call,
//!   outcome packaging). Takes a [`attempt::RollbackCtx`] for router,
//!   flow control, stats, and patterns — no boom-main dependency.
//! - [`finalize`]: 502→400 passthrough conversion.
//!
//! The L1 identifier and the `RollbackStats` counters live in
//! `boom-core/src/context_error.rs` — the leaf crate, so boom-dashboard
//! can read them without depending on this crate or boom-main. The handler
//! glue (rb_fail! macros, setup, bookkeeping) stays in boom-main's
//! `routes.rs` where it has access to handler-local state.
//!
//! Retry semantics (design doc §6 / §12 #4): strictly single-shot,
//! sequential, no recursion. The retry re-sends the request VERBATIM.
//! Client-transparent (admission errors precede any SSE byte).

pub mod attempt;
pub mod decision;
pub mod finalize;

pub use attempt::{attempt_call, attempt_stream, RollbackCtx, RollbackOutcome};
pub use decision::{capable, select_fallback, FallbackDecision, RollbackRoute};
pub use finalize::finalize_error;

// Re-export for callers that reference the stats via this crate.
pub use boom_core::context_error::RollbackStats;

#[cfg(test)]
mod tests {
    use super::*;
    use boom_core::error::GatewayError;
    use boom_core::provider::Provider;
    use boom_core::types::{ChatCompletionRequest, ChatCompletionResponse, ChatStream};
    use boom_routing::Router;
    use std::sync::Arc;
    use async_trait::async_trait;

    /// Identity-only provider with rollback surface fields.
    struct FakeProvider {
        dep: &'static str,
        max_ctx: Option<u64>,
        rb: bool,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn chat(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<ChatCompletionResponse, GatewayError> {
            unimplemented!("rollback tests only inspect identity/selection")
        }
        async fn chat_stream(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<ChatStream, GatewayError> {
            unimplemented!()
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn models(&self) -> &[String] {
            &[]
        }
        fn deployment_id(&self) -> Option<&str> {
            Some(self.dep)
        }
        fn max_context_tokens(&self) -> Option<u64> {
            self.max_ctx
        }
        fn rollback_enabled(&self) -> bool {
            self.rb
        }
    }

    fn pool() -> Vec<Arc<dyn Provider>> {
        // 2×200K + 1×500K + 2×1M, all rollback-enabled.
        vec![
            Arc::new(FakeProvider { dep: "d200a", max_ctx: Some(200_000), rb: true }),
            Arc::new(FakeProvider { dep: "d200b", max_ctx: Some(200_000), rb: true }),
            Arc::new(FakeProvider { dep: "d500", max_ctx: Some(500_000), rb: true }),
            Arc::new(FakeProvider { dep: "d1ma", max_ctx: Some(1_000_000), rb: true }),
            Arc::new(FakeProvider { dep: "d1mb", max_ctx: Some(1_000_000), rb: true }),
        ]
    }

    fn patterns() -> Vec<String> {
        boom_core::context_error::DEFAULT_CONTEXT_ERROR_PATTERNS
            .iter()
            .map(|p| p.to_string())
            .collect()
    }

    fn ctx_error(requested: Option<u64>) -> GatewayError {
        let msg = match requested {
            Some(r) => format!(
                "{{\"error\":{{\"code\":\"InputTokenExceeded\",\"message\":\"This model's maximum input length is 200000 tokens. However, you requested {} tokens.\"}}}}",
                r
            ),
            None => r#"{"error":{"code":"InputTokenExceeded","message":"too long"}}"#.to_string(),
        };
        GatewayError::UpstreamError { status: 400, message: msg }
    }

    fn route<'a>(candidates: &'a [Arc<dyn Provider>]) -> RollbackRoute<'a> {
        RollbackRoute {
            model: "m",
            key_hash: Some("k1"),
            input_chars: 100,
            prefix_bytes: b"",
            candidates,
        }
    }

    fn router() -> Arc<Router> {
        let store = Arc::new(boom_routing::DeploymentStore::new());
        for p in pool() {
            store.add_deployment("m", p);
        }
        Arc::new(Router::new(
            store,
            Arc::new(boom_routing::AliasStore::new()),
            Arc::new(boom_routing::RoundRobinPolicy::new()),
        ))
    }

    fn max_of(p: &Arc<dyn Provider>) -> u64 {
        p.max_context_tokens().unwrap()
    }

    #[test]
    fn select_fallback_picks_smallest_satisfying_tier() {
        // requested 250K → 500K tier（不是 1M）
        let cands = pool();
        let failed = cands[0].clone(); // 200K
        let r = router();
        let target = select_fallback(&ctx_error(Some(250_000)), &patterns(), &route(&cands), &r, &failed)
            .target().expect("fallback exists");
        assert_eq!(max_of(&target), 500_000);
        assert_ne!(target.deployment_id(), Some("d200a"));
    }

    #[test]
    fn select_fallback_requested_exceeds_all_uses_largest() {
        let cands = pool();
        let failed = cands[0].clone();
        let r = router();
        let target = select_fallback(&ctx_error(Some(2_000_000)), &patterns(), &route(&cands), &r, &failed)
            .target().expect("fallback exists");
        assert_eq!(max_of(&target), 1_000_000);
    }

    #[test]
    fn select_fallback_unparsed_requested_uses_largest() {
        let cands = pool();
        let failed = cands[0].clone();
        let r = router();
        let target = select_fallback(&ctx_error(None), &patterns(), &route(&cands), &r, &failed)
            .target().expect("fallback exists");
        assert_eq!(max_of(&target), 1_000_000);
    }

    #[test]
    fn select_fallback_failed_on_largest_returns_none() {
        // 失败实例已在最大层 → 无更大层 → NoRetry
        let cands = pool();
        let failed = cands[3].clone(); // 1M
        let r = router();
        assert!(
            select_fallback(&ctx_error(Some(2_000_000)), &patterns(), &route(&cands), &r, &failed)
                .target()
                .is_none()
        );
    }

    #[test]
    fn select_fallback_gates_on_pool_state() {
        let r = router();
        // ① 池里有实例未配 max_context → mixed → NoRetry
        let mut mixed = pool();
        mixed.push(Arc::new(FakeProvider { dep: "dx", max_ctx: None, rb: true }));
        assert!(select_fallback(&ctx_error(Some(250_000)), &patterns(), &route(&mixed), &r, &mixed[0]).target().is_none());
        // ② 有实例未开 rollback → NoRetry
        let mut off = pool();
        off.push(Arc::new(FakeProvider { dep: "dy", max_ctx: Some(300_000), rb: false }));
        assert!(select_fallback(&ctx_error(Some(250_000)), &patterns(), &route(&off), &r, &off[0]).target().is_none());
        // ③ 非上下文错误 → NoRetry
        let cands = pool();
        let not_ctx = GatewayError::UpstreamError { status: 400, message: r#"{"error":{"code":"BadModel"}}"#.into() };
        assert!(select_fallback(&not_ctx, &patterns(), &route(&cands), &r, &cands[0]).target().is_none());
        // ④ 非 400 → NoRetry
        let e401 = GatewayError::UpstreamError { status: 401, message: "nope".into() };
        assert!(select_fallback(&e401, &patterns(), &route(&cands), &r, &cands[0]).target().is_none());
        // ⑤ 空模式表 → NoRetry
        assert!(select_fallback(&ctx_error(Some(250_000)), &[], &route(&cands), &r, &cands[0]).target().is_none());
    }

    #[test]
    fn select_fallback_excludes_failed_provider_from_target_tier() {
        // 失败实例在 500K（唯一实例）→ 目标层若是 500K 则无候选。
        // 构造：requested 200K？失败在 200K、目标 500K 唯一实例 d500 —— 排除规则
        // 只排除失败实例本身；d500 不是失败实例，仍可选。
        let cands = pool();
        let failed = cands[0].clone(); // d200a
        let r = router();
        let target = select_fallback(&ctx_error(Some(500_000)), &patterns(), &route(&cands), &r, &failed)
            .target()
            .expect("500K tier reachable");
        assert_eq!(target.deployment_id(), Some("d500"));
        // 失败实例 = d500 本身（同层重试场景：requested 解析偏小）→ 排除后
        // 500K 层空 → 升到 1M
        let target2 = select_fallback(&ctx_error(Some(500_000)), &patterns(), &route(&cands), &r, &cands[2])
            .target()
            .expect("fallback skips the failed 500K instance");
        assert_eq!(max_of(&target2), 1_000_000);
    }

    #[test]
    fn capable_mirrors_gates() {
        assert!(capable(&patterns(), &pool()));
        // 混配
        let mut mixed = pool();
        mixed.push(Arc::new(FakeProvider { dep: "dx", max_ctx: None, rb: true }));
        assert!(!capable(&patterns(), &mixed));
        // 单层
        let single = vec![pool()[0].clone(), pool()[1].clone()];
        assert!(!capable(&patterns(), &single));
        // 空 patterns / 单候选
        assert!(!capable(&[], &pool()));
        assert!(!capable(&patterns(), &pool()[..1]));
    }

    #[test]
    fn finalize_error_converts_and_preserves() {
        let e = ctx_error(Some(250_000));
        let converted = finalize_error(e, &patterns(), None);
        match converted {
            GatewayError::ContextLengthExceeded(ref msg) => {
                assert!(msg.contains("InputTokenExceeded"));
            }
            other => panic!("expected ContextLengthExceeded, got {:?}", other),
        }
        assert_eq!(converted.status_code(), 400);
        // 特性关闭（空 patterns）：原样保留 502 语义
        let e2 = ctx_error(Some(250_000));
        let kept = finalize_error(e2, &[], None);
        assert_eq!(kept.status_code(), 502);
        // 非上下文错误：原样
        let e3 = GatewayError::UpstreamError { status: 400, message: "other".into() };
        assert_eq!(finalize_error(e3, &patterns(), None).status_code(), 502);
    }
}
