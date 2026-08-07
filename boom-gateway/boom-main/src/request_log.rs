use sqlx::PgPool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use boom_core::types::AuthIdentity;
use boom_core::types::Usage;
use boom_core::{DebugErrorEntry, GatewayError, LogDroppedCounter};
use crate::state::AppState;
use tokio::sync::mpsc;

/// Channel capacity — sized to absorb ~2s of DB outage at 50K QPS.
/// Single RequestLog ≈ 850 bytes → 100K slots × 850B ≈ 85 MB peak.
const LOG_CHANNEL_CAPACITY: usize = 100_000;
/// Max rows per batch INSERT. PG parameter limit (65535) / 25 columns ≈ 2621.
/// 2000 leaves headroom and aligns with flush interval at 50K QPS.
const LOG_BATCH_SIZE: usize = 2_000;
/// Flush window. At 50K QPS this batches ~5000 rows (3 INSERTs).
const LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
/// Per-batch INSERT timeout. 2000-row INSERT can take ~1s under load.
const LOG_INSERT_TIMEOUT: Duration = Duration::from_secs(10);

/// Dedup cache for expected rejections (rate-limit, concurrency, budget).
/// Key: "{error_type}:{key_hash}:{model}", auto-expires after 60 s.
/// Within the window, only the first rejection per (type, key, model) is written to DB.
static REJECTION_DEDUP: LazyLock<moka::sync::Cache<String, ()>> = LazyLock::new(|| {
    moka::sync::Cache::builder()
        .time_to_live(Duration::from_secs(60))
        .max_capacity(10_000)
        .build()
});

/// A single request log record.
pub struct RequestLog {
    pub request_id: Option<String>,
    pub key_hash: String,
    pub key_name: Option<String>,
    pub key_alias: Option<String>,
    pub team_id: Option<String>,
    pub model: String,
    pub model_name: Option<String>,
    pub api_path: String,
    pub is_stream: bool,
    pub status_code: u16,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub duration_ms: Option<i32>,
    pub ttft_ms: Option<i32>,
    pub deployment_id: Option<String>,
    pub client_ip: Option<String>,
    /// vLLM-reported real KV-cache hit (HBM + external store), from the final
    /// usage chunk's prompt_tokens_details.cached_tokens.
    pub cached_tokens: Option<i64>,
    // DFX scheduling observability — columns on boom_request_log.
    pub schedule_policy: Option<String>,
    pub kv_hit_blocks: Option<i64>,
    pub kv_input_blocks: Option<i64>,
    pub trie_blocks: Option<i64>,
    pub trie_max_blocks: Option<i64>,
    pub request_tokens: Option<i64>,
}

/// Background audit-log writer.
///
/// Replaces the previous per-request `tokio::spawn + sqlx::query` pattern
/// which competed with auth/routing for the shared 30-conn PgPool and silently
/// dropped logs at high QPS (the 5s timeout fired far more often than the
/// `tracing::warn!` revealed).
///
/// Architecture:
///   - Sender side (hot path): `try_send` into mpsc channel (100K capacity).
///     Non-blocking, returns immediately. On full channel, drops the incoming
///     log and bumps `dropped` counter — at 50K QPS this only triggers during
///     sustained DB outages >2s.
///   - Receiver side (single task): drains channel into a buffer, flushes every
///     100ms or when buffer hits 2000 rows. Uses a dedicated 8-conn PgPool so
///     log writes never compete with auth/routing. Batch INSERT (2000 rows in
///     one statement) amortizes DB round-trip from N to N/2000.
///   - Retry: one retry on failure (deadlock / transient DB error). Second
///     failure drops the batch and bumps `dropped`.
///
/// Lifecycle: spawned once at startup, lives for the process. On AppState
/// reload, the writer is preserved (DB pool itself survives reload — the
/// `Option<PgPool>` swap-in-place is fine).
pub struct LogWriter {
    tx: mpsc::Sender<RequestLog>,
    dropped: Arc<AtomicU64>,
}

impl LogWriter {
    /// Enqueue a log record. Non-blocking; on full channel the record is
    /// dropped and `dropped` counter incremented.
    ///
    /// Public API: `log_request(writer, log)` should be preferred over this
    /// to keep call sites readable.
    pub fn try_enqueue(&self, log: RequestLog) {
        match self.tx.try_send(log) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                // Channel closed = writer task died. Static flag would be cleaner,
                // but emitting on every drop is fine — at 50K QPS the log spam is
                // self-limiting because new logs aren't being enqueued either.
                tracing::error!("LogWriter channel closed — log_writer task not running");
            }
        }
    }
}

impl LogDroppedCounter for LogWriter {
    fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Spawn the background writer task and return a handle for enqueueing logs.
///
/// `pool` should be a *dedicated* PgPool (not the shared main pool) — log
/// writes must not compete with auth/routing for connections.
pub fn start_log_writer(pool: PgPool) -> Arc<LogWriter> {
    let (tx, rx) = mpsc::channel(LOG_CHANNEL_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let writer = Arc::new(LogWriter {
        tx,
        dropped: dropped.clone(),
    });
    tokio::spawn(log_writer_task(rx, pool, dropped));
    writer
}

async fn log_writer_task(
    mut rx: mpsc::Receiver<RequestLog>,
    pool: PgPool,
    dropped: Arc<AtomicU64>,
) {
    tracing::info!(
        channel_capacity = LOG_CHANNEL_CAPACITY,
        batch_size = LOG_BATCH_SIZE,
        flush_ms = LOG_FLUSH_INTERVAL.as_millis() as u64,
        "log_writer task started"
    );

    let mut buffer: Vec<RequestLog> = Vec::with_capacity(LOG_BATCH_SIZE);
    // Skip missed ticks — under load the channel arm fires more often than
    // the timer; default tokio interval would "catch up" with rapid flushes
    // we don't need.
    let mut flush_timer = tokio::time::interval(LOG_FLUSH_INTERVAL);
    flush_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Discard the immediate first tick (interval fires at t=0).
    flush_timer.tick().await;

    // Periodic stats logger — surfaces drop rate so operators can size the
    // channel vs. actual DB throughput. Every 60s.
    let mut stats_timer = tokio::time::interval(Duration::from_secs(60));
    stats_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    stats_timer.tick().await;
    let mut last_dropped: u64 = 0;

    loop {
        tokio::select! {
            // recv() returns None when all senders dropped — only happens at shutdown.
            Some(log) = rx.recv() => {
                buffer.push(log);
                if buffer.len() >= LOG_BATCH_SIZE {
                    flush_batch(&pool, &mut buffer, &dropped).await;
                }
            }
            _ = flush_timer.tick() => {
                if !buffer.is_empty() {
                    flush_batch(&pool, &mut buffer, &dropped).await;
                }
            }
            _ = stats_timer.tick() => {
                let total = dropped.load(Ordering::Relaxed);
                if total != last_dropped {
                    tracing::warn!(
                        total_dropped = total,
                        delta = total - last_dropped,
                        "audit log drops in past 60s (channel full or batch failures)",
                    );
                    last_dropped = total;
                }
            }
        }
    }
}

/// Drain `buffer` and INSERT all rows in a single batch statement.
/// One retry on transient failure; second failure drops the batch + counts.
async fn flush_batch(pool: &PgPool, buffer: &mut Vec<RequestLog>, dropped: &AtomicU64) {
    // Drain into an owned Vec — leaves `buffer` empty (capacity preserved)
    // so the next batch doesn't reallocate.
    let batch: Vec<RequestLog> = buffer.drain(..).collect();
    let batch_len = batch.len();

    for attempt in 0..=1 {
        let result = tokio::time::timeout(
            LOG_INSERT_TIMEOUT,
            batch_insert(pool, &batch),
        ).await;

        match result {
            Ok(Ok(rows_affected)) => {
                if (rows_affected as usize) < batch_len {
                    // PG can report fewer rows affected under rare conditions
                    // (e.g. trigger-suppressed INSERTs). Surface the gap.
                    let missing = batch_len - rows_affected as usize;
                    dropped.fetch_add(missing as u64, Ordering::Relaxed);
                    tracing::warn!(
                        batch_len,
                        rows_affected,
                        "Batch INSERT partial — {} logs not persisted",
                        missing,
                    );
                }
                return;
            }
            Ok(Err(e)) => {
                if attempt == 0 {
                    tracing::warn!(batch_len, error = %e, "Batch INSERT failed, retrying once");
                    continue;
                }
                tracing::error!(
                    batch_len,
                    error = %e,
                    "Batch INSERT failed twice, dropping {} logs",
                    batch_len,
                );
                dropped.fetch_add(batch_len as u64, Ordering::Relaxed);
                return;
            }
            Err(_) => {
                if attempt == 0 {
                    tracing::warn!(
                        batch_len,
                        timeout_secs = LOG_INSERT_TIMEOUT.as_secs(),
                        "Batch INSERT timed out, retrying once"
                    );
                    continue;
                }
                tracing::error!(
                    batch_len,
                    "Batch INSERT timed out twice, dropping {} logs",
                    batch_len,
                );
                dropped.fetch_add(batch_len as u64, Ordering::Relaxed);
                return;
            }
        }
    }
}

/// Build and execute a multi-row INSERT via sqlx QueryBuilder.
///
/// Uses `push_values` to construct `VALUES ($1,$2,...), ($26,...), ...` with
/// 25 placeholders per row. At LOG_BATCH_SIZE=2000, this is 50000 placeholders
/// — under PG's 65535 protocol limit.
async fn batch_insert(pool: &PgPool, batch: &[RequestLog]) -> Result<u64, sqlx::Error> {
    use sqlx::QueryBuilder;

    let mut qb: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(
        "INSERT INTO boom_request_log \
         (request_id, key_hash, key_name, key_alias, team_id, model, model_name, api_path, \
          is_stream, status_code, error_type, error_message, \
          input_tokens, output_tokens, duration_ms, deployment_id, client_ip, ttft_ms, \
          cached_tokens, schedule_policy, kv_hit_blocks, kv_input_blocks, \
          trie_blocks, trie_max_blocks, request_tokens) "
    );
    qb.push_values(batch.iter(), |mut b, log| {
        b.push_bind(log.request_id.clone())
            .push_bind(&log.key_hash)
            .push_bind(log.key_name.clone())
            .push_bind(log.key_alias.clone())
            .push_bind(log.team_id.clone())
            .push_bind(&log.model)
            .push_bind(log.model_name.clone())
            .push_bind(&log.api_path)
            .push_bind(log.is_stream)
            .push_bind(log.status_code as i16)
            .push_bind(log.error_type.clone())
            .push_bind(log.error_message.clone())
            .push_bind(log.input_tokens)
            .push_bind(log.output_tokens)
            .push_bind(log.duration_ms)
            .push_bind(log.deployment_id.clone())
            .push_bind(log.client_ip.clone())
            .push_bind(log.ttft_ms)
            .push_bind(log.cached_tokens)
            .push_bind(log.schedule_policy.clone())
            .push_bind(log.kv_hit_blocks)
            .push_bind(log.kv_input_blocks)
            .push_bind(log.trie_blocks)
            .push_bind(log.trie_max_blocks)
            .push_bind(log.request_tokens);
    });

    let result = qb.build().execute(pool).await?;
    Ok(result.rows_affected())
}

/// Enqueue a log record through the writer. Public API.
///
/// Replaces the old `log_request(pool, log)` which spawned a per-request task.
/// The hot path now does one mpsc `try_send` (sub-microsecond) and returns.
///
/// `None` writer (no DB configured) is a no-op, matching the prior behavior
/// of `if let Some(pool) = pool { ... }`.
pub fn log_request(writer: Option<&LogWriter>, log: RequestLog) {
    if let Some(w) = writer {
        w.try_enqueue(log);
    }
}

/// Helper to log an error from a route handler. Call this before returning the error.
/// `request_body` is an optional serialized request JSON for debug recording.
pub fn log_error(
    state: &AppState,
    identity: &AuthIdentity,
    model: &str,
    api_path: &str,
    is_stream: bool,
    start: Instant,
    error: &GatewayError,
    request_id: Option<String>,
    deployment_id: Option<String>,
    request_body: Option<String>,
    client_ip: Option<String>,
) {
    log_error_with_usage(
        state,
        identity,
        model,
        api_path,
        is_stream,
        start,
        error,
        request_id,
        deployment_id,
        request_body,
        client_ip,
        None,
    );
}

/// Log an auth-phase error from the extractor, before `AuthIdentity` exists.
///
/// Constructs a minimal identity keyed by `hash_token(raw_key)` — enough to
/// populate `boom_request_log` (whose `key_hash` column is NOT NULL) and to
/// drive `REJECTION_DEDUP` for `KeyExpired` / `KeyBlocked` / `BudgetExceeded`.
///
/// `AuthError` (401) is **not** deduped — every auth failure is logged in
/// full for security visibility. The `model` column is filled with the
/// placeholder `"<auth>"` because the request hasn't been routed yet and
/// the real model is unknown.
///
/// `raw_key` is the original key string the client sent (before any
/// `pre_auth` hook replacement). Hashing it here is consistent with
/// `DbAuthenticator::hash_token`, so dedup keys line up with the
/// in-DB `key_hash` column for the same key.
pub fn log_auth_error(
    state: &AppState,
    raw_key: &str,
    api_path: &str,
    start: Instant,
    error: &GatewayError,
    request_id: Option<String>,
    client_ip: Option<String>,
) {
    let key_hash = boom_auth::DbAuthenticator::hash_token(raw_key);
    let identity = AuthIdentity {
        key_hash,
        key_name: None,
        key_alias: None,
        user_id: None,
        team_id: None,
        team_alias: None,
        models: vec![],
        team_models: vec![],
        rpm_limit: None,
        tpm_limit: None,
        max_budget: None,
        spend: 0.0,
        blocked: false,
        expires_at: None,
        metadata: serde_json::Value::Null,
    };

    log_error_with_usage(
        state,
        &identity,
        "<auth>",
        api_path,
        false,
        start,
        error,
        request_id,
        None,
        None,
        client_ip,
        None,
    );

    // AuthError (401) is not a dedup member, so log_error_with_usage does
    // not emit a tracing line for it — emit one here so failed auth shows
    // up in the console log unconditionally.
    if matches!(error, GatewayError::AuthError(_)) {
        tracing::warn!(
            status_code = error.status_code(),
            error_type = error.error_type(),
            key_hash = %identity.key_hash,
            "{:.80}",
            error.to_string()
        );
    }
}

/// Error logging variant for composite providers that may have completed
/// successful child calls before the parent request failed.
pub fn log_error_with_usage(
    state: &AppState,
    identity: &AuthIdentity,
    model: &str,
    api_path: &str,
    is_stream: bool,
    start: Instant,
    error: &GatewayError,
    request_id: Option<String>,
    deployment_id: Option<String>,
    request_body: Option<String>,
    client_ip: Option<String>,
    usage: Option<&Usage>,
) {
    if error.should_dedup_log() {
        let dedup_key = format!("{}:{}:{}", error.error_type(), identity.key_hash, model);
        if REJECTION_DEDUP.get(&dedup_key).is_some() {
            // Deduplicated — skip both DB log and console output.
            return;
        }
        REJECTION_DEDUP.insert(dedup_key, ());
        // First rejection in this window — log to console and DB.
        tracing::warn!(
            status_code = error.status_code(),
            error_type = error.error_type(),
            key = identity.key_alias.as_deref().or(identity.key_name.as_deref()).unwrap_or("-"),
            model = model,
            "{:.80}",
            error.to_string()
        );
    }

    log_request(
        state.log_writer.as_deref(),
        RequestLog {
            request_id: request_id.clone(),
            key_hash: identity.key_hash.clone(),
            key_name: identity.key_name.clone(),
            key_alias: identity.key_alias.clone(),
            team_id: identity.team_id.clone(),
            model: model.to_string(),
            model_name: None,
            api_path: api_path.to_string(),
            is_stream,
            status_code: error.status_code(),
            error_type: Some(error.error_type().to_string()),
            error_message: Some(error.to_string()),
            input_tokens: usage.map(|usage| {
                usage.prompt_tokens.min(i32::MAX as u32) as i32
            }),
            output_tokens: usage.map(|usage| {
                usage.completion_tokens.min(i32::MAX as u32) as i32
            }),
            duration_ms: Some(start.elapsed().as_millis() as i32),
            ttft_ms: None,
            deployment_id,
            client_ip: client_ip.clone(),
            cached_tokens: usage
                .and_then(|usage| usage.prompt_tokens_details.as_ref())
                .and_then(|details| details.cached_tokens)
                .map(i64::from),
            schedule_policy: None,
            kv_hit_blocks: None,
            kv_input_blocks: None,
            trie_blocks: None,
            trie_max_blocks: None,
            request_tokens: None,
        },
    );

    // Debug recording — capture upstream errors with full request body.
    if state.debug_store.is_enabled() && error.should_log_to_db() {
        let error_type = error.error_type();
        if error_type == "upstream_error" || error_type == "provider_error" || error_type == "timeout" {
            let (upstream_status, upstream_body) = match error {
                GatewayError::UpstreamError { status, message } => {
                    (Some(*status), Some(message.clone()))
                }
                _ => (None, None),
            };

            let rid = request_id.unwrap_or_default();
            if !rid.is_empty() {
                state.debug_store.record(DebugErrorEntry {
                    request_id: rid,
                    key_hash: identity.key_hash.clone(),
                    key_alias: identity.key_alias.clone(),
                    model: model.to_string(),
                    api_path: api_path.to_string(),
                    is_stream,
                    created_at: chrono::Utc::now().to_rfc3339(),
                    status_code: error.status_code(),
                    error_type: error_type.to_string(),
                    error_message: error.to_string(),
                    upstream_status,
                    upstream_body,
                    request_body,
                });
            }
        }
    }
}
