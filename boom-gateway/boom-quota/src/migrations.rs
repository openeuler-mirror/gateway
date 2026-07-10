//! DDL owned by boom-quota.
//!
//! `boom_rate_limit_cumulative` stores permanent counters (input/output token
//! totals, cost totals) keyed by `kc:{key_hash}:{kind}` / `tc:{team_id}:{kind}`.
//! These rows never expire; they are only cleared by explicit reset operations.

/// DDL for the cumulative counters table.
pub fn cumulative_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_rate_limit_cumulative (
    cache_key   TEXT PRIMARY KEY,
    value       BIGINT NOT NULL DEFAULT 0,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#
}
