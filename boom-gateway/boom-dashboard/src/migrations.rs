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
    // Set lock_timeout on this connection so ALTER TABLE / CREATE INDEX can't
    // hang indefinitely on a GaussDB distributed table with stale locks from
    // a crashed previous process. Some PG-compatible databases (notably
    // openGauss) don't support lock_timeout — log + continue, since most
    // ALTERs in our migrations are idempotent no-ops in steady state.
    if let Err(e) = sqlx::query("SET lock_timeout = '10s'")
        .execute(&mut *conn)
        .await
    {
        tracing::warn!(
            "SET lock_timeout failed on migration connection (continuing without timeout protection): {}",
            e
        );
    }

    // 1. Request logs (boom-audit owns all boom_request_log DDL).
    tracing::info!("Migration 1/7: request_log...");
    boom_audit::migrations::run_request_log_migration(&mut conn).await?;
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
    // Add client_type_header column (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_model_deployment ADD COLUMN IF NOT EXISTS client_type_header BOOLEAN NOT NULL DEFAULT false"#,
    )
    .execute(&mut *conn)
    .await;
    tracing::info!("Migration 2/7: done");
    tracing::info!("Migration 3/7: alias...");
    run_ddl_on_conn(&mut conn, boom_routing::migrations::alias_ddl()).await?;
    tracing::info!("Migration 3/7: done");

    // 3. Rate limit state + assignments + plans (boom-limiter).
    tracing::info!("Migration 4/13: rate_limit_state...");
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::rate_limit_state_ddl()).await?;
    tracing::info!("Migration 4/13: done");
    tracing::info!("Migration 5/13: assignment...");
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::assignment_ddl()).await?;
    tracing::info!("Migration 5/13: done");
    tracing::info!("Migration 6/13: plan...");
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::plan_ddl()).await?;
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::plan_alter_ddl()).await?;
    tracing::info!("Migration 6/13: done");
    tracing::info!("Migration 6b/13: team_assignment...");
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::team_assignment_ddl()).await?;
    tracing::info!("Migration 6b/13: done");
    tracing::info!("Migration 6c/13: quota cumulative...");
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::cumulative_ddl()).await?;
    // Idempotent ALTER: add tokens + costs_micros columns to existing tables
    // (no-op if already present). New tables created by cumulative_ddl() above
    // already include them.
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::state_alter_ddl()).await?;
    // GaussDB distributed: mark as REPLICATION so the table is copied to
    // every datanode. Without this, single-table queries still get routed
    // to a datanode where the table doesn't exist ("relation does not exist
    // on datanode"). Idempotent — REPLICATION is a no-op if already set.
    // Errors are warned (not fatal): vanilla Postgres / single-node
    // openGauss don't support DISTRIBUTE BY syntax.
    match sqlx::query(
        "ALTER TABLE boom_rate_limit_cumulative DISTRIBUTE BY REPLICATION",
    )
    .execute(&mut *conn)
    .await
    {
        Ok(_) => tracing::info!("Migration 6c: ALTER DISTRIBUTE BY REPLICATION ok"),
        Err(e) => tracing::warn!(
            "Migration 6c: ALTER DISTRIBUTE BY REPLICATION failed (continuing): {}",
            e
        ),
    }
    tracing::info!("Migration 6c/13: done");

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
///
/// Uses sqlx's raw simple-query protocol so PL/pgSQL `DO $$ ... $$` blocks
/// (which contain semicolons) are sent to the server as one unit instead of
/// being split client-side. The previous `split(';')` approach broke dollar
/// quoting — `DO $$ BEGIN ... ; ... END $$` got cut into unterminated pieces.
async fn run_ddl_on_conn(
    conn: &mut sqlx::PgConnection,
    ddl: &str,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(ddl).execute(&mut *conn).await?;
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
