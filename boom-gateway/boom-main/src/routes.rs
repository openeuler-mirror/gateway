use crate::extractor::RequiredAuth;
use crate::request_log::{log_error, log_request, RequestDfx, RequestLog};
use crate::state::AppState;
use axum::extract::{ConnectInfo, Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use boom_core::anthropic::{
    anthropic_request_to_openai, openai_response_to_anthropic, AnthropicStreamTranscoder,
};
use boom_core::provider::RateLimiter;
use boom_core::types::*;
use boom_core::GatewayError;
use boom_ctxaware::AgentStatsTracker;
use boom_flowcontrol::{FlowControlError, FlowControlledStream};
use boom_limiter::{ConcurrencyGuard, GuardedStream, PlanStore, RateLimitPlan};
use boom_promptlog::{PromptLogEntry, PromptLogStream};
use boom_routing::{InFlightGuard, Router};
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

/// Fallback local tokenization using the gateway's minijinja-rendered chat
/// template. Used only when a deployment's `/tokenize` endpoint is unavailable;
/// the upstream path is preferred because it matches vLLM's tokens exactly.
fn tokenize_locally(
    state: &AppState,
    resolved_model: &str,
    req: &ChatCompletionRequest,
) -> Vec<u32> {
    let pool_guard = state.tokenizer_pool.load();
    match &**pool_guard {
        Some(pool) => {
            let msgs: Vec<serde_json::Value> = req
                .messages
                .iter()
                .map(|m| serde_json::to_value(m).unwrap_or_default())
                .collect();
            let tools_vals: Option<Vec<serde_json::Value>> = req
                .tools
                .as_ref()
                .map(|t| t.iter().map(|tool| serde_json::to_value(tool).unwrap_or_default()).collect());
            pool.tokenize_openai(resolved_model, &msgs, tools_vals.as_deref()).token_ids
        }
        None => Vec::new(),
    }
}

/// Shared HTTP client for vLLM `/tokenize` pool calls.
static TOKENIZE_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
fn tokenize_client() -> reqwest::Client {
    TOKENIZE_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("tokenize http client")
        })
        .clone()
}

/// Extract the host (no scheme/port/path) from a base URL, to match against
/// worker_ids reported in KV LoadMetrics events (which are bare hosts).
/// Tokenize via a per-model pool of vLLM `/tokenize` endpoints (config:
/// `kvc_aware.vllm_tokenize_endpoints`). Picks the least-loaded endpoint by
/// gateway inflight (inflight/capacity %, same signal as routing) and fails
/// over to the next on any error / 404 / empty result. Returns None if no pool
/// is configured for the model or all endpoints failed (caller falls back to
/// local tokenization).
async fn tokenize_via_vllm_pool(
    state: &AppState,
    resolved_model: &str,
    req: &ChatCompletionRequest,
    candidates: &[Arc<dyn boom_core::provider::Provider>],
) -> Option<Vec<u32>> {
    let endpoints = {
        let inner = state.inner.load();
        inner
            .config
            .router_settings
            .kvc_aware
            .vllm_tokenize_endpoints
            .get(resolved_model)
            .cloned()?
    };
    if endpoints.is_empty() {
        return None;
    }

    // Rank endpoints by gateway inflight load (same signal as kvc routing):
    // map each endpoint's host to the candidate whose kv_worker_id matches,
    // and use its deployment_load (inflight/capacity %). Endpoints with no
    // matching candidate sort last (u64::MAX). This keeps tokenize-pool LB
    // consistent with routing LB and doesn't depend on vLLM LoadMetrics.
    let queue_info: Option<Arc<dyn boom_core::provider::DeploymentQueueInfo>> =
        Some(state.flow_controller.clone());
    let mut ranked: Vec<(u64, String)> = endpoints
        .iter()
        .map(|url| {
            let host = match boom_core::normalize::host_of_api_base(url) {
                Some(h) => h,
                None => return (u64::MAX, url.clone()),
            };
            let load_pct = candidates
                .iter()
                .find(|c| c.kv_worker_id().map(|w| w == host).unwrap_or(false))
                .map(|c| {
                    boom_routing::policy::load_helpers::deployment_load(
                        &state.inflight,
                        &queue_info,
                        resolved_model,
                        c.as_ref(),
                    )
                })
                .unwrap_or(u64::MAX);
            (load_pct, url.clone())
        })
        .collect();
    ranked.sort_by_key(|(lp, _)| *lp);

    let client = tokenize_client();
    // Include tools when present: vLLM's chat template renders tool definitions
    // into the prompt, so /tokenize MUST include them to produce the exact
    // prefill token sequence. Without tools the tokens diverge from vLLM's
    // actual prefill (tool section missing), capping trie match depth.
    let mut body = serde_json::json!({
        "model": resolved_model,
        "messages": req.messages,
        "add_generation_prompt": true,
    });
    if let Some(tools) = &req.tools {
        if !tools.is_empty() {
            if let Ok(v) = serde_json::to_value(tools) {
                body["tools"] = v;
            }
        }
    }

    for (load_pct, base) in ranked {
        let root = base.trim_end_matches('/');
        let root = root.strip_suffix("/v1").unwrap_or(root);
        let url = format!("{root}/tokenize");
        let attempt = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .ok()
            .and_then(|resp| {
                if resp.status().is_success() {
                    // status known; body parsed below
                    Some(resp)
                } else {
                    tracing::debug!(
                        model = resolved_model,
                        url = %url,
                        status = %resp.status(),
                        load_pct = load_pct,
                        "vllm /tokenize endpoint non-2xx, failing over"
                    );
                    None
                }
            });
        let resp = match attempt {
            Some(r) => r,
            None => continue, // network error or non-2xx → next endpoint
        };
        match resp.json::<serde_json::Value>().await {
            Ok(v) => {
                let tokens: Vec<u32> = v
                    .get("tokens")
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_u64().map(|n| n as u32))
                            .collect()
                    })
                    .unwrap_or_default();
                if !tokens.is_empty() {
                    tracing::debug!(
                        model = resolved_model,
                        url = %url,
                        tokens = tokens.len(),
                        first_4_tokens = ?tokens.iter().take(4).copied().collect::<Vec<_>>(),
                        // u64::MAX = no matching candidate (e.g. messages path
                        // passes empty candidates), so load is unknown — don't
                        // log the raw sentinel.
                        load_pct = %if load_pct == u64::MAX {
                            "unknown".to_string()
                        } else {
                            load_pct.to_string()
                        },
                        "tokenized via vllm pool"
                    );
                    return Some(tokens);
                }
            }
            Err(e) => {
                tracing::debug!(
                    model = resolved_model, url = %url, error = %e,
                    "vllm /tokenize bad body, failing over"
                );
            }
        }
    }
    // All endpoints in the pool failed (non-2xx / bad body / empty tokens on
    // every one). Surface as warn so it's visible without debug: tokenization
    // is unavailable, the request will fall back to the local tokenizer (which
    // may diverge from vLLM's tokens) and KV-affinity matching can break.
    tracing::warn!(
        model = resolved_model,
        endpoint_count = endpoints.len(),
        "vllm /tokenize pool exhausted: all endpoints failed, falling back to local tokenizer"
    );
    None
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
struct LoggedStream<S> {
    inner: S,
    pool: Option<PgPool>,
    log: Option<RequestLog>,
    start: Instant,
    first_token_at: Option<Instant>,
    usage: UsageTracker,
    agent_stats: Option<Arc<AgentStatsTracker>>,
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
        }
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
        api_path, &rl_info,
    );

    // 3. KV-cache prefix matching.
    // Resolve candidates first so we can ask a deployment's vLLM to tokenize
    // the prompt via `/tokenize` — those are the EXACT tokens vLLM will prefill
    // (same chat template + content normalization), so the gateway's prefix
    // query matches the trie byte-for-byte, eliminating any rendering drift
    // between the gateway's local tokenizer and vLLM. Tokenization is
    // model-level (identical across deployments of the same model), so any
    // candidate works. Falls back to local tokenization on error.
    let candidates = state.router.candidates_for(&resolved_model);
    // Skip the /tokenize call when the trie has no blocks for this model yet
    // (cold start): an empty trie can't match regardless, so the tokens would
    // be wasted work. BUT we still want this request to trigger a full KV
    // report so vLLM backfills the trie — otherwise the trie can only be
    // populated by someone else's traffic (cold-start chicken-and-egg). The
    // `trie_was_empty` flag forces a full report below regardless of what the
    // policy reports for kv_match_attempted (the empty-token_ids degradation
    // path sets kv_match_attempted=false, which would suppress the report).
    let trie_was_empty = match &**state.kv_index.load() {
        Some(k) => !k.model_names().contains(&resolved_model),
        None => true,
    };
    let token_ids = if trie_was_empty {
        tracing::debug!(
            model = %resolved_model,
            "trie empty for model, skipping tokenize (will force full report)"
        );
        Vec::new()
    } else if let Some(ids) = tokenize_via_vllm_pool(
        &state,
        &resolved_model,
        &req,
        candidates.as_ref().map(|c| c.as_slice()).unwrap_or(&[]),
    )
    .await
    {
        ids
    } else {
        match candidates.as_ref().and_then(|cs| cs.first().cloned()) {
            Some(p) => match p.tokenize_prompt(&req).await {
                Ok(ids) => {
                    tracing::debug!(
                        model = %resolved_model,
                        tokens = ids.len(),
                        via = "candidate_tokenize",
                        "request tokenized"
                    );
                    ids
                }
                Err(e) => {
                    tracing::warn!(
                        model = %resolved_model, error = %e,
                        "upstream /tokenize failed, falling back to local tokenizer"
                    );
                    tokenize_locally(&state, &resolved_model, &req)
                }
            },
            None => tokenize_locally(&state, &resolved_model, &req),
        }
    };

    let selection = state
        .router
        .select_with_candidates(
            &resolved_model,
            candidates.as_ref().map(|c| c.as_slice()).unwrap_or(&[]),
            Some(&identity.key_hash),
            input_chars as u64,
            &token_ids,
        )
        .ok_or_else(|| {
            let e = GatewayError::ModelNotFound(resolved_model.clone());
            log_error(&state, &identity, &model, api_path, is_stream, start, &e, Some(request_id.clone()), None, None, Some(client_ip.clone()));
            rollback_plan_quota(&state.limiter, &rl_info);
            GatewayErrorReply(e, false)
        })?;
    let provider = selection.provider;
    let kv_hit_ratio = selection.kv_hit_ratio;

    let deployment_id = provider.deployment_id().map(|s| s.to_string());
    // DFX: capture scheduling signals for the request log. schedule_policy is
    // the configured policy name (kvc_aware / key_affinity / round_robin);
    // trie_blocks is the KV index fill level at request time; request_tokens
    // is the tokenized prompt length (0 when tokenization was skipped).
    // DFX policy label: configured policy name, suffixed with the actual
    // branch taken when kvc_aware degrades (e.g. "kvc_aware→key_affinity").
    // The dashboard shortens this to "kvc→key" so it's visible at a glance
    // whether a request actually used KV affinity or fell back to sticky.
    let schedule_policy = {
        let base = state.router.policy_name();
        if selection.degraded && base == "kvc_aware" {
            "kvc_aware→key_affinity".to_string()
        } else {
            base
        }
    };
    // DFX raw block counts (stored raw; dashboard derives ratios/percents):
    //   kv_hit_blocks / kv_input_blocks = trie-estimated prefix hit ratio
    //   trie_blocks / trie_max_blocks   = trie capacity fill at request time
    let kv_hit_blocks = selection.kv_hit_blocks;
    let kv_input_blocks = selection.kv_input_blocks;
    let trie_max_blocks = inner.config.router_settings.kvc_aware.max_blocks.max(0) as i64;
    let trie_blocks = match &**state.kv_index.load() {
        Some(k) => Some(k.block_count() as i64),
        None => None,
    };
    let request_tokens = Some(token_ids.len() as i64);
    let inflight_model = state.router.resolve_model_name(&resolved_model);

    // 3.2. Determine KV-cache reporting mode.
    //      Full reporting is requested only when the policy actually queried
    //      the KV index (`kv_match_attempted`) AND the trie is missing enough
    //      of the prefix (hit_ratio < threshold). When KV-aware routing was a
    //      no-op (single candidate, no tokenizer, non-KV policy) we must NOT
    //      ask vLLM for a full report — there is no routing benefit and it
    //      only adds overhead. vLLM then uses its default incremental mode.
    //
    //      EXCEPTION: if the trie was empty for this model at request time
    //      (cold start), force a full report anyway — the empty-token_ids
    //      degradation path leaves kv_match_attempted=false, which would
    //      suppress the report and leave the trie empty forever (only other
    //      users' traffic could populate it). Forcing it here lets this very
    //      request trigger vLLM to backfill the trie.
    req.kv_cache_report_full = trie_was_empty
        || (selection.kv_match_attempted
            && need_full_kv_report(
                true,
                kv_hit_ratio,
                inner.config.router_settings.kvc_aware.full_report_hit_threshold,
            ));
    // Log only when a full KV report is requested (the actionable case — the
    // gateway is asking vLLM to backfill the trie). The common case (no full
    // report, hit was high enough) is silent to avoid per-request noise.
    if req.kv_cache_report_full {
        tracing::info!(
            model = %resolved_model,
            kv_hit_ratio = format!("{:.3}", kv_hit_ratio),
            trie_was_empty,
            deployment = ?deployment_id,
            "KV full report requested (asking vLLM to backfill trie)"
        );
    }

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
        let stream = provider.chat_stream(req).await.map_err(|e| {
            log_error(&state, &identity, &model, api_path, true, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone(), Some(client_ip.clone()));
            crate::health_monitor::record_request_failure(&state, &deployment_id, &e);
            rollback_plan_quota(&state.limiter, &rl_info);
            GatewayErrorReply(e, true)
        })?;
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
        let guarded = GuardedStream::new(flow_controlled, guard);

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
            dfx: Some(RequestDfx {
                schedule_policy: Some(schedule_policy.clone()),
                kv_hit_blocks: Some(kv_hit_blocks as i64),
                kv_input_blocks: Some(kv_input_blocks as i64),
                trie_blocks,
                trie_max_blocks: Some(trie_max_blocks),
                request_tokens,
            }),
        }, start, usage, Some(state.agent_stats.clone()));

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
        let response = provider.chat(req).await.map_err(|e| {
            log_error(&state, &identity, &model, api_path, false, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone(), Some(client_ip.clone()));
            crate::health_monitor::record_request_failure(&state, &deployment_id, &e);
            rollback_plan_quota(&state.limiter, &rl_info);
            GatewayErrorReply(e, false)
        })?;
        crate::health_monitor::reset_request_failure(&state, &deployment_id);

        let duration_ms = start.elapsed().as_millis() as i32;
        let input_tokens = response.usage.prompt_tokens as i32;
        let output_tokens = response.usage.completion_tokens as i32;
        let cached_tokens = response
            .usage
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .map(|c| c as i64);

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
                dfx: Some(RequestDfx {
                    schedule_policy: Some(schedule_policy.clone()),
                    kv_hit_blocks: Some(kv_hit_blocks as i64),
                    kv_input_blocks: Some(kv_input_blocks as i64),
                    trie_blocks,
                    trie_max_blocks: Some(trie_max_blocks),
                    request_tokens,
                }),
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
        "/v1/messages", &rl_info,
    );

    // 3. Select provider deployment.
    // Tokenize for KV-cache prefix matching via vLLM's own /tokenize (exact
    // prefill tokens) so the trie query matches what vLLM caches. The local
    // tokenizer diverged from vLLM after ~16 blocks, capping match depth.
    // No fallback: if no vLLM /tokenize endpoint is configured, token_ids is
    // empty (no trie query).
    //
    // trie_was_empty short-circuit (mirrors the OpenAI path): if the trie has
    // no entry for this model, skip the /tokenize HTTP round-trip — an empty
    // trie matches nothing regardless. trie_was_empty also forces a full KV
    // report below so this very request backfills the trie.
    let trie_was_empty = match &**state.kv_index.load() {
        Some(k) => !k.model_names().contains(&resolved_model),
        None => true,
    };
    let token_ids = if trie_was_empty {
        tracing::debug!(
            model = %resolved_model,
            "trie empty for model, skipping tokenize (will force full report)"
        );
        Vec::new()
    } else {
        tokenize_via_vllm_pool(
            &state,
            &resolved_model,
            &openai_req,
            // candidates aren't resolved until after selection (which needs the
            // token_ids); pass empty — tokenize_via_vllm_pool only uses them for
            // load-based endpoint ranking, not correctness, so endpoints are tried
            // in config order.
            &[],
        )
        .await
        .unwrap_or_default()
    };

    let selection = state
        .router
        .select_provider_with_prefix(&resolved_model, Some(&identity.key_hash), input_chars as u64, &token_ids)
        .ok_or_else(|| {
            let e = GatewayError::ModelNotFound(resolved_model.clone());
            log_error(&state, &identity, &model, "/v1/messages", is_stream, start, &e, Some(request_id.clone()), None, None, Some(client_ip.clone()));
            rollback_plan_quota(&state.limiter, &rl_info);
            AnthropicErrorReply(e, is_stream)
        })?;
    let provider = selection.provider;
    let kv_hit_ratio = selection.kv_hit_ratio;
    let kv_match_attempted = selection.kv_match_attempted;

    let deployment_id = provider.deployment_id().map(|s| s.to_string());
    let inflight_model = state.router.resolve_model_name(&resolved_model);
    // DFX: scheduling signals for the request log (mirror the OpenAI path).
    // DFX policy label: configured policy name, suffixed with the actual
    // branch taken when kvc_aware degrades (e.g. "kvc_aware→key_affinity").
    // The dashboard shortens this to "kvc→key" so it's visible at a glance
    // whether a request actually used KV affinity or fell back to sticky.
    let schedule_policy = {
        let base = state.router.policy_name();
        if selection.degraded && base == "kvc_aware" {
            "kvc_aware→key_affinity".to_string()
        } else {
            base
        }
    };
    // DFX raw block counts (stored raw; dashboard derives ratios/percents):
    //   kv_hit_blocks / kv_input_blocks = trie-estimated prefix hit ratio
    //   trie_blocks / trie_max_blocks   = trie capacity fill at request time
    let kv_hit_blocks = selection.kv_hit_blocks;
    let kv_input_blocks = selection.kv_input_blocks;
    let trie_max_blocks = inner.config.router_settings.kvc_aware.max_blocks.max(0) as i64;
    let trie_blocks = match &**state.kv_index.load() {
        Some(k) => Some(k.block_count() as i64),
        None => None,
    };
    let request_tokens = Some(token_ids.len() as i64);

    // 3.2. Determine KV-cache reporting mode (same logic as OpenAI path):
    //      only request a full report when KV-aware routing actually queried
    //      the index (kv_match_attempted) AND the trie is missing the prefix.
    openai_req.kv_cache_report_full = trie_was_empty
        || (kv_match_attempted
            && need_full_kv_report(
                true,
                kv_hit_ratio,
                inner.config.router_settings.kvc_aware.full_report_hit_threshold,
            ));

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
        let stream = provider.chat_stream(openai_req).await.map_err(|e| {
            log_error(&state, &identity, &model, "/v1/messages", true, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone(), Some(client_ip.clone()));
            crate::health_monitor::record_request_failure(&state, &deployment_id, &e);
            rollback_plan_quota(&state.limiter, &rl_info);
            AnthropicErrorReply(e, true)
        })?;
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
        let guarded = GuardedStream::new(flow_controlled, guard);

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
            dfx: Some(RequestDfx {
                schedule_policy: Some(schedule_policy.clone()),
                kv_hit_blocks: Some(kv_hit_blocks as i64),
                kv_input_blocks: Some(kv_input_blocks as i64),
                trie_blocks,
                trie_max_blocks: Some(trie_max_blocks),
                request_tokens,
            }),
        }, start, usage, Some(state.agent_stats.clone()));

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
        let response = provider.chat(openai_req).await.map_err(|e| {
            log_error(&state, &identity, &model, "/v1/messages", false, start, &e, Some(request_id.clone()), deployment_id.clone(), debug_req_body.clone(), Some(client_ip.clone()));
            crate::health_monitor::record_request_failure(&state, &deployment_id, &e);
            rollback_plan_quota(&state.limiter, &rl_info);
            AnthropicErrorReply(e, false)
        })?;
        crate::health_monitor::reset_request_failure(&state, &deployment_id);

        let duration_ms = start.elapsed().as_millis() as i32;
        let input_tokens = response.usage.prompt_tokens as i32;
        let output_tokens = response.usage.completion_tokens as i32;
        let cached_tokens = response
            .usage
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .map(|c| c as i64);

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
                dfx: Some(RequestDfx {
                    schedule_policy: Some(schedule_policy.clone()),
                    kv_hit_blocks: Some(kv_hit_blocks as i64),
                    kv_input_blocks: Some(kv_input_blocks as i64),
                    trie_blocks,
                    trie_max_blocks: Some(trie_max_blocks),
                    request_tokens,
                }),
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
