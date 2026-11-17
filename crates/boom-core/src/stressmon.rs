//! Real-time system pressure metrics — trait shared between boom-stressmon
//! (producer) and boom-dashboard (consumer). Defined here so both crates can
//! reference it without boom-dashboard depending on boom-stressmon (CLAUDE.md
//! §5 trait-over-concrete-type pattern).
//!
//! The canonical storage is one 10s point per instance, persisted to
//! `boom_stress_sample` (24h retention): every instance's dashboard reads
//! every series (self included) from the same rows via [`StressmonApi::history`],
//! so any page shows the same cluster picture. The in-process ring buffer
//! (`timeseries`) is the no-DB fallback and the aggregation buffer feeding
//! persistence. Anything longer-term belongs to an external Prometheus/Grafana
//! stack — this trait is for "who is under pressure, and since when".

use async_trait::async_trait;
use serde::Serialize;

/// A single 1Hz sample of process + runtime pressure. `Copy` so the ring
/// buffer can hand out by-value without cloning.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct StressmonSample {
    /// Unix seconds. Frontend formats in the viewer's local timezone.
    pub ts: i64,
    /// Process-level CPU utilization as a percentage. Sum of utime + stime
    /// from `/proc/self/stat`, diffed against the previous sample and scaled
    /// by wall-clock — same definition as `top`'s `100 * cpu_ticks / hertz /
    /// elapsed`. **Not normalized against worker count**: an 8-worker
    /// runtime under heavy load can report far more than 800% because tokio's
    /// blocking pool (prompt-log gzip, DB migrations, file I/O) runs threads
    /// outside the worker pool. The Y axis is left auto-scaled so a 16-core
    /// process can display as 1300%.
    pub cpu_pct: f32,
    /// Resident set size in bytes (`/proc/self/status` VmRSS).
    pub rss_bytes: u64,
    /// Sum of `worker_queue_depth(i)` across all tokio workers. Non-zero +
    /// rising = workers can't keep up.
    pub worker_queue_depth: usize,
    /// `num_blocking_tasks_queued` — spawn_blocking backlog (prompt-log gzip,
    /// DB migrations, etc).
    pub blocking_tasks_queued: usize,
    /// In-flight request count at sample time (from `InFlightTracker`).
    pub inflight: u64,
}

/// One persisted 10s point as stored in `boom_stress_sample`. Same fields
/// as [`StressmonSample`]; `instance_id` rides every row so a single
/// `ts_secs > since` query returns every instance's series interleaved.
#[derive(Debug, Clone, Serialize)]
pub struct StressHistorySample {
    pub instance_id: String,
    pub ts: i64,
    pub cpu_pct: f32,
    pub rss_bytes: u64,
    pub worker_queue_depth: u32,
    pub blocking_tasks_queued: u32,
    pub inflight: u32,
}

/// Response to `timeseries()` — samples plus the cached worker count so the
/// frontend can derive the "worker pool 80% saturated" warning threshold
/// (`num_workers × 80` in absolute process-CPU% units).
#[derive(Debug, Serialize)]
pub struct StressmonSnapshot {
    pub num_workers: usize,
    pub samples: Vec<StressmonSample>,
    /// Seconds since the gateway process started. Same `started_at` origin
    /// as `/health`'s `uptime_secs` (AppStateInner.health.started_at);
    /// survives hot reloads and range switches.
    pub uptime_secs: u64,
}

#[async_trait]
pub trait StressmonApi: Send + Sync + 'static {
    /// Return samples from `[now - window_secs, now)`. `window_secs` is
    /// clamped to the producer's ring buffer capacity (24h of 10s points)
    /// by the impl. Local ring only — the no-DB display path.
    async fn timeseries(&self, window_secs: i64) -> StressmonSnapshot;

    /// Persisted 10s points for ALL instances (self included) with
    /// `ts > since_ts`, oldest-first. `since_ts = 0` fetches the whole
    /// retained 24h. Empty when running without a DB.
    async fn history(&self, since_ts: i64) -> Vec<StressHistorySample>;
}
