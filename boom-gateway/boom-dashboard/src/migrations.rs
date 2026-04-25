use sqlx::PgPool;

/// Run database migrations for all BooMGateway persistence tables.
///
/// DDL definitions are owned by their respective crates:
/// - boom_audit:      boom_request_log
/// - boom_routing:    boom_model_deployment, boom_model_alias
/// - boom_limiter:    boom_rate_limit_state, boom_key_plan_assignment, boom_rate_limit_plan
/// - boom_dashboard:  boom_config (generic KV store)
///
/// Called once at startup; uses CREATE TABLE IF NOT EXISTS for idempotency.
/// Requires PostgreSQL 13+ (for built-in gen_random_uuid()).
///
/// All DDL runs on a **single acquired connection** with `lock_timeout` set,
/// so that stale locks from previous crashed sessions don't cause indefinite hangs.
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Acquire a dedicated connection and set lock_timeout on it.
    // This ensures ALL migration statements run on the same connection
    // with the timeout active — PgPool multiplexes queries across connections,
    // so SET on one connection does NOT affect others.
    let mut conn = pool.acquire().await?;
    sqlx::query("SET lock_timeout = '10s'")
        .execute(&mut *conn)
        .await?;

    // 1. Request logs (boom-audit).
    tracing::info!("Migration 1/7: request_log...");
    run_ddl_on_conn(&mut conn, boom_audit::migrations::request_log_ddl()).await?;
    // Add key_alias column (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS key_alias TEXT"#,
    )
    .execute(&mut *conn)
    .await;
    // Add deployment_id column (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS deployment_id TEXT"#,
    )
    .execute(&mut *conn)
    .await;
    // Add model_name column for resolved deployment name (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_request_log ADD COLUMN IF NOT EXISTS model_name TEXT"#,
    )
    .execute(&mut *conn)
    .await;
    let _ = sqlx::query(
        r#"CREATE INDEX IF NOT EXISTS idx_request_log_model_name ON boom_request_log(model_name)"#,
    )
    .execute(&mut *conn)
    .await;
    tracing::info!("Migration 1/7: done");

    // 2. Model deployments + aliases (boom-routing).
    tracing::info!("Migration 2/7: deployment...");
    run_ddl_on_conn(&mut conn, boom_routing::migrations::deployment_ddl()).await?;
    // Add deployment_id column to existing tables (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_model_deployment ADD COLUMN IF NOT EXISTS deployment_id TEXT"#,
    )
    .execute(&mut *conn)
    .await;
    // Add quota_count_ratio column (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_model_deployment ADD COLUMN IF NOT EXISTS quota_count_ratio BIGINT DEFAULT 1"#,
    )
    .execute(&mut *conn)
    .await;
    // Add auto_disabled column (no-op if already present).
    let _ = sqlx::query(
        boom_routing::migrations::migration_add_auto_disabled(),
    )
    .execute(&mut *conn)
    .await;
    // Add flow control columns (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_model_deployment ADD COLUMN IF NOT EXISTS max_inflight_queue_len INTEGER"#,
    )
    .execute(&mut *conn)
    .await;
    let _ = sqlx::query(
        r#"ALTER TABLE boom_model_deployment ADD COLUMN IF NOT EXISTS max_context_len BIGINT"#,
    )
    .execute(&mut *conn)
    .await;
    tracing::info!("Migration 2/7: done");
    tracing::info!("Migration 3/7: alias...");
    run_ddl_on_conn(&mut conn, boom_routing::migrations::alias_ddl()).await?;
    tracing::info!("Migration 3/7: done");

    // 3. Rate limit state + assignments + plans (boom-limiter).
    tracing::info!("Migration 4/7: rate_limit_state...");
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::rate_limit_state_ddl()).await?;
    tracing::info!("Migration 4/7: done");
    tracing::info!("Migration 5/7: assignment...");
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::assignment_ddl()).await?;
    tracing::info!("Migration 5/7: done");
    tracing::info!("Migration 6/7: plan...");
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::plan_ddl()).await?;
    tracing::info!("Migration 6/7: done");

    // 4. KV config store (dashboard-owned).
    tracing::info!("Migration 7/8: boom_config...");
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS boom_config (
            key         TEXT PRIMARY KEY,
            value       JSONB NOT NULL,
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    )
    .execute(&mut *conn)
    .await?;
    tracing::info!("Migration 7/8: done");

    // 5. Team table (dashboard-owned).
    tracing::info!("Migration 8/8: boom_team_table...");
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS boom_team_table (
            team_id     TEXT PRIMARY KEY,
            team_alias  TEXT,
            models      TEXT[] DEFAULT '{}',
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    )
    .execute(&mut *conn)
    .await?;
    tracing::info!("Migration 8/8: done");

    // 6. Verification token table (boom-auth).
    tracing::info!("Migration 9/9: boom_verification_token...");
    run_ddl_on_conn(&mut conn, verification_token_ddl()).await?;
    tracing::info!("Migration 9/9: done");

    // Connection returns to pool on drop.
    drop(conn);

    tracing::info!("BooMGateway persistence tables ensured (9 tables)");
    Ok(())
}

/// Execute a multi-statement DDL string on a single connection.
async fn run_ddl_on_conn(
    conn: &mut sqlx::PgConnection,
    ddl: &str,
) -> Result<(), sqlx::Error> {
    for stmt in ddl.split(';') {
        let trimmed = stmt.trim();
        if !trimmed.is_empty() {
            sqlx::query(trimmed).execute(&mut *conn).await?;
        }
    }
    Ok(())
}

/// DDL for `boom_verification_token` (boom-auth table).
/// Defined here because boom-dashboard cannot depend on boom-auth per architecture rules.
fn verification_token_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS "boom_verification_token" (
    token TEXT PRIMARY KEY,
    key_name TEXT,
    key_alias TEXT,
    user_id TEXT,
    team_id TEXT,
    models TEXT[] DEFAULT '{}',
    aliases JSONB,
    config JSONB,
    spend DOUBLE PRECISION DEFAULT 0.0,
    expires TIMESTAMP,
    blocked BOOLEAN DEFAULT false,
    max_parallel_requests INTEGER,
    tpm_limit BIGINT,
    rpm_limit BIGINT,
    max_budget DOUBLE PRECISION,
    budget_duration TEXT,
    budget_reset_at TIMESTAMP,
    allowed_cache_controls TEXT[] DEFAULT '{}',
    allowed_routes TEXT[] DEFAULT '{}',
    metadata JSONB DEFAULT '{}',
    model_spend JSONB,
    model_max_budget JSONB,
    budget_id TEXT,
    organization_id TEXT,
    created_at TIMESTAMP DEFAULT NOW(),
    created_by TEXT,
    updated_at TIMESTAMP DEFAULT NOW()
)
"#
}
