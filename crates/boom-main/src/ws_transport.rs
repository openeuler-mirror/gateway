//! WebSocket transport for the Responses API (`GET /v1/responses` upgrade).
//!
//! Protocol aligned with vLLM's agentic-api (`supports_websockets` — what
//! Codex uses): the client sends `{"type":"response.create", "stream_id"?,
//! ...ResponsesRequest}` text frames; the server relays the ordinary
//! Responses SSE event stream back as text frames with a `stream_id`
//! routing member attached. No new event vocabulary — the WS layer is a
//! pure transport adapter over the existing HTTP handler.
//!
//! Design invariants (docs/websocket-responses-transport-design.md §5):
//! - a rate-limited create holds no capacity (limit checked before admission)
//! - same-`stream_id` requests run strictly FIFO (previous_response_id
//!   chains never race); different ids run in parallel (bounded)
//! - oversized outbound frames never enter the queue
//! - `payload.stream` is forced true; `store` semantics untouched
//! - a stalled socket (send timeout) is disconnected, never frozen
//! - a panicking lane runner releases its lane (error frame + budget
//!   release), never wedges the stream_id
//! - connection limits (`general_settings.responses_ws`) bound everything:
//!   outstanding requests, in-flight bytes, lanes, per-key connections

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::ConnectInfo;
use axum::response::Response;
use axum::extract::State;
use boom_core::GatewayError;
use futures::{FutureExt, SinkExt, StreamExt, TryStreamExt};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use crate::extractor::RequiredAuth;
use crate::hooks::PreAuthOutcome;
use crate::request_log::log_auth_error;
use crate::routes;
use crate::state::AppState;
use boom_core::types::AuthIdentity;

/// Max chars of a client-supplied stream_id (agentic-api: 256).
const MAX_STREAM_ID_CHARS: usize = 256;
/// Inbound frame ceiling — the HTTP ingress cap, on purpose: a WS
/// `response.create` is the transport twin of a POST body, so both paths
/// reject the same oversized-request class with the same budget. Enforced
/// twice: at the upgrade (`max_message_size`, transport-level, design
/// §1.2) and here before any JSON parse (defense against fragmented
/// messages that reassemble under the wire cap).
const MAX_REQUEST_FRAME_BYTES: usize = crate::extractor::MAX_REQUEST_BODY_BYTES;

/// Entry point: route registered at `GET /v1/responses` alongside the POST
/// handler — axum dispatches here only when the request carries a
/// WebSocket upgrade (`Upgrade: websocket`); plain GETs never match this
/// extractor and fall through to the method-not-allowed path.
/// Identity plus the raw key, so every frame can recheck revocation
/// (P1: a key blocked mid-connection stops serving new creates; in-flight
/// generations finish — same window HTTP has between cache entries).
#[derive(Clone)]
struct SessionPrincipal {
    identity: AuthIdentity,
    raw_key: String,
}

pub async fn responses_ws(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> Response {
    // Rollout switch first — while disabled the endpoint does not exist
    // (404), so merging the transport never flips production traffic over
    // unannounced.
    let ws_cfg = state
        .inner
        .load()
        .config
        .general_settings
        .responses_ws
        .clone()
        .unwrap_or_default();
    if !ws_cfg.enabled {
        return handshake_reject(404);
    }

    // Transport-level frame caps BEFORE the upgrade (design §1.2): frames
    // over budget are rejected by tungstenite and never reach JSON
    // parsing. One request frame ≈ one HTTP body, so the HTTP ingress cap
    // is the right ceiling for both.
    let ws = ws
        .max_message_size(MAX_REQUEST_FRAME_BYTES)
        .max_frame_size(MAX_REQUEST_FRAME_BYTES);

    let key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        });
    let raw_key = match key {
        Some(k) => k,
        None => return handshake_reject(401),
    };

    // pre_auth hook at the handshake (model unknown yet — the per-frame
    // hook re-runs with it). Same resolution as the HTTP extractor:
    // hook → effective key → authenticate, failures logged into the
    // request audit table.
    let start = Instant::now();
    let user_tags = routes::extract_user_tags(&state.user_tag_store.get(), &headers);
    let client_ip = Some(routes::extract_client_ip(&headers, None));
    let inner = state.inner.load();
    let hook_result = inner.hooks.pre_auth(&raw_key, &headers, None);
    let effective_key = match hook_result.outcome {
        PreAuthOutcome::NoHook | PreAuthOutcome::Continue => raw_key.clone(),
        PreAuthOutcome::Replace(new_key) => new_key,
        // No model at handshake — a ReplaceModel still swaps the key; the
        // rewrite itself is applied per frame where the model is known.
        PreAuthOutcome::ReplaceModel { new_key, .. } => new_key,
        PreAuthOutcome::Reject(reason) => {
            let e = GatewayError::AuthError(reason);
            log_auth_error(
                &state, &raw_key, "/v1/responses", start, &e,
                Some(uuid::Uuid::new_v4().to_string()), client_ip, user_tags,
            );
            return handshake_reject(401);
        }
        PreAuthOutcome::Deny => {
            let e = GatewayError::InternalError("pre_auth hook failure (deny mode)".into());
            log_auth_error(
                &state, &raw_key, "/v1/responses", start, &e,
                Some(uuid::Uuid::new_v4().to_string()), client_ip, user_tags,
            );
            return handshake_reject(500);
        }
    };
    let identity = match inner.auth.authenticate(&effective_key).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "websocket handshake auth failed");
            log_auth_error(
                &state, &effective_key, "/v1/responses", start, &e,
                Some(uuid::Uuid::new_v4().to_string()), client_ip, user_tags,
            );
            return handshake_reject(e.status_code());
        }
    };
    drop(inner);

    // Per-key concurrent connection cap (per instance).
    let conn_guard = match WsConnectionGuard::acquire(&identity.key_hash, ws_cfg.max_connections_per_key) {
        Some(g) => g,
        None => {
            tracing::info!(
                key_alias = identity.key_alias.as_deref().unwrap_or("-"),
                limit = ws_cfg.max_connections_per_key,
                "websocket handshake rejected: per-key connection limit"
            );
            return handshake_reject(429);
        }
    };

    ws.on_upgrade(move |socket| {
        Box::pin(session_loop(
            socket,
            state,
            headers,
            remote_addr,
            SessionPrincipal { identity, raw_key },
            ws_cfg,
            conn_guard,
        ))
    })
}

/// Per-instance registry behind `max_connections_per_key`. Connections are
/// instance-local state (no cross-instance session), so a process-global
/// map is the whole story.
fn connection_registry() -> &'static dashmap::DashMap<String, usize> {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<dashmap::DashMap<String, usize>> = OnceLock::new();
    REGISTRY.get_or_init(dashmap::DashMap::new)
}

/// RAII slot in [`connection_registry`]: acquire bumps the key's count
/// (rejecting the handshake at the cap), Drop releases it when the session
/// loop ends — however it ends.
struct WsConnectionGuard {
    key_hash: String,
}

impl WsConnectionGuard {
    fn acquire(key_hash: &str, max_per_key: usize) -> Option<Self> {
        let registry = connection_registry();
        // The shard lock is held across check+increment — concurrent
        // handshakes of the same key serialize on it, no check-then-act race.
        let mut entry = registry.entry(key_hash.to_string()).or_insert(0);
        if *entry >= max_per_key {
            return None;
        }
        *entry += 1;
        drop(entry);
        Some(Self { key_hash: key_hash.to_string() })
    }
}

impl Drop for WsConnectionGuard {
    fn drop(&mut self) {
        let registry = connection_registry();
        if let Some(mut entry) = registry.get_mut(&self.key_hash) {
            *entry -= 1;
            if *entry == 0 {
                drop(entry);
                registry.remove(&self.key_hash);
            }
        }
    }
}

fn handshake_reject(status: u16) -> Response {
    // The upgrade already got a 101 in axum's flow by the time on_upgrade
    // runs; this branch only covers pre-upgrade failures, rendered as a
    // plain status response.
    use axum::response::IntoResponse;
    let code = axum::http::StatusCode::from_u16(status)
        .unwrap_or(axum::http::StatusCode::FORBIDDEN);
    (code, "websocket handshake rejected").into_response()
}

// ---------------------------------------------------------------------------
// Outbound events
// ---------------------------------------------------------------------------

/// One serialized, size-checked outbound frame. Built BEFORE entering the
/// bounded queue so an oversized event is rejected at the source. The cap
/// is `responses_ws.max_stream_event_bytes`, threaded from session config.
#[derive(Debug)]
struct WsOutboundEvent(String);

impl WsOutboundEvent {
    fn build(
        mut value: serde_json::Value,
        stream_id: Option<&str>,
        max_event_bytes: usize,
    ) -> Result<Self, String> {
        if let Some(obj) = value.as_object_mut() {
            if let Some(id) = stream_id {
                obj.insert("stream_id".to_string(), serde_json::Value::String(id.to_string()));
            }
        }
        let text = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        if text.len() > max_event_bytes {
            return Err(format!("event exceeds {max_event_bytes} bytes"));
        }
        Ok(Self(text))
    }
}

fn error_frame(status: u16, code: &str, message: String, stream_id: Option<&str>) -> WsOutboundEvent {
    let mut v = serde_json::json!({
        "type": "error",
        "status": status,
        "error": { "message": message, "type": "invalid_request_error", "code": code },
    });
    if let (Some(obj), Some(id)) = (v.as_object_mut(), stream_id) {
        obj.insert("stream_id".to_string(), serde_json::Value::String(id.to_string()));
    }
    // Error frames are small by construction; a serialization failure here
    // is impossible in practice — fall back to a minimal frame.
    WsOutboundEvent(serde_json::to_string(&v).unwrap_or_else(|_| "{\"type\":\"error\"}".into()))
}

// ---------------------------------------------------------------------------
// Lanes / admission
// ---------------------------------------------------------------------------

struct WsCreate {
    inner: boom_core::responses::ResponsesRequest,
    stream_id: Option<String>,
}

/// `generate: false` — accept-and-store without inference. The mock-free
/// contract (agentic-api local.rs): synthesize the lifecycle pair around
/// the client's own input items, persist the checkpoint, relay the two
/// events. This is how a client parks state on the store over WS.
async fn complete_without_inference(
    state: &AppState,
    identity: &AuthIdentity,
    create: &WsCreate,
    tx: &tokio::sync::mpsc::Sender<WsOutboundEvent>,
    max_event_bytes: usize,
) {
    let inner = create.inner.clone();
    let sid = create.stream_id.as_deref();
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let input_items: Vec<serde_json::Value> = match &inner.input {
        boom_core::responses::ResponsesInput::Text(t) => {
            vec![serde_json::json!({ "role": "user", "content": t })]
        }
        boom_core::responses::ResponsesInput::Items(items) => items.clone(),
    };
    let shell = |status: &str, seq: u64| {
        serde_json::json!({
            "type": if seq == 0 { "response.created" } else { "response.completed" },
            "sequence_number": seq,
            "response": {
                "id": response_id, "object": "response", "created_at": created_at,
                "status": status, "model": inner.model, "output": [],
                "usage": { "input_tokens": 0, "output_tokens": 0, "total_tokens": 0 },
            },
        })
    };
    // Persist the checkpoint so the id is a continuation point — the whole
    // point of a non-generating create. Store unavailable → skip silently:
    // the events still relay, the id just isn't continuable (same semantic
    // as HTTP with store disabled).
    // Same gate as the HTTP path: ResponsesStoreSettings.enabled AND
    // (force_persist OR store != false). The WS branch used to check only
    // the pool, diverging from HTTP when the flag is toggled off or
    // force_persist is on.
    let store_cfg = state.inner.load().config.general_settings.responses_store.clone().unwrap_or_default();
    let store_ok = state.response_store.is_enabled() && store_cfg.enabled
        && (store_cfg.force_persist || inner.store != Some(false));
    if store_ok {
        let history: Vec<serde_json::Value> = match &inner.previous_response_id {
            Some(prev) => state
                .response_store
                .rehydrate(prev, &identity.key_hash)
                .await
                .map(|t| t.items)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let mut items = history;
        items.extend(input_items);
        let turn = boom_responses::PersistTurn {
            response_id: response_id.clone(),
            previous_response_id: inner.previous_response_id.clone(),
            key_hash: identity.key_hash.clone(),
            model: inner.model.clone(),
            instructions: inner.instructions.clone(),
            tools: inner.tools.clone(),
            items,
            status: "completed".to_string(),
            response_json: shell("completed", 1)["response"].clone(),
            ttl_secs: store_cfg.ttl_secs,
            max_history_items: store_cfg.max_history_items,
        };
        if let Err(e) = state.response_store.persist_turn(turn).await {
            tracing::warn!(error = %e, "ws generate:false persist failed");
        }
    }
    for seq in [0u64, 1] {
        let ev = shell(if seq == 0 { "in_progress" } else { "completed" }, seq);
        let _ = tx.send(WsOutboundEvent::build(ev, sid, max_event_bytes).unwrap_or_else(|e| {
            error_frame(500, "server_error", e, sid)
        })).await;
    }
}

struct WorkItem {
    request: WsCreate,
    input_bytes: usize,
    /// `generate: false` — local accept-and-store completion, no upstream
    /// inference. The flag lives here, not in `request.inner`: it is a
    /// transport-layer member, stripped from the body before parsing.
    generate: bool,
}

/// Per-connection state: byte budget + per-lane FIFO queues. All three
/// ceilings come from `general_settings.responses_ws` (session config) —
/// no hardcoded limits.
///
/// (P1 lane-continuation cache — skipping the DB rehydrate for same-lane
/// follow-ups — is deferred: it needs the handler kernel extraction so the
/// WS layer can inject pre-rehydrated history. The DB round trip remains.)
struct Multiplexer {
    lanes: HashMap<Option<String>, VecDeque<WorkItem>>,
    inflight: usize,
    queued: usize,
    bytes_used: usize,
    max_outstanding_requests: usize,
    max_outstanding_bytes: usize,
    max_lanes: usize,
}

impl Multiplexer {
    fn new(cfg: &boom_config::ResponsesWsSettings) -> Self {
        Self {
            lanes: HashMap::new(),
            inflight: 0,
            queued: 0,
            bytes_used: 0,
            max_outstanding_requests: cfg.max_outstanding_requests,
            max_outstanding_bytes: cfg.max_outstanding_bytes,
            max_lanes: cfg.max_session_lanes,
        }
    }

    fn has_capacity(&self, input_bytes: usize) -> bool {
        self.inflight + self.queued < self.max_outstanding_requests
            && self.bytes_used + input_bytes <= self.max_outstanding_bytes
    }

    fn lane_available(&self, lane: &Option<String>) -> bool {
        self.lanes.contains_key(lane) || self.lanes.len() < self.max_lanes
    }

    /// Returns Some(item) when the item should start executing now (its lane
    /// was idle), None when it was queued behind a running request.
    fn admit(&mut self, item: WorkItem) -> Option<WorkItem> {
        self.bytes_used += item.input_bytes;
        let lane = item.request.stream_id.clone();
        match self.lanes.get_mut(&lane) {
            Some(queue) => {
                queue.push_back(item);
                self.queued += 1;
                None
            }
            None => {
                // Register the lane with an empty backlog; the caller runs
                // the item and calls `complete` when done.
                self.lanes.insert(lane, VecDeque::new());
                self.inflight += 1;
                Some(item)
            }
        }
    }

    /// A request finished: release its budget and start the next one on its
    /// lane, if any (FIFO).
    fn complete(&mut self, lane: &Option<String>, input_bytes: usize) -> Option<WorkItem> {
        self.bytes_used = self.bytes_used.saturating_sub(input_bytes);
        self.inflight -= 1;
        let next = self
            .lanes
            .get_mut(lane)
            .and_then(|q| q.pop_front());
        match next {
            Some(item) => {
                self.queued -= 1;
                self.inflight += 1;
                Some(item)
            }
            None => {
                self.lanes.remove(lane);
                None
            }
        }
    }

    fn discard_all(&mut self) {
        self.lanes.clear();
        self.queued = 0;
        self.bytes_used = 0;
    }
}

// ---------------------------------------------------------------------------
// Session loop
// ---------------------------------------------------------------------------

async fn session_loop(
    socket: WebSocket,
    state: AppState,
    headers: axum::http::HeaderMap,
    remote_addr: SocketAddr,
    principal: SessionPrincipal,
    ws_cfg: boom_config::ResponsesWsSettings,
    _conn_guard: WsConnectionGuard,
) {
    let identity = principal.identity.clone();
    tracing::info!(
        key_alias = identity.key_alias.as_deref().unwrap_or("-"),
        "websocket responses session open"
    );
    // Session heartbeat stat: one INFO line per minute while open, so a
    // long-lived WS session is visible in gateway.log without waiting for
    // close (the open-session log blind spot).
    let served = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut stat_tick = tokio::time::interval(std::time::Duration::from_secs(60));
    stat_tick.tick().await; // consume the immediate first tick
    let (mut sink, mut source) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<WsOutboundEvent>(ws_cfg.outbound_buffer);
    let send_timeout = std::time::Duration::from_millis(ws_cfg.send_timeout_ms);
    let mut mux = Multiplexer::new(&ws_cfg);
    // JoinSet of per-lane runner tasks; each runner executes creates
    // serially for its lane until the backlog is empty.
    let mut runners: tokio::task::JoinSet<Option<(Option<String>, usize)>> = Default::default();
    let mut draining = false;

    loop {
        tokio::select! {
            _ = stat_tick.tick() => {
                let served_requests = served.load(std::sync::atomic::Ordering::Relaxed);
                tracing::info!(
                    served_requests,
                    active_lanes = mux.lanes.len(),
                    inflight = mux.inflight,
                    queued = mux.queued,
                    "websocket responses session stat"
                );
            }
            // Completed lane-runner: it returns the lane + input bytes it
            // just finished; None means the lane's backlog is exhausted.
            Some(joined) = runners.join_next(), if !runners.is_empty() => {
                match joined {
                    Ok(Some((lane, input_bytes))) => {
                        if let Some(next) = mux.complete(&lane, input_bytes) {
                            spawn_runner_inner(&mut runners, state.clone(), tx.clone(), headers.clone(), remote_addr, principal.raw_key.clone(), next, served.clone(), &ws_cfg);
                        }
                    }
                    Ok(None) => {} // backlog drained inside the runner
                    // Defensive only: runner panics are contained inside
                    // guard_runner (which releases the lane itself). A
                    // JoinError here carries no lane/bytes, so the lane
                    // would be unrecoverable — log loudly if it ever fires.
                    Err(e) => { tracing::warn!(error = %e, "ws lane runner escaped panic guard"); }
                }
            }
            frame = rx.recv() => {
                let Some(frame) = frame else { break };
                let ok = tokio::time::timeout(
                    send_timeout,
                    sink.send(Message::Text(frame.0.into())),
                ).await.map_or(false, |r| r.is_ok());
                if !ok {
                    tracing::warn!("websocket send stalled; dropping connection");
                    break;
                }
            }
            message = source.next() => {
                let Some(Ok(msg)) = message else { break };
                match msg {
                    Message::Text(_) if draining => {}
                    Message::Text(text) => {
                        match handle_client_text(
                            &state, &tx, &mut mux, &mut runners, &headers, remote_addr,
                            &identity, &principal.raw_key, &served, text.as_str(), &ws_cfg,
                        ).await {
                            Flow::Continue => {}
                        }
                    }
                    Message::Binary(_) => {
                        let _ = tx.try_send(error_frame(400, "invalid_request_error",
                            "websocket messages must be JSON text frames".into(), None));
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                // The process-wide shutdown signal is handled by main's
                // listener; per-connection we simply begin draining so the
                // close is orderly.
                draining = true;
                mux.discard_all();
            }
        }
        if draining && runners.is_empty() {
            break;
        }
    }
    let _ = sink.close().await;
    tracing::info!(
        key_alias = identity.key_alias.as_deref().unwrap_or("-"),
        served_requests = served.load(std::sync::atomic::Ordering::Relaxed),
        lanes_remaining = mux.lanes.len(),
        "websocket responses session closed"
    );
}

enum Flow { Continue }

#[allow(clippy::too_many_arguments)]
async fn handle_client_text(
    state: &AppState,
    tx: &tokio::sync::mpsc::Sender<WsOutboundEvent>,
    mux: &mut Multiplexer,
    runners: &mut tokio::task::JoinSet<Option<(Option<String>, usize)>>,
    headers: &axum::http::HeaderMap,
    remote_addr: SocketAddr,
    identity: &AuthIdentity,
    raw_key: &str,
    served: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    text: &str,
    ws_cfg: &boom_config::ResponsesWsSettings,
) -> Flow {
    // Frame ceiling BEFORE any parse (the second enforcement layer over
    // the upgrade's max_message_size — a fragmented message reassembles
    // under the wire cap but must not burn JSON parsing either). Same
    // budget class as the HTTP ingress cap: a create frame is a POST body.
    if text.len() > MAX_REQUEST_FRAME_BYTES {
        let _ = tx.try_send(error_frame(400, "context_length_exceeded",
            format!("request frame of {} bytes exceeds the gateway request limit of {MAX_REQUEST_FRAME_BYTES} bytes", text.len()),
            None));
        return Flow::Continue;
    }
    // Discriminator-only probe first (agentic-api pattern): only
    // response.create is accepted at P0.
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.try_send(error_frame(400, "invalid_json", e.to_string(), None));
            return Flow::Continue;
        }
    };
    let stream_id: Option<String> = value.get("stream_id").and_then(|v| v.as_str()).map(str::to_string);
    if let Some(id) = &stream_id {
        if id.chars().count() > MAX_STREAM_ID_CHARS {
            let _ = tx.try_send(error_frame(400, "invalid_request_error",
                format!("stream_id must be at most {MAX_STREAM_ID_CHARS} characters"), Some(id)));
            return Flow::Continue;
        }
    }
    if value.get("type").and_then(|v| v.as_str()) != Some("response.create") {
        let _ = tx.try_send(error_frame(400, "invalid_request_error",
            "websocket message type must be response.create".into(), stream_id.as_deref()));
        return Flow::Continue;
    }

    let mut value = value;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("type");
        obj.remove("stream_id");
        obj.remove("generate"); // transport member: never reach ResponsesRequest.extra
    }
    let mut request: boom_core::responses::ResponsesRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.try_send(error_frame(400, "invalid_request_error", e.to_string(), stream_id.as_deref()));
            return Flow::Continue;
        }
    };
    // generate:false → accept-and-store without inference (P1).
    let generate = value_generate(text);
    request.stream = Some(true); // forced: a non-streaming create is meaningless over WS

    let input_bytes = text.len();
    if !mux.lane_available(&stream_id) {
        let _ = tx.try_send(error_frame(429, "rate_limit_exceeded",
            format!("too many concurrent streams (max {})", mux.max_lanes), stream_id.as_deref()));
        return Flow::Continue;
    }
    if !mux.has_capacity(input_bytes) {
        let _ = tx.try_send(error_frame(429, "rate_limit_exceeded",
            "too many outstanding websocket response.create requests".into(), stream_id.as_deref()));
        return Flow::Continue;
    }

    // Rate-limit preflight BEFORE admission (design §5.1: a rate-limited
    // create holds no capacity). Peek-only, same plan resolution as the
    // HTTP handler; the authoritative check+commit still runs later (in
    // routes::responses for inference creates, responses_charge_local for
    // generate:false). A rejected frame never occupies a lane slot or
    // byte budget.
    if let Err(e) = routes::responses_rate_preflight(state, identity, &request.model).await {
        let _ = tx.try_send(error_frame(e.status_code(), "rate_limit_exceeded",
            e.to_string(), stream_id.as_deref()));
        return Flow::Continue;
    }

    if generate == Some(false) {
        // Local completion counts against the same admission budgets as
        // inference creates (same lane machinery, `MAX_OUTSTANDING_*` and
        // per-lane FIFO apply): the persist alone is two DB round-trips
        // plus up to max_history_items writes — an unbounded spawn is an
        // unbounded write amplifier, and lane FIFO also serializes same-
        // stream_id stores against generating creates on the chain.
        let item = WorkItem { request: WsCreate { inner: request, stream_id }, input_bytes, generate: false };
        let ready = mux.admit(item);
        if let Some(ready) = ready {
            spawn_runner_inner(runners, state.clone(), tx.clone(), headers.clone(), remote_addr, raw_key.to_string(), ready, served.clone(), ws_cfg);
        }
        return Flow::Continue;
    }

    let item = WorkItem { request: WsCreate { inner: request, stream_id }, input_bytes, generate: true };
    let ready = mux.admit(item);
    if let Some(ready) = ready {
        spawn_runner_inner(runners, state.clone(), tx.clone(), headers.clone(), remote_addr, raw_key.to_string(), ready, served.clone(), ws_cfg);
    }
    Flow::Continue
}

/// Extract top-level `generate` (bool) from the raw frame text — it is a
/// transport-layer member like type/stream_id, stripped before the body
/// parse, so we must probe the original text.
fn value_generate(text: &str) -> Option<bool> {
    #[derive(serde::Deserialize)]
    struct Probe {
        generate: Option<bool>,
    }
    serde_json::from_str::<Probe>(text).ok().and_then(|p| p.generate)
}

/// A lane runner executes its item, then keeps draining the lane's backlog
/// (returned as None when empty) — the loop only re-spawns on completion
/// messages, so FIFO order within a lane is preserved end to end. The
/// future is panic-contained (see `guard_runner`): a panicking runner
/// releases its lane instead of wedging it.
#[allow(clippy::too_many_arguments)]
fn spawn_runner_inner(
    runners: &mut tokio::task::JoinSet<Option<(Option<String>, usize)>>,
    state: AppState,
    tx: tokio::sync::mpsc::Sender<WsOutboundEvent>,
    headers: axum::http::HeaderMap,
    remote_addr: SocketAddr,
    raw_key: String,
    item: WorkItem,
    served: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ws_cfg: &boom_config::ResponsesWsSettings,
) {
    let stream_id = item.request.stream_id.clone();
    let input_bytes = item.input_bytes;
    let ws_cfg = ws_cfg.clone();
    runners.spawn(guard_runner(tx.clone(), stream_id, input_bytes, async move {
        served.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if item.generate {
            execute_create(&state, &tx, headers, remote_addr, raw_key, item, &ws_cfg).await
        } else {
            store_only_runner(&state, &tx, headers, raw_key, item, &ws_cfg).await
        }
    }));
}

/// Per-frame authentication — the WS transport bypasses the RequiredAuth
/// extractor, so the hook resolution the HTTP path gets for free runs
/// here: same order (pre_auth hook → effective key → authenticate), same
/// failure logging into the request audit table (`log_auth_error` — WS
/// auth failures must be as visible as HTTP ones), and hook-constructed
/// headers applied to the forwarded HeaderMap so user_tags attribution,
/// prompt-log capture and the client-header whitelist see them exactly as
/// on HTTP. Runs per frame with the frame's `model`, so `ReplaceModel`
/// works: the returned identity + new_model rebuild a RequiredAuth that
/// drives the same handler-side rewrite `check_model_access` depends on.
async fn frame_auth(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    raw_key: &str,
    model: Option<&str>,
) -> Result<(AuthIdentity, Option<String>, axum::http::HeaderMap), GatewayError> {
    let start = Instant::now();
    let client_ip = Some(routes::extract_client_ip(headers, None));
    let user_tags = routes::extract_user_tags(&state.user_tag_store.get(), headers);
    let inner = state.inner.load();
    let hook_result = inner.hooks.pre_auth(raw_key, headers, model);
    let mut headers = headers.clone();
    crate::extractor::apply_hook_headers(&mut headers, &hook_result.headers);
    let mut new_model: Option<String> = None;
    let effective_key = match hook_result.outcome {
        PreAuthOutcome::NoHook | PreAuthOutcome::Continue => raw_key.to_string(),
        PreAuthOutcome::Replace(new_key) => new_key,
        PreAuthOutcome::ReplaceModel { new_key, new_model: m } => {
            new_model = Some(m);
            new_key
        }
        PreAuthOutcome::Reject(reason) => {
            let e = GatewayError::AuthError(reason);
            log_auth_error(
                state, raw_key, "/v1/responses", start, &e,
                Some(uuid::Uuid::new_v4().to_string()), client_ip, user_tags,
            );
            return Err(e);
        }
        PreAuthOutcome::Deny => {
            let e = GatewayError::InternalError("pre_auth hook failure (deny mode)".into());
            log_auth_error(
                state, raw_key, "/v1/responses", start, &e,
                Some(uuid::Uuid::new_v4().to_string()), client_ip, user_tags,
            );
            return Err(e);
        }
    };
    let identity = inner.auth.authenticate(&effective_key).await.inspect_err(|e| {
        tracing::info!(error = %e, "websocket frame auth recheck failed");
        log_auth_error(
            state, &effective_key, "/v1/responses", start, e,
            Some(uuid::Uuid::new_v4().to_string()), client_ip, user_tags,
        );
    })?;
    drop(inner);
    Ok((identity, new_model, headers))
}

/// `generate: false` lane-runner body: per-frame auth (hook + revocation
/// recheck — a blocked key stops serving new creates even when they only
/// persist), authoritative rate-limit charge (the counts commit an
/// inference create gets inside the handler), then the local
/// accept-and-store completion. Always completes its lane budget.
async fn store_only_runner(
    state: &AppState,
    tx: &tokio::sync::mpsc::Sender<WsOutboundEvent>,
    headers: axum::http::HeaderMap,
    raw_key: String,
    item: WorkItem,
    ws_cfg: &boom_config::ResponsesWsSettings,
) -> Option<(Option<String>, usize)> {
    let stream_id = item.request.stream_id.clone();
    let input_bytes = item.input_bytes;
    let model = item.request.inner.model.clone();
    let (identity, new_model, _headers) = match frame_auth(state, &headers, &raw_key, Some(&model)).await {
        Ok(v) => v,
        Err(e) => {
            let _ = tx
                .send(error_frame(e.status_code(), "invalid_token", e.to_string(), stream_id.as_deref()))
                .await;
            return Some((stream_id, input_bytes));
        }
    };
    let mut create = item.request;
    if let Some(m) = new_model {
        create.inner.model = m;
    }
    // Authoritative check+commit (the preflight gate ran pre-admission).
    // May still reject if the window filled between peek and here — the
    // lane releases normally, same window the HTTP path has.
    if let Err(e) = routes::responses_charge_local(state, &identity, &create.inner.model).await {
        let _ = tx
            .send(error_frame(e.status_code(), "rate_limit_exceeded", e.to_string(), stream_id.as_deref()))
            .await;
        return Some((stream_id, input_bytes));
    }
    complete_without_inference(state, &identity, &create, tx, ws_cfg.max_stream_event_bytes).await;
    Some((stream_id, input_bytes))
}

/// Panic containment for lane runners. JoinSet reports a panicking task as
/// a JoinError that carries no lane/bytes — `mux` would keep the lane
/// registered forever (every later same-stream_id create queues behind a
/// runner that never completes, inflight/bytes budgets leak, and nothing
/// self-heals). The caught panic surfaces as the normal completion value
/// so `mux.complete` releases the budget and drains the backlog, and the
/// client gets an error frame as the request's terminal event.
async fn guard_runner(
    tx: tokio::sync::mpsc::Sender<WsOutboundEvent>,
    stream_id: Option<String>,
    input_bytes: usize,
    fut: impl std::future::Future<Output = Option<(Option<String>, usize)>>,
) -> Option<(Option<String>, usize)> {
    match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                stream_id = stream_id.as_deref().unwrap_or("-"),
                "ws lane runner panicked; lane released with error frame"
            );
            let _ = tx
                .send(error_frame(500, "internal_error",
                    "websocket request handler panicked".into(), stream_id.as_deref()))
                .await;
            Some((stream_id, input_bytes))
        }
    }
}

/// Execute one response.create through the ordinary HTTP handler and relay
/// its SSE body as WS frames. Returns the (lane, input_bytes) the loop uses
/// to release budget; also returns None-variant via the runner protocol to
/// signal "lane backlog exhausted" (this implementation always exhausts —
/// backlog pull happens through the completion path).
async fn execute_create(
    state: &AppState,
    tx: &tokio::sync::mpsc::Sender<WsOutboundEvent>,
    headers: axum::http::HeaderMap,
    remote_addr: SocketAddr,
    raw_key: String,
    item: WorkItem,
    ws_cfg: &boom_config::ResponsesWsSettings,
) -> Option<(Option<String>, usize)> {
    let stream_id = item.request.stream_id.clone();
    let input_bytes = item.input_bytes;

    // Per-frame auth: pre_auth hook + revocation recheck (P1) — blocked/
    // rotated keys stop serving new creates immediately; moka-cached, so
    // this is cheap. The handshake identity is NOT reused: the hook may
    // swap the key per frame, and a hook `ReplaceModel` flows into the
    // handler via RequiredAuth::set_new_model, exactly like the HTTP
    // extractor.
    let model = item.request.inner.model.clone();
    let (identity, new_model, headers) = match frame_auth(state, &headers, &raw_key, Some(&model)).await {
        Ok(v) => v,
        Err(e) => {
            let _ = tx
                .send(error_frame(e.status_code(), "invalid_token", e.to_string(), stream_id.as_deref()))
                .await;
            return Some((stream_id, input_bytes));
        }
    };
    let mut auth = RequiredAuth::from_identity(identity);
    auth.set_new_model(new_model);

    let result = routes::responses(
        State(state.clone()),
        auth,
        headers,
        ConnectInfo(remote_addr),
        crate::extractor::CachedJson(item.request.inner.clone()),
    )
    .await;

    match result {
        Ok(response) => relay_sse_body(response, tx.clone(), stream_id.as_deref(), ws_cfg.max_stream_event_bytes).await,
        Err(e) => {
            let status = e.0.status_code();
            let message = e.0.to_string();
            let _ = tx.send(error_frame(status, "upstream_error", message, stream_id.as_deref())).await;
        }
    }
    Some((stream_id, input_bytes))
}

/// Consume the handler's SSE body stream, re-framing each `data:` event as
/// one WS text frame with the stream_id member attached.
async fn relay_sse_body(
    response: axum::response::Response,
    tx: tokio::sync::mpsc::Sender<WsOutboundEvent>,
    stream_id: Option<&str>,
    max_event_bytes: usize,
) {
    let body = response.into_body();
    let mut stream = body.into_data_stream();
    // Byte-accurate buffering: chunk boundaries don't align with UTF-8 or SSE
    // frame boundaries; decode once per complete frame so multi-byte chars are
    // never lossy-replaced, and a mid-stream body error surfaces as an error
    // frame instead of a silently truncated stream with no terminal event.
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        match stream.try_next().await {
            Ok(Some(chunk)) => buffer.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(e) => {
                let _ = tx
                    .send(error_frame(502, "upstream_stream_error",
                        format!("SSE body stream failed: {e}"), stream_id))
                    .await;
                return;
            }
        }
        // SSE frames are separated by a blank line; within a frame we only
        // care about `data: ` lines.
        while let Some(pos) = buffer.windows(2).position(|w| w == b"\n\n") {
            let frame_bytes: Vec<u8> = buffer.drain(..=pos + 1).collect();
            let frame = String::from_utf8_lossy(&frame_bytes);
            for line in frame.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" {
                        continue; // chat-completions sentinel never applies here
                    }
                    match serde_json::from_str::<serde_json::Value>(data) {
                        Ok(v) => match WsOutboundEvent::build(v, stream_id, max_event_bytes) {
                            Ok(ev) => {
                                if tx.send(ev).await.is_err() {
                                    return; // connection gone
                                }
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(error_frame(500, "server_error", e, stream_id))
                                    .await;
                            }
                        },
                        Err(_) => { /* non-JSON data line (keep-alive) — skip */ }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (design §4 P0 acceptance — lane FIFO, budgets, panic containment,
// SSE re-framing; the mechanisms the 637-line transport was previously
// shipping with zero coverage)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod ws_transport_tests {
    use super::*;
    use axum::body::{Body, Bytes};
    use boom_config::ResponsesWsSettings;
    use http_body_util::StreamBody;

    fn test_cfg() -> ResponsesWsSettings {
        ResponsesWsSettings::default()
    }

    fn work_item(lane: Option<&str>, bytes: usize, generate: bool) -> WorkItem {
        let request: boom_core::responses::ResponsesRequest =
            serde_json::from_str(r#"{"model":"m","input":"hi"}"#).expect("parse request");
        WorkItem {
            request: WsCreate { inner: request, stream_id: lane.map(str::to_string) },
            input_bytes: bytes,
            generate,
        }
    }

    fn lane(item: &WorkItem) -> Option<String> {
        item.request.stream_id.clone()
    }

    // ── Multiplexer: same-lane FIFO ──

    #[test]
    fn same_lane_requests_run_fifo() {
        let mut mux = Multiplexer::new(&test_cfg());
        let first = work_item(Some("a"), 10, true);
        let second = work_item(Some("a"), 10, true);

        let ready = mux.admit(first);
        assert!(ready.is_some(), "first create on an idle lane starts immediately");
        assert_eq!(mux.inflight, 1);

        let ready = mux.admit(second);
        assert!(ready.is_none(), "second create on a busy lane queues");
        assert_eq!(mux.queued, 1);

        // Completing the first releases the queued one on the same lane.
        let next = mux.complete(&Some("a".to_string()), 10);
        assert!(next.is_some(), "queued create is scheduled FIFO");
        assert_eq!(mux.inflight, 1);
        assert_eq!(mux.queued, 0);

        // Completing the last one retires the lane.
        let next = mux.complete(&Some("a".to_string()), 10);
        assert!(next.is_none());
        assert!(!mux.lanes.contains_key(&Some("a".to_string())), "lane removed");
    }

    #[test]
    fn byte_budget_is_reserved_and_released() {
        let mut mux = Multiplexer::new(&test_cfg());
        let item = work_item(Some("a"), 100, true);
        assert!(mux.has_capacity(100));
        assert!(mux.admit(item).is_some());
        assert_eq!(mux.bytes_used, 100);
        assert!(!mux.has_capacity(test_cfg().max_outstanding_bytes), "budget exhausted");
        mux.complete(&Some("a".to_string()), 100);
        assert_eq!(mux.bytes_used, 0, "budget released on completion");
        assert!(mux.has_capacity(100));
    }

    #[test]
    fn lane_cap_rejects_new_lanes_until_one_completes() {
        let cfg = ResponsesWsSettings { max_session_lanes: 2, ..test_cfg() };
        let mut mux = Multiplexer::new(&cfg);
        assert!(mux.admit(work_item(Some("a"), 1, true)).is_some());
        assert!(mux.admit(work_item(Some("b"), 1, true)).is_some());
        assert!(!mux.lane_available(&Some("c".to_string())), "lane cap reached");
        // Existing lane still accepted (queued on it).
        assert!(mux.lane_available(&Some("a".to_string())));
        mux.complete(&Some("a".to_string()), 1);
        assert!(mux.lane_available(&Some("c".to_string())), "freed lane slot reused");
    }

    #[test]
    fn outstanding_request_cap_counts_inflight_and_queued() {
        let cfg = ResponsesWsSettings { max_outstanding_requests: 2, ..test_cfg() };
        let mut mux = Multiplexer::new(&cfg);
        assert!(mux.admit(work_item(Some("a"), 1, true)).is_some());
        assert!(mux.admit(work_item(Some("a"), 1, true)).is_none(), "queues behind");
        assert!(!mux.has_capacity(1), "inflight + queued hit the cap");
        mux.complete(&Some("a".to_string()), 1); // pops the queued one
        assert!(mux.has_capacity(1));
    }

    #[test]
    fn discard_all_clears_backlog_and_budget() {
        let mut mux = Multiplexer::new(&test_cfg());
        assert!(mux.admit(work_item(Some("a"), 50, true)).is_some());
        assert!(mux.admit(work_item(Some("a"), 50, true)).is_none());
        mux.discard_all();
        assert_eq!(mux.bytes_used, 0);
        assert_eq!(mux.queued, 0);
        assert!(mux.lanes.is_empty());
    }

    // ── Transport helpers ──

    #[test]
    fn value_generate_reads_transport_member_from_raw_text() {
        assert_eq!(value_generate(r#"{"type":"response.create","generate":false}"#), Some(false));
        assert_eq!(value_generate(r#"{"type":"response.create","generate":true}"#), Some(true));
        assert_eq!(value_generate(r#"{"type":"response.create"}"#), None);
        // Wrong type is not a generate:false — the frame proceeds as a
        // normal create rather than crashing the probe.
        assert_eq!(value_generate(r#"{"type":"response.create","generate":"no"}"#), None);
    }

    #[test]
    fn error_frame_carries_status_code_and_stream_id() {
        let frame = error_frame(429, "rate_limit_exceeded", "slow down".into(), Some("lane-1"));
        let v: serde_json::Value = serde_json::from_str(&frame.0).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["status"], 429);
        assert_eq!(v["stream_id"], "lane-1");
        assert_eq!(v["error"]["code"], "rate_limit_exceeded");
        assert_eq!(v["error"]["type"], "invalid_request_error");

        let no_lane = error_frame(400, "invalid_json", "bad".into(), None);
        let v: serde_json::Value = serde_json::from_str(&no_lane.0).unwrap();
        assert!(v.get("stream_id").is_none());
    }

    #[test]
    fn outbound_event_over_cap_is_rejected_before_queueing() {
        let value = serde_json::json!({ "type": "response.completed", "big": "x".repeat(64) });
        let err = WsOutboundEvent::build(value.clone(), None, 32).unwrap_err();
        assert!(err.contains("exceeds"), "oversize event rejected: {err}");
        let ok = WsOutboundEvent::build(value, Some("lane-9"), 4096).unwrap();
        let v: serde_json::Value = serde_json::from_str(&ok.0).unwrap();
        assert_eq!(v["stream_id"], "lane-9", "stream_id attached on build");
    }

    // ── Panic containment ──

    #[tokio::test]
    async fn panicking_runner_releases_lane_with_error_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WsOutboundEvent>(8);
        let result = guard_runner(tx.clone(), Some("panicky".to_string()), 42, async {
            panic!("handler exploded");
            #[allow(unreachable_code)]
            None
        })
        .await;
        assert_eq!(
            result,
            Some((Some("panicky".to_string()), 42)),
            "caught panic surfaces as a normal completion — mux.complete releases the budget"
        );
        let frame = rx.try_recv().expect("client gets a terminal error frame");
        let v: serde_json::Value = serde_json::from_str(&frame.0).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["status"], 500);
        assert_eq!(v["stream_id"], "panicky");
    }

    #[tokio::test]
    async fn guard_runner_passes_through_normal_completion() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WsOutboundEvent>(8);
        let result = guard_runner(tx, Some("calm".to_string()), 7, async {
            Some((Some("calm".to_string()), 7))
        })
        .await;
        assert_eq!(result, Some((Some("calm".to_string()), 7)));
        assert!(rx.try_recv().is_err(), "no error frame on the happy path");
    }

    // ── Per-key connection registry ──

    #[tokio::test]
    async fn per_key_connection_cap_is_raii() {
        let key = format!("hash-{}", line!()); // unique per test
        let guard = WsConnectionGuard::acquire(&key, 2).expect("first connection");
        let guard2 = WsConnectionGuard::acquire(&key, 2).expect("second connection");
        assert!(
            WsConnectionGuard::acquire(&key, 2).is_none(),
            "third connection rejected at the cap"
        );
        drop(guard);
        let guard3 = WsConnectionGuard::acquire(&key, 2).expect("slot freed by drop");
        assert!(WsConnectionGuard::acquire(&key, 2).is_none());
        drop(guard2);
        drop(guard3);
        assert!(!connection_registry().contains_key(&key), "registry cleans up at zero");
    }

    // ── relay_sse_body: byte-accurate re-framing ──

    fn sse_response(chunks: Vec<Result<Bytes, std::io::Error>>) -> axum::response::Response {
        // StreamBody wants Result<Frame<_>, _> — wrap raw Bytes chunks.
        let stream = futures::stream::iter(chunks).map(|res| res.map(http_body::Frame::data));
        axum::response::Response::builder()
            .body(Body::new(StreamBody::new(stream)))
            .unwrap()
    }

    async fn drain_frames(rx: &mut tokio::sync::mpsc::Receiver<WsOutboundEvent>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(serde_json::from_str(&frame.0).expect("frame is valid JSON"));
        }
        out
    }

    #[tokio::test]
    async fn sse_multibyte_char_split_across_chunks_is_not_lossy() {
        // "中" is 3 bytes; cut it mid-sequence between body chunks. The
        // per-chunk lossy decode this test replaces produced U+FFFD and the
        // event JSON failed to parse (event silently dropped).
        let event = r#"data: {"type":"response.output_text.delta","delta":"中文"}"#;
        let bytes = event.as_bytes();
        let cut = bytes.len() - "文".len() - 4; // inside the last multibyte char
        let (head, tail) = bytes.split_at(cut);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WsOutboundEvent>(8);
        let response = sse_response(vec![
            Ok(Bytes::from_static(b"data: {\"type\":\"response.created\"}\n\n")),
            Ok(Bytes::copy_from_slice(head)),
            Ok(Bytes::copy_from_slice(tail)),
            Ok(Bytes::from_static(b"\n\n")),
        ]);
        relay_sse_body(response, tx, Some("lane-1"), 512 * 1024).await;
        let frames = drain_frames(&mut rx).await;
        assert_eq!(frames.len(), 2, "both events delivered");
        assert_eq!(frames[0]["type"], "response.created");
        assert_eq!(frames[0]["stream_id"], "lane-1");
        assert_eq!(frames[1]["type"], "response.output_text.delta");
        assert_eq!(
            frames[1]["delta"], "中文",
            "multibyte char reassembled byte-accurately, no U+FFFD"
        );
    }

    #[tokio::test]
    async fn sse_midstream_body_error_emits_error_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WsOutboundEvent>(8);
        let response = sse_response(vec![
            Ok(Bytes::from_static(b"data: {\"type\":\"response.created\"}\n\n")),
            Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "upstream cut")),
        ]);
        relay_sse_body(response, tx, Some("lane-2"), 512 * 1024).await;
        let frames = drain_frames(&mut rx).await;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1]["type"], "error", "mid-stream failure is terminal and visible");
        assert_eq!(frames[1]["status"], 502);
        assert_eq!(frames[1]["stream_id"], "lane-2");
    }

    #[tokio::test]
    async fn sse_done_sentinel_is_not_relayed() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WsOutboundEvent>(8);
        let response = sse_response(vec![Ok(Bytes::from_static(
            b"data: {\"type\":\"response.completed\"}\n\ndata: [DONE]\n\n",
        ))]);
        relay_sse_body(response, tx, None, 512 * 1024).await;
        let frames = drain_frames(&mut rx).await;
        assert_eq!(frames.len(), 1, "chat-completions [DONE] never reaches WS clients");
        assert_eq!(frames[0]["type"], "response.completed");
    }

    #[tokio::test]
    async fn sse_event_over_session_cap_becomes_error_frame() {
        let big_delta = "x".repeat(600);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WsOutboundEvent>(8);
        let response = sse_response(vec![Ok(Bytes::from(
            format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{big_delta}\"}}\n\n"),
        ))]);
        // Cap smaller than the event: rejected at build, surfaced as error.
        relay_sse_body(response, tx, Some("lane-3"), 128).await;
        let frames = drain_frames(&mut rx).await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["type"], "error");
        assert_eq!(frames[0]["error"]["code"], "server_error");
    }
}
