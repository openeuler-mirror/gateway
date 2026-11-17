//! System pressure metrics: 1Hz sampling aggregated into wall-clock-aligned
//! 10s MAX points, kept in a 24h ring buffer and (when a DB is configured)
//! persisted to `boom_stress_sample` one row per (instance, bucket).
//!
//! Why MAX per bucket: the page exists to surface spikes — a mean would
//! shave off exactly the interesting part. Why 10s buckets: one persisted
//! point per 10s per instance is ~8.6k rows/day/instance (~1MB), cheap to
//! write and read incrementally, while still resolving pressure bursts to
//! the bucket granularity.
//!
//! Sampling (reading `/proc`, tokio metrics, the inflight tracker) lives in
//! boom-main's `spawn_stressmon_sampler` task; this crate stays a leaf
//! (boom-core + sqlx only) per CLAUDE.md §1. The sampler pushes 1Hz samples
//! in via [`StressmonCollector::observe`], which folds them into the open
//! bucket and returns each bucket on close for the caller to persist.

pub mod migrations;

use boom_core::{StressHistorySample, StressmonApi, StressmonSample, StressmonSnapshot};
use async_trait::async_trait;
use sqlx::PgPool;
use std::sync::{Arc, Mutex};

/// Seconds per persisted point. Buckets are wall-clock aligned
/// (`ts - ts % PERIOD`), so every instance maps the same wall interval to
/// the same row — the limiter's window-alignment lesson (21e0c0b) applied
/// to telemetry: deterministic placement, idempotent writes, zero
/// coordination.
pub const PERIOD_SECS: i64 = 10;

/// 24h of 10s points. Fixed cap → bounded memory (~400 KB worst case).
const CAPACITY: usize = 24 * 60 * 60 / PERIOD_SECS as usize;

/// Rows older than this are pruned. Small grace beyond 24h so a chart
/// requesting "last 24h" never sees a shaved tail.
pub const RETENTION_SECS: i64 = 24 * 60 * 60 + 60;

/// Fixed-capacity ring buffer. Writes overwrite the oldest sample once full.
/// `head` is the next write index, `len` tracks how many slots are populated
/// (so a snapshot before the first 24h returns only what's been written).
struct RingBuffer {
    data: Vec<StressmonSample>,
    head: usize,
    len: usize,
}

impl RingBuffer {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(CAPACITY),
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, sample: StressmonSample) {
        if self.data.len() < CAPACITY {
            self.data.push(sample);
            self.head = (self.head + 1) % CAPACITY;
            self.len += 1;
        } else {
            self.data[self.head] = sample;
            self.head = (self.head + 1) % CAPACITY;
            if self.len < CAPACITY {
                self.len += 1;
            }
        }
    }

    /// Return up to `count` most-recent samples in chronological order
    /// (oldest first, newest last) — the orientation the frontend chart
    /// expects when drawing left-to-right.
    fn recent(&self, count: usize) -> Vec<StressmonSample> {
        if self.len == 0 {
            return Vec::new();
        }
        let take = count.min(self.len);
        let start = if self.len < CAPACITY {
            // Buffer not yet wrapped: `head` is the count, samples 0..head.
            self.head - take
        } else {
            // Wrapped: `head` points at the oldest sample, newest is `head-1`.
            (self.head + CAPACITY - take) % CAPACITY
        };
        let mut out = Vec::with_capacity(take);
        for i in 0..take {
            let idx = (start + i) % CAPACITY;
            out.push(self.data[idx]);
        }
        out
    }
}

/// Open-bucket accumulator: per-field running MAX of the 1Hz samples that
/// landed in the current 10s bucket.
#[derive(Clone, Copy)]
struct OpenBucket {
    /// Wall-clock bucket start (`ts - ts % PERIOD_SECS`).
    bucket_ts: i64,
    max: StressmonSample,
}

pub struct StressmonCollector {
    /// Closed buckets (10s points), newest last.
    samples: Mutex<RingBuffer>,
    /// The bucket currently accumulating 1Hz samples. None until the first
    /// sample arrives.
    open: Mutex<Option<OpenBucket>>,
    num_workers: usize,
    /// Process start time, passed in from boom-main (the same origin as
    /// `AppStateInner.health.started_at`, which survives hot reloads) so the
    /// dashboard's uptime stays consistent with `/health`'s `uptime_secs`.
    started_at: chrono::DateTime<chrono::Utc>,
    /// Set when running with a DB: enables `history()` reads. Writes are
    /// the sampler task's job (it owns pacing + prune), so the pool is not
    /// stored here — reads and writes stay independently testable.
    pool: Option<PgPool>,
}

impl StressmonCollector {
    pub fn new(
        num_workers: usize,
        started_at: chrono::DateTime<chrono::Utc>,
        pool: Option<PgPool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            samples: Mutex::new(RingBuffer::new()),
            open: Mutex::new(None),
            num_workers,
            started_at,
            pool,
        })
    }

    /// Fold one 1Hz sample into the open 10s bucket. When the sample starts
    /// a NEW bucket, the closed bucket's MAX point is pushed to the ring and
    /// returned for the caller to persist. A sample whose bucket is older
    /// than the open one (clock step back) is dropped — it can no longer
    /// affect a bucket that may already be persisted.
    pub fn observe(&self, sample: StressmonSample) -> Option<StressmonSample> {
        let bucket_ts = sample.ts - sample.ts.rem_euclid(PERIOD_SECS);
        let mut open = self.open.lock().unwrap();
        match open.as_mut() {
            Some(acc) if acc.bucket_ts == bucket_ts => {
                acc.max.cpu_pct = acc.max.cpu_pct.max(sample.cpu_pct);
                acc.max.rss_bytes = acc.max.rss_bytes.max(sample.rss_bytes);
                acc.max.worker_queue_depth =
                    acc.max.worker_queue_depth.max(sample.worker_queue_depth);
                acc.max.blocking_tasks_queued =
                    acc.max.blocking_tasks_queued.max(sample.blocking_tasks_queued);
                acc.max.inflight = acc.max.inflight.max(sample.inflight);
                None
            }
            Some(acc) if acc.bucket_ts < bucket_ts => {
                let closed = acc.max;
                acc.bucket_ts = bucket_ts;
                acc.max = StressmonSample { ts: bucket_ts, ..sample };
                if let Ok(mut buf) = self.samples.lock() {
                    buf.push(closed);
                }
                Some(closed)
            }
            // Clock step back onto an older bucket: drop — the open bucket
            // (possibly already-persisted history) must not be disturbed.
            Some(_) => None,
            // First sample ever.
            None => {
                *open = Some(OpenBucket {
                    bucket_ts,
                    max: StressmonSample { ts: bucket_ts, ..sample },
                });
                None
            }
        }
    }

    /// Read the most recent `window_secs` of 10s points (rounded up to whole
    /// buckets). Clamped to the ring's 24h capacity.
    pub fn snapshot(&self, window_secs: i64) -> Vec<StressmonSample> {
        let buckets = (window_secs + PERIOD_SECS - 1) / PERIOD_SECS;
        let count = buckets.clamp(1, CAPACITY as i64) as usize;
        match self.samples.lock() {
            Ok(buf) => buf.recent(count),
            Err(_) => Vec::new(),
        }
    }
}

#[async_trait]
impl StressmonApi for StressmonCollector {
    async fn timeseries(&self, window_secs: i64) -> StressmonSnapshot {
        let samples = self.snapshot(window_secs);
        let uptime_secs = chrono::Utc::now()
            .signed_duration_since(self.started_at)
            .num_seconds()
            .max(0) as u64;
        StressmonSnapshot {
            num_workers: self.num_workers,
            samples,
            uptime_secs,
        }
    }

    async fn history(&self, since_ts: i64) -> Vec<StressHistorySample> {
        match &self.pool {
            Some(pool) => fetch_history(pool, since_ts).await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "stress history read failed");
                Vec::new()
            }),
            None => Vec::new(),
        }
    }
}

/// Incremental read: every instance's points with `ts_secs > since_ts`,
/// oldest-first, bounded as a safety valve. `since_ts = 0` is the initial
/// full 24h load; callers then track their max ts and ask only for what's
/// new. served index: `ts_secs` (the PK leads with instance_id).
pub async fn fetch_history(
    pool: &PgPool,
    since_ts: i64,
) -> Result<Vec<StressHistorySample>, sqlx::Error> {
    sqlx::query_as::<_, (String, i64, f32, i64, i32, i32, i32)>(
        "SELECT instance_id, ts_secs, cpu_pct, rss_bytes, worker_queue_depth, \
         blocking_tasks_queued, inflight FROM boom_stress_sample \
         WHERE ts_secs > $1 ORDER BY ts_secs ASC LIMIT 300000",
    )
    .bind(since_ts)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|(instance_id, ts, cpu_pct, rss_bytes, wq, bq, inflight)| StressHistorySample {
                instance_id,
                ts,
                cpu_pct,
                rss_bytes: rss_bytes as u64,
                worker_queue_depth: wq as u32,
                blocking_tasks_queued: bq as u32,
                inflight: inflight as u32,
            })
            .collect()
    })
}

/// Persist one closed bucket. UPDATE-then-INSERT (`gaussdb_upsert!`) so a
/// bucket re-finalized after a restart (or a duplicate instance id) lands
/// on the same row instead of dying on the PK.
pub async fn write_sample(
    pool: &PgPool,
    instance_id: &str,
    s: &StressmonSample,
) -> Result<(), sqlx::Error> {
    boom_core::gaussdb_upsert!(
        pool,
        || {
            sqlx::query(
                "UPDATE boom_stress_sample SET cpu_pct=$3, rss_bytes=$4, \
                 worker_queue_depth=$5, blocking_tasks_queued=$6, inflight=$7 \
                 WHERE instance_id=$1 AND ts_secs=$2",
            )
            .bind(instance_id)
            .bind(s.ts)
            .bind(s.cpu_pct)
            .bind(s.rss_bytes as i64)
            .bind(s.worker_queue_depth as i32)
            .bind(s.blocking_tasks_queued as i32)
            .bind(s.inflight as i32)
        },
        || {
            sqlx::query(
                "INSERT INTO boom_stress_sample (instance_id, ts_secs, cpu_pct, \
                 rss_bytes, worker_queue_depth, blocking_tasks_queued, inflight) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7)",
            )
            .bind(instance_id)
            .bind(s.ts)
            .bind(s.cpu_pct)
            .bind(s.rss_bytes as i64)
            .bind(s.worker_queue_depth as i32)
            .bind(s.blocking_tasks_queued as i32)
            .bind(s.inflight as i32)
        }
    )
}

/// Delete rows past the retention window. Safe to run concurrently from
/// several instances — the second DELETE simply affects zero rows.
pub async fn prune(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let cutoff = chrono::Utc::now().timestamp() - RETENTION_SECS;
    let res = sqlx::query("DELETE FROM boom_stress_sample WHERE ts_secs < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64) -> StressmonSample {
        StressmonSample {
            ts,
            cpu_pct: 0.0,
            rss_bytes: 0,
            worker_queue_depth: 0,
            blocking_tasks_queued: 0,
            inflight: 0,
        }
    }

    fn collector() -> StressmonCollector {
        StressmonCollector {
            samples: Mutex::new(RingBuffer::new()),
            open: Mutex::new(None),
            num_workers: 4,
            started_at: chrono::Utc::now(),
            pool: None,
        }
    }

    #[test]
    fn observe_folds_max_per_field_and_closes_on_boundary() {
        let c = collector();
        // Bucket 1000..1009: peaks land in different samples.
        assert_eq!(c.observe(sample(1000)), None);
        assert_eq!(c.observe(sample(1005)), None);
        // First sample of bucket 1010 closes bucket 1000.
        let closed = c.observe(sample(1012)).expect("bucket 1000 must close");
        assert_eq!(closed.ts, 1000); // point ts = aligned bucket start
        // Bucket 1010 stays open until 1020 arrives.
        assert_eq!(c.observe(sample(1019)), None);
        let closed = c.observe(sample(1020)).expect("bucket 1010 closes");
        assert_eq!(closed.ts, 1010);
        assert_eq!(c.snapshot(30).len(), 2);
    }

    #[test]
    fn observe_takes_independent_field_maxima() {
        let c = collector();
        let mut a = sample(2000);
        a.cpu_pct = 90.0;
        a.inflight = 1;
        assert_eq!(c.observe(a), None);
        let mut b = sample(2003);
        b.cpu_pct = 10.0;
        b.inflight = 9;
        b.rss_bytes = 500;
        assert_eq!(c.observe(b), None);
        let closed = c.observe(sample(2010)).unwrap();
        assert_eq!(closed.cpu_pct, 90.0);
        assert_eq!(closed.inflight, 9);
        assert_eq!(closed.rss_bytes, 500);
    }

    #[test]
    fn observe_drops_clock_step_back() {
        let c = collector();
        c.observe(sample(5000));
        c.observe(sample(5010));
        // A sample for the already-closed 5000 bucket must not reopen it.
        assert_eq!(c.observe(sample(5001)), None);
        assert_eq!(c.observe(sample(5020)).unwrap().ts, 5010);
    }

    #[test]
    fn snapshot_window_rounds_up_and_clamps() {
        let c = collector();
        for b in 0..5 {
            c.observe(sample(b * 10));
        }
        // 45s → ceil to 5 buckets, but only 4 closed.
        assert_eq!(c.snapshot(45).len(), 4);
        // Huge window clamps to capacity, still bounded by written count.
        assert_eq!(c.snapshot(10_000_000).len(), 4);
        // Zero window clamps to one bucket (pre-existing timeseries
        // semantics — callers use it as a cheap meta fetch).
        assert_eq!(c.snapshot(0).len(), 1);
    }

    #[test]
    fn ring_wraps_and_keeps_newest() {
        let c = collector();
        for b in 0..(CAPACITY as i64 + 5) {
            c.observe(sample(b * 10));
        }
        let out = c.snapshot(i64::MAX / 2);
        assert_eq!(out.len(), CAPACITY);
        // CAP+4 buckets closed; the oldest 4 were overwritten.
        assert_eq!(out[0].ts, 4 * 10);
        assert_eq!(out[CAPACITY - 1].ts, (CAPACITY as i64 + 3) * 10);
    }

    #[tokio::test]
    async fn uptime_counts_from_started_at() {
        // Started 1h ago → uptime ~3600s (allow scheduling slack, bounded
        // above so a wildly wrong origin fails).
        let started = chrono::Utc::now() - chrono::Duration::seconds(3600);
        let c = StressmonCollector::new(4, started, None);
        let snap = c.timeseries(60).await;
        assert!(snap.uptime_secs >= 3600 && snap.uptime_secs <= 3610);

        // Future origin (clock skew) must not panic — clamps to 0.
        let future = chrono::Utc::now() + chrono::Duration::seconds(300);
        let c = StressmonCollector::new(4, future, None);
        let snap = c.timeseries(60).await;
        assert_eq!(snap.uptime_secs, 0);
    }

    #[tokio::test]
    async fn history_without_pool_is_empty() {
        let c = StressmonCollector::new(4, chrono::Utc::now(), None);
        assert!(c.history(0).await.is_empty());
    }
}
