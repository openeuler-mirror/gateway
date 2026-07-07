use crate::extractor::RequiredAuth;
use crate::request_log::{log_error, log_request, RequestLog};
use crate::state::AppState;
use axum::extract::{ConnectInfo, Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use boom_core::anthropic::{
    anthropic_request_to_openai, openai_response_to_anthropic, AnthropicStreamTranscoder,
};
use boom_core::types::*;
use boom_core::GatewayError;
use boom_ctxaware::AgentStatsTracker;
use boom_flowcontrol::{FlowControlError, FlowControlledStream};
use boom_limiter::{ConcurrencyGuard, GuardedStream, PlanStore, RateLimitPlan};
use boom_promptlog::{PromptLogEntry, PromptLogStream};
use boom_quota::{CumulativeKind, QuotaScope, QuotaStore};
use boom_routing::{InFlightGuard, ModelCostRate, Router};
use futures::StreamExt;
use sqlx::PgPool;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

/// Determine whether to request full KV-cache block reporting from vLLM.
///
/// vLLM defaults to incremental event reporting (only newly allocated blocks).
/// Full reporting asks vLLM to re-emit the request's currently cached prefix
/// blocks, backfilling the trie.
///
/// Decision logic (kvc-aware enabled):
///   - hit ratio ≥ threshold → false (the trie already covers the prefix vLLM
///     will reuse, so incremental reporting is sufficient — vLLM reuses those
///     blocks without re-reporting them, but they are already in the trie)
///   - hit ratio < threshold → true (the trie is missing part of the prefix;
///     request a full report to backfill, also self-healing future requests)
/// kvc-aware disabled (no kv_index) → false (vLLM default incremental).
fn need_full_kv_report(
    kvc_enabled: bool,
    kv_hit_ratio: f64,
    threshold: f64,
) -> bool {
    if !kvc_enabled {
        // kvc-aware disabled → vLLM default (incremental).
        return false;
    }
    // kvc-aware enabled: full report only when the trie doesn't yet
    // cover enough of the request's prefix.
    kv_hit_ratio < threshold
}

/// Extract client IP from request headers with TCP fallback.
/// Priority: X-Real-IP > X-Forwarded-For (first IP) > TCP remote addr > "unknown".
fn extract_client_ip(headers: &axum::http::HeaderMap, remote_addr: Option<std::net::SocketAddr>) -> String {
    // Try X-Real-IP first (set by nginx etc.)
    if let Some(val) = headers.get("X-Real-IP").and_then(|v| v.to_str().ok()) {
        let ip = val.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    // Try X-Forwarded-For (first IP in the list).
    if let Some(val) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok()) {
        if let Some(ip) = val.split(',').next() {
            let ip = ip.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }
    }
    // Fallback to TCP connection remote address.
    if let Some(addr) = remote_addr {
        return addr.ip().to_string();
    }
    "unknown".to_string()
}

/// Acquire flow control guard for a deployment.
/// Returns Ok(Some(guard)) if acquired, Ok(None) if no slot (pass-through),
/// or Err with appropriate error reply.
async fn acquire_fc_guard<E>(
    state: &AppState,
    deployment_id: &str,
    context_chars: u64,
    is_vip: bool,
    key_alias: Option<String>,
    fc_key_hash: Option<String>,
    fc_model: Option<String>,
    api_path: &str,
    identity: &boom_core::types::AuthIdentity,
    model: &str,
    is_stream: bool,
    start: Instant,
    request_id: &str,
    client_ip: Option<String>,
    err_wrap: impl Fn(GatewayError, bool) -> E,
) -> Result<Option<boom_flowcontrol::FlowControlGuard>, E> {
    let timeout = std::time::Duration::from_secs(1200);
    match state.flow_controller.acquire(deployment_id, context_chars, timeout, is_vip, key_alias, fc_key_hash, fc_model).await {
        Ok(g) => Ok(Some(g)),
        Err(FlowControlError::Timeout { waiters, .. }) => {
            let e = GatewayError::FlowControlQueueTimeout {
                deployment_id: deployment_id.to_string(),
                waiters,
                message: format!("Deployment '{}' flow control queue timeout — too many concurrent requests", deployment_id),
            };
            log_error(state, identity, model, api_path, is_stream, start, &e, Some(request_id.to_string()), Some(deployment_id.to_string()), None, client_ip);
            Err(err_wrap(e, is_stream))
        }
        Err(FlowControlError::NoSlot) => Ok(None),
        Err(FlowControlError::ContextExceeded { deployment_id: _, context_chars, max_context }) => {
            let e = GatewayError::RateLimitExceeded {
                retry_after_secs: None,
                message: format!("Request context ({} chars) exceeds deployment max_context limit ({} chars)", context_chars, max_context),
                limit_type: "flow_control_context",
                scope: None,
                scope_id: None,
                plan_name: None,
            };
            log_error(state, identity, model, api_path, is_stream, start, &e, Some(request_id.to_string()), Some(deployment_id.to_string()), None, client_ip);
            Err(err_wrap(e, is_stream))
        }
    }
}

fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ═══════════════════════════════════════════════════════════
// Debug logging — dump full request/response for specific keys
// ═══════════════════════════════════════════════════════════


// ============================================================
// InFlightStream — wraps a stream with an InFlightGuard so the
// guard is released when the stream is fully consumed or dropped.
// ============================================================

struct InFlightStream<S> {
    inner: S,
    _guard: InFlightGuard,
}

impl<S> InFlightStream<S> {
    fn new(inner: S, guard: InFlightGuard) -> Self {
        Self { inner, _guard: guard }
    }
}

impl<S: futures::Stream + Unpin> futures::Stream for InFlightStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

// ============================================================
// LoggedStream — delays log write until the stream is fully consumed
// ============================================================

type UsageTracker = std::sync::Arc<std::sync::Mutex<UsageTrackerState>>;

/// Accumulated usage from the upstream stream, read by LoggedStream on drop.
#[derive(Default, Clone)]
struct UsageTrackerState {
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
    /// Real KV-cache hit reported by vLLM in the final usage chunk
    /// (prompt_tokens_details.cached_tokens). None if the chunk didn't carry it.
    cached_tokens: Option<i32>,
}

/// Wrapper stream that writes the request log when dropped (i.e. when the
/// stream has been fully consumed or the connection is torn down).
/// This captures the *real* duration for streaming requests instead of
/// recording the time at which the stream *started*.
///
/// Also settles the `PlanCharge` (token + cost attribution) on drop, using
/// the final usage from `usage_tracker` once the stream has completed.
struct LoggedStream<S> {
    inner: S,
    pool: Option<PgPool>,
    log: Option<RequestLog>,
    start: Instant,
    first_token_at: Option<Instant>,
    usage: UsageTracker,
    agent_stats: Option<Arc<AgentStatsTracker>>,
    /// Plan charge to settle on drop. None when running without quota tracking
    /// (e.g. legacy per-model RPM path with no plan).
    plan_charge: Option<PlanCharge>,
}

impl<S> LoggedStream<S> {
    fn new(
        inner: S,
        pool: Option<PgPool>,
        log: RequestLog,
        start: Instant,
        usage: UsageTracker,
        agent_stats: Option<Arc<AgentStatsTracker>>,
    ) -> Self {
        Self {
            inner,
            pool,
            log: Some(log),
            start,
            first_token_at: None,
            usage,
            agent_stats,
            plan_charge: None,
        }
    }

    /// Attach a `PlanCharge` to be settled on drop. Caller MUST have already
    /// called `commit()` on the charge (so limiter counters are incremented)
    /// and `take_concurrency_guard()` (so the concurrency guard is attached
    /// to the inner GuardedStream). The remaining settle step (token + cost
    /// attribution) is performed here when the stream finishes.
    fn with_plan_charge(mut self, charge: PlanCharge) -> Self {
        self.plan_charge = Some(charge);
        self
    }
}

impl<S> Drop for LoggedStream<S> {
    fn drop(&mut self) {
        if let Some(mut log) = self.log.take() {
            log.duration_ms = Some(self.start.elapsed().as_millis() as i32);
            log.ttft_ms = self.first_token_at.map(|t| (t - self.start).as_millis() as i32);
            // Read accumulated usage from tracker (written by stream mapper).
            if let Ok(guard) = self.usage.lock() {
                log.input_tokens = guard.prompt_tokens;
                log.output_tokens = guard.completion_tokens;
                log.cached_tokens = guard.cached_tokens.map(|c| c as i64);
            }
            // Account tokens to agent stats before moving log into log_request.
            if let Some(tracker) = &self.agent_stats {
                let input = log.input_tokens.unwrap_or(0) as u64;
                let output = log.output_tokens.unwrap_or(0) as u64;
                if input > 0 || output > 0 {
                    tracker.record_tokens(&log.api_path, input, output);
                }
            }
            log_request(self.pool.clone(), log);
        }
        // Settle the plan charge with real token counts from the stream.
        // If the stream errored or was cancelled before the final usage
        // chunk arrived, input/output will be 0 — settle is a no-op on
        // cumulative counters (cost will be $0), which is the desired
        // "未拿到 usage 不计费" behavior.
        if let Some(mut charge) = self.plan_charge.take() {
            let (input, cached, output) = if let Ok(guard) = self.usage.lock() {
                (
                    guard.prompt_tokens.unwrap_or(0) as u64,
                    guard.cached_tokens.unwrap_or(0).max(0) as u64,
                    guard.completion_tokens.unwrap_or(0) as u64,
                )
            } else {
                (0u64, 0u64, 0u64)
            };
            charge.settle(input, cached, output);
        }
    }
}

impl<S: futures::Stream + Unpin> futures::Stream for LoggedStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = self.inner.poll_next_unpin(cx);
        if self.first_token_at.is_none() && result.is_ready() {
            self.first_token_at = Some(Instant::now());
        }
        result
    }
}

// ============================================================
// Chat Completions
// ============================================================

pub async fn chat_completions(
    State(state): State<AppState>,
    auth: RequiredAuth,
    headers: axum::http::HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<std::net::SocketAddr>,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<impl IntoResponse, GatewayErrorReply> {
    chat_completions_inner(state, auth, &headers, Some(remote_addr), req, "/v1/chat/completions").await
}

/// Shared inner logic for chat completions and legacy completions.
async fn chat_completions_inner(
    state: AppState,
    auth: RequiredAuth,
    headers: &axum::http::HeaderMap,
    remote_addr: Option<std::net::SocketAddr>,
    mut req: ChatCompletionRequest,
    api_path: &str,
) -> Result<impl IntoResponse, GatewayErrorReply> {
    let start = Instant::now();
    let request_id = new_request_id();
    let identity = auth.identity();
    let client_ip = extract_client_ip(headers, remote_addr);
    let inner = state.inner.load();
    let model = req.model.clone();
    let is_stream = req.stream.unwrap_or(false);

    tracing::info!(request_id = %request_id, model = %model, stream = is_stream, path = api_path, "chat_completions request started");

    // Prompt log: check early to avoid unnecessary cloning.
    let prompt_log_should = state.prompt_log_writer.should_capture(
        &identity.key_hash,
        identity.team_id.as_deref(),
    );
    let prompt_log_req_body = if prompt_log_should {
        serde_json::to_value(&req).ok()
    } else {
        None
    };
    let prompt_log_sender = if prompt_log_should {
        Some(state.prompt_log_writer.sender())
    } else {
        None
    };
    // Clone request_id and model for prompt log (they get moved into RequestLog later).
    let prompt_log_rid = if prompt_log_should { Some(request_id.clone()) } else { None };
    let prompt_log_model = if prompt_log_should { Some(model.clone()) } else { None };

    // 1. Model access check (deployment-aware, alias-aware).
    check_model_access(identity, &req.model, &state.router, &inner.config.general_settings.public_models)
        .map_err(|e| {
            log_error(&state, &identity, &model, api_path, is_stream, start, &e, Some(request_id.clone()), None, None, Some(client_ip.clone()));
            GatewayErrorReply(e, false)
        })?;

    // 1b. Content-aware model resolution (hybrid router).
    let resolved_model = state.router.resolve_request_model(
        &req.model, &req.messages, &req.tools,
    );

    // 2. Plan-based or default rate limiting.
    let window_limits: Vec<boom_core::types::WindowLimit> = inner
        .config
        .rate_limit
        .window_limits
        .iter()
        .filter_map(|w| {
            if w.len() >= 2 {
                Some(boom_core::types::WindowLimit {
                    counts: Some(w[0]),
                    tokens: None,
                    costs: None,
                    window_secs: w[1],
                })
            } else {
                None
            }
        })
        .collect();
    let weight = resolve_quota_weight(&req.model, &state);
    let resolved_model_for_quota = resolve_model_name(&req.model, &state);

    let mut plan_charge = check_plan_limits(
        &state,
        &state.plan_store,
        &state.limiter,
        &identity.key_hash,
        identity.team_id.as_deref(),
        &req.model,
        &resolved_model_for_quota,
        identity.rpm_limit,
        &window_limits,
        weight,
    )
    .await
    .map_err(|e| {
        log_error(&state, &identity, &model, api_path, is_stream, start, &e, Some(request_id.clone()), None, None, Some(client_ip.clone()));
        GatewayErrorReply(e, false)
    })?;

    let input_chars: usize = req.messages.iter().map(|m| match &m.content {
        boom_core::types::MessageContent::Text(t) => t.len(),
        boom_core::types::MessageContent::Parts(parts) => parts.iter().map(|p| match p {
            boom_core::types::ContentPart::Text { text } => text.len(),
            _ => 0,
        }).sum(),
        boom_core::types::MessageContent::Null => 0,
    }).sum();
    log_request_summary(
        &request_id, identity, &model, input_chars, is_stream,
        api_path, &plan_charge,
    );

    // 3. Select provider deployment.
    // Tokenize request if tokenizer pool is available (for KV-cache prefix matching).
    let token_ids = {
        let pool_guard = state.tokenizer_pool.load();
        match &**pool_guard {
            Some(pool) => {
                let msgs: Vec<serde_json::Value> = req.messages.iter().map(|m| serde_json::to_value(m).unwrap_or_default()).collect();
                let tools_vals: Option<Vec<serde_json::Value>> = req.tools.as_ref().map(|t| t.iter().map(|tool| serde_json::to_value(tool).unwrap_or_default()).collect());
                let result = pool.tokenize_openai(&resolved_model, &msgs, tools_vals.as_deref());
                tracing::debug!(model = %resolved_model, tokens = result.token_ids.len(), "request tokenized");
                result.token_ids
            }
            None => Vec::new(),
        }
    };

    let selection = state
        .router
        .select_provider_with_prefix(&resolved_model, Some(&identity.key_hash), input_chars as u64, &token_ids)
        .ok_or_else(|| {
            let e = GatewayError::ModelNotFound(resolved_model.clone());
            log_error(&state, &identity, &model, api_path, is_stream, start, &e, Some(request_id.clone()), None, None, Some(client_ip.clone()));
            GatewayErrorReply(e, false)
        })?;
    let provider = selection.provider;
    let kv_hit_ratio = selection.kv_hit_ratio;

    let deployment_id = provider.deployment_id().map(|s| s.to_string());
    let inflight_model = state.router.resolve_model_name(&resolved_model);

    // 3.2. Determine KV-cache reporting mode.
    //      Full reporting is needed when kvc-aware is enabled so the trie
    //      stays in sync with vLLM's internal prefix cache — vLLM reuses
    //      cached blocks without reporting them in incremental mode.
    //      When kvc-aware is disabled, vLLM uses its default incremental mode.
    req.kv_cache_report_full = need_full_kv_report(
        state.kv_index.load().is_some(),
        kv_hit_ratio,
        inner.config.router_settings.kvc_aware.full_report_hit_threshold,
    );

    // 3.5. Flow control — queue if per-deployment limits exceeded.
    let is_vip = is_vip_key(&identity.metadata);
    let fc_guard = if let Some(ref did) = deployment_id {
        acquire_fc_guard(
            &state, did, input_chars as u64, is_vip,
            identity.key_alias.clone(), Some(identity.key_hash.clone()),
            Some(inflight_model.clone()),
            api_path, &identity, &model,
            is_stream, start, &request_id, Some(client_ip.clone()), GatewayErrorReply,
        ).await?
    } else {
        None
    };

    // Attach gateway-internal headers (e.g. X-Gateway-Priority) when enabled.
    req.gateway_headers = build_gateway_headers(
        is_vip,
        inner.config.router_settings.enable_priority_header,
        api_path,
        provider.client_type_header(),
    );

    // Capture request body for debug recording if debug mode is enabled.
    let debug_req_body = if state.debug_store.is_enabled() {
        serde_json::to_string(&req).ok()
    } else {
        None
    };

    // 4. Route to provider (streaming or non-streaming).
    if is_stream {
        let stream = match provider.chat_stream(req).await {
            Ok(s) => s,
            Err(e) => {
                log_error(&state, &identity, &model, api_path, true, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone(), Some(client_ip.clone()));
                crate::health_monitor::record_request_failure(&state, &deployment_id, &e);
                // plan_charge drops here without commit — no quota consumed.
                return Err(GatewayErrorReply(e, true));
            }
        };
        // Provider accepted the request — commit the plan charge now.
        let _decision = plan_charge.commit();
        crate::health_monitor::reset_request_failure(&state, &deployment_id);
        let usage = UsageTracker::default();
        let sse_stream = sse_stream_from_chat_stream(stream, usage.clone());
        let inflight_guard = if let Some(ref did) = deployment_id {
            InFlightGuard::new_for_deployment(state.inflight.clone(), &inflight_model, did, input_chars as u64)
        } else {
            InFlightGuard::new(state.inflight.clone(), &inflight_model, input_chars as u64)
        };
        let tracked = InFlightStream::new(sse_stream, inflight_guard);
        // Wrap with flow control guard (held until stream ends).
        let flow_controlled = match fc_guard {
            Some(g) => FlowControlledStream::new(tracked, g),
            None => FlowControlledStream::passthrough(tracked),
        };
        let guarded = GuardedStream::new(flow_controlled, plan_charge.take_concurrency_guard());

        // Provider returned a stream — count as a successfully handled request.
        state.agent_stats.record(api_path);
        if let Some(ref did) = deployment_id {
            state.request_rate.record(did);
        }

        let api_path_owned = api_path.to_string();
        let logged = LoggedStream::new(guarded, state.db_pool.clone(), RequestLog {
            request_id: Some(request_id),
            key_hash: identity.key_hash.clone(),
            key_name: identity.key_name.clone(),
            key_alias: identity.key_alias.clone(),
            team_id: identity.team_id.clone(),
            model,
            model_name: Some(inflight_model.clone()),
            api_path: api_path_owned,
            is_stream: true,
            status_code: 200,
            error_type: None,
            error_message: None,
            input_tokens: None,
            output_tokens: None,
            duration_ms: None,
            ttft_ms: None,
            deployment_id,
            client_ip: Some(client_ip.clone()),
            cached_tokens: None,
        }, start, usage, Some(state.agent_stats.clone()))
        .with_plan_charge(plan_charge);

        // Wrap with prompt log stream if enabled, then wrap in Sse.
        if let Some(sender) = prompt_log_sender {
            if let Some(req_body) = prompt_log_req_body {
                let prompt_entry = PromptLogEntry::new(
                    prompt_log_rid.as_deref().unwrap_or_default(),
                    &identity.key_hash,
                    identity.key_alias.as_deref(),
                    identity.team_alias.as_deref(),
                    prompt_log_model.as_deref().unwrap_or_default(),
                    api_path,
                    true,
                    req_body,
                    Some(&client_ip),
                );
                let prompt_logged = PromptLogStream::new(logged, sender, prompt_entry, sse_raw_data_extractor(), None);
                let response = Sse::new(sse_item_to_event(prompt_logged)).keep_alive(KeepAlive::default());
                return Ok(response.into_response());
            }
        }
        let response = Sse::new(sse_item_to_event(logged)).keep_alive(KeepAlive::default());
        Ok(response.into_response())
    } else {
        let _inflight = if let Some(ref did) = deployment_id {
            InFlightGuard::new_for_deployment(state.inflight.clone(), &inflight_model, did, input_chars as u64)
        } else {
            InFlightGuard::new(state.inflight.clone(), &inflight_model, input_chars as u64)
        };
        let response = match provider.chat(req).await {
            Ok(r) => r,
            Err(e) => {
                log_error(&state, &identity, &model, api_path, false, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone(), Some(client_ip.clone()));
                crate::health_monitor::record_request_failure(&state, &deployment_id, &e);
                // plan_charge drops here without commit — no quota consumed.
                return Err(GatewayErrorReply(e, false));
            }
        };
        // Provider accepted the request — commit the plan charge now.
        let _decision = plan_charge.commit();
        // Release concurrency guard now (non-streaming: response is already complete,
        // concurrency slot no longer needed once we have the full response).
        drop(plan_charge.take_concurrency_guard());
        crate::health_monitor::reset_request_failure(&state, &deployment_id);

        let duration_ms = start.elapsed().as_millis() as i32;
        let input_tokens = response.usage.prompt_tokens as i32;
        let output_tokens = response.usage.completion_tokens as i32;
        // Settle quota: real token counts from vLLM go to key + team cumulative
        // and 1-min TPM windows. Cost is computed from the model's cost_rate;
        // cached_tokens get the discounted rate when configured.
        let cached_tokens_i64 = response
            .usage
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0);
        plan_charge.settle(
            input_tokens as u64,
            cached_tokens_i64.max(0) as u64,
            output_tokens as u64,
        );
        let cached_tokens = Some(cached_tokens_i64 as i64);

        // Provider returned a response — count as a successfully handled request.
        state.agent_stats.record(api_path);
        if let Some(ref did) = deployment_id {
            state.request_rate.record(did);
        }

        log_request(
            state.db_pool.clone(),
            RequestLog {
                request_id: Some(request_id),
                key_hash: identity.key_hash.clone(),
                key_name: identity.key_name.clone(),
                key_alias: identity.key_alias.clone(),
                team_id: identity.team_id.clone(),
                model,
                model_name: Some(inflight_model.clone()),
                api_path: api_path.to_string(),
                is_stream: false,
                status_code: 200,
                error_type: None,
                error_message: None,
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
                duration_ms: Some(duration_ms),
                ttft_ms: None,
                deployment_id,
                client_ip: Some(client_ip.clone()),
                cached_tokens,
            },
        );
        state.agent_stats.record_tokens(api_path, input_tokens as u64, output_tokens as u64);

        // Normalize internal ContentPart::Reasoning to standard OpenAI `reasoning_content`
        // field so clients receive {"reasoning_content": "..."} instead of non-standard
        // content parts with type "reasoning".
        let mut resp = response;
        for choice in &mut resp.choices {
            choice.message.normalize_reasoning_for_openai();
        }

        // Prompt log: capture non-streaming response.
        if let Some(sender) = prompt_log_sender {
            if let Some(req_body) = prompt_log_req_body {
                let mut prompt_entry = PromptLogEntry::new(
                    prompt_log_rid.as_deref().unwrap_or_default(),
                    &identity.key_hash,
                    identity.key_alias.as_deref(),
                    identity.team_alias.as_deref(),
                    prompt_log_model.as_deref().unwrap_or_default(),
                    api_path,
                    false,
                    req_body,
                    Some(client_ip.as_str()),
                );
                prompt_entry.set_response(serde_json::to_value(&resp).unwrap_or(serde_json::Value::Null));
                prompt_entry.set_status(200, duration_ms as u64);
                let _ = sender.send(prompt_entry);
            }
        }

        Ok(Json(resp).into_response())
    }
}

// ============================================================
// Legacy Completions (OpenAI /v1/completions, /completions)
// ============================================================

pub async fn completions(
    State(state): State<AppState>,
    auth: RequiredAuth,
    headers: axum::http::HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<std::net::SocketAddr>,
    Json(req): Json<CompletionRequest>,
) -> Result<impl IntoResponse, GatewayErrorReply> {
    let chat_req = req.into_chat_request();
    chat_completions_inner(state, auth, &headers, Some(remote_addr), chat_req, "/v1/completions").await
}

// ============================================================
// List Models
// ============================================================

pub async fn list_models(
    State(state): State<AppState>,
    auth: RequiredAuth,
) -> Result<Json<serde_json::Value>, GatewayErrorReply> {
    let identity = auth.identity();
    let inner = state.inner.load();

    // Collect all visible model names (deployments + non-hidden aliases, excluding "*").
    let all_names = state.router.visible_model_names();
    let public_models = &inner.config.general_settings.public_models;

    let visible: Vec<ModelInfo> = if identity.models.is_empty() {
        // Unrestricted key — show all visible models.
        all_names
            .iter()
            .map(|name| ModelInfo {
                id: name.clone(),
                object: "model".to_string(),
                created: 0,
                owned_by: "boom-gateway".to_string(),
            })
            .collect()
    } else {
        // Restricted key — show models the key has access to + public_models.
        all_names
            .iter()
            .filter(|name| is_model_visible(name, &identity.models, &state.router, public_models))
            .map(|name| ModelInfo {
                id: name.clone(),
                object: "model".to_string(),
                created: 0,
                owned_by: "boom-gateway".to_string(),
            })
            .collect()
    };

    Ok(Json(serde_json::json!({
        "object": "list",
        "data": visible,
    })))
}

// ============================================================
// Get Single Model
// ============================================================

pub async fn get_model(
    State(state): State<AppState>,
    auth: RequiredAuth,
    Path(model_id): Path<String>,
) -> Result<Json<serde_json::Value>, GatewayErrorReply> {
    let identity = auth.identity();
    let inner = state.inner.load();

    // Collect all visible model names (same logic as list_models).
    let all_names = state.router.visible_model_names();
    let public_models = &inner.config.general_settings.public_models;

    let is_accessible = if identity.models.is_empty() {
        true // Unrestricted key — check existence only.
    } else {
        // Restricted key — check if model_id is in the accessible set (including public_models).
        all_names.iter()
            .filter(|name| is_model_visible(name, &identity.models, &state.router, public_models))
            .any(|name| name == &model_id)
    };

    if !is_accessible || !all_names.iter().any(|n| n == &model_id) {
        return Err(GatewayErrorReply(GatewayError::ModelNotFound(model_id), false));
    }

    Ok(Json(serde_json::json!({
        "id": model_id,
        "object": "model",
        "created": 0,
        "owned_by": "boom-gateway",
    })))
}

// ============================================================
// Unsupported Endpoints (return proper OpenAI-style errors)
// ============================================================

macro_rules! unsupported_handler {
    ($name:ident, $label:expr) => {
        pub async fn $name(
            State(_state): State<AppState>,
            _auth: RequiredAuth,
        ) -> Result<axum::http::StatusCode, GatewayErrorReply> {
            Err(GatewayErrorReply(GatewayError::NotSupported($label.to_string()), false))
        }
    };
}

unsupported_handler!(embeddings, "The /v1/embeddings endpoint is not supported by BooMGateway");
unsupported_handler!(audio_speech, "The /v1/audio/speech endpoint is not supported by BooMGateway");
unsupported_handler!(audio_transcriptions, "The /v1/audio/transcriptions endpoint is not supported by BooMGateway");
unsupported_handler!(moderations, "The /v1/moderations endpoint is not supported by BooMGateway");

// ============================================================
// Health Check
// ============================================================

#[derive(serde::Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub uptime_secs: u64,
    pub db_connected: bool,
    pub models_count: usize,
    pub reload_count: u64,
    pub last_reload_at: Option<String>,
}

pub async fn health_check(State(state): State<AppState>) -> Json<HealthResponse> {
    let inner = state.inner.load();
    let uptime = chrono::Utc::now()
        .signed_duration_since(inner.health.started_at)
        .num_seconds();

    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: uptime as u64,
        db_connected: inner.health.db_connected,
        models_count: state.deployment_store.len(),
        reload_count: inner.health.reload_count,
        last_reload_at: Some(inner.health.last_reload_at.to_rfc3339()),
    })
}

pub async fn liveness_check() -> &'static str {
    "ok"
}

pub async fn readiness_check(State(state): State<AppState>) -> impl IntoResponse {
    let inner = state.inner.load();
    if inner.health.db_connected || inner.config.general_settings.database_url.is_none() {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    }
}

// ============================================================
// Admin: Config Reload (hot-reload trigger)
// ============================================================

#[derive(serde::Serialize)]
pub struct ReloadResponse {
    pub status: String,
    pub message: String,
}

/// POST /admin/config/reload
///
/// Requires master key. Re-reads config.yaml and atomically swaps
/// the running state. New requests immediately see the new config;
/// in-flight requests complete with the old state untouched.
pub async fn admin_reload_config(
    State(state): State<AppState>,
    auth: RequiredAuth,
) -> Result<Json<ReloadResponse>, GatewayErrorReply> {
    // Only master key can trigger reload.
    let identity = auth.identity();
    if identity.key_hash != "master" {
        return Err(GatewayErrorReply(GatewayError::AuthError(
            "Only master key can trigger config reload".to_string(),
        ), false));
    }

    match state.reload().await {
        Ok(summary) => Ok(Json(ReloadResponse {
            status: "ok".to_string(),
            message: summary,
        })),
        Err(e) => Err(GatewayErrorReply(GatewayError::ConfigError(format!("Reload failed: {}", e)), false)),
    }
}

// ============================================================
// Admin: Plan Management
// ============================================================

/// PUT /admin/plans — create or update a rate limit plan.
pub async fn admin_upsert_plan(
    State(state): State<AppState>,
    auth: RequiredAuth,
    Json(plan): Json<RateLimitPlan>,
) -> Result<Json<serde_json::Value>, GatewayErrorReply> {
    require_master(auth.identity())?;
    let name = plan.name.clone();
    state.plan_store.upsert_plan(plan);
    tracing::info!(plan = %name, "Plan upserted");
    Ok(Json(serde_json::json!({
        "status": "ok",
        "message": format!("Plan '{}' saved", name),
    })))
}

/// GET /admin/plans — list all plans.
pub async fn admin_list_plans(
    State(state): State<AppState>,
    auth: RequiredAuth,
) -> Result<Json<serde_json::Value>, GatewayErrorReply> {
    require_master(auth.identity())?;
    let plans = state.plan_store.list_plans();
    Ok(Json(serde_json::json!({ "plans": plans })))
}

/// DELETE /admin/plans/{name} — delete a plan (clears key assignments).
pub async fn admin_delete_plan(
    State(state): State<AppState>,
    auth: RequiredAuth,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, GatewayErrorReply> {
    require_master(auth.identity())?;
    if state.plan_store.delete_plan(&name) {
        tracing::info!(plan = %name, "Plan deleted");
        Ok(Json(serde_json::json!({
            "status": "ok",
            "message": format!("Plan '{}' deleted", name),
        })))
    } else {
        Err(GatewayErrorReply(GatewayError::ConfigError(format!("Plan '{}' not found",
            name
        )), false))
    }
}

/// POST /admin/plans/assign — assign a key to a plan.
#[derive(serde::Deserialize)]
pub(crate) struct AssignRequest {
    key_hash: String,
    plan_name: String,
}

pub async fn admin_assign_key(
    State(state): State<AppState>,
    auth: RequiredAuth,
    Json(body): Json<AssignRequest>,
) -> Result<Json<serde_json::Value>, GatewayErrorReply> {
    require_master(auth.identity())?;
    state
        .plan_store
        .assign_key(&body.key_hash, &body.plan_name)
        .map_err(|e| GatewayErrorReply(GatewayError::ConfigError(e), false))?;
    tracing::info!(key_hash = %body.key_hash, plan = %body.plan_name, "Key assigned to plan");
    Ok(Json(serde_json::json!({
        "status": "ok",
        "message": format!("Key '{}' assigned to plan '{}'", body.key_hash, body.plan_name),
    })))
}

/// DELETE /admin/plans/assign/{key_hash} — unassign a key from its plan.
pub async fn admin_unassign_key(
    State(state): State<AppState>,
    auth: RequiredAuth,
    Path(key_hash): Path<String>,
) -> Result<Json<serde_json::Value>, GatewayErrorReply> {
    require_master(auth.identity())?;
    if state.plan_store.unassign_key(&key_hash) {
        tracing::info!(key_hash = %key_hash, "Key unassigned from plan");
        Ok(Json(serde_json::json!({
            "status": "ok",
            "message": format!("Key '{}' unassigned", key_hash),
        })))
    } else {
        Err(GatewayErrorReply(GatewayError::ConfigError(format!("Key '{}' not assigned to any plan",
            key_hash
        )), false))
    }
}

/// GET /admin/plans/assignments — list all key-to-plan assignments.
pub async fn admin_list_assignments(
    State(state): State<AppState>,
    auth: RequiredAuth,
) -> Result<Json<serde_json::Value>, GatewayErrorReply> {
    require_master(auth.identity())?;
    let assignments = state.plan_store.list_assignments();
    let data: Vec<_> = assignments
        .into_iter()
        .map(|(key_hash, plan_name)| {
            serde_json::json!({
                "key_hash": key_hash,
                "plan_name": plan_name,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "assignments": data })))
}

fn require_master(identity: &AuthIdentity) -> Result<(), GatewayErrorReply> {
    if identity.key_hash != "master" {
        return Err(GatewayErrorReply(GatewayError::AuthError(
            "Only master key can manage plans".to_string(),
        ), false));
    }
    Ok(())
}

// ============================================================
// Error Response — stream-aware wrappers
// ============================================================

/// OpenAI-style error reply. When `is_stream` is true, returns SSE format
/// so streaming clients receive a clear error instead of hanging.
pub struct GatewayErrorReply(pub GatewayError, pub bool /* is_stream */);

impl IntoResponse for GatewayErrorReply {
    fn into_response(self) -> axum::response::Response {
        let status = axum::http::StatusCode::from_u16(self.0.status_code())
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        if self.1 {
            // Streaming mode: return SSE-formatted error so the client
            // (which expects text/event-stream) can parse it correctly.
            let body = serde_json::json!({
                "error": {
                    "message": self.0.to_string(),
                    "type": self.0.error_type(),
                    "code": self.0.status_code(),
                }
            });
            let data = serde_json::to_string(&body).unwrap_or_default();
            let sse_body = format!("data: {data}\n\ndata: [DONE]\n\n");

            let mut response = sse_body.into_response();
            *response.status_mut() = status;
            response.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                "text/event-stream".parse().unwrap(),
            );
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                "no-cache".parse().unwrap(),
            );
            if let GatewayError::RateLimitExceeded {
                retry_after_secs: Some(secs),
                ..
            } = self.0
            {
                response.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    secs.to_string().parse().unwrap(),
                );
            }
            response
        } else {
            // Non-streaming: standard JSON error.
            let body = serde_json::json!({
                "error": {
                    "message": self.0.to_string(),
                    "type": self.0.error_type(),
                    "code": self.0.status_code(),
                }
            });

            let mut response = (status, Json(body)).into_response();
            if let GatewayError::RateLimitExceeded {
                retry_after_secs: Some(secs),
                ..
            } = self.0
            {
                response.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    secs.to_string().parse().unwrap(),
                );
            }
            response
        }
    }
}

impl From<GatewayError> for GatewayErrorReply {
    fn from(e: GatewayError) -> Self {
        GatewayErrorReply(e, false)
    }
}

// ============================================================
// Model Visibility Helper (shared by list_models + get_model)
// ============================================================

/// Check if a model name should be visible to a restricted key.
/// Considers: key whitelist, aliases, and public_models.
fn is_model_visible(
    name: &str,
    key_models: &[String],
    router: &Router,
    public_models: &[String],
) -> bool {
    // Public model — always visible.
    if public_models.iter().any(|m| m == name)
        || router.resolve_model(name).map_or(false, |target| public_models.iter().any(|m| m == &target))
    {
        return true;
    }
    // Direct match in key's allowed list.
    if key_models.iter().any(|m| m == name && m != "*") {
        return true;
    }
    // If name is an alias, check if key has access to target model.
    if let Some(target) = router.resolve_model(name) {
        if key_models.iter().any(|m| m == &target && m != "*") {
            return true;
        }
    }
    // If name is a deployment (target), check if key has an alias that maps to it.
    for allowed in key_models {
        if allowed == "*" {
            continue;
        }
        if let Some(target) = router.resolve_model(allowed) {
            if target == name {
                return true;
            }
        }
    }
    false
}

// ============================================================
// Model Access Check (deployment-aware, uses stores)
// ============================================================

/// Check if an identity can access the given model, considering the gateway's
/// configured deployments and model aliases.
fn check_model_access(
    identity: &AuthIdentity,
    model: &str,
    router: &Router,
    public_models: &[String],
) -> Result<(), GatewayError> {
    // Public model — bypass all key-level whitelist checks.
    // Match both the requested name and its alias target (if any).
    let is_public = public_models.iter().any(|m| m == model)
        || router.resolve_model(model).map_or(false, |target| public_models.iter().any(|m| m == &target));
    if is_public {
        return Ok(());
    }

    // Unrestricted key
    if identity.models.is_empty() {
        tracing::debug!(
            "check_model_access: key={:?}, model={}, result=allow (unrestricted)",
            identity.key_name, model
        );
        return Ok(());
    }

    // Direct match
    if identity.models.iter().any(|m| m == model) {
        tracing::debug!(
            "check_model_access: key={:?}, model={}, result=allow (direct match)",
            identity.key_name, model
        );
        return Ok(());
    }

    // Alias match: requested model is an alias -> check if target is in key_models
    if let Some(target) = router.resolve_model(model) {
        if identity.models.iter().any(|m| m == &target) {
            tracing::debug!(
                "check_model_access: key={:?}, model={}, result=allow (alias -> target={})",
                identity.key_name, model, target
            );
            return Ok(());
        }
    }

    // Reverse alias: key_models has an alias that targets the requested model
    for allowed in &identity.models {
        if let Some(target) = router.resolve_model(allowed) {
            if target == model {
                tracing::debug!(
                    "check_model_access: key={:?}, model={}, result=allow (key has alias '{}' -> this model)",
                    identity.key_name, model, allowed
                );
                return Ok(());
            }
        }
    }

    // Not in key_models — check if it's a configured model or a wildcard case
    let has_wildcard = identity.models.iter().any(|m| m == "*");
    let model_configured = router.is_model_configured(model);
    let is_virtual = router.is_hybrid_virtual_model(model);

    // Virtual model (hybrid router) — always allow, it resolves to a real model later.
    if is_virtual {
        tracing::debug!(
            "check_model_access: key={:?}, model={}, result=allow (hybrid virtual model)",
            identity.key_name, model
        );
        return Ok(());
    }

    if model_configured {
        tracing::warn!(
            "check_model_access: key={:?}, model={}, result=deny (configured model, not in key_models={:?})",
            identity.key_name, model, identity.models
        );
        return Err(GatewayError::ModelNotAllowed(model.to_string()));
    }

    if has_wildcard {
        tracing::debug!(
            "check_model_access: key={:?}, model={}, result=allow (wildcard fallback)",
            identity.key_name, model
        );
        return Ok(());
    }

    tracing::warn!(
        "check_model_access: key={:?}, model={}, result=deny (no match, no wildcard, key_models={:?})",
        identity.key_name, model, identity.models
    );
    Err(GatewayError::ModelNotAllowed(model.to_string()))
}

// ============================================================
// Plan-based Rate Limiting Helper
// ============================================================

/// A pending plan charge: created by `check_plan_limits` (peek-only, no
/// counters incremented), committed by `PlanCharge::commit` once the upstream
/// provider has accepted the request. If dropped without commit, no quota is
/// consumed — the ConcurrencyGuard is released via its own Drop.
///
/// Settle is called separately (after the response is fully consumed) to
/// record token/cost usage against key + team quota counters.
///
/// This implements principle 1: "未服务不计费" — plan counters are only
/// incremented after `provider.chat_stream`/`provider.chat` returns Ok.
#[allow(dead_code)]
pub struct PlanCharge {
    limiter: Arc<boom_limiter::SlidingWindowLimiter>,
    key: RateLimitKey,
    rpm_limit: Option<u64>,
    /// Multi-dimensional window limits as resolved from the plan (shorthand
    /// merged in). The `counts` dimension is enforced via `limiter`'s
    /// SlidingWindowLimiter; `tokens` / `costs` dimensions are enforced via
    /// `QuotaStore` windows.
    window_limits: Vec<boom_core::types::WindowLimit>,
    weight: u64,
    /// Held until `take_concurrency_guard` moves it onto GuardedStream.
    /// Drop releases the key concurrency slot (RAII).
    concurrency_guard: Option<ConcurrencyGuard>,
    /// Team concurrency guard — separate field so it can have its own RAII
    /// drop semantics independently of the key guard.
    team_concurrency_guard: Option<ConcurrencyGuard>,

    /// Resolved model name (post-alias) for cost rate lookup at settle time.
    resolved_model: String,
    /// team_id if the key belongs to a team (for team quota attribution).
    team_id: Option<String>,
    /// Quota store reference for settle. None when running in no-DB mode
    /// (no persistent quota tracking — settle becomes a no-op).
    quota_store: Option<Arc<QuotaStore>>,
    /// Per-model cost rate (input + output cost per token in USD).
    /// Zero if no rate is configured for this model.
    cost_rate: ModelCostRate,

    /// For display in log_request_summary.
    plan_name: Option<String>,
    /// Team plan name (if team plan was applied).
    team_plan_name: Option<String>,
    rpm_limit_display: u64,
    concurrency_display: u32,
    concurrency_limit_display: Option<u32>,
    /// Pre-commit peek remaining (for log before commit).
    peek_remaining: u64,
    committed: bool,
    /// Post-commit remaining (filled by commit()).
    committed_remaining: Option<u64>,
    /// True after settle has been called. Prevents double-settle.
    settled: bool,
}

impl PlanCharge {
    /// Commit the charge: increment RPM + counts-windows by `weight`.
    /// MUST be called only after `provider.chat_stream`/`provider.chat`
    /// returned Ok — this is the moment the request is truly sent to the
    /// upstream and "已服务" semantics begin.
    ///
    /// Tokens/costs windows are NOT incremented here — they're rolled forward
    /// in `settle` once real token counts are known. This split mirrors the
    /// three-phase rate-limit contract (peek → commit → settle): counts
    /// dimension is request-rate (increment on accept), tokens/costs are
    /// usage-rate (increment after the upstream returns real usage).
    pub fn commit(&mut self) -> RateLimitDecision {
        let counts_only: Vec<(u64, u64)> = self
            .window_limits
            .iter()
            .filter_map(|w| w.counts.map(|c| (c, w.window_secs)))
            .collect();
        let decision = self.limiter.commit_record(
            &self.key,
            self.rpm_limit,
            &counts_only,
            self.weight,
        );
        self.committed = true;
        self.committed_remaining = Some(decision.remaining);
        decision
    }

    /// Move the ConcurrencyGuard out so it can be attached to GuardedStream
    /// and released when the stream ends. Called after `commit()`.
    pub fn take_concurrency_guard(&mut self) -> Option<ConcurrencyGuard> {
        self.concurrency_guard.take()
    }

    /// Remaining RPM for display. Post-commit value if committed, else peek.
    pub fn display_remaining(&self) -> u64 {
        self.committed_remaining.unwrap_or(self.peek_remaining)
    }

    /// Settle the request: add input/output tokens + cost to the key and
    /// (if set) the team's cumulative + multi-window counters. Idempotent —
    /// a second call is a no-op. Only valid after `commit()`.
    ///
    /// `cached_tokens` is the KV-cache hit count reported by vLLM in
    /// `prompt_tokens_details.cached_tokens`. When the model's cost rate
    /// has a separate `cached_input_cost_per_token`, cached hits are billed
    /// at the discounted rate; otherwise at the regular input rate.
    pub fn settle(&mut self, input_tokens: u64, cached_tokens: u64, output_tokens: u64) {
        if !self.committed || self.settled {
            return;
        }
        self.settled = true;
        if let Some(ref store) = self.quota_store {
            let (regular_input_cost, cached_input_cost, output_cost) = self
                .cost_rate
                .compute_cost_breakdown(input_tokens, cached_tokens, output_tokens);
            // Roll forward every configured window_secs — tokens and cost
            // both get added to each window. Dedup so multiple dimensions on
            // the same window_secs only produce one window per kind.
            let mut secs_set: Vec<u64> = Vec::new();
            for w in &self.window_limits {
                if !secs_set.contains(&w.window_secs) {
                    secs_set.push(w.window_secs);
                }
            }
            store.settle_usage(
                &self.key.key_hash,
                self.team_id.as_deref(),
                &self.resolved_model,
                input_tokens,
                output_tokens,
                regular_input_cost,
                cached_input_cost,
                output_cost,
                &secs_set,
            );
        }
    }
}

impl Drop for PlanCharge {
    fn drop(&mut self) {
        if !self.committed {
            tracing::debug!(
                key = ?self.key,
                committed = false,
                "PlanCharge dropped without commit — no quota consumed"
            );
        } else if !self.settled {
            // Committed but never settled (e.g. provider error after commit,
            // or streaming dropped mid-flight without a final usage chunk).
            // The limiter counters (RPM / window) were already incremented
            // by commit() and will expire naturally — no rollback needed.
            // Cumulative token/cost counters are NOT incremented (settle
            // was never called), so team/key TPM windows stay uncounted.
            tracing::debug!(
                key = ?self.key,
                "PlanCharge dropped without settle — limiter counters stay, quota counters untouched"
            );
        }
    }
}

/// Resolve the quota weight for a model, handling alias resolution.
/// If the model is an alias, resolves to the target model first.
/// Returns the `quota_count_ratio` for the resolved model, defaulting to 1.
fn resolve_quota_weight(model: &str, state: &AppState) -> u64 {
    let resolved = state.router.resolve_model(model)
        .unwrap_or_else(|| model.to_string());
    state.deployment_store.get_quota_ratio(&resolved)
}

/// Resolve the model name for cost-rate / quota attribution. Alias-resolved
/// so the cost rate matches the actual upstream deployment.
fn resolve_model_name(model: &str, state: &AppState) -> String {
    state.router.resolve_model(model)
        .unwrap_or_else(|| model.to_string())
}

/// Check if a key has VIP status from metadata.
fn is_vip_key(metadata: &serde_json::Value) -> bool {
    metadata
        .as_object()
        .and_then(|m| m.get("vip"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Build gateway-internal HTTP headers to attach to the upstream request.
///
/// Returns an empty map (no extra headers) unless priority-header injection is
/// explicitly enabled via `router_settings.enable_priority_header`. This keeps
/// normal deployments clean and only emits `X-Gateway-Priority` when a
/// downstream scheduler is being rolled out.
fn build_gateway_headers(
    is_vip: bool,
    enable_priority_header: bool,
    api_path: &str,
    client_type_enabled: bool,
) -> std::collections::HashMap<String, String> {
    let mut headers = std::collections::HashMap::new();
    if enable_priority_header {
        let priority = if is_vip { 100 } else { 0 };
        headers.insert("X-Gateway-Priority".to_string(), priority.to_string());
    }
    if client_type_enabled {
        let kind = boom_ctxaware::classify(api_path);
        headers.insert(
            boom_ctxaware::CLIENT_TYPE_HEADER.to_string(),
            kind.wire_label().to_string(),
        );
    }
    headers
}

/// Build a RateLimitExceeded error with full scope info.
fn rate_limit_exceeded(
    retry_after_secs: Option<u64>,
    message: String,
    limit_type: &'static str,
    scope: &'static str,
    scope_id: String,
    plan_name: Option<String>,
) -> GatewayError {
    GatewayError::RateLimitExceeded {
        retry_after_secs,
        message,
        limit_type,
        scope: Some(scope),
        scope_id: Some(scope_id),
        plan_name,
    }
}

/// Peek the total cost counter (stored as integer micros) against a Decimal
/// cost limit. Converts the limit to micros for comparison.
/// Returns Some(error) if exceeded, None if OK.
fn peek_cost_limit(
    store: &QuotaStore,
    scope: &QuotaScope,
    limit: rust_decimal::Decimal,
    scope_label: &'static str,
    scope_id: &str,
    plan_name: Option<&str>,
    limit_type: &'static str,
) -> Option<GatewayError> {
    let limit_micros = boom_quota::decimal_to_micros(limit);
    let current_micros = store.peek_cumulative(scope, CumulativeKind::TotalCost);
    if current_micros >= limit_micros {
        let plan_str = plan_name.map(|n| format!("Plan '{}', ", n)).unwrap_or_default();
        Some(rate_limit_exceeded(
            None,
            format!(
                "{}Total cost limit exceeded. Current: ${}, Limit: ${}",
                plan_str,
                boom_quota::micros_to_decimal(current_micros),
                limit
            ),
            limit_type,
            scope_label,
            scope_id.to_string(),
            plan_name.map(|s| s.to_string()),
        ))
    } else {
        None
    }
}

/// Peek a token-window against a limit. Returns Some(error) if exceeded.
///
/// `window_secs` selects which sliding window to read — typically 60 for the
/// shorthand `tpm_limit`, or the entry's `window_secs` for the multi-dim
/// `window_limits`.
fn peek_tokens_window(
    store: &QuotaStore,
    scope: &QuotaScope,
    model: &str,
    window_secs: u64,
    limit: u64,
    scope_label: &'static str,
    scope_id: &str,
    plan_name: Option<&str>,
    limit_type: &'static str,
) -> Option<GatewayError> {
    let snap = store.peek_window(scope, model, boom_quota::WindowKind::Tokens, window_secs);
    if snap.count >= limit {
        let plan_str = plan_name.map(|n| format!("Plan '{}', ", n)).unwrap_or_default();
        Some(rate_limit_exceeded(
            Some(window_secs.saturating_sub(snap.elapsed_secs)),
            format!(
                "{}Token window limit exceeded ({}s). Current: {}, Limit: {} per {}s",
                plan_str, window_secs, snap.count, limit, window_secs
            ),
            limit_type,
            scope_label,
            scope_id.to_string(),
            plan_name.map(|s| s.to_string()),
        ))
    } else {
        None
    }
}

/// Peek a cost-window against a limit. Returns Some(error) if exceeded.
///
/// `window_secs` selects which sliding window to read; `limit` is in USD
/// (Decimal) — internally converted to micros for comparison.
fn peek_cost_window(
    store: &QuotaStore,
    scope: &QuotaScope,
    model: &str,
    window_secs: u64,
    limit: rust_decimal::Decimal,
    scope_label: &'static str,
    scope_id: &str,
    plan_name: Option<&str>,
    limit_type: &'static str,
) -> Option<GatewayError> {
    let limit_micros = boom_quota::decimal_to_micros(limit);
    let snap = store.peek_window(scope, model, boom_quota::WindowKind::CostMicros, window_secs);
    if snap.count >= limit_micros {
        let plan_str = plan_name.map(|n| format!("Plan '{}', ", n)).unwrap_or_default();
        Some(rate_limit_exceeded(
            Some(window_secs.saturating_sub(snap.elapsed_secs)),
            format!(
                "{}Cost window limit exceeded ({}s). Current: ${}, Limit: ${} per {}s",
                plan_str,
                window_secs,
                boom_quota::micros_to_decimal(snap.count),
                limit,
                window_secs
            ),
            limit_type,
            scope_label,
            scope_id.to_string(),
            plan_name.map(|s| s.to_string()),
        ))
    } else {
        None
    }
}

/// Check plan-based limits (peek-only, no counters incremented) if a plan is
/// assigned, otherwise fall back to default per-model rate limits.
///
/// Double-layer check:
/// 1. Team layer (if key has team_id and team has a plan assigned):
///    - concurrency_limit, rpm_limit, window_limits (multi-dim)
///    - total_tpm_limit, total_cost_limit
/// 2. Key layer:
///    - concurrency_limit, rpm_limit, window_limits (multi-dim)
///    - total_tpm_limit, total_cost_limit
///
/// A plan is a generic template — the same set of fields applies whether it
/// is assigned to a key or a team. The `type` field (key/team) only gates
/// assignment, not which fields are read.
///
/// For each `WindowLimit` entry:
///   - `counts` dimension → SlidingWindowLimiter peek (request-rate semantics)
///   - `tokens` dimension → QuotaStore Tokens window peek (current cumulative
///     vs limit; the request that pushes the counter past the limit is allowed
///     through, the next one is rejected)
///   - `costs` dimension → QuotaStore CostMicros window peek (same semantics)
///
/// Reject if EITHER layer exceeds ANY configured limit. Team layer is checked
/// first because that's the outer box.
///
/// Returns a `PlanCharge` that holds the pending charge info + ConcurrencyGuard.
/// The caller MUST call `PlanCharge::commit()` after the upstream provider
/// accepts the request, and `PlanCharge::settle(input, output)` after the
/// response is fully consumed (with real token counts from vLLM).
async fn check_plan_limits(
    state: &AppState,
    plan_store: &Arc<PlanStore>,
    limiter: &Arc<boom_limiter::SlidingWindowLimiter>,
    key_hash: &str,
    team_id: Option<&str>,
    requested_model: &str,
    resolved_model: &str,
    rpm_limit: Option<u64>,
    window_limits: &[boom_core::types::WindowLimit],
    weight: u64,
) -> Result<PlanCharge, GatewayError> {
    // ── Resolve key plan (with default_plan fallback) ──
    // If key has no assignment, use default_plan. If the resolved plan is
    // type=Team (misconfiguration), warn and fall back to default_plan.
    let key_plan: Option<RateLimitPlan> = match plan_store.resolve_plan(key_hash) {
        Some(p) if p.r#type == boom_core::types::PlanType::Team => {
            tracing::warn!(
                key_hash = %key_hash,
                plan = %p.name,
                "Key is assigned a type=team plan — falling back to default_plan. \
                 Team plans cannot be assigned to individual keys."
            );
            plan_store.get_default_plan()
        }
        Some(p) => Some(p),
        None => plan_store.get_default_plan(),
    };

    // ── Resolve team plan (if team_id is set) ──
    let team_plan: Option<RateLimitPlan> = team_id
        .and_then(|tid| plan_store.resolve_team_plan(tid));

    // If neither key nor team has a plan → fall back to legacy per-model RPM limits.
    if key_plan.is_none() && team_plan.is_none() {
        let rl_key = RateLimitKey {
            key_hash: key_hash.to_string(),
            model: requested_model.to_string(),
        };
        let counts_only: Vec<(u64, u64)> = window_limits
            .iter()
            .filter_map(|w| w.counts.map(|c| (c, w.window_secs)))
            .collect();

        let decision = limiter
            .peek_only(&rl_key, rpm_limit, &counts_only, weight)
            .await;

        if !decision.allowed {
            let (limit_type, message) = match decision.rejected_window_secs {
                Some(60) | None => (
                    "rpm_limit",
                    format!("RPM limit exceeded. Model: {}, Limit: {}/min", requested_model, decision.limit),
                ),
                Some(secs) => (
                    "window_limit",
                    format!("Window limit exceeded. Model: {}, Limit: {} per {}s", requested_model, decision.limit, secs),
                ),
            };
            return Err(GatewayError::RateLimitExceeded {
                retry_after_secs: decision.retry_after_secs,
                message,
                limit_type,
                scope: Some("key"),
                scope_id: Some(key_hash.to_string()),
                plan_name: None,
            });
        }

        // Per-window tokens/costs peek (QuotaStore).
        let key_scope = QuotaScope::Key { key_hash: key_hash.to_string() };
        for w in window_limits {
            if let Some(tok_limit) = w.tokens {
                if let Some(e) = peek_tokens_window(
                    &state.quota_store,
                    &key_scope,
                    resolved_model,
                    w.window_secs,
                    tok_limit,
                    "key",
                    key_hash,
                    None,
                    "key_window_tokens",
                ) {
                    return Err(e);
                }
            }
            if let Some(cost_limit) = w.costs {
                if let Some(e) = peek_cost_window(
                    &state.quota_store,
                    &key_scope,
                    resolved_model,
                    w.window_secs,
                    cost_limit,
                    "key",
                    key_hash,
                    None,
                    "key_window_cost",
                ) {
                    return Err(e);
                }
            }
        }

        let cost_rate = state.deployment_store.get_cost_rate(resolved_model);
        return Ok(PlanCharge {
            limiter: limiter.clone(),
            key: rl_key,
            rpm_limit,
            window_limits: window_limits.to_vec(),
            weight,
            concurrency_guard: None,
            team_concurrency_guard: None,
            resolved_model: resolved_model.to_string(),
            team_id: team_id.map(|s| s.to_string()),
            quota_store: Some(state.quota_store.clone()),
            cost_rate,
            plan_name: None,
            team_plan_name: None,
            rpm_limit_display: decision.limit,
            concurrency_display: 0,
            concurrency_limit_display: None,
            peek_remaining: decision.remaining,
            committed: false,
            committed_remaining: None,
            settled: false,
        });
    }

    // ── Plan-based path: team layer first, then key layer ──
    let key_plan_ref = key_plan.as_ref();
    let team_plan_ref = team_plan.as_ref();

    // 1a. Team concurrency guard
    let team_concurrency_guard = if let (Some(tp), Some(tid)) = (team_plan_ref, team_id) {
        let (tc_limit, _, _, _) = tp.effective_limits();
        if let Some(limit) = tc_limit {
            Some(plan_store.try_acquire_team(tid, limit).ok_or_else(|| {
                GatewayError::ConcurrencyExceeded {
                    limit,
                    message: format!(
                        "Team concurrency limit exceeded. Team: {}, Plan: {}, Limit: {}",
                        tid, tp.name, limit
                    ),
                }
            })?)
        } else {
            None
        }
    } else {
        None
    };

    // 1b. Team RPM / counts-window / tokens-window / cost-window peek
    let team_plan_name = team_plan_ref.map(|p| p.name.clone());
    if let (Some(tp), Some(tid)) = (team_plan_ref, team_id) {
        let (_, team_rpm, team_windows, team_stale) = tp.effective_limits();

        let team_rl_key = RateLimitKey {
            key_hash: format!("__team__{}", tid),
            model: "__plan__".to_string(),
        };

        // Clear stale team counters whose window_secs is no longer active.
        if !team_stale.is_empty() {
            limiter.clear_windows(&team_rl_key, &team_stale);
        }

        // Counts dimension: extract (limit, secs) pairs and let SlidingWindowLimiter peek.
        let team_counts: Vec<(u64, u64)> = team_windows
            .iter()
            .filter_map(|w| w.counts.map(|c| (c, w.window_secs)))
            .collect();

        let team_decision = limiter
            .peek_only(&team_rl_key, team_rpm, &team_counts, weight)
            .await;

        if !team_decision.allowed {
            drop(team_concurrency_guard);
            let (limit_type, message) = match team_decision.rejected_window_secs {
                Some(60) | None => (
                    "team_rpm_limit",
                    format!("Team '{}' RPM limit exceeded. Plan: {}, Limit: {}/min", tid, tp.name, team_decision.limit),
                ),
                Some(secs) => (
                    "team_window_limit",
                    format!("Team '{}' window limit exceeded. Plan: {}, Limit: {} per {}s", tid, tp.name, team_decision.limit, secs),
                ),
            };
            return Err(GatewayError::RateLimitExceeded {
                retry_after_secs: team_decision.retry_after_secs,
                message,
                limit_type,
                scope: Some("team"),
                scope_id: Some(tid.to_string()),
                plan_name: Some(tp.name.clone()),
            });
        }

        // Tokens / costs dimensions per window entry.
        let team_scope = QuotaScope::Team { team_id: tid.to_string() };
        for w in &team_windows {
            if let Some(tok_limit) = w.tokens {
                if let Some(e) = peek_tokens_window(
                    &state.quota_store,
                    &team_scope,
                    resolved_model,
                    w.window_secs,
                    tok_limit,
                    "team",
                    tid,
                    Some(&tp.name),
                    "team_window_tokens",
                ) {
                    drop(team_concurrency_guard);
                    return Err(e);
                }
            }
            if let Some(cost_limit) = w.costs {
                if let Some(e) = peek_cost_window(
                    &state.quota_store,
                    &team_scope,
                    resolved_model,
                    w.window_secs,
                    cost_limit,
                    "team",
                    tid,
                    Some(&tp.name),
                    "team_window_cost",
                ) {
                    drop(team_concurrency_guard);
                    return Err(e);
                }
            }
        }

        // 1c. Team cumulative total-TPM / total-cost peeks
        if let Some(limit) = tp.total_tpm_limit {
            let input = state.quota_store.peek_cumulative(&team_scope, CumulativeKind::TotalInputTokens);
            let output = state.quota_store.peek_cumulative(&team_scope, CumulativeKind::TotalOutputTokens);
            if input + output >= limit {
                drop(team_concurrency_guard);
                return Err(rate_limit_exceeded(
                    None,
                    format!(
                        "Plan '{}' Total TPM limit exceeded. Current: {}, Limit: {}",
                        tp.name, input + output, limit
                    ),
                    "team_total_tpm",
                    "team",
                    tid.to_string(),
                    Some(tp.name.clone()),
                ));
            }
        }
        if let Some(limit) = tp.total_cost_limit {
            if let Some(e) = peek_cost_limit(
                &state.quota_store,
                &team_scope,
                limit,
                "team",
                tid,
                Some(&tp.name),
                "team_total_cost",
            ) {
                drop(team_concurrency_guard);
                return Err(e);
            }
        }
    }

    // 2a. Key concurrency guard
    let (concurrency_limit, plan_rpm_limit, plan_window_limits, stale_limits) = match key_plan_ref {
        Some(p) => p.effective_limits(),
        None => (None, None, Vec::new(), Vec::new()),
    };
    let plan_name = key_plan_ref.map(|p| p.name.clone());

    let key_concurrency_guard = if let (Some(limit), Some(_kp)) = (concurrency_limit, key_plan_ref) {
        Some(plan_store.try_acquire(key_hash, limit).ok_or_else(|| {
            let msg = if let Some(pn) = plan_name.as_ref() {
                format!("Concurrency limit exceeded. Plan: {}, Limit: {}", pn, limit)
            } else {
                format!("Concurrency limit exceeded. Limit: {}", limit)
            };
            GatewayError::ConcurrencyExceeded {
                limit,
                message: msg,
            }
        })?)
    } else {
        None
    };

    // 2b. Key RPM / counts-window peek
    let key_rl_key = RateLimitKey {
        key_hash: key_hash.to_string(),
        model: "__plan__".to_string(),
    };

    if !stale_limits.is_empty() {
        limiter.clear_windows(&key_rl_key, &stale_limits);
    }

    // Counts dimension: extract (limit, secs) pairs for SlidingWindowLimiter.
    let key_counts: Vec<(u64, u64)> = plan_window_limits
        .iter()
        .filter_map(|w| w.counts.map(|c| (c, w.window_secs)))
        .collect();

    let key_decision = if let Some(_kp) = key_plan_ref {
        limiter.peek_only(&key_rl_key, plan_rpm_limit, &key_counts, weight).await
    } else {
        // No key plan, only team plan: still allow per-model RPM fallback for key.
        let counts_fallback: Vec<(u64, u64)> = window_limits
            .iter()
            .filter_map(|w| w.counts.map(|c| (c, w.window_secs)))
            .collect();
        limiter.peek_only(&RateLimitKey {
            key_hash: key_hash.to_string(),
            model: requested_model.to_string(),
        }, rpm_limit, &counts_fallback, weight).await
    };

    if !key_decision.allowed {
        drop(key_concurrency_guard);
        drop(team_concurrency_guard);
        let (limit_type, message) = match key_decision.rejected_window_secs {
            Some(60) | None => {
                if let Some(ref pn) = plan_name {
                    ("plan_rpm_limit", format!("Plan '{}' RPM limit exceeded. Limit: {}/min", pn, key_decision.limit))
                } else {
                    ("rpm_limit", format!("RPM limit exceeded. Model: {}, Limit: {}/min", requested_model, key_decision.limit))
                }
            }
            Some(secs) => {
                if let Some(ref pn) = plan_name {
                    ("plan_window_limit", format!("Plan '{}' window limit exceeded. Limit: {} per {}s", pn, key_decision.limit, secs))
                } else {
                    ("window_limit", format!("Window limit exceeded. Model: {}, Limit: {} per {}s", requested_model, key_decision.limit, secs))
                }
            }
        };
        let plan_name_for_err = plan_name.clone();
        return Err(GatewayError::RateLimitExceeded {
            retry_after_secs: key_decision.retry_after_secs,
            message,
            limit_type,
            scope: Some("key"),
            scope_id: Some(key_hash.to_string()),
            plan_name: plan_name_for_err,
        });
    }

    // 2c. Key tokens-window / cost-window / cumulative peeks
    if let Some(kp) = key_plan_ref {
        let key_scope = QuotaScope::Key { key_hash: key_hash.to_string() };

        for w in &plan_window_limits {
            if let Some(tok_limit) = w.tokens {
                if let Some(e) = peek_tokens_window(
                    &state.quota_store,
                    &key_scope,
                    resolved_model,
                    w.window_secs,
                    tok_limit,
                    "key",
                    key_hash,
                    Some(&kp.name),
                    "key_window_tokens",
                ) {
                    drop(key_concurrency_guard);
                    drop(team_concurrency_guard);
                    return Err(e);
                }
            }
            if let Some(cost_limit) = w.costs {
                if let Some(e) = peek_cost_window(
                    &state.quota_store,
                    &key_scope,
                    resolved_model,
                    w.window_secs,
                    cost_limit,
                    "key",
                    key_hash,
                    Some(&kp.name),
                    "key_window_cost",
                ) {
                    drop(key_concurrency_guard);
                    drop(team_concurrency_guard);
                    return Err(e);
                }
            }
        }
        if let Some(limit) = kp.total_tpm_limit {
            let input = state.quota_store.peek_cumulative(&key_scope, CumulativeKind::TotalInputTokens);
            let output = state.quota_store.peek_cumulative(&key_scope, CumulativeKind::TotalOutputTokens);
            if input + output >= limit {
                drop(key_concurrency_guard);
                drop(team_concurrency_guard);
                return Err(rate_limit_exceeded(
                    None,
                    format!(
                        "Plan '{}' Total TPM limit exceeded. Current: {}, Limit: {}",
                        kp.name, input + output, limit
                    ),
                    "key_total_tpm",
                    "key",
                    key_hash.to_string(),
                    Some(kp.name.clone()),
                ));
            }
        }
        if let Some(limit) = kp.total_cost_limit {
            if let Some(e) = peek_cost_limit(
                &state.quota_store,
                &key_scope,
                limit,
                "key",
                key_hash,
                Some(&kp.name),
                "key_total_cost",
            ) {
                drop(key_concurrency_guard);
                drop(team_concurrency_guard);
                return Err(e);
            }
        }
    }

    // ── All checks passed — assemble PlanCharge ──
    let cost_rate = state.deployment_store.get_cost_rate(resolved_model);
    let concurrency_display = plan_store.get_concurrency(key_hash);

    Ok(PlanCharge {
        limiter: limiter.clone(),
        key: key_rl_key,
        rpm_limit: plan_rpm_limit.or(rpm_limit),
        window_limits: if key_plan_ref.is_some() { plan_window_limits } else { window_limits.to_vec() },
        weight,
        concurrency_guard: key_concurrency_guard,
        team_concurrency_guard,
        resolved_model: resolved_model.to_string(),
        team_id: team_id.map(|s| s.to_string()),
        quota_store: Some(state.quota_store.clone()),
        cost_rate,
        plan_name,
        team_plan_name,
        rpm_limit_display: key_decision.limit,
        concurrency_display,
        concurrency_limit_display: concurrency_limit,
        peek_remaining: key_decision.remaining,
        committed: false,
        committed_remaining: None,
        settled: false,
    })
}

// ============================================================
// Helpers
// ============================================================

/// Print a concise one-line summary of an accepted request to the server console.
fn log_request_summary(
    request_id: &str,
    identity: &AuthIdentity,
    model: &str,
    input_chars: usize,
    is_stream: bool,
    api_path: &str,
    plan_charge: &PlanCharge,
) {
    let key_display = identity.key_alias.as_deref()
        .or(identity.key_name.as_deref())
        .or(identity.user_id.as_deref())
        .unwrap_or("-");
    let plan_display = plan_charge.plan_name.as_deref().unwrap_or("-");
    let concurrency_display = match plan_charge.concurrency_limit_display {
        Some(limit) => format!("{}/{}", plan_charge.concurrency_display, limit),
        None => "-".to_string(),
    };
    tracing::info!(
        "[{}] {} {} stream={} key={} plan={} rpm={}/{} input_chars={} concurrency={}",
        &request_id[..8],
        api_path,
        model,
        is_stream,
        key_display,
        plan_display,
        plan_charge.display_remaining(),
        plan_charge.rpm_limit_display,
        input_chars,
        concurrency_display,
    );
}

fn sse_stream_from_chat_stream(
    stream: ChatStream,
    usage: UsageTracker,
) -> impl futures::Stream<Item = Result<SseItem, Infallible>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<SseItem>(32);
    tokio::spawn(async move {
        let mut tool_arg_buf: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
        let mut stream = std::pin::pin!(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(mut chunk) => {
                    if let Some(ref u) = chunk.usage {
                        if let Ok(mut g) = usage.lock() {
                            g.prompt_tokens = u.prompt_tokens;
                            g.completion_tokens = u.completion_tokens;
                            g.cached_tokens = u
                                .prompt_tokens_details
                                .as_ref()
                                .and_then(|d| d.cached_tokens)
                                .map(|c| c as i32);
                        }
                    }

                    let has_finish = chunk.choices.iter().any(|c| c.finish_reason.is_some());

                    // Buffer tool_call arguments via take() — zero-copy, no clone.
                    for choice in &mut chunk.choices {
                        if let Some(ref mut tool_calls) = choice.delta.tool_calls {
                            for tc in tool_calls.iter_mut() {
                                let args_taken = tc.function.as_mut().and_then(|f| f.arguments.take());
                                if let Some(args) = args_taken {
                                    if !args.is_empty() {
                                        // Fast path: incremental fragments almost never end with '}'.
                                        // Skip the expensive JSON parse when the string clearly isn't
                                        // a complete object.
                                        let is_complete = args.starts_with('{')
                                            && args.ends_with('}')
                                            && serde_json::from_str::<serde_json::Value>(&args).is_ok();
                                        let has_existing = tool_arg_buf.get(&tc.index).map_or(false, |e| !e.is_empty());

                                        if is_complete && has_existing {
                                            tool_arg_buf.insert(tc.index, args); // move, no clone
                                        } else {
                                            tool_arg_buf.entry(tc.index).or_default().push_str(&args);
                                        }
                                    }
                                    // arguments already taken — chunk emits without them
                                }
                            }
                        }
                    }

                    // At finish, flush buffered arguments BEFORE the finish chunk
                    // so the client accumulates complete args before seeing finish_reason.
                    if has_finish {
                        let mut indices: Vec<u32> = tool_arg_buf.keys().copied().collect();
                        indices.sort();
                        for idx in indices {
                            if let Some(buf) = tool_arg_buf.remove(&idx) {
                                if !buf.is_empty() {
                                    let flush_chunk = ChatStreamChunk {
                                        id: String::new(), // client ignores id on intermediate chunks
                                        object: "chat.completion.chunk".to_string(),
                                        created: 0,
                                        model: String::new(),
                                        choices: vec![StreamChoice {
                                            index: 0,
                                            delta: StreamDelta {
                                                role: None,
                                                content: None,
                                                tool_calls: Some(vec![ToolCallDelta {
                                                    index: idx,
                                                    id: None,
                                                    call_type: None,
                                                    function: Some(FunctionCallDelta {
                                                        name: None,
                                                        arguments: Some(buf), // move, no clone
                                                    }),
                                                }]),
                                                reasoning_content: None,
                                            },
                                            finish_reason: None,
                                        }],
                                        usage: None,
                                        raw_data: None,
                                    };
                                    let data = serde_json::to_string(&flush_chunk).unwrap_or_default();
                                    if tx.send(SseItem { event: Event::default().data(&data), json_data: data }).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }

                    // Emit the original chunk (arguments already taken).
                    let data = serde_json::to_string(&chunk).unwrap_or_default();
                    if tx.send(SseItem { event: Event::default().data(&data), json_data: data }).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::error!("SSE stream error (OpenAI): {}", e);
                    let error_data =
                        serde_json::to_string(&serde_json::json!({"error": "Upstream error"}))
                            .unwrap_or_default();
                    let _ = tx.send(SseItem { event: Event::default().data(&error_data), json_data: error_data }).await;
                }
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok)
}

// ============================================================
// Anthropic Messages
// ============================================================

pub async fn messages(
    State(state): State<AppState>,
    auth: RequiredAuth,
    headers: axum::http::HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<std::net::SocketAddr>,
    Json(mut req): Json<AnthropicMessagesRequest>,
) -> Result<impl IntoResponse, AnthropicErrorReply> {
    let start = Instant::now();
    let request_id = new_request_id();
    let identity = auth.identity();
    let client_ip = extract_client_ip(&headers, Some(remote_addr));
    let inner = state.inner.load();

    // Optional body rewrite: strip Claude Code's attribution block from the
    // system prompt. Done before anthropic_request_to_openai so that both the
    // converted openai_req and the prompt-log capture below see the cleaned
    // body in one shot.
    if inner.config.router_settings.strip_claude_code_attribution
        && crate::rewrite::strip_cc_attribution_anthropic(&mut req)
    {
        tracing::debug!(
            request_id = %request_id,
            "stripped claude code attribution block from anthropic system prompt"
        );
    }

    let mut openai_req = anthropic_request_to_openai(&req);
    let model = openai_req.model.clone();
    let is_stream = openai_req.stream.unwrap_or(false);

    tracing::info!(request_id = %request_id, model = %model, stream = is_stream, "messages request started");

    // Prompt log: check early to avoid unnecessary cloning.
    let prompt_log_should = state.prompt_log_writer.should_capture(
        &identity.key_hash,
        identity.team_id.as_deref(),
    );
    let prompt_log_capture_raw = prompt_log_should && state.prompt_log_writer.config().capture_raw_upstream;
    let prompt_log_req_body = if prompt_log_should {
        serde_json::to_value(&req).ok()
    } else {
        None
    };
    let prompt_log_sender = if prompt_log_should {
        Some(state.prompt_log_writer.sender())
    } else {
        None
    };
    // Clone request_id and model for prompt log (they get moved into RequestLog later).
    let prompt_log_rid = if prompt_log_should { Some(request_id.clone()) } else { None };
    let prompt_log_model = if prompt_log_should { Some(model.clone()) } else { None };

    // 1. Model access check (deployment-aware, alias-aware).
    check_model_access(identity, &openai_req.model, &state.router, &inner.config.general_settings.public_models)
        .map_err(|e| {
            log_error(&state, &identity, &model, "/v1/messages", is_stream, start, &e, Some(request_id.clone()), None, None, Some(client_ip.clone()));
            AnthropicErrorReply(e, is_stream)
        })?;

    // 1b. Content-aware model resolution (hybrid router).
    let resolved_model = state.router.resolve_request_model(
        &openai_req.model, &openai_req.messages, &openai_req.tools,
    );

    // 2. Plan-based or default rate limiting.
    let window_limits: Vec<boom_core::types::WindowLimit> = inner
        .config
        .rate_limit
        .window_limits
        .iter()
        .filter_map(|w| {
            if w.len() >= 2 {
                Some(boom_core::types::WindowLimit {
                    counts: Some(w[0]),
                    tokens: None,
                    costs: None,
                    window_secs: w[1],
                })
            } else {
                None
            }
        })
        .collect();
    let weight = resolve_quota_weight(&openai_req.model, &state);
    let resolved_model_for_quota = resolve_model_name(&openai_req.model, &state);

    let mut plan_charge = check_plan_limits(
        &state,
        &state.plan_store,
        &state.limiter,
        &identity.key_hash,
        identity.team_id.as_deref(),
        &openai_req.model,
        &resolved_model_for_quota,
        identity.rpm_limit,
        &window_limits,
        weight,
    )
    .await
    .map_err(|e| {
        log_error(&state, &identity, &model, "/v1/messages", is_stream, start, &e, Some(request_id.clone()), None, None, Some(client_ip.clone()));
        AnthropicErrorReply(e, is_stream)
    })?;

    let input_chars: usize = req.messages.iter().map(|m| match &m.content {
        boom_core::types::AnthropicContent::Text(t) => t.len(),
        boom_core::types::AnthropicContent::Blocks(blocks) => blocks.iter().map(|b| match b {
            boom_core::types::AnthropicContentBlock::Text { text, .. } => text.len(),
            _ => 0,
        }).sum(),
    }).sum();
    log_request_summary(
        &request_id, identity, &model, input_chars, is_stream,
        "/v1/messages", &plan_charge,
    );

    // 3. Select provider deployment.
    // Tokenize request if tokenizer pool is available (for KV-cache prefix matching).
    let token_ids = {
        let pool_guard = state.tokenizer_pool.load();
        match &**pool_guard {
            Some(pool) => {
                let system_str = req.system.as_ref().and_then(|s| match s {
                    boom_core::types::AnthropicSystemContent::Text(t) => Some(t.as_str()),
                    _ => None,
                });
                let msgs: Vec<serde_json::Value> = req.messages.iter().map(|m| serde_json::to_value(m).unwrap_or_default()).collect();
                pool.tokenize_anthropic(&resolved_model, system_str, &msgs).token_ids
            }
            None => Vec::new(),
        }
    };

    let selection = state
        .router
        .select_provider_with_prefix(&resolved_model, Some(&identity.key_hash), input_chars as u64, &token_ids)
        .ok_or_else(|| {
            let e = GatewayError::ModelNotFound(resolved_model.clone());
            log_error(&state, &identity, &model, "/v1/messages", is_stream, start, &e, Some(request_id.clone()), None, None, Some(client_ip.clone()));
            AnthropicErrorReply(e, is_stream)
        })?;
    let provider = selection.provider;
    let kv_hit_ratio = selection.kv_hit_ratio;

    let deployment_id = provider.deployment_id().map(|s| s.to_string());
    let inflight_model = state.router.resolve_model_name(&resolved_model);

    // 3.2. Determine KV-cache reporting mode (same logic as OpenAI path).
    openai_req.kv_cache_report_full = need_full_kv_report(
        state.kv_index.load().is_some(),
        kv_hit_ratio,
        inner.config.router_settings.kvc_aware.full_report_hit_threshold,
    );

    // 3.5. Flow control — queue if per-deployment limits exceeded.
    let is_vip = is_vip_key(&identity.metadata);
    let fc_guard = if let Some(ref did) = deployment_id {
        acquire_fc_guard(
            &state, did, input_chars as u64, is_vip,
            identity.key_alias.clone(), Some(identity.key_hash.clone()),
            Some(inflight_model.clone()),
            "/v1/messages", &identity, &model,
            is_stream, start, &request_id, Some(client_ip.clone()), AnthropicErrorReply,
        ).await?
    } else {
        None
    };

    // Attach gateway-internal headers (e.g. X-Gateway-Priority) when enabled.
    openai_req.gateway_headers = build_gateway_headers(
        is_vip,
        inner.config.router_settings.enable_priority_header,
        "/v1/messages",
        provider.client_type_header(),
    );

    // Capture request body for debug recording if debug mode is enabled.
    let debug_req_body = if state.debug_store.is_enabled() {
        serde_json::to_string(&req).ok()
    } else {
        None
    };

    // 4. Route to provider.
    if is_stream {
        let stream = match provider.chat_stream(openai_req).await {
            Ok(s) => s,
            Err(e) => {
                log_error(&state, &identity, &model, "/v1/messages", true, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone(), Some(client_ip.clone()));
                crate::health_monitor::record_request_failure(&state, &deployment_id, &e);
                // plan_charge drops here without commit — no quota consumed.
                return Err(AnthropicErrorReply(e, true));
            }
        };
        // Provider accepted the request — commit the plan charge now.
        let _decision = plan_charge.commit();
        crate::health_monitor::reset_request_failure(&state, &deployment_id);
        let usage = UsageTracker::default();
        // Create shared buffer for raw upstream SSE chunks (before Anthropic transcoding).
        let raw_upstream_buf = if prompt_log_capture_raw {
            Some(std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new())))
        } else {
            None
        };
        let sse_stream = sse_stream_from_anthropic_chat_stream(stream, model.clone(), usage.clone(), raw_upstream_buf.clone());
        let inflight_guard = if let Some(ref did) = deployment_id {
            InFlightGuard::new_for_deployment(state.inflight.clone(), &inflight_model, did, input_chars as u64)
        } else {
            InFlightGuard::new(state.inflight.clone(), &inflight_model, input_chars as u64)
        };
        let tracked = InFlightStream::new(sse_stream, inflight_guard);
        let flow_controlled = match fc_guard {
            Some(g) => FlowControlledStream::new(tracked, g),
            None => FlowControlledStream::passthrough(tracked),
        };
        let guarded = GuardedStream::new(flow_controlled, plan_charge.take_concurrency_guard());

        // Provider returned a stream — count as a successfully handled request.
        state.agent_stats.record("/v1/messages");
        if let Some(ref did) = deployment_id {
            state.request_rate.record(did);
        }

        // Wrap with LoggedStream — log is written when stream finishes (Drop).
        let logged = LoggedStream::new(guarded, state.db_pool.clone(), RequestLog {
            request_id: Some(request_id),
            key_hash: identity.key_hash.clone(),
            key_name: identity.key_name.clone(),
            key_alias: identity.key_alias.clone(),
            team_id: identity.team_id.clone(),
            model,
            model_name: Some(inflight_model.clone()),
            api_path: "/v1/messages".to_string(),
            is_stream: true,
            status_code: 200,
            error_type: None,
            error_message: None,
            input_tokens: None,
            output_tokens: None,
            duration_ms: None, // filled by LoggedStream::drop
            ttft_ms: None,
            deployment_id,
            client_ip: Some(client_ip.clone()),
            cached_tokens: None,
        }, start, usage, Some(state.agent_stats.clone()))
        .with_plan_charge(plan_charge);

        // Wrap with prompt log stream if enabled, then wrap in Sse.
        if let Some(sender) = prompt_log_sender {
            if let Some(req_body) = prompt_log_req_body {
                let prompt_entry = PromptLogEntry::new(
                    prompt_log_rid.as_deref().unwrap_or_default(),
                    &identity.key_hash,
                    identity.key_alias.as_deref(),
                    identity.team_alias.as_deref(),
                    prompt_log_model.as_deref().unwrap_or_default(),
                    "/v1/messages",
                    true,
                    req_body,
                    Some(&client_ip.as_str()),
                );
                let prompt_logged = PromptLogStream::new(logged, sender, prompt_entry, sse_raw_data_extractor(), raw_upstream_buf);
                let response = Sse::new(sse_item_to_event(prompt_logged)).keep_alive(KeepAlive::default());
                return Ok(response.into_response());
            }
        }
        let response = Sse::new(sse_item_to_event(logged)).keep_alive(KeepAlive::default());
        Ok(response.into_response())
    } else {
        let _inflight = if let Some(ref did) = deployment_id {
            InFlightGuard::new_for_deployment(state.inflight.clone(), &inflight_model, did, input_chars as u64)
        } else {
            InFlightGuard::new(state.inflight.clone(), &inflight_model, input_chars as u64)
        };
        let response = match provider.chat(openai_req).await {
            Ok(r) => r,
            Err(e) => {
                log_error(&state, &identity, &model, "/v1/messages", false, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone(), Some(client_ip.clone()));
                crate::health_monitor::record_request_failure(&state, &deployment_id, &e);
                // plan_charge drops here without commit — no quota consumed.
                return Err(AnthropicErrorReply(e, false));
            }
        };
        // Provider accepted the request — commit the plan charge now.
        let _decision = plan_charge.commit();
        // Release concurrency guard now (non-streaming: response is already complete).
        drop(plan_charge.take_concurrency_guard());
        crate::health_monitor::reset_request_failure(&state, &deployment_id);

        let duration_ms = start.elapsed().as_millis() as i32;
        let input_tokens = response.usage.prompt_tokens as i32;
        let output_tokens = response.usage.completion_tokens as i32;
        // Settle quota: real token counts from vLLM go to key + team cumulative
        // and 1-min TPM windows. Cost is computed from the model's cost_rate;
        // cached_tokens get the discounted rate when configured.
        let cached_tokens_i64 = response
            .usage
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0);
        plan_charge.settle(
            input_tokens as u64,
            cached_tokens_i64.max(0) as u64,
            output_tokens as u64,
        );
        let cached_tokens = Some(cached_tokens_i64 as i64);

        // Provider returned a response — count as a successfully handled request.
        state.agent_stats.record("/v1/messages");
        if let Some(ref did) = deployment_id {
            state.request_rate.record(did);
        }

        log_request(
            state.db_pool.clone(),
            RequestLog {
                request_id: Some(request_id),
                key_hash: identity.key_hash.clone(),
                key_name: identity.key_name.clone(),
                key_alias: identity.key_alias.clone(),
                team_id: identity.team_id.clone(),
                model,
                model_name: Some(inflight_model.clone()),
                api_path: "/v1/messages".to_string(),
                is_stream: false,
                status_code: 200,
                error_type: None,
                error_message: None,
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
                duration_ms: Some(duration_ms),
                ttft_ms: None,
                deployment_id,
                client_ip: Some(client_ip.clone()),
                cached_tokens,
            },
        );
        state.agent_stats.record_tokens("/v1/messages", input_tokens as u64, output_tokens as u64);

        let anthropic_resp = openai_response_to_anthropic(&response);

        // Prompt log: capture non-streaming Anthropic response.
        if let Some(sender) = prompt_log_sender {
            if let Some(req_body) = prompt_log_req_body {
                let mut prompt_entry = PromptLogEntry::new(
                    prompt_log_rid.as_deref().unwrap_or_default(),
                    &identity.key_hash,
                    identity.key_alias.as_deref(),
                    identity.team_alias.as_deref(),
                    prompt_log_model.as_deref().unwrap_or_default(),
                    "/v1/messages",
                    false,
                    req_body,
                    Some(client_ip.as_str()),
                );
                // Capture raw upstream response (exact bytes from provider, if enabled).
                if prompt_log_capture_raw {
                    if let Some(ref raw) = response.raw_response {
                        prompt_entry.set_raw_upstream_response(
                            serde_json::from_str::<serde_json::Value>(raw)
                                .unwrap_or(serde_json::Value::String(raw.clone()))
                        );
                    }
                }
                prompt_entry.set_response(serde_json::to_value(&anthropic_resp).unwrap_or(serde_json::Value::Null));
                prompt_entry.set_status(200, duration_ms as u64);
                let _ = sender.send(prompt_entry);
            }
        }

        Ok(Json(anthropic_resp).into_response())
    }
}

/// Convert an OpenAI stream into Anthropic-format SSE events via a transcoder + channel.
fn sse_stream_from_anthropic_chat_stream(
    stream: ChatStream,
    model: String,
    usage: UsageTracker,
    raw_upstream_sink: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
) -> impl futures::Stream<Item = Result<SseItem, Infallible>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<SseItem>(64);

    tokio::spawn(async move {
        let mut transcoder = AnthropicStreamTranscoder::new(model);
        let mut stream = std::pin::pin!(stream);

        while let Some(result) = stream.next().await {
            match result {
                Ok(chunk) => {
                    // Capture raw upstream chunk before transcoding (if enabled).
                    if let Some(ref sink) = raw_upstream_sink {
                        if let Some(ref raw) = chunk.raw_data {
                            if let Ok(mut guard) = sink.lock() {
                                guard.push(raw.clone());
                            }
                        }
                    }
                    // Extract usage from the chunk (OpenAI sends usage in the final chunk).
                    if let Some(ref u) = chunk.usage {
                        if let Ok(mut g) = usage.lock() {
                            g.prompt_tokens = u.prompt_tokens;
                            g.completion_tokens = u.completion_tokens;
                            g.cached_tokens = u
                                .prompt_tokens_details
                                .as_ref()
                                .and_then(|d| d.cached_tokens)
                                .map(|c| c as i32);
                        }
                    }
                    let events = transcoder.transcode(&chunk);
                    for ev in events {
                        let item = SseItem {
                            event: Event::default().event(&ev.event).data(&ev.data),
                            json_data: ev.data,
                        };
                        if tx.send(item).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("SSE stream error (Anthropic): {}", e);
                    let error_data = serde_json::json!({
                        "type": "error",
                        "error": { "type": "api_error", "message": "Upstream error" }
                    });
                    let data_str = error_data.to_string();
                    let _ = tx
                        .send(SseItem {
                            event: Event::default().event("error").data(&data_str),
                            json_data: data_str,
                        })
                        .await;
                    return;
                }
            }
        }

        // Flush any held finish events (e.g. if usage chunk never arrived).
        for ev in transcoder.drain() {
            let item = SseItem {
                event: Event::default().event(&ev.event).data(&ev.data),
                json_data: ev.data,
            };
            if tx.send(item).await.is_err() {
                return;
            }
        }
    });

    tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok)
}

// ============================================================
// Anthropic Error Response
// ============================================================

pub struct AnthropicErrorReply(pub GatewayError, pub bool /* is_stream */);

impl IntoResponse for AnthropicErrorReply {
    fn into_response(self) -> axum::response::Response {
        let status = axum::http::StatusCode::from_u16(self.0.status_code())
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        if self.1 {
            // Streaming mode: return Anthropic SSE error event.
            let body = serde_json::json!({
                "type": "error",
                "error": {
                    "type": self.0.error_type(),
                    "message": self.0.to_string(),
                }
            });
            let data = serde_json::to_string(&body).unwrap_or_default();
            let sse_body = format!("event: error\ndata: {data}\n\n");

            let mut response = sse_body.into_response();
            *response.status_mut() = status;
            response.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                "text/event-stream".parse().unwrap(),
            );
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                "no-cache".parse().unwrap(),
            );
            if let GatewayError::RateLimitExceeded {
                retry_after_secs: Some(secs),
                ..
            } = self.0
            {
                response.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    secs.to_string().parse().unwrap(),
                );
            }
            response
        } else {
            // Non-streaming: standard Anthropic JSON error.
            let body = serde_json::json!({
                "type": "error",
                "error": {
                    "type": self.0.error_type(),
                    "message": self.0.to_string(),
                }
            });

            let mut response = (status, Json(body)).into_response();
            if let GatewayError::RateLimitExceeded {
                retry_after_secs: Some(secs),
                ..
            } = self.0
            {
                response.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    secs.to_string().parse().unwrap(),
                );
            }
            response
        }
    }
}

impl From<GatewayError> for AnthropicErrorReply {
    fn from(e: GatewayError) -> Self {
        AnthropicErrorReply(e, false)
    }
}

// ============================================================
// SSE Content Extraction for Prompt Log
// ============================================================

/// Newtype that pairs an axum SSE Event with its raw JSON data string.
/// This allows PromptLogStream to extract content without depending on axum
/// (axum 0.8 Event does not expose a public data getter).
struct SseItem {
    event: Event,
    json_data: String,
}

/// Convert a stream of `Result<SseItem, Infallible>` into `Result<Event, Infallible>`
/// for consumption by `Sse::new()`.
fn sse_item_to_event<S>(stream: S) -> impl futures::Stream<Item = Result<Event, Infallible>>
where
    S: futures::Stream<Item = Result<SseItem, Infallible>> + Unpin,
{
    futures::stream::unfold(stream, |mut s| async move {
        use futures::StreamExt;
        match s.next().await {
            Some(Ok(item)) => Some((Ok(item.event), s)),
            Some(Err(e)) => Some((Err(e), s)),
            None => None,
        }
    })
}

/// Returns a closure that extracts raw JSON data from an SSE stream item.
fn sse_raw_data_extractor() -> impl FnMut(&Result<SseItem, Infallible>) -> Option<String> + Unpin {
    |item: &Result<SseItem, Infallible>| match item {
        Ok(sse_item) => Some(sse_item.json_data.clone()),
        Err(_) => None,
    }
}

// ═══════════════════════════════════════════════════════════
// KV Index debug endpoint (internal)
// ═══════════════════════════════════════════════════════════

/// GET /internal/kv-index — dump KV index status with hash details (pretty-printed).
pub async fn kv_index_status(
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Debug snapshot of the kvc_aware config currently in effect (helps verify
    // that reload picked up the latest settings).
    let cfg = &state.inner.load().config.router_settings;
    let kvc_cfg = &cfg.kvc_aware;
    let config_snapshot = serde_json::json!({
        "schedule_policy": cfg.schedule_policy,
        "block_size": kvc_cfg.block_size,
        "max_blocks": kvc_cfg.max_blocks,
        "cache_weight": kvc_cfg.cache_weight,
        "tier_weight": kvc_cfg.tier_weight,
        "load_weight": kvc_cfg.load_weight,
        "full_report_hit_threshold": kvc_cfg.full_report_hit_threshold,
        "zmq_endpoints": kvc_cfg.zmq_endpoints,
        "tokenizer_dir": kvc_cfg.tokenizer_dir,
    });

    let kv_index_guard = state.kv_index.load();
    let body = match &**kv_index_guard {
        Some(kv_index) => {
            let block_count = kv_index.block_count();
            let models: Vec<String> = kv_index.model_names().into_iter().collect();
            let dump = kv_index.debug_dump();
            let blocks: Vec<serde_json::Value> = dump
                .into_iter()
                .map(|(model, trie_key, workers, tier, depth)| {
                    serde_json::json!({
                        "model": model,
                        "trie_key": trie_key,
                        "workers": workers,
                        "tier": serde_json::to_value(tier).unwrap_or_default(),
                        "depth": depth,
                    })
                })
                .collect();
            serde_json::json!({
                "status": "active",
                "block_count": block_count,
                "models": models,
                "blocks": blocks,
                "config": config_snapshot,
            })
        }
        None => serde_json::json!({
            "status": "disabled",
            "block_count": 0,
            "models": [],
            "blocks": [],
            "config": config_snapshot,
        }),
    };
    let pretty = serde_json::to_string_pretty(&body).unwrap_or_default();
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        pretty,
    ).into_response()
}

#[cfg(test)]
mod tests {
    use super::{build_gateway_headers, is_vip_key};
    use serde_json::json;

    #[test]
    fn vip_true_in_metadata() {
        let meta = json!({"vip": true});
        assert!(is_vip_key(&meta));
    }

    #[test]
    fn vip_false_in_metadata() {
        let meta = json!({"vip": false});
        assert!(!is_vip_key(&meta));
    }

    #[test]
    fn vip_missing_from_metadata() {
        let meta = json!({"some_other_field": 123});
        assert!(!is_vip_key(&meta));
    }

    #[test]
    fn empty_object_metadata() {
        let meta = json!({});
        assert!(!is_vip_key(&meta));
    }

    #[test]
    fn null_metadata() {
        let meta = json!(null);
        assert!(!is_vip_key(&meta));
    }

    #[test]
    fn vip_is_string_not_bool() {
        let meta = json!({"vip": "true"});
        assert!(!is_vip_key(&meta), "string 'true' should not count as VIP");
    }

    #[test]
    fn vip_is_number_not_bool() {
        let meta = json!({"vip": 1});
        assert!(!is_vip_key(&meta), "numeric 1 should not count as VIP");
    }

    #[test]
    fn headers_enabled_vip_gives_100() {
        let headers = build_gateway_headers(true, true, "/v1/chat/completions", false);
        assert_eq!(headers.get("X-Gateway-Priority").map(String::as_str), Some("100"));
    }

    #[test]
    fn headers_enabled_normal_gives_0() {
        let headers = build_gateway_headers(false, true, "/v1/chat/completions", false);
        assert_eq!(headers.get("X-Gateway-Priority").map(String::as_str), Some("0"));
    }

    #[test]
    fn headers_disabled_vip_is_empty() {
        let headers = build_gateway_headers(true, false, "/v1/chat/completions", false);
        assert!(headers.is_empty(), "no header should be injected when disabled");
    }

    #[test]
    fn headers_disabled_normal_is_empty() {
        let headers = build_gateway_headers(false, false, "/v1/chat/completions", false);
        assert!(headers.is_empty(), "no header should be injected when disabled");
    }

    #[test]
    fn client_type_header_anthropic_on_messages() {
        let headers = build_gateway_headers(false, false, "/v1/messages", true);
        assert_eq!(
            headers.get("X-BooM-Client-Type").map(String::as_str),
            Some("anthropic"),
        );
    }

    #[test]
    fn client_type_header_anonymous_on_chat_completions() {
        let headers = build_gateway_headers(false, false, "/v1/chat/completions", true);
        assert_eq!(
            headers.get("X-BooM-Client-Type").map(String::as_str),
            Some("anonymous"),
        );
    }

    #[test]
    fn client_type_header_omitted_when_disabled() {
        let headers = build_gateway_headers(false, false, "/v1/messages", false);
        assert!(!headers.contains_key("X-BooM-Client-Type"));
    }
}
