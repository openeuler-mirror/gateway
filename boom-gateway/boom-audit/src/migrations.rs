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

/// Run the request_log migration (DDL is idempotent).
pub async fn run_request_log_migration(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    // Split on ";" to execute each statement separately —
    // CREATE TABLE and each CREATE INDEX are individual statements.
    for stmt in request_log_ddl().split(';') {
        let trimmed = stmt.trim();
        if !trimmed.is_empty() {
            sqlx::query(trimmed).execute(pool).await?;
        }
    }
    // Add key_alias column to existing tables (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS key_alias TEXT"#,
    )
    .execute(pool)
    .await;
    // Add deployment_id column to existing tables (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS deployment_id TEXT"#,
    )
    .execute(pool)
    .await;
    // Add model_name column for resolved deployment model name (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS model_name TEXT"#,
    )
    .execute(pool)
    .await;
    let _ = sqlx::query(
        r#"CREATE INDEX IF NOT EXISTS idx_request_log_model_name ON boom_request_log(model_name)"#,
    )
    .execute(pool)
    .await;
    Ok(())
}
