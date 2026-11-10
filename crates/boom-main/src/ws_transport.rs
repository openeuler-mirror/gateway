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

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::ConnectInfo;
use axum::response::Response;
use axum::extract::State;
use futures::{SinkExt, StreamExt, TryStreamExt};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;

use crate::extractor::RequiredAuth;
use crate::routes;
use crate::state::AppState;
use boom_core::types::AuthIdentity;

/// Outstanding create requests allowed on one connection (agentic-api value).
const MAX_OUTSTANDING_REQUESTS: usize = 64;
/// Serialized-size budget for in-flight request payloads (12 MiB).
const MAX_OUTSTANDING_BYTES: usize = 12 * 1024 * 1024;
/// Distinct stream_id lanes allowed on one connection.
const MAX_LANES: usize = 128;
/// Outbound frames buffered ahead of the socket writer.
const OUTBOUND_BUFFER: usize = 64;
/// A frame the socket cannot deliver within this window kills the
/// connection — a stalled client must not freeze the session.
const SEND_TIMEOUT_SECS: u64 = 10;
/// Max chars of a client-supplied stream_id (agentic-api: 256).
const MAX_STREAM_ID_CHARS: usize = 256;
/// Hard cap on one outbound frame; events beyond it are dropped to an
/// error frame rather than queued.
const MAX_EVENT_BYTES: usize = 512 * 1024;

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
    // One authentication per connection, at the handshake — the same call
    // the HTTP extractor makes. Frames reuse the identity; P1 will recheck
    // revocation per frame.
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
    let identity = {
        let inner = state.inner.load();
        match inner.auth.authenticate(&raw_key).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "websocket handshake auth failed");
                return handshake_reject(e.status_code());
            }
        }
    };

    ws.on_upgrade(move |socket| {
        Box::pin(session_loop(
            socket,
            state,
            headers,
            remote_addr,
            SessionPrincipal { identity, raw_key },
        ))
    })
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
/// bounded queue so an oversized event is rejected at the source.
struct WsOutboundEvent(String);

impl WsOutboundEvent {
    fn build(mut value: serde_json::Value, stream_id: Option<&str>) -> Result<Self, String> {
        if let Some(obj) = value.as_object_mut() {
            if let Some(id) = stream_id {
                obj.insert("stream_id".to_string(), serde_json::Value::String(id.to_string()));
            }
        }
        let text = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        if text.len() > MAX_EVENT_BYTES {
            return Err(format!("event exceeds {MAX_EVENT_BYTES} bytes"));
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
    if state.response_store.is_enabled() && inner.store != Some(false) {
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
            ttl_secs: 2_592_000,
            max_history_items: 1000,
        };
        if let Err(e) = state.response_store.persist_turn(turn).await {
            tracing::warn!(error = %e, "ws generate:false persist failed");
        }
    }
    for seq in [0u64, 1] {
        let ev = shell(if seq == 0 { "in_progress" } else { "completed" }, seq);
        let _ = tx.send(WsOutboundEvent::build(ev, sid).unwrap_or_else(|e| {
            error_frame(500, "server_error", e, sid)
        })).await;
    }
}

struct WorkItem {
    request: WsCreate,
    input_bytes: usize,
}

/// Per-connection state: byte budget + per-lane FIFO queues.
///
/// (P1 lane-continuation cache — skipping the DB rehydrate for same-lane
/// follow-ups — is deferred: it needs the handler kernel extraction so the
/// WS layer can inject pre-rehydrated history. The DB round trip remains.)
struct Multiplexer {
    lanes: HashMap<Option<String>, VecDeque<WorkItem>>,
    inflight: usize,
    queued: usize,
    bytes_used: usize,
}

impl Multiplexer {
    fn new() -> Self {
        Self {
            lanes: HashMap::new(),
            inflight: 0,
            queued: 0,
            bytes_used: 0,
        }
    }

    fn has_capacity(&self, input_bytes: usize) -> bool {
        self.inflight + self.queued < MAX_OUTSTANDING_REQUESTS
            && self.bytes_used + input_bytes <= MAX_OUTSTANDING_BYTES
    }

    fn lane_available(&self, lane: &Option<String>) -> bool {
        self.lanes.contains_key(lane) || self.lanes.len() < MAX_LANES
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
) {
    let identity = principal.identity.clone();
    let (mut sink, mut source) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<WsOutboundEvent>(OUTBOUND_BUFFER);
    let mut mux = Multiplexer::new();
    // JoinSet of per-lane runner tasks; each runner executes creates
    // serially for its lane until the backlog is empty.
    let mut runners: tokio::task::JoinSet<Option<(Option<String>, usize)>> = Default::default();
    let mut draining = false;

    loop {
        tokio::select! {
            // Completed lane-runner: it returns the lane + input bytes it
            // just finished; None means the lane's backlog is exhausted.
            Some(joined) = runners.join_next(), if !runners.is_empty() => {
                match joined {
                    Ok(Some((lane, input_bytes))) => {
                        if let Some(next) = mux.complete(&lane, input_bytes) {
                            spawn_runner_inner(&mut runners, state.clone(), tx.clone(), headers.clone(), remote_addr, identity.clone(), principal.raw_key.clone(), next);
                        }
                    }
                    Ok(None) => {} // backlog drained inside the runner
                    Err(e) => { tracing::warn!(error = %e, "ws lane runner panicked"); }
                }
            }
            frame = rx.recv() => {
                let Some(frame) = frame else { break };
                let ok = tokio::time::timeout(
                    std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
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
                            &identity, &principal.raw_key, text.as_str(),
                        ) {
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
    tracing::debug!(lanes_remaining = mux.lanes.len(), "websocket responses session closed");
}

enum Flow { Continue }

#[allow(clippy::too_many_arguments)]
fn handle_client_text(
    state: &AppState,
    tx: &tokio::sync::mpsc::Sender<WsOutboundEvent>,
    mux: &mut Multiplexer,
    runners: &mut tokio::task::JoinSet<Option<(Option<String>, usize)>>,
    headers: &axum::http::HeaderMap,
    remote_addr: SocketAddr,
    identity: &AuthIdentity,
    raw_key: &str,
    text: &str,
) -> Flow {
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
            format!("too many concurrent streams (max {MAX_LANES})"), stream_id.as_deref()));
        return Flow::Continue;
    }
    if !mux.has_capacity(input_bytes) {
        let _ = tx.try_send(error_frame(429, "rate_limit_exceeded",
            "too many outstanding websocket response.create requests".into(), stream_id.as_deref()));
        return Flow::Continue;
    }

    if generate == Some(false) {
        // Local completion bypasses admission (no inference, no upstream);
        // budget impact is the persist alone.
        let create = WsCreate { inner: request, stream_id };
        let state = state.clone();
        let identity = identity.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            complete_without_inference(&state, &identity, &create, &tx).await;
        });
        return Flow::Continue;
    }

    let item = WorkItem { request: WsCreate { inner: request, stream_id }, input_bytes };
    let ready = mux.admit(item);
    if let Some(ready) = ready {
        spawn_runner_inner(runners, state.clone(), tx.clone(), headers.clone(), remote_addr, identity.clone(), raw_key.to_string(), ready);
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
/// messages, so FIFO order within a lane is preserved end to end.
fn spawn_runner_inner(
    runners: &mut tokio::task::JoinSet<Option<(Option<String>, usize)>>,
    state: AppState,
    tx: tokio::sync::mpsc::Sender<WsOutboundEvent>,
    headers: axum::http::HeaderMap,
    remote_addr: SocketAddr,
    identity: AuthIdentity,
    raw_key: String,
    item: WorkItem,
) {
    runners.spawn(async move {
        execute_create(&state, &tx, headers, remote_addr, identity, raw_key, item).await
    });
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
    identity: AuthIdentity,
    raw_key: String,
    item: WorkItem,
) -> Option<(Option<String>, usize)> {
    let stream_id = item.request.stream_id.clone();
    let input_bytes = item.input_bytes;

    // Per-frame revocation recheck (P1): blocked/rotated keys stop serving
    // new creates immediately; moka-cached, so this is cheap.
    if let Err(e) = state.inner.load().auth.authenticate(&raw_key).await {
        tracing::info!(error = %e, "websocket frame auth recheck failed");
        let _ = tx
            .send(error_frame(e.status_code(), "invalid_token", e.to_string(), stream_id.as_deref()))
            .await;
        return Some((stream_id, input_bytes));
    }

    let auth = RequiredAuth::from_identity(identity);
    let result = routes::responses(
        State(state.clone()),
        auth,
        headers,
        ConnectInfo(remote_addr),
        crate::extractor::CachedJson(item.request.inner.clone()),
    )
    .await;

    match result {
        Ok(response) => relay_sse_body(response, tx.clone(), stream_id.as_deref()).await,
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
) {
    let body = response.into_body();
    let mut stream = body.into_data_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.try_next().await.ok().flatten() {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // SSE frames are separated by a blank line; within a frame we only
        // care about `data: ` lines.
        while let Some(pos) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..=pos + 1).collect();
            for line in frame.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" {
                        continue; // chat-completions sentinel never applies here
                    }
                    match serde_json::from_str::<serde_json::Value>(data) {
                        Ok(v) => match WsOutboundEvent::build(v, stream_id) {
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
