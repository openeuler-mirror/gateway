use sqlx::PgPool;

/// One row per (instance, 10s bucket). PK gives idempotent placement for
/// re-finalized buckets; the secondary index serves the incremental
/// `ts_secs > since` dashboard reads.
pub fn stress_sample_ddl() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS boom_stress_sample (
    instance_id           TEXT   NOT NULL,
    ts_secs               BIGINT NOT NULL,
    cpu_pct               REAL   NOT NULL DEFAULT 0,
    rss_bytes             BIGINT NOT NULL DEFAULT 0,
    worker_queue_depth    INT    NOT NULL DEFAULT 0,
    blocking_tasks_queued INT    NOT NULL DEFAULT 0,
    inflight              INT    NOT NULL DEFAULT 0,
    PRIMARY KEY (instance_id, ts_secs)
)"#
}

pub fn stress_sample_ts_index_ddl() -> &'static str {
    r#"
CREATE INDEX IF NOT EXISTS idx_boom_stress_sample_ts ON boom_stress_sample (ts_secs)"#
}

pub async fn run(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(stress_sample_ddl()).execute(pool).await?;
    sqlx::query(stress_sample_ts_index_ddl()).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_is_gaussdb_safe() {
        // GaussDB: no ON CONFLICT in DDL, TEXT ids, separate statements.
        for ddl in [stress_sample_ddl(), stress_sample_ts_index_ddl()] {
            let lower = ddl.to_lowercase();
            assert!(lower.contains("if not exists"));
            assert!(!lower.contains("on conflict"));
            assert!(!lower.contains("uuid"), "TEXT/BIGINT keys only");
            assert!(!lower.contains(";"), "one statement per query() call");
        }
    }
}
