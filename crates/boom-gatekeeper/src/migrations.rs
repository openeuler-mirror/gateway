//! DDL for the `boom_client_block_rule` table and the
//! `boom_client_blocklist_state` single-row table (owned by this crate,
//! applied centrally by `boom_dashboard::migrations::run_migrations`).

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

/// The global blocklist switch, DB-backed so it survives restarts and is
/// shared across future instances. Single row (`id = 1`), created disabled
/// on fresh databases — the boot path adopts the YAML `client_blocklist.
/// enabled` value (or defaults to enabled) when the row is absent, so a
/// fresh DB never silently turns interception off.
pub fn blocklist_state_ddl() -> &'static str {
    r#"CREATE TABLE IF NOT EXISTS boom_client_blocklist_state (
    id         INTEGER PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    enabled    BOOLEAN NOT NULL DEFAULT false,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
)"#
}
