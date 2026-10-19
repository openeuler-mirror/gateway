//! DDL for `boom_config_meta` — the config-seed idempotency marker table,
//! owned by boom-main.
//!
//! The DB is the runtime authority for business config (models, aliases,
//! plans, block rules). YAML runtime sections act as a one-time seed: on the
//! first boot against a fresh DB they are imported; a marker row in this
//! table records that the seed (or the adopt pass on a pre-existing DB) has
//! run, so later boots never re-import — deleting all models and restarting
//! must not resurrect them from YAML.
//!
//! Applied by `from_config` after `boom_dashboard::migrations::run_migrations`
//! (boom-main owns this table; boom-dashboard must not depend on boom-main,
//! so the DDL cannot ride the dashboard migration runner).

use sqlx::PgPool;

/// Idempotent DDL — safe on every boot.
pub async fn ensure_config_meta_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS boom_config_meta (
        key        TEXT PRIMARY KEY,
        value      JSONB NOT NULL,
        updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
    )"#,
    )
    .execute(pool)
    .await?;
    Ok(())
}
