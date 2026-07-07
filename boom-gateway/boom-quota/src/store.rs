//! QuotaStore — cumulative token / cost counters + multi-window TPM/cost windows.
//!
//! Two DashMaps:
//! - `cumulative` — permanent counters keyed `kc:{key_hash}:{kind}` /
//!   `tc:{team_id}:{kind}`. Never auto-expire. Restored fully on startup.
//! - `windows` — sliding windows keyed `kw:{key_hash}:{model}:{kind}:{secs}` /
//!   `tw:{team_id}:{model}:{kind}:{secs}`. Auto-expire after `window_secs`.
//!   `kind` is `tok` for token-windows and `cst` for cost-windows (cost stored
//!   as integer micros = 1e-6 USD).
//!
//! Dirty tracking: settle marks touched keys; the background sync task only
//! writes those entries. Crash loss ≤ one sync interval (≈30s).

use dashmap::DashMap;
use rust_decimal::Decimal;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// Counter scope — which entity a counter applies to.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum QuotaScope {
    Key { key_hash: String },
    Team { team_id: String },
}

/// Which permanent counter to operate on.
///
/// `TotalCost` is stored as integer micros (1e-6 USD) internally;
/// callers convert via `decimal_to_micros` / `micros_to_decimal`.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum CumulativeKind {
    TotalInputTokens,
    TotalOutputTokens,
    TotalCost,
}

impl CumulativeKind {
    /// Suffix used in the cache_key, e.g. `kc:{key_hash}:tin`.
    fn suffix(self) -> &'static str {
        match self {
            CumulativeKind::TotalInputTokens => "tin",
            CumulativeKind::TotalOutputTokens => "tout",
            CumulativeKind::TotalCost => "tcost",
        }
    }
}

/// Which window-kind to operate on for the multi-window API.
///
/// `Tokens` value is in tokens. `CostMicros` value is in micros (1e-6 USD).
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum WindowKind {
    Tokens,
    CostMicros,
}

impl WindowKind {
    fn suffix(self) -> &'static str {
        match self {
            WindowKind::Tokens => "tok",
            WindowKind::CostMicros => "cst",
        }
    }
}

/// Read-only snapshot of a single sliding window.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WindowSnapshot {
    pub cache_key: String,
    pub count: u64,
    pub window_secs: u64,
    pub elapsed_secs: u64,
}

/// Read-only info about one window entry, with parsed (kind, model, secs).
/// Returned by `peek_key_windows` / `peek_team_windows` for dashboard listing.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WindowInfo {
    pub kind: WindowKind,
    pub model: String,
    pub window_secs: u64,
    pub count: u64,
    pub elapsed_secs: u64,
    pub remaining_secs: u64,
}

/// Result of recomputing a team's cumulative counters from member keys.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TeamRecomputeResult {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_micros: u64,
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

/// Build a multi-window cache_key: `kw:{kh}:{model}:tok:{secs}` etc.
fn window_cache_key(scope: &QuotaScope, model: &str, kind: WindowKind, window_secs: u64) -> String {
    match scope {
        QuotaScope::Key { key_hash } => {
            format!("kw:{}:{}:{}:{}", key_hash, model, kind.suffix(), window_secs)
        }
        QuotaScope::Team { team_id } => {
            format!("tw:{}:{}:{}:{}", team_id, model, kind.suffix(), window_secs)
        }
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
// QuotaStore
// ─────────────────────────────────────────────────────────────

/// In-memory quota counters. Survives config reloads. Persisted to DB.
pub struct QuotaStore {
    /// Permanent counters — `kc:{kh}:{kind}` / `tc:{tid}:{kind}`.
    /// Value is in tokens for tin/tout, micros (1e-6 USD) for tcost.
    cumulative: Arc<DashMap<String, i64>>,
    /// Sliding windows — `kw:{kh}:{model}:{kind}:{secs}` /
    /// `tw:{tid}:{model}:{kind}:{secs}`. `count` is tokens for `tok`,
    /// micros for `cst`. Stored with `window_secs` so different window sizes
    /// coexist independently.
    windows: Arc<DashMap<String, WindowEntry>>,
    /// Entries touched since last sync.
    dirty_cumulative: Arc<RwLock<HashSet<String>>>,
    dirty_windows: Arc<RwLock<HashSet<String>>>,
}

#[derive(Debug, Clone)]
struct WindowEntry {
    count: u64,
    window_start: u64,
    window_secs: u64,
}

impl QuotaStore {
    pub fn new() -> Self {
        Self {
            cumulative: Arc::new(DashMap::new()),
            windows: Arc::new(DashMap::new()),
            dirty_cumulative: Arc::new(RwLock::new(HashSet::new())),
            dirty_windows: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    // ── Cumulative: peek / add / overwrite ──

    /// Read a cumulative counter (returns 0 if absent or expired... but
    /// cumulative counters don't expire — only absence returns 0).
    pub fn peek_cumulative(&self, scope: &QuotaScope, kind: CumulativeKind) -> u64 {
        let key = cumulative_cache_key(scope, kind);
        self.cumulative
            .get(&key)
            .map(|r| (*r.value()).max(0) as u64)
            .unwrap_or(0)
    }

    /// Add `delta` to a cumulative counter. Marks dirty.
    pub fn add_cumulative(&self, scope: &QuotaScope, kind: CumulativeKind, delta: u64) {
        if delta == 0 {
            return;
        }
        let key = cumulative_cache_key(scope, kind);
        {
            let mut entry = self
                .cumulative
                .entry(key.clone())
                .or_insert_with(|| 0i64);
            *entry += delta as i64;
        }
        self.mark_dirty_cumulative(&key);
    }

    /// Overwrite a cumulative counter (used by recompute). Marks dirty.
    pub fn overwrite_cumulative(&self, scope: &QuotaScope, kind: CumulativeKind, value: u64) {
        let key = cumulative_cache_key(scope, kind);
        {
            let mut entry = self
                .cumulative
                .entry(key.clone())
                .or_insert_with(|| 0i64);
            *entry = value as i64;
        }
        self.mark_dirty_cumulative(&key);
    }

    /// Reset all three cumulative counters for a key to zero (in-memory only).
    /// Returns the pre-reset values: (input, output, cost_micros).
    /// Caller is responsible for DB cleanup and team recompute.
    pub fn reset_key_cumulative_local(&self, key_hash: &str) -> (u64, u64, u64) {
        let scope = QuotaScope::Key {
            key_hash: key_hash.to_string(),
        };
        let tin = self.reset_one_local(&scope, CumulativeKind::TotalInputTokens);
        let tout = self.reset_one_local(&scope, CumulativeKind::TotalOutputTokens);
        let tcost = self.reset_one_local(&scope, CumulativeKind::TotalCost);
        (tin, tout, tcost)
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

    // ── Sliding windows (multi-window: tokens / cost) ──

    /// Read the current value for a (scope, model, kind, window_secs) window.
    /// Returns 0 count if absent or expired.
    pub fn peek_window(
        &self,
        scope: &QuotaScope,
        model: &str,
        kind: WindowKind,
        window_secs: u64,
    ) -> WindowSnapshot {
        let key = window_cache_key(scope, model, kind, window_secs);
        let now = now_epoch_secs();
        match self.windows.get(&key) {
            Some(entry) => {
                let elapsed = now.saturating_sub(entry.window_start);
                let count = if elapsed >= entry.window_secs { 0 } else { entry.count };
                WindowSnapshot {
                    cache_key: key,
                    count,
                    window_secs: entry.window_secs,
                    elapsed_secs: elapsed,
                }
            }
            None => WindowSnapshot {
                cache_key: key,
                count: 0,
                window_secs,
                elapsed_secs: 0,
            },
        }
    }

    /// Add `delta` to a (scope, model, kind, window_secs) window. Marks dirty.
    pub fn add_window(
        &self,
        scope: &QuotaScope,
        model: &str,
        kind: WindowKind,
        window_secs: u64,
        delta: u64,
    ) {
        if delta == 0 {
            return;
        }
        let key = window_cache_key(scope, model, kind, window_secs);
        let now = now_epoch_secs();
        let mut dirty = false;
        self.windows
            .entry(key.clone())
            .and_modify(|e| {
                let elapsed = now.saturating_sub(e.window_start);
                if elapsed >= e.window_secs {
                    e.count = delta;
                    e.window_start = now;
                    e.window_secs = window_secs;
                } else {
                    e.count += delta;
                }
                dirty = true;
            })
            .or_insert_with(|| {
                dirty = true;
                WindowEntry {
                    count: delta,
                    window_start: now,
                    window_secs,
                }
            });
        if dirty {
            self.mark_dirty_window(&key);
        }
    }

    /// Remove all expired windows. Returns count removed.
    pub fn cleanup_expired(&self) -> usize {
        let now = now_epoch_secs();
        let before = self.windows.len();
        self.windows
            .retain(|_, e| now.saturating_sub(e.window_start) < e.window_secs);
        before - self.windows.len()
    }

    /// List all non-expired window entries for a given key.
    /// Each entry is parsed back into (kind, model, window_secs, current count).
    pub fn peek_key_windows(&self, key_hash: &str) -> Vec<WindowInfo> {
        let prefix = format!("kw:{}:", key_hash);
        self.scan_windows_with_prefix(&prefix)
    }

    /// List all non-expired window entries for a given team.
    pub fn peek_team_windows(&self, team_id: &str) -> Vec<WindowInfo> {
        let prefix = format!("tw:{}:", team_id);
        self.scan_windows_with_prefix(&prefix)
    }

    /// Scan all window cache_keys matching `prefix` and parse each into WindowInfo.
    /// Cache_key layout: `{prefix}{model}:{kind_suffix}:{window_secs}` where
    /// kind_suffix ∈ {tok, cst}. The prefix already includes the leading
    /// `kw:{kh}:` or `tw:{tid}:`.
    fn scan_windows_with_prefix(&self, prefix: &str) -> Vec<WindowInfo> {
        let now = now_epoch_secs();
        let mut out = Vec::new();
        for entry in self.windows.iter() {
            let ck = entry.key();
            if !ck.starts_with(prefix) {
                continue;
            }
            let we = entry.value();
            let elapsed = now.saturating_sub(we.window_start);
            if elapsed >= we.window_secs {
                continue;
            }
            // Strip prefix → "{model}:{kind}:{secs}"
            let tail = &ck[prefix.len()..];
            // Parse from the right: secs, then kind, then model (model may
            // itself contain ':'). Format guarantees exactly two ':' after
            // the prefix.
            let (rest, secs_str) = match tail.rsplit_once(':') {
                Some(p) => p,
                None => continue,
            };
            let (model, kind_str) = match rest.rsplit_once(':') {
                Some(p) => p,
                None => continue,
            };
            let secs: u64 = match secs_str.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let kind = match kind_str {
                "tok" => WindowKind::Tokens,
                "cst" => WindowKind::CostMicros,
                _ => continue,
            };
            out.push(WindowInfo {
                kind,
                model: model.to_string(),
                window_secs: secs,
                count: we.count,
                elapsed_secs: elapsed,
                remaining_secs: we.window_secs.saturating_sub(elapsed),
            });
        }
        out
    }

    // ── Dirty tracking ──

    fn mark_dirty_cumulative(&self, key: &str) {
        if let Ok(mut s) = self.dirty_cumulative.write() {
            s.insert(key.to_string());
        }
    }

    fn mark_dirty_window(&self, key: &str) {
        if let Ok(mut s) = self.dirty_windows.write() {
            s.insert(key.to_string());
        }
    }

    fn take_dirty_cumulative(&self) -> HashSet<String> {
        std::mem::take(&mut *self.dirty_cumulative.write().expect("dirty lock poisoned"))
    }

    fn take_dirty_windows(&self) -> HashSet<String> {
        std::mem::take(&mut *self.dirty_windows.write().expect("dirty lock poisoned"))
    }

    // ── Settle: called after upstream returns real usage ──

    /// Settle a finished request. Adds input/output tokens and cost to both
    /// the key and (if present) the team. Never rejects — the request is
    /// already served.
    ///
    /// `model` should be the resolved model name (post-alias), so the cost
    /// rate lookup is correct.
    ///
    /// `window_secs_list` is the set of window durations to roll forward —
    /// typically the configured `window_secs` of each `WindowLimit` entry from
    /// the active plan. Each duration produces one `tok` and one `cst` window.
    /// Empty list is OK: cumulative counters still get updated; only the
    /// multi-window roll-forward is skipped.
    pub fn settle_usage(
        &self,
        key_hash: &str,
        team_id: Option<&str>,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        total_cost: Decimal,
        window_secs_list: &[u64],
    ) {
        let total_tokens = input_tokens.saturating_add(output_tokens);
        let cost_micros = decimal_to_micros(total_cost);

        let key_scope = QuotaScope::Key {
            key_hash: key_hash.to_string(),
        };
        for &ws in window_secs_list {
            self.add_window(&key_scope, model, WindowKind::Tokens, ws, total_tokens);
            self.add_window(&key_scope, model, WindowKind::CostMicros, ws, cost_micros);
        }
        self.add_cumulative(&key_scope, CumulativeKind::TotalInputTokens, input_tokens);
        self.add_cumulative(&key_scope, CumulativeKind::TotalOutputTokens, output_tokens);
        self.add_cumulative(&key_scope, CumulativeKind::TotalCost, cost_micros);

        if let Some(tid) = team_id {
            let team_scope = QuotaScope::Team {
                team_id: tid.to_string(),
            };
            for &ws in window_secs_list {
                self.add_window(&team_scope, model, WindowKind::Tokens, ws, total_tokens);
                self.add_window(&team_scope, model, WindowKind::CostMicros, ws, cost_micros);
            }
            self.add_cumulative(&team_scope, CumulativeKind::TotalInputTokens, input_tokens);
            self.add_cumulative(&team_scope, CumulativeKind::TotalOutputTokens, output_tokens);
            self.add_cumulative(&team_scope, CumulativeKind::TotalCost, cost_micros);
        }
    }

    // ── Reset operations (DB-backed) ──

    /// Clear all cumulative + window entries for a key (memory + DB).
    /// Returns pre-reset cumulative values: (input, output, cost_micros).
    pub async fn clear_key_all(
        &self,
        pool: &sqlx::PgPool,
        key_hash: &str,
    ) -> Result<(u64, u64, u64), sqlx::Error> {
        let prev = self.reset_key_cumulative_local(key_hash);

        // Windows for this key (any model/kind/secs)
        let prefix = format!("kw:{}:", key_hash);
        self.windows.retain(|k, _| !k.starts_with(&prefix));

        // DB cleanup — cumulative
        sqlx::query(r#"DELETE FROM boom_rate_limit_cumulative WHERE cache_key LIKE $1"#)
            .bind(format!("kc:{}:%", key_hash))
            .execute(pool)
            .await?;

        // DB cleanup — windows (boom_rate_limit_state, shared with limiter)
        sqlx::query(r#"DELETE FROM boom_rate_limit_state WHERE cache_key LIKE $1"#)
            .bind(format!("kw:{}:%", key_hash))
            .execute(pool)
            .await?;

        Ok(prev)
    }

    /// Reset a team's cumulative + windows to zero, cascading to all
    /// member keys. DB-then-memory order.
    pub async fn reset_team_all(
        &self,
        pool: &sqlx::PgPool,
        team_id: &str,
        member_keys: &[String],
    ) -> Result<(), sqlx::Error> {
        // DB: delete team cumulative + team windows
        sqlx::query(r#"DELETE FROM boom_rate_limit_cumulative WHERE cache_key LIKE $1"#)
            .bind(format!("tc:{}:%", team_id))
            .execute(pool)
            .await?;

        sqlx::query(r#"DELETE FROM boom_rate_limit_state WHERE cache_key LIKE $1"#)
            .bind(format!("tw:{}:%", team_id))
            .execute(pool)
            .await?;

        // Memory: clear team entries
        let team_prefix = format!("tc:{}:", team_id);
        let team_window_prefix = format!("tw:{}:", team_id);
        self.cumulative.retain(|k, _| !k.starts_with(&team_prefix));
        self.windows
            .retain(|k, _| !k.starts_with(&team_window_prefix));

        // Cascade: clear each member key
        for kh in member_keys {
            self.clear_key_all(pool, kh).await?;
        }

        Ok(())
    }

    /// Clear ONLY a key's cumulative counters (memory + DB), leaving windows
    /// untouched. If `team_id` is provided, the same delta is subtracted from
    /// the team's cumulative so the rollup stays consistent with member sum.
    /// Returns the pre-reset key cumulative: (input, output, cost_micros).
    pub async fn clear_key_cumulative_db(
        &self,
        pool: &sqlx::PgPool,
        key_hash: &str,
        team_id: Option<&str>,
    ) -> Result<(u64, u64, u64), sqlx::Error> {
        let prev = self.reset_key_cumulative_local(key_hash);
        // DB: delete key cumulative rows.
        sqlx::query(r#"DELETE FROM boom_rate_limit_cumulative WHERE cache_key LIKE $1"#)
            .bind(format!("kc:{}:%", key_hash))
            .execute(pool)
            .await?;

        // Cascade the subtraction to team rollup (keeps team = Σ members).
        if let Some(tid) = team_id {
            let team_scope = QuotaScope::Team {
                team_id: tid.to_string(),
            };
            // i64 arithmetic — subtracting never underflows in practice
            // because team cumulative ≥ any single member's contribution.
            self.add_cumulative_signed(
                &team_scope,
                CumulativeKind::TotalInputTokens,
                -(prev.0 as i64),
            );
            self.add_cumulative_signed(
                &team_scope,
                CumulativeKind::TotalOutputTokens,
                -(prev.1 as i64),
            );
            self.add_cumulative_signed(
                &team_scope,
                CumulativeKind::TotalCost,
                -(prev.2 as i64),
            );
        }
        Ok(prev)
    }

    /// Clear ONLY a key's windows (memory + DB), cumulative untouched.
    /// Returns count of window entries removed from memory.
    pub async fn clear_key_windows_db(
        &self,
        pool: &sqlx::PgPool,
        key_hash: &str,
    ) -> Result<usize, sqlx::Error> {
        let prefix = format!("kw:{}:", key_hash);
        let before = self.windows.len();
        self.windows.retain(|k, _| !k.starts_with(&prefix));
        let removed = before - self.windows.len();

        sqlx::query(r#"DELETE FROM boom_rate_limit_state WHERE cache_key LIKE $1"#)
            .bind(format!("kw:{}:%", key_hash))
            .execute(pool)
            .await?;
        Ok(removed)
    }

    /// Clear ONLY the team's + member keys' cumulative counters (memory + DB),
    /// windows untouched. The team row is zeroed directly (not derived), so
    /// member per-key audit history is also cleared.
    pub async fn clear_team_cumulative_db(
        &self,
        pool: &sqlx::PgPool,
        team_id: &str,
        member_keys: &[String],
    ) -> Result<TeamRecomputeResult, sqlx::Error> {
        // DB: team cumulative rows first.
        sqlx::query(r#"DELETE FROM boom_rate_limit_cumulative WHERE cache_key LIKE $1"#)
            .bind(format!("tc:{}:%", team_id))
            .execute(pool)
            .await?;
        // Memory: clear team cumulative.
        let team_prefix = format!("tc:{}:", team_id);
        self.cumulative.retain(|k, _| !k.starts_with(&team_prefix));

        // Cascade: clear each member key's cumulative (no further team adjustment —
        // team row is independent and already zeroed above).
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
        // DB: team windows first.
        sqlx::query(r#"DELETE FROM boom_rate_limit_state WHERE cache_key LIKE $1"#)
            .bind(format!("tw:{}:%", team_id))
            .execute(pool)
            .await?;
        // Memory: clear team windows.
        let team_window_prefix = format!("tw:{}:", team_id);
        let mut removed = 0;
        self.windows.retain(|k, _| {
            if k.starts_with(&team_window_prefix) {
                removed += 1;
                false
            } else {
                true
            }
        });

        // Cascade: clear each member key's windows.
        for kh in member_keys {
            let r = self.clear_key_windows_db(pool, kh).await?;
            removed += r;
        }
        Ok(removed)
    }

    /// Like `add_cumulative` but accepts signed delta. Used when subtracting
    /// a member's pre-reset value from the team rollup.
    fn add_cumulative_signed(
        &self,
        scope: &QuotaScope,
        kind: CumulativeKind,
        delta: i64,
    ) {
        if delta == 0 {
            return;
        }
        let key = cumulative_cache_key(scope, kind);
        {
            let mut entry = self
                .cumulative
                .entry(key.clone())
                .or_insert_with(|| 0i64);
            *entry += delta;
        }
        self.mark_dirty_cumulative(&key);
    }

    /// Recompute a team's cumulative counters by SUM-ing member keys' rows.
    /// Overwrites the team row in memory and DB. Does NOT touch windows
    /// (they auto-expire). O(team_size) — does not scan boom_request_log.
    pub async fn recompute_team_cumulative(
        &self,
        pool: &sqlx::PgPool,
        team_id: &str,
        member_keys: &[String],
    ) -> Result<TeamRecomputeResult, sqlx::Error> {
        // Build IN list: 3 cumulative kinds × N member keys
        let mut cache_keys: Vec<String> = Vec::with_capacity(member_keys.len() * 3);
        for kh in member_keys {
            cache_keys.push(format!("kc:{}:tin", kh));
            cache_keys.push(format!("kc:{}:tout", kh));
            cache_keys.push(format!("kc:{}:tcost", kh));
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
        for (ck, v) in &rows {
            let v = (*v).max(0) as u64;
            if ck.ends_with(":tin") {
                tin = tin.saturating_add(v);
            } else if ck.ends_with(":tout") {
                tout = tout.saturating_add(v);
            } else if ck.ends_with(":tcost") {
                tcost = tcost.saturating_add(v);
            }
        }

        // Overwrite team cumulative in memory + DB
        let team_scope = QuotaScope::Team {
            team_id: team_id.to_string(),
        };
        self.overwrite_cumulative(&team_scope, CumulativeKind::TotalInputTokens, tin);
        self.overwrite_cumulative(&team_scope, CumulativeKind::TotalOutputTokens, tout);
        self.overwrite_cumulative(&team_scope, CumulativeKind::TotalCost, tcost);

        for (kind, value) in [
            (CumulativeKind::TotalInputTokens, tin),
            (CumulativeKind::TotalOutputTokens, tout),
            (CumulativeKind::TotalCost, tcost),
        ] {
            let key = cumulative_cache_key(&team_scope, kind);
            upsert_cumulative_db(pool, &key, value as i64).await?;
        }

        Ok(TeamRecomputeResult {
            total_input_tokens: tin,
            total_output_tokens: tout,
            total_cost_micros: tcost,
        })
    }

    // ── DB persistence ──

    /// Restore all cumulative counters and non-expired windows from DB.
    /// Called once at startup.
    pub async fn restore_from_db(&self, pool: &sqlx::PgPool) {
        // Cumulative: restore all (never expires)
        match sqlx::query_as::<_, (String, i64)>(
            r#"SELECT cache_key, value FROM boom_rate_limit_cumulative"#,
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                for (ck, v) in &rows {
                    self.cumulative.insert(ck.clone(), *v);
                }
                tracing::info!("Restored {} cumulative quota counter(s) from DB", rows.len());
            }
            Err(e) => {
                tracing::error!("Failed to restore cumulative quota counters: {}", e);
            }
        }

        // Windows: restore only non-expired, kw:* and tw:* rows.
        // Schema: (cache_key, count, window_start, window_secs).
        match sqlx::query_as::<_, (String, i64, i64, i64)>(
            r#"SELECT cache_key, count, window_start, window_secs
               FROM boom_rate_limit_state
               WHERE cache_key LIKE 'kw:%' OR cache_key LIKE 'tw:%'"#,
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                let now = now_epoch_secs();
                let mut restored = 0;
                for (ck, count, window_start, window_secs) in &rows {
                    let window_secs_u = *window_secs as u64;
                    if window_secs_u == 0
                        || now.saturating_sub(*window_start as u64) >= window_secs_u
                    {
                        continue;
                    }
                    self.windows.insert(
                        ck.clone(),
                        WindowEntry {
                            count: *count as u64,
                            window_start: *window_start as u64,
                            window_secs: window_secs_u,
                        },
                    );
                    restored += 1;
                }
                tracing::info!("Restored {} quota window(s) from DB", restored);
            }
            Err(e) => {
                tracing::error!("Failed to restore quota windows: {}", e);
            }
        }
    }

    /// Sync dirty entries to DB. Called periodically by the background task.
    pub async fn sync_to_db(&self, pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
        // Cumulative dirty
        let dirty_cum = self.take_dirty_cumulative();
        for ck in &dirty_cum {
            let value = self.cumulative.get(ck).map(|r| *r.value()).unwrap_or(0);
            upsert_cumulative_db(pool, ck, value).await?;
        }

        // Window dirty
        let dirty_win = self.take_dirty_windows();
        let now = now_epoch_secs();
        for ck in &dirty_win {
            if let Some(entry) = self.windows.get(ck) {
                if now.saturating_sub(entry.window_start) >= entry.window_secs {
                    continue;
                }
                let count = entry.count as i64;
                let ws = entry.window_start as i64;
                let wsec = entry.window_secs as i64;
                upsert_window_db(pool, ck, count, ws, wsec).await?;
            }
        }
        Ok(())
    }
}

impl Default for QuotaStore {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────
// DB upsert helpers (GaussDB-compatible: UPDATE → INSERT → UPDATE)
// ─────────────────────────────────────────────────────────────

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

async fn upsert_window_db(
    pool: &sqlx::PgPool,
    cache_key: &str,
    count: i64,
    window_start: i64,
    window_secs: i64,
) -> Result<(), sqlx::Error> {
    boom_core::gaussdb_upsert!(
        pool,
        || sqlx::query(
            r#"UPDATE boom_rate_limit_state
               SET count = $2, window_start = $3, window_secs = $4, updated_at = NOW()
               WHERE cache_key = $1"#,
        )
        .bind(cache_key)
        .bind(count)
        .bind(window_start)
        .bind(window_secs),
        || sqlx::query(
            r#"INSERT INTO boom_rate_limit_state (cache_key, count, window_start, window_secs, updated_at)
               VALUES ($1, $2, $3, $4, NOW())"#,
        )
        .bind(cache_key)
        .bind(count)
        .bind(window_start)
        .bind(window_secs)
    )
}
