//! Real-time system pressure metrics — trait shared between boom-stressmon
//! (producer) and boom-dashboard (consumer). Defined here so both crates can
//! reference it without boom-dashboard depending on boom-stressmon (CLAUDE.md
//! §5 trait-over-concrete-type pattern).
//!
//! The dashboard polls `timeseries` every 1.5s for the live chart. The window
//! is bounded by the producer's ring buffer capacity (1Hz × 60min = 3600
//! samples). Anything longer-term belongs to an external Prometheus/Grafana
//! stack — this trait is for "is the runtime under pressure *right now*".

use async_trait::async_trait;
use serde::Serialize;

/// A single 1Hz sample of process + runtime pressure. `Copy` so the ring
/// buffer can hand out by-value without cloning.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct StressmonSample {
    /// Unix seconds. Frontend formats in the viewer's local timezone.
    pub ts: i64,
    /// Process-level CPU% (utime+stime delta over wall clock). Range
    /// `0..=num_workers * 100` — a fully loaded 4-worker process peaks at 400.
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

/// Response to `timeseries()` — samples plus the cached worker count so the
/// frontend can scale the CPU axis (`cpu_pct / num_workers / 100`).
#[derive(Debug, Serialize)]
pub struct StressmonSnapshot {
    pub num_workers: usize,
    pub samples: Vec<StressmonSample>,
    /// Cumulative count of samples where normalized CPU exceeded 80% —
    /// `cpu_pct > 0.8 * num_workers * 100`. Lifetime of the process,
    /// survives range switches (it's a counter, not a windowed value).
    pub cpu_over_80_count: u64,
}

#[async_trait]
pub trait StressmonApi: Send + Sync + 'static {
    /// Return samples from `[now - window_secs, now)`. `window_secs` is
    /// clamped to the producer's ring buffer capacity (3600s) by the impl.
    async fn timeseries(&self, window_secs: i64) -> StressmonSnapshot;
}
