//! Real-time pressure metrics storage — a fixed-capacity ring buffer of
//! `StressmonSample`s. Sampling (reading `/proc`, tokio metrics, the inflight
//! tracker) lives in boom-main's `spawn_stressmon_sampler` task; this crate
//! is intentionally just the data structure + the `StressmonApi` impl so
//! the dashboard can read it via `Arc<dyn StressmonApi>`.
//!
//! Why split like this: boom-stressmon is a leaf crate (depends only on
//! boom-core). Putting the sampler here would force a dependency on
//! boom-routing (for `InFlightTracker`) — that violates CLAUDE.md §1.
//! Keeping the sampler in boom-main (which already has every dep) and
//! passing samples in via `record_sample` keeps this crate pure data.

use boom_core::{StressmonApi, StressmonSample, StressmonSnapshot};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 60 minutes × 1 Hz. Fixed cap → bounded memory (~140 KB worst case).
const CAPACITY: usize = 60 * 60;

/// Fixed-capacity ring buffer. Writes overwrite the oldest sample once full.
/// `head` is the next write index, `len` tracks how many slots are populated
/// (so a snapshot before the first hour returns only what's been written).
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

    /// Return up to `window` most-recent samples in chronological order
    /// (oldest first, newest last) — the orientation the frontend chart
    /// expects when drawing left-to-right.
    fn recent(&self, window: usize) -> Vec<StressmonSample> {
        if self.len == 0 {
            return Vec::new();
        }
        let take = window.min(self.len);
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

pub struct StressmonCollector {
    samples: Mutex<RingBuffer>,
    num_workers: usize,
    /// Lifetime counter of samples whose normalized CPU exceeds 80%.
    /// Compared here (not in the sampler) so the rule stays with the data.
    cpu_over_80_count: AtomicU64,
}

impl StressmonCollector {
    pub fn new(num_workers: usize) -> Arc<Self> {
        Arc::new(Self {
            samples: Mutex::new(RingBuffer::new()),
            num_workers,
            cpu_over_80_count: AtomicU64::new(0),
        })
    }

    /// Push one sample. Called from boom-main's sampler task at 1 Hz.
    /// Also bumps the lifetime CPU-over-80% counter if this sample crosses
    /// the threshold — `cpu_pct > 0.8 * num_workers * 100` is equivalent to
    /// "normalized CPU% > 80%".
    pub fn record_sample(&self, sample: StressmonSample) {
        let threshold = (self.num_workers as f32) * 80.0;
        if sample.cpu_pct > threshold {
            self.cpu_over_80_count.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut buf) = self.samples.lock() {
            buf.push(sample);
        }
    }

    /// Read the most recent `window_secs` of samples. Clamped to ring
    /// buffer capacity (3600s) so a huge `window_secs` doesn't blow up —
    /// long-term history belongs to Prometheus, not this in-memory store.
    pub fn snapshot(&self, window_secs: i64) -> Vec<StressmonSample> {
        let window = (window_secs.clamp(0, CAPACITY as i64) as usize).max(1);
        match self.samples.lock() {
            Ok(buf) => buf.recent(window),
            Err(_) => Vec::new(),
        }
    }
}

#[async_trait]
impl StressmonApi for StressmonCollector {
    async fn timeseries(&self, window_secs: i64) -> StressmonSnapshot {
        let samples = self.snapshot(window_secs);
        StressmonSnapshot {
            num_workers: self.num_workers,
            samples,
            cpu_over_80_count: self.cpu_over_80_count.load(Ordering::Relaxed),
        }
    }
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

    #[test]
    fn empty_returns_empty() {
        let c = StressmonCollector::new(4);
        assert!(c.snapshot(60).is_empty());
    }

    #[test]
    fn returns_recent_in_chronological_order() {
        let c = StressmonCollector::new(4);
        for t in 1..=10 {
            c.record_sample(sample(t));
        }
        let out = c.snapshot(3);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].ts, 8);
        assert_eq!(out[1].ts, 9);
        assert_eq!(out[2].ts, 10);
    }

    #[test]
    fn window_clamps_to_available() {
        let c = StressmonCollector::new(4);
        for t in 1..=5 {
            c.record_sample(sample(t));
        }
        let out = c.snapshot(100);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0].ts, 1);
        assert_eq!(out[4].ts, 5);
    }

    #[test]
    fn wraps_and_overwrites_oldest() {
        let c = StressmonCollector::new(4);
        // Fill to capacity + 1.
        for t in 1..=CAPACITY + 1 {
            c.record_sample(sample(t as i64));
        }
        let out = c.snapshot(CAPACITY as i64);
        assert_eq!(out.len(), CAPACITY);
        // The first sample (ts=1) should be overwritten — out starts at ts=2.
        assert_eq!(out[0].ts, 2);
        // Newest sample is last.
        assert_eq!(out[CAPACITY - 1].ts, (CAPACITY + 1) as i64);
    }

    #[test]
    fn window_clamps_to_capacity() {
        let c = StressmonCollector::new(4);
        for t in 1..=10 {
            c.record_sample(sample(t));
        }
        // Request 10000s — should clamp to CAPACITY (3600), but only 10
        // samples are populated.
        let out = c.snapshot(10000);
        assert_eq!(out.len(), 10);
    }
}
