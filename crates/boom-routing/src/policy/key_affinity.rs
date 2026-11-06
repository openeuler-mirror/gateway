use boom_core::provider::{DeploymentQueueInfo, Provider};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::inflight::InFlightTracker;
use crate::rebalance::RebalanceMoveTracker;
use super::load_helpers::{deployment_load, min_load_candidate, select_lowest_load, should_rebalance};
use super::SchedulePolicy;

/// One recorded affinity binding. `last_seen` is refreshed on every hit, so
/// the TTL measures idleness, not age since first binding.
#[derive(Debug, Clone)]
struct AffinityEntry {
    deployment_id: String,
    last_seen: Instant,
}

/// Key-affinity scheduling: route requests from the same API key to the same
/// provider deployment for a given model, with load-aware rebalancing.
///
/// Affinity key: `{key_hash}:{model}` → `deployment_id`
///
/// Two time-scale signals cooperate:
/// - New-key assignment uses the long-horizon signal — per-deployment
///   bindings normalized by capacity (`bindings / max_inflight`). Instances
///   with different capacities fill up in proportion (32/64/128 → 1:2:4), so
///   they reach "full" roughly together. This is the dominant signal at low
///   traffic, where instantaneous load is always zero and lowest-load
///   selection would collapse onto one instance.
/// - Rebalancing of existing bindings uses the short-horizon signal —
///   instantaneous utilization (in-flight + queued). It only matters under
///   real concurrency, exactly where the binding ratio's uniform-traffic
///   assumption breaks down.
///
/// Bindings carry an idle TTL: a binding that outlives the upstream prefix
/// cache has no reuse upside, only load rigidity, so idle keys are swept and
/// re-spread on their next request.
///
/// Short-context requests (`input_chars < context_threshold`) bypass affinity
/// entirely and ride the instantaneous lowest-load pick: re-prefilling a
/// short input is cheap, so there is no cache value to protect and load
/// balance is the better use of the routing freedom. They write no binding
/// (one-shot shorts must not pin keys) and only refresh an existing
/// binding's idle clock.
///
/// Logic per request:
///   1. Single candidate → direct return
///   2. No key_hash provided → fall back to lowest-load
///   3. Short context (below context_threshold) → lowest-load, no binding
///      written, existing binding only refreshed
///   4. Affinity lookup (refreshes idleness) → validate deployment still
///      exists → rebalance check
///   5. New key or stale binding → lowest bindings/capacity ratio → record
pub struct KeyAffinityPolicy {
    /// Reference to the in-flight tracker for load queries.
    tracker: Arc<InFlightTracker>,
    /// Optional flow control queue info for total load (in-flight + queued).
    queue_info: Option<Arc<dyn DeploymentQueueInfo>>,
    /// Affinity map: `{key_hash}:{model}` → binding
    affinity: DashMap<String, AffinityEntry>,
    /// deployment_id → number of keys currently bound. Maintained on every
    /// write path (assignment, rebalance move, TTL sweep) so new-key
    /// assignment stays O(candidates) instead of scanning the affinity map.
    bindings: DashMap<String, AtomicU64>,
    /// Short-context bypass threshold (input chars). Requests below it ride
    /// the lowest-load pick. 0 = always use affinity.
    context_threshold: u64,
    /// Rebalance threshold: if the preferred provider's utilization exceeds
    /// the minimum by more than this percentage (1..=100), reassign to the
    /// least loaded provider.
    rebalance_threshold: u8,
    /// Idle TTL for bindings. Zero = bindings never expire.
    ttl: Duration,
    /// Optional tracker for per-deployment rebalance move counts (in/out).
    rebalance_move_tracker: Option<Arc<RebalanceMoveTracker>>,
}

impl KeyAffinityPolicy {
    pub fn new(
        tracker: Arc<InFlightTracker>,
        context_threshold: u64,
        rebalance_threshold: u8,
        rebalance_move_tracker: Option<Arc<RebalanceMoveTracker>>,
        ttl: Duration,
    ) -> Self {
        Self {
            tracker,
            queue_info: None,
            affinity: DashMap::new(),
            bindings: DashMap::new(),
            context_threshold,
            rebalance_threshold,
            ttl,
            rebalance_move_tracker,
        }
    }

    /// Inject flow control queue info for load and capacity queries.
    pub fn set_queue_info(&mut self, info: Arc<dyn DeploymentQueueInfo>) {
        self.queue_info = Some(info);
    }

    /// Create, move, or refresh a binding, keeping per-deployment counters
    /// balanced. The check-and-mutate runs under the affinity shard lock, so
    /// concurrent selects for the same key cannot double-count.
    fn rebind(&self, affinity_key: &str, deployment_id: &str) {
        match self.affinity.entry(affinity_key.to_string()) {
            Entry::Vacant(v) => {
                v.insert(AffinityEntry {
                    deployment_id: deployment_id.to_string(),
                    last_seen: Instant::now(),
                });
                self.increment(deployment_id);
            }
            Entry::Occupied(mut o) => {
                let entry = o.get_mut();
                if entry.deployment_id != deployment_id {
                    let old = std::mem::replace(
                        &mut entry.deployment_id,
                        deployment_id.to_string(),
                    );
                    entry.last_seen = Instant::now();
                    drop(o);
                    self.decrement(&old);
                    self.increment(deployment_id);
                } else {
                    entry.last_seen = Instant::now();
                }
            }
        }
    }

    fn increment(&self, deployment_id: &str) {
        self.bindings
            .entry(deployment_id.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    fn decrement(&self, deployment_id: &str) {
        if let Some(c) = self.bindings.get(deployment_id) {
            c.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Pick the candidate with the lowest bindings/capacity ratio — the
    /// long-horizon spreading signal for new keys. Ties go to the first
    /// candidate (same tie-break as `min_load_candidate`); after the first
    /// assignment the ratios differentiate, so the magnet effect of an
    /// all-zero tie lasts exactly one request per restart.
    ///
    /// Capacity semantics mirror `deployment_load`: max_inflight == 0 means
    /// unlimited, falls back to the absolute binding count so an unlimited
    /// instance still accumulates and eventually yields to others instead of
    /// absorbing every new key forever.
    fn select_min_binding_ratio(&self, candidates: &[Arc<dyn Provider>]) -> Arc<dyn Provider> {
        let mut best = candidates[0].clone();
        let mut best_ratio = u64::MAX;
        for candidate in candidates {
            let Some(id) = candidate.deployment_id() else { continue };
            let count = self
                .bindings
                .get(id)
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(0);
            let capacity = self
                .queue_info
                .as_ref()
                .map(|q| q.max_capacity(id))
                .unwrap_or(0);
            let ratio = if capacity > 0 {
                count * 100 / capacity as u64
            } else {
                count
            };
            if ratio < best_ratio {
                best_ratio = ratio;
                best = candidate.clone();
            }
        }
        best
    }
}

impl SchedulePolicy for KeyAffinityPolicy {
    fn select(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        input_chars: u64,
    ) -> Option<Arc<dyn Provider>> {
        if candidates.is_empty() {
            return None;
        }
        if candidates.len() == 1 {
            return Some(candidates[0].clone());
        }

        // No key context → fall back to lowest-load.
        let key_hash = match key_hash {
            Some(k) => k,
            None => return select_lowest_load(&self.tracker, &self.queue_info, model, candidates),
        };

        let affinity_key = format!("{}:{}", key_hash, model);

        // Short-context bypass: re-prefill of a small input is cheap, so
        // affinity has no value — route by instantaneous load instead. No
        // binding is written: under serial traffic lowest-load ties collapse
        // onto one instance, and recording that would pin one-shot keys there
        // (the magnet the binding-ratio assignment exists to prevent). An
        // existing binding only has its idle clock refreshed, so the TTL
        // tracks key activity, not long-request activity.
        if self.context_threshold > 0 && input_chars < self.context_threshold {
            if let Some(mut e) = self.affinity.get_mut(&affinity_key) {
                e.last_seen = Instant::now();
            }
            return select_lowest_load(&self.tracker, &self.queue_info, model, candidates);
        }

        // Look up existing affinity, refreshing idleness on hit. Clone the
        // value immediately to release the DashMap guard before any
        // subsequent write — holding a Ref while writing the same shard
        // self-deadlocks.
        let preferred_id = self.affinity.get_mut(&affinity_key).map(|mut e| {
            e.last_seen = Instant::now();
            e.deployment_id.clone()
        });

        if let Some(ref preferred_id) = preferred_id {
            // Find the preferred provider in candidates.
            if let Some(provider) = candidates.iter().find(|c| {
                c.deployment_id()
                    .map(|id| id == preferred_id.as_str())
                    .unwrap_or(false)
            }) {
                // Rebalance check: if the preferred provider is significantly
                // more loaded than the least-loaded candidate, reassign.
                let load_preferred = deployment_load(&self.tracker, &self.queue_info, model, provider.as_ref());
                let (min_load, least_loaded) = min_load_candidate(&self.tracker, &self.queue_info, model, candidates);

                if should_rebalance(load_preferred, min_load, self.rebalance_threshold as u64) {
                    // Rebalance to least loaded.
                    if let Some(ref tracker) = self.rebalance_move_tracker {
                        tracker.record_move(
                            provider.deployment_id(),
                            least_loaded.deployment_id(),
                        );
                    }
                    let from_id = preferred_id.clone();
                    if let Some(did) = least_loaded.deployment_id() {
                        self.rebind(&affinity_key, did);
                    }
                    tracing::info!(
                        model,
                        key = %short_key(key_hash),
                        from = %from_id,
                        to = ?least_loaded.deployment_id(),
                        load_preferred_pct = load_preferred,
                        min_load_pct = min_load,
                        threshold = self.rebalance_threshold,
                        "key_affinity rebalance: migrated key (load gap exceeds threshold)"
                    );
                    return Some(least_loaded);
                }

                return Some(provider.clone());
            }
            // Preferred deployment no longer in candidates — fall through.
        }

        // New key or stale binding: spread by lowest bindings/capacity ratio.
        let provider = self.select_min_binding_ratio(candidates);
        if let Some(did) = provider.deployment_id() {
            self.rebind(&affinity_key, did);
            tracing::info!(
                model,
                key = %short_key(key_hash),
                assigned_to = %did,
                "key_affinity: initial assignment (new key or stale binding)"
            );
        }
        Some(provider)
    }

    fn sweep_expired(&self, now: Instant) -> usize {
        if self.ttl.is_zero() {
            return 0;
        }
        let mut removed = 0usize;
        self.affinity.retain(|_, entry| {
            if now.saturating_duration_since(entry.last_seen) > self.ttl {
                self.decrement(&entry.deployment_id);
                removed += 1;
                false
            } else {
                true
            }
        });
        removed
    }

    fn affinity_binding_counts(&self) -> Vec<(String, u64)> {
        let mut counts: Vec<(String, u64)> = self
            .bindings
            .iter()
            .map(|r| (r.key().clone(), r.value().load(Ordering::Relaxed)))
            .filter(|(_, c)| *c > 0)
            .collect();
        counts.sort();
        counts
    }

    fn name(&self) -> &str {
        "key_affinity"
    }
}

/// First 8 chars of the key hash — enough to correlate logs for the same user
/// without logging the full key hash (privacy / log noise).
fn short_key(key_hash: &str) -> String {
    key_hash.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inflight::InFlightGuard;
    use async_trait::async_trait;
    use boom_core::types::{ChatCompletionRequest, ChatCompletionResponse};
    use boom_core::GatewayError;
    use std::collections::HashMap;

    struct FakeProvider {
        dep: &'static str,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn chat(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<ChatCompletionResponse, GatewayError> {
            unimplemented!("policy tests only inspect identity")
        }
        async fn chat_stream(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<boom_core::types::ChatStream, GatewayError> {
            unimplemented!()
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn models(&self) -> &[String] {
            &[]
        }
        fn deployment_id(&self) -> Option<&str> {
            Some(self.dep)
        }
        fn kv_worker_id(&self) -> Option<&str> {
            None
        }
    }

    struct FakeQueue {
        caps: HashMap<String, u32>,
    }

    impl DeploymentQueueInfo for FakeQueue {
        fn total_load(&self, _deployment_id: &str) -> u64 {
            0
        }
        fn max_capacity(&self, deployment_id: &str) -> u32 {
            self.caps.get(deployment_id).copied().unwrap_or(0)
        }
    }

    fn make_policy(
        caps: &[(&'static str, u32)],
        ctx: u64,
        ttl: Duration,
    ) -> (KeyAffinityPolicy, Vec<Arc<dyn Provider>>) {
        let mut policy = KeyAffinityPolicy::new(
            Arc::new(InFlightTracker::new()),
            ctx,
            20,
            Some(Arc::new(RebalanceMoveTracker::new())),
            ttl,
        );
        let caps: HashMap<String, u32> = caps
            .iter()
            .map(|(id, c)| (id.to_string(), *c))
            .collect();
        policy.set_queue_info(Arc::new(FakeQueue { caps }));
        let candidates: Vec<Arc<dyn Provider>> = vec![
            Arc::new(FakeProvider { dep: "d1" }),
            Arc::new(FakeProvider { dep: "d2" }),
            Arc::new(FakeProvider { dep: "d3" }),
        ];
        (policy, candidates)
    }

    fn counts(p: &KeyAffinityPolicy) -> Vec<(String, u64)> {
        p.affinity_binding_counts()
    }

    #[test]
    fn new_keys_spread_by_capacity_ratio() {
        // 32/64/128 capacities: bindings converge to 1:2:4 so all three
        // instances hit their configured ceiling roughly together.
        let (p, candidates) = make_policy(
            &[("d1", 32), ("d2", 64), ("d3", 128)],
            0,
            Duration::ZERO,
        );
        for i in 0..28 {
            let key = format!("key{}", i);
            p.select("m", &candidates, Some(&key), 0);
        }
        assert_eq!(
            counts(&p),
            vec![
                ("d1".to_string(), 4),
                ("d2".to_string(), 8),
                ("d3".to_string(), 16),
            ]
        );
    }

    #[test]
    fn same_key_sticks_to_binding() {
        let (p, candidates) = make_policy(&[("d1", 32), ("d2", 64), ("d3", 128)], 0, Duration::ZERO);
        let mut picks = std::collections::HashSet::new();
        for _ in 0..5 {
            let provider = p.select("m", &candidates, Some("k"), 0);
            picks.insert(provider.unwrap().deployment_id().unwrap().to_string());
        }
        assert_eq!(picks.len(), 1, "same key must stay on one deployment");
        assert_eq!(
            counts(&p),
            vec![("d1".to_string(), 1)]
        );
    }

    #[test]
    fn unlimited_capacity_falls_back_to_absolute_count() {
        // No FC config anywhere (capacity 0): ratio = absolute count, so
        // assignments still rotate instead of magnetizing onto d1.
        let (p, candidates) = make_policy(&[], 0, Duration::ZERO);
        for i in 0..6 {
            p.select("m", &candidates, Some(&format!("key{}", i)), 0);
        }
        let c = counts(&p);
        assert!(c.iter().all(|(_, n)| *n == 2), "expected 2/2/2, got {:?}", c);
    }

    #[test]
    fn ttl_sweep_removes_idle_and_rebalances_counters() {
        let (p, candidates) = make_policy(
            &[("d1", 32), ("d2", 64), ("d3", 128)],
            0,
            Duration::from_secs(60),
        );
        for i in 0..3 {
            p.select("m", &candidates, Some(&format!("key{}", i)), 0);
        }
        // Sweep from the future: every binding is idle relative to `now`.
        let removed = p.sweep_expired(Instant::now() + Duration::from_secs(120));
        assert_eq!(removed, 3);
        assert!(counts(&p).is_empty(), "counters must return to zero");
        assert_eq!(p.sweep_expired(Instant::now() + Duration::from_secs(120)), 0);
    }

    #[test]
    fn ttl_sweep_keeps_recently_refreshed_binding() {
        let (p, candidates) = make_policy(
            &[("d1", 32), ("d2", 64), ("d3", 128)],
            0,
            Duration::from_secs(60),
        );
        for i in 0..3 {
            p.select("m", &candidates, Some(&format!("key{}", i)), 0);
        }
        // Push key0's last_seen into the future: relative to the sweep time
        // below it is "recently active" and must survive.
        {
            let mut e = p.affinity.get_mut("key0:m").unwrap();
            e.last_seen = Instant::now() + Duration::from_secs(30);
        }
        let removed = p.sweep_expired(Instant::now() + Duration::from_secs(61));
        assert_eq!(removed, 2);
        assert_eq!(counts(&p).len(), 1);
    }

    #[test]
    fn ttl_zero_disables_sweep() {
        let (p, candidates) = make_policy(&[("d1", 32)], 0, Duration::ZERO);
        p.select("m", &candidates, Some("k"), 0);
        assert_eq!(
            p.sweep_expired(Instant::now() + Duration::from_secs(10_000)),
            0
        );
        assert_eq!(counts(&p), vec![("d1".to_string(), 1)]);
    }

    #[test]
    fn rebind_moves_counter_atomically() {
        let (p, _candidates) = make_policy(&[("d1", 32), ("d2", 64)], 0, Duration::ZERO);
        p.rebind("k:m", "d1");
        p.rebind("k:m", "d1"); // same-id refresh must not double-count
        assert_eq!(counts(&p), vec![("d1".to_string(), 1)]);
        p.rebind("k:m", "d2");
        assert_eq!(counts(&p), vec![("d2".to_string(), 1)]); // d1 drops to 0 and is filtered
    }

    #[test]
    fn short_context_bypasses_affinity_and_writes_no_binding() {
        // d1 carries one in-flight request so lowest-load prefers d2 —
        // proving the short request rides load, not the binding-ratio path.
        let (p, candidates) =
            make_policy(&[("d1", 32), ("d2", 64), ("d3", 128)], 1000, Duration::ZERO);
        let _g = InFlightGuard::new_for_deployment(p.tracker.clone(), "m", "d1", 10);
        let provider = p.select("m", &candidates, Some("k"), 10).unwrap();
        assert_eq!(provider.deployment_id(), Some("d2"));
        assert!(
            p.affinity.get("k:m").is_none(),
            "short request must not create a binding"
        );
        assert!(counts(&p).is_empty());
    }

    #[test]
    fn short_context_refreshes_but_keeps_existing_binding() {
        let (p, candidates) =
            make_policy(&[("d1", 32), ("d2", 64), ("d3", 128)], 1000, Duration::ZERO);
        let first = p.select("m", &candidates, Some("k"), 5000).unwrap(); // long → binds d1
        let bound = first.deployment_id().unwrap().to_string();
        // Load the bound instance: the short request must follow lowest-load
        // (d2), NOT the binding, and must not move or duplicate the binding.
        let _g = InFlightGuard::new_for_deployment(p.tracker.clone(), "m", &bound, 10);
        let provider = p.select("m", &candidates, Some("k"), 10).unwrap();
        assert_ne!(provider.deployment_id(), Some(bound.as_str()));
        assert_eq!(
            p.affinity.get("k:m").map(|e| e.deployment_id.clone()),
            Some(bound.clone())
        );
        assert_eq!(counts(&p), vec![(bound, 1)]);
    }
}
