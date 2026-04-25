/// DDL for boom_rate_limit_state table.
pub fn rate_limit_state_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_rate_limit_state (
    cache_key    TEXT PRIMARY KEY,
    count        BIGINT NOT NULL DEFAULT 0,
    window_start BIGINT NOT NULL,
    window_secs  BIGINT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#
}

/// DDL for boom_key_plan_assignment table.
pub fn assignment_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_key_plan_assignment (
    key_hash     TEXT PRIMARY KEY,
    plan_name    TEXT NOT NULL,
    assigned_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#
}

/// DDL for boom_rate_limit_plan table + index.
pub fn plan_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_rate_limit_plan (
    name              TEXT PRIMARY KEY,
    concurrency_limit INTEGER,
    rpm_limit         BIGINT,
    window_limits     JSONB  NOT NULL DEFAULT '[]',
    schedule          JSONB  NOT NULL DEFAULT '[]',
    is_default        BOOLEAN NOT NULL DEFAULT false,
    source            TEXT    NOT NULL DEFAULT 'yaml',
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_boom_plan_default
   ON boom_rate_limit_plan (is_default) WHERE is_default = true;
"#
}
