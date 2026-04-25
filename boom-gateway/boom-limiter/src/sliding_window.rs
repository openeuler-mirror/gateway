use boom_core::provider::RateLimiter;
use boom_core::types::{RateLimitDecision, RateLimitKey};
use boom_core::GatewayError;
use async_trait::async_trait;
use dashmap::DashMap;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

/// Read-only snapshot of a single window counter.
#[derive(Debug, Clone, Serialize)]
pub struct WindowUsage {
    pub cache_key: String,
    pub count: u64,
    pub window_secs: u64,
    pub elapsed_secs: u64,
}

/// In-memory sliding window rate limiter.
///
/// Uses DashMap for lock-free concurrent access.
/// Each (key, model, window) combination tracks its own counter.
///
/// Supports:
/// - RPM (requests per minute) — standard per-minute sliding window.
/// - Custom time windows — e.g. 100 requests per 5 hours (18000 seconds).
///
/// All timestamps are Unix epoch seconds for persistence compatibility.
pub struct SlidingWindowLimiter {
    /// Window counters: cache_key → WindowCounter.
    windows: Arc<DashMap<String, WindowCounter>>,
}

#[derive(Debug, Clone)]
struct WindowCounter {
    count: u64,
    /// Unix epoch seconds when this window started.
    window_start: u64,
    window_secs: u64,
}

/// Return current Unix epoch seconds.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time went backwards")
        .as_secs()
}

impl SlidingWindowLimiter {
    pub fn new() -> Self {
        Self {
            windows: Arc::new(DashMap::new()),
        }
    }

    /// Build the internal cache key from rate limit key and window duration.
    fn cache_key(key: &RateLimitKey, window_secs: u64) -> String {
        format!("{}:{}:{}", key.key_hash, key.model, window_secs)
    }

    /// Read-only check of a single window limit.
    /// Returns (allowed, current_count, limit, reset_at) WITHOUT incrementing.
    /// `weight` is the quota consumption multiplier to check against.
    fn peek_window(
        &self,
        cache_key: &str,
        limit: u64,
        window_secs: u64,
        weight: u64,
    ) -> (bool, u64, u64, chrono::DateTime<chrono::Utc>) {
        let now = now_epoch_secs();

        let (allowed, current_count) = match self.windows.get(cache_key) {
            Some(counter) => {
                let elapsed = now.saturating_sub(counter.window_start);
                if elapsed >= counter.window_secs {
                    // Window expired — would reset, so allowed with count 0.
                    (true, 0)
                } else {
                    // Check: current + weight <= limit
                    (counter.count + weight <= limit, counter.count)
                }
            }
            None => (true, 0),
        };

        let reset_at = match self.windows.get(cache_key) {
            Some(counter) => {
                let elapsed = now.saturating_sub(counter.window_start);
                let remaining = counter.window_secs.saturating_sub(elapsed);
                chrono::Utc::now() + chrono::Duration::seconds(remaining as i64)
            }
            None => chrono::Utc::now() + chrono::Duration::seconds(window_secs as i64),
        };

        (allowed, current_count, limit, reset_at)
    }

    /// Increment a single window counter by `weight`.
    /// If the window expired, it resets and starts fresh.
    fn record_window(&self, cache_key: &str, window_secs: u64, weight: u64) {
        let now = now_epoch_secs();
        self.windows
            .entry(cache_key.to_string())
            .and_modify(|c| {
                let elapsed = now_epoch_secs().saturating_sub(c.window_start);
                if elapsed >= c.window_secs {
                    c.count = weight;
                    c.window_start = now_epoch_secs();
                } else {
                    c.count += weight;
                }
            })
            .or_insert(WindowCounter {
                count: weight,
                window_start: now,
                window_secs,
            });
    }

    /// Decrement a single window counter by `weight` (undo a previous record).
    /// Used to rollback plan window counters when an upstream request fails
    /// after the rate limit check passed.
    fn unrecord_window(&self, cache_key: &str, weight: u64) {
        self.windows
            .entry(cache_key.to_string())
            .and_modify(|c| {
                c.count = c.count.saturating_sub(weight);
            });
    }

    /// Rollback plan window counters for a rate limit key.
    /// Call this when an upstream request fails after `check_and_record` already
    /// incremented the counters. RPM counters are NOT rolled back (DDoS protection).
    pub fn rollback_plan_windows(
        &self,
        key: &RateLimitKey,
        window_limits: &[(u64, u64)],
        weight: u64,
    ) {
        for &(_, window_secs) in window_limits {
            let win_key = Self::cache_key(key, window_secs);
            self.unrecord_window(&win_key, weight);
        }
    }

    // ── Read-only query methods (for dashboard) ────────────

    /// Read the current counter for a specific cache key.
    pub fn get_usage(&self, cache_key: &str) -> Option<WindowUsage> {
        let now = now_epoch_secs();
        self.windows.get(cache_key).map(|counter| {
            let elapsed = now.saturating_sub(counter.window_start);
            WindowUsage {
                cache_key: cache_key.to_string(),
                count: counter.count,
                window_secs: counter.window_secs,
                elapsed_secs: elapsed,
            }
        })
    }

    /// Read all window counters for a given key_hash.
    pub fn get_usage_for_key(&self, key_hash: &str) -> Vec<WindowUsage> {
        let now = now_epoch_secs();
        self.windows
            .iter()
            .filter(|entry| entry.key().starts_with(&format!("{}:", key_hash)))
            .map(|entry| {
                let elapsed = now.saturating_sub(entry.value().window_start);
                WindowUsage {
                    cache_key: entry.key().clone(),
                    count: entry.value().count,
                    window_secs: entry.value().window_secs,
                    elapsed_secs: elapsed,
                }
            })
            .collect()
    }

    /// Single-pass aggregation of usage for ALL keys.
    /// Returns `HashMap<key_hash, (total_count, max_remaining_secs)>`.
    /// Much cheaper than calling `get_usage_for_key` per key when you need all keys.
    pub fn get_all_key_usage(&self) -> HashMap<String, (u64, u64)> {
        let now = now_epoch_secs();
        let mut result: HashMap<String, (u64, u64)> = HashMap::new();
        for entry in self.windows.iter() {
            // cache_key format: "{key_hash}:{model}:{window_secs}"
            let key_hash = entry.key().split(':').next().unwrap_or("");
            let counter = entry.value();
            let remaining = counter.window_secs.saturating_sub(now.saturating_sub(counter.window_start));
            let slot = result.entry(key_hash.to_string()).or_insert((0, 0));
            slot.0 += counter.count;
            slot.1 = slot.1.max(remaining);
        }
        result
    }

    // ── Persistence methods ─────────────────────────────────

    /// Snapshot all non-expired entries for DB persistence.
    /// Returns Vec<(cache_key, count, window_start, window_secs)>.
    pub fn snapshot(&self) -> Vec<(String, u64, u64, u64)> {
        let now = now_epoch_secs();
        self.windows
            .iter()
            .filter(|entry| {
                let elapsed = now.saturating_sub(entry.value().window_start);
                elapsed < entry.value().window_secs
            })
            .map(|entry| {
                (
                    entry.key().clone(),
                    entry.value().count,
                    entry.value().window_start,
                    entry.value().window_secs,
                )
            })
            .collect()
    }

    /// Restore a counter entry from DB into memory.
    /// Called at startup to recover state after restart.
    pub fn restore_counter(&self, cache_key: String, count: u64, window_start: u64, window_secs: u64) {
        let now = now_epoch_secs();
        // Only restore if the window hasn't expired yet.
        if now.saturating_sub(window_start) < window_secs {
            self.windows.insert(
                cache_key,
                WindowCounter {
                    count,
                    window_start,
                    window_secs,
                },
            );
        }
    }

    /// Remove all expired entries from memory. Returns the count of removed entries.
    pub fn cleanup_expired(&self) -> usize {
        let now = now_epoch_secs();
        let before = self.windows.len();
        self.windows.retain(|_, counter| {
            let elapsed = now.saturating_sub(counter.window_start);
            elapsed < counter.window_secs
        });
        before - self.windows.len()
    }

    /// Clear specific window counters for a rate limit key.
    /// Used to reset stale counters when schedule switches.
    pub fn clear_windows(&self, key: &RateLimitKey, window_limits: &[(u64, u64)]) {
        for &(_, window_secs) in window_limits {
            let win_key = Self::cache_key(key, window_secs);
            self.windows.remove(&win_key);
        }
    }

    /// Clear all window counters for a specific key_hash.
    /// Returns the number of counters removed.
    pub fn clear_for_key(&self, key_hash: &str) -> usize {
        let prefix = format!("{}:", key_hash);
        let before = self.windows.len();
        self.windows.retain(|k, _| !k.starts_with(&prefix));
        before - self.windows.len()
    }

    /// Clear all window counters (reset everything).
    /// Returns the number of counters removed.
    pub fn clear_all(&self) -> usize {
        let count = self.windows.len();
        self.windows.clear();
        count
    }
}

impl Default for SlidingWindowLimiter {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════
// DB operations (boom_limiter owns boom_rate_limit_state table)
// ═══════════════════════════════════════════════════════════

impl SlidingWindowLimiter {
    /// Restore active rate limit counters from DB.
    pub async fn restore_counters_from_db(&self, pool: &sqlx::PgPool) {
        match sqlx::query_as::<_, (String, i64, i64, i64)>(
            r#"SELECT cache_key, count, window_start, window_secs
               FROM boom_rate_limit_state
               WHERE window_start + window_secs > EXTRACT(EPOCH FROM NOW())::BIGINT"#,
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                let count = rows.len();
                for (cache_key, count_val, window_start, window_secs) in rows {
                    self.restore_counter(cache_key, count_val as u64, window_start as u64, window_secs as u64);
                }
                tracing::info!("Restored {} rate limit counter(s) from DB", count);
            }
            Err(e) => {
                tracing::error!("Failed to restore rate limit state: {}", e);
            }
        }
    }

    /// Sync in-memory counters to DB (periodic background task).
    pub async fn sync_counters_to_db(&self, pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
        let entries = self.snapshot();
        for (cache_key, count, window_start, window_secs) in &entries {
            sqlx::query(
                r#"INSERT INTO boom_rate_limit_state (cache_key, count, window_start, window_secs, updated_at)
                   VALUES ($1, $2, $3, $4, NOW())
                   ON CONFLICT (cache_key) DO UPDATE
                   SET count = EXCLUDED.count,
                       window_start = EXCLUDED.window_start,
                       window_secs = EXCLUDED.window_secs,
                       updated_at = NOW()"#,
            )
            .bind(cache_key)
            .bind(*count as i64)
            .bind(*window_start as i64)
            .bind(*window_secs as i64)
            .execute(pool)
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl RateLimiter for SlidingWindowLimiter {
    async fn check_and_record(
        &self,
        key: &RateLimitKey,
        rpm_limit: Option<u64>,
        window_limits: &[(u64, u64)],
        weight: u64,
    ) -> Result<RateLimitDecision, GatewayError> {
        // Phase 1: Read-only check ALL windows. No counters incremented yet.
        // If any window rejects, we return immediately without recording anything.
        if let Some(rpm) = rpm_limit {
            let rpm_key = Self::cache_key(key, 60);
            let (allowed, _count, limit, reset_at) = self.peek_window(&rpm_key, rpm, 60, weight);

            if !allowed {
                let elapsed = self
                    .windows
                    .get(&rpm_key)
                    .map(|c| now_epoch_secs().saturating_sub(c.window_start))
                    .unwrap_or(0);
                let retry_after = 60u64.saturating_sub(elapsed);

                return Ok(RateLimitDecision {
                    allowed: false,
                    remaining: 0,
                    limit,
                    reset_at,
                    retry_after_secs: Some(retry_after),
                    rejected_window_secs: Some(60),
                });
            }
        }

        for &(limit, window_secs) in window_limits {
            let win_key = Self::cache_key(key, window_secs);
            let (allowed, _count, _, reset_at) = self.peek_window(&win_key, limit, window_secs, weight);

            if !allowed {
                let elapsed = self
                    .windows
                    .get(&win_key)
                    .map(|c| now_epoch_secs().saturating_sub(c.window_start))
                    .unwrap_or(0);
                let retry_after = window_secs.saturating_sub(elapsed);

                return Ok(RateLimitDecision {
                    allowed: false,
                    remaining: 0,
                    limit,
                    reset_at,
                    retry_after_secs: Some(retry_after),
                    rejected_window_secs: Some(window_secs),
                });
            }
        }

        // Phase 2: All checks passed — increment all counters atomically.
        if let Some(_) = rpm_limit {
            let rpm_key = Self::cache_key(key, 60);
            self.record_window(&rpm_key, 60, weight);
        }

        for &(_, window_secs) in window_limits {
            let win_key = Self::cache_key(key, window_secs);
            self.record_window(&win_key, window_secs, weight);
        }

        // Calculate remaining for response.
        let rpm_remaining = rpm_limit
            .map(|rpm| {
                let rpm_key = Self::cache_key(key, 60);
                self.windows
                    .get(&rpm_key)
                    .map(|c| rpm.saturating_sub(c.count))
                    .unwrap_or(rpm)
            })
            .unwrap_or(u64::MAX);

        Ok(RateLimitDecision {
            allowed: true,
            remaining: rpm_remaining,
            limit: rpm_limit.unwrap_or(0),
            reset_at: chrono::Utc::now() + chrono::Duration::seconds(60),
            retry_after_secs: None,
            rejected_window_secs: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rpm_limit() {
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "test_key".to_string(),
            model: "gpt-4".to_string(),
        };

        // Should allow up to 3 RPM.
        for _ in 0..3 {
            let decision = limiter.check_and_record(&key, Some(3), &[], 1).await.unwrap();
            assert!(decision.allowed);
        }

        // 4th request should be rejected.
        let decision = limiter.check_and_record(&key, Some(3), &[], 1).await.unwrap();
        assert!(!decision.allowed);
        assert!(decision.retry_after_secs.is_some());
    }

    #[tokio::test]
    async fn test_custom_window() {
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "test_key2".to_string(),
            model: "gpt-4".to_string(),
        };

        // Allow 2 requests per 5 hours (18000 seconds).
        let windows = vec![(2u64, 18000u64)];

        let decision = limiter
            .check_and_record(&key, None, &windows, 1)
            .await
            .unwrap();
        assert!(decision.allowed);

        let decision = limiter
            .check_and_record(&key, None, &windows, 1)
            .await
            .unwrap();
        assert!(decision.allowed);

        let decision = limiter
            .check_and_record(&key, None, &windows, 1)
            .await
            .unwrap();
        assert!(!decision.allowed);
    }

    #[tokio::test]
    async fn test_snapshot_and_restore() {
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "snap_key".to_string(),
            model: "gpt-4".to_string(),
        };

        // Record some requests.
        limiter.check_and_record(&key, Some(10), &[], 1).await.unwrap();
        limiter.check_and_record(&key, Some(10), &[], 1).await.unwrap();

        let snap = limiter.snapshot();
        assert!(!snap.is_empty());

        // Create new limiter and restore.
        let limiter2 = SlidingWindowLimiter::new();
        for (ck, count, ws, wsecs) in &snap {
            limiter2.restore_counter(ck.clone(), *count, *ws, *wsecs);
        }

        // Next request should see count=3 (restored 2 + 1 new).
        let usage = limiter2.get_usage_for_key("snap_key");
        assert!(!usage.is_empty());
        assert_eq!(usage[0].count, 2);
    }

    #[tokio::test]
    async fn test_rejected_request_does_not_consume_quota() {
        // Verify: if a custom window rejects, the RPM counter is NOT incremented.
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "dual_key".to_string(),
            model: "gpt-4".to_string(),
        };

        // RPM=10, custom window = 1 request per 18000 seconds.
        let windows = vec![(1u64, 18000u64)];

        // First request: both pass, both counters = 1.
        let d = limiter
            .check_and_record(&key, Some(10), &windows, 1)
            .await
            .unwrap();
        assert!(d.allowed);

        // Second request: custom window (1/18000s) should reject.
        // RPM has plenty of room (2/10), so only custom window blocks.
        let d = limiter
            .check_and_record(&key, Some(10), &windows, 1)
            .await
            .unwrap();
        assert!(!d.allowed);

        // Verify: RPM counter should still be 1 (NOT 2).
        let rpm_key = "dual_key:gpt-4:60";
        let rpm_count = limiter
            .windows
            .get(rpm_key)
            .map(|c| c.count)
            .unwrap_or(0);
        assert_eq!(
            rpm_count, 1,
            "RPM counter should be 1 — rejected request must NOT increment it"
        );

        // Verify: custom window counter should still be 1 (NOT 2).
        let win_key = "dual_key:gpt-4:18000";
        let win_count = limiter
            .windows
            .get(win_key)
            .map(|c| c.count)
            .unwrap_or(0);
        assert_eq!(
            win_count, 1,
            "Custom window counter should be 1 — rejected request must NOT increment it"
        );
    }

    #[tokio::test]
    async fn test_cleanup_expired() {
        let limiter = SlidingWindowLimiter::new();

        // Manually insert an already-expired counter.
        limiter.windows.insert(
            "expired_key:gpt-4:60".to_string(),
            WindowCounter {
                count: 5,
                window_start: now_epoch_secs() - 120, // 2 minutes ago
                window_secs: 60,
            },
        );

        // Insert a valid counter.
        let key = RateLimitKey {
            key_hash: "valid_key".to_string(),
            model: "gpt-4".to_string(),
        };
        limiter.check_and_record(&key, Some(10), &[], 1).await.unwrap();

        let removed = limiter.cleanup_expired();
        assert_eq!(removed, 1);
    }
}
