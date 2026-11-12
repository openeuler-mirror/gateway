use std::time::Duration;

use sqlx::PgPool;

/// Immutable per-process identity. The instance id is a fresh UUID on
/// every boot and is never persisted: a restarted process IS a new
/// instance, and the dead row ages out once its heartbeat goes stale.
#[derive(Debug, Clone)]
pub struct InstanceInfo {
    pub instance_id: String,
    pub hostname: String,
    pub pid: u32,
    pub version: String,
    /// Directly reachable address (host:port) for dashboard scatter
    /// drill-down. `None` when not advertised.
    pub advertise_addr: Option<String>,
}

pub fn generate_instance_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Upsert the heartbeat row. `hostname`/`pid`/`version`/`started_at` are
/// write-once (INSERT); only `heartbeat_at` advances. All timestamps are
/// DB-side NOW() — liveness math never trusts instance clocks, so clock
/// drift cannot kill a live instance or resurrect a dead one.
///
/// GaussDB-compatible upsert (UPDATE → INSERT → UPDATE on race).
pub async fn heartbeat(pool: &PgPool, info: &InstanceInfo) -> Result<(), sqlx::Error> {
    boom_core::gaussdb_upsert!(
        pool,
        || sqlx::query(
            r#"UPDATE boom_instance_state
               SET heartbeat_at = NOW(), advertise_addr = $2
               WHERE instance_id = $1"#,
        )
        .bind(&info.instance_id)
        .bind(&info.advertise_addr),
        || sqlx::query(
            r#"INSERT INTO boom_instance_state
               (instance_id, hostname, pid, version, advertise_addr)
               VALUES ($1, $2, $3, $4, $5)"#,
        )
        .bind(&info.instance_id)
        .bind(&info.hostname)
        .bind(info.pid as i64)
        .bind(&info.version)
        .bind(&info.advertise_addr)
    )
}

/// A live (or recently live) instance row.
#[derive(Debug, Clone)]
pub struct InstanceRow {
    pub instance_id: String,
    pub hostname: String,
    pub pid: i64,
    pub version: Option<String>,
    pub advertise_addr: Option<String>,
    /// JSON blob (TEXT column), caller-serialized telemetry snapshot.
    pub snapshot: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub heartbeat_at: chrono::DateTime<chrono::Utc>,
}

/// Instances whose heartbeat is fresher than `ttl`. Judged entirely on
/// the DB clock. Stale rows are NOT deleted here — cleanup is a separate
/// maintenance concern; this query only decides who counts as live.
pub async fn live_instances(
    pool: &PgPool,
    ttl: Duration,
) -> Result<Vec<InstanceRow>, sqlx::Error> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        r#"SELECT instance_id, hostname, pid, version, advertise_addr, snapshot,
                  started_at, heartbeat_at
           FROM boom_instance_state
           WHERE heartbeat_at > NOW() - ($1 * interval '1 second')"#,
    )
    .bind(ttl.as_secs() as i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(instance_id, hostname, pid, version, advertise_addr, snapshot, started_at, heartbeat_at)| InstanceRow {
                instance_id,
                hostname,
                pid,
                version,
                advertise_addr,
                snapshot,
                started_at,
                heartbeat_at,
            },
        )
        .collect())
}
