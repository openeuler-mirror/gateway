//! Mock OpenAI/Anthropic backend for BooMGateway throughput benchmarking.
//!
//! Returns 100~400 char random responses for any incoming request. Both
//! OpenAI-style (/v1/chat/completions, /v1/completions) and Anthropic-style
//! (/v1/messages) endpoints accept any body shape; ALL responses are OpenAI
//! format — this matches the gateway's protocol-conversion path (Anthropic
//! request → `boom-core::anthropic::anthropic_request_to_openai()` → OpenAI
//! upstream).

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, sse::{Event, Sse}},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use rand::Rng;
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Semaphore};
use tokio_stream::wrappers::ReceiverStream;

// ── Pre-generated character pool ────────────────────────────────────────
// Repeated sampling from a fixed pool avoids per-request RNG of the charset.
// 1 MiB is large enough that random slices look diverse; small enough to
// stay in L2.
const POOL_SIZE: usize = 1024 * 1024;
const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ";

#[derive(Parser, Debug, Clone)]
#[command(name = "mock-backend", about = "Mock OpenAI/Anthropic backend for gateway benchmarking")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8000")]
    bind: String,

    /// Minimum response length (chars).
    #[arg(long, default_value_t = 100)]
    min_chars: usize,

    /// Maximum response length (chars).
    #[arg(long, default_value_t = 400)]
    max_chars: usize,

    /// Per-chunk interval in streaming mode (ms).
    #[arg(long, default_value_t = 2)]
    chunk_interval_ms: u64,

    /// Max concurrent requests before returning 503.
    #[arg(long, default_value_t = 10000)]
    max_concurrent: usize,

    /// Optional first-token delay (ms) — simulates reasoning-model TTFT.
    #[arg(long, default_value_t = 0)]
    ttft_ms: u64,

    /// Optional non-streaming response delay (ms) — simulates a slow panel.
    #[arg(long, default_value_t = 0)]
    response_delay_ms: u64,

    /// When set, advertise and accept only this model.
    #[arg(long)]
    served_model: Option<String>,

    /// Reject requests that explicitly send an empty tools array.
    #[arg(long, default_value_t = false)]
    reject_empty_tools: bool,
}

struct AppState {
    pool: Vec<u8>,
    args: Args,
    semaphore: Arc<Semaphore>,
    stats: Arc<Stats>,
}

#[derive(Default)]
struct Stats {
    inflight: AtomicU64,
    total_received: AtomicU64,
    total_chat: AtomicU64,
    total_completion: AtomicU64,
    total_messages: AtomicU64,
    total_503: AtomicU64,
    rejected_model: AtomicU64,
    rejected_empty_tools: AtomicU64,
}

#[derive(Deserialize, Default)]
struct AnyBody {
    model: Option<String>,
    messages: Option<Vec<serde_json::Value>>,
    prompt: Option<serde_json::Value>,
    stream: Option<bool>,
    #[allow(dead_code)]
    max_tokens: Option<u64>,
    tools: Option<Vec<serde_json::Value>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mock_backend=info,warn".into()),
        )
        .init();

    let args = Args::parse();
    tracing::info!(?args, "starting mock-backend");

    let pool = generate_pool();
    let semaphore = Arc::new(Semaphore::new(args.max_concurrent));
    let stats = Arc::new(Stats::default());

    let state = Arc::new(AppState {
        pool,
        args: args.clone(),
        semaphore,
        stats,
    });

    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/v1/messages", post(messages))
        .route("/v1/models", get(list_models))
        .route("/health", get(health))
        .route("/internal/stats", get(get_stats))
        .with_state(state);

    let addr: SocketAddr = args.bind.parse()?;
    tracing::info!("listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn generate_pool() -> Vec<u8> {
    let mut rng = rand::thread_rng();
    (0..POOL_SIZE)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())])
        .collect()
}

// ── Handlers ────────────────────────────────────────────────────────────

async fn chat_completions(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    st.stats.total_chat.fetch_add(1, Ordering::Relaxed);
    handle_any(&st, &headers, body, "chat.completion").await
}

async fn completions(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    st.stats.total_completion.fetch_add(1, Ordering::Relaxed);
    handle_any(&st, &headers, body, "text_completion").await
}

/// Anthropic-style endpoint — request body has Anthropic shape, but the
/// response is OpenAI format (matches the gateway's protocol-conversion
/// path where Anthropic requests are converted to OpenAI upstream).
async fn messages(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    st.stats.total_messages.fetch_add(1, Ordering::Relaxed);
    handle_any(&st, &headers, body, "chat.completion").await
}

async fn handle_any(
    st: &AppState,
    _headers: &HeaderMap,
    body: serde_json::Value,
    object_kind: &str,
) -> Response {
    st.stats.total_received.fetch_add(1, Ordering::Relaxed);

    // Acquire concurrency permit; reject with 503 when over limit.
    let permit = match st.semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            st.stats.total_503.fetch_add(1, Ordering::Relaxed);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":{"type":"server_overloaded","message":"mock max_concurrent reached"}})),
            )
                .into_response();
        }
    };

    let parsed: AnyBody = serde_json::from_value(body).unwrap_or_default();
    let stream = parsed.stream.unwrap_or(false);
    let model = parsed.model.clone().unwrap_or_else(|| "mock-model".into());
    if st.args.reject_empty_tools && parsed.tools.as_ref().is_some_and(Vec::is_empty) {
        st.stats
            .rejected_empty_tools
            .fetch_add(1, Ordering::Relaxed);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "type": "invalid_request_error",
                    "message": "tools must not be an empty array; either provide at least one tool or omit the field entirely"
                }
            })),
        )
            .into_response();
    }
    if st
        .args
        .served_model
        .as_deref()
        .is_some_and(|served| served != model)
    {
        st.stats.rejected_model.fetch_add(1, Ordering::Relaxed);
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "type": "model_not_found",
                    "message": format!("model '{model}' is not served by this backend")
                }
            })),
        )
            .into_response();
    }
    let prompt_chars = estimate_prompt_chars(&parsed);
    let prompt_tokens = (prompt_chars / 4).max(1) as u64;

    let output_chars = rand::thread_rng().gen_range(st.args.min_chars..=st.args.max_chars);
    let output_text = random_string(&st.pool, output_chars);
    let completion_tokens = (output_chars / 4).max(1) as u64;
    let total_tokens = prompt_tokens + completion_tokens;

    let id = format!("chatcmpl-mock-{}", uuid::Uuid::new_v4().simple());
    let created = chrono_now_secs();

    st.stats.inflight.fetch_add(1, Ordering::Relaxed);
    let stats_clone = st.stats.clone();

    if stream {
        let interval_ms = st.args.chunk_interval_ms;
        let ttft_ms = st.args.ttft_ms;
        let object_str = object_kind.to_string();

        // Channel carries Event values; axum's Sse wraps the ReceiverStream.
        // Buffer of 32 is enough to absorb client backpressure without
        // blocking the producer on every chunk.
        let (tx, rx) = mpsc::channel::<Result<Event, std::convert::Infallible>>(32);

        tokio::spawn(async move {
            // Hold permit + inflight guard for the duration of the stream.
            let _permit = permit;
            let _inflight_guard = InflightGuard(stats_clone);

            // TTFT delay (simulates reasoning model first-token latency).
            if ttft_ms > 0 {
                tokio::time::sleep(Duration::from_millis(ttft_ms)).await;
            }

            // First chunk: role announcement.
            let first = json!({
                "id": id,
                "object": format!("{}.chunk", object_str),
                "created": created,
                "model": model,
                "choices": [{"index":0,"delta":{"role":"assistant"},"logprobs":null,"finish_reason":null}]
            });
            if tx.send(Ok(Event::default().json_data(first).unwrap())).await.is_err() {
                return; // client disconnected
            }

            // Content chunks: 1~3 chars each, spaced by interval.
            let mut chars = output_text.chars().peekable();
            while chars.peek().is_some() {
                let mut chunk = String::new();
                for _ in 0..rand::thread_rng().gen_range(1..=3) {
                    if let Some(c) = chars.next() {
                        chunk.push(c);
                    } else {
                        break;
                    }
                }
                let payload = json!({
                    "id": id,
                    "object": format!("{}.chunk", object_str),
                    "created": created,
                    "model": model,
                    "choices": [{"index":0,"delta":{"content":chunk},"logprobs":null,"finish_reason":null}]
                });
                if interval_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(interval_ms)).await;
                }
                if tx.send(Ok(Event::default().json_data(payload).unwrap())).await.is_err() {
                    return;
                }
            }

            // Final chunk: finish_reason + usage (vLLM convention — usage in last chunk).
            let final_payload = json!({
                "id": id,
                "object": format!("{}.chunk", object_str),
                "created": created,
                "model": model,
                "choices": [{"index":0,"delta":{},"logprobs":null,"finish_reason":"stop"}],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": total_tokens
                }
            });
            let _ = tx.send(Ok(Event::default().json_data(final_payload).unwrap())).await;

            // Closing sentinel (OpenAI convention).
            let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
        });

        Sse::new(ReceiverStream::new(rx)).into_response()
    } else {
        if st.args.response_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(st.args.response_delay_ms)).await;
        }
        st.stats.inflight.fetch_sub(1, Ordering::Relaxed);
        drop(permit);
        let body = json!({
            "id": id,
            "object": object_kind,
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role":"assistant","content": output_text},
                "logprobs": null,
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": total_tokens
            }
        });
        Json(body).into_response()
    }
}

async fn list_models(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let models = match st.args.served_model.as_deref() {
        Some(model) => vec![json!({
            "id": model,
            "object": "model",
            "created": 0,
            "owned_by": "mock"
        })],
        None => vec![
            json!({"id":"mock-gpt-4o","object":"model","created":0,"owned_by":"mock"}),
            json!({"id":"mock-claude","object":"model","created":0,"owned_by":"mock"}),
            json!({"id":"mock-deepseek","object":"model","created":0,"owned_by":"mock"}),
        ],
    };
    Json(json!({
        "object": "list",
        "data": models
    }))
}

async fn health() -> impl IntoResponse {
    Json(json!({"status":"ok"}))
}

async fn get_stats(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let s = &st.stats;
    Json(json!({
        "inflight": s.inflight.load(Ordering::Relaxed),
        "total_received": s.total_received.load(Ordering::Relaxed),
        "endpoint_chat": s.total_chat.load(Ordering::Relaxed),
        "endpoint_completion": s.total_completion.load(Ordering::Relaxed),
        "endpoint_messages": s.total_messages.load(Ordering::Relaxed),
        "rejected_503": s.total_503.load(Ordering::Relaxed),
        "rejected_model": s.rejected_model.load(Ordering::Relaxed),
        "rejected_empty_tools": s.rejected_empty_tools.load(Ordering::Relaxed),
        "max_concurrent": st.args.max_concurrent,
    }))
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn random_string(pool: &[u8], len: usize) -> String {
    if len == 0 || pool.is_empty() {
        return String::new();
    }
    let mut rng = rand::thread_rng();
    let start = rng.gen_range(0..pool.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(pool[(start + i) % pool.len()]);
    }
    // SAFETY: pool is all ASCII.
    String::from_utf8(out).unwrap()
}

fn estimate_prompt_chars(body: &AnyBody) -> usize {
    let mut total = 0;
    if let Some(msgs) = &body.messages {
        for m in msgs {
            if let Some(content) = m.get("content") {
                if let Some(s) = content.as_str() {
                    total += s.len();
                } else if let Some(arr) = content.as_array() {
                    // Anthropic-style content blocks: [{type:"text", text:"..."}]
                    for block in arr {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            total += t.len();
                        }
                    }
                }
            }
        }
    }
    if let Some(p) = &body.prompt {
        if let Some(s) = p.as_str() {
            total += s.len();
        } else if let Some(arr) = p.as_array() {
            for s in arr.iter().filter_map(|v| v.as_str()) {
                total += s.len();
            }
        }
    }
    total
}

fn chrono_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// RAII guard that decrements `inflight` on drop. Required so streaming
/// responses (which finish well after the handler returns) still account
/// correctly.
struct InflightGuard(Arc<Stats>);
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}
