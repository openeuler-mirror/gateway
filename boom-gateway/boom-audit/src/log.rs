use sqlx::PgPool;

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
    pub deployment_id: Option<String>,
}

/// Fire-and-forget: spawn a tokio task to INSERT the log record.
/// Does nothing if `pool` is None (no DB configured).
pub fn log_request(pool: Option<PgPool>, log: RequestLog) {
    if let Some(pool) = pool {
        tokio::spawn(async move {
            if let Err(e) = sqlx::query(
                r#"INSERT INTO boom_request_log
                   (request_id, key_hash, key_name, key_alias, team_id, model, model_name, api_path,
                    is_stream, status_code, error_type, error_message,
                    input_tokens, output_tokens, duration_ms, deployment_id)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)"#,
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
            .execute(&pool)
            .await
            {
                tracing::debug!("Failed to insert request log: {}", e);
            }
        });
    }
}
