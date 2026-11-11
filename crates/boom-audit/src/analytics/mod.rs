//! Analytics detail pages (模型分析 / 消费分析 / 调度与缓存历史):
//! read-only aggregations over `boom_request_log` with the same lifecycle
//! as `daily_stats` — lazy TTL snapshots, no background tasks. A report is
//! loaded on the first dashboard request and re-aggregated at most once per
//! TTL window while someone is viewing the page; nobody viewing → zero DB
//! sync. Every refresh re-aggregates the full window (no watermark — the
//! audit writer batches INSERTs from multiple instances, so id-based
//! incremental reads would undercount).
//!
//! `TtlCache` generalizes `DailyStatsTracker`'s per-range slot to arbitrary
//! string keys: the fixed windows key on `range` alone; the model detail
//! keys on `"{model}|{range}"` (model names can be any literal, including
//! the real `"*"` fallback route). Concurrent callers for one key coalesce
//! on the per-slot tokio Mutex — late callers wait, then re-read the
//! winner's snapshot instead of running a duplicate query.

use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex,
    },
};

use arc_swap::ArcSwapOption;
use chrono::Utc;

mod consumption;
mod model;

pub use consumption::{
    query_consumption, query_key_consumption_detail, ConsumptionSeriesPoint, ConsumptionSnapshot,
    IpAgg, KeyAgg, KeyConsumptionDetail, KeySeriesPoint, TagAgg, TeamAgg,
};
pub use model::{query_model_detail, DeploymentAgg, ModelDetailSnapshot, ModelSeriesPoint};

/// Cache slots kept before stale-entry pruning kicks in.
const MAX_SLOTS: usize = 64;
/// A slot unused for this long is prunable (bounds the model-keyed cache:
/// stale models accumulate only while nobody revisits them).
const PRUNE_AFTER_SECS: i64 = 3600;

struct Slot<T> {
    snapshot: ArcSwapOption<T>,
    refresh_lock: tokio::sync::Mutex<()>,
    /// Epoch secs when the current snapshot was stored (TTL anchor).
    generated: AtomicI64,
    /// Epoch secs of the last cache hit or insert (prune anchor).
    last_used: AtomicI64,
}

/// Generic presence-driven snapshot cache. Reads are lock-free after a
/// brief map lookup (ArcSwap); refreshes for one key are serialized through
/// the slot's tokio Mutex so polling, manual force-refresh and concurrent
/// viewers never run duplicate queries.
pub struct TtlCache<T> {
    slots: Mutex<HashMap<String, Arc<Slot<T>>>>,
}

impl<T> Default for TtlCache<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TtlCache<T> {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// Latest cached snapshot without refreshing (None before the first
    /// successful refresh for this key).
    pub fn current(&self, key: &str) -> Option<Arc<T>> {
        let guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(key).and_then(|slot| slot.snapshot.load_full())
    }

    /// Snapshot for the key, re-aggregating when stale. A cache younger
    /// than `ttl_secs` is returned as-is; `force` bypasses the age check
    /// (manual refresh button); `query_fn` runs only when a refresh is
    /// actually needed, under the slot lock.
    pub async fn get_or_refresh<F, Fut>(
        &self,
        key: &str,
        ttl_secs: i64,
        force: bool,
        query_fn: F,
    ) -> Result<Arc<T>, sqlx::Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, sqlx::Error>>,
    {
        let now = Utc::now().timestamp();
        let slot = {
            let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(guard.entry(key.to_string()).or_insert_with(|| {
                Arc::new(Slot {
                    snapshot: ArcSwapOption::new(None),
                    refresh_lock: tokio::sync::Mutex::new(()),
                    generated: AtomicI64::new(0),
                    last_used: AtomicI64::new(now),
                })
            }))
        };
        let _guard = slot.refresh_lock.lock().await;
        // Re-read the clock after the lock: a caller that waited for a
        // peer's in-flight refresh must judge freshness by the snapshot
        // the peer just stored, not by its own (stale) arrival time.
        let now = Utc::now().timestamp();
        if !force {
            if let Some(existing) = slot.snapshot.load_full() {
                let age = now - slot.generated.load(Ordering::Relaxed);
                if age >= 0 && age < ttl_secs {
                    slot.last_used.store(now, Ordering::Relaxed);
                    return Ok(existing);
                }
            }
        }
        let snap = Arc::new(query_fn().await?);
        let stored_at = Utc::now().timestamp();
        slot.snapshot.store(Some(Arc::clone(&snap)));
        slot.generated.store(stored_at, Ordering::Relaxed);
        slot.last_used.store(stored_at, Ordering::Relaxed);
        self.prune(stored_at);
        Ok(snap)
    }

    /// Drop entries unused for over `PRUNE_AFTER_SECS`; if still over
    /// `MAX_SLOTS`, trim oldest-first. Only called after a successful
    /// insert (a fresh page visit), never on the hot read path — the
    /// just-inserted slot is the newest and never evicted here.
    fn prune(&self, now: i64) {
        let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() <= MAX_SLOTS {
            return;
        }
        guard.retain(|_, slot| {
            now - slot.last_used.load(Ordering::Relaxed) < PRUNE_AFTER_SECS
        });
        if guard.len() > MAX_SLOTS {
            let mut by_age: Vec<(String, i64)> = guard
                .iter()
                .map(|(k, slot)| (k.clone(), slot.last_used.load(Ordering::Relaxed)))
                .collect();
            by_age.sort_by_key(|&(_, used)| used);
            let excess = guard.len() - MAX_SLOTS;
            for (key, _) in by_age.into_iter().take(excess) {
                guard.remove(&key);
            }
        }
    }
}

/// Snapshot caches for the analytics detail pages. Owned by the dashboard
/// state (built once per process, no boom-main wiring: unlike
/// `daily_stats`, nothing outside the dashboard ever touches these).
pub struct AnalyticsHub {
    /// Model detail snapshots, keyed `"{model}|{range}"`.
    pub model: TtlCache<ModelDetailSnapshot>,
    /// Consumption snapshots, keyed `"{range}"`.
    pub consumption: TtlCache<ConsumptionSnapshot>,
}

impl Default for AnalyticsHub {
    fn default() -> Self {
        Self::new()
    }
}

impl AnalyticsHub {
    pub fn new() -> Self {
        Self {
            model: TtlCache::new(),
            consumption: TtlCache::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn refresh(
        cache: &TtlCache<i64>,
        key: &str,
        ttl: i64,
        force: bool,
        value: i64,
        calls: &std::sync::atomic::AtomicUsize,
    ) -> Result<Arc<i64>, sqlx::Error> {
        cache
            .get_or_refresh(key, ttl, force, move || {
                calls.fetch_add(1, Ordering::Relaxed);
                std::future::ready(Ok(value))
            })
            .await
    }

    #[tokio::test]
    async fn ttl_hit_skip_force_bypass() {
        let cache: TtlCache<i64> = TtlCache::new();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let s = refresh(&cache, "k", 300, false, 7, &calls).await.unwrap();
        assert_eq!(*s, 7);
        let s2 = refresh(&cache, "k", 300, false, 9, &calls).await.unwrap();
        assert_eq!(*s2, 7, "TTL hit must serve cached value");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let s3 = refresh(&cache, "k", 300, true, 11, &calls).await.unwrap();
        assert_eq!(*s3, 11, "force must re-query");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn ttl_zero_always_queries() {
        let cache: TtlCache<i64> = TtlCache::new();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        refresh(&cache, "k", 0, false, 1, &calls).await.unwrap();
        refresh(&cache, "k", 0, false, 2, &calls).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn distinct_keys_refresh_independently() {
        let cache: TtlCache<i64> = TtlCache::new();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let a = refresh(&cache, "gpt-4o|today", 300, false, 1, &calls).await.unwrap();
        let b = refresh(&cache, "gpt-4o|1d", 300, false, 2, &calls).await.unwrap();
        let c = refresh(&cache, "claude|today", 300, false, 3, &calls).await.unwrap();
        assert_eq!((*a, *b, *c), (1, 2, 3));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn concurrent_callers_coalesce_on_one_query() {
        let cache = Arc::new(TtlCache::<i64>::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                cache
                    .get_or_refresh("k", 300, false, move || {
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::Relaxed);
                            // Slow query: overlap the callers.
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            Ok(42)
                        }
                    })
                    .await
                    .unwrap()
            }));
        }
        for h in handles {
            let r = h.await.unwrap();
            assert_eq!(*r, 42);
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1, "coalesced into one query");
    }

    #[tokio::test]
    async fn prune_drops_stale_entries_only() {
        let cache: TtlCache<i64> = TtlCache::new();
        let zero = std::sync::atomic::AtomicUsize::new(0);
        // Fill to exactly MAX_SLOTS (prune no-ops at len <= MAX_SLOTS),
        // then backdate the even-numbered keys beyond PRUNE_AFTER_SECS
        // (deterministic by key name — HashMap iteration order is
        // arbitrary, key parsing is not).
        for i in 0..MAX_SLOTS as i64 {
            refresh(&cache, &format!("m{i}|today"), 300, false, i, &zero)
                .await
                .unwrap();
        }
        let now = Utc::now().timestamp();
        {
            let guard = cache.slots.lock().unwrap_or_else(|e| e.into_inner());
            for (key, slot) in guard.iter() {
                let idx: i64 = key[1..].split('|').next().unwrap().parse().unwrap();
                if idx % 2 == 0 {
                    slot.last_used
                        .store(now - PRUNE_AFTER_SECS - 10, Ordering::Relaxed);
                }
            }
        }
        // This insert triggers prune: backdated entries are dropped,
        // fresh ones and the just-inserted slot survive.
        let survivor = refresh(&cache, "fresh|today", 300, false, 99, &zero)
            .await
            .unwrap();
        assert_eq!(*survivor, 99);
        for i in 0..MAX_SLOTS as i64 {
            let gone = cache.current(&format!("m{i}|today")).is_none();
            if i % 2 == 0 {
                assert!(gone, "m{i} backdated and pruned");
            } else {
                assert!(!gone, "m{i} fresh and retained");
            }
        }
        assert!(cache.current("fresh|today").is_some());
        let len = {
            let guard = cache.slots.lock().unwrap_or_else(|e| e.into_inner());
            guard.len()
        };
        assert_eq!(len, MAX_SLOTS / 2 + 1);
    }
}
