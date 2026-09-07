use std::collections::BTreeMap;
use std::sync::Mutex;

const NUM_BUCKETS: usize = 60;

/// Tracks key-affinity rebalance events over the last 60 minutes.
///
/// Internally a ring buffer of 60 counters, one per minute.
/// Bucket index is `minute % 60` — fixed mapping, independent of window position.
/// Expired buckets are zeroed lazily on the next `record()` or `snapshot()`.
pub struct RebalanceCounter {
    inner: Mutex<Inner>,
}

struct Inner {
    /// The earliest minute still in the valid window.
    /// Valid range: `[start_minute, start_minute + 59]`.
    start_minute: u64,
    /// 60 counters, indexed by `minute % 60`.
    buckets: [u64; NUM_BUCKETS],
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

impl RebalanceCounter {
    pub fn new() -> Self {
        let now = to_minute(now_epoch_secs());
        Self {
            inner: Mutex::new(Inner {
                start_minute: now.saturating_sub((NUM_BUCKETS - 1) as u64),
                buckets: [0; NUM_BUCKETS],
            }),
        }
    }

    /// Record one rebalance event in the current minute's bucket.
    pub fn record(&self) {
        let now_min = to_minute(now_epoch_secs());
        let mut guard = self.inner.lock().unwrap();
        guard.advance_to(now_min);
        guard.buckets[now_min as usize % NUM_BUCKETS] += 1;
    }

    /// Return 60 data points for the last 60 minutes.
    ///
    /// Each entry is `(label, count)` where label is like "-59m", "-58m", ..., "now".
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let now_min = to_minute(now_epoch_secs());
        let mut guard = self.inner.lock().unwrap();
        guard.advance_to(now_min);

        let base = now_min.saturating_sub((NUM_BUCKETS - 1) as u64);
        let mut result = Vec::with_capacity(NUM_BUCKETS);
        for i in 0..NUM_BUCKETS {
            let min = base + i as u64;
            let label = if i == NUM_BUCKETS - 1 {
                "now".to_string()
            } else {
                format!("-{}m", NUM_BUCKETS - 1 - i)
            };
            result.push((label, guard.buckets[min as usize % NUM_BUCKETS]));
        }
        result
    }
}

impl Inner {
    /// Advance the window so that `target_min` is within range,
    /// clearing any buckets that fell outside the 60-minute window.
    fn advance_to(&mut self, target_min: u64) {
        let new_start = target_min.saturating_sub((NUM_BUCKETS - 1) as u64);

        if new_start <= self.start_minute {
            return;
        }

        let shift = (new_start - self.start_minute) as usize;

        if shift >= NUM_BUCKETS {
            // Entire buffer is stale.
            self.buckets = [0; NUM_BUCKETS];
        } else {
            // Clear expired buckets: minutes [old_start, new_start - 1].
            for m in self.start_minute..new_start {
                self.buckets[m as usize % NUM_BUCKETS] = 0;
            }
        }

        self.start_minute = new_start;
    }
}

// ═══════════════════════════════════════════════════════════
// RebalanceMoveTracker — per-deployment (in, out) lifetime counters
// ═══════════════════════════════════════════════════════════

/// Per-deployment rebalance move counts.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct MoveCounts {
    pub in_count: u64,
    pub out_count: u64,
}

/// A single deployment's rebalance move snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RebalanceMove {
    pub deployment_id: String,
    pub in_count: u64,
    pub out_count: u64,
}

/// Tracks per-deployment rebalance migration counts over the process lifetime.
///
/// No time window, no decay — accumulates from startup until restart.
/// `record_move(from, to)` records one rebalance event: `from` gets out+1,
/// `to` gets in+1. Either side may be `None` (deployment has no id), in which
/// case that direction is skipped.
pub struct RebalanceMoveTracker {
    inner: Mutex<BTreeMap<String, MoveCounts>>,
}

impl RebalanceMoveTracker {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// Record one rebalance move.
    /// `from_id` = the deployment being migrated away from (out+1).
    /// `to_id` = the deployment being migrated to (in+1).
    /// Either may be `None` — that direction is skipped.
    pub fn record_move(&self, from_id: Option<&str>, to_id: Option<&str>) {
        let mut map = self.inner.lock().unwrap();
        if let Some(id) = from_id {
            if !id.is_empty() {
                map.entry(id.to_string())
                    .or_insert_with(MoveCounts::default)
                    .out_count += 1;
            }
        }
        if let Some(id) = to_id {
            if !id.is_empty() {
                map.entry(id.to_string())
                    .or_insert_with(MoveCounts::default)
                    .in_count += 1;
            }
        }
    }

    /// Snapshot all deployments that have ever appeared in a rebalance.
    /// Returns entries sorted by deployment_id (BTreeMap natural order).
    pub fn snapshot(&self) -> Vec<RebalanceMove> {
        let map = self.inner.lock().unwrap();
        map.iter()
            .map(|(id, c)| RebalanceMove {
                deployment_id: id.clone(),
                in_count: c.in_count,
                out_count: c.out_count,
            })
            .collect()
    }
}

impl Default for RebalanceMoveTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_snapshot() {
        let counter = RebalanceCounter::new();
        counter.record();
        counter.record();
        counter.record();

        let snap = counter.snapshot();
        assert_eq!(snap.len(), 60);
        let (_label, count) = snap.last().unwrap();
        assert_eq!(*count, 3);
    }

    #[test]
    fn test_snapshot_labels() {
        let counter = RebalanceCounter::new();
        let snap = counter.snapshot();
        assert_eq!(snap[0].0, "-59m");
        assert_eq!(snap[59].0, "now");
    }

    #[test]
    fn test_advance_clears_old_data() {
        let counter = RebalanceCounter::new();
        // Simulate writing at a known minute.
        {
            let mut guard = counter.inner.lock().unwrap();
            let write_min = guard.start_minute + 30; // 30 minutes in
            guard.buckets[write_min as usize % NUM_BUCKETS] = 5;
        }

        let snap = counter.snapshot();
        // The event should show up at "-29m" (30 minutes from start, 29 before now).
        assert!(snap.iter().any(|(_, c)| *c == 5));
    }

    // ── RebalanceMoveTracker tests ──

    fn find<'a>(moves: &'a [RebalanceMove], id: &'a str) -> Option<&'a RebalanceMove> {
        moves.iter().find(|m| m.deployment_id == id)
    }

    #[test]
    fn test_move_basic() {
        let t = RebalanceMoveTracker::new();
        for _ in 0..3 {
            t.record_move(Some("a"), Some("b"));
        }
        let s = t.snapshot();
        assert_eq!(s.len(), 2);
        let a = find(&s, "a").unwrap();
        assert_eq!(a.in_count, 0);
        assert_eq!(a.out_count, 3);
        let b = find(&s, "b").unwrap();
        assert_eq!(b.in_count, 3);
        assert_eq!(b.out_count, 0);
    }

    #[test]
    fn test_move_none_to() {
        let t = RebalanceMoveTracker::new();
        t.record_move(Some("a"), None);
        let s = t.snapshot();
        assert_eq!(s.len(), 1);
        let a = find(&s, "a").unwrap();
        assert_eq!(a.out_count, 1);
        assert_eq!(a.in_count, 0);
    }

    #[test]
    fn test_move_none_from() {
        let t = RebalanceMoveTracker::new();
        t.record_move(None, Some("b"));
        let s = t.snapshot();
        assert_eq!(s.len(), 1);
        let b = find(&s, "b").unwrap();
        assert_eq!(b.in_count, 1);
        assert_eq!(b.out_count, 0);
    }

    #[test]
    fn test_move_both_none() {
        let t = RebalanceMoveTracker::new();
        t.record_move(None, None);
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn test_move_empty_id_skipped() {
        let t = RebalanceMoveTracker::new();
        t.record_move(Some(""), Some(""));
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn test_move_same_id() {
        let t = RebalanceMoveTracker::new();
        t.record_move(Some("a"), Some("a"));
        let s = t.snapshot();
        assert_eq!(s.len(), 1);
        let a = find(&s, "a").unwrap();
        assert_eq!(a.in_count, 1);
        assert_eq!(a.out_count, 1);
    }

    #[test]
    fn test_move_empty_snapshot() {
        let t = RebalanceMoveTracker::new();
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn test_move_sorted() {
        let t = RebalanceMoveTracker::new();
        t.record_move(Some("c"), Some("a"));
        t.record_move(Some("b"), Some("d"));
        let s: Vec<String> = t.snapshot().iter().map(|m| m.deployment_id.clone()).collect();
        assert_eq!(s, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn test_move_bidirectional() {
        let t = RebalanceMoveTracker::new();
        t.record_move(Some("a"), Some("b"));
        t.record_move(Some("b"), Some("a"));
        let s = t.snapshot();
        let a = find(&s, "a").unwrap();
        assert_eq!(a.in_count, 1);
        assert_eq!(a.out_count, 1);
        let b = find(&s, "b").unwrap();
        assert_eq!(b.in_count, 1);
        assert_eq!(b.out_count, 1);
    }

    #[test]
    fn test_move_concurrent() {
        let t = std::sync::Arc::new(RebalanceMoveTracker::new());
        let mut handles = vec![];
        for _ in 0..4 {
            let t = t.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    t.record_move(Some("a"), Some("b"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let s = t.snapshot();
        let a = find(&s, "a").unwrap();
        let b = find(&s, "b").unwrap();
        assert_eq!(a.out_count, 400);
        assert_eq!(b.in_count, 400);
    }
}
