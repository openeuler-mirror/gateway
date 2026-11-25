use sqlx::PgPool;

/// Run database migrations for all BooMGateway persistence tables.
///
/// DDL definitions are owned by their respective crates:
/// - boom_audit:      boom_request_log
/// - boom_routing:    boom_model_deployment, boom_model_alias
/// - boom_limiter:    boom_rate_limit_state, boom_key_plan_assignment, boom_rate_limit_plan
/// - boom_dashboard:  boom_config (generic KV store)
///
/// Called once at startup; uses CREATE TABLE IF NOT EXISTS for idempotency,
/// guarded by catalog probes (plain SELECTs) so warm restarts issue no DDL
/// at all on push-hot tables. See `table_missing` for why that matters.
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
    if columns_missing(&mut conn, "boom_model_deployment", &["request_defaults"]).await {
        run_ddl_on_conn(&mut conn, boom_routing::migrations::migration_add_request_defaults()).await?;
    }
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
    // Add serve_not_match column (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_model_deployment ADD COLUMN IF NOT EXISTS serve_not_match BOOLEAN NOT NULL DEFAULT false"#,
    )
    .execute(&mut *conn)
    .await;
    // Add model_info column as JSONB for cost metadata (no-op if already present).
    let _ = sqlx::query(
        r#"ALTER TABLE boom_model_deployment ADD COLUMN IF NOT EXISTS model_info JSONB"#,
    )
    .execute(&mut *conn)
    .await;
    // Add allowed_teams column as JSONB team ACL (no-op if already present).
    // NULL = public; JSONB array of team_ids = private; [] = locked.
    let _ = sqlx::query(
        boom_routing::migrations::migration_add_allowed_teams(),
    )
    .execute(&mut *conn)
    .await;
    // Add the unified visibility column (no legacy-row UPDATE — see the
    // migration fn doc for why a boot-time flip was removed).
    // MUST go through run_ddl_on_conn (raw_sql simple-query protocol).
    // sqlx::query() uses the prepared protocol, which rejects multi-command
    // strings; combined with the previous `let _ =` error-swallowing that
    // left the column uncreated while every DeploymentRow SELECT referenced
    // it ("visibility" does not exist). Propagates with `?` — startup
    // should fail loudly when a hard-required column can't be added.
    run_ddl_on_conn(&mut conn, boom_routing::migrations::migration_add_visibility()).await?;
    // Add max_context_tokens (rollback tiering capability, tokens). Same
    // run_ddl_on_conn + `?` discipline as visibility: DeploymentRow SELECTs
    // reference the column, so a failed add must fail startup loudly.
    run_ddl_on_conn(&mut conn, boom_routing::migrations::migration_add_max_context_tokens()).await?;
    // Intent-carrying drop of rollback_enabled (pool toggle removal):
    // clears declarations in pools that had rollback disabled, then drops
    // the column. No-op on fresh DBs.
    boom_routing::migrations::migration_drop_rollback_enabled(&mut conn).await?;
    // Backfill deployment_id for legacy rows written before insert-path
    // auto-generation (NULL/''): gives them a stable persisted identity for
    // flow-control slots and kv affinity. Idempotent — fires at most once
    // per row. Warn-and-continue: a failed backfill only degrades those rows
    // to host-derived kv identity, it must not block startup.
    match sqlx::query(boom_routing::migrations::migration_backfill_deployment_id())
        .execute(&mut *conn)
        .await
    {
        Ok(res) => {
            let n = res.rows_affected();
            if n > 0 {
                tracing::info!(
                    "Migration 2: backfilled deployment_id for {} legacy row(s)",
                    n
                );
            }
        }
        Err(e) => tracing::warn!(
            "Migration 2: deployment_id backfill failed (continuing): {}",
            e
        ),
    }
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
    // Idempotent: relax plan_name to NULL so "explicit no-plan" is distinct
    // from "never configured". See assignment_alter_ddl doc. Probe-first: the
    // ALTER takes an AccessExclusiveLock even as a no-op, and other
    // instances reconcile this table on their 5s revision poll.
    if column_not_nullable(&mut conn, "boom_key_plan_assignment", "plan_name").await {
        run_ddl_on_conn(&mut conn, boom_limiter::migrations::assignment_alter_ddl()).await?;
    }
    tracing::info!("Migration 5/13: done");
    tracing::info!("Migration 6/13: plan...");
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::plan_ddl()).await?;
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::plan_alter_ddl()).await?;
    tracing::info!("Migration 6/13: done");
    tracing::info!("Migration 6b/13: team_assignment...");
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::team_assignment_ddl()).await?;
    tracing::info!("Migration 6b/13: done");
    tracing::info!("Migration 6c/13: quota cumulative...");
    // Probe-first: every statement below is a no-op on a warm DB, but DDLs
    // take an AccessExclusiveLock even to discover that — and these tables
    // are push-hot (each instance's delta-push transaction touches them
    // once per quota_sync interval), so an unprobed restart landing inside
    // another instance's push window silently queues for the transaction's
    // whole remaining lifetime. Catalog probes are plain SELECTs and never
    // queue; DDLs run only when the shape check says they are actually
    // needed (fresh DB / upgrade / drift repair).
    if table_missing(&mut conn, "boom_rate_limit_cumulative").await {
        run_ddl_on_conn(&mut conn, boom_limiter::migrations::cumulative_ddl()).await?;
    }
    // Add tokens + costs_micros columns to pre-refactor databases (fresh
    // tables from Migration 4's base DDL already include them).
    if columns_missing(
        &mut conn,
        "boom_rate_limit_state",
        &["tokens", "costs_micros"],
    )
    .await
    {
        run_ddl_on_conn(&mut conn, boom_limiter::migrations::state_alter_ddl()).await?;
    }
    // Widen the legacy cache_key PK to (cache_key, window_start) — required
    // by the multi-instance delta protocol (per-generation rows). Self-
    // probing DO block (catalog read, no table lock when already composite);
    // errors propagate.
    run_ddl_on_conn(&mut conn, boom_limiter::migrations::state_pk_alter_ddl()).await?;
    // Delta-push watermark table (idempotent incremental sync).
    if table_missing(&mut conn, "boom_quota_delta").await {
        run_ddl_on_conn(&mut conn, boom_limiter::migrations::quota_delta_ddl()).await?;
    }
    // GaussDB distributed: mark as REPLICATION so the table is copied to
    // every datanode. Without this, single-table queries still get routed
    // to a datanode where the table doesn't exist ("relation does not exist
    // on datanode"). Probe-first: a pgxc_class row for the table means a
    // previous boot already set the distribution (REPLICATION is the only
    // value this code sets), so the ALTER's exclusive lock on the
    // push-hot cumulative table is only paid on the first distributed boot.
    // No pgxc_class at all = not a distributed deployment (vanilla
    // Postgres / single-node openGauss): the ALTER would be a guaranteed
    // syntax error, so skip quietly instead of warning on every boot.
    match pgxc_class_state(&mut conn).await {
        PgxcClass::AlreadyDistributed => {
            tracing::debug!("Migration 6c: pgxc_class row present, skipping DISTRIBUTE BY")
        }
        PgxcClass::NotDistributed => tracing::debug!(
            "Migration 6c: no pgxc_class (single-node deployment), skipping DISTRIBUTE BY"
        ),
        PgxcClass::NeedsDistribute | PgxcClass::ProbeFailed => {
            match sqlx::query("ALTER TABLE boom_rate_limit_cumulative DISTRIBUTE BY REPLICATION")
                .execute(&mut *conn)
                .await
            {
                Ok(_) => tracing::info!("Migration 6c: ALTER DISTRIBUTE BY REPLICATION ok"),
                Err(e) => tracing::warn!(
                    "Migration 6c: ALTER DISTRIBUTE BY REPLICATION failed (continuing): {}",
                    e
                ),
            }
        }
    }
    tracing::info!("Migration 6c/13: done");

    // 4. KV config store (dashboard-owned).
    tracing::info!("Migration 7/8: boom_config...");
    // The abandoned config-db-authority experiment (P1/P2, reverted in
    // 7bf138d) recreated boom_config with a `section` column. IF NOT EXISTS
    // can't evolve that back to the KV shape — probe the column shape and
    // drop explicitly (mirror of the section_store era's conditional
    // legacy-KV drop in ba4b68f, now in the opposite direction). Section
    // rows belong to the reverted line and have no readers in current code.
    let section_shaped: Option<String> = sqlx::query_scalar(
        r#"SELECT column_name FROM information_schema.columns
           WHERE table_name = 'boom_config' AND column_name = 'section'"#,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if section_shaped.is_some() {
        sqlx::query("DROP TABLE boom_config").execute(&mut *conn).await?;
        tracing::info!(
            "Dropped section-shaped boom_config (reverted config-db-authority experiment leftover)"
        );
    }
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
    // Idempotent ALTER: add key_prefix column for prefixed-key feature
    // (no-op if already present). New tables created above already include it.
    let _ = sqlx::query(
        r#"ALTER TABLE "boom_verification_token" ADD COLUMN IF NOT EXISTS key_prefix TEXT"#,
    )
    .execute(&mut *conn)
    .await;
    // Idempotent ALTER: add tag column for user-supplied key classification
    // (no-op if already present). Forward-compatible — old rows have NULL.
    let _ = sqlx::query(
        r#"ALTER TABLE "boom_verification_token" ADD COLUMN IF NOT EXISTS tag TEXT"#,
    )
    .execute(&mut *conn)
    .await;
    tracing::info!("Migration 9/9: done");

    // 7. Client block rules (boom-gatekeeper).
    tracing::info!("Migration 10: client_block_rule...");
    run_ddl_on_conn(&mut conn, boom_gatekeeper::migrations::block_rule_ddl()).await?;
    tracing::info!("Migration 10: done");

    // 8. Client blocklist global switch (boom-gatekeeper, single row).
    tracing::info!("Migration 11: client_blocklist_state...");
    run_ddl_on_conn(&mut conn, boom_gatekeeper::migrations::blocklist_state_ddl()).await?;
    tracing::info!("Migration 11: done");

    // 9. Workflow definitions + model bindings (boom-routing).
    tracing::info!("Migration 12: workflow_definition...");
    run_ddl_on_conn(&mut conn, boom_routing::migrations::workflow_definition_ddl()).await?;
    tracing::info!("Migration 12: done");
    tracing::info!("Migration 13: workflow_model...");
    run_ddl_on_conn(&mut conn, boom_routing::migrations::workflow_model_ddl()).await?;
    tracing::info!("Migration 13: done");

    // 10. Cost templates (boom-routing).
    tracing::info!("Migration 14: cost_template...");
    run_ddl_on_conn(&mut conn, boom_routing::migrations::cost_template_ddl()).await?;
    tracing::info!("Migration 14: done");

    // 11. User tag header config (boom-audit, single row).
    tracing::info!("Migration 15: user_tag_config...");
    run_ddl_on_conn(&mut conn, boom_audit::migrations::user_tag_config_ddl()).await?;
    tracing::info!("Migration 15: done");

    // 12. Cluster scheduling policy group (boom-routing, single row).
    tracing::info!("Migration 16: router_policy...");
    run_ddl_on_conn(&mut conn, boom_routing::migrations::router_policy_ddl()).await?;
    tracing::info!("Migration 16: done");

    // 13. kvc_max_blocks column for pre-existing router_policy tables.
    tracing::info!("Migration 17: router_policy kvc_max_blocks...");
    run_ddl_on_conn(
        &mut conn,
        boom_routing::migrations::router_policy_max_blocks_ddl(),
    )
    .await?;
    tracing::info!("Migration 17: done");

    // Connection returns to pool on drop.
    drop(conn);

    tracing::info!("BooMGateway persistence tables ensured (17 migrations)");
    Ok(())
}

/// Catalog probe: does the table NOT exist yet? True = the CREATE is
/// actually needed. On probe error returns true so the caller falls back to
/// running the (idempotent) DDL — a probe must never turn a boot that would
/// have succeeded into one that fails. Unlike the DDL itself, this is a
/// plain SELECT: it takes only weak catalog locks and never queues behind
/// another instance's in-flight transaction on the table.
async fn table_missing(conn: &mut sqlx::PgConnection, table: &str) -> bool {
    sqlx::query_scalar(
        r#"SELECT EXISTS (
               SELECT 1 FROM information_schema.tables
               WHERE table_schema = current_schema() AND table_name = $1
           )"#,
    )
    .bind(table)
    .fetch_one(&mut *conn)
    .await
    .map(|exists: bool| !exists)
    .unwrap_or(true)
}

/// Catalog probe: are any of `columns` absent from the table? True = the
/// ALTER is actually needed. Same probe-error semantics as `table_missing`.
async fn columns_missing(conn: &mut sqlx::PgConnection, table: &str, columns: &[&str]) -> bool {
    sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM information_schema.columns
           WHERE table_schema = current_schema()
             AND table_name = $1
             AND column_name = ANY($2)"#,
    )
    .bind(table)
    .bind(columns.iter().map(|c| c.to_string()).collect::<Vec<_>>())
    .fetch_one(&mut *conn)
    .await
    .map(|n: i64| (n as usize) < columns.len())
    .unwrap_or(true)
}

/// Catalog probe: is the column missing or still NOT NULL? True = the
/// relax-ALTER is actually needed. Same probe-error semantics as
/// `table_missing`.
async fn column_not_nullable(conn: &mut sqlx::PgConnection, table: &str, column: &str) -> bool {
    sqlx::query_scalar(
        r#"SELECT is_nullable FROM information_schema.columns
           WHERE table_schema = current_schema()
             AND table_name = $1
             AND column_name = $2"#,
    )
    .bind(table)
    .bind(column)
    .fetch_optional(&mut *conn)
    .await
    .map(|nullable: Option<String>| nullable.as_deref() != Some("YES"))
    .unwrap_or(true)
}

/// pgxc_class probe outcomes for the GaussDB DISTRIBUTE BY step.
enum PgxcClass {
    /// Distributed openGauss/GaussDB, table not yet registered — the ALTER
    /// should run.
    NeedsDistribute,
    /// Distributed deployment, table already registered by a previous boot
    /// (REPLICATION is the only value this code sets) — skip.
    AlreadyDistributed,
    /// No pgxc_class catalog: vanilla Postgres / single-node openGauss.
    /// The ALTER would be a syntax error — skip quietly.
    NotDistributed,
    /// Probe failed (unexpected catalog shape) — fall back to attempting
    /// the ALTER, preserving pre-probe behavior.
    ProbeFailed,
}

async fn pgxc_class_state(conn: &mut sqlx::PgConnection) -> PgxcClass {
    let has_catalog: bool = match sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_class WHERE relname = 'pgxc_class')",
    )
    .fetch_one(&mut *conn)
    .await
    {
        Ok(exists) => exists,
        Err(_) => return PgxcClass::ProbeFailed,
    };
    if !has_catalog {
        return PgxcClass::NotDistributed;
    }
    match sqlx::query_scalar(
        r#"SELECT EXISTS (
               SELECT 1 FROM pgxc_class
               WHERE pcrelid = 'boom_rate_limit_cumulative'::regclass
           )"#,
    )
    .fetch_one(&mut *conn)
    .await
    {
        Ok(true) => PgxcClass::AlreadyDistributed,
        Ok(false) => PgxcClass::NeedsDistribute,
        Err(_) => PgxcClass::ProbeFailed,
    }
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
    key_prefix TEXT,
    tag TEXT,
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
