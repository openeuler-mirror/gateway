use boom_core::kv_event::KvIndexBackend;
use boom_core::provider::{DeploymentQueueInfo, Provider};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::inflight::InFlightTracker;
use crate::rebalance::RebalanceMoveTracker;
use super::load_helpers::{deployment_load, should_rebalance};
use super::{SchedulePolicy, Selection};

// ── Global scheduling counters (debug page) ───────────────────────────────

/// Process-lifetime counters for kvc_aware scheduling decisions.
///
/// Held at AppState top level (survives config reloads — the policy itself is
/// recreated on every reload, so per-policy fields would reset). The policy
/// holds a clone of the Arc; `None` in unit tests.
///
/// Counted at every `select_with_context` exit that produces a real routing
/// decision, so `total == affinity + cold + lowest_load + rebalance` always
/// holds. Exits that make no decision are NOT counted: the single-candidate
/// short-circuit and `select()` (no prefix context).
#[derive(Default)]
pub struct KvcSchedulerStats {
    total: AtomicU64,
    /// Final pick by prefix affinity score (hit > 0, no rebalance handoff).
    affinity: AtomicU64,
    /// All candidates cold (hit == 0) — spread by round-robin tie-break.
    cold: AtomicU64,
    /// Degraded: empty prefix or candidates without kv_worker_id →
    /// lowest-load pick.
    lowest_load: AtomicU64,
    /// Winner handed off to the least-loaded candidate (rebalance_threshold
    /// exceeded).
    rebalance: AtomicU64,
}

/// Serializable point-in-time snapshot. Field names are the dashboard API
/// contract (`GET /admin/stats/kvc-scheduler`).
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct KvcSchedulerStatsSnapshot {
    pub total: u64,
    pub affinity: u64,
    pub cold: u64,
    pub lowest_load: u64,
    pub rebalance: u64,
}

impl KvcSchedulerStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_affinity(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
        self.affinity.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cold(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
        self.cold.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_lowest_load(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
        self.lowest_load.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_rebalance(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
        self.rebalance.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> KvcSchedulerStatsSnapshot {
        KvcSchedulerStatsSnapshot {
            total: self.total.load(Ordering::Relaxed),
            affinity: self.affinity.load(Ordering::Relaxed),
            cold: self.cold.load(Ordering::Relaxed),
            lowest_load: self.lowest_load.load(Ordering::Relaxed),
            rebalance: self.rebalance.load(Ordering::Relaxed),
        }
    }
}

/// KV-cache aware scheduling policy (self-contained, no vLLM event dependency).
///
/// Scores every candidate on ONE axis — prefix affinity:
///   score = hit_ratio
/// Higher is better. Ties on the top score (including the all-cold case,
/// where every candidate scores 0) are broken by round-robin, so cold
/// traffic spreads evenly without a load term. Load balancing is delegated
/// to the router-level rebalance_threshold: when the affinity winner is
/// markedly more loaded than the least-loaded candidate, the request is
/// handed off (same policy as key_affinity). There is NO overload gate —
/// a warm worker keeps winning regardless of load, and only the rebalance
/// threshold can move traffic away.
///
/// The trie is self-learned: the orchestrator records each routed request's
/// prefix under the chosen worker, so a miss (hit=0) just round-robins —
/// no key_affinity fallback. The orchestrator then records the new worker
/// (add-only, no migration of old entries — aligned with dynamo).
pub struct KvcAwarePolicy {
    /// Reference to the KV-cache prefix index.
    kv_index: Arc<dyn KvIndexBackend>,
    /// In-flight tracker for load-based fallback.
    tracker: Arc<InFlightTracker>,
    /// Optional flow control queue info for total load (in-flight + queued).
    queue_info: Option<Arc<dyn DeploymentQueueInfo>>,
    /// Load-balance threshold: when the winner's load_pct exceeds the lowest
    /// candidate's load_pct by more than this, hand off to the least-loaded
    /// candidate. 100 = disabled.
    rebalance_threshold: u64,
    /// Round-robin counter for tie-breaking among equal top scores (LB).
    tie_counter: AtomicU64,
    /// Optional tracker for rebalance-move counters (Debug page "Rebalance Moves").
    /// Records (from, to) deployment ids on each capacity rebalance handoff.
    rebalance_move_tracker: Option<Arc<RebalanceMoveTracker>>,
    /// Global scheduling counters (Debug page "KVC Scheduler Stats").
    /// AppState-owned Arc — survives reloads, unlike the policy itself.
    stats: Option<Arc<KvcSchedulerStats>>,
}

impl KvcAwarePolicy {
    pub fn new(
        kv_index: Arc<dyn KvIndexBackend>,
        tracker: Arc<InFlightTracker>,
        rebalance_move_tracker: Option<Arc<RebalanceMoveTracker>>,
        stats: Option<Arc<KvcSchedulerStats>>,
    ) -> Self {
        Self {
            kv_index,
            tracker,
            queue_info: None,
            rebalance_threshold: 100,
            tie_counter: AtomicU64::new(0),
            rebalance_move_tracker,
            stats,
        }
    }

    /// Configure the load-balance hand-off threshold (router-level setting
    /// shared with key_affinity; 100 = disabled).
    pub fn set_rebalance_threshold(&mut self, rebalance_threshold: u64) {
        self.rebalance_threshold = rebalance_threshold;
    }

    /// Inject flow control queue info for total load queries.
    pub fn set_queue_info(&mut self, info: Arc<dyn DeploymentQueueInfo>) {
        self.queue_info = Some(info);
    }

    /// Workers to query for a candidate. No KV-sharing groups (PD topology
    /// removed) — each candidate is queried alone.
    fn query_workers_for(&self, worker_id: &str) -> Vec<String> {
        vec![worker_id.to_string()]
    }
}

impl SchedulePolicy for KvcAwarePolicy {
    fn select(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        _key_hash: Option<&str>,
        _input_chars: u64,
    ) -> Option<Arc<dyn Provider>> {
        // Without prefix context, fall back to lowest-load.
        select_lowest_load(&self.tracker, &self.queue_info, model, candidates)
    }

    fn select_with_context(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        _key_hash: Option<&str>,
        _input_chars: u64,
        prefix_bytes: &[u8],
    ) -> Option<Selection> {
        if candidates.is_empty() {
            return None;
        }

        // Single candidate — no routing decision to make; skip the KV lookup.
        if candidates.len() == 1 {
            tracing::trace!(model, "single candidate, skip KVC lookup");
            return Some(Selection {
                provider: candidates[0].clone(),
                kv_hit_ratio: 0.0,
                kv_hit_blocks: 0,
                kv_input_blocks: 0,
                kv_match_attempted: false,
                degraded: false,
            });
        }

        // Empty prefix (e.g. empty request) — nothing to hash/match. Route by
        // lowest load. No key_affinity fallback: self-contained mode relies on
        // the trie being learned from prior requests; an empty prefix can't
        // learn or match, so load-balanced pick is the right call.
        if prefix_bytes.is_empty() {
            if let Some(ref stats) = self.stats {
                stats.record_lowest_load();
            }
            let provider = select_lowest_load(&self.tracker, &self.queue_info, model, candidates);
            tracing::info!(
                model,
                routed_worker = ?provider.as_ref().and_then(|p| p.kv_worker_id().map(|s| s.to_string())),
                "KVC empty prefix, fallback to lowest_load"
            );
            return provider
                .map(|provider| Selection { provider, kv_hit_ratio: 0.0, kv_hit_blocks: 0, kv_input_blocks: 0, kv_match_attempted: false, degraded: true });
        }

        // Collect worker IDs (derived from each deployment's api_base host)
        // for KV index lookup. The trie is keyed by worker_id (upstream host).
        let cand_worker_ids: Vec<String> = candidates
            .iter()
            .filter_map(|c| c.kv_worker_id().map(|id| id.to_string()))
            .collect();

        if cand_worker_ids.is_empty() {
            if let Some(ref stats) = self.stats {
                stats.record_lowest_load();
            }
            tracing::debug!(model, "no kv_worker_id on candidates, fallback to lowest-load");
            return select_lowest_load(&self.tracker, &self.queue_info, model, candidates)
                .map(|provider| Selection { provider, kv_hit_ratio: 0.0, kv_hit_blocks: 0, kv_input_blocks: 0, kv_match_attempted: false, degraded: false });
        }

        // Expand each candidate to its KV-sharing group (peers whose cache is
        // shared with it). The trie query then covers shared caches too, so a
        // peer's cached prefix counts as a hit for routing to this candidate.
        // Dedup preserves insertion order so candidate workers come first.
        let mut query_worker_ids: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for wid in &cand_worker_ids {
            for peer in self.query_workers_for(wid) {
                if seen.insert(peer.clone()) {
                    query_worker_ids.push(peer);
                }
            }
        }

        // Query KV index for prefix matches across the candidate workers AND
        // their shared-cache peers. Used only to read per-worker affinity
        // (hit_ratio / depth); scoring is done below on the unified axis.
        let matches = self.kv_index.find_matches(model, prefix_bytes, &query_worker_ids);

        // worker_id → best hit_ratio / matched depth reported for it (matched
        // workers only). Depth (matched block layers) is surfaced in the
        // selection log so affinity strength is visible at a glance.
        let mut hit_by_worker: HashMap<&str, f64> = HashMap::new();
        let mut depth_by_worker: HashMap<&str, u64> = HashMap::new();
        for m in &matches {
            hit_by_worker.insert(m.worker_id.as_str(), m.hit_ratio);
            depth_by_worker.insert(m.worker_id.as_str(), m.match_depth);
        }
        // Total prefix blocks in the request — constant across all workers
        // (it's request_hashes.len() inside find_matches). Read directly from
        // the index so it's accurate even when no worker matched (empty
        // find_matches result). Stored raw so the DFX log can keep
        // hit_blocks / input_blocks instead of just the ratio.
        let request_total_blocks: u64 = self.kv_index.prefix_block_count(prefix_bytes);

        // Score EVERY candidate on the affinity axis. Unmatched candidates
        // get hit = 0 — all-cold requests tie at 0 and are spread by the
        // round-robin below. Load is not part of the score; the rebalance
        // stage below is the only load-based intervention.
        struct Scored {
            score: f64,
            provider: Arc<dyn Provider>,
            hit: f64,
            depth: u64,
            load_pct: u64,
        }
        // Score one candidate: pure prefix hit_ratio.
        let score_cand = |cand: &Arc<dyn Provider>| -> Scored {
            // Effective hit/depth = max across the candidate's own worker and
            // its KV-sharing peers (a peer's cached prefix is reusable via the
            // shared pool).
            let (hit, depth) = cand
                .kv_worker_id()
                .map(|w| {
                    let mut best_hit = hit_by_worker.get(w).copied().unwrap_or(0.0);
                    let mut best_depth = depth_by_worker.get(w).copied().unwrap_or(0);
                    for peer in self.query_workers_for(w) {
                        best_hit = best_hit
                            .max(hit_by_worker.get(peer.as_str()).copied().unwrap_or(0.0));
                        best_depth = best_depth
                            .max(depth_by_worker.get(peer.as_str()).copied().unwrap_or(0));
                    }
                    (best_hit, best_depth)
                })
                .unwrap_or((0.0, 0));
            let load_pct = deployment_load(&self.tracker, &self.queue_info, model, cand.as_ref());
            Scored { score: hit, provider: cand.clone(), hit, depth, load_pct }
        };

        let scored: Vec<Scored> = candidates.iter().map(score_cand).collect();

        // Pick the top score; round-robin among exact ties so equal-score
        // candidates (e.g. both cold + equally idle) split traffic evenly.
        let max_score = scored
            .iter()
            .map(|s| s.score)
            .fold(f64::NEG_INFINITY, f64::max);
        let ties: Vec<&Scored> = scored.iter().filter(|s| s.score == max_score).collect();
        let idx = if ties.len() > 1 {
            (self.tie_counter.fetch_add(1, Ordering::Relaxed) % ties.len() as u64) as usize
        } else {
            0
        };
        let winner = ties[idx];

        // For the selection log only (does not affect scoring/routing): which
        // worker reported the matched prefix? In PD-disaggregated setups the
        // routed deployment (winner.provider) may differ from the worker whose
        // KV events populated the trie for this prefix — e.g. routed to 202 but
        // the hit came from 131's events because they share a KV pool. Look up
        // the routed worker and its KV-sharing peers, pick the one with a hit.
        let reported_by = if winner.hit > 0.0 {
            winner
                .provider
                .kv_worker_id()
                .map(|w| {
                    let mut best_hit = hit_by_worker.get(w).copied().unwrap_or(0.0);
                    let mut best = w.to_string();
                    for peer in self.query_workers_for(w) {
                        let peer_hit = hit_by_worker.get(peer.as_str()).copied().unwrap_or(0.0);
                        if peer_hit > best_hit {
                            best_hit = peer_hit;
                            best = peer.clone();
                        }
                    }
                    best
                })
                .unwrap_or_default()
        } else {
            String::new()
        };

        // No candidate had any KV-cache hit for this request (winner.hit == 0):
        // the trie hasn't learned this prefix yet (cold). The orchestrator
        // records the chosen winner after routing, so the next request with the
        // same prefix will hit. No key_affinity fallback — all-cold candidates
        // tie at score 0 and the round-robin above spreads them; the record
        // step then makes subsequent requests sticky to the winner.

        // Trie fill for diagnostics. `trie_nodes` (live trie nodes) vs
        // `trie_blocks` (claims): the gap is the shell population — nodes
        // left by non-cascading eviction, awaiting sweep_stale reclaim.
        // Watch `trie_nodes` across pressure rounds: plateau = sweep keeps
        // up; linear growth = node accumulation.
        let trie_blocks = self.kv_index.block_count();
        let trie_capacity = self.kv_index.block_capacity();
        let trie_nodes = self.kv_index.node_count();

        // Rebalance: when the winner is markedly more loaded than the least-
        // loaded candidate, hand off to the least-loaded so traffic spreads
        // by capacity — same policy as key_affinity (pure load balancing;
        // target is NOT picked by cache score). This is the ONLY load-based
        // intervention left (no overload gate, no load term in the score).
        // Disabled when rebalance_threshold >= 100. Only runs on the
        // non-degraded path (degraded = empty prefix → lowest_load, returns
        // early before this stage).
        let mut rebalanced = false;
        let winner = if self.rebalance_threshold < 100 {
            let load_winner = winner.load_pct;
            let least_loaded = scored
                .iter()
                .min_by_key(|s| s.load_pct)
                .expect("scored non-empty (overload fallback returns earlier)");
            if should_rebalance(load_winner, least_loaded.load_pct, self.rebalance_threshold) {
                rebalanced = true;
                if let Some(ref tracker) = self.rebalance_move_tracker {
                    tracker.record_move(
                        winner.provider.deployment_id(),
                        least_loaded.provider.deployment_id(),
                    );
                }
                tracing::info!(
                    model,
                    from_worker = ?winner.provider.kv_worker_id(),
                    to_worker = ?least_loaded.provider.kv_worker_id(),
                    load_winner,
                    load_min = least_loaded.load_pct,
                    threshold = self.rebalance_threshold,
                    "KVC rebalance: winner too loaded, routing to least-loaded (aligned with key_affinity)"
                );
                least_loaded
            } else {
                winner
            }
        } else {
            winner
        };

        // Global counters (debug page): classify the final pick. The invariant
        // total == affinity + cold + lowest_load + rebalance holds because the
        // two lowest-load exits above return early and each count exactly one
        // category here.
        if let Some(ref stats) = self.stats {
            if rebalanced {
                stats.record_rebalance();
            } else if winner.hit > 0.0 {
                stats.record_affinity();
            } else {
                stats.record_cold();
            }
        }

        // Distinguish an affinity pick (some prefix matched) from a cold/load
        // pick (no candidate had any cache for this request) — same selection
        // path, but the label makes the routing intent obvious in logs.
        if winner.hit > 0.0 {
            tracing::info!(
                model,
                routed_worker = ?winner.provider.kv_worker_id(),
                reported_by = %reported_by,
                match_depth = winner.depth,
                hit_ratio = format!("{:.2}", winner.hit),
                load_pct = winner.load_pct,
                score = format!("{:.3}", winner.score),
                candidates = candidates.len(),
                ties = ties.len(),
                trie_blocks,
                trie_capacity,
                trie_nodes,
                request_bytes = prefix_bytes.len(),
                "KVC selected (affinity)"
            );
        } else {
            tracing::info!(
                model,
                routed_worker = ?winner.provider.kv_worker_id(),
                reported_by = %reported_by,
                match_depth = winner.depth,
                hit_ratio = format!("{:.2}", winner.hit),
                load_pct = winner.load_pct,
                score = format!("{:.3}", winner.score),
                candidates = candidates.len(),
                ties = ties.len(),
                trie_blocks,
                trie_capacity,
                trie_nodes,
                request_bytes = prefix_bytes.len(),
                "KVC selected (cold, round-robin)"
            );
        }
        Some(Selection {
            provider: winner.provider.clone(),
            kv_hit_ratio: winner.hit,
            kv_hit_blocks: winner.depth,
            kv_input_blocks: request_total_blocks,
            kv_match_attempted: true,
            degraded: false,
        })
    }

    fn name(&self) -> &str {
        "kvc_aware"
    }
}

use super::load_helpers::select_lowest_load;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inflight::{InFlightGuard, InFlightTracker};
    use crate::rebalance::RebalanceMoveTracker;
    use async_trait::async_trait;
    use boom_core::kv_event::{GatewayKvEvent, KvMatchResult};
    use boom_core::types::{ChatCompletionRequest, ChatCompletionResponse};
    use boom_core::GatewayError;
    use std::collections::HashMap;

    /// Minimal Provider fake — only the identity surface the policy reads.
    struct FakeProvider {
        dep: &'static str,
        worker: &'static str,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn chat(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<ChatCompletionResponse, GatewayError> {
            unimplemented!("stats test only inspects counters")
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
            Some(self.worker)
        }
    }

    /// Canned KV index: worker → (hit_ratio, match_depth) regardless of the
    /// queried prefix. Only `find_matches` carries signal for these tests.
    struct FakeIndex {
        hits: HashMap<String, (f64, u64)>,
    }

    impl KvIndexBackend for FakeIndex {
        fn apply_event(&self, _event: &GatewayKvEvent) {}
        fn find_matches(
            &self,
            _model: &str,
            _prefix_bytes: &[u8],
            candidate_worker_ids: &[String],
        ) -> Vec<KvMatchResult> {
            candidate_worker_ids
                .iter()
                .filter_map(|w| {
                    self.hits.get(w).map(|(hr, d)| KvMatchResult {
                        worker_id: w.clone(),
                        match_depth: *d,
                        total_blocks: 4,
                        hit_ratio: *hr,
                        load_score: 0.0,
                        combined_score: *hr,
                    })
                })
                .collect()
        }
        fn remove_worker(&self, _worker_id: &str) {}
        fn block_count(&self) -> usize {
            0
        }
        fn model_names(&self) -> std::collections::HashSet<String> {
            std::collections::HashSet::new()
        }
        fn debug_dump(
            &self,
        ) -> Vec<(String, u64, Vec<String>, boom_core::kv_event::StorageTier, u64)> {
            Vec::new()
        }
    }

    fn two_candidates() -> Vec<Arc<dyn Provider>> {
        vec![
            Arc::new(FakeProvider { dep: "d1", worker: "w1" }) as Arc<dyn Provider>,
            Arc::new(FakeProvider { dep: "d2", worker: "w2" }) as Arc<dyn Provider>,
        ]
    }

    fn make_policy(
        hits: HashMap<String, (f64, u64)>,
        threshold: u64,
    ) -> (KvcAwarePolicy, Arc<InFlightTracker>, Arc<KvcSchedulerStats>) {
        let stats = Arc::new(KvcSchedulerStats::new());
        let tracker = Arc::new(InFlightTracker::new());
        let mut policy = KvcAwarePolicy::new(
            Arc::new(FakeIndex { hits }),
            tracker.clone(),
            Some(Arc::new(RebalanceMoveTracker::new())),
            Some(stats.clone()),
        );
        policy.set_rebalance_threshold(threshold);
        (policy, tracker, stats)
    }

    const PREFIX: &[u8] = b"0123456789abcdef";

    #[test]
    fn test_stats_affinity_pick() {
        let (policy, _tracker, stats) = make_policy(HashMap::from([("w1".into(), (0.5, 2))]), 100);
        let sel = policy
            .select_with_context("m", &two_candidates(), None, 0, PREFIX)
            .expect("selection");
        assert_eq!(sel.kv_hit_ratio, 0.5);
        assert_eq!(sel.kv_hit_blocks, 2);
        let s = stats.snapshot();
        assert_eq!((s.total, s.affinity, s.cold, s.lowest_load, s.rebalance), (1, 1, 0, 0, 0));
    }

    #[test]
    fn test_stats_cold_pick() {
        let (policy, _tracker, stats) = make_policy(HashMap::new(), 100);
        let sel = policy
            .select_with_context("m", &two_candidates(), None, 0, PREFIX)
            .expect("selection");
        assert_eq!(sel.kv_hit_ratio, 0.0);
        assert!(sel.kv_match_attempted);
        let s = stats.snapshot();
        assert_eq!((s.total, s.affinity, s.cold, s.lowest_load, s.rebalance), (1, 0, 1, 0, 0));
    }

    #[test]
    fn test_stats_empty_prefix_lowest_load() {
        let (policy, _tracker, stats) = make_policy(HashMap::new(), 100);
        let sel = policy
            .select_with_context("m", &two_candidates(), None, 0, &[])
            .expect("selection");
        assert!(sel.degraded);
        let s = stats.snapshot();
        assert_eq!((s.total, s.affinity, s.cold, s.lowest_load, s.rebalance), (1, 0, 0, 1, 0));
    }

    #[test]
    fn test_stats_rebalance_handoff() {
        // w1 owns the prefix (affinity winner) but is loaded 30 vs w2's 0;
        // threshold 20 → hand off to w2.
        let (policy, tracker, stats) = make_policy(HashMap::from([("w1".into(), (0.5, 2))]), 20);
        let _guards: Vec<_> = (0..30)
            .map(|_| InFlightGuard::new_for_deployment(tracker.clone(), "m", "d1", 0))
            .collect();
        let sel = policy
            .select_with_context("m", &two_candidates(), None, 0, PREFIX)
            .expect("selection");
        assert_eq!(sel.provider.deployment_id(), Some("d2"));
        let s = stats.snapshot();
        assert_eq!((s.total, s.affinity, s.cold, s.lowest_load, s.rebalance), (1, 0, 0, 0, 1));
    }

    #[test]
    fn test_stats_single_candidate_not_counted() {
        let (policy, _tracker, stats) = make_policy(HashMap::from([("w1".into(), (1.0, 4))]), 100);
        let one = vec![Arc::new(FakeProvider { dep: "d1", worker: "w1" }) as Arc<dyn Provider>];
        let sel = policy
            .select_with_context("m", &one, None, 0, PREFIX)
            .expect("selection");
        assert!(!sel.kv_match_attempted);
        assert_eq!(stats.snapshot().total, 0, "no decision made — not counted");
    }
}
