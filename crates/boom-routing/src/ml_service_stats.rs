//! Statistics and periodic summary logging for [`MlServiceClient`].
//!
//! # Design
//!
//! - **No `tokio::spawn`.** Summaries are emitted inline from `classify()`
//!   based on a `last_summary_at` timestamp. Zero background tasks, zero
//!   Drop-management, zero resource cost when idle.
//! - **Tradeoff:** if traffic stops, no summary is emitted for the partial
//!   window. Acceptable for an ML classifier with sustained traffic.
//! - **Lock-free counters.** `AtomicU64` for scalars, `DashMap` for the
//!   tier distribution (tier names are dynamic, configured per-deployment)
//!   — matches the pattern in `policy::round_robin::RoundRobinPolicy`.
//! - **Min/max latency via CAS.** Only successful calls update them;
//!   failure latency reflects the failure mode, not the ML service's real
//!   response time.
//! - **Order in `classify()`**: `maybe_emit_summary` runs BEFORE
//!   `record_attempt`. This keeps the invariant `attempts == successes +
//!   failures` in every emitted summary — the triggering request is fully
//!   accounted in the next window, not half-counted in this one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;

/// Window for periodic summary emission. 60s = operator-friendly cadence
/// without flooding logs at high QPS.
const SUMMARY_WINDOW: Duration = Duration::from_secs(60);

/// Sentinel value for "no latency recorded yet". Using `u64::MAX` so
/// the first real latency (always smaller) wins the CAS in
/// [`MlServiceStats::record_success`].
const NO_LATENCY: u64 = u64::MAX;

/// Counters for one [`crate::MlServiceClient`] instance.
///
/// All fields are atomic — no locking, no contention on the hot path.
/// `tier_distribution` is a `DashMap` because tier names are dynamic
/// (configured per-deployment); matches the pattern in
/// `boom_routing::policy::round_robin::RoundRobinPolicy`.
pub struct MlServiceStats {
    /// Total `classify()` calls entered (before HTTP).
    pub attempts: AtomicU64,
    /// HTTP call returned 2xx + valid tier.
    pub successes: AtomicU64,
    /// HTTP failed (timeout / non-2xx / malformed / unknown tier).
    /// Invariant: `failures == attempts - successes` (fallbacks are a
    /// subset of failures).
    pub failures: AtomicU64,
    /// Times we fell back to `TierClassifier`. Subset of `failures`.
    pub fallbacks: AtomicU64,
    /// Accumulator for average latency computation (nanoseconds).
    pub total_latency_ns: AtomicU64,
    /// Maximum observed latency across successful calls (nanoseconds).
    /// Updated via CAS: only wins if new value is larger.
    pub max_latency_ns: AtomicU64,
    /// Minimum observed latency across successful calls (nanoseconds).
    /// Initialized to [`NO_LATENCY`] (= "no sample yet"); updated via
    /// CAS: only wins if new value is smaller.
    pub min_latency_ns: AtomicU64,
    /// Final tier returned → count. Keyed by tier name (e.g. "small").
    pub tier_distribution: DashMap<String, AtomicU64>,
    /// Nanosecond timestamp of last summary emission (Unix epoch).
    /// Zero = never emitted. Read on every `classify()` call to decide
    /// whether to emit a new summary.
    last_summary_at_ns: AtomicU64,
}

impl MlServiceStats {
    pub fn new() -> Self {
        Self {
            attempts: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
            total_latency_ns: AtomicU64::new(0),
            max_latency_ns: AtomicU64::new(0),
            min_latency_ns: AtomicU64::new(NO_LATENCY),
            tier_distribution: DashMap::new(),
            last_summary_at_ns: AtomicU64::new(0),
        }
    }

    /// Record the start of a `classify()` call. Returns the [`Instant`]
    /// to measure latency at the end.
    pub fn record_attempt(&self) -> Instant {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        Instant::now()
    }

    /// Record a successful HTTP classification. `latency` is the duration
    /// from [`record_attempt`](Self::record_attempt) to now.
    ///
    /// Updates `total_latency_ns`, `max_latency_ns` (CAS: only if larger),
    /// and `min_latency_ns` (CAS: only if smaller). Min/max are only
    /// updated on success — failure latency is unmeaningful (it reflects
    /// the failure mode, not the ML service's real response time).
    pub fn record_success(&self, tier: &str, latency: Duration) {
        let ns = latency.as_nanos() as u64;
        self.successes.fetch_add(1, Ordering::Relaxed);
        self.total_latency_ns.fetch_add(ns, Ordering::Relaxed);

        // Max: load then CAS until we win or see a larger value.
        let mut current_max = self.max_latency_ns.load(Ordering::Relaxed);
        loop {
            if ns <= current_max {
                break;
            }
            match self.max_latency_ns.compare_exchange(
                current_max,
                ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current_max = actual,
            }
        }

        // Min: same pattern, opposite direction.
        let mut current_min = self.min_latency_ns.load(Ordering::Relaxed);
        loop {
            if ns >= current_min {
                break;
            }
            match self.min_latency_ns.compare_exchange(
                current_min,
                ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current_min = actual,
            }
        }

        self.bump_tier(tier);
    }

    /// Record an HTTP failure (any reason). Does NOT bump `fallbacks` —
    /// caller does that explicitly via [`record_fallback`](Self::record_fallback)
    /// to keep the accounting orthogonal. Does NOT update min/max latency.
    pub fn record_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a fallback to `TierClassifier`. Bumps the tier counter
    /// with the final tier returned by the fallback, not the ML service's
    /// (possibly missing) response.
    pub fn record_fallback(&self, tier: &str) {
        self.fallbacks.fetch_add(1, Ordering::Relaxed);
        self.bump_tier(tier);
    }

    fn bump_tier(&self, tier: &str) {
        self.tier_distribution
            .entry(tier.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Called at the top of `classify()`. If [`SUMMARY_WINDOW`] has
    /// elapsed since the last emission, atomically claim the emission
    /// slot (CAS on `last_summary_at_ns`) and log a one-line summary.
    /// Returns whether a summary was emitted (mainly for testability).
    pub fn maybe_emit_summary(&self, ml_url: &str) -> bool {
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let last = self.last_summary_at_ns.load(Ordering::Relaxed);
        if last != 0 && now_ns.saturating_sub(last) < SUMMARY_WINDOW.as_nanos() as u64 {
            return false;
        }
        // CAS: only one caller wins the emission slot per window.
        match self.last_summary_at_ns.compare_exchange(
            last,
            now_ns,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                self.emit_summary(ml_url);
                true
            }
            Err(_) => false, // Another caller already claimed this window.
        }
    }

    fn emit_summary(&self, ml_url: &str) {
        let attempts = self.attempts.load(Ordering::Relaxed);
        let successes = self.successes.load(Ordering::Relaxed);
        let failures = self.failures.load(Ordering::Relaxed);
        let fallbacks = self.fallbacks.load(Ordering::Relaxed);
        let total_ns = self.total_latency_ns.load(Ordering::Relaxed);
        let max_ns = self.max_latency_ns.load(Ordering::Relaxed);
        let min_ns = self.min_latency_ns.load(Ordering::Relaxed);

        let avg_ms = if successes > 0 {
            (total_ns / successes / 1_000_000) as f64
        } else {
            0.0
        };
        let max_ms = if max_ns == 0 { 0.0 } else { (max_ns / 1_000_000) as f64 };
        let min_ms = if min_ns == NO_LATENCY {
            0.0
        } else {
            (min_ns / 1_000_000) as f64
        };
        let mut tiers: Vec<(String, u64)> = self
            .tier_distribution
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect();
        tiers.sort_by(|a, b| a.0.cmp(&b.0));
        let tier_str = tiers
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join(" ");

        tracing::info!(
            ml_url = %ml_url,
            window_secs = SUMMARY_WINDOW.as_secs(),
            attempts,
            successes,
            failures,
            fallbacks,
            avg_latency_ms = format!("{:.2}", avg_ms),
            min_latency_ms = format!("{:.2}", min_ms),
            max_latency_ms = format!("{:.2}", max_ms),
            tier_distribution = %tier_str,
            "ML classifier summary"
        );
    }
}

impl Default for MlServiceStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_record_correctly() {
        let stats = MlServiceStats::new();
        stats.record_attempt();
        stats.record_success("small", Duration::from_millis(10));
        stats.record_attempt();
        stats.record_failure();
        stats.record_fallback("small");
        assert_eq!(stats.attempts.load(Ordering::Relaxed), 2);
        assert_eq!(stats.successes.load(Ordering::Relaxed), 1);
        assert_eq!(stats.failures.load(Ordering::Relaxed), 1);
        assert_eq!(stats.fallbacks.load(Ordering::Relaxed), 1);
        assert_eq!(
            stats
                .tier_distribution
                .get("small")
                .unwrap()
                .value()
                .load(Ordering::Relaxed),
            2
        );
    }

    #[test]
    fn min_max_latency_track_only_successful_calls() {
        let stats = MlServiceStats::new();
        stats.record_success("small", Duration::from_millis(10));
        stats.record_success("large", Duration::from_millis(50));
        stats.record_success("small", Duration::from_millis(5));
        stats.record_failure();

        let max_ms = stats.max_latency_ns.load(Ordering::Relaxed) / 1_000_000;
        let min_ns = stats.min_latency_ns.load(Ordering::Relaxed);
        assert_eq!(max_ms, 50);
        assert_ne!(min_ns, NO_LATENCY);
        assert_eq!(min_ns / 1_000_000, 5);
    }

    #[test]
    fn min_latency_sentinel_when_no_successes() {
        let stats = MlServiceStats::new();
        stats.record_attempt();
        stats.record_failure();
        let min_ns = stats.min_latency_ns.load(Ordering::Relaxed);
        assert_eq!(min_ns, NO_LATENCY);
    }

    #[test]
    fn maybe_emit_summary_returns_false_within_window() {
        let stats = MlServiceStats::new();
        assert!(stats.maybe_emit_summary("http://test"));
        assert!(!stats.maybe_emit_summary("http://test"));
    }

    #[test]
    fn tier_distribution_alphabetical_sort() {
        let mut tiers = vec![
            ("small".to_string(), 100u64),
            ("large".to_string(), 50u64),
            ("medium".to_string(), 75u64),
        ];
        tiers.sort_by(|a, b| a.0.cmp(&b.0));
        let names: Vec<_> = tiers.into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["large", "medium", "small"]);
    }
}
