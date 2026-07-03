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
///
/// Takes a pooled connection (not the pool) so the caller's `SET lock_timeout`
/// applies to every statement here — PgPool multiplexes queries across
/// connections, so a `SET` on one connection would NOT carry over to DDL run
/// on a different connection. The caller acquires a connection and sets
/// `lock_timeout = '10s'` before calling this.
pub async fn run_request_log_migration(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    // Split on ";" to execute each statement separately —
    // CREATE TABLE and each CREATE INDEX are individual statements.
    for stmt in request_log_ddl().split(';') {
        let trimmed = stmt.trim();
        if !trimmed.is_empty() {
            sqlx::query(trimmed).execute(&mut *conn).await?;
        }
    }
    // Add key_alias column to existing tables (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS key_alias TEXT"#,
    )
    .execute(&mut *conn)
    .await;
    // Add deployment_id column to existing tables (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS deployment_id TEXT"#,
    )
    .execute(&mut *conn)
    .await;
    // Add model_name column for resolved deployment model name (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS model_name TEXT"#,
    )
    .execute(&mut *conn)
    .await;
    // Add client_ip column for tracking client IP address (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS client_ip TEXT"#,
    )
    .execute(&mut *conn)
    .await;
    let _ = sqlx::query(
        r#"CREATE INDEX IF NOT EXISTS idx_request_log_model_name ON boom_request_log(model_name)"#,
    )
    .execute(&mut *conn)
    .await;
    // Add ttft_ms column for streaming Time-To-First-Token tracking (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS ttft_ms INTEGER"#,
    )
    .execute(&mut *conn)
    .await;
    // Composite index for dashboard time-window aggregations filtering by api_path.
    let _ = sqlx::query(
        r#"CREATE INDEX IF NOT EXISTS idx_request_log_created_apipath
           ON boom_request_log(created_at DESC, api_path)"#,
    )
    .execute(&mut *conn)
    .await;
    // cached_tokens: vLLM-reported real KV-cache hit (from final usage chunk).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS cached_tokens BIGINT"#,
    )
    .execute(&mut *conn)
    .await;
    // DFX scheduling observability — kept in a SEPARATE table to isolate it
    // from the main request log. Joined by request_id when needed (dashboard).
    let dfx_ddl = r#"
CREATE TABLE IF NOT EXISTS boom_request_dfx (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    request_id      TEXT NOT NULL,
    schedule_policy TEXT,
    kv_hit_blocks   BIGINT,
    kv_input_blocks BIGINT,
    trie_blocks     BIGINT,
    trie_max_blocks BIGINT,
    request_tokens  BIGINT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_request_dfx_request_id ON boom_request_dfx(request_id);
CREATE INDEX IF NOT EXISTS idx_request_dfx_created    ON boom_request_dfx(created_at DESC);
"#;
    for stmt in dfx_ddl.split(';') {
        let trimmed = stmt.trim();
        if !trimmed.is_empty() {
            // Propagate DDL errors (CREATE TABLE / CREATE INDEX) — if the
            // table fails to create, the downstream ALTERs and all DFX
            // inserts would silently fail. Match the boom_request_log DDL
            // handling above (which uses `?`).
            sqlx::query(trimmed).execute(&mut *conn).await?;
        }
    }
    // Migrate an existing boom_request_dfx table (created by an earlier
    // version with the old precomputed-ratio schema) to the raw-count schema:
    // add the new columns and drop the obsolete ratio column. No-op on a
    // fresh table that already has these columns.
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_dfx ADD COLUMN IF NOT EXISTS kv_hit_blocks BIGINT"#,
    )
    .execute(&mut *conn)
    .await;
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_dfx ADD COLUMN IF NOT EXISTS kv_input_blocks BIGINT"#,
    )
    .execute(&mut *conn)
    .await;
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_dfx ADD COLUMN IF NOT EXISTS trie_max_blocks BIGINT"#,
    )
    .execute(&mut *conn)
    .await;
    // Old precomputed-ratio column is now derived on read — drop it.
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_dfx DROP COLUMN IF EXISTS kv_hit_ratio"#,
    )
    .execute(&mut *conn)
    .await;
    Ok(())
}
