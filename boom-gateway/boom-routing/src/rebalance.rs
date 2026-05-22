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
}
