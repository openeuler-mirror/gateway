//! 60-minute ring-buffer tracker for agent (client-type) statistics.
//!
//! Modelled after `boom_routing::RebalanceCounter`: fixed mapping
//! `bucket = minute % 60`, lazy eviction on `record`/`snapshot`.
//! Each bucket carries two counters — total requests and the subset
//! that hit the Anthropic-native endpoint — so the dashboard can
//! render a stacked bar (total height = total, green segment =
//! anthropic share) without a second tracker.

use std::sync::Mutex;

use serde::Serialize;

use crate::classifier::is_anthropic_path;

const NUM_BUCKETS: usize = 60;

#[derive(Debug, Clone, Default, Serialize)]
pub struct MinuteBucket {
    pub minute: String,
    pub total: u64,
    pub anthropic: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSummary {
    pub total: u64,
    pub anthropic: u64,
    /// Anthropic share over the last hour, `0.0..=1.0`. `0.0` when
    /// there are no samples to avoid a divide-by-zero on cold start.
    pub ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentStatsSnapshot {
    /// 60 entries, oldest first (`-59m`) → newest last (`now`).
    pub events: Vec<MinuteBucket>,
    pub summary: AgentSummary,
}

#[derive(Default, Clone, Copy)]
struct Bucket {
    total: u64,
    anthropic: u64,
}

pub struct AgentStatsTracker {
    inner: Mutex<Inner>,
}

struct Inner {
    /// Earliest minute still inside the valid window.
    start_minute: u64,
    buckets: [Bucket; NUM_BUCKETS],
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn to_minute(secs: u64) -> u64 {
    secs / 60
}

impl AgentStatsTracker {
    pub fn new() -> Self {
        let now = to_minute(now_epoch_secs());
        Self {
            inner: Mutex::new(Inner {
                start_minute: now.saturating_sub((NUM_BUCKETS - 1) as u64),
                buckets: [Bucket::default(); NUM_BUCKETS],
            }),
        }
    }

    /// Record one request under the current minute's bucket.
    pub fn record(&self, api_path: &str) {
        let now_min = to_minute(now_epoch_secs());
        let anthropic = is_anthropic_path(api_path);
        let mut guard = self.inner.lock().unwrap();
        guard.advance_to(now_min);
        let slot = &mut guard.buckets[now_min as usize % NUM_BUCKETS];
        slot.total += 1;
        if anthropic {
            slot.anthropic += 1;
        }
    }

    /// Return 60 one-minute buckets covering the last hour.
    pub fn snapshot(&self) -> AgentStatsSnapshot {
        let now_min = to_minute(now_epoch_secs());
        let mut guard = self.inner.lock().unwrap();
        guard.advance_to(now_min);

        let base = now_min.saturating_sub((NUM_BUCKETS - 1) as u64);
        let mut events = Vec::with_capacity(NUM_BUCKETS);
        let mut total: u64 = 0;
        let mut anthropic: u64 = 0;
        for i in 0..NUM_BUCKETS {
            let min = base + i as u64;
            let slot = guard.buckets[min as usize % NUM_BUCKETS];
            let label = if i == NUM_BUCKETS - 1 {
                "now".to_string()
            } else {
                format!("-{}m", NUM_BUCKETS - 1 - i)
            };
            total += slot.total;
            anthropic += slot.anthropic;
            events.push(MinuteBucket {
                minute: label,
                total: slot.total,
                anthropic: slot.anthropic,
            });
        }

        let ratio = if total == 0 {
            0.0
        } else {
            anthropic as f64 / total as f64
        };

        AgentStatsSnapshot {
            events,
            summary: AgentSummary {
                total,
                anthropic,
                ratio,
            },
        }
    }
}

impl Default for AgentStatsTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
    fn advance_to(&mut self, target_min: u64) {
        let new_start = target_min.saturating_sub((NUM_BUCKETS - 1) as u64);

        if new_start <= self.start_minute {
            return;
        }

        if new_start.saturating_sub(self.start_minute) >= NUM_BUCKETS as u64 {
            self.buckets = [Bucket::default(); NUM_BUCKETS];
        } else {
            for m in self.start_minute..new_start {
                self.buckets[m as usize % NUM_BUCKETS] = Bucket::default();
            }
        }

        self.start_minute = new_start;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_buckets_by_path() {
        let tracker = AgentStatsTracker::new();
        tracker.record("/v1/messages");
        tracker.record("/v1/messages");
        tracker.record("/v1/chat/completions");
        tracker.record("/v1/completions");

        let snap = tracker.snapshot();
        assert_eq!(snap.events.len(), 60);
        let last = snap.events.last().unwrap();
        assert_eq!(last.total, 4);
        assert_eq!(last.anthropic, 2);
        assert!((snap.summary.ratio - 0.5).abs() < 1e-9);
    }

    #[test]
    fn summary_aggregates_all_buckets() {
        let tracker = AgentStatsTracker::new();
        // Current minute.
        tracker.record("/v1/messages");
        tracker.record("/v1/chat/completions");

        let snap = tracker.snapshot();
        assert_eq!(snap.summary.total, 2);
        assert_eq!(snap.summary.anthropic, 1);
    }

    #[test]
    fn cold_start_has_zero_ratio() {
        let tracker = AgentStatsTracker::new();
        let snap = tracker.snapshot();
        assert_eq!(snap.summary.total, 0);
        assert_eq!(snap.summary.ratio, 0.0);
    }

    #[test]
    fn advance_clears_old_buckets() {
        let tracker = AgentStatsTracker::new();
        {
            let mut guard = tracker.inner.lock().unwrap();
            let target_min = guard.start_minute + 70; // beyond full window
            guard.advance_to(target_min);
        }
        let snap = tracker.snapshot();
        assert_eq!(snap.summary.total, 0);
    }
}
