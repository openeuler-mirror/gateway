use boom_core::kv_event::KvIndexBackend;
use boom_core::provider::{DeploymentQueueInfo, Provider};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::inflight::InFlightTracker;
use crate::rebalance::RebalanceMoveTracker;
use super::load_helpers::{deployment_load, should_rebalance};
use super::{SchedulePolicy, Selection};

/// KV-cache aware scheduling policy (self-contained, no vLLM event dependency).
///
/// Scores EVERY candidate on a single unified axis combining prefix affinity
/// (cache hit) and gateway-side load (inflight as % of capacity):
///   score = cache_weight × hit_ratio + load_weight × (1 − load_pct/100)
/// Higher is better. Overloaded workers (load_pct > overload_threshold_pct)
/// are HARD-EXCLUDED before scoring; only if all are overloaded does it fall
/// back to lowest-load. Ties on the top score are broken by round-robin
/// (counter) so traffic spreads evenly when affinity/load don't differentiate.
///
/// The trie is self-learned: the orchestrator records each routed request's
/// prefix under the chosen worker, so a miss (hit=0) simply scores by load —
/// no key_affinity fallback. The capacity rebalance hands off to a less-loaded
/// candidate when the winner is overloaded; the orchestrator then records the
/// new worker (add-only, no migration of old entries — aligned with dynamo).
pub struct KvcAwarePolicy {
    /// Reference to the KV-cache prefix index.
    kv_index: Arc<dyn KvIndexBackend>,
    /// In-flight tracker for load-based fallback.
    tracker: Arc<InFlightTracker>,
    /// Optional flow control queue info for total load (in-flight + queued).
    queue_info: Option<Arc<dyn DeploymentQueueInfo>>,
    /// Weight for the cache-hit term in the unified score.
    cache_weight: f64,
    /// Weight for the load term in the unified score.
    load_weight: f64,
    /// A candidate whose inflight load ≥ this % of capacity is excluded
    /// (hard overload gate). 100 = disabled.
    overload_threshold_pct: u64,
    /// Load-balance threshold: when the winner's load_pct exceeds the lowest
    /// non-overloaded candidate's load_pct by more than this, hand off to the
    /// least-loaded candidate. 100 = disabled.
    rebalance_threshold: u64,
    /// Round-robin counter for tie-breaking among equal top scores (LB).
    tie_counter: AtomicU64,
    /// Optional tracker for rebalance-move counters (Debug page "Rebalance Moves").
    /// Records (from, to) deployment ids on each capacity rebalance handoff.
    rebalance_move_tracker: Option<Arc<RebalanceMoveTracker>>,
}

impl KvcAwarePolicy {
    pub fn new(
        kv_index: Arc<dyn KvIndexBackend>,
        tracker: Arc<InFlightTracker>,
        rebalance_move_tracker: Option<Arc<RebalanceMoveTracker>>,
    ) -> Self {
        Self {
            kv_index,
            tracker,
            queue_info: None,
            cache_weight: 0.5,
            load_weight: 0.5,
            overload_threshold_pct: 100,
            rebalance_threshold: 100,
            tie_counter: AtomicU64::new(0),
            rebalance_move_tracker,
        }
    }

    /// Configure the unified-score weights and the hard overload gate.
    pub fn set_scoring(
        &mut self,
        cache_weight: f64,
        load_weight: f64,
        overload_threshold_pct: u64,
        rebalance_threshold: u64,
    ) {
        self.cache_weight = cache_weight;
        self.load_weight = load_weight;
        self.overload_threshold_pct = overload_threshold_pct;
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

        // Score EVERY candidate on the unified axis. Unmatched candidates get
        // hit = 0 but still compete on load — this is what makes LB work across
        // independent deployments (a cold, idle worker can win when the cached
        // one is loaded). Overloaded workers are hard-excluded.
        struct Scored {
            score: f64,
            provider: Arc<dyn Provider>,
            hit: f64,
            depth: u64,
            load_pct: u64,
        }
        let mut scored: Vec<Scored> = Vec::new();
        let mut overloaded: Vec<String> = Vec::new();
        for cand in candidates {
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
            if self.overload_threshold_pct < 100 && load_pct >= self.overload_threshold_pct {
                overloaded.push(
                    cand.kv_worker_id().unwrap_or("?").to_string(),
                );
                continue;
            }
            let load_avail = 1.0 - (load_pct.min(100) as f64 / 100.0);
            let score = self.cache_weight * hit + self.load_weight * load_avail;
            scored.push(Scored {
                score,
                provider: cand.clone(),
                hit,
                depth,
                load_pct,
            });
        }

        // All candidates overloaded → route to lowest-load anyway (don't drop
        // the request). Hit ratio 0 ⇒ gateway will request a full report.
        if scored.is_empty() {
            let picked = select_lowest_load(&self.tracker, &self.queue_info, model, candidates);
            tracing::warn!(
                model,
                ?overloaded,
                routed = ?picked.as_ref().and_then(|p| p.kv_worker_id().map(|s| s.to_string())),
                "all candidates overloaded, fallback to lowest-load"
            );
            return picked
                .map(|provider| Selection { provider, kv_hit_ratio: 0.0, kv_hit_blocks: 0, kv_input_blocks: request_total_blocks, kv_match_attempted: true, degraded: false });
        }

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
        // same prefix will hit. No key_affinity fallback — the unified score
        // (hit=0 → load_weight·load_avail) plus the capacity rebalance below
        // pick a reasonable worker, and the record step makes subsequent
        // requests sticky to it.

        // Trie fill for diagnostics.
        let trie_blocks = self.kv_index.block_count();
        let trie_capacity = self.kv_index.block_capacity();

        // Rebalance: when the winner is markedly more loaded than the least-
        // loaded non-overloaded candidate, hand off to the least-loaded so
        // traffic spreads by capacity — same policy as key_affinity (pure
        // load balancing; target is NOT picked by cache score). Disabled
        // when rebalance_threshold >= 100. Only runs on the non-degraded
        // path (degraded = empty prefix → lowest_load, returns early before
        // this stage).
        let winner = if self.rebalance_threshold < 100 {
            let load_winner = winner.load_pct;
            let least_loaded = scored
                .iter()
                .min_by_key(|s| s.load_pct)
                .expect("scored non-empty (overload fallback returns earlier)");
            if should_rebalance(load_winner, least_loaded.load_pct, self.rebalance_threshold) {
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
                overloaded = ?overloaded,
                ties = ties.len(),
                trie_blocks,
                trie_capacity,
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
                overloaded = ?overloaded,
                ties = ties.len(),
                trie_blocks,
                trie_capacity,
                request_bytes = prefix_bytes.len(),
                "KVC selected (cold, load+round-robin)"
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
