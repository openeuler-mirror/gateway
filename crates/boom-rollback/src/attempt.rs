//! Single retry orchestration: guard swap, verbatim re-call, outcome
//! packaging. The only submodule that takes `&AppState` (for flow control
//! and the observability counters).

use super::decision::{select_fallback, FallbackDecision, RollbackRoute};

use boom_core::context_error::RollbackStats;

/// Everything the retry orchestration needs from the calling context.
/// Replaces a direct `&AppState` dependency so this crate stands alone —
/// boom-main constructs it from its state, tests construct it from fakes.
pub struct RollbackCtx<'a> {
    /// Router for the fallback's second select (full policy pipeline).
    pub router: &'a boom_routing::Router,
    /// Flow controller for the guard swap (release old slot, acquire new).
    pub flow_controller: &'a boom_flowcontrol::FlowController,
    /// Observability counters (identify/retry/guard/passthrough).
    pub stats: &'a boom_core::context_error::RollbackStats,
    /// Identification patterns (`router_settings.rollback.context.patterns`).
    pub patterns: &'a [String],
    /// Flow-control queue timeout in seconds.
    pub fc_queue_timeout_secs: u64,
}

use boom_core::error::GatewayError;
use boom_core::provider::{Provider, ProviderCallContext};
use boom_core::types::{ChatCompletionRequest, ChatCompletionResponse, ChatStream};
use std::sync::Arc;

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

/// Release the failed deployment's slot, acquire one for the fallback.
/// A guard-acquisition failure becomes the retry's error (the handler's
/// uniform error path reports it). FlowControlError → GatewayError mirrors
/// acquire_fc_guard's mapping, minus the queue-wait DFX (the error payload
/// still carries deployment_id + waiter count).
async fn swap_fc_guard(
    ctx: &RollbackCtx<'_>,
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
        ctx.fc_queue_timeout_secs,
    );
    match ctx
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
    ctx: &RollbackCtx<'_>,
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
    let patterns = ctx.patterns.to_vec();
    let decision = select_fallback(first_error, &patterns, route, &ctx.router, &failed_provider);
    // Composite event methods: each bumps its counters + per-model entry +
    // error code derived INTERNALLY from the error (error_code_of) — no
    // caller-side extraction, no hand-crafted codes, one request → one
    // identical code string in every tree section.
    match &decision {
        FallbackDecision::Target(_) => ctx.stats.record_selected(route.model),
        FallbackDecision::MissIdentify => {
            ctx.stats.record_miss_identified(route.model, first_error)
        }
        FallbackDecision::PoolInert => {
            ctx.stats.record_pool_inert_identified(route.model, first_error)
        }
        FallbackDecision::NoTarget => {
            ctx.stats.record_no_target_identified(route.model, first_error)
        }
    }
    let Some(target) = decision.target()
    else {
        return RollbackOutcome::NoRetry;
    };
    let new_id = target.deployment_id().map(str::to_string);
    let (new_guard, guard_err) = swap_fc_guard(
        ctx,
        fc_guard,
        new_id.as_deref().unwrap_or(""),
        route.input_chars,
        is_vip,
        key_alias.clone(),
        route.key_hash,
        route.model,
    )
    .await;
    if let Some(ref ge) = guard_err.as_ref().err() {
        ctx.stats.record_guard_error(route.model, ge);
    }
    let retry_start = std::time::Instant::now();
    let guard_failed = guard_err.is_err();
    let result = match guard_err {
        Err(e) => Err(e),
        Ok(()) => match call_ctx {
            Some(ctx) => target.chat_stream_with_context(req, ctx).await,
            None => target.chat_stream(req).await,
        },
    };
    match &result {
        Ok(_) => {
            let ms = retry_start.elapsed().as_millis() as u64;
            // `first_error` = the ORIGINAL context error the retry FIXED —
            // its code feeds the retry-success tree's sub-rows ("成功修复
            // 了什么错误、各多少次").
            ctx.stats.record_retry_success_fixed(route.model, first_error, ms);
        }
        // Guard-swap failure is already counted as guard_error (terminal:
        // no retry call was made) — bumping retry_error here too would
        // double-count and break invariant ② selected = success +
        // retry_error + guard_error.
        Err(ref e) if !guard_failed => {
            ctx.stats.record_retry_error(route.model, e);
        }
        Err(_) => {}
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
    ctx: &RollbackCtx<'_>,
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
    let patterns = ctx.patterns.to_vec();
    let decision = select_fallback(first_error, &patterns, route, &ctx.router, &failed_provider);
    // Composite event methods: each bumps its counters + per-model entry +
    // error code derived INTERNALLY from the error (error_code_of) — no
    // caller-side extraction, no hand-crafted codes, one request → one
    // identical code string in every tree section.
    match &decision {
        FallbackDecision::Target(_) => ctx.stats.record_selected(route.model),
        FallbackDecision::MissIdentify => {
            ctx.stats.record_miss_identified(route.model, first_error)
        }
        FallbackDecision::PoolInert => {
            ctx.stats.record_pool_inert_identified(route.model, first_error)
        }
        FallbackDecision::NoTarget => {
            ctx.stats.record_no_target_identified(route.model, first_error)
        }
    }
    let Some(target) = decision.target()
    else {
        return RollbackOutcome::NoRetry;
    };
    let new_id = target.deployment_id().map(str::to_string);
    let (new_guard, guard_err) = swap_fc_guard(
        ctx,
        fc_guard,
        new_id.as_deref().unwrap_or(""),
        route.input_chars,
        is_vip,
        key_alias.clone(),
        route.key_hash,
        route.model,
    )
    .await;
    if let Some(ref ge) = guard_err.as_ref().err() {
        ctx.stats.record_guard_error(route.model, ge);
    }
    let retry_start = std::time::Instant::now();
    let guard_failed = guard_err.is_err();
    let result = match guard_err {
        Err(e) => Err(e),
        Ok(()) => match call_ctx {
            Some(ctx) => target.chat_with_context(req, ctx).await,
            None => target.chat(req).await,
        },
    };
    match &result {
        Ok(_) => {
            let ms = retry_start.elapsed().as_millis() as u64;
            // `first_error` = the ORIGINAL context error the retry FIXED —
            // its code feeds the retry-success tree's sub-rows ("成功修复
            // 了什么错误、各多少次").
            ctx.stats.record_retry_success_fixed(route.model, first_error, ms);
        }
        // Guard-swap failure is already counted as guard_error (terminal:
        // no retry call was made) — bumping retry_error here too would
        // double-count and break invariant ② selected = success +
        // retry_error + guard_error.
        Err(ref e) if !guard_failed => {
            ctx.stats.record_retry_error(route.model, e);
        }
        Err(_) => {}
    }
    RollbackOutcome::Retried {
        result,
        provider: target,
        deployment_id: new_id,
        fc_guard: new_guard,
        from_deployment_id: failed_deployment_id,
    }
}

