use crate::extractor::RequiredAuth;
use crate::request_log::{log_error, log_request, RequestLog};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use boom_core::anthropic::{
    anthropic_request_to_openai, openai_response_to_anthropic, AnthropicStreamTranscoder,
};
use boom_core::provider::RateLimiter;
use boom_core::types::*;
use boom_core::GatewayError;
use boom_flowcontrol::{FlowControlError, FlowControlledStream};
use boom_limiter::{ConcurrencyGuard, GuardedStream, PlanStore, RateLimitPlan};
use boom_promptlog::{PromptLogEntry, PromptLogStream};
use boom_routing::{InFlightGuard, Router};
use futures::StreamExt;
use sqlx::PgPool;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::Instant;

/// Acquire flow control guard for a deployment.
/// Returns Ok(Some(guard)) if acquired, Ok(None) if no slot (pass-through),
/// or Err with appropriate error reply.
async fn acquire_fc_guard<E>(
    state: &AppState,
    deployment_id: &str,
    context_chars: u64,
    is_vip: bool,
    key_alias: Option<String>,
    api_path: &str,
    identity: &boom_core::types::AuthIdentity,
    model: &str,
    is_stream: bool,
    start: Instant,
    request_id: &str,
    err_wrap: impl Fn(GatewayError, bool) -> E,
) -> Result<Option<boom_flowcontrol::FlowControlGuard>, E> {
    let timeout = std::time::Duration::from_secs(1200);
    match state.flow_controller.acquire(deployment_id, context_chars, timeout, is_vip, key_alias).await {
        Ok(g) => Ok(Some(g)),
        Err(FlowControlError::Timeout { waiters, .. }) => {
            let e = GatewayError::FlowControlQueueTimeout {
                deployment_id: deployment_id.to_string(),
                waiters,
                message: format!("Deployment '{}' flow control queue timeout — too many concurrent requests", deployment_id),
            };
            log_error(state, identity, model, api_path, is_stream, start, &e, Some(request_id.to_string()), Some(deployment_id.to_string()), None);
            Err(err_wrap(e, is_stream))
        }
        Err(FlowControlError::NoSlot) => Ok(None),
        Err(FlowControlError::ContextExceeded { deployment_id: _, context_chars, max_context }) => {
            let e = GatewayError::RateLimitExceeded {
                retry_after_secs: None,
                message: format!("Request context ({} chars) exceeds deployment max_context limit ({} chars)", context_chars, max_context),
                limit_type: "flow_control_context",
            };
            log_error(state, identity, model, api_path, is_stream, start, &e, Some(request_id.to_string()), Some(deployment_id.to_string()), None);
            Err(err_wrap(e, is_stream))
        }
    }
}

/// Consecutive failure threshold to trigger auto-disable.
const AUTO_DISABLE_THRESHOLD: u32 = 3;

/// Record a deployment failure: increment the counter and auto-disable if threshold reached.
fn record_deployment_failure(state: &AppState, deployment_id: &Option<String>, model: &str, error: &GatewayError) {
    if !error.is_deployment_failure() {
        return;
    }
    if let Some(ref did) = deployment_id {
        let counter = state.failure_counter
            .entry(did.clone())
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicU32::new(0)));
        let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(
            deployment_id = %did,
            count = count,
            "Deployment failure recorded"
        );
        if count >= AUTO_DISABLE_THRESHOLD {
            let state = state.clone();
            let did = did.clone();
            let model = model.to_string();
            tokio::spawn(async move {
                if let Some(ref pool) = state.db_pool {
                    crate::admin_command::auto_disable_deployment(
                        pool, &state.deployment_store, &did, &model,
                    ).await;
                }
                // Remove counter after auto-disable to prevent repeated triggers.
                state.failure_counter.remove(&did);
            });
        }
    }
}

/// Reset the failure counter for a deployment on success.
fn reset_deployment_failure(state: &AppState, deployment_id: &Option<String>) {
    if let Some(ref did) = deployment_id {
        if let Some(counter) = state.failure_counter.get(did) {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ═══════════════════════════════════════════════════════════
// Debug logging — dump full request/response for specific keys
// ═══════════════════════════════════════════════════════════

const DEBUG_KEY_PATTERN: &str = "fortest";

fn is_debug_key(key_alias: Option<&str>) -> bool {
    key_alias.map(|a| a.contains(DEBUG_KEY_PATTERN)).unwrap_or(false)
}

fn debug_append(label: &str, data: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("reqs.log")
    {
        let ts = chrono::Utc::now().to_rfc3339();
        let _ = writeln!(f, "\n===== {} {} =====\n{}", ts, label, data);
        let _ = f.flush();
    }
}

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

type UsageTracker = std::sync::Arc<std::sync::Mutex<(Option<i32>, Option<i32>)>>;

/// Wrapper stream that writes the request log when dropped (i.e. when the
/// stream has been fully consumed or the connection is torn down).
/// This captures the *real* duration for streaming requests instead of
/// recording the time at which the stream *started*.
struct LoggedStream<S> {
    inner: S,
    pool: Option<PgPool>,
    log: Option<RequestLog>,
    start: Instant,
    usage: UsageTracker,
}

impl<S> LoggedStream<S> {
    fn new(inner: S, pool: Option<PgPool>, log: RequestLog, start: Instant, usage: UsageTracker) -> Self {
        Self {
            inner,
            pool,
            log: Some(log),
            start,
            usage,
        }
    }
}

impl<S> Drop for LoggedStream<S> {
    fn drop(&mut self) {
        if let Some(mut log) = self.log.take() {
            log.duration_ms = Some(self.start.elapsed().as_millis() as i32);
            // Read accumulated usage from tracker (written by stream mapper).
            if let Ok(guard) = self.usage.lock() {
                log.input_tokens = guard.0;
                log.output_tokens = guard.1;
            }
            log_request(self.pool.clone(), log);
        }
    }
}

impl<S: futures::Stream + Unpin> futures::Stream for LoggedStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

// ============================================================
// Chat Completions
// ============================================================

pub async fn chat_completions(
    State(state): State<AppState>,
    auth: RequiredAuth,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<impl IntoResponse, GatewayErrorReply> {
    chat_completions_inner(state, auth, req, "/v1/chat/completions").await
}

/// Shared inner logic for chat completions and legacy completions.
async fn chat_completions_inner(
    state: AppState,
    auth: RequiredAuth,
    req: ChatCompletionRequest,
    api_path: &str,
) -> Result<impl IntoResponse, GatewayErrorReply> {
    let start = Instant::now();
    let request_id = new_request_id();
    let identity = auth.identity();
    let inner = state.inner.load();
    let model = req.model.clone();
    let is_stream = req.stream.unwrap_or(false);

    tracing::info!(request_id = %request_id, model = %model, stream = is_stream, path = api_path, "chat_completions request started");

    let debug = is_debug_key(identity.key_alias.as_deref());
    if debug {
        debug_append("REQUEST", &serde_json::to_string(&req).unwrap_or_default());
    }

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
            log_error(&state, &identity, &model, api_path, is_stream, start, &e, Some(request_id.clone()), None, None);
            GatewayErrorReply(e, false)
        })?;

    // 2. Plan-based or default rate limiting.
    let window_limits: Vec<(u64, u64)> = inner
        .config
        .rate_limit
        .window_limits
        .iter()
        .filter_map(|w| {
            if w.len() >= 2 {
                Some((w[0], w[1]))
            } else {
                None
            }
        })
        .collect();
    let weight = resolve_quota_weight(&req.model, &state);

    let (guard, rl_info) = check_plan_or_default_limits(
        &state.plan_store,
        &state.limiter,
        &identity.key_hash,
        &req.model,
        identity.rpm_limit,
        &window_limits,
        weight,
    )
    .await
    .map_err(|e| {
        log_error(&state, &identity, &model, api_path, is_stream, start, &e, Some(request_id.clone()), None, None);
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
        api_path, &rl_info,
    );

    // 3. Select provider deployment.
    let provider = state
        .router
        .select_provider(&req.model, Some(&identity.key_hash), input_chars as u64)
        .ok_or_else(|| {
            let e = GatewayError::ModelNotFound(req.model.clone());
            log_error(&state, &identity, &model, api_path, is_stream, start, &e, Some(request_id.clone()), None, None);
            rollback_plan_quota(&state.limiter, &rl_info);
            GatewayErrorReply(e, false)
        })?;

    let deployment_id = provider.deployment_id().map(|s| s.to_string());
    let inflight_model = state.router.resolve_model_name(&model);

    // 3.5. Flow control — queue if per-deployment limits exceeded.
    let fc_guard = if let Some(ref did) = deployment_id {
        acquire_fc_guard(
            &state, did, input_chars as u64, is_vip_key(&identity.metadata),
            identity.key_alias.clone(), api_path, &identity, &model,
            is_stream, start, &request_id, GatewayErrorReply,
        ).await?
    } else {
        None
    };

    // Capture request body for debug recording if debug mode is enabled.
    let debug_req_body = if state.debug_store.is_enabled() {
        serde_json::to_string(&req).ok()
    } else {
        None
    };

    // 4. Route to provider (streaming or non-streaming).
    if is_stream {
        let stream = provider.chat_stream(req).await.map_err(|e| {
            log_error(&state, &identity, &model, api_path, true, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone());
            record_deployment_failure(&state, &deployment_id, &model, &e);
            rollback_plan_quota(&state.limiter, &rl_info);
            GatewayErrorReply(e, true)
        })?;
        reset_deployment_failure(&state, &deployment_id);
        let usage = UsageTracker::default();
        let sse_stream = sse_stream_from_chat_stream(stream, usage.clone(), debug);
        let inflight_guard = if let Some(ref did) = deployment_id {
            InFlightGuard::new_for_deployment_with_key(state.inflight.clone(), &inflight_model, did, input_chars as u64, identity.key_alias.as_deref(), &identity.key_hash)
        } else {
            InFlightGuard::new(state.inflight.clone(), &inflight_model, input_chars as u64)
        };
        let tracked = InFlightStream::new(sse_stream, inflight_guard);
        // Wrap with flow control guard (held until stream ends).
        let flow_controlled = match fc_guard {
            Some(g) => FlowControlledStream::new(tracked, g),
            None => FlowControlledStream::passthrough(tracked),
        };
        let guarded = GuardedStream::new(flow_controlled, guard);

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
            deployment_id,
        }, start, usage);

        // Wrap with prompt log stream if enabled, then wrap in Sse.
        if let Some(sender) = prompt_log_sender {
            if let Some(req_body) = prompt_log_req_body {
                let prompt_entry = PromptLogEntry::new(
                    prompt_log_rid.as_deref().unwrap_or_default(),
                    &identity.key_hash,
                    identity.team_alias.as_deref(),
                    prompt_log_model.as_deref().unwrap_or_default(),
                    api_path,
                    true,
                    req_body,
                );
                let prompt_logged = PromptLogStream::new(logged, sender, prompt_entry);
                let response = Sse::new(prompt_logged).keep_alive(KeepAlive::default());
                return Ok(response.into_response());
            }
        }
        let response = Sse::new(logged).keep_alive(KeepAlive::default());
        Ok(response.into_response())
    } else {
        let _inflight = if let Some(ref did) = deployment_id {
            InFlightGuard::new_for_deployment_with_key(state.inflight.clone(), &inflight_model, did, input_chars as u64, identity.key_alias.as_deref(), &identity.key_hash)
        } else {
            InFlightGuard::new(state.inflight.clone(), &inflight_model, input_chars as u64)
        };
        let response = provider.chat(req).await.map_err(|e| {
            log_error(&state, &identity, &model, api_path, false, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone());
            record_deployment_failure(&state, &deployment_id, &model, &e);
            rollback_plan_quota(&state.limiter, &rl_info);
            GatewayErrorReply(e, false)
        })?;
        reset_deployment_failure(&state, &deployment_id);

        let duration_ms = start.elapsed().as_millis() as i32;
        let input_tokens = response.usage.prompt_tokens as i32;
        let output_tokens = response.usage.completion_tokens as i32;

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
                deployment_id,
            },
        );

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
                    identity.team_alias.as_deref(),
                    prompt_log_model.as_deref().unwrap_or_default(),
                    api_path,
                    false,
                    req_body,
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
    Json(req): Json<CompletionRequest>,
) -> Result<impl IntoResponse, GatewayErrorReply> {
    let chat_req = req.into_chat_request();
    chat_completions_inner(state, auth, chat_req, "/v1/completions").await
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

/// Rate-limit check result returned on success.
struct RateLimitInfo {
    /// Name of the plan applied (if any).
    plan_name: Option<String>,
    /// RPM remaining (from the sliding window decision).
    rpm_remaining: u64,
    /// RPM limit.
    rpm_limit: u64,
    /// Current concurrency count for this key.
    concurrency: u32,
    /// Concurrency limit (if plan has one).
    concurrency_limit: Option<u32>,
    /// Info needed to rollback plan window counters if the upstream request fails.
    /// RPM counters are never rolled back (DDoS protection).
    plan_rollback: Option<PlanRollback>,
}

/// Carries the info needed to rollback plan window counters on upstream failure.
struct PlanRollback {
    key: RateLimitKey,
    window_limits: Vec<(u64, u64)>,
    weight: u64,
}

/// Resolve the quota weight for a model, handling alias resolution.
/// If the model is an alias, resolves to the target model first.
/// Returns the `quota_count_ratio` for the resolved model, defaulting to 1.
fn resolve_quota_weight(model: &str, state: &AppState) -> u64 {
    let resolved = state.router.resolve_model(model)
        .unwrap_or_else(|| model.to_string());
    state.deployment_store.get_quota_ratio(&resolved)
}

/// Check if a key has VIP status from metadata.
fn is_vip_key(metadata: &serde_json::Value) -> bool {
    metadata
        .as_object()
        .and_then(|m| m.get("vip"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Rollback plan window counters when an upstream request fails.
/// RPM counters are NOT rolled back (DDoS protection).
fn rollback_plan_quota(limiter: &Arc<boom_limiter::SlidingWindowLimiter>, rl_info: &RateLimitInfo) {
    if let Some(ref rollback) = rl_info.plan_rollback {
        tracing::debug!(
            key = ?rollback.key,
            windows = rollback.window_limits.len(),
            weight = rollback.weight,
            "Rolling back plan window counters (upstream failure)"
        );
        limiter.rollback_plan_windows(&rollback.key, &rollback.window_limits, rollback.weight);
    }
}

/// Check plan-based limits if a plan is assigned, otherwise fall back to
/// default per-model rate limits.
/// `weight` is the quota consumption multiplier for this request.
async fn check_plan_or_default_limits(
    plan_store: &Arc<PlanStore>,
    limiter: &Arc<boom_limiter::SlidingWindowLimiter>,
    key_hash: &str,
    model: &str,
    rpm_limit: Option<u64>,
    window_limits: &[(u64, u64)],
    weight: u64,
) -> Result<(Option<ConcurrencyGuard>, RateLimitInfo), GatewayError> {
    let plan = plan_store
        .resolve_plan(key_hash)
        .or_else(|| plan_store.get_default_plan());

    match plan {
        Some(plan) => {
            let (concurrency_limit, rpm_limit, window_limits, stale_limits) = plan.effective_limits();
            tracing::debug!(
                key_hash = %key_hash,
                plan = %plan.name,
                "Using plan-based rate limits"
            );

            let guard = if let Some(limit) = concurrency_limit {
                Some(plan_store.try_acquire(key_hash, limit).ok_or_else(|| {
                    GatewayError::ConcurrencyExceeded {
                        limit,
                        message: format!(
                            "Concurrency limit exceeded. Plan: {}, Limit: {}",
                            plan.name, limit
                        ),
                    }
                })?)
            } else {
                None
            };

            let rl_key = RateLimitKey {
                key_hash: key_hash.to_string(),
                model: "__plan__".to_string(),
            };

            // Clear stale counters from the other schedule period so the user
            // starts fresh on every schedule switch.
            if !stale_limits.is_empty() {
                limiter.clear_windows(&rl_key, &stale_limits);
            }

            let decision = limiter
                .check_and_record(&rl_key, rpm_limit, &window_limits, weight)
                .await?;

            if !decision.allowed {
                drop(guard);
                let limit_type = match decision.rejected_window_secs {
                    Some(60) | None => ("plan_rpm_limit", format!("Plan '{}' RPM limit exceeded. Limit: {}/min", plan.name, decision.limit)),
                    Some(secs) => ("plan_window_limit", format!("Plan '{}' window limit exceeded. Limit: {} per {}s", plan.name, decision.limit, secs)),
                };
                return Err(GatewayError::RateLimitExceeded {
                    retry_after_secs: decision.retry_after_secs,
                    message: limit_type.1,
                    limit_type: limit_type.0,
                });
            }

            let concurrency = plan_store.get_concurrency(key_hash);
            Ok((guard, RateLimitInfo {
                plan_name: Some(plan.name.clone()),
                rpm_remaining: decision.remaining,
                rpm_limit: decision.limit,
                concurrency,
                concurrency_limit,
                plan_rollback: Some(PlanRollback {
                    key: RateLimitKey {
                        key_hash: key_hash.to_string(),
                        model: "__plan__".to_string(),
                    },
                    window_limits: window_limits.clone(),
                    weight,
                }),
            }))
        }
        None => {
            let rl_key = RateLimitKey {
                key_hash: key_hash.to_string(),
                model: model.to_string(),
            };

            let decision = limiter
                .check_and_record(&rl_key, rpm_limit, window_limits, weight)
                .await?;

            if !decision.allowed {
                let limit_type = match decision.rejected_window_secs {
                    Some(60) | None => ("rpm_limit", format!("RPM limit exceeded. Model: {}, Limit: {}/min", model, decision.limit)),
                    Some(secs) => ("window_limit", format!("Window limit exceeded. Model: {}, Limit: {} per {}s", model, decision.limit, secs)),
                };
                return Err(GatewayError::RateLimitExceeded {
                    retry_after_secs: decision.retry_after_secs,
                    message: limit_type.1,
                    limit_type: limit_type.0,
                });
            }

            Ok((None, RateLimitInfo {
                plan_name: None,
                rpm_remaining: decision.remaining,
                rpm_limit: decision.limit,
                concurrency: 0,
                concurrency_limit: None,
                plan_rollback: if window_limits.is_empty() {
                    None
                } else {
                    Some(PlanRollback {
                        key: RateLimitKey {
                            key_hash: key_hash.to_string(),
                            model: model.to_string(),
                        },
                        window_limits: window_limits.to_vec(),
                        weight,
                    })
                },
            }))
        }
    }
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
    rl_info: &RateLimitInfo,
) {
    let key_display = identity.key_alias.as_deref()
        .or(identity.key_name.as_deref())
        .or(identity.user_id.as_deref())
        .unwrap_or("-");
    let plan_display = rl_info.plan_name.as_deref().unwrap_or("-");
    let concurrency_display = match rl_info.concurrency_limit {
        Some(limit) => format!("{}/{}", rl_info.concurrency, limit),
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
        rl_info.rpm_remaining,
        rl_info.rpm_limit,
        input_chars,
        concurrency_display,
    );
}

fn sse_stream_from_chat_stream(
    stream: ChatStream,
    usage: UsageTracker,
    debug: bool,
) -> impl futures::Stream<Item = Result<Event, Infallible>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(32);
    tokio::spawn(async move {
        let mut tool_arg_buf: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
        let mut stream = std::pin::pin!(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(mut chunk) => {
                    if debug {
                        debug_append("STREAM CHUNK", &serde_json::to_string(&chunk).unwrap_or_default());
                    }
                    if let Some(ref u) = chunk.usage {
                        if let Ok(mut g) = usage.lock() {
                            g.0 = u.prompt_tokens;
                            g.1 = u.completion_tokens;
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
                                    };
                                    let data = serde_json::to_string(&flush_chunk).unwrap_or_default();
                                    if tx.send(Event::default().data(data)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }

                    // Emit the original chunk (arguments already taken).
                    let data = serde_json::to_string(&chunk).unwrap_or_default();
                    if tx.send(Event::default().data(data)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    if debug {
                        debug_append("STREAM ERROR", &e.to_string());
                    }
                    tracing::error!("SSE stream error (OpenAI): {}", e);
                    let error_data =
                        serde_json::to_string(&serde_json::json!({"error": "Upstream error"}))
                            .unwrap_or_default();
                    let _ = tx.send(Event::default().data(error_data)).await;
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
    Json(req): Json<AnthropicMessagesRequest>,
) -> Result<impl IntoResponse, AnthropicErrorReply> {
    let start = Instant::now();
    let request_id = new_request_id();
    let openai_req = anthropic_request_to_openai(&req);
    let identity = auth.identity();
    let inner = state.inner.load();
    let model = openai_req.model.clone();
    let is_stream = openai_req.stream.unwrap_or(false);

    tracing::info!(request_id = %request_id, model = %model, stream = is_stream, "messages request started");

    let debug = is_debug_key(identity.key_alias.as_deref());
    if debug {
        debug_append("REQUEST (anthropic)", &serde_json::to_string(&req).unwrap_or_default());
    }

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
    check_model_access(identity, &openai_req.model, &state.router, &inner.config.general_settings.public_models)
        .map_err(|e| {
            log_error(&state, &identity, &model, "/v1/messages", is_stream, start, &e, Some(request_id.clone()), None, None);
            AnthropicErrorReply(e, is_stream)
        })?;

    // 2. Plan-based or default rate limiting.
    let window_limits: Vec<(u64, u64)> = inner
        .config
        .rate_limit
        .window_limits
        .iter()
        .filter_map(|w| {
            if w.len() >= 2 {
                Some((w[0], w[1]))
            } else {
                None
            }
        })
        .collect();
    let weight = resolve_quota_weight(&openai_req.model, &state);

    let (guard, rl_info) = check_plan_or_default_limits(
        &state.plan_store,
        &state.limiter,
        &identity.key_hash,
        &openai_req.model,
        identity.rpm_limit,
        &window_limits,
        weight,
    )
    .await
    .map_err(|e| {
        log_error(&state, &identity, &model, "/v1/messages", is_stream, start, &e, Some(request_id.clone()), None, None);
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
        "/v1/messages", &rl_info,
    );

    // 3. Select provider deployment.
    let provider = state
        .router
        .select_provider(&openai_req.model, Some(&identity.key_hash), input_chars as u64)
        .ok_or_else(|| {
            let e = GatewayError::ModelNotFound(openai_req.model.clone());
            log_error(&state, &identity, &model, "/v1/messages", is_stream, start, &e, Some(request_id.clone()), None, None);
            rollback_plan_quota(&state.limiter, &rl_info);
            AnthropicErrorReply(e, is_stream)
        })?;

    let deployment_id = provider.deployment_id().map(|s| s.to_string());
    let inflight_model = state.router.resolve_model_name(&model);

    // 3.5. Flow control — queue if per-deployment limits exceeded.
    let fc_guard = if let Some(ref did) = deployment_id {
        acquire_fc_guard(
            &state, did, input_chars as u64, is_vip_key(&identity.metadata),
            identity.key_alias.clone(), "/v1/messages", &identity, &model,
            is_stream, start, &request_id, AnthropicErrorReply,
        ).await?
    } else {
        None
    };

    // Capture request body for debug recording if debug mode is enabled.
    let debug_req_body = if state.debug_store.is_enabled() {
        serde_json::to_string(&req).ok()
    } else {
        None
    };

    // 4. Route to provider.
    if is_stream {
        let stream = provider.chat_stream(openai_req).await.map_err(|e| {
            log_error(&state, &identity, &model, "/v1/messages", true, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone());
            record_deployment_failure(&state, &deployment_id, &model, &e);
            rollback_plan_quota(&state.limiter, &rl_info);
            AnthropicErrorReply(e, true)
        })?;
        reset_deployment_failure(&state, &deployment_id);
        let usage = UsageTracker::default();
        let sse_stream = sse_stream_from_anthropic_chat_stream(stream, model.clone(), usage.clone(), debug);
        let inflight_guard = if let Some(ref did) = deployment_id {
            InFlightGuard::new_for_deployment_with_key(state.inflight.clone(), &inflight_model, did, input_chars as u64, identity.key_alias.as_deref(), &identity.key_hash)
        } else {
            InFlightGuard::new(state.inflight.clone(), &inflight_model, input_chars as u64)
        };
        let tracked = InFlightStream::new(sse_stream, inflight_guard);
        let flow_controlled = match fc_guard {
            Some(g) => FlowControlledStream::new(tracked, g),
            None => FlowControlledStream::passthrough(tracked),
        };
        let guarded = GuardedStream::new(flow_controlled, guard);

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
            deployment_id,
        }, start, usage);

        // Wrap with prompt log stream if enabled, then wrap in Sse.
        if let Some(sender) = prompt_log_sender {
            if let Some(req_body) = prompt_log_req_body {
                let prompt_entry = PromptLogEntry::new(
                    prompt_log_rid.as_deref().unwrap_or_default(),
                    &identity.key_hash,
                    identity.team_alias.as_deref(),
                    prompt_log_model.as_deref().unwrap_or_default(),
                    "/v1/messages",
                    true,
                    req_body,
                );
                let prompt_logged = PromptLogStream::new(logged, sender, prompt_entry);
                let response = Sse::new(prompt_logged).keep_alive(KeepAlive::default());
                return Ok(response.into_response());
            }
        }
        let response = Sse::new(logged).keep_alive(KeepAlive::default());
        Ok(response.into_response())
    } else {
        let _inflight = if let Some(ref did) = deployment_id {
            InFlightGuard::new_for_deployment_with_key(state.inflight.clone(), &inflight_model, did, input_chars as u64, identity.key_alias.as_deref(), &identity.key_hash)
        } else {
            InFlightGuard::new(state.inflight.clone(), &inflight_model, input_chars as u64)
        };
        let response = provider.chat(openai_req).await.map_err(|e| {
            log_error(&state, &identity, &model, "/v1/messages", false, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone());
            record_deployment_failure(&state, &deployment_id, &model, &e);
            rollback_plan_quota(&state.limiter, &rl_info);
            AnthropicErrorReply(e, false)
        })?;
        reset_deployment_failure(&state, &deployment_id);

        let duration_ms = start.elapsed().as_millis() as i32;
        let input_tokens = response.usage.prompt_tokens as i32;
        let output_tokens = response.usage.completion_tokens as i32;

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
                deployment_id,
            },
        );

        let anthropic_resp = openai_response_to_anthropic(&response);

        // Prompt log: capture non-streaming Anthropic response.
        if let Some(sender) = prompt_log_sender {
            if let Some(req_body) = prompt_log_req_body {
                let mut prompt_entry = PromptLogEntry::new(
                    prompt_log_rid.as_deref().unwrap_or_default(),
                    &identity.key_hash,
                    identity.team_alias.as_deref(),
                    prompt_log_model.as_deref().unwrap_or_default(),
                    "/v1/messages",
                    false,
                    req_body,
                );
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
    debug: bool,
) -> impl futures::Stream<Item = Result<Event, Infallible>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);

    tokio::spawn(async move {
        let mut transcoder = AnthropicStreamTranscoder::new(model);
        let mut stream = std::pin::pin!(stream);

        while let Some(result) = stream.next().await {
            match result {
                Ok(chunk) => {
                    if debug {
                        debug_append("STREAM CHUNK (anthropic)", &serde_json::to_string(&chunk).unwrap_or_default());
                    }
                    // Extract usage from the chunk (OpenAI sends usage in the final chunk).
                    if let Some(ref u) = chunk.usage {
                        if let Ok(mut g) = usage.lock() {
                            g.0 = u.prompt_tokens;
                            g.1 = u.completion_tokens;
                        }
                    }
                    let events = transcoder.transcode(&chunk);
                    for ev in &events {
                        if debug {
                            debug_append("ANTHROPIC SSE", &format!("event: {} | data: {}", ev.event, ev.data));
                        }
                    }
                    for ev in events {
                        let axum_event = Event::default()
                            .event(&ev.event)
                            .data(ev.data);
                        if tx.send(axum_event).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    if debug {
                        debug_append("STREAM ERROR (anthropic)", &e.to_string());
                    }
                    let error_data = serde_json::json!({
                        "type": "error",
                        "error": { "type": "api_error", "message": "Upstream error" }
                    });
                    let _ = tx
                        .send(
                            Event::default()
                                .event("error")
                                .data(error_data.to_string()),
                        )
                        .await;
                    return;
                }
            }
        }

        // Flush any held finish events (e.g. if usage chunk never arrived).
        for ev in transcoder.drain() {
            let axum_event = Event::default().event(&ev.event).data(ev.data);
            if tx.send(axum_event).await.is_err() {
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
