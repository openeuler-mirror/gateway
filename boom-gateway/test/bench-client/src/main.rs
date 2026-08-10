//! Bench client for BooMGateway throughput benchmarking.
//!
//! Sends OpenAI- or Anthropic-style requests to the gateway with 1..N API
//! keys (randomly rotated per request, to exercise the DB auth path) and
//! collects HDR histograms for TTFT + E2E latency plus error breakdowns.

use bytes::Bytes;
use clap::Parser;
use futures_util::StreamExt;
use hdrhistogram::Histogram;
use rand::Rng;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

// ── CLI ─────────────────────────────────────────────────────────────────

#[derive(Parser, Debug, Clone)]
#[command(name = "bench-client", about = "Throughput/latency bench client for BooMGateway")]
struct Args {
    /// Gateway base URL, e.g. http://127.0.0.1:8080
    #[arg(long)]
    target: String,

    /// Request format: `openai` (POST /v1/chat/completions) or `anthropic` (POST /v1/messages).
    #[arg(long, value_parser = ["openai", "anthropic"])]
    format: String,

    /// Auth header style: `bearer` (Authorization: Bearer) or `anthropic` (x-api-key).
    #[arg(long, default_value = "bearer", value_parser = ["bearer", "anthropic"])]
    auth_style: String,

    /// Comma-separated list of API keys; one is picked at random per request.
    #[arg(long, value_delimiter = ',')]
    keys: Vec<String>,

    /// Model name to send in the request body.
    #[arg(long)]
    model: String,

    /// Min prompt length (chars).
    #[arg(long, default_value_t = 1000)]
    prompt_min: usize,

    /// Max prompt length (chars).
    #[arg(long, default_value_t = 5000)]
    prompt_max: usize,

    /// Load mode:
    ///   `qps=N`              constant requests/sec
    ///   `concurrent=N`       constant in-flight requests
    ///   `ramp=FROM,TO,STEP,DURATION_SECS`
    #[arg(long)]
    mode: String,

    /// Total test duration (e.g. `60s`, `5m`).
    #[arg(long, default_value = "60s")]
    duration: String,

    /// Use streaming responses (default true; pass --stream=false to disable).
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    stream: bool,

    /// Output JSON report path. Empty = stdout only.
    #[arg(long, default_value = "")]
    report: String,

    /// Connection pool idle per host.
    #[arg(long, default_value_t = 2048)]
    pool_idle_per_host: usize,

    /// Request timeout (seconds).
    #[arg(long, default_value_t = 120)]
    request_timeout_secs: u64,
}

// ── Shared state ────────────────────────────────────────────────────────

struct Shared {
    client: reqwest::Client,
    keys: Vec<String>,
    key_counts: Arc<Vec<AtomicU64>>,
    pool_bytes: Vec<u8>,
    args: Args,
}

enum Outcome {
    Ok { ttft: Duration, e2e: Duration, bytes_in: u64, bytes_out: u64 },
    Err(ErrKind),
}

#[derive(Copy, Clone)]
enum ErrKind {
    RateLimited,
    Server5xx,
    Client4xx,
    Timeout,
    Connect,
    Parse,
    Stream,
}

// Global tallies — read by both the per-second live printer and the final
// summary. Avoids passing a Counters struct through every call.
static SENT: AtomicU64 = AtomicU64::new(0);
static OK: AtomicU64 = AtomicU64::new(0);
static ERR_429: AtomicU64 = AtomicU64::new(0);
static ERR_5XX: AtomicU64 = AtomicU64::new(0);
static ERR_4XX: AtomicU64 = AtomicU64::new(0);
static ERR_TMO: AtomicU64 = AtomicU64::new(0);
static ERR_CNT: AtomicU64 = AtomicU64::new(0);
static ERR_PRS: AtomicU64 = AtomicU64::new(0);
static ERR_STR: AtomicU64 = AtomicU64::new(0);
static BYTES_IN: AtomicU64 = AtomicU64::new(0);
static BYTES_OUT: AtomicU64 = AtomicU64::new(0);

// ── Main ────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bench_client=info,warn".into()),
        )
        .init();

    let args = Args::parse();
    if args.keys.is_empty() {
        return Err("at least one --keys value is required".into());
    }
    if args.prompt_min > args.prompt_max {
        return Err("--prompt-min > --prompt-max".into());
    }
    tracing::info!(?args, "starting bench-client");

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(args.pool_idle_per_host)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .timeout(Duration::from_secs(args.request_timeout_secs))
        .build()?;

    let pool_bytes = generate_pool();

    let key_counts = Arc::new(
        args.keys
            .iter()
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>(),
    );
    let shared = Arc::new(Shared {
        client,
        keys: args.keys.clone(),
        key_counts: key_counts.clone(),
        pool_bytes,
        args: args.clone(),
    });

    let duration = parse_duration(&args.duration)?;
    let mode = parse_mode(&args.mode)?;

    let (tx, rx) = mpsc::unbounded_channel::<Outcome>();
    let agg_handle = tokio::spawn(aggregate_stats(
        rx,
        mode.clone(),
        args.clone(),
        key_counts,
    ));

    run_load(shared, tx, duration, mode).await;

    let summary = agg_handle.await?;
    let summary_json = serde_json::to_string_pretty(&summary)?;
    if !args.report.is_empty() {
        std::fs::write(&args.report, &summary_json)?;
        tracing::info!("report written to {}", args.report);
    }
    println!("{}", summary_json);
    Ok(())
}

// ── Load modes ──────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Mode {
    Qps(u64),
    Concurrent(usize),
    Ramp { from: u64, to: u64, step: u64, step_duration: Duration },
}

fn parse_mode(s: &str) -> Result<Mode, String> {
    let (kind, val) = s.split_once('=').ok_or("mode must be K=V (e.g. qps=100)")?;
    match kind {
        "qps" => {
            let n: u64 = val.parse().map_err(|_| "qps value not a number")?;
            Ok(Mode::Qps(n))
        }
        "concurrent" => {
            let n: usize = val.parse().map_err(|_| "concurrent value not a number")?;
            Ok(Mode::Concurrent(n))
        }
        "ramp" => {
            let parts: Vec<&str> = val.split(',').collect();
            if parts.len() != 4 {
                return Err("ramp=FROM,TO,STEP,DURATION_SECS".into());
            }
            let step = parts[2].parse().map_err(|_| "step not a number")?;
            if step == 0 {
                return Err("ramp step must be greater than zero".into());
            }
            Ok(Mode::Ramp {
                from: parts[0].parse().map_err(|_| "from not a number")?,
                to: parts[1].parse().map_err(|_| "to not a number")?,
                step,
                step_duration: Duration::from_secs(parts[3].parse().map_err(|_| "duration not a number")?),
            })
        }
        _ => Err(format!("unknown mode '{}'", kind)),
    }
}

async fn run_load(
    shared: Arc<Shared>,
    tx: mpsc::UnboundedSender<Outcome>,
    duration: Duration,
    mode: Mode,
) {
    let deadline = Instant::now() + duration;
    match mode {
        Mode::Qps(n) => run_qps(shared, tx, deadline, n).await,
        Mode::Concurrent(n) => run_concurrent(shared, tx, deadline, n).await,
        Mode::Ramp { from, to, step, step_duration } => {
            let mut qps = from.min(to);
            let target = from.max(to);
            let mut join = JoinSet::<()>::new();
            loop {
                if Instant::now() >= deadline {
                    break;
                }
                let step_deadline = (Instant::now() + step_duration).min(deadline);
                let step_dur = step_deadline.saturating_duration_since(Instant::now());
                tracing::info!("ramp step: qps={} for {:?}", qps, step_dur);
                schedule_qps(shared.clone(), tx.clone(), step_deadline, qps, &mut join).await;
                if qps < target {
                    qps = (qps + step).min(target);
                }
            }
            while join.join_next().await.is_some() {}
        }
    }
    // tx is dropped here when run_load returns; aggregator sees channel close.
}

async fn run_qps(
    shared: Arc<Shared>,
    tx: mpsc::UnboundedSender<Outcome>,
    deadline: Instant,
    qps: u64,
) {
    let mut join = JoinSet::<()>::new();
    schedule_qps(shared, tx, deadline, qps, &mut join).await;
    while join.join_next().await.is_some() {}
}

async fn schedule_qps(
    shared: Arc<Shared>,
    tx: mpsc::UnboundedSender<Outcome>,
    deadline: Instant,
    qps: u64,
    join: &mut JoinSet<()>,
) {
    if qps == 0 {
        return;
    }
    let interval = Duration::from_nanos(1_000_000_000u64 / qps);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
        let shared = shared.clone();
        let tx = tx.clone();
        join.spawn(async move {
            let outcome = send_one(&shared).await;
            let _ = tx.send(outcome);
        });
        // Reap finished tasks to avoid unbounded JoinSet growth.
        while join.len() > 4096 {
            if join.try_join_next().is_none() {
                break;
            }
        }
    }
}

async fn run_concurrent(
    shared: Arc<Shared>,
    tx: mpsc::UnboundedSender<Outcome>,
    deadline: Instant,
    concurrency: usize,
) {
    let mut join = JoinSet::<()>::new();
    for _ in 0..concurrency {
        let shared = shared.clone();
        let tx = tx.clone();
        join.spawn(async move {
            loop {
                if Instant::now() >= deadline {
                    break;
                }
                let outcome = send_one(&shared).await;
                let _ = tx.send(outcome);
            }
        });
    }
    while join.join_next().await.is_some() {}
}

// ── Per-request send ────────────────────────────────────────────────────

async fn send_one(shared: &Shared) -> Outcome {
    SENT.fetch_add(1, Ordering::Relaxed);

    let key_index = rand::thread_rng().gen_range(0..shared.keys.len());
    let key = &shared.keys[key_index];
    shared.key_counts[key_index].fetch_add(1, Ordering::Relaxed);
    let prompt_len = if shared.args.prompt_min == shared.args.prompt_max {
        shared.args.prompt_min
    } else {
        rand::thread_rng().gen_range(shared.args.prompt_min..=shared.args.prompt_max)
    };
    let prompt = random_string(&shared.pool_bytes, prompt_len);

    let (path, body) = build_request(&shared.args, &prompt);
    let body_bytes = body.len() as u64;
    BYTES_OUT.fetch_add(body_bytes, Ordering::Relaxed);

    let mut req = shared.client.post(format!("{}{}", shared.args.target.trim_end_matches('/'), path));
    req = req.header("Content-Type", "application/json");
    req = match shared.args.auth_style.as_str() {
        "anthropic" => {
            req = req.header("x-api-key", key);
            req = req.header("anthropic-version", "2023-06-01");
            req
        }
        _ => req.header("Authorization", format!("Bearer {}", key)),
    };
    req = req.body(body);

    let start = Instant::now();
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return if e.is_timeout() { Outcome::Err(ErrKind::Timeout) }
                   else { Outcome::Err(ErrKind::Connect) };
        }
    };

    let status = resp.status().as_u16();
    if status == 429 { return Outcome::Err(ErrKind::RateLimited); }
    if status >= 500 { return Outcome::Err(ErrKind::Server5xx); }
    if status >= 400 { return Outcome::Err(ErrKind::Client4xx); }

    if shared.args.stream {
        consume_stream(resp, start).await
    } else {
        match resp.bytes().await {
            Ok(bytes) => {
                BYTES_IN.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
                    return Outcome::Err(ErrKind::Parse);
                }
                let e2e = start.elapsed();
                Outcome::Ok { ttft: e2e, e2e, bytes_in: bytes.len() as u64, bytes_out: body_bytes }
            }
            Err(e) => {
                if e.is_timeout() { Outcome::Err(ErrKind::Timeout) }
                else { Outcome::Err(ErrKind::Stream) }
            }
        }
    }
}

async fn consume_stream(resp: reqwest::Response, start: Instant) -> Outcome {
    let mut stream = resp.bytes_stream();
    let mut ttft_set = false;
    let mut ttft = Duration::ZERO;
    let mut total_bytes_in: u64 = 0;
    let mut saw_done = false;
    // Two end-of-stream signals are accepted:
    //   (1) `data: [DONE]` sentinel — sent by mock-backend directly, and by
    //       the gateway on error paths.
    //   (2) `"finish_reason":"stop"` (or any non-null value) in a chunk — the
    //       gateway's success-path streams terminate this way without sending
    //       `[DONE]`, so this is the primary signal when benching through the
    //       gateway.
    // Both signals are scanned across a rolling window because SSE chunks can
    // split either pattern across HTTP frame boundaries.
    let mut tail: Vec<u8> = Vec::with_capacity(32);
    const NEEDLE_DONE: &[u8] = b"data: [DONE]";
    const NEEDLE_FINISH: &[u8] = b"\"finish_reason\":\"";
    const NEEDLE_MSG_STOP: &[u8] = b"\"type\":\"message_stop\"";

    while let Some(frame) = stream.next().await {
        match frame {
            Ok(bytes) => {
                if !ttft_set {
                    ttft = start.elapsed();
                    ttft_set = true;
                }
                let n = bytes.len() as u64;
                total_bytes_in += n;
                BYTES_IN.fetch_add(n, Ordering::Relaxed);
                if !saw_done {
                    let max_needle = NEEDLE_DONE
                        .len()
                        .max(NEEDLE_FINISH.len())
                        .max(NEEDLE_MSG_STOP.len());
                    let window_start = tail.len().saturating_sub(max_needle - 1);
                    let scan_window: Vec<u8> = tail[window_start..]
                        .iter()
                        .chain(bytes.as_ref().iter())
                        .copied()
                        .collect();
                    if memmem(&scan_window, NEEDLE_DONE).is_some()
                        || has_terminal_finish_reason(&scan_window, NEEDLE_FINISH)
                        || memmem(&scan_window, NEEDLE_MSG_STOP).is_some()
                    {
                        saw_done = true;
                    }
                    tail.clear();
                    let take = bytes.len().min(32);
                    tail.extend_from_slice(&bytes.as_ref()[bytes.len() - take..]);
                }
            }
            Err(e) => {
                return if e.is_timeout() { Outcome::Err(ErrKind::Timeout) }
                       else { Outcome::Err(ErrKind::Stream) };
            }
        }
    }

    if !ttft_set { return Outcome::Err(ErrKind::Stream); }
    if !saw_done { return Outcome::Err(ErrKind::Stream); }
    let e2e = start.elapsed();
    Outcome::Ok { ttft, e2e, bytes_in: total_bytes_in, bytes_out: 0 }
}

fn memmem(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() { return None; }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Returns true if `window` contains `"finish_reason":"<value>"` where value
/// is not the literal `null`. The gateway terminates successful streams
/// with `"finish_reason":"stop"` (or `length`/`content_filter`/`tool_calls`)
/// without sending the `data: [DONE]` sentinel, so this is the primary
/// end-of-stream signal when benching through the gateway.
fn has_terminal_finish_reason(window: &[u8], needle: &[u8]) -> bool {
    let mut from = 0;
    while let Some(idx) = memmem(&window[from..], needle) {
        let abs = from + idx;
        let after = &window[abs + needle.len()..];
        match after.first() {
            Some(&b'n') => {
                // `null` — keep scanning past this occurrence.
                from = abs + needle.len();
            }
            Some(_) => return true,
            None => break,
        }
    }
    false
}

fn build_request(args: &Args, prompt: &str) -> (&'static str, String) {
    match args.format.as_str() {
        "anthropic" => {
            let body = json!({
                "model": args.model,
                "max_tokens": 1024,
                "messages": [{"role":"user","content":[{"type":"text","text": prompt}]}],
                "stream": args.stream
            });
            ("/v1/messages", body.to_string())
        }
        _ => {
            let body = json!({
                "model": args.model,
                "messages": [{"role":"user","content": prompt}],
                "stream": args.stream
            });
            ("/v1/chat/completions", body.to_string())
        }
    }
}

// ── Aggregator ──────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct Summary {
    duration_secs: f64,
    mode: String,
    format: String,
    auth_style: String,
    stream: bool,
    sent: u64,
    ok: u64,
    err_429: u64,
    err_5xx: u64,
    err_4xx: u64,
    err_timeout: u64,
    err_connect: u64,
    err_parse: u64,
    err_stream: u64,
    actual_qps: f64,
    success_qps: f64,
    bytes_in: u64,
    bytes_out: u64,
    keys_configured: usize,
    keys_used: usize,
    key_requests_min: u64,
    key_requests_max: u64,
    ttft: Percentiles,
    e2e: Percentiles,
}

#[derive(serde::Serialize, Default)]
struct Percentiles {
    p50_us: u64,
    p90_us: u64,
    p95_us: u64,
    p99_us: u64,
    p999_us: u64,
    max_us: u64,
    count: u64,
}

impl Percentiles {
    fn from_hist(h: &Histogram<u64>) -> Self {
        if h.len() == 0 {
            return Self::default();
        }
        Percentiles {
            p50_us: h.value_at_quantile(0.5),
            p90_us: h.value_at_quantile(0.9),
            p95_us: h.value_at_quantile(0.95),
            p99_us: h.value_at_quantile(0.99),
            p999_us: h.value_at_quantile(0.999),
            max_us: h.max(),
            count: h.len(),
        }
    }
}

async fn aggregate_stats(
    mut rx: mpsc::UnboundedReceiver<Outcome>,
    mode: Mode,
    args: Args,
    key_counts: Arc<Vec<AtomicU64>>,
) -> Summary {
    let mut ttft_hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
    let mut e2e_hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();

    let start = Instant::now();
    let mut live_ticker = tokio::time::interval(Duration::from_secs(1));
    live_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    live_ticker.tick().await;

    loop {
        tokio::select! {
            maybe_outcome = rx.recv() => {
                let Some(outcome) = maybe_outcome else { break; };
                record_outcome(outcome, &mut ttft_hist, &mut e2e_hist);
            }
            _ = live_ticker.tick() => {
                print_live(&ttft_hist, &e2e_hist, start);
            }
        }
    }

    // Drain remaining items.
    while let Ok(outcome) = rx.try_recv() {
        record_outcome(outcome, &mut ttft_hist, &mut e2e_hist);
    }

    let elapsed = start.elapsed().as_secs_f64().max(0.001);
    let ok = OK.load(Ordering::Relaxed);
    let key_request_counts = key_counts
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .collect::<Vec<_>>();
    Summary {
        duration_secs: elapsed,
        mode: format!("{:?}", mode),
        format: args.format,
        auth_style: args.auth_style,
        stream: args.stream,
        sent: SENT.load(Ordering::Relaxed),
        ok,
        err_429: ERR_429.load(Ordering::Relaxed),
        err_5xx: ERR_5XX.load(Ordering::Relaxed),
        err_4xx: ERR_4XX.load(Ordering::Relaxed),
        err_timeout: ERR_TMO.load(Ordering::Relaxed),
        err_connect: ERR_CNT.load(Ordering::Relaxed),
        err_parse: ERR_PRS.load(Ordering::Relaxed),
        err_stream: ERR_STR.load(Ordering::Relaxed),
        actual_qps: SENT.load(Ordering::Relaxed) as f64 / elapsed,
        success_qps: ok as f64 / elapsed,
        bytes_in: BYTES_IN.load(Ordering::Relaxed),
        bytes_out: BYTES_OUT.load(Ordering::Relaxed),
        keys_configured: key_request_counts.len(),
        keys_used: key_request_counts.iter().filter(|count| **count > 0).count(),
        key_requests_min: key_request_counts.iter().copied().min().unwrap_or(0),
        key_requests_max: key_request_counts.iter().copied().max().unwrap_or(0),
        ttft: Percentiles::from_hist(&ttft_hist),
        e2e: Percentiles::from_hist(&e2e_hist),
    }
}

fn record_outcome(outcome: Outcome, ttft: &mut Histogram<u64>, e2e: &mut Histogram<u64>) {
    match outcome {
        Outcome::Ok { ttft: t, e2e: e, bytes_in: _, bytes_out: _ } => {
            let _ = ttft.record(t.as_micros() as u64);
            let _ = e2e.record(e.as_micros() as u64);
            OK.fetch_add(1, Ordering::Relaxed);
        }
        Outcome::Err(k) => {
            let counter = match k {
                ErrKind::RateLimited => &ERR_429,
                ErrKind::Server5xx  => &ERR_5XX,
                ErrKind::Client4xx  => &ERR_4XX,
                ErrKind::Timeout    => &ERR_TMO,
                ErrKind::Connect    => &ERR_CNT,
                ErrKind::Parse      => &ERR_PRS,
                ErrKind::Stream     => &ERR_STR,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn print_live(ttft: &Histogram<u64>, e2e: &Histogram<u64>, start: Instant) {
    let sent = SENT.load(Ordering::Relaxed);
    let ok = OK.load(Ordering::Relaxed);
    let err429 = ERR_429.load(Ordering::Relaxed);
    let err5xx = ERR_5XX.load(Ordering::Relaxed);
    let errtmo = ERR_TMO.load(Ordering::Relaxed);
    let errcnt = ERR_CNT.load(Ordering::Relaxed);
    let elapsed = start.elapsed().as_secs_f64();
    let ttft_p50 = if ttft.len() > 0 { ttft.value_at_quantile(0.5) } else { 0 };
    let ttft_p99 = if ttft.len() > 0 { ttft.value_at_quantile(0.99) } else { 0 };
    let e2e_p50 = if e2e.len() > 0 { e2e.value_at_quantile(0.5) } else { 0 };
    let e2e_p99 = if e2e.len() > 0 { e2e.value_at_quantile(0.99) } else { 0 };
    println!(
        "[t+{:>5.1}s] sent={} ok={} 429={} 5xx={} tmo={} cnt={} | ttft p50={}ms p99={}ms | e2e p50={}ms p99={}ms | ok_qps={:.0}",
        elapsed,
        sent, ok, err429, err5xx, errtmo, errcnt,
        ttft_p50 / 1000, ttft_p99 / 1000,
        e2e_p50 / 1000, e2e_p99 / 1000,
        ok as f64 / elapsed.max(0.001),
    );
}

// ── Helpers ─────────────────────────────────────────────────────────────

const POOL_SIZE: usize = 1024 * 1024;
const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ";

fn generate_pool() -> Vec<u8> {
    let mut rng = rand::thread_rng();
    (0..POOL_SIZE).map(|_| CHARSET[rng.gen_range(0..CHARSET.len())]).collect()
}

fn random_string(pool: &[u8], len: usize) -> String {
    if len == 0 {
        return String::new();
    }
    let mut rng = rand::thread_rng();
    let start = rng.gen_range(0..pool.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(pool[(start + i) % pool.len()]);
    }
    String::from_utf8(out).unwrap()
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("ms") {
        return Ok(Duration::from_millis(n.parse().map_err(|_| "bad ms".to_string())?));
    }
    if let Some(n) = s.strip_suffix("s") {
        return Ok(Duration::from_secs(n.parse().map_err(|_| "bad secs".to_string())?));
    }
    if let Some(n) = s.strip_suffix("m") {
        return Ok(Duration::from_secs(60 * n.parse::<u64>().map_err(|_| "bad mins".to_string())?));
    }
    s.parse::<u64>().map(Duration::from_secs).map_err(|_| "unrecognized duration".to_string())
}
