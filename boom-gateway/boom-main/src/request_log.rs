use sqlx::PgPool;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use boom_core::types::AuthIdentity;
use boom_core::{DebugErrorEntry, GatewayError};
use crate::state::AppState;

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
    /// DFX scheduling observability. Stored in a separate `boom_request_dfx`
    /// table (isolated from the main request log); None when no routing/dfx
    /// info applies (e.g. errors before routing, or non-kvc_aware policies).
    pub dfx: Option<RequestDfx>,
}

/// DFX scheduling observability for a single request. Persisted to
/// `boom_request_dfx`, joined back to `boom_request_log` by `request_id`.
pub struct RequestDfx {
    /// Which policy handled the request (kvc_aware / key_affinity / round_robin).
    pub schedule_policy: Option<String>,
    /// Raw hit blocks (matched prefix blocks) for the request.
    pub kv_hit_blocks: Option<i64>,
    /// Raw input blocks (total prefix blocks in the request).
    pub kv_input_blocks: Option<i64>,
    /// Raw trie block count at request time (how full the trie was).
    pub trie_blocks: Option<i64>,
    /// Trie capacity (max_blocks config) at request time, so the fill % can be
    /// derived at read time as trie_blocks / trie_max_blocks.
    pub trie_max_blocks: Option<i64>,
    /// Token count of the request prompt.
    pub request_tokens: Option<i64>,
}

/// Fire-and-forget: spawn a tokio task to INSERT the log record.
/// Does nothing if `pool` is None (no DB configured).
/// Includes a 5s timeout to prevent log writes from starving the connection pool.
pub fn log_request(pool: Option<PgPool>, log: RequestLog) {
    if let Some(pool) = pool {
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sqlx::query(
                    r#"INSERT INTO boom_request_log
                       (request_id, key_hash, key_name, key_alias, team_id, model, model_name, api_path,
                        is_stream, status_code, error_type, error_message,
                        input_tokens, output_tokens, duration_ms, deployment_id, client_ip, ttft_ms,
                        cached_tokens)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)"#,
                )
                .bind(&log.request_id)
                .bind(&log.key_hash)
                .bind(&log.key_name)
                .bind(&log.key_alias)
                .bind(&log.team_id)
                .bind(&log.model)
                .bind(&log.model_name)
                .bind(&log.api_path)
                .bind(log.is_stream)
                .bind(log.status_code as i16)
                .bind(&log.error_type)
                .bind(&log.error_message)
                .bind(log.input_tokens)
                .bind(log.output_tokens)
                .bind(log.duration_ms)
                .bind(&log.deployment_id)
                .bind(&log.client_ip)
                .bind(log.ttft_ms)
                .bind(log.cached_tokens)
                .execute(&pool),
            )
            .await;

            match result {
                Ok(Err(e)) => tracing::debug!("Failed to insert request log: {}", e),
                Err(_) => tracing::warn!("Request log write timed out after 5s, dropping"),
                Ok(Ok(_)) => {}
            }

            // Best-effort DFX write to the isolated boom_request_dfx table.
            // Joined back by request_id. Failures here must not affect the main
            // log record (already written above) — only a debug log line.
            if let (Some(dfx), Some(rid)) = (log.dfx.as_ref(), log.request_id.as_ref()) {
                if let Err(e) = sqlx::query(
                    r#"INSERT INTO boom_request_dfx
                       (request_id, schedule_policy, kv_hit_blocks, kv_input_blocks,
                        trie_blocks, trie_max_blocks, request_tokens)
                       VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
                )
                .bind(rid)
                .bind(&dfx.schedule_policy)
                .bind(dfx.kv_hit_blocks)
                .bind(dfx.kv_input_blocks)
                .bind(dfx.trie_blocks)
                .bind(dfx.trie_max_blocks)
                .bind(dfx.request_tokens)
                .execute(&pool)
                .await
                {
                    // warn (not debug): a persistent failure here surfaces as
                    // "Prefix Hit Rate has no data" in the dashboard, which
                    // looks like an upstream issue but is really this write
                    // failing. Make it visible.
                    tracing::warn!("Failed to insert request dfx: {}", e);
                }
            }
        });
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
    if !error.should_log_to_db() {
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
        state.db_pool.clone(),
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
            input_tokens: None,
            output_tokens: None,
            duration_ms: Some(start.elapsed().as_millis() as i32),
            ttft_ms: None,
            deployment_id,
            client_ip: client_ip.clone(),
            cached_tokens: None,
            dfx: None,
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
