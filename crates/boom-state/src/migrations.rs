use sqlx::PgPool;

/// One row per live gateway process. `snapshot` is a JSON blob (TEXT,
/// serialized by the caller) carrying coarse telemetry for the cluster
/// overview page. `started_at`/`heartbeat_at` default to DB NOW().
pub fn instance_state_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_instance_state (
    instance_id     TEXT PRIMARY KEY,
    hostname        TEXT NOT NULL,
    pid             BIGINT NOT NULL,
    version         TEXT,
    advertise_addr  TEXT,
    snapshot        TEXT,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    heartbeat_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#
}

/// DB-as-changelog signal table. Writers bump the category counter after
/// committing a business write; instances poll and reconcile the changed
/// category. Tiny table, cheap to poll cluster-wide.
pub fn config_revision_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_config_revision (
    category    TEXT PRIMARY KEY,
    revision    BIGINT NOT NULL DEFAULT 0,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
"#
}

pub async fn run(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(instance_state_ddl()).execute(pool).await?;
    sqlx::query(config_revision_ddl()).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_is_gaussdb_safe() {
        // GaussDB: no ON CONFLICT in DDL/DML, TEXT PKs, DB-side NOW().
        for ddl in [instance_state_ddl(), config_revision_ddl()] {
            let lower = ddl.to_lowercase();
            assert!(lower.contains("create table if not exists"));
            assert!(!lower.contains("on conflict"));
            assert!(!lower.contains("uuid"), "TEXT ids only — no native uuid type");
        }
    }
}
