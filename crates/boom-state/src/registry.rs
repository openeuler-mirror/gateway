//! Stable instance identity: resolution, boot-time registration, and
//! heartbeat.
//!
//! **Identity model.** An instance id is STABLE across restarts — either
//! configured (`server.instance_id`) or derived from the bind address
//! (`host:port`) — so rows keyed by it (quota watermarks, dashboards)
//! stay attributable to the same logical instance. The process-level
//! fields (pid, version, started_at) are *incarnation* fields: they are
//! rewritten when a boot legitimately takes over the row.
//!
//! **Duplicate detection** happens at boot, in a single transaction
//! ([`register`]): a fresh id inserts, a provably dead holder's row is
//! taken over, and a live holder fails the boot with the holder's
//! coordinates ([`RegisterError::Conflict`]). This is what makes two
//! instances started with the same copied YAML (loopback + same port →
//! same derived id) fail loudly instead of silently splitting cluster
//! state.
//!
//! **Arbitration** ("is the holder dead?") never trusts the local clock:
//! staleness is judged DB-side (`NOW() - heartbeat_at` vs
//! [`INSTANCE_TTL_SECS`]), computed in the registration SELECT. The only
//! local knowledge used is a same-host pid probe — valid because a
//! matching hostname means the row was written on this machine.
//!
//! **Runtime visibility**: the heartbeat UPDATE is pid-guarded. If
//! another process takes over the row (or an operator deletes it), the
//! next beat detects that via a 0-row UPDATE and reports it
//! ([`Heartbeat`]) — a duplicate identity is never silent.

use std::time::Duration;

use sqlx::PgPool;

use crate::INSTANCE_TTL_SECS;

/// Per-process identity registered under a stable instance id. `hostname`
/// is arbitration identity (same hostname ⇒ pid probe is valid);
/// `advertise_addr` is the configured bind address (host:port) for
/// dashboard drill-down — `None` when not advertised.
#[derive(Debug, Clone)]
pub struct InstanceInfo {
    pub instance_id: String,
    pub hostname: String,
    pub pid: u32,
    pub version: String,
    pub advertise_addr: Option<String>,
}

impl InstanceInfo {
    /// Build this process's identity for `register`/`heartbeat`.
    pub fn current(instance_id: &str, host: &str, port: u16) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            hostname: resolve_hostname(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            advertise_addr: Some(format!("{}:{}", host, port)),
        }
    }
}

/// Host identity for takeover arbitration. Container convention first
/// (k8s/docker set `$HOSTNAME` to the pod/container name), then the OS
/// hostname (bare-metal systemd services do not reliably export the env
/// var). "unknown" means hostname could not be resolved — same-host pid
/// probing is then disabled and only DB-clock staleness arbitrates.
pub fn resolve_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    os_hostname().unwrap_or_else(|| "unknown".into())
}

#[cfg(unix)]
fn os_hostname() -> Option<String> {
    // 256 bytes covers Linux HOST_NAME_MAX (64) with headroom; on a
    // longer name gethostname fails with ENAMETOOLONG and we degrade to
    // "unknown" rather than truncate.
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let s = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(not(unix))]
fn os_hostname() -> Option<String> {
    None
}

/// Probe whether `pid` is alive on THIS machine. Only meaningful when the
/// row being probed was written on this host (matching hostname).
/// kill(pid, 0): 0 = exists, EPERM = exists (other user), ESRCH = gone.
/// Pid reuse within the heartbeat TTL can mislabel a recycled pid as
/// alive — the safe direction: the takeover then waits for the TTL to
/// lapse. Out-of-range pids are treated as dead (they cannot be ours).
#[cfg(unix)]
fn pid_alive_local(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive_local(_pid: i64) -> bool {
    // No probe available — conservative: treat as alive so only DB-clock
    // staleness can take over.
    true
}

/// Column list shared by every instance-state SELECT.
const INSTANCE_COLUMNS: &str =
    "instance_id, hostname, pid, version, advertise_addr, snapshot, started_at, heartbeat_at";

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

/// Instance-row read with the DB-side staleness verdict.
struct InstanceRead {
    row: InstanceRow,
    /// `heartbeat_at` older than [`INSTANCE_TTL_SECS`], computed by the
    /// DB clock — instance clocks are never trusted for liveness.
    stale: bool,
}

type InstanceTuple = (
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
);

/// Boot-time registration of the stable instance id. Exactly one boot owns
/// an id: fresh ids insert, provably dead holders are taken over (row
/// rewritten in place — the id never accumulates one row per restart),
/// and live holders fail the boot.
///
/// Race safety without error-code classification (GaussDB-compatible):
/// the check runs in one transaction with `FOR UPDATE`; a boot that loses
/// a concurrent insert race gets a unique violation, re-reads the row, and
/// reports the winner as the holder.
pub async fn register(pool: &PgPool, info: &InstanceInfo) -> Result<Registration, RegisterError> {
    let mut tx = pool.begin().await.map_err(RegisterError::Db)?;

    let existing: Option<(InstanceTuple, bool)> = sqlx::query_as::<_, (InstanceTuple, bool)>(
        &format!(
            r#"SELECT {}, (heartbeat_at < NOW() - ($2 * interval '1 second'))
               FROM boom_instance_state
               WHERE instance_id = $1
               FOR UPDATE"#,
            INSTANCE_COLUMNS
        ),
    )
    .bind(&info.instance_id)
    .bind(INSTANCE_TTL_SECS as i64)
    .fetch_optional(&mut *tx)
    .await
    .map_err(RegisterError::Db)?;

    let Some(existing) = existing else {
        // Absent — insert. A concurrent boot with the same id wins the PK
        // race; we lose, re-read the row, and report it as the holder.
        let insert = sqlx::query(
            r#"INSERT INTO boom_instance_state
               (instance_id, hostname, pid, version, advertise_addr)
               VALUES ($1, $2, $3, $4, $5)"#,
        )
        .bind(&info.instance_id)
        .bind(&info.hostname)
        .bind(info.pid as i64)
        .bind(&info.version)
        .bind(&info.advertise_addr)
        .execute(&mut *tx)
        .await;

        if let Err(e) = insert {
            drop(tx);
            if let Ok(Some(read)) = fetch_instance(pool, &info.instance_id).await {
                return Err(RegisterError::Conflict {
                    instance_id: info.instance_id.clone(),
                    holder: Some(read.row),
                });
            }
            return Err(RegisterError::Db(e));
        }
        tx.commit().await.map_err(RegisterError::Db)?;
        return Ok(Registration::Registered);
    };

    let (t, stale) = existing;
    let (instance_id, hostname, pid, version, advertise_addr, snapshot, started_at, heartbeat_at) = t;
    let read = InstanceRead {
        row: InstanceRow {
            instance_id,
            hostname,
            pid,
            version,
            advertise_addr,
            snapshot,
            started_at,
            heartbeat_at,
        },
        stale,
    };

    if !dead_holder(read.stale, &read.row.hostname, read.row.pid, &info.hostname) {
        return Err(RegisterError::Conflict {
            instance_id: info.instance_id.clone(),
            holder: Some(read.row),
        });
    }

    // Take over: rewrite the row with this boot's incarnation.
    sqlx::query(
        r#"UPDATE boom_instance_state
           SET hostname = $2, pid = $3, version = $4, advertise_addr = $5,
               started_at = NOW(), heartbeat_at = NOW()
           WHERE instance_id = $1"#,
    )
    .bind(&info.instance_id)
    .bind(&info.hostname)
    .bind(info.pid as i64)
    .bind(&info.version)
    .bind(&info.advertise_addr)
    .execute(&mut *tx)
    .await
    .map_err(RegisterError::Db)?;
    tx.commit().await.map_err(RegisterError::Db)?;

    Ok(Registration::TookOver {
        previous: read.row,
    })
}

/// Is the row's holder provably gone?
///
/// - `stale`: heartbeat past TTL, judged on the DB clock — covers every
///   host (crash, OOM-kill, migration to a new machine).
/// - Same hostname + dead pid: crash-restart on this host reclaims the id
///   immediately instead of waiting out the TTL. The "unknown" hostname
///   disables the probe: without a real hostname the row may come from a
///   different machine, where probing local pids is meaningless (and a
///   coincidentally-dead local pid would fake a takeover).
fn dead_holder(stale: bool, row_hostname: &str, row_pid: i64, our_hostname: &str) -> bool {
    stale
        || (row_hostname == our_hostname
            && our_hostname != "unknown"
            && !pid_alive_local(row_pid))
}

/// Read one instance row (+ DB-side staleness verdict).
async fn fetch_instance(
    pool: &PgPool,
    instance_id: &str,
) -> Result<Option<InstanceRead>, sqlx::Error> {
    let row: Option<(InstanceTuple, bool)> = sqlx::query_as::<_, (InstanceTuple, bool)>(
        &format!(
            r#"SELECT {}, (heartbeat_at < NOW() - ($2 * interval '1 second'))
               FROM boom_instance_state
               WHERE instance_id = $1"#,
            INSTANCE_COLUMNS
        ),
    )
    .bind(instance_id)
    .bind(INSTANCE_TTL_SECS as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(t, stale)| {
        let (instance_id, hostname, pid, version, advertise_addr, snapshot, started_at, heartbeat_at) = t;
        InstanceRead {
            row: InstanceRow {
                instance_id,
                hostname,
                pid,
                version,
                advertise_addr,
                snapshot,
                started_at,
                heartbeat_at,
            },
            stale,
        }
    }))
}

/// Outcome of a successful [`register`].
#[derive(Debug)]
pub enum Registration {
    /// Fresh row inserted for this id.
    Registered,
    /// The previous holder was provably dead; its row was rewritten in
    /// place with this boot's incarnation fields.
    TookOver { previous: InstanceRow },
}

/// Boot-time registration failure. `Conflict` is the actionable one: the
/// id is already held, so the boot must stop and the operator must either
/// wait for the holder to die out or configure a unique id.
#[derive(Debug)]
pub enum RegisterError {
    Conflict {
        instance_id: String,
        holder: Option<InstanceRow>,
    },
    Db(sqlx::Error),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterError::Db(e) => write!(f, "instance registration DB error: {e}"),
            RegisterError::Conflict { instance_id, holder } => match holder {
                Some(h) => {
                    let age = (chrono::Utc::now() - h.heartbeat_at).num_seconds().max(0);
                    write!(
                        f,
                        "instance id '{instance_id}' is already held by hostname={} pid={} \
                         (last heartbeat {}s ago, started {}). If that process is dead and \
                         elsewhere, its row can be reclaimed after the {INSTANCE_TTL_SECS}s \
                         heartbeat TTL; if it is dead on this host, reclaim is immediate on \
                         the next attempt. Otherwise every instance needs a unique \
                         server.instance_id.",
                        h.hostname, h.pid, age, h.started_at.to_rfc3339()
                    )
                }
                None => write!(
                    f,
                    "instance id '{instance_id}' is already registered by a concurrently \
                     booting instance — every instance needs a unique server.instance_id"
                ),
            },
        }
    }
}

impl std::error::Error for RegisterError {}

/// One heartbeat beat's outcome, for caller-side visibility.
#[derive(Debug, PartialEq, Eq)]
pub enum Heartbeat {
    /// Row refreshed normally.
    Own,
    /// Row had vanished (operator cleanup) and was re-inserted.
    ReRegistered,
    /// Another process owns the row — our registration was taken over.
    /// Duplicate identity is a live misconfiguration; the caller logs it
    /// loudly.
    TakenOver { holder_pid: i64 },
}

/// Advance the heartbeat. The UPDATE is pid-guarded: a 0-row result means
/// the row was taken over (another pid) or deleted, and the follow-up
/// SELECT distinguishes the two. All timestamps are DB-side NOW() — clock
/// drift cannot kill a live instance or resurrect a dead one. No
/// error-code classification: every branch is decided by row counts, so
/// this is GaussDB-safe.
pub async fn heartbeat(pool: &PgPool, info: &InstanceInfo) -> Result<Heartbeat, sqlx::Error> {
    let updated = sqlx::query(
        r#"UPDATE boom_instance_state
           SET heartbeat_at = NOW(), advertise_addr = $2
           WHERE instance_id = $1 AND pid = $3"#,
    )
    .bind(&info.instance_id)
    .bind(&info.advertise_addr)
    .bind(info.pid as i64)
    .execute(pool)
    .await?
    .rows_affected();
    if updated > 0 {
        return Ok(Heartbeat::Own);
    }

    let holder_pid: Option<i64> = sqlx::query_scalar(
        r#"SELECT pid FROM boom_instance_state WHERE instance_id = $1"#,
    )
    .bind(&info.instance_id)
    .fetch_optional(pool)
    .await?;

    match holder_pid {
        None => {
            // Vanished (manual cleanup): re-insert the full row.
            sqlx::query(
                r#"INSERT INTO boom_instance_state
                   (instance_id, hostname, pid, version, advertise_addr)
                   VALUES ($1, $2, $3, $4, $5)"#,
            )
            .bind(&info.instance_id)
            .bind(&info.hostname)
            .bind(info.pid as i64)
            .bind(&info.version)
            .bind(&info.advertise_addr)
            .execute(pool)
            .await?;
            Ok(Heartbeat::ReRegistered)
        }
        // Cannot happen (the UPDATE already matches on pid); tolerate by
        // reporting a normal beat.
        Some(pid) if pid == info.pid as i64 => Ok(Heartbeat::Own),
        Some(holder_pid) => Ok(Heartbeat::TakenOver { holder_pid }),
    }
}

/// Instances whose heartbeat is fresher than `ttl`. Judged entirely on
/// the DB clock. Stale rows are NOT deleted here — cleanup is a separate
/// maintenance concern; this query only decides who counts as live.
pub async fn live_instances(
    pool: &PgPool,
    ttl: Duration,
) -> Result<Vec<InstanceRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, InstanceTuple>(
        &format!(
            r#"SELECT {}
               FROM boom_instance_state
               WHERE heartbeat_at > NOW() - ($1 * interval '1 second')"#,
            INSTANCE_COLUMNS
        ),
    )
    .bind(ttl.as_secs() as i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|t| {
            let (instance_id, hostname, pid, version, advertise_addr, snapshot, started_at, heartbeat_at) = t;
            InstanceRow {
                instance_id,
                hostname,
                pid,
                version,
                advertise_addr,
                snapshot,
                started_at,
                heartbeat_at,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_hostname_is_never_empty() {
        let h = resolve_hostname();
        assert!(!h.trim().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn pid_probe_self_is_alive() {
        assert!(pid_alive_local(std::process::id() as i64));
    }

    #[cfg(unix)]
    #[test]
    fn pid_probe_invalid_pids_are_dead() {
        assert!(!pid_alive_local(0));
        assert!(!pid_alive_local(-1));
        assert!(!pid_alive_local(i64::MAX));
    }

    #[cfg(unix)]
    #[test]
    fn pid_probe_exited_process_is_dead() {
        let mut child = std::process::Command::new("sleep")
            .arg("0")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i64;
        let _ = child.wait();
        assert!(!pid_alive_local(pid));
    }

    #[test]
    fn dead_holder_requires_stale_or_same_host_dead_pid() {
        // Stale heartbeat: always reclaimable, on any host.
        assert!(dead_holder(true, "other-host", 12345, "this-host"));
        // Fresh + same host + dead pid: crash-restart fast path.
        // (pid 0 is out-of-range ⇒ treated as dead by the probe.)
        assert!(dead_holder(false, "this-host", 0, "this-host"));
        // Fresh + same host + live pid: genuine conflict.
        assert!(!dead_holder(false, "this-host", std::process::id() as i64, "this-host"));
        // Fresh + different host: cannot probe, must conflict.
        assert!(!dead_holder(false, "other-host", 0, "this-host"));
        // "unknown" hostname disables the probe even with a dead pid —
        // the row may come from a different machine.
        assert!(!dead_holder(false, "unknown", 0, "unknown"));
    }

    #[test]
    fn conflict_message_names_the_holder() {
        let holder = InstanceRow {
            instance_id: "gw-a".into(),
            hostname: "host-1".into(),
            pid: 42,
            version: None,
            advertise_addr: None,
            snapshot: None,
            started_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            heartbeat_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        };
        let e = RegisterError::Conflict {
            instance_id: "gw-a".into(),
            holder: Some(holder),
        };
        let msg = e.to_string();
        assert!(msg.contains("gw-a"));
        assert!(msg.contains("host-1"));
        assert!(msg.contains("pid=42"));
        assert!(msg.contains("server.instance_id"));
    }
}
