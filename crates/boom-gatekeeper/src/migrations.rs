//! DDL for the `boom_client_block_rule` table (owned by this crate, applied
//! centrally by `boom_dashboard::migrations::run_migrations` at startup).

/// Idempotent DDL — `CREATE TABLE IF NOT EXISTS`, executed on a connection
/// that already holds the dashboard migration lock.
pub fn block_rule_ddl() -> &'static str {
    r#"CREATE TABLE IF NOT EXISTS boom_client_block_rule (
    rule_name  TEXT PRIMARY KEY,
    enabled    BOOLEAN NOT NULL DEFAULT true,
    conditions JSONB NOT NULL,
    action     JSONB NOT NULL,
    source     TEXT NOT NULL DEFAULT 'db',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
)"#
}
