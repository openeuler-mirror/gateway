/// DDL for boom_model_deployment table.
pub fn deployment_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_model_deployment (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    model_name        TEXT    NOT NULL,
    litellm_model     TEXT    NOT NULL,
    api_key           TEXT,
    api_key_env       BOOLEAN NOT NULL DEFAULT false,
    api_base          TEXT,
    api_version       TEXT,
    aws_region_name   TEXT,
    aws_access_key_id TEXT,
    aws_secret_access_key TEXT,
    rpm               BIGINT,
    tpm               BIGINT,
    timeout           BIGINT  NOT NULL DEFAULT 1200,
    headers           JSONB   NOT NULL DEFAULT '{}',
    temperature       DOUBLE PRECISION,
    max_tokens        INTEGER,
    enabled           BOOLEAN NOT NULL DEFAULT true,
    auto_disabled     BOOLEAN NOT NULL DEFAULT false,
    source            TEXT    NOT NULL DEFAULT 'yaml',
    deployment_id     TEXT,
    client_type_header BOOLEAN NOT NULL DEFAULT false,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_boom_deployment_model ON boom_model_deployment(model_name);
"#
}

/// Migration: add auto_disabled column to existing tables.
pub fn migration_add_auto_disabled() -> &'static str {
    r#"
ALTER TABLE boom_model_deployment ADD COLUMN IF NOT EXISTS auto_disabled BOOLEAN NOT NULL DEFAULT false;
"#
}

/// Migration: add allowed_teams column (team ACL for private models).
/// NULL = public pool (normal permission rules); JSONB array = private model
/// accessible only to keys of the listed team_ids; empty array = locked for
/// all teams. Default NULL keeps every existing deployment public.
pub fn migration_add_allowed_teams() -> &'static str {
    r#"
ALTER TABLE boom_model_deployment ADD COLUMN IF NOT EXISTS allowed_teams JSONB;
"#
}

/// Migration: add the unified `visibility` column ('normal' | 'public' |
/// 'private', default 'normal') and normalize legacy rows — deployments
/// marked private via the pre-visibility `allowed_teams` marker are flipped
/// to visibility='private'. Idempotent: the UPDATE's WHERE guard only
/// matches rows that still need it.
pub fn migration_add_visibility() -> &'static str {
    r#"
ALTER TABLE boom_model_deployment ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'normal';
UPDATE boom_model_deployment SET visibility = 'private' WHERE allowed_teams IS NOT NULL AND visibility = 'normal';
"#
}

/// DDL for boom_model_alias table.
pub fn alias_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_model_alias (
    alias_name    TEXT PRIMARY KEY,
    target_model  TEXT    NOT NULL,
    hidden        BOOLEAN NOT NULL DEFAULT false,
    source        TEXT    NOT NULL DEFAULT 'yaml',
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#
}
