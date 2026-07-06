/// DDL for boom_request_log table + indexes.
pub fn request_log_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_request_log (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    request_id     TEXT,
    key_hash       TEXT NOT NULL,
    key_name       TEXT,
    key_alias      TEXT,
    team_id        TEXT,
    model          TEXT NOT NULL,
    api_path       TEXT NOT NULL,
    is_stream      BOOLEAN NOT NULL DEFAULT false,
    status_code    SMALLINT NOT NULL DEFAULT 200,
    error_type     TEXT,
    error_message  TEXT,
    input_tokens   INTEGER,
    output_tokens  INTEGER,
    duration_ms    INTEGER,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_request_log_created ON boom_request_log(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_request_log_key_hash ON boom_request_log(key_hash);
CREATE INDEX IF NOT EXISTS idx_request_log_model ON boom_request_log(model);
"#
}

/// Run the request_log migration on a single connection (DDL is idempotent).
///
/// The caller is expected to have set `lock_timeout` on `conn` already —
/// `boom_dashboard::migrations::run_migrations` does this for the whole
/// migration sequence. All `boom_request_log` DDL lives here, the sole owner
/// of that table per the architecture rules.
pub async fn run_request_log_migration(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    // CREATE TABLE + base indexes.
    for stmt in request_log_ddl().split(';') {
        let trimmed = stmt.trim();
        if !trimmed.is_empty() {
            sqlx::query(trimmed).execute(&mut *conn).await?;
        }
    }
    // key_alias: litellm-compatible alias for display.
    execute_alter(
        conn,
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS key_alias TEXT"#,
    )
    .await;
    // deployment_id: resolved deployment id at request time.
    execute_alter(
        conn,
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS deployment_id TEXT"#,
    )
    .await;
    // model_name: resolved deployment model name (may differ from requested model).
    execute_alter(
        conn,
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS model_name TEXT"#,
    )
    .await;
    // client_ip: tracking the originating client IP.
    execute_alter(
        conn,
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS client_ip TEXT"#,
    )
    .await;
    let _ = sqlx::query(
        r#"CREATE INDEX IF NOT EXISTS idx_request_log_model_name ON boom_request_log(model_name)"#,
    )
    .execute(&mut *conn)
    .await;
    // ttft_ms: streaming Time-To-First-Token.
    execute_alter(
        conn,
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS ttft_ms INTEGER"#,
    )
    .await;
    // Composite index for dashboard time-window aggregations filtering by api_path.
    let _ = sqlx::query(
        r#"CREATE INDEX IF NOT EXISTS idx_request_log_created_apipath
           ON boom_request_log(created_at DESC, api_path)"#,
    )
    .execute(&mut *conn)
    .await;
    // cached_tokens: vLLM-reported real KV-cache hit (prompt_tokens_details.cached_tokens).
    execute_alter(
        conn,
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS cached_tokens BIGINT"#,
    )
    .await;
    Ok(())
}

/// Run an idempotent `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` best-effort:
/// returns Ok even if the DB refuses the statement (some PG-compatible
/// backends reject `IF NOT EXISTS`). Errors on the base `CREATE TABLE`
/// above are propagated, but column additions are non-fatal.
async fn execute_alter(conn: &mut sqlx::PgConnection, sql: &str) {
    let _ = sqlx::query(sql).execute(&mut *conn).await;
}
