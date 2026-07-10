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

/// DDL for boom_team_plan_assignment table.
pub fn team_assignment_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_team_plan_assignment (
    team_id     TEXT PRIMARY KEY,
    plan_name   TEXT NOT NULL,
    assigned_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
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

/// ALTER boom_rate_limit_plan: rename `key_*` columns to unprefixed names and
/// DROP the `team_*` columns. A plan is a generic template — the `type` field
/// (key/team) only gates which entity it may be assigned to, so all limit
/// columns are universal.
///
/// Idempotent — safe to run on every startup. On a fresh DB the RENAME will
/// be a no-op (columns don't exist) and the DROP will be a no-op (already
/// gone); on a pre-migration DB the rename + drop runs once and subsequent
/// starts skip through.
///
/// Cost columns are stored as BIGINT micros (1e-6 USD) to avoid
/// rust_decimal ↔ sqlx integration overhead. Conversion happens in
/// `row_to_plan` / `upsert_plan_db` via decimal_to_micros.
pub fn plan_alter_ddl() -> &'static str {
    r#"
-- DO block lets us catch per-statement errors and continue, which is the
-- only way to express "rename if exists, otherwise no-op" in pure SQL.
DO $$
BEGIN
    -- type / member_plan: kept for fresh DBs.
    BEGIN
        ALTER TABLE boom_rate_limit_plan
            ADD COLUMN IF NOT EXISTS type         TEXT        NOT NULL DEFAULT 'key',
            ADD COLUMN IF NOT EXISTS member_plan  TEXT;
    EXCEPTION WHEN OTHERS THEN NULL; END;

    -- Rename key_* → unprefixed. No-op if the column is already renamed
    -- (or never existed on a fresh DB whose base DDL uses unprefixed names).
    BEGIN
        ALTER TABLE boom_rate_limit_plan
            RENAME COLUMN key_tpm_limit TO tpm_limit;
    EXCEPTION WHEN OTHERS THEN NULL; END;
    BEGIN
        ALTER TABLE boom_rate_limit_plan
            RENAME COLUMN key_total_tpm_limit TO total_tpm_limit;
    EXCEPTION WHEN OTHERS THEN NULL; END;
    BEGIN
        ALTER TABLE boom_rate_limit_plan
            RENAME COLUMN key_total_cost_limit_micros TO total_cost_limit_micros;
    EXCEPTION WHEN OTHERS THEN NULL; END;

    -- Rename total_tpm_limit → total_token_limit. The old name was misleading:
    -- "TPM" is a per-minute concept, but this column is a cumulative cap.
    -- No-op on a fresh DB whose base DDL uses the new name.
    BEGIN
        ALTER TABLE boom_rate_limit_plan
            RENAME COLUMN total_tpm_limit TO total_token_limit;
    EXCEPTION WHEN OTHERS THEN NULL; END;

    -- For fresh DBs whose base plan_ddl doesn't include these columns yet,
    -- ensure they exist (no-op on already-migrated DBs).
    BEGIN
        ALTER TABLE boom_rate_limit_plan
            ADD COLUMN IF NOT EXISTS tpm_limit           BIGINT,
            ADD COLUMN IF NOT EXISTS total_token_limit   BIGINT,
            ADD COLUMN IF NOT EXISTS total_cost_limit_micros BIGINT;
    EXCEPTION WHEN OTHERS THEN NULL; END;

    -- DROP cost_limit_micros (per-minute cost cap was never needed in
    -- practice — only total_cost_limit is enforced). Idempotent.
    BEGIN
        ALTER TABLE boom_rate_limit_plan
            DROP COLUMN IF EXISTS cost_limit_micros;
    EXCEPTION WHEN OTHERS THEN NULL; END;
    -- Also drop any legacy key_cost_limit_micros that pre-migration DBs may
    -- still carry (the RENAME above was removed, so the unprefixed form never
    -- came back; this clears the original column directly).
    BEGIN
        ALTER TABLE boom_rate_limit_plan
            DROP COLUMN IF EXISTS key_cost_limit_micros;
    EXCEPTION WHEN OTHERS THEN NULL; END;

    -- DROP team_* columns. They are no longer read by any code path. The
    -- data is not migrated because no deployment has ever written production
    -- data to them (this is a pre-release refactor).
    BEGIN
        ALTER TABLE boom_rate_limit_plan
            DROP COLUMN IF EXISTS team_concurrency_limit,
            DROP COLUMN IF EXISTS team_rpm_limit,
            DROP COLUMN IF EXISTS team_tpm_limit,
            DROP COLUMN IF EXISTS team_cost_limit_micros,
            DROP COLUMN IF EXISTS team_window_limits,
            DROP COLUMN IF EXISTS team_total_tpm_limit,
            DROP COLUMN IF EXISTS team_total_cost_limit_micros;
    EXCEPTION WHEN OTHERS THEN NULL; END;
END $$;
"#
}
