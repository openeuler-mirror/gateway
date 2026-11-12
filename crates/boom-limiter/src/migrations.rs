/// DDL for boom_rate_limit_state table.
///
/// `tokens` and `costs_micros` are NEW columns added by the limiter
/// normalization refactor (boom-quota deletion). They are nullable + default 0
/// so pre-migration rows (counts-only) round-trip cleanly. `state_alter_ddl`
/// adds them idempotently on existing databases; fresh databases get them
/// from this base DDL.
///
/// PK is `(cache_key, window_start)`: rows hold per-window-generation global
/// counters and the multi-instance delta protocol applies add-deltas keyed by
/// generation, so an old generation's row survives alongside the new one
/// until the expiry purge removes it.
pub fn rate_limit_state_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_rate_limit_state (
    cache_key     TEXT NOT NULL,
    count         BIGINT NOT NULL DEFAULT 0,
    tokens        BIGINT,
    costs_micros  BIGINT,
    window_start  BIGINT NOT NULL,
    window_secs   BIGINT NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (cache_key, window_start)
);
"#
}

/// Idempotent ALTER: widen the legacy single-column PK (cache_key) to the
/// composite (cache_key, window_start). Guarded by an information_schema
/// check so it is a true no-op on databases already migrated or freshly
/// created by the composite base DDL. Errors propagate (not swallowed): a
/// silently-skipped PK change would surface later as duplicate-key failures
/// on every window rollover push.
pub fn state_pk_alter_ddl() -> &'static str {
    r#"
DO $$
DECLARE pk_name TEXT;
BEGIN
    SELECT tc.constraint_name INTO pk_name
    FROM information_schema.table_constraints tc
    JOIN information_schema.key_column_usage k
      ON k.constraint_name = tc.constraint_name
     AND k.table_name = tc.table_name
    WHERE tc.table_name = 'boom_rate_limit_state'
      AND tc.constraint_type = 'PRIMARY KEY'
      AND tc.table_schema = current_schema()
    GROUP BY tc.constraint_name
    HAVING COUNT(*) = 1 AND BOOL_AND(k.column_name = 'cache_key');

    IF pk_name IS NOT NULL THEN
        EXECUTE format('ALTER TABLE boom_rate_limit_state DROP CONSTRAINT %I', pk_name);
        ALTER TABLE boom_rate_limit_state
            ADD PRIMARY KEY (cache_key, window_start);
    END IF;
END $$;
"#
}

/// DDL for the delta-push watermark table (multi-instance incremental sync).
///
/// One row per committed push batch. PK (instance_id, seq) makes pushes
/// idempotent: a retry after an ambiguous commit finds the row, skips the
/// DB re-apply, and replays only the in-memory mirror fix from the stored
/// batch. instance_id is a per-boot UUID, so seq restarts at 0 each boot and
/// rows are pruned after 2h (any retry horizon is far shorter).
pub fn quota_delta_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_quota_delta (
    instance_id       TEXT NOT NULL,
    seq               BIGINT NOT NULL,
    window_deltas     JSONB NOT NULL DEFAULT '[]',
    cumulative_deltas JSONB NOT NULL DEFAULT '[]',
    applied_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (instance_id, seq)
);
"#
}

/// Idempotent ALTER: ensure pre-refactor databases have the new tokens and
/// costs_micros columns on boom_rate_limit_state. No-op on fresh DBs whose
/// base DDL already includes them.
pub fn state_alter_ddl() -> &'static str {
    r#"
DO $$
BEGIN
    BEGIN
        ALTER TABLE boom_rate_limit_state
            ADD COLUMN IF NOT EXISTS tokens       BIGINT,
            ADD COLUMN IF NOT EXISTS costs_micros BIGINT;
    EXCEPTION WHEN OTHERS THEN NULL; END;
END $$;
"#
}

/// DDL for the cumulative counters table.
///
/// `boom_rate_limit_cumulative` stores permanent counters (input/output token
/// totals, cost totals) keyed by `kc:{key_hash}:{kind}` / `tc:{team_id}:{kind}`.
/// These rows never expire; they are only cleared by explicit reset operations.
///
/// Ownership transferred from boom-quota (deleted) to boom-limiter during the
/// limiter normalization refactor. Table name and schema are unchanged.
pub fn cumulative_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_rate_limit_cumulative (
    cache_key   TEXT PRIMARY KEY,
    value       BIGINT NOT NULL DEFAULT 0,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
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

/// Idempotent ALTER for boom_key_plan_assignment: relax `plan_name` to NULL
/// so the table can represent three distinct states:
/// - **row absent**                → key was never explicitly assigned (follows default_plan at runtime)
/// - **plan_name IS NULL**         → user explicitly chose "no plan" (does NOT follow default_plan)
/// - **plan_name = 'some_name'**   → user explicitly assigned a plan
///
/// Without NULL support, "unassign" deletes the row, which is indistinguishable
/// from "never configured" — both then fall back to default_plan, masking the
/// user's intent. See CLAUDE.md §9 (single source of truth for assignment state).
pub fn assignment_alter_ddl() -> &'static str {
    r#"
DO $$
BEGIN
    ALTER TABLE boom_key_plan_assignment ALTER COLUMN plan_name DROP NOT NULL;
EXCEPTION WHEN OTHERS THEN NULL;
END $$;
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

/// DDL for boom_rate_limit_plan table + indexes.
///
/// `is_default_team` is the team-plan counterpart of `is_default` — the
/// default-plan pointers must live in DB (not YAML) now that DB is the
/// runtime authority for plans. Both use partial unique indexes so at most
/// one row carries each flag.
///
/// The guard ALTER must run BEFORE `idx_boom_plan_default_team`: on an
/// upgrade-path DB the CREATE TABLE is a no-op (table exists without the
/// column), so without the ALTER the index creation fails with "column
/// is_default_team does not exist" and aborts every later migration.
pub fn plan_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_rate_limit_plan (
    name              TEXT PRIMARY KEY,
    concurrency_limit INTEGER,
    rpm_limit         BIGINT,
    window_limits     JSONB  NOT NULL DEFAULT '[]',
    schedule          JSONB  NOT NULL DEFAULT '[]',
    is_default        BOOLEAN NOT NULL DEFAULT false,
    is_default_team   BOOLEAN NOT NULL DEFAULT false,
    source            TEXT    NOT NULL DEFAULT 'yaml',
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
ALTER TABLE boom_rate_limit_plan
    ADD COLUMN IF NOT EXISTS is_default_team BOOLEAN NOT NULL DEFAULT false;

CREATE UNIQUE INDEX IF NOT EXISTS idx_boom_plan_default
   ON boom_rate_limit_plan (is_default) WHERE is_default = true;
CREATE UNIQUE INDEX IF NOT EXISTS idx_boom_plan_default_team
   ON boom_rate_limit_plan (is_default_team) WHERE is_default_team = true;
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

    -- is_default_team: team-plan default pointer moved from YAML-only state
    -- to a DB column (DB-authoritative config refactor). No-op on fresh DBs
    -- whose base DDL already includes it.
    BEGIN
        ALTER TABLE boom_rate_limit_plan
            ADD COLUMN IF NOT EXISTS is_default_team BOOLEAN NOT NULL DEFAULT false;
    EXCEPTION WHEN OTHERS THEN NULL; END;
END $$;
"#
}
