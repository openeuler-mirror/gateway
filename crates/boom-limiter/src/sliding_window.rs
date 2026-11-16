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

use boom_core::types::{LimitDimension, RateLimitDecision, RateLimitKey, WindowLimit};
use dashmap::DashMap;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
    /// Epoch seconds when the current `max_budget` period started (internal,
    /// not user-visible spend). 0 means "no period started yet".
    BudgetPeriodStart,
    /// Lifetime total cost (micros) captured at `BudgetPeriodStart`, used to
    /// derive the spend accrued inside the current budget period (internal).
    BudgetPeriodBaseCost,
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
            CumulativeKind::BudgetPeriodStart => "bpstart",
            CumulativeKind::BudgetPeriodBaseCost => "bpbase",
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

/// Wall-clock aligned window start (tumbling window). All instances map the
/// same wall interval of a cache_key onto the same generation, so the
/// composite-PK DB row is the cluster-wide counter for that interval and a
/// shared-mode pull always matches the local generation. Raw first-request
/// anchoring would give each instance its own generation — peers would pull
/// rows their generation check skips and under-count admission.
fn aligned_window_start(now: u64, window_secs: u64) -> u64 {
    let w = window_secs.max(1);
    now - now % w
}

/// Partition a lifetime cost counter into the current budget period.
///
/// Returns `(period_start, baseline, period_spend)`:
/// - `period_start == 0` means no period has been anchored yet; the period
///   starts at `now` with the lifetime total as baseline (spend 0).
/// - once `period_secs` has elapsed the period rolls over the same way.
/// - `period_secs == 0` is treated as "no reset" so a bad config can never
///   reset the budget on every request.
fn budget_period_partition(
    period_start: u64,
    baseline: u64,
    total: u64,
    now: u64,
    period_secs: u64,
) -> (u64, u64, u64) {
    let elapsed = period_start == 0 || now.saturating_sub(period_start) >= period_secs;
    if period_secs > 0 && elapsed {
        (now, total, 0)
    } else {
        (period_start, baseline, total.saturating_sub(baseline))
    }
}

// ─────────────────────────────────────────────────────────────
// SlidingWindowLimiter
// ─────────────────────────────────────────────────────────────

/// In-memory rate limiter + quota store. Survives config reloads.
/// Persisted to `boom_rate_limit_state` (windows) and `boom_rate_limit_cumulative`.
///
/// Multi-instance counter model (delta protocol): each in-memory counter is
/// split into a global base (`g_*` — mirror of the shared DB row, refreshed
/// at boot-restore, by our own pushes, and by pulls) plus the local unpushed
/// increment (`counts`/`tokens`/`costs_micros`, zeroed on every successful
/// push). Admission checks and dashboard reads use the sum — the 判定口径 is
/// 全局基数 + 本地增量. `decision_shared=false` (the default) freezes the
/// base at boot + own pushes, i.e. instances enforce independently while
/// billing data still converges in the DB via pushes.
pub struct SlidingWindowLimiter {
    /// Window counters: `{kh}:{model}:{secs}` → WindowCounter (three dimensions).
    windows: Arc<DashMap<String, WindowCounter>>,
    /// Permanent counters: `kc:{kh}:{kind}` / `tc:{tid}:{kind}` → entry.
    /// Value is tokens for tin/tout, micros (1e-6 USD) for cost kinds.
    cumulative: Arc<DashMap<String, CumulativeEntry>>,
    /// Cumulative keys touched since last push (windows derive dirtiness
    /// from non-zero unpushed dims; the cumulative map can hold every key
    /// that ever had traffic, so a precise set avoids full scans).
    dirty_cumulative: Arc<RwLock<HashSet<String>>>,
    /// Snapshot-class cumulative keys (budget-period anchors) pending a
    /// full-value upsert — unlike delta counters these are overwritten, so
    /// a retried push is naturally idempotent.
    dirty_snapshots: Arc<RwLock<HashSet<String>>>,
    /// Budget-period anchors keyed by the `BudgetPeriodStart` cache_key.
    /// Kept in a single map entry so the start/baseline pair is read and
    /// updated atomically (the two cumulative rows are the persisted mirror).
    budget_periods: Arc<DashMap<String, BudgetPeriod>>,
    /// Instance identity for the delta protocol's watermark rows. Set once
    /// at boot by boom-main (before traffic); `None` disables pushes.
    instance_id: RwLock<Option<String>>,
    /// Monotonic push sequence in this instance id's watermark namespace.
    /// Stable ids make the namespace span restarts — hydrate_push_seq
    /// continues after the last committed row at boot.
    push_seq: std::sync::atomic::AtomicI64,
    /// When true, the sync cycle also pulls peer deltas into the global
    /// bases (shared enforcement). Default false → per-instance decisions.
    decision_shared: std::sync::atomic::AtomicBool,
}

/// Current `max_budget` period for one scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BudgetPeriod {
    /// Epoch seconds when the period started; 0 = not anchored yet.
    start: u64,
    /// Lifetime total cost (micros) when the period started.
    baseline: u64,
}

#[derive(Debug, Clone, Copy)]
struct WindowCounter {
    /// Unpushed local request-count increments (RPM, custom-window counts).
    counts: u64,
    /// Unpushed local token increments (TPM, custom-window tokens).
    tokens: u64,
    /// Unpushed local cost increments (micros 1e-6 USD).
    costs_micros: u64,
    /// Global base for the counts dimension (DB row mirror, this generation).
    g_counts: u64,
    /// Global base for the tokens dimension.
    g_tokens: u64,
    /// Global base for the costs dimension (micros).
    g_costs_micros: u64,
    /// Unix epoch seconds when this window started.
    window_start: u64,
    window_secs: u64,
}

impl WindowCounter {
    /// Effective counts dimension: global base + local unpushed increment.
    fn eff_counts(&self) -> u64 {
        self.g_counts.saturating_add(self.counts)
    }
    fn eff_tokens(&self) -> u64 {
        self.g_tokens.saturating_add(self.tokens)
    }
    fn eff_costs_micros(&self) -> u64 {
        self.g_costs_micros.saturating_add(self.costs_micros)
    }
    /// True when there is nothing to push in any dimension.
    fn has_unpushed(&self) -> bool {
        self.counts != 0 || self.tokens != 0 || self.costs_micros != 0
    }
}

/// One permanent counter: local unpushed delta on top of a global base.
/// Signed because admin team-rollup adjustments can locally subtract.
#[derive(Debug, Clone, Copy, Default)]
struct CumulativeEntry {
    value: i64,
    g: i64,
}

impl CumulativeEntry {
    fn effective(&self) -> i64 {
        self.g.saturating_add(self.value)
    }
}

impl SlidingWindowLimiter {
    pub fn new() -> Self {
        Self {
            windows: Arc::new(DashMap::new()),
            cumulative: Arc::new(DashMap::new()),
            dirty_cumulative: Arc::new(RwLock::new(HashSet::new())),
            dirty_snapshots: Arc::new(RwLock::new(HashSet::new())),
            budget_periods: Arc::new(DashMap::new()),
            instance_id: RwLock::new(None),
            push_seq: std::sync::atomic::AtomicI64::new(0),
            decision_shared: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Assign the instance identity used for delta-push watermark rows.
    /// Called once at boot, before any traffic.
    pub fn set_instance_id(&self, instance_id: String) {
        if let Ok(mut guard) = self.instance_id.write() {
            *guard = Some(instance_id);
        }
    }

    /// Continue the push sequence after this id's last committed watermark
    /// row. With stable instance ids the (instance_id, seq) namespace spans
    /// boots: restarting from 0 would collide with the previous
    /// incarnation's rows (PK) and break every push. Aggregate MAX always
    /// answers exactly one row — GaussDB INTO-safe. Pruned rows (instance
    /// down > 2h) restarting from 0 is safe: the pruned namespace cannot
    /// collide, and boot-time mirror restoration comes from the counter
    /// rows, not from the watermark.
    pub async fn hydrate_push_seq(&self, pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
        let instance_id = match self.instance_id.read() {
            Ok(guard) => guard.clone(),
            Err(_) => None,
        };
        let Some(instance_id) = instance_id else {
            return Ok(()); // No identity assigned — pushes are disabled anyway.
        };
        let (max_seq,): (Option<i64>,) =
            sqlx::query_as(r#"SELECT MAX(seq) FROM boom_quota_delta WHERE instance_id = $1"#)
                .bind(&instance_id)
                .fetch_one(pool)
                .await?;
        let next = max_seq.map_or(0, |m| m.saturating_add(1));
        self.push_seq.store(next, std::sync::atomic::Ordering::Relaxed);
        tracing::debug!(instance_id, next_seq = next, "push sequence hydrated");
        Ok(())
    }

    /// Flip the decision base: `true` = global (pull peer deltas into the
    /// bases at each sync cycle), `false` = per-instance independent
    /// enforcement. Pushes (billing convergence) happen in both modes.
    pub fn set_decision_shared(&self, shared: bool) {
        self.decision_shared
            .store(shared, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn decision_shared(&self) -> bool {
        self.decision_shared
            .load(std::sync::atomic::Ordering::Relaxed)
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
                        (c.eff_counts(), c.eff_tokens(), c.eff_costs_micros())
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
                        rejected_kind: Some(LimitDimension::Counts),
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
                        rejected_kind: Some(LimitDimension::Tokens),
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
                        rejected_kind: Some(LimitDimension::Costs),
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
                        if elapsed >= c.window_secs { 0 } else { c.eff_counts() }
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
            rejected_kind: None,
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
                    .map(|c| limit.saturating_sub(c.eff_counts()))
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
            rejected_kind: None,
        }
    }

    fn bump_counts(&self, cache_key: &str, window_secs: u64, weight: u64, now: u64) {
        self.windows
            .entry(cache_key.to_string())
            .and_modify(|c| {
                let elapsed = now_epoch_secs().saturating_sub(c.window_start);
                if elapsed >= c.window_secs {
                    // Rollover into a fresh generation: local unpushed AND the
                    // global bases reset together — the new generation's DB row
                    // is created by the next push, keyed by the new window_start.
                    c.counts = weight;
                    c.tokens = 0;
                    c.costs_micros = 0;
                    c.g_counts = 0;
                    c.g_tokens = 0;
                    c.g_costs_micros = 0;
                    c.window_start = aligned_window_start(now, c.window_secs);
                } else {
                    c.counts += weight;
                }
            })
            .or_insert(WindowCounter {
                counts: weight,
                tokens: 0,
                costs_micros: 0,
                g_counts: 0,
                g_tokens: 0,
                g_costs_micros: 0,
                window_start: aligned_window_start(now, window_secs),
                window_secs,
            });
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
                    c.g_counts = 0;
                    c.g_tokens = 0;
                    c.g_costs_micros = 0;
                    c.window_start = aligned_window_start(now, c.window_secs);
                } else {
                    c.tokens += tokens;
                    c.costs_micros += costs_micros;
                }
            })
            .or_insert(WindowCounter {
                counts: 0,
                tokens,
                costs_micros,
                g_counts: 0,
                g_tokens: 0,
                g_costs_micros: 0,
                window_start: aligned_window_start(now, window_secs),
                window_secs,
            });
    }

    // ── Cumulative: peek / add / reset ──

    /// Read a cumulative counter (returns 0 if absent).
    pub fn peek_cumulative(&self, scope: &QuotaScope, kind: CumulativeKind) -> u64 {
        let key = cumulative_cache_key(scope, kind);
        self.cumulative
            .get(&key)
            .map(|r| r.value().effective().max(0) as u64)
            .unwrap_or(0)
    }

    /// Spend accrued inside the current `max_budget` period, in micros.
    ///
    /// The lifetime `TotalCost` counter is partitioned by a persisted period
    /// anchor: the first call (or the first call after `period_secs` has
    /// elapsed) stores the current lifetime total as the baseline, so later
    /// calls report only the spend since the period started.
    pub fn peek_budget_period_cost_micros(&self, scope: &QuotaScope, period_secs: u64) -> u64 {
        self.peek_budget_period_cost_micros_at(scope, period_secs, now_epoch_secs())
    }

    fn peek_budget_period_cost_micros_at(
        &self,
        scope: &QuotaScope,
        period_secs: u64,
        now: u64,
    ) -> u64 {
        let total = self.peek_cumulative(scope, CumulativeKind::TotalCost);
        let cache_key = cumulative_cache_key(scope, CumulativeKind::BudgetPeriodStart);

        // The anchor pair lives in ONE map entry, so concurrent requests at a
        // period boundary cannot observe a start/baseline mismatch.
        let mut rollover: Option<(u64, u64)> = None;
        let spend = {
            let mut entry = self
                .budget_periods
                .entry(cache_key)
                .or_insert_with(|| BudgetPeriod {
                    // First touch after a restart: hydrate from the persisted
                    // cumulative mirror (loaded by `restore_cumulative`).
                    start: self.peek_cumulative(scope, CumulativeKind::BudgetPeriodStart),
                    baseline: self.peek_cumulative(scope, CumulativeKind::BudgetPeriodBaseCost),
                });
            let (start, baseline, spend) =
                budget_period_partition(entry.start, entry.baseline, total, now, period_secs);
            if start != entry.start || baseline != entry.baseline {
                entry.start = start;
                entry.baseline = baseline;
                rollover = Some((start, baseline));
            }
            spend
        };

        if let Some((start, baseline)) = rollover {
            // Mirror the new anchor into the cumulative rows for persistence.
            self.overwrite_cumulative(scope, CumulativeKind::BudgetPeriodStart, start);
            self.overwrite_cumulative(scope, CumulativeKind::BudgetPeriodBaseCost, baseline);
        }
        spend
    }

    fn add_cumulative(&self, scope: &QuotaScope, kind: CumulativeKind, delta: u64) {
        if delta == 0 {
            return;
        }
        let key = cumulative_cache_key(scope, kind);
        {
            let mut entry = self
                .cumulative
                .entry(key.clone())
                .or_insert_with(CumulativeEntry::default);
            entry.value_mut().value += delta as i64;
        }
        self.mark_dirty_cumulative(&key);
    }

    /// Snapshot-class write (budget-period anchors): full-value overwrite of
    /// the global base. Retried pushes rewrite the same value — idempotent
    /// without a watermark entry.
    fn overwrite_cumulative(&self, scope: &QuotaScope, kind: CumulativeKind, value: u64) {
        let key = cumulative_cache_key(scope, kind);
        {
            let mut entry = self
                .cumulative
                .entry(key.clone())
                .or_insert_with(CumulativeEntry::default);
            let e = entry.value_mut();
            e.g = value as i64;
            e.value = 0;
        }
        if let Ok(mut s) = self.dirty_snapshots.write() {
            s.insert(key);
        }
    }

    /// Reset all cumulative counters for a scope to zero (in-memory only).
    /// Returns pre-reset snapshot. Caller handles DB cleanup + team recompute.
    pub fn reset_cumulative_local(&self, scope: &QuotaScope) -> CumulativeSnapshot {
        // Budget-period anchors are derived state; clearing them makes the
        // next request start a fresh period alongside the zeroed totals.
        self.budget_periods
            .remove(&cumulative_cache_key(scope, CumulativeKind::BudgetPeriodStart));
        self.reset_one_local(scope, CumulativeKind::BudgetPeriodStart);
        self.reset_one_local(scope, CumulativeKind::BudgetPeriodBaseCost);
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
                let prev = entry.value().effective().max(0) as u64;
                *entry.value_mut() = CumulativeEntry::default();
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
                counts: counter.eff_counts(),
                tokens: counter.eff_tokens(),
                costs_micros: counter.eff_costs_micros(),
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
                    counts: entry.value().eff_counts(),
                    tokens: entry.value().eff_tokens(),
                    costs_micros: entry.value().eff_costs_micros(),
                    window_secs: entry.value().window_secs,
                    elapsed_secs: elapsed,
                }
            })
            .collect()
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

    /// Snapshot the key's plan-window counters — entries recorded under the
    /// `__plan__` model namespace by `check_plan_limits` (boom-main). One
    /// `WindowUsage` per distinct window_secs, carrying all three dimensions
    /// (counts / tokens / costs). Used by the dashboard window-usage column.
    pub fn peek_plan_window_usage(&self, key_hash: &str) -> Vec<WindowUsage> {
        let now = now_epoch_secs();
        let prefix = format!("{}:__plan__:", key_hash);
        self.windows
            .iter()
            .filter(|entry| {
                let ck = entry.key();
                ck.starts_with(&prefix)
                    && now.saturating_sub(entry.value().window_start) < entry.value().window_secs
            })
            .map(|entry| {
                let counter = entry.value();
                WindowUsage {
                    cache_key: entry.key().clone(),
                    counts: counter.eff_counts(),
                    tokens: counter.eff_tokens(),
                    costs_micros: counter.eff_costs_micros(),
                    window_secs: counter.window_secs,
                    elapsed_secs: now.saturating_sub(counter.window_start),
                }
            })
            .collect()
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
            let eff_tokens = counter.eff_tokens();
            let eff_costs = counter.eff_costs_micros();

            if eff_tokens > 0 {
                out.push(WindowInfo {
                    kind: WindowKind::Tokens,
                    model: model.to_string(),
                    window_secs: secs,
                    count: eff_tokens,
                    elapsed_secs: elapsed,
                    remaining_secs,
                });
            }
            if eff_costs > 0 {
                out.push(WindowInfo {
                    kind: WindowKind::CostMicros,
                    model: model.to_string(),
                    window_secs: secs,
                    count: eff_costs,
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
            self.windows.remove(&win_key);
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
        removed
    }

    /// Clear ALL window counters for every key/team (memory only). Returns
    /// entry count. Cumulative metering (lifetime spend/tokens) is NOT
    /// touched — it feeds the dashboard "spend" column and enforces plan
    /// total_token_limit/total_cost_limit; resetting it is a deliberate
    /// per-key quota operation (`clear_key_all`), not a limits reset.
    pub fn clear_all_windows(&self) -> usize {
        let count = self.windows.len();
        self.windows.clear();
        count
    }

    // ── Restore (DB → in-memory, boot path) ──

    /// Restore a window counter's global base from DB (boot path).
    /// Only restores if the window hasn't expired yet. The DB row holds the
    /// GLOBAL counter — an instance joining mid-window adopts it as its base
    /// with zero local unpushed increments.
    pub fn restore_counter(
        &self,
        cache_key: String,
        g_counts: u64,
        g_tokens: u64,
        g_costs_micros: u64,
        window_start: u64,
        window_secs: u64,
    ) {
        let now = now_epoch_secs();
        if now.saturating_sub(window_start) < window_secs {
            self.windows.insert(
                cache_key,
                WindowCounter {
                    counts: 0,
                    tokens: 0,
                    costs_micros: 0,
                    g_counts,
                    g_tokens,
                    g_costs_micros,
                    window_start,
                    window_secs,
                },
            );
        }
    }

    /// Restore a cumulative counter's global base from DB (boot path).
    pub fn restore_cumulative(&self, cache_key: String, value: i64) {
        self.cumulative.insert(
            cache_key,
            CumulativeEntry {
                value: 0,
                g: value,
            },
        );
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

    fn mark_dirty_cumulative(&self, key: &str) {
        if let Ok(mut s) = self.dirty_cumulative.write() {
            s.insert(key.to_string());
        }
    }

    fn take_dirty_cumulative(&self) -> HashSet<String> {
        std::mem::take(&mut *self.dirty_cumulative.write().expect("dirty lock poisoned"))
    }

    fn take_dirty_snapshots(&self) -> HashSet<String> {
        std::mem::take(&mut *self.dirty_snapshots.write().expect("dirty lock poisoned"))
    }
}

impl Default for SlidingWindowLimiter {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════
// Incremental delta sync (multi-instance protocol)
//
// boom_limiter owns boom_rate_limit_state (per-generation global window
// counters), boom_rate_limit_cumulative (global cumulative counters) and
// boom_quota_delta (push watermark).
// ═══════════════════════════════════════════════════════════

/// One window-dimension delta, serialized into the watermark row.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowDeltaJson {
    k: String,
    /// Window generation (window_start) the delta belongs to.
    ws: i64,
    /// Delta of the counts dimension.
    c: i64,
    /// Delta of the tokens dimension.
    t: i64,
    /// Delta of the costs dimension (micros).
    m: i64,
    /// Window length, needed when this generation's row is first inserted.
    w: i64,
}

/// One cumulative delta, serialized into the watermark row.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CumulativeDeltaJson {
    k: String,
    v: i64,
}

/// Per-cycle sync statistics.
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncStats {
    pub window_deltas: usize,
    pub cumulative_deltas: usize,
    pub snapshot_upserts: usize,
    pub pulled_windows: usize,
    pub pulled_cumulative: usize,
    pub recovered_from_watermark: bool,
}

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

    /// One incremental sync cycle:
    ///
    /// 0. Housekeeping — purge expired window rows and old watermark rows.
    /// 1. Recovery — if the watermark row for the current seq exists, the
    ///    previous ambiguous attempt actually committed; replay its mirror
    ///    effects (from the stored batch) without re-applying to the DB.
    /// 2. Collect local unpushed deltas (windows by full-map scan, cumulative
    ///    via the precise dirty set) + snapshot-class anchor overwrites.
    /// 3. In ONE transaction: apply `value = value + delta` updates, insert
    ///    the watermark row, and (decision_shared) read back global rows so
    ///    the bases absorb peer deltas atomically with our own push.
    /// 4. Post-commit mirror updates — subtract pushed deltas from the local
    ///    unpushed slots (requests that landed mid-tx keep their increments),
    ///    then overwrite bases with pulled values where available.
    ///
    /// Single-instance deployments run the exact same path: the deltas land
    /// on rows nobody else touches and pulls read back our own writes.
    pub async fn sync_delta_cycle(&self, pool: &sqlx::PgPool) -> Result<SyncStats, sqlx::Error> {
        let mut stats = SyncStats::default();

        // 0a. Purge fully-expired window rows — unbounded-growth guard. The
        // delete runs before the delta applies so a live row caught on the
        // clock boundary is re-pushed in this same cycle.
        match sqlx::query(
            r#"DELETE FROM boom_rate_limit_state
               WHERE window_start + window_secs <= EXTRACT(EPOCH FROM NOW())::BIGINT"#,
        )
        .execute(pool)
        .await
        {
            Ok(res) => {
                let n = res.rows_affected();
                if n > 0 {
                    tracing::info!(purged = n, "Removed expired rows from boom_rate_limit_state");
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Purging expired boom_rate_limit_state rows failed (continuing): {}",
                    e
                );
            }
        }

        // 0b. Prune watermark rows — any retry horizon is far shorter than 2h.
        match sqlx::query(
            r#"DELETE FROM boom_quota_delta WHERE applied_at < NOW() - INTERVAL '2 hours'"#,
        )
        .execute(pool)
        .await
        {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Pruning boom_quota_delta failed (continuing): {}", e);
            }
        }

        let instance_id = match self.instance_id.read() {
            Ok(guard) => guard.clone(),
            Err(_) => None,
        };
        let Some(instance_id) = instance_id else {
            // No identity assigned (no-DB mode / tests): counters are purely
            // in-memory this boot; nothing to push or pull.
            return Ok(stats);
        };

        let mut seq = self.push_seq.load(std::sync::atomic::Ordering::Relaxed);

        // 1. Recovery: watermark present ⇒ the previous cycle's ambiguous
        // attempt committed. Re-apply only its mirror effects.
        let committed: Option<(serde_json::Value, serde_json::Value)> =
            sqlx::query_as::<_, (serde_json::Value, serde_json::Value)>(
                r#"SELECT window_deltas, cumulative_deltas FROM boom_quota_delta
                   WHERE instance_id = $1 AND seq = $2"#,
            )
            .bind(&instance_id)
            .bind(seq)
            .fetch_optional(pool)
            .await?;
        if let Some((wd, cd)) = committed {
            let wd: Vec<WindowDeltaJson> = serde_json::from_value(wd).unwrap_or_default();
            let cd: Vec<CumulativeDeltaJson> = serde_json::from_value(cd).unwrap_or_default();
            tracing::warn!(
                seq,
                windows = wd.len(),
                cumulative = cd.len(),
                "Recovering committed-but-unmirrored delta push from watermark"
            );
            self.apply_push_mirrors(&wd, &cd);
            seq += 1;
            self.push_seq.store(seq, std::sync::atomic::Ordering::Relaxed);
            stats.recovered_from_watermark = true;
        }

        // 2. Collect deltas.
        let mut win_deltas: Vec<WindowDeltaJson> = Vec::new();
        let mut pull_window_keys: Vec<String> = Vec::new();
        {
            let now = now_epoch_secs();
            for entry in self.windows.iter() {
                let c = entry.value();
                if now.saturating_sub(c.window_start) >= c.window_secs {
                    continue; // expired generation — its deltas are dropped
                }
                pull_window_keys.push(entry.key().clone());
                if c.has_unpushed() {
                    win_deltas.push(WindowDeltaJson {
                        k: entry.key().clone(),
                        ws: c.window_start as i64,
                        c: c.counts as i64,
                        t: c.tokens as i64,
                        m: c.costs_micros as i64,
                        w: c.window_secs as i64,
                    });
                }
            }
        }

        let dirty_cum = self.take_dirty_cumulative();
        let mut cum_deltas: Vec<CumulativeDeltaJson> = Vec::new();
        for ck in &dirty_cum {
            if let Some(e) = self.cumulative.get(ck) {
                if e.value().value != 0 {
                    cum_deltas.push(CumulativeDeltaJson {
                        k: ck.clone(),
                        v: e.value().value,
                    });
                }
            }
        }

        let snapshot_keys = self.take_dirty_snapshots();
        let mut snapshot_writes: Vec<(String, i64)> = Vec::new();
        for ck in &snapshot_keys {
            if let Some(e) = self.cumulative.get(ck) {
                snapshot_writes.push((ck.clone(), e.value().g));
            }
        }

        stats.window_deltas = win_deltas.len();
        stats.cumulative_deltas = cum_deltas.len();
        stats.snapshot_upserts = snapshot_writes.len();

        let pull = self.decision_shared();
        if win_deltas.is_empty()
            && cum_deltas.is_empty()
            && snapshot_writes.is_empty()
            && (!pull || pull_window_keys.is_empty())
        {
            return Ok(stats);
        }

        // 3. Apply + watermark + read-back in one transaction.
        let mut tx = pool.begin().await?;

        for d in &win_deltas {
            let res = sqlx::query(
                r#"UPDATE boom_rate_limit_state
                   SET count = count + $3,
                       tokens = COALESCE(tokens, 0) + $4,
                       costs_micros = COALESCE(costs_micros, 0) + $5,
                       updated_at = NOW()
                   WHERE cache_key = $1 AND window_start = $2"#,
            )
            .bind(&d.k)
            .bind(d.ws)
            .bind(d.c)
            .bind(d.t)
            .bind(d.m)
            .execute(&mut *tx)
            .await?;
            if res.rows_affected() == 0 {
                // First push of this generation — create its row.
                sqlx::query(
                    r#"INSERT INTO boom_rate_limit_state
                       (cache_key, count, tokens, costs_micros, window_start, window_secs, updated_at)
                       VALUES ($1, $2, $3, $4, $5, $6, NOW())"#,
                )
                .bind(&d.k)
                .bind(d.c)
                .bind(d.t)
                .bind(d.m)
                .bind(d.ws)
                .bind(d.w)
                .execute(&mut *tx)
                .await?;
            }
        }

        for d in &cum_deltas {
            let res = sqlx::query(
                r#"UPDATE boom_rate_limit_cumulative
                   SET value = value + $2, updated_at = NOW()
                   WHERE cache_key = $1"#,
            )
            .bind(&d.k)
            .bind(d.v)
            .execute(&mut *tx)
            .await?;
            if res.rows_affected() == 0 {
                sqlx::query(
                    r#"INSERT INTO boom_rate_limit_cumulative (cache_key, value, updated_at)
                       VALUES ($1, $2, NOW())"#,
                )
                .bind(&d.k)
                .bind(d.v)
                .execute(&mut *tx)
                .await?;
            }
        }

        for (k, v) in &snapshot_writes {
            let res = sqlx::query(
                r#"UPDATE boom_rate_limit_cumulative
                   SET value = $2, updated_at = NOW()
                   WHERE cache_key = $1"#,
            )
            .bind(k)
            .bind(*v)
            .execute(&mut *tx)
            .await?;
            if res.rows_affected() == 0 {
                sqlx::query(
                    r#"INSERT INTO boom_rate_limit_cumulative (cache_key, value, updated_at)
                       VALUES ($1, $2, NOW())"#,
                )
                .bind(k)
                .bind(*v)
                .execute(&mut *tx)
                .await?;
            }
        }

        let wd_value = serde_json::to_value(&win_deltas).unwrap_or(serde_json::json!([]));
        let cd_value = serde_json::to_value(&cum_deltas).unwrap_or(serde_json::json!([]));
        sqlx::query(
            r#"INSERT INTO boom_quota_delta (instance_id, seq, window_deltas, cumulative_deltas)
               VALUES ($1, $2, $3, $4)"#,
        )
        .bind(&instance_id)
        .bind(seq)
        .bind(&wd_value)
        .bind(&cd_value)
        .execute(&mut *tx)
        .await?;

        // Read back global rows inside the same transaction — the bases then
        // absorb peer deltas atomically with our own push.
        let mut pulled_win: Vec<(String, i64, Option<i64>, Option<i64>, i64)> = Vec::new();
        let mut pulled_cum: Vec<(String, i64)> = Vec::new();
        if pull {
            if !pull_window_keys.is_empty() {
                pulled_win = sqlx::query_as::<_, (String, i64, Option<i64>, Option<i64>, i64)>(
                    r#"SELECT cache_key, count, tokens, costs_micros, window_start
                       FROM boom_rate_limit_state
                       WHERE window_start + window_secs > EXTRACT(EPOCH FROM NOW())::BIGINT
                         AND cache_key = ANY($1)"#,
                )
                .bind(&pull_window_keys)
                .fetch_all(&mut *tx)
                .await?;
            }
            if !cum_deltas.is_empty() {
                let keys: Vec<&str> = cum_deltas.iter().map(|d| d.k.as_str()).collect();
                pulled_cum = sqlx::query_as::<_, (String, i64)>(
                    r#"SELECT cache_key, value FROM boom_rate_limit_cumulative
                       WHERE cache_key = ANY($1)"#,
                )
                .bind(&keys)
                .fetch_all(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;

        // 4. Post-commit mirror updates. Entries that rolled to a new
        // generation (or were admin-cleared) mid-transaction are skipped:
        // their deltas landed on the old generation's row, whose base died
        // with the entry. Cumulative mirrors subtract with signed math — a
        // concurrent admin adjustment can leave a small negative unpushed
        // slot that self-corrects on the next push.
        self.apply_push_mirrors(&win_deltas, &cum_deltas);

        for (k, count, tokens, costs_micros, ws) in &pulled_win {
            if let Some(mut c) = self.windows.get_mut(k.as_str()) {
                let c = c.value_mut();
                if c.window_start as i64 == *ws {
                    c.g_counts = (*count).max(0) as u64;
                    c.g_tokens = tokens.unwrap_or(0).max(0) as u64;
                    c.g_costs_micros = costs_micros.unwrap_or(0).max(0) as u64;
                }
            }
        }
        for (k, v) in &pulled_cum {
            if let Some(mut e) = self.cumulative.get_mut(k.as_str()) {
                e.value_mut().g = *v;
            }
        }
        stats.pulled_windows = pulled_win.len();
        stats.pulled_cumulative = pulled_cum.len();

        self.push_seq
            .store(seq + 1, std::sync::atomic::Ordering::Relaxed);

        Ok(stats)
    }

    /// Fold a committed batch into the in-memory mirrors: the deltas are in
    /// the DB global rows, so move them from the unpushed slots into the
    /// bases. Shared by the post-commit path and watermark recovery.
    fn apply_push_mirrors(&self, wd: &[WindowDeltaJson], cd: &[CumulativeDeltaJson]) {
        for d in wd {
            if let Some(mut c) = self.windows.get_mut(&d.k) {
                let c = c.value_mut();
                if c.window_start as i64 != d.ws {
                    continue;
                }
                let dc = d.c.max(0) as u64;
                let dt = d.t.max(0) as u64;
                let dm = d.m.max(0) as u64;
                c.g_counts = c.g_counts.saturating_add(dc);
                c.g_tokens = c.g_tokens.saturating_add(dt);
                c.g_costs_micros = c.g_costs_micros.saturating_add(dm);
                c.counts = c.counts.saturating_sub(dc);
                c.tokens = c.tokens.saturating_sub(dt);
                c.costs_micros = c.costs_micros.saturating_sub(dm);
            }
        }
        for d in cd {
            if let Some(mut e) = self.cumulative.get_mut(&d.k) {
                let e = e.value_mut();
                e.g = e.g.saturating_add(d.v);
                e.value -= d.v;
            }
        }
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
            let mut entry = self
                .cumulative
                .entry(key.clone())
                .or_insert_with(CumulativeEntry::default);
            entry.value_mut().value += delta;
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

    // ── Budget-period partitioning ──

    #[test]
    fn budget_period_starts_on_first_call() {
        // No anchor yet: the period starts now and the lifetime total becomes
        // the baseline, so nothing counts as spent inside the period.
        assert_eq!(
            budget_period_partition(0, 0, 42_000, 1_000, 86_400),
            (1_000, 42_000, 0)
        );
    }

    #[test]
    fn budget_period_reports_spend_inside_window() {
        assert_eq!(
            budget_period_partition(1_000, 42_000, 60_000, 2_000, 86_400),
            (1_000, 42_000, 18_000)
        );
    }

    #[test]
    fn budget_period_rolls_over_after_duration() {
        assert_eq!(
            budget_period_partition(1_000, 42_000, 60_000, 1_000 + 86_400, 86_400),
            (1_000 + 86_400, 60_000, 0)
        );
    }

    #[test]
    fn budget_period_zero_duration_never_resets() {
        // A zero length must not silently reset the budget on every request.
        assert_eq!(
            budget_period_partition(1_000, 42_000, 60_000, 999_999, 0),
            (1_000, 42_000, 18_000)
        );
    }

    #[test]
    fn budget_period_baseline_above_total_saturates() {
        // An admin cumulative reset can leave a stale, larger baseline.
        assert_eq!(
            budget_period_partition(1_000, 90_000, 10_000, 2_000, 86_400),
            (1_000, 90_000, 0)
        );
    }

    #[test]
    fn budget_period_cost_tracks_usage_and_rollover() {
        let limiter = SlidingWindowLimiter::new();
        let scope = QuotaScope::Key {
            key_hash: "budget_period".to_string(),
        };
        let now = 10_000;

        // First check anchors the period.
        assert_eq!(limiter.peek_budget_period_cost_micros_at(&scope, 3_600, now), 0);

        limiter.add_cumulative(&scope, CumulativeKind::TotalCost, 5_000);
        assert_eq!(
            limiter.peek_budget_period_cost_micros_at(&scope, 3_600, now + 10),
            5_000
        );

        // Still inside the hour: spend does not roll off on its own.
        assert_eq!(
            limiter.peek_budget_period_cost_micros_at(&scope, 3_600, now + 3_599),
            5_000
        );

        // Period elapsed: fresh baseline, spend starts over.
        assert_eq!(
            limiter.peek_budget_period_cost_micros_at(&scope, 3_600, now + 3_600),
            0
        );
        limiter.add_cumulative(&scope, CumulativeKind::TotalCost, 1_500);
        assert_eq!(
            limiter.peek_budget_period_cost_micros_at(&scope, 3_600, now + 3_601),
            1_500
        );
    }

    #[test]
    fn reset_cumulative_clears_budget_period() {
        let limiter = SlidingWindowLimiter::new();
        let scope = QuotaScope::Key {
            key_hash: "budget_reset".to_string(),
        };
        let now = 20_000;

        limiter.add_cumulative(&scope, CumulativeKind::TotalCost, 5_000);
        assert_eq!(limiter.peek_budget_period_cost_micros_at(&scope, 3_600, now), 0);
        limiter.add_cumulative(&scope, CumulativeKind::TotalCost, 2_000);
        assert_eq!(
            limiter.peek_budget_period_cost_micros_at(&scope, 3_600, now + 1),
            2_000
        );

        limiter.reset_cumulative_local(&scope);

        // Anchors are gone: the next check starts a clean period.
        assert_eq!(
            limiter.peek_budget_period_cost_micros_at(&scope, 3_600, now + 2),
            0
        );
        limiter.add_cumulative(&scope, CumulativeKind::TotalCost, 700);
        assert_eq!(
            limiter.peek_budget_period_cost_micros_at(&scope, 3_600, now + 3),
            700
        );
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

        // Push the local deltas (global rows = the pushed values here), then
        // restore those values into a fresh limiter as global bases.
        let pushed: Vec<(String, u64, u64, u64, u64, u64)> = limiter
            .windows
            .iter()
            .map(|e| {
                let c = e.value();
                (
                    e.key().clone(),
                    c.counts,
                    c.tokens,
                    c.costs_micros,
                    c.window_start,
                    c.window_secs,
                )
            })
            .collect();
        assert!(!pushed.is_empty());

        let limiter2 = SlidingWindowLimiter::new();
        for (ck, counts, tokens, costs, ws, wsec) in &pushed {
            limiter2.restore_counter(ck.clone(), *counts, *tokens, *costs, *ws, *wsec);
        }

        let usage = limiter2.get_usage_for_key("snap_key");
        assert!(!usage.is_empty());
        assert_eq!(usage[0].counts, 2, "restored global base must be visible");
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
                g_counts: 0,
                g_tokens: 0,
                g_costs_micros: 0,
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

    // ── Delta protocol invariants (pure in-memory parts) ──

    #[test]
    fn push_mirrors_move_unpushed_into_base_preserving_effective() {
        let limiter = SlidingWindowLimiter::new();
        let ck = "mirror:gpt-4:60".to_string();
        let ws = now_epoch_secs();
        limiter.windows.insert(
            ck.clone(),
            WindowCounter {
                counts: 3,
                tokens: 700,
                costs_micros: 9_000,
                g_counts: 5,
                g_tokens: 100,
                g_costs_micros: 1_000,
                window_start: ws,
                window_secs: 60,
            },
        );
        let before = limiter.get_usage(&ck).unwrap();
        assert_eq!(before.counts, 8);
        assert_eq!(before.tokens, 800);

        let batch = vec![WindowDeltaJson {
            k: ck.clone(),
            ws: ws as i64,
            c: 3,
            t: 700,
            m: 9_000,
            w: 60,
        }];
        limiter.apply_push_mirrors(&batch, &[]);

        let after = limiter.windows.get(&ck).unwrap();
        assert_eq!(after.counts, 0, "unpushed counts zeroed");
        assert_eq!(after.tokens, 0);
        assert_eq!(after.g_counts, 8, "base absorbed the delta");
        assert_eq!(after.g_tokens, 800);
        let eff = limiter.get_usage(&ck).unwrap();
        assert_eq!(eff.counts, 8, "effective value unchanged by the push");
        assert_eq!(eff.tokens, 800);
    }

    #[test]
    fn push_mirrors_skip_rolled_generation() {
        let limiter = SlidingWindowLimiter::new();
        let ck = "rolled:gpt-4:60".to_string();
        let old_ws = now_epoch_secs() - 120;
        limiter.windows.insert(
            ck.clone(),
            WindowCounter {
                counts: 2,
                tokens: 0,
                costs_micros: 0,
                g_counts: 0,
                g_tokens: 0,
                g_costs_micros: 0,
                window_start: now_epoch_secs(), // generation rolled mid-push
                window_secs: 60,
            },
        );
        let batch = vec![WindowDeltaJson {
            k: ck.clone(),
            ws: old_ws as i64,
            c: 2,
            t: 0,
            m: 0,
            w: 60,
        }];
        limiter.apply_push_mirrors(&batch, &[]);
        let c = limiter.windows.get(&ck).unwrap();
        assert_eq!(c.counts, 2, "new generation's unpushed kept untouched");
        assert_eq!(c.g_counts, 0);
    }

    #[test]
    fn cumulative_mirror_allows_signed_self_correction() {
        let limiter = SlidingWindowLimiter::new();
        let ck = "kc:sign:tcost".to_string();
        limiter.cumulative.insert(
            ck.clone(),
            CumulativeEntry {
                value: 1_000,
                g: 5_000,
            },
        );
        // Push mirrors for a delta larger than the current unpushed slot
        // (admin team-adjust subtracted in between): the slot goes negative
        // and the NEXT push sends a negative delta, converging back.
        let batch = vec![CumulativeDeltaJson { k: ck.clone(), v: 2_000 }];
        limiter.apply_push_mirrors(&[], &batch);
        let e = limiter.cumulative.get(&ck).unwrap();
        assert_eq!(e.g, 7_000);
        assert_eq!(e.value, -1_000);
        assert_eq!(e.effective(), 6_000);
    }

    #[test]
    fn rollover_resets_global_bases() {
        let limiter = SlidingWindowLimiter::new();
        let ck = "rollbase:gpt-4:60".to_string();
        let now = now_epoch_secs();
        limiter.windows.insert(
            ck.clone(),
            WindowCounter {
                counts: 1,
                tokens: 10,
                costs_micros: 100,
                g_counts: 99,
                g_tokens: 990,
                g_costs_micros: 9_900,
                window_start: now - 120, // expired generation
                window_secs: 60,
            },
        );
        limiter.bump_counts(&ck, 60, 4, now);
        let c = limiter.windows.get(&ck).unwrap();
        assert_eq!(c.counts, 4);
        assert_eq!(c.g_counts, 0, "fresh generation starts from a zero base");
        assert_eq!(c.window_start, aligned_window_start(now, 60));
    }

    #[tokio::test]
    async fn decisions_use_global_base_plus_local_increment() {
        // Peer instance pushed 8 requests into the global row; this instance
        // restored it at boot and adds its own traffic on top. Limit 10 →
        // the 9th global request (3rd local) must be rejected.
        let limiter = SlidingWindowLimiter::new();
        let key = RateLimitKey {
            key_hash: "shared_view".to_string(),
            model: "gpt-4".to_string(),
        };
        let windows = vec![WindowLimit {
            counts: Some(10),
            tokens: None,
            costs: None,
            window_secs: 3600,
        }];
        limiter.restore_counter("shared_view:gpt-4:3600".to_string(), 8, 0, 0, now_epoch_secs(), 3600);

        assert!(limiter.peek_only(&key, &windows, 1).await.allowed);
        limiter.commit_counts(&key, &windows, 1);
        assert!(limiter.peek_only(&key, &windows, 1).await.allowed);
        limiter.commit_counts(&key, &windows, 1);
        let d = limiter.peek_only(&key, &windows, 1).await;
        assert!(!d.allowed, "global base (8) + local (2) = 10 must reject");
    }
}
