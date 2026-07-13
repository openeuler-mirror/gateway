//! Sliding window rate limiter — the single source of truth for rate/quota.
//!
//! Two DashMaps:
//! - `windows` — multi-dim counters keyed `{kh}:{model}:{secs}` /
//!   `__team__{tid}:{model}:{secs}`. One entry stores counts/tokens/costs_micros
//!   in three independent dimensions, matching `WindowLimit`'s shape.
//! - `cumulative` — permanent counters keyed `kc:{kh}:{kind}` /
//!   `tc:{tid}:{kind}`. Never auto-expire. Restored fully on startup.
//!
//! Three-phase contract:
//! - `peek_only` — read-only check of all three dimensions across all windows.
//!   Counts checks are weight-aware (will this request fit?); tokens/costs
//!   only check historical accumulated value (we don't know real token counts
//!   at request time).
//! - `commit_counts` — increment the counts dimension (request-rate semantics).
//! - `settle_usage` — increment tokens/costs dimensions of windows + cumulative
//!   (usage-rate semantics, called after upstream returns real usage).

use boom_core::types::{RateLimitDecision, RateLimitKey, WindowLimit};
use dashmap::DashMap;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

// ═══════════════════════════════════════════════════════════
// Public types (migrated from boom-quota when it was deleted)
// ═══════════════════════════════════════════════════════════

/// Counter scope — which entity a counter applies to.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum QuotaScope {
    Key { key_hash: String },
    Team { team_id: String },
}

/// Which permanent counter to operate on.
///
/// Cost counters are stored as integer micros (1e-6 USD) internally;
/// callers convert via `decimal_to_micros` / `micros_to_decimal`.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum CumulativeKind {
    TotalInputTokens,
    TotalOutputTokens,
    TotalCost,
    /// Non-cached input token cost.
    TotalRegularInputCost,
    /// KV-cache hit input token cost (discounted rate when configured).
    TotalCachedInputCost,
    /// Output token cost.
    TotalOutputCost,
}

impl CumulativeKind {
    /// Suffix used in the cache_key, e.g. `kc:{key_hash}:tin`.
    fn suffix(self) -> &'static str {
        match self {
            CumulativeKind::TotalInputTokens => "tin",
            CumulativeKind::TotalOutputTokens => "tout",
            CumulativeKind::TotalCost => "tcost",
            CumulativeKind::TotalRegularInputCost => "cric",
            CumulativeKind::TotalCachedInputCost => "ccic",
            CumulativeKind::TotalOutputCost => "coc",
        }
    }
}

/// Which window-kind to operate on for the multi-window listing API.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum WindowKind {
    Tokens,
    CostMicros,
}

/// Read-only info about one window dimension, parsed from cache_key.
/// Returned by `peek_key_windows` / `peek_team_windows` for dashboard listing.
#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    pub kind: WindowKind,
    pub model: String,
    pub window_secs: u64,
    pub count: u64,
    pub elapsed_secs: u64,
    pub remaining_secs: u64,
}

/// Snapshot of all 6 cumulative counters for one entity (key or team).
/// Returned by reset operations so callers can log or cascade-adjust.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CumulativeSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_cost_micros: u64,
    pub regular_input_cost_micros: u64,
    pub cached_input_cost_micros: u64,
    pub output_cost_micros: u64,
}

/// Result of recomputing a team's cumulative counters from member keys.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TeamRecomputeResult {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_micros: u64,
    pub regular_input_cost_micros: u64,
    pub cached_input_cost_micros: u64,
    pub output_cost_micros: u64,
}

/// Read-only snapshot of a single multi-dim window counter.
#[derive(Debug, Clone, Serialize)]
pub struct WindowUsage {
    pub cache_key: String,
    pub counts: u64,
    pub tokens: u64,
    pub costs_micros: u64,
    pub window_secs: u64,
    pub elapsed_secs: u64,
}

// ─────────────────────────────────────────────────────────────
// Helpers: cache_key encoding + Decimal<->micros
// ─────────────────────────────────────────────────────────────

fn cumulative_cache_key(scope: &QuotaScope, kind: CumulativeKind) -> String {
    match scope {
        QuotaScope::Key { key_hash } => format!("kc:{}:{}", key_hash, kind.suffix()),
        QuotaScope::Team { team_id } => format!("tc:{}:{}", team_id, kind.suffix()),
    }
}

/// Convert Decimal USD → integer micros (1e-6 USD).
pub fn decimal_to_micros(d: Decimal) -> u64 {
    use rust_decimal::prelude::ToPrimitive;
    (d * Decimal::from(1_000_000)).to_u64().unwrap_or(0)
}

/// Convert integer micros back to Decimal USD.
pub fn micros_to_decimal(micros: u64) -> Decimal {
    Decimal::from(micros) / Decimal::from(1_000_000)
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────
// SlidingWindowLimiter
// ─────────────────────────────────────────────────────────────

/// In-memory rate limiter + quota store. Survives config reloads.
/// Persisted to `boom_rate_limit_state` (windows) and `boom_rate_limit_cumulative`.
pub struct SlidingWindowLimiter {
    /// Window counters: `{kh}:{model}:{secs}` → WindowCounter (three dimensions).
    windows: Arc<DashMap<String, WindowCounter>>,
    /// Permanent counters: `kc:{kh}:{kind}` / `tc:{tid}:{kind}` → value.
    /// Value is tokens for tin/tout, micros (1e-6 USD) for cost kinds.
    cumulative: Arc<DashMap<String, i64>>,
    /// Entries touched since last sync.
    dirty_windows: Arc<RwLock<HashSet<String>>>,
    dirty_cumulative: Arc<RwLock<HashSet<String>>>,
}

#[derive(Debug, Clone)]
struct WindowCounter {
    /// Request-count dimension (RPM, custom-window counts).
    counts: u64,
    /// Token-rate dimension (TPM, custom-window tokens).
    tokens: u64,
    /// Cost-rate dimension (cost-per-window, stored as micros 1e-6 USD).
    costs_micros: u64,
    /// Unix epoch seconds when this window started.
    window_start: u64,
    window_secs: u64,
}

impl SlidingWindowLimiter {
    pub fn new() -> Self {
        Self {
            windows: Arc::new(DashMap::new()),
            cumulative: Arc::new(DashMap::new()),
            dirty_windows: Arc::new(RwLock::new(HashSet::new())),
            dirty_cumulative: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Build the internal window cache_key from rate limit key and window duration.
    fn cache_key(key: &RateLimitKey, window_secs: u64) -> String {
        format!("{}:{}:{}", key.key_hash, key.model, window_secs)
    }

    // ── Three-phase API: peek → commit_counts → settle_usage ──

    /// Phase 1: read-only check of ALL windows. No counters incremented.
    ///
    /// For each `WindowLimit` entry:
    /// - `counts` dimension: weight-aware check (`current + weight <= limit`).
    /// - `tokens` / `costs` dimensions: historical-cumulative check
    ///   (`current >= limit` rejects — the request that pushes the counter
    ///   past the limit is allowed through; the next one is rejected).
    ///
    /// Returns `RateLimitDecision::allowed=false` if any dimension rejects,
    /// otherwise `allowed=true`. Counter state is unchanged — call
    /// `commit_counts` afterwards to actually increment counts.
    pub async fn peek_only(
        &self,
        key: &RateLimitKey,
        windows: &[WindowLimit],
        weight: u64,
    ) -> RateLimitDecision {
        let now = now_epoch_secs();

        for w in windows {
            let win_key = Self::cache_key(key, w.window_secs);
            let (cur_counts, cur_tokens, cur_costs) = match self.windows.get(&win_key) {
                Some(c) => {
                    let elapsed = now.saturating_sub(c.window_start);
                    if elapsed >= c.window_secs {
                        (0u64, 0u64, 0u64)
                    } else {
                        (c.counts, c.tokens, c.costs_micros)
                    }
                }
                None => (0u64, 0u64, 0u64),
            };

            // Counts dimension: weight-aware (will this request fit?)
            if let Some(limit) = w.counts {
                if cur_counts + weight > limit {
                    let elapsed = self
                        .windows
                        .get(&win_key)
                        .map(|c| now.saturating_sub(c.window_start))
                        .unwrap_or(0);
                    let retry_after = w.window_secs.saturating_sub(elapsed);
                    return RateLimitDecision {
                        allowed: false,
                        remaining: 0,
                        limit,
                        reset_at: chrono::Utc::now()
                            + chrono::Duration::seconds(retry_after as i64),
                        retry_after_secs: Some(retry_after),
                        rejected_window_secs: Some(w.window_secs),
                    };
                }
            }
            // Tokens dimension: historical accumulated check.
            if let Some(limit) = w.tokens {
                if cur_tokens >= limit {
                    let elapsed = self
                        .windows
                        .get(&win_key)
                        .map(|c| now.saturating_sub(c.window_start))
                        .unwrap_or(0);
                    let retry_after = w.window_secs.saturating_sub(elapsed);
                    return RateLimitDecision {
                        allowed: false,
                        remaining: 0,
                        limit,
                        reset_at: chrono::Utc::now()
                            + chrono::Duration::seconds(retry_after as i64),
                        retry_after_secs: Some(retry_after),
                        rejected_window_secs: Some(w.window_secs),
                    };
                }
            }
            // Costs dimension: same historical-cumulative logic.
            if let Some(limit) = w.costs {
                let limit_micros = decimal_to_micros(limit);
                if cur_costs >= limit_micros {
                    let elapsed = self
                        .windows
                        .get(&win_key)
                        .map(|c| now.saturating_sub(c.window_start))
                        .unwrap_or(0);
                    let retry_after = w.window_secs.saturating_sub(elapsed);
                    return RateLimitDecision {
                        allowed: false,
                        remaining: 0,
                        limit: limit_micros,
                        reset_at: chrono::Utc::now()
                            + chrono::Duration::seconds(retry_after as i64),
                        retry_after_secs: Some(retry_after),
                        rejected_window_secs: Some(w.window_secs),
                    };
                }
            }
        }

        // All passed — compute remaining for the smallest active counts window
        // (used by callers for display + log lines). u64::MAX when no counts.
        let counts_remaining = windows
            .iter()
            .filter_map(|w| w.counts.map(|limit| (w.window_secs, limit)))
            .min_by_key(|(secs, _)| *secs)
            .map(|(secs, limit)| {
                let k = Self::cache_key(key, secs);
                let cur = self
                    .windows
                    .get(&k)
                    .map(|c| {
                        let elapsed = now.saturating_sub(c.window_start);
                        if elapsed >= c.window_secs { 0 } else { c.counts }
                    })
                    .unwrap_or(0);
                limit.saturating_sub(cur).saturating_sub(weight)
            })
            .unwrap_or(u64::MAX);

        RateLimitDecision {
            allowed: true,
            remaining: counts_remaining,
            limit: 0,
            reset_at: chrono::Utc::now() + chrono::Duration::seconds(60),
            retry_after_secs: None,
            rejected_window_secs: None,
        }
    }

    /// Phase 2: increment the `counts` dimension by `weight`.
    /// MUST be called only after `peek_only` returned `allowed: true`.
    /// Does NOT touch tokens/costs dimensions — those roll forward in `settle_usage`.
    pub fn commit_counts(
        &self,
        key: &RateLimitKey,
        windows: &[WindowLimit],
        weight: u64,
    ) -> RateLimitDecision {
        let now = now_epoch_secs();
        // Collect unique window_secs from windows. Only entries whose counts
        // dimension is set (Some) actually need to be incremented, but entries
        // whose counts is None still need a counter to exist for tokens/costs
        // tracking — we create entries lazily in settle_usage instead, so here
        // we only touch entries with a counts limit.
        let mut secs_to_commit: Vec<u64> = Vec::new();
        for w in windows {
            if w.counts.is_some() && !secs_to_commit.contains(&w.window_secs) {
                secs_to_commit.push(w.window_secs);
            }
        }

        for &secs in &secs_to_commit {
            let win_key = Self::cache_key(key, secs);
            self.bump_counts(&win_key, secs, weight, now);
        }

        // Compute remaining for display.
        let counts_remaining = windows
            .iter()
            .filter_map(|w| w.counts.map(|limit| (w.window_secs, limit)))
            .min_by_key(|(secs, _)| *secs)
            .map(|(secs, limit)| {
                let k = Self::cache_key(key, secs);
                self.windows
                    .get(&k)
                    .map(|c| limit.saturating_sub(c.counts))
                    .unwrap_or(limit)
            })
            .unwrap_or(u64::MAX);

        RateLimitDecision {
            allowed: true,
            remaining: counts_remaining,
            limit: 0,
            reset_at: chrono::Utc::now() + chrono::Duration::seconds(60),
            retry_after_secs: None,
            rejected_window_secs: None,
        }
    }

    fn bump_counts(&self, cache_key: &str, window_secs: u64, weight: u64, now: u64) {
        self.windows
            .entry(cache_key.to_string())
            .and_modify(|c| {
                let elapsed = now_epoch_secs().saturating_sub(c.window_start);
                if elapsed >= c.window_secs {
                    c.counts = weight;
                    c.tokens = 0;
                    c.costs_micros = 0;
                    c.window_start = now_epoch_secs();
                } else {
                    c.counts += weight;
                }
            })
            .or_insert(WindowCounter {
                counts: weight,
                tokens: 0,
                costs_micros: 0,
                window_start: now,
                window_secs,
            });
        self.mark_dirty_window(cache_key);
    }

    /// Phase 3: settle real token/cost usage after upstream returns.
    ///
    /// Increments tokens and costs_micros dimensions of every window (keyed by
    /// unique `window_secs`), and adds to all 6 cumulative counters via `scope`.
    /// Idempotent at the caller level — caller must guard with `settled` flag.
    ///
    /// Pass the SAME `RateLimitKey` used for peek/commit (so window counters
    /// land on the same cache_key), and the matching `QuotaScope` (so
    /// cumulative counters land on the right entity).
    pub fn settle_usage(
        &self,
        key: &RateLimitKey,
        scope: &QuotaScope,
        windows: &[WindowLimit],
        input_tokens: u64,
        output_tokens: u64,
        regular_input_cost_micros: u64,
        cached_input_cost_micros: u64,
        output_cost_micros: u64,
    ) {
        let total_tokens = input_tokens.saturating_add(output_tokens);
        let total_cost_micros = regular_input_cost_micros
            .saturating_add(cached_input_cost_micros)
            .saturating_add(output_cost_micros);

        let now = now_epoch_secs();

        // 1. Roll forward windows: one entry per unique window_secs.
        // Multiple WindowLimit entries sharing the same window_secs share a
        // counter, so we dedupe — settling once covers all of them.
        let mut secs_set: Vec<u64> = Vec::new();
        for w in windows {
            if !secs_set.contains(&w.window_secs) {
                secs_set.push(w.window_secs);
            }
        }
        for &secs in &secs_set {
            let win_key = Self::cache_key(key, secs);
            self.bump_tokens_and_cost(&win_key, secs, total_tokens, total_cost_micros, now);
        }

        // 2. Cumulative: add to all 6 counters.
        self.add_cumulative(scope, CumulativeKind::TotalInputTokens, input_tokens);
        self.add_cumulative(scope, CumulativeKind::TotalOutputTokens, output_tokens);
        self.add_cumulative(scope, CumulativeKind::TotalCost, total_cost_micros);
        self.add_cumulative(
            scope,
            CumulativeKind::TotalRegularInputCost,
            regular_input_cost_micros,
        );
        self.add_cumulative(
            scope,
            CumulativeKind::TotalCachedInputCost,
            cached_input_cost_micros,
        );
        self.add_cumulative(scope, CumulativeKind::TotalOutputCost, output_cost_micros);
    }

    fn bump_tokens_and_cost(
        &self,
        cache_key: &str,
        window_secs: u64,
        tokens: u64,
        costs_micros: u64,
        now: u64,
    ) {
        if tokens == 0 && costs_micros == 0 {
            return;
        }
        self.windows
            .entry(cache_key.to_string())
            .and_modify(|c| {
                let elapsed = now_epoch_secs().saturating_sub(c.window_start);
                if elapsed >= c.window_secs {
                    c.counts = 0;
                    c.tokens = tokens;
                    c.costs_micros = costs_micros;
                    c.window_start = now_epoch_secs();
                } else {
                    c.tokens += tokens;
                    c.costs_micros += costs_micros;
                }
            })
            .or_insert(WindowCounter {
                counts: 0,
                tokens,
                costs_micros,
                window_start: now,
                window_secs,
            });
        self.mark_dirty_window(cache_key);
    }

    // ── Cumulative: peek / add / reset ──

    /// Read a cumulative counter (returns 0 if absent).
    pub fn peek_cumulative(&self, scope: &QuotaScope, kind: CumulativeKind) -> u64 {
        let key = cumulative_cache_key(scope, kind);
        self.cumulative
            .get(&key)
            .map(|r| (*r.value()).max(0) as u64)
            .unwrap_or(0)
    }

    fn add_cumulative(&self, scope: &QuotaScope, kind: CumulativeKind, delta: u64) {
        if delta == 0 {
            return;
        }
        let key = cumulative_cache_key(scope, kind);
        {
            let mut entry = self.cumulative.entry(key.clone()).or_insert_with(|| 0i64);
            *entry += delta as i64;
        }
        self.mark_dirty_cumulative(&key);
    }

    fn overwrite_cumulative(&self, scope: &QuotaScope, kind: CumulativeKind, value: u64) {
        let key = cumulative_cache_key(scope, kind);
        {
            let mut entry = self.cumulative.entry(key.clone()).or_insert_with(|| 0i64);
            *entry = value as i64;
        }
        self.mark_dirty_cumulative(&key);
    }

    /// Reset all cumulative counters for a scope to zero (in-memory only).
    /// Returns pre-reset snapshot. Caller handles DB cleanup + team recompute.
    pub fn reset_cumulative_local(&self, scope: &QuotaScope) -> CumulativeSnapshot {
        CumulativeSnapshot {
            input_tokens: self.reset_one_local(scope, CumulativeKind::TotalInputTokens),
            output_tokens: self.reset_one_local(scope, CumulativeKind::TotalOutputTokens),
            total_cost_micros: self.reset_one_local(scope, CumulativeKind::TotalCost),
            regular_input_cost_micros: self
                .reset_one_local(scope, CumulativeKind::TotalRegularInputCost),
            cached_input_cost_micros: self
                .reset_one_local(scope, CumulativeKind::TotalCachedInputCost),
            output_cost_micros: self.reset_one_local(scope, CumulativeKind::TotalOutputCost),
        }
    }

    fn reset_one_local(&self, scope: &QuotaScope, kind: CumulativeKind) -> u64 {
        let key = cumulative_cache_key(scope, kind);
        let prev = match self.cumulative.get_mut(&key) {
            Some(mut entry) => {
                let prev = (*entry).max(0) as u64;
                *entry = 0;
                prev
            }
            None => 0,
        };
        self.mark_dirty_cumulative(&key);
        prev
    }

    // ── Dashboard listing queries ──

    /// Read the current counter for a specific cache key.
    pub fn get_usage(&self, cache_key: &str) -> Option<WindowUsage> {
        let now = now_epoch_secs();
        self.windows.get(cache_key).map(|counter| {
            let elapsed = now.saturating_sub(counter.window_start);
            WindowUsage {
                cache_key: cache_key.to_string(),
                counts: counter.counts,
                tokens: counter.tokens,
                costs_micros: counter.costs_micros,
                window_secs: counter.window_secs,
                elapsed_secs: elapsed,
            }
        })
    }

    /// Read all window counters for a given key_hash.
    pub fn get_usage_for_key(&self, key_hash: &str) -> Vec<WindowUsage> {
        let now = now_epoch_secs();
        let prefix = format!("{}:", key_hash);
        self.windows
            .iter()
            .filter(|entry| entry.key().starts_with(&prefix))
            .map(|entry| {
                let elapsed = now.saturating_sub(entry.value().window_start);
                WindowUsage {
                    cache_key: entry.key().clone(),
                    counts: entry.value().counts,
                    tokens: entry.value().tokens,
                    costs_micros: entry.value().costs_micros,
                    window_secs: entry.value().window_secs,
                    elapsed_secs: elapsed,
                }
            })
            .collect()
    }

    /// Single-pass aggregation of usage for ALL keys (counts dimension only).
    /// Returns `HashMap<key_hash, (total_counts, max_remaining_secs)>`.
    /// Skips team namespace entries (`__team__{tid}`) — those aren't real keys.
    pub fn get_all_key_usage(&self) -> HashMap<String, (u64, u64)> {
        let now = now_epoch_secs();
        let mut result: HashMap<String, (u64, u64)> = HashMap::new();
        for entry in self.windows.iter() {
            // cache_key format: "{key_hash}:{model}:{window_secs}"
            let key_hash = entry.key().split(':').next().unwrap_or("");
            if key_hash.starts_with("__team__") {
                continue;
            }
            let counter = entry.value();
            let remaining =
                counter.window_secs.saturating_sub(now.saturating_sub(counter.window_start));
            let slot = result.entry(key_hash.to_string()).or_insert((0, 0));
            slot.0 += counter.counts;
            slot.1 = slot.1.max(remaining);
        }
        result
    }

    /// List all non-expired token/cost window entries for a given key.
    /// Each non-zero dimension produces a separate `WindowInfo` (mirrors the
    /// old QuotaStore.peek_key_windows contract so the dashboard code stays
    /// roughly the same).
    ///
    /// Cache_key layout: `{kh}:{model}:{secs}` — model may itself contain ':'.
    pub fn peek_key_windows(&self, key_hash: &str) -> Vec<WindowInfo> {
        self.scan_windows(&format!("{}:", key_hash))
    }

    /// List all non-expired token/cost window entries for a given team.
    pub fn peek_team_windows(&self, team_id: &str) -> Vec<WindowInfo> {
        let team_kh = format!("__team__{}", team_id);
        self.scan_windows(&format!("{}:", team_kh))
    }

    fn scan_windows(&self, prefix: &str) -> Vec<WindowInfo> {
        let now = now_epoch_secs();
        let mut out = Vec::new();
        for entry in self.windows.iter() {
            let ck = entry.key();
            if !ck.starts_with(prefix) {
                continue;
            }
            let counter = entry.value();
            let elapsed = now.saturating_sub(counter.window_start);
            if elapsed >= counter.window_secs {
                continue;
            }
            // Strip prefix → "{model}:{secs}". Parse from the right since model
            // may itself contain ':'.
            let tail = &ck[prefix.len()..];
            let (model, secs_str) = match tail.rsplit_once(':') {
                Some(p) => p,
                None => continue,
            };
            let secs: u64 = match secs_str.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let remaining_secs = counter.window_secs.saturating_sub(elapsed);

            if counter.tokens > 0 {
                out.push(WindowInfo {
                    kind: WindowKind::Tokens,
                    model: model.to_string(),
                    window_secs: secs,
                    count: counter.tokens,
                    elapsed_secs: elapsed,
                    remaining_secs,
                });
            }
            if counter.costs_micros > 0 {
                out.push(WindowInfo {
                    kind: WindowKind::CostMicros,
                    model: model.to_string(),
                    window_secs: secs,
                    count: counter.costs_micros,
                    elapsed_secs: elapsed,
                    remaining_secs,
                });
            }
        }
        out
    }

    // ── Stale-window clearing (used on schedule slot switch) ──

    /// Clear specific window counters for a rate limit key.
    /// Takes only `window_secs` values — the limit dimension is irrelevant when clearing.
    pub fn clear_windows(&self, key: &RateLimitKey, window_secs_list: &[u64]) {
        for &window_secs in window_secs_list {
            let win_key = Self::cache_key(key, window_secs);
            if self.windows.remove(&win_key).is_some() {
                self.mark_dirty_window(&win_key);
            }
        }
    }

    /// Clear all window counters for a specific key_hash (memory only).
    /// Returns count of entries removed.
    pub fn clear_for_key(&self, key_hash: &str) -> usize {
        let prefix = format!("{}:", key_hash);
        let mut removed = 0;
        self.windows.retain(|k, _| {
            if k.starts_with(&prefix) {
                removed += 1;
                false
            } else {
                true
            }
        });
        // Dirty tracking: caller will sync whole windows table on next reset,
        // so we don't bother marking individual keys dirty here.
        removed
    }

    /// Clear all window + cumulative counters (memory only). Returns windows count.
    pub fn clear_all(&self) -> usize {
        let count = self.windows.len();
        self.windows.clear();
        self.cumulative.clear();
        // Best-effort: clear dirty sets too.
        self.dirty_windows.write().expect("dirty lock poisoned").clear();
        self.dirty_cumulative.write().expect("dirty lock poisoned").clear();
        count
    }

    // ── Snapshot / restore (in-memory ↔ DB) ──

    /// Snapshot all non-expired window entries for DB persistence.
    pub fn snapshot(&self) -> Vec<WindowSnapshot> {
        let now = now_epoch_secs();
        self.windows
            .iter()
            .filter(|entry| {
                let elapsed = now.saturating_sub(entry.value().window_start);
                elapsed < entry.value().window_secs
            })
            .map(|entry| WindowSnapshot {
                cache_key: entry.key().clone(),
                counts: entry.value().counts,
                tokens: entry.value().tokens,
                costs_micros: entry.value().costs_micros,
                window_start: entry.value().window_start,
                window_secs: entry.value().window_secs,
            })
            .collect()
    }

    /// Restore a window counter entry from DB into memory.
    /// Only restores if the window hasn't expired yet.
    pub fn restore_counter(
        &self,
        cache_key: String,
        counts: u64,
        tokens: u64,
        costs_micros: u64,
        window_start: u64,
        window_secs: u64,
    ) {
        let now = now_epoch_secs();
        if now.saturating_sub(window_start) < window_secs {
            self.windows.insert(
                cache_key,
                WindowCounter {
                    counts,
                    tokens,
                    costs_micros,
                    window_start,
                    window_secs,
                },
            );
        }
    }

    /// Restore a cumulative counter from DB.
    pub fn restore_cumulative(&self, cache_key: String, value: i64) {
        self.cumulative.insert(cache_key, value);
    }

    /// Remove all expired window entries. Returns count removed.
    pub fn cleanup_expired(&self) -> usize {
        let now = now_epoch_secs();
        let before = self.windows.len();
        self.windows.retain(|_, counter| {
            let elapsed = now.saturating_sub(counter.window_start);
            elapsed < counter.window_secs
        });
        before - self.windows.len()
    }

    // ── Dirty tracking ──

    fn mark_dirty_window(&self, key: &str) {
        if let Ok(mut s) = self.dirty_windows.write() {
            s.insert(key.to_string());
        }
    }

    fn mark_dirty_cumulative(&self, key: &str) {
        if let Ok(mut s) = self.dirty_cumulative.write() {
            s.insert(key.to_string());
        }
    }

    fn take_dirty_cumulative(&self) -> HashSet<String> {
        std::mem::take(&mut *self.dirty_cumulative.write().expect("dirty lock poisoned"))
    }
}

impl Default for SlidingWindowLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of a window entry, used for DB persistence.
#[derive(Debug, Clone)]
pub struct WindowSnapshot {
    pub cache_key: String,
    pub counts: u64,
    pub tokens: u64,
    pub costs_micros: u64,
    pub window_start: u64,
    pub window_secs: u64,
}

// ═══════════════════════════════════════════════════════════
// DB operations (boom_limiter owns boom_rate_limit_state AND
// boom_rate_limit_cumulative tables)
// ═══════════════════════════════════════════════════════════

impl SlidingWindowLimiter {
    /// Restore active window counters + cumulative counters from DB.
    pub async fn restore_counters_from_db(&self, pool: &sqlx::PgPool) {
        // Windows: load non-expired rows from boom_rate_limit_state.
        // The tokens/costs_micros columns are new — old rows have them as NULL
        // (defaults to 0), which is correct: pre-migration counters were
        // counts-only and lived entirely in this same table.
        match sqlx::query_as::<_, (String, i64, Option<i64>, Option<i64>, i64, i64)>(
            r#"SELECT cache_key, count, tokens, costs_micros, window_start, window_secs
               FROM boom_rate_limit_state
               WHERE window_start + window_secs > EXTRACT(EPOCH FROM NOW())::BIGINT"#,
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                let count = rows.len();
                for (cache_key, counts, tokens, costs_micros, window_start, window_secs) in rows {
                    self.restore_counter(
                        cache_key,
                        counts as u64,
                        tokens.unwrap_or(0) as u64,
                        costs_micros.unwrap_or(0) as u64,
                        window_start as u64,
                        window_secs as u64,
                    );
                }
                tracing::info!("Restored {} rate limit window(s) from DB", count);
            }
            Err(e) => {
                tracing::error!("Failed to restore rate limit state: {}", e);
            }
        }

        // Cumulative: load all (never expires).
        match sqlx::query_as::<_, (String, i64)>(
            r#"SELECT cache_key, value FROM boom_rate_limit_cumulative"#,
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                let count = rows.len();
                for (ck, v) in rows {
                    self.restore_cumulative(ck, v);
                }
                tracing::info!("Restored {} cumulative counter(s) from DB", count);
            }
            Err(e) => {
                tracing::error!("Failed to restore cumulative quota counters: {}", e);
            }
        }
    }

    /// Sync dirty window + dirty cumulative entries to DB (background task).
    pub async fn sync_counters_to_db(&self, pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
        // 1. Windows: write all current entries (simpler than tracking exactly
        //    which keys changed — the snapshot is cheap and GaussDB upsert is
        //    idempotent). Use the snapshot() filter to skip expired entries.
        let entries = self.snapshot();
        for entry in &entries {
            let c = entry.counts as i64;
            let tok = entry.tokens as i64;
            let cst = entry.costs_micros as i64;
            let ws = entry.window_start as i64;
            let wsec = entry.window_secs as i64;
            boom_core::gaussdb_upsert!(
                pool,
                || sqlx::query(
                    r#"UPDATE boom_rate_limit_state
                       SET count = $2, tokens = $3, costs_micros = $4,
                           window_start = $5, window_secs = $6, updated_at = NOW()
                       WHERE cache_key = $1"#,
                )
                .bind(&entry.cache_key)
                .bind(c)
                .bind(tok)
                .bind(cst)
                .bind(ws)
                .bind(wsec),
                || sqlx::query(
                    r#"INSERT INTO boom_rate_limit_state
                       (cache_key, count, tokens, costs_micros, window_start, window_secs, updated_at)
                       VALUES ($1, $2, $3, $4, $5, $6, NOW())"#,
                )
                .bind(&entry.cache_key)
                .bind(c)
                .bind(tok)
                .bind(cst)
                .bind(ws)
                .bind(wsec)
            )?;
        }

        // 2. Cumulative: write only dirty entries (cheap).
        let dirty_cum = self.take_dirty_cumulative();
        for ck in &dirty_cum {
            let value = self.cumulative.get(ck).map(|r| *r.value()).unwrap_or(0);
            upsert_cumulative_db(pool, ck, value).await?;
        }

        // 3. Window dirty-set is informational only (we always sync full
        //    snapshot above) — clear it so it doesn't grow without bound.
        self.dirty_windows.write().expect("dirty lock poisoned").clear();

        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════
// Reset operations (DB-backed, cascade-aware)
// ═══════════════════════════════════════════════════════════

impl SlidingWindowLimiter {
    /// Clear all cumulative + window entries for a key (memory + DB).
    /// Returns pre-reset cumulative snapshot.
    pub async fn clear_key_all(
        &self,
        pool: &sqlx::PgPool,
        key_hash: &str,
    ) -> Result<CumulativeSnapshot, sqlx::Error> {
        let key_scope = QuotaScope::Key {
            key_hash: key_hash.to_string(),
        };
        let prev = self.reset_cumulative_local(&key_scope);
        self.clear_for_key(key_hash);

        // DB cleanup — cumulative
        sqlx::query(r#"DELETE FROM boom_rate_limit_cumulative WHERE cache_key LIKE $1"#)
            .bind(format!("kc:{}:%", key_hash))
            .execute(pool)
            .await?;

        // DB cleanup — windows
        sqlx::query(r#"DELETE FROM boom_rate_limit_state WHERE cache_key LIKE $1"#)
            .bind(format!("{}:%", key_hash))
            .execute(pool)
            .await?;

        Ok(prev)
    }

    /// Reset a team's cumulative + windows to zero, cascading to all member keys.
    pub async fn reset_team_all(
        &self,
        pool: &sqlx::PgPool,
        team_id: &str,
        member_keys: &[String],
    ) -> Result<(), sqlx::Error> {
        let team_scope = QuotaScope::Team {
            team_id: team_id.to_string(),
        };
        let team_kh = format!("__team__{}", team_id);

        // DB: delete team cumulative + team windows.
        sqlx::query(r#"DELETE FROM boom_rate_limit_cumulative WHERE cache_key LIKE $1"#)
            .bind(format!("tc:{}:%", team_id))
            .execute(pool)
            .await?;
        sqlx::query(r#"DELETE FROM boom_rate_limit_state WHERE cache_key LIKE $1"#)
            .bind(format!("{}:%", team_kh))
            .execute(pool)
            .await?;

        // Memory: clear team entries.
        self.reset_cumulative_local(&team_scope);
        self.clear_for_key(&team_kh);

        // Cascade: clear each member key.
        for kh in member_keys {
            self.clear_key_all(pool, kh).await?;
        }

        Ok(())
    }

    /// Clear ONLY a key's cumulative counters (memory + DB), leaving windows
    /// untouched. If `team_id` is provided, subtracts the key's pre-reset
    /// values from the team rollup so team = Σ members stays consistent.
    pub async fn clear_key_cumulative_db(
        &self,
        pool: &sqlx::PgPool,
        key_hash: &str,
        team_id: Option<&str>,
    ) -> Result<CumulativeSnapshot, sqlx::Error> {
        let key_scope = QuotaScope::Key {
            key_hash: key_hash.to_string(),
        };
        let prev = self.reset_cumulative_local(&key_scope);
        sqlx::query(r#"DELETE FROM boom_rate_limit_cumulative WHERE cache_key LIKE $1"#)
            .bind(format!("kc:{}:%", key_hash))
            .execute(pool)
            .await?;

        if let Some(tid) = team_id {
            let team_scope = QuotaScope::Team {
                team_id: tid.to_string(),
            };
            self.add_cumulative_signed(
                &team_scope,
                CumulativeKind::TotalInputTokens,
                -(prev.input_tokens as i64),
            );
            self.add_cumulative_signed(
                &team_scope,
                CumulativeKind::TotalOutputTokens,
                -(prev.output_tokens as i64),
            );
            self.add_cumulative_signed(
                &team_scope,
                CumulativeKind::TotalCost,
                -(prev.total_cost_micros as i64),
            );
            self.add_cumulative_signed(
                &team_scope,
                CumulativeKind::TotalRegularInputCost,
                -(prev.regular_input_cost_micros as i64),
            );
            self.add_cumulative_signed(
                &team_scope,
                CumulativeKind::TotalCachedInputCost,
                -(prev.cached_input_cost_micros as i64),
            );
            self.add_cumulative_signed(
                &team_scope,
                CumulativeKind::TotalOutputCost,
                -(prev.output_cost_micros as i64),
            );
        }
        Ok(prev)
    }

    /// Clear ONLY a key's windows (memory + DB), cumulative untouched.
    pub async fn clear_key_windows_db(
        &self,
        pool: &sqlx::PgPool,
        key_hash: &str,
    ) -> Result<usize, sqlx::Error> {
        let removed = self.clear_for_key(key_hash);
        sqlx::query(r#"DELETE FROM boom_rate_limit_state WHERE cache_key LIKE $1"#)
            .bind(format!("{}:%", key_hash))
            .execute(pool)
            .await?;
        Ok(removed)
    }

    /// Clear ONLY the team's + member keys' cumulative counters (memory + DB),
    /// windows untouched.
    pub async fn clear_team_cumulative_db(
        &self,
        pool: &sqlx::PgPool,
        team_id: &str,
        member_keys: &[String],
    ) -> Result<TeamRecomputeResult, sqlx::Error> {
        sqlx::query(r#"DELETE FROM boom_rate_limit_cumulative WHERE cache_key LIKE $1"#)
            .bind(format!("tc:{}:%", team_id))
            .execute(pool)
            .await?;
        let team_scope = QuotaScope::Team {
            team_id: team_id.to_string(),
        };
        self.reset_cumulative_local(&team_scope);

        for kh in member_keys {
            self.clear_key_cumulative_db(pool, kh, None).await?;
        }
        Ok(TeamRecomputeResult::default())
    }

    /// Clear ONLY the team's + member keys' windows (memory + DB),
    /// cumulative untouched.
    pub async fn clear_team_windows_db(
        &self,
        pool: &sqlx::PgPool,
        team_id: &str,
        member_keys: &[String],
    ) -> Result<usize, sqlx::Error> {
        let team_kh = format!("__team__{}", team_id);
        sqlx::query(r#"DELETE FROM boom_rate_limit_state WHERE cache_key LIKE $1"#)
            .bind(format!("{}:%", team_kh))
            .execute(pool)
            .await?;
        let mut removed = self.clear_for_key(&team_kh);

        for kh in member_keys {
            let r = self.clear_key_windows_db(pool, kh).await?;
            removed += r;
        }
        Ok(removed)
    }

    fn add_cumulative_signed(&self, scope: &QuotaScope, kind: CumulativeKind, delta: i64) {
        if delta == 0 {
            return;
        }
        let key = cumulative_cache_key(scope, kind);
        {
            let mut entry = self.cumulative.entry(key.clone()).or_insert_with(|| 0i64);
            *entry += delta;
        }
        self.mark_dirty_cumulative(&key);
    }

    /// Recompute a team's cumulative counters by SUM-ing member keys' rows.
    pub async fn recompute_team_cumulative(
        &self,
        pool: &sqlx::PgPool,
        team_id: &str,
        member_keys: &[String],
    ) -> Result<TeamRecomputeResult, sqlx::Error> {
        let suffixes = ["tin", "tout", "tcost", "cric", "ccic", "coc"];
        let mut cache_keys: Vec<String> = Vec::with_capacity(member_keys.len() * suffixes.len());
        for kh in member_keys {
            for sfx in suffixes {
                cache_keys.push(format!("kc:{}:{}", kh, sfx));
            }
        }

        let rows: Vec<(String, i64)> = if cache_keys.is_empty() {
            Vec::new()
        } else {
            sqlx::query_as::<_, (String, i64)>(
                r#"SELECT cache_key, value FROM boom_rate_limit_cumulative
                   WHERE cache_key = ANY($1)"#,
            )
            .bind(&cache_keys)
            .fetch_all(pool)
            .await?
        };

        let mut tin: u64 = 0;
        let mut tout: u64 = 0;
        let mut tcost: u64 = 0;
        let mut cric: u64 = 0;
        let mut ccic: u64 = 0;
        let mut coc: u64 = 0;
        for (ck, v) in &rows {
            let v = (*v).max(0) as u64;
            if ck.ends_with(":tin") {
                tin = tin.saturating_add(v);
            } else if ck.ends_with(":tout") {
                tout = tout.saturating_add(v);
            } else if ck.ends_with(":tcost") {
                tcost = tcost.saturating_add(v);
            } else if ck.ends_with(":cric") {
                cric = cric.saturating_add(v);
            } else if ck.ends_with(":ccic") {
                ccic = ccic.saturating_add(v);
            } else if ck.ends_with(":coc") {
                coc = coc.saturating_add(v);
            }
        }

        let team_scope = QuotaScope::Team {
            team_id: team_id.to_string(),
        };
        self.overwrite_cumulative(&team_scope, CumulativeKind::TotalInputTokens, tin);
        self.overwrite_cumulative(&team_scope, CumulativeKind::TotalOutputTokens, tout);
        self.overwrite_cumulative(&team_scope, CumulativeKind::TotalCost, tcost);
        self.overwrite_cumulative(&team_scope, CumulativeKind::TotalRegularInputCost, cric);
        self.overwrite_cumulative(&team_scope, CumulativeKind::TotalCachedInputCost, ccic);
        self.overwrite_cumulative(&team_scope, CumulativeKind::TotalOutputCost, coc);

        for (kind, value) in [
            (CumulativeKind::TotalInputTokens, tin),
            (CumulativeKind::TotalOutputTokens, tout),
            (CumulativeKind::TotalCost, tcost),
            (CumulativeKind::TotalRegularInputCost, cric),
            (CumulativeKind::TotalCachedInputCost, ccic),
            (CumulativeKind::TotalOutputCost, coc),
        ] {
            let key = cumulative_cache_key(&team_scope, kind);
            upsert_cumulative_db(pool, &key, value as i64).await?;
        }

        Ok(TeamRecomputeResult {
            total_input_tokens: tin,
            total_output_tokens: tout,
            total_cost_micros: tcost,
            regular_input_cost_micros: cric,
            cached_input_cost_micros: ccic,
            output_cost_micros: coc,
        })
    }
}

// ═══════════════════════════════════════════════════════════
// DB upsert helper (GaussDB-compatible: UPDATE → INSERT → UPDATE)
// ═══════════════════════════════════════════════════════════

async fn upsert_cumulative_db(
    pool: &sqlx::PgPool,
    cache_key: &str,
    value: i64,
) -> Result<(), sqlx::Error> {
    boom_core::gaussdb_upsert!(
        pool,
        || sqlx::query(
            r#"UPDATE boom_rate_limit_cumulative
               SET value = $2, updated_at = NOW()
               WHERE cache_key = $1"#,
        )
        .bind(cache_key)
        .bind(value),
        || sqlx::query(
            r#"INSERT INTO boom_rate_limit_cumulative (cache_key, value, updated_at)
               VALUES ($1, $2, NOW())"#,
        )
        .bind(cache_key)
        .bind(value)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Three-dimension window tests ──

    #[tokio::test]
    async fn test_counts_dim_rejects_on_overflow() {
        // Counts dimension: weight-aware. Limit=2 → 3rd request rejected.
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "counts_dim".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(2),
            tokens: None,
            costs: None,
            window_secs: 60,
        }];

        let d1 = limiter.peek_only(&key, &windows, 1).await;
        assert!(d1.allowed);
        limiter.commit_counts(&key, &windows, 1);

        let d2 = limiter.peek_only(&key, &windows, 1).await;
        assert!(d2.allowed);
        limiter.commit_counts(&key, &windows, 1);

        let d3 = limiter.peek_only(&key, &windows, 1).await;
        assert!(!d3.allowed, "third request must be rejected by counts dim");
        assert_eq!(d3.rejected_window_secs, Some(60));
    }

    #[tokio::test]
    async fn test_tokens_dim_rejects_on_historical_overflow() {
        // Tokens dimension: historical-cumulative check. Settle 100 tokens,
        // limit=100 → next peek rejects (current >= limit).
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "tokens_dim".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: None,
            tokens: Some(100),
            costs: None,
            window_secs: 60,
        }];
        let scope = QuotaScope::Key {
            key_hash: "tokens_dim".to_string(),
        };

        // Settle 100 tokens (simulates a previous request's usage).
        limiter.settle_usage(&key, &scope, &windows, 100, 0, 0, 0, 0);

        // Peek now: tokens dimension has current=100, limit=100 → reject.
        let d = limiter.peek_only(&key, &windows, 1).await;
        assert!(!d.allowed, "tokens dim must reject when current >= limit");
        assert_eq!(d.rejected_window_secs, Some(60));
    }

    #[tokio::test]
    async fn test_costs_dim_rejects_on_historical_overflow() {
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "costs_dim".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: None,
            tokens: None,
            costs: Some(Decimal::new(50, 0)),
            window_secs: 60,
        }];
        let scope = QuotaScope::Key {
            key_hash: "costs_dim".to_string(),
        };

        // Settle $50 → 50_000_000 micros.
        limiter.settle_usage(
            &key, &scope, &windows, 0, 0,
            50_000_000, 0, 0,
        );

        // Peek: costs dim current=50_000_000, limit_micros=50_000_000 → reject.
        let d = limiter.peek_only(&key, &windows, 1).await;
        assert!(!d.allowed, "costs dim must reject when current >= limit");
    }

    #[tokio::test]
    async fn test_three_dim_window_independent_limits() {
        // Same window entry has all three dimensions set. Each dim rejects
        // independently — the dim that overflows first dictates rejection.
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "three_dim".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(10),
            tokens: Some(100_000),
            costs: Some(Decimal::new(50, 0)),
            window_secs: 60,
        }];
        let scope = QuotaScope::Key {
            key_hash: "three_dim".to_string(),
        };

        // First request: counts=1, tokens=0, costs=0. All pass.
        let d1 = limiter.peek_only(&key, &windows, 1).await;
        assert!(d1.allowed);
        limiter.commit_counts(&key, &windows, 1);
        // Settle 50K tokens + $30.
        limiter.settle_usage(
            &key, &scope, &windows,
            30_000, 20_000,
            30_000_000, 0, 0,
        );

        // Second request: counts=2/10 ok, tokens=50K/100K ok, costs=$30/$50 ok.
        let d2 = limiter.peek_only(&key, &windows, 1).await;
        assert!(d2.allowed);
        limiter.commit_counts(&key, &windows, 1);
        // Settle another 60K tokens (now 110K > 100K).
        limiter.settle_usage(
            &key, &scope, &windows,
            60_000, 0,
            0, 0, 0,
        );

        // Third request: tokens dim current=110K >= 100K → reject.
        let d3 = limiter.peek_only(&key, &windows, 1).await;
        assert!(!d3.allowed, "tokens dim must reject on independent overflow");
    }

    #[tokio::test]
    async fn test_cumulative_persists_across_windows() {
        // Cumulative counters don't reset when the window expires.
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "cum_persists".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(2),
            tokens: None,
            costs: None,
            window_secs: 1, // 1-second window — easy to expire in test
        }];
        let scope = QuotaScope::Key {
            key_hash: "cum_persists".to_string(),
        };

        limiter.settle_usage(&key, &scope, &windows, 100, 200, 0, 0, 0);

        // Wait for window to expire.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        // Window counters for tokens should be gone (expired).
        let windows_after = limiter.peek_key_windows("cum_persists");
        assert!(
            windows_after.is_empty(),
            "token windows should have expired"
        );

        // Cumulative counters should still hold the values.
        let input = limiter.peek_cumulative(&scope, CumulativeKind::TotalInputTokens);
        let output = limiter.peek_cumulative(&scope, CumulativeKind::TotalOutputTokens);
        assert_eq!(input, 100);
        assert_eq!(output, 200);
    }

    #[tokio::test]
    async fn test_commit_counts_only_increments_counts_dim() {
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "commit_counts_only".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(10),
            tokens: Some(100),
            costs: None,
            window_secs: 60,
        }];

        limiter.commit_counts(&key, &windows, 1);
        limiter.commit_counts(&key, &windows, 1);

        let win_key = "commit_counts_only:gpt-4:60";
        let counter = limiter.windows.get(win_key).unwrap();
        assert_eq!(counter.counts, 2, "counts dim should be 2 after two commits");
        assert_eq!(counter.tokens, 0, "tokens dim should be 0 (commit_counts doesn't touch it)");
        assert_eq!(counter.costs_micros, 0);
    }

    #[tokio::test]
    async fn test_settle_usage_updates_both_windows_and_cumulative() {
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "settle_test".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(10),
            tokens: Some(1000),
            costs: None,
            window_secs: 60,
        }];
        let scope = QuotaScope::Key {
            key_hash: "settle_test".to_string(),
        };

        limiter.settle_usage(
            &key, &scope, &windows,
            100, 50,
            1_500_000, 0, 500_000,
        );

        // Window counter: tokens = 150, costs_micros = 2_000_000.
        let win_key = "settle_test:gpt-4:60";
        let counter = limiter.windows.get(win_key).unwrap();
        assert_eq!(counter.tokens, 150);
        assert_eq!(counter.costs_micros, 2_000_000);
        assert_eq!(counter.counts, 0, "settle_usage doesn't touch counts dim");

        // Cumulative counters.
        assert_eq!(limiter.peek_cumulative(&scope, CumulativeKind::TotalInputTokens), 100);
        assert_eq!(limiter.peek_cumulative(&scope, CumulativeKind::TotalOutputTokens), 50);
        assert_eq!(limiter.peek_cumulative(&scope, CumulativeKind::TotalCost), 2_000_000);
        assert_eq!(limiter.peek_cumulative(&scope, CumulativeKind::TotalRegularInputCost), 1_500_000);
        assert_eq!(limiter.peek_cumulative(&scope, CumulativeKind::TotalOutputCost), 500_000);
    }

    #[tokio::test]
    async fn test_peek_only_does_not_increment() {
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "peek_only_test".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(5),
            tokens: None,
            costs: None,
            window_secs: 60,
        }];

        let d = limiter.peek_only(&key, &windows, 1).await;
        assert!(d.allowed);
        assert!(
            limiter.get_usage(&SlidingWindowLimiter::cache_key(&key, 60)).is_none(),
            "peek_only must not create any counter"
        );
    }

    #[tokio::test]
    async fn test_rejected_peek_does_not_allow_commit_to_bump() {
        // If peek_only rejects, caller should not call commit_counts. Verify
        // peek_only correctly rejects when counts dim is full, and the caller
        // not committing leaves counters clean.
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "reject_test".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(1),
            tokens: None,
            costs: None,
            window_secs: 18000,
        }];

        let d1 = limiter.peek_only(&key, &windows, 1).await;
        assert!(d1.allowed);
        limiter.commit_counts(&key, &windows, 1);

        let d2 = limiter.peek_only(&key, &windows, 1).await;
        assert!(!d2.allowed);
        assert_eq!(d2.rejected_window_secs, Some(18000));

        // Caller honors the rejection — does NOT commit. Window counter stays at 1.
        let usage = limiter
            .get_usage(&SlidingWindowLimiter::cache_key(&key, 18000))
            .unwrap();
        assert_eq!(usage.counts, 1, "rejected peek must not bump counts");
    }

    #[tokio::test]
    async fn test_rpm_limit_via_counts_window_60s() {
        // The post-normalization shape: rpm_limit is a 60s counts entry
        // (effective_limits merges shorthand). Verify 3 requests against
        // counts=2 limit allows 2 and rejects the 3rd.
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "rpm_60s".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(2),
            tokens: None,
            costs: None,
            window_secs: 60,
        }];

        for i in 0..2 {
            let d = limiter.peek_only(&key, &windows, 1).await;
            assert!(d.allowed, "request {} should pass", i + 1);
            limiter.commit_counts(&key, &windows, 1);
        }
        let d3 = limiter.peek_only(&key, &windows, 1).await;
        assert!(!d3.allowed, "third request must be rejected");
    }

    #[tokio::test]
    async fn test_custom_window_counts() {
        // Allow 2 requests per 5 hours (18000 seconds).
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "custom_window".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(2),
            tokens: None,
            costs: None,
            window_secs: 18000,
        }];

        let d1 = limiter.peek_only(&key, &windows, 1).await;
        assert!(d1.allowed);
        limiter.commit_counts(&key, &windows, 1);

        let d2 = limiter.peek_only(&key, &windows, 1).await;
        assert!(d2.allowed);
        limiter.commit_counts(&key, &windows, 1);

        let d3 = limiter.peek_only(&key, &windows, 1).await;
        assert!(!d3.allowed);
    }

    #[tokio::test]
    async fn test_snapshot_and_restore() {
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "snap_key".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(10),
            tokens: None,
            costs: None,
            window_secs: 60,
        }];

        limiter.commit_counts(&key, &windows, 1);
        limiter.commit_counts(&key, &windows, 1);

        let snap = limiter.snapshot();
        assert!(!snap.is_empty());

        let limiter2 = SlidingWindowLimiter::new();
        for entry in &snap {
            limiter2.restore_counter(
                entry.cache_key.clone(),
                entry.counts,
                entry.tokens,
                entry.costs_micros,
                entry.window_start,
                entry.window_secs,
            );
        }

        let usage = limiter2.get_usage_for_key("snap_key");
        assert!(!usage.is_empty());
        assert_eq!(usage[0].counts, 2);
    }

    #[tokio::test]
    async fn test_cleanup_expired() {
        let limiter = SlidingWindowLimiter::new();

        // Manually insert an already-expired counter.
        limiter.windows.insert(
            "expired_key:gpt-4:60".to_string(),
            WindowCounter {
                counts: 5,
                tokens: 0,
                costs_micros: 0,
                window_start: now_epoch_secs() - 120,
                window_secs: 60,
            },
        );

        // Insert a valid counter.
        let key = RateLimitKey {
            key_hash: "valid_key".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(10),
            tokens: None,
            costs: None,
            window_secs: 60,
        }];
        let _ = limiter.peek_only(&key, &windows, 1).await;
        limiter.commit_counts(&key, &windows, 1);

        let removed = limiter.cleanup_expired();
        assert_eq!(removed, 1);
    }

    #[tokio::test]
    async fn test_reset_cumulative_local() {
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "reset_test".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(10),
            tokens: None,
            costs: None,
            window_secs: 60,
        }];
        let scope = QuotaScope::Key {
            key_hash: "reset_test".to_string(),
        };

        limiter.settle_usage(&key, &scope, &windows, 100, 50, 1_000_000, 0, 0);

        let snap = limiter.reset_cumulative_local(&scope);
        assert_eq!(snap.input_tokens, 100);
        assert_eq!(snap.output_tokens, 50);
        assert_eq!(snap.total_cost_micros, 1_000_000);

        // After reset, peek returns 0.
        assert_eq!(limiter.peek_cumulative(&scope, CumulativeKind::TotalInputTokens), 0);
    }
}
