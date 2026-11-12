//! Context rollback execution chain — rollback L3 (design doc §6).
//!
//! Standalone module (mirrors the kvc.rs orchestrator pattern): everything
//! rollback-related that runs AFTER a provider call fails lives here —
//! identification (L1 via `GatewayError::context_error_match`), pool/tier
//! gating (L2), the single retry with the ORIGINAL request verbatim, the
//! flow-control guard swap, and the 502→400 passthrough conversion. The
//! handlers (routes.rs, 6 call sites) glue this into their Err arms.
//!
//! Retry semantics (design doc §6 / §12 #4):
//! - STRICTLY single-shot, sequential — no loops, no recursion anywhere in
//!   this module; stack depth is constant by construction.
//! - The retry re-sends the request VERBATIM (the caller keeps a clone when
//!   [`capable`] says a rollback is possible): the context error is in the
//!   RESPONSE, not the request — any truncation would break agent chains.
//! - Flow control: the failed deployment's slot is released before the
//!   retry, re-acquired for the fallback deployment; a guard-acquisition
//!   failure surfaces as the retry's error (uniform handler error path).
//! - Client transparency: context errors happen at upstream admission,
//!   before any SSE byte — the retry is invisible to the client, and
//!   plan_charge (committed only after provider success) is never doubled.
//!
//! Gating (L2, design doc §5): a model group participates when EVERY
//! online candidate has `rollback_enabled` (pool toggle) AND
//! `max_context_tokens` configured (all-or-nothing — mixed groups are
//! inert, worst case = today's behavior). Target tier selection: smallest
//! tier with `max_context >= requested_tokens` when the L1 match parsed
//! the requested count, else the largest tier; always strictly larger than
//! the FAILED deployment's tier, and the failed deployment itself is
//! excluded. The second select runs the FULL policy pipeline
//! (rr/key-affinity/kvc + rebalance) on the tier's candidates — same
//! semantics as normal routing, confined to the tier.

use crate::state::AppState;
pub use boom_core::context_error::RollbackStats;
use boom_core::error::GatewayError;
use boom_core::provider::{Provider, ProviderCallContext};
use boom_core::types::{ChatCompletionRequest, ChatCompletionResponse, ChatStream};
use boom_routing::Router;
use std::sync::Arc;


/// Routing context for the fallback's second select — everything the
/// normal first select used, so the retry rides the identical policy
/// pipeline (just on a tier-confined candidate list).
pub struct RollbackRoute<'a> {
    pub model: &'a str,
    pub key_hash: Option<&'a str>,
    pub input_chars: u64,
    pub prefix_bytes: &'a [u8],
    /// Full online candidate pool for the model (the same list the first
    /// select used).
    pub candidates: &'a [Arc<dyn Provider>],
}

/// Result of a rollback attempt. `NoRetry` leaves everything untouched —
/// the handler runs its normal error path with the original error.
pub enum RollbackOutcome<T> {
    NoRetry,
    /// The retry was attempted on a fallback-tier deployment. `result` is
    /// the SECOND call's outcome; the identities are what the handler
    /// should use from here on (audit, trace, inflight, Ok-continuation).
    Retried {
        result: Result<T, GatewayError>,
        provider: Arc<dyn Provider>,
        deployment_id: Option<String>,
        /// Re-acquired flow-control guard for the fallback deployment
        /// (the failed one's slot was released).
        fc_guard: Option<boom_flowcontrol::FlowControlGuard>,
        /// The deployment the request originally failed on (audit/trace
        /// marker: `from -> to`).
        from_deployment_id: Option<String>,
    },
}

/// Cheap pre-check: can a rollback possibly happen for this pool? Used by
/// the handlers to decide whether to keep a request clone for the retry
/// (the only per-request cost of the feature when it can't fire).
/// Everything is re-checked by [`select_fallback`] after the actual error
/// arrives — this is purely the clone-cost gate.
pub fn capable(patterns: &[String], candidates: &[Arc<dyn Provider>]) -> bool {
    if patterns.is_empty() || candidates.len() < 2 {
        return false;
    }
    let mut tiers = std::collections::BTreeSet::new();
    for c in candidates {
        if !c.rollback_enabled() {
            return false;
        }
        match c.max_context_tokens() {
            Some(v) => {
                tiers.insert(v);
            }
            None => return false, // mixed pool → inert (all-or-nothing gate)
        }
    }
    tiers.len() >= 2
}

/// Decision outcome from [`select_fallback`] — every exit is a named
/// reason so the stats can count exactly why fallbacks do or don't fire.
pub enum FallbackDecision {
    /// A fallback target was selected; retry should proceed.
    Target(Arc<dyn Provider>),
    /// L1 miss: not an identified context error.
    MissIdentify,
    /// L2 gate: pool not participating (not enrolled / mixed / single tier).
    PoolInert,
    /// L2 gate: pool active but no larger tier online.
    NoTarget,
}

impl FallbackDecision {
    pub fn target(self) -> Option<Arc<dyn Provider>> {
        match self {
            Self::Target(p) => Some(p),
            _ => None,
        }
    }
}

/// L1 + L2 decision: identify the error, gate the pool, pick the target
/// tier, run the second select through the router (full policy pipeline).
/// Every exit is a named [`FallbackDecision`] reason. Flat: identify →
/// gates → tier → one select; no recursion.
pub fn select_fallback(
    error: &GatewayError,
    patterns: &[String],
    route: &RollbackRoute<'_>,
    router: &Router,
    failed_provider: &Arc<dyn Provider>,
) -> FallbackDecision {
    // ── L1: identification (status gate + code/message patterns). ──
    let m = error.context_error_match(patterns);
    if !m.matched {
        return FallbackDecision::MissIdentify;
    }

    // ── L2 gates: pool participation (all-or-nothing on both fields). ──
    let failed_max = failed_provider.max_context_tokens();
    let mut tiers = std::collections::BTreeSet::new();
    for c in route.candidates {
        if !c.rollback_enabled() {
            return FallbackDecision::PoolInert;
        }
        match c.max_context_tokens() {
            Some(v) => {
                tiers.insert(v);
            }
            None => return FallbackDecision::PoolInert,
        }
    }
    if tiers.len() < 2 {
        return FallbackDecision::PoolInert; // single tier: nowhere to fall back to
    }

    // ── Target tier: strictly larger than the failed tier; smallest tier
    // satisfying the parsed request size, else the largest. ──
    let larger: Vec<u64> = match failed_max {
        Some(fm) => tiers.iter().copied().filter(|&v| v > fm).collect(),
        // Failed provider not in the pool anymore (disabled mid-flight?):
        // treat every tier as eligible.
        None => tiers.iter().copied().collect(),
    };
    if larger.is_empty() {
        return FallbackDecision::NoTarget;
    }
    let target_tier = match m.requested_tokens {
        Some(r) => larger
            .iter()
            .copied()
            .find(|&v| v >= r) // smallest satisfying tier
            .unwrap_or(*larger.last().unwrap()), // exceeds all → largest anyway
        None => *larger.last().unwrap(), // unparsed → largest (design §5)
    };

    // ── Tier candidates, excluding the failed deployment itself. ──
    let tier_candidates: Vec<Arc<dyn Provider>> = route
        .candidates
        .iter()
        .filter(|c| c.max_context_tokens() == Some(target_tier))
        .filter(|c| !Arc::ptr_eq(c, failed_provider))
        .cloned()
        .collect();
    if tier_candidates.is_empty() {
        return FallbackDecision::NoTarget;
    }

    // ── Second select: the FULL policy pipeline on the tier's list. ──
    match router.select_with_candidates(
        route.model,
        &tier_candidates,
        route.key_hash,
        route.input_chars,
        route.prefix_bytes,
    ) {
        Some(selection) => FallbackDecision::Target(selection.provider),
        None => FallbackDecision::NoTarget,
    }
}

/// Release the failed deployment's slot, acquire one for the fallback.
/// A guard-acquisition failure becomes the retry's error (the handler's
/// uniform error path reports it). FlowControlError → GatewayError mirrors
/// acquire_fc_guard's mapping, minus the queue-wait DFX (the error payload
/// still carries deployment_id + waiter count).
async fn swap_fc_guard(
    state: &AppState,
    old_guard: Option<boom_flowcontrol::FlowControlGuard>,
    new_deployment_id: &str,
    input_chars: u64,
    is_vip: bool,
    key_alias: Option<String>,
    key_hash: Option<&str>,
    model: &str,
) -> (Option<boom_flowcontrol::FlowControlGuard>, Result<(), GatewayError>) {
    drop(old_guard); // release the failed deployment's slot first
    let timeout = std::time::Duration::from_secs(
        state.inner.load().config.router_settings.flow_control_queue_timeout_secs(),
    );
    match state
        .flow_controller
        .acquire(
            new_deployment_id,
            input_chars,
            timeout,
            is_vip,
            key_alias,
            key_hash.map(str::to_string),
            Some(model.to_string()),
        )
        .await
    {
        Ok(g) => (Some(g), Ok(())),
        Err(boom_flowcontrol::FlowControlError::Timeout { waiters, .. }) => (
            None,
            Err(GatewayError::FlowControlQueueTimeout {
                deployment_id: new_deployment_id.to_string(),
                waiters,
                message: format!(
                    "Deployment '{}' flow control queue timeout during rollback retry",
                    new_deployment_id
                ),
            }),
        ),
        // NoSlot mirrors acquire_fc_guard's semantics: the deployment has no
        // flow-control configuration (no max_inflight) → pass-through with no
        // guard, NOT an error. Treating it as one would wrongly reject every
        // rollback to an unconfigured deployment (e2e-caught).
        Err(boom_flowcontrol::FlowControlError::NoSlot) => (None, Ok(())),
        Err(boom_flowcontrol::FlowControlError::ContextExceeded { context_chars, max_context, .. }) => (
            None,
            Err(GatewayError::RateLimitExceeded {
                retry_after_secs: None,
                message: format!(
                    "Rollback retry context ({} chars) exceeds deployment max_context ({})",
                    context_chars, max_context
                ),
                limit_type: "flow_control_context",
                scope: None,
                scope_id: None,
                plan_name: None,
            }),
        ),
    }
}

/// Streaming retry: identify → gate → second select → guard swap → ONE
/// re-call of the ORIGINAL request on the fallback provider. The caller
/// passes the request it saved (clone) and the provider identities of the
/// failed attempt; on `Retried` the caller rebinds provider/deployment_id/
/// fc_guard to the returned values and treats `result` as the call outcome.
/// Flat: one decision, one guard swap, one provider call. No loops.
pub async fn attempt_stream(
    state: &AppState,
    first_error: &GatewayError,
    req: ChatCompletionRequest,
    call_ctx: Option<ProviderCallContext>,
    failed_provider: Arc<dyn Provider>,
    failed_deployment_id: Option<String>,
    fc_guard: Option<boom_flowcontrol::FlowControlGuard>,
    is_vip: bool,
    key_alias: Option<String>,
    route: &RollbackRoute<'_>,
) -> RollbackOutcome<ChatStream> {
    let patterns = state
        .inner
        .load()
        .config
        .router_settings
        .rollback
        .context
        .patterns
        .clone();
    let decision = select_fallback(first_error, &patterns, route, &state.router, &failed_provider);
    state.rollback_stats.record_identify_total();
    match &decision {
        FallbackDecision::Target(_) => state.rollback_stats.record_fallback_selected(),
        FallbackDecision::MissIdentify => state.rollback_stats.record_identify_miss(),
        FallbackDecision::PoolInert => state.rollback_stats.record_pool_inert(),
        FallbackDecision::NoTarget => state.rollback_stats.record_no_target(),
    }
    let Some(target) = decision.target()
    else {
        return RollbackOutcome::NoRetry;
    };
    let new_id = target.deployment_id().map(str::to_string);
    let (new_guard, guard_err) = swap_fc_guard(
        state,
        fc_guard,
        new_id.as_deref().unwrap_or(""),
        route.input_chars,
        is_vip,
        key_alias.clone(),
        route.key_hash,
        route.model,
    )
    .await;
    if guard_err.is_err() {
        state.rollback_stats.record_guard_error();
    }
    let result = match guard_err {
        Err(e) => Err(e),
        Ok(()) => match call_ctx {
            Some(ctx) => target.chat_stream_with_context(req, ctx).await,
            None => target.chat_stream(req).await,
        },
    };
    match &result {
        Ok(_) => state.rollback_stats.record_retry_success(),
        Err(_) => state.rollback_stats.record_retry_error(),
    }
    RollbackOutcome::Retried {
        result,
        provider: target,
        deployment_id: new_id,
        fc_guard: new_guard,
        from_deployment_id: failed_deployment_id,
    }
}

/// Non-streaming retry — same contract as [`attempt_stream`].
pub async fn attempt_call(
    state: &AppState,
    first_error: &GatewayError,
    req: ChatCompletionRequest,
    call_ctx: Option<ProviderCallContext>,
    failed_provider: Arc<dyn Provider>,
    failed_deployment_id: Option<String>,
    fc_guard: Option<boom_flowcontrol::FlowControlGuard>,
    is_vip: bool,
    key_alias: Option<String>,
    route: &RollbackRoute<'_>,
) -> RollbackOutcome<ChatCompletionResponse> {
    let patterns = state
        .inner
        .load()
        .config
        .router_settings
        .rollback
        .context
        .patterns
        .clone();
    let decision = select_fallback(first_error, &patterns, route, &state.router, &failed_provider);
    state.rollback_stats.record_identify_total();
    match &decision {
        FallbackDecision::Target(_) => state.rollback_stats.record_fallback_selected(),
        FallbackDecision::MissIdentify => state.rollback_stats.record_identify_miss(),
        FallbackDecision::PoolInert => state.rollback_stats.record_pool_inert(),
        FallbackDecision::NoTarget => state.rollback_stats.record_no_target(),
    }
    let Some(target) = decision.target()
    else {
        return RollbackOutcome::NoRetry;
    };
    let new_id = target.deployment_id().map(str::to_string);
    let (new_guard, guard_err) = swap_fc_guard(
        state,
        fc_guard,
        new_id.as_deref().unwrap_or(""),
        route.input_chars,
        is_vip,
        key_alias.clone(),
        route.key_hash,
        route.model,
    )
    .await;
    if guard_err.is_err() {
        state.rollback_stats.record_guard_error();
    }
    let result = match guard_err {
        Err(e) => Err(e),
        Ok(()) => match call_ctx {
            Some(ctx) => target.chat_with_context(req, ctx).await,
            None => target.chat(req).await,
        },
    };
    match &result {
        Ok(_) => state.rollback_stats.record_retry_success(),
        Err(_) => state.rollback_stats.record_retry_error(),
    }
    RollbackOutcome::Retried {
        result,
        provider: target,
        deployment_id: new_id,
        fc_guard: new_guard,
        from_deployment_id: failed_deployment_id,
    }
}

/// 502→400 passthrough (design §12 #8): when the rollback feature is on
/// and the final error is an identified context rejection, convert the
/// reply error so clients see the upstream's 400 semantics instead of the
/// misleading 502 (agents retry 5xx). The original error is preserved for
/// logging; only the reply carries the conversion.
pub fn finalize_error(
    e: GatewayError,
    patterns: &[String],
    stats: Option<&RollbackStats>,
) -> GatewayError {
    if patterns.is_empty() {
        return e;
    }
    if let GatewayError::UpstreamError { status, message } = &e {
        if boom_core::context_error::identify(*status, message, patterns).matched {
            if let Some(stats) = stats {
                stats.record_passthrough_400();
            }
            return GatewayError::ContextLengthExceeded(message.clone());
        }
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;
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
