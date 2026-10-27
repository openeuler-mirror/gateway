/// DDL for the response store tables (Response.md §6.8, ADR-02 checkpoint
/// model). Idempotent — safe on every startup.
pub fn response_store_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_response (
    id                   TEXT PRIMARY KEY,
    previous_response_id TEXT,
    key_hash             TEXT NOT NULL,
    model                TEXT NOT NULL,
    instructions         TEXT,
    tools_json           JSONB,
    history_item_ids     JSONB NOT NULL,
    status               TEXT NOT NULL,
    response_json        JSONB NOT NULL,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at           TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_boom_response_expires ON boom_response (expires_at);
CREATE INDEX IF NOT EXISTS idx_boom_response_key_hash ON boom_response (key_hash);

CREATE TABLE IF NOT EXISTS boom_response_item (
    id          TEXT NOT NULL,
    response_id TEXT NOT NULL REFERENCES boom_response(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    data        JSONB NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Composite PK: passthrough item ids are upstream-minted and only
    -- unique WITHIN one response (e.g. per-conversation counters) — a
    -- global single-column PK would fail the whole persist on collisions.
    PRIMARY KEY (response_id, id)
);

CREATE INDEX IF NOT EXISTS idx_boom_response_item_resp ON boom_response_item (response_id, seq);
"#
}

/// Run the response-store migration. Mirrors the other crates' pattern: a
/// dedicated connection with a best-effort `lock_timeout` (GaussDB may not
/// support it), then the idempotent DDL statements one by one.
pub async fn run(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    if let Err(e) = sqlx::query("SET lock_timeout = '10s'").execute(&mut *conn).await {
        tracing::warn!("SET lock_timeout failed on responses-store migration connection: {e}");
    }
    for stmt in response_store_ddl().split(';') {
        let trimmed = stmt.trim();
        if !trimmed.is_empty() {
            sqlx::query(trimmed).execute(&mut *conn).await?;
        }
    }
    Ok(())
}
