use chrono::Timelike;
use dashmap::DashMap;
use futures::Stream;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

/// Convert Decimal USD → integer micros (1e-6 USD) for DB storage.
/// Local copy to avoid a cross-crate dependency on boom-quota.
fn decimal_to_micros(d: Decimal) -> i64 {
    use rust_decimal::prelude::ToPrimitive;
    (d * Decimal::from(1_000_000)).to_i64().unwrap_or(0)
}

/// Convert integer micros (1e-6 USD) back to Decimal USD.
fn micros_to_decimal(micros: i64) -> Decimal {
    Decimal::from(micros.max(0)) / Decimal::from(1_000_000)
}

/// Re-export of `boom_core::types::PlanType` — single source of truth.
pub use boom_core::types::PlanType;

/// Re-export of `boom_core::types::WindowLimit` — single source of truth.
pub use boom_core::types::WindowLimit;

/// A rate limit plan definition.
///
/// A plan is a **generic template** of limits — fields carry no `key_` /
/// `team_` prefix because the same plan can be assigned to either entity.
/// The `type` field only gates assignment: `type=team` plans can only be
/// assigned to teams (typically because they carry larger quotas that would
/// be dangerous to leak to a single key), `type=key` plans only to keys.
///
/// `window_limits` is multi-dimensional — see [`WindowLimit`]. The shorthand
/// `rpm_limit` / `tpm_limit` / `cost_limit` fields are 1-minute-window
/// conveniences that get merged into the effective `Vec<WindowLimit>` list
/// by `effective_limits` so callers only need to consult one place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitPlan {
    pub name: String,
    #[serde(default)]
    pub r#type: PlanType,
    /// Only used when type=Team. Plan name applied to each member key.
    #[serde(default)]
    pub member_plan: Option<String>,

    #[serde(default)]
    pub concurrency_limit: Option<u32>,
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    #[serde(default)]
    pub tpm_limit: Option<u64>,
    #[serde(default)]
    pub cost_limit: Option<Decimal>,
    #[serde(
        default,
        deserialize_with = "boom_core::types::deserialize_window_limit_vec"
    )]
    pub window_limits: Vec<WindowLimit>,
    #[serde(default)]
    pub total_tpm_limit: Option<u64>,
    #[serde(default)]
    pub total_cost_limit: Option<Decimal>,

    /// Optional time-based schedule overrides.
    #[serde(default)]
    pub schedule: Vec<ScheduleSlot>,
}

impl RateLimitPlan {
    /// Return the effective limits for the current time.
    ///
    /// Returns (concurrency, rpm, active_windows, stale_window_secs).
    /// `stale_window_secs` contains the `window_secs` values from the OTHER
    /// schedule period that should be cleared so the user starts fresh on
    /// every schedule switch.
    ///
    /// `active_windows` already includes the convenience shorthand merged in:
    ///   - `rpm_limit`    → synthetic 60s `counts` entry
    ///   - `tpm_limit`    → synthetic 60s `tokens` entry
    ///   - `cost_limit`   → synthetic 60s `costs` entry
    ///
    /// Whether the plan applies to a key or a team, the limits are the same —
    /// the caller is responsible for picking the right plan (and the right
    /// concurrency counter namespace) based on `self.r#type`.
    pub fn effective_limits(
        &self,
    ) -> (Option<u32>, Option<u64>, Vec<WindowLimit>, Vec<u64>) {
        for slot in &self.schedule {
            if slot.is_active_now() {
                // Merge: slot fields override plan base, unset fields fall back to base.
                let concurrency = slot.concurrency_limit.or(self.concurrency_limit);
                let rpm = slot.rpm_limit.or(self.rpm_limit);
                let tpm = slot.tpm_limit.or(self.tpm_limit);
                let cost = slot.cost_limit.or(self.cost_limit);
                let windows = if slot.window_limits.is_empty() {
                    self.window_limits.clone()
                } else {
                    slot.window_limits.clone()
                };
                let merged = Self::merge_shorthand(windows, rpm, tpm, cost);
                // Clear base counters whose window_secs doesn't overlap with active.
                let active_secs: Vec<u64> = merged.iter().map(|w| w.window_secs).collect();
                let stale: Vec<u64> = self
                    .window_limits
                    .iter()
                    .map(|w| w.window_secs)
                    .filter(|s| !active_secs.contains(s))
                    .collect();
                return (concurrency, rpm, merged, stale);
            }
        }
        // No schedule active: clear all schedule slot counters that don't overlap with base.
        let base_secs: Vec<u64> = self.window_limits.iter().map(|w| w.window_secs).collect();
        let stale: Vec<u64> = self
            .schedule
            .iter()
            .flat_map(|s| s.window_limits.iter())
            .map(|w| w.window_secs)
            .filter(|s| !base_secs.contains(s))
            .collect();
        let merged = Self::merge_shorthand(
            self.window_limits.clone(),
            self.rpm_limit,
            self.tpm_limit,
            self.cost_limit,
        );
        (
            self.concurrency_limit,
            self.rpm_limit,
            merged,
            stale,
        )
    }

    /// Merge the 1-min shorthand fields (rpm/tpm/cost) into the window list.
    ///
    /// Each shorthand field, when set, becomes a synthetic 60s entry. If a
    /// user-configured 60s entry already covers that dimension, the
    /// user-configured entry wins (we don't overwrite it) — the shorthand is
    /// only a fallback for the 60s window when no explicit 60s entry exists
    /// for that dimension.
    fn merge_shorthand(
        mut windows: Vec<WindowLimit>,
        rpm: Option<u64>,
        tpm: Option<u64>,
        cost: Option<Decimal>,
    ) -> Vec<WindowLimit> {
        // Find whether there's a 60s entry that already covers each dimension.
        let has_60s_counts = windows
            .iter()
            .any(|w| w.window_secs == 60 && w.counts.is_some());
        let has_60s_tokens = windows
            .iter()
            .any(|w| w.window_secs == 60 && w.tokens.is_some());
        let has_60s_costs = windows
            .iter()
            .any(|w| w.window_secs == 60 && w.costs.is_some());

        let need_new_60s = rpm.is_some() && !has_60s_counts
            || tpm.is_some() && !has_60s_tokens
            || cost.is_some() && !has_60s_costs;
        if !need_new_60s {
            return windows;
        }

        // Find or create a 60s entry to host the missing shorthand dimensions.
        let mut entry_idx = windows.iter().position(|w| w.window_secs == 60);
        if entry_idx.is_none() {
            windows.push(WindowLimit {
                counts: None,
                tokens: None,
                costs: None,
                window_secs: 60,
            });
            entry_idx = Some(windows.len() - 1);
        }
        let idx = entry_idx.unwrap();
        if !has_60s_counts {
            windows[idx].counts = rpm;
        }
        if !has_60s_tokens {
            windows[idx].tokens = tpm;
        }
        if !has_60s_costs {
            windows[idx].costs = cost;
        }
        windows
    }
}

/// A time-based schedule slot within a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleSlot {
    /// Time range, e.g. "9:00-21:00" or "21:00-9:00" (cross-midnight).
    pub hours: String,
    #[serde(default)]
    pub concurrency_limit: Option<u32>,
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    #[serde(default)]
    pub tpm_limit: Option<u64>,
    #[serde(default)]
    pub cost_limit: Option<Decimal>,
    #[serde(
        default,
        deserialize_with = "boom_core::types::deserialize_window_limit_vec"
    )]
    pub window_limits: Vec<WindowLimit>,
}

impl ScheduleSlot {
    /// Check whether this slot is currently active (UTC+8 Beijing time).
    pub fn is_active_now(&self) -> bool {
        let (start_min, end_min) = match parse_hours(&self.hours) {
            Some(pair) => pair,
            None => return false,
        };
        let now = chrono::Utc::now().with_timezone(
            &chrono::FixedOffset::east_opt(8 * 3600).unwrap(),
        );
        let current_min = now.hour() * 60 + now.minute();
        if start_min <= end_min {
            // Same day: e.g. 9:00-21:00
            current_min >= start_min && current_min < end_min
        } else {
            // Cross-midnight: e.g. 21:00-9:00 → [21:00, 24:00) ∪ [0:00, 9:00)
            current_min >= start_min || current_min < end_min
        }
    }
}

/// Parse a time range string like "9:00-21:00" into (start_minutes, end_minutes).
fn parse_hours(s: &str) -> Option<(u32, u32)> {
    let (start, end) = s.split_once('-')?;
    Some((parse_hm(start.trim())?, parse_hm(end.trim())?))
}

/// Parse "H:MM" or "HH:MM" into minutes since midnight.
fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    Some(h.parse::<u32>().ok()? * 60 + m.parse::<u32>().ok()?)
}

/// In-memory store for rate limit plans and key/team assignments.
/// Survives config reloads.
#[derive(Debug)]
pub struct PlanStore {
    plans: DashMap<String, RateLimitPlan>,
    key_assignments: DashMap<String, String>,
    team_assignments: DashMap<String, String>,
    key_concurrency_counters: DashMap<String, Arc<AtomicU32>>,
    team_concurrency_counters: DashMap<String, Arc<AtomicU32>>,
    default_plan_name: std::sync::Mutex<Option<String>>,
    default_team_plan_name: std::sync::Mutex<Option<String>>,
}

impl PlanStore {
    pub fn new() -> Self {
        Self {
            plans: DashMap::new(),
            key_assignments: DashMap::new(),
            team_assignments: DashMap::new(),
            key_concurrency_counters: DashMap::new(),
            team_concurrency_counters: DashMap::new(),
            default_plan_name: std::sync::Mutex::new(None),
            default_team_plan_name: std::sync::Mutex::new(None),
        }
    }

    /// Set the default plan name (called during config load / reload).
    pub fn set_default_plan(&self, name: Option<String>) {
        let mut guard = self.default_plan_name.lock().unwrap();
        *guard = name;
    }

    /// Get the default plan name (raw string, for comparison purposes).
    pub fn get_default_plan_name(&self) -> Option<String> {
        self.default_plan_name.lock().unwrap().clone()
    }

    /// Get the default plan (if configured and the plan actually exists).
    /// Mutex is released before reading the plans DashMap to prevent ABBA
    /// with `clear_plans` (which writes plans then takes Mutex).
    pub fn get_default_plan(&self) -> Option<RateLimitPlan> {
        let name = {
            let guard = self.default_plan_name.lock().unwrap();
            guard.clone()
        }?;
        self.plans.get(&name).map(|r| r.value().clone())
    }

    /// Set the default team plan name (called during config load / reload).
    pub fn set_default_team_plan(&self, name: Option<String>) {
        let mut guard = self.default_team_plan_name.lock().unwrap();
        *guard = name;
    }

    /// Get the default team plan name (raw string).
    pub fn get_default_team_plan_name(&self) -> Option<String> {
        self.default_team_plan_name.lock().unwrap().clone()
    }

    /// Get the default team plan (if configured and the plan actually exists).
    pub fn get_default_team_plan(&self) -> Option<RateLimitPlan> {
        let name = {
            let guard = self.default_team_plan_name.lock().unwrap();
            guard.clone()
        }?;
        self.plans.get(&name).map(|r| r.value().clone())
    }

    /// Resolve the plan assigned to a key.
    pub fn resolve_plan(&self, key_hash: &str) -> Option<RateLimitPlan> {
        let plan_name = self.key_assignments.get(key_hash)?;
        let plan = self.plans.get(plan_name.value())?;
        Some(plan.value().clone())
    }

    /// Resolve the plan assigned to a team.
    /// Falls back to default_team_plan when the team has no explicit assignment.
    pub fn resolve_team_plan(&self, team_id: &str) -> Option<RateLimitPlan> {
        if let Some(plan_name) = self.team_assignments.get(team_id) {
            if let Some(plan) = self.plans.get(plan_name.value()) {
                return Some(plan.value().clone());
            }
        }
        self.get_default_team_plan()
    }

    /// Get the plan name assigned to a key (for display purposes).
    pub fn get_plan_name(&self, key_hash: &str) -> Option<String> {
        self.key_assignments.get(key_hash).map(|n| n.value().clone())
    }

    /// Get the plan name assigned to a team (for display purposes).
    pub fn get_team_plan_name(&self, team_id: &str) -> Option<String> {
        self.team_assignments.get(team_id).map(|n| n.value().clone())
    }

    /// Get the effective plan name for a team — explicit assignment if any,
    /// otherwise the default_team_plan. Returns None when neither is set.
    pub fn get_team_plan_name_effective(&self, team_id: &str) -> Option<String> {
        self.get_team_plan_name(team_id)
            .or_else(|| self.get_default_team_plan_name())
    }

    /// Try to acquire a concurrency slot for a key.
    /// Returns a guard that decrements on drop, or None if limit exceeded.
    pub fn try_acquire(&self, key_hash: &str, limit: u32) -> Option<ConcurrencyGuard> {
        Self::try_acquire_inner(&self.key_concurrency_counters, key_hash, limit)
    }

    /// Try to acquire a concurrency slot for a team.
    pub fn try_acquire_team(&self, team_id: &str, limit: u32) -> Option<ConcurrencyGuard> {
        Self::try_acquire_inner(&self.team_concurrency_counters, team_id, limit)
    }

    fn try_acquire_inner(
        counters: &DashMap<String, Arc<AtomicU32>>,
        id: &str,
        limit: u32,
    ) -> Option<ConcurrencyGuard> {
        let counter_ref = counters
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(AtomicU32::new(0)));
        let counter = counter_ref.value().clone();
        let prev = counter.fetch_add(1, Ordering::Relaxed);
        if prev >= limit {
            counter.fetch_sub(1, Ordering::Relaxed);
            None
        } else {
            Some(ConcurrencyGuard { counter })
        }
    }

    // ── Plan CRUD ──────────────────────────────────────────────

    pub fn upsert_plan(&self, plan: RateLimitPlan) {
        let name = plan.name.clone();
        self.plans.insert(name, plan);
    }

    pub fn get_plan(&self, name: &str) -> Option<RateLimitPlan> {
        self.plans.get(name).map(|r| r.value().clone())
    }

    pub fn list_plans(&self) -> Vec<RateLimitPlan> {
        self.plans.iter().map(|r| r.value().clone()).collect()
    }

    /// Delete a plan and remove all key/team assignments referencing it.
    /// In-flight concurrency counters are untouched — they drain naturally
    /// as guards are dropped.
    pub fn delete_plan(&self, name: &str) -> bool {
        self.key_assignments
            .retain(|_, plan_name| plan_name != name);
        self.team_assignments
            .retain(|_, plan_name| plan_name != name);
        self.plans.remove(name).is_some()
    }

    // ── Key assignment CRUD ────────────────────────────────────

    /// Assign a key to a plan. Rejects if the plan's type=Team — team plans
    /// cannot be assigned to individual keys. The caller (dashboard) should
    /// catch this and fall back to default_plan with a warning.
    pub fn assign_key(&self, key_hash: &str, plan_name: &str) -> Result<(), String> {
        let plan = self
            .plans
            .get(plan_name)
            .map(|r| r.value().clone())
            .ok_or_else(|| format!("Plan '{}' not found", plan_name))?;
        if plan.r#type == PlanType::Team {
            return Err(format!(
                "Plan '{}' is type=team, cannot be assigned to key '{}'. \
                 Use a type=key plan or default_plan.",
                plan_name, key_hash
            ));
        }
        self.key_assignments
            .insert(key_hash.to_string(), plan_name.to_string());
        Ok(())
    }

    pub fn unassign_key(&self, key_hash: &str) -> bool {
        self.key_assignments.remove(key_hash).is_some()
    }

    pub fn list_assignments(&self) -> Vec<(String, String)> {
        self.key_assignments
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    /// Read the current concurrency count for a key.
    pub fn get_concurrency(&self, key_hash: &str) -> u32 {
        self.key_concurrency_counters
            .get(key_hash)
            .map(|c| c.value().load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Read the current concurrency count for a team.
    pub fn get_team_concurrency(&self, team_id: &str) -> u32 {
        self.team_concurrency_counters
            .get(team_id)
            .map(|c| c.value().load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    // ── Team assignment CRUD ───────────────────────────────────

    /// Assign a team to a plan. Rejects if the plan's type=Key.
    pub fn assign_team(&self, team_id: &str, plan_name: &str) -> Result<(), String> {
        let plan = self
            .plans
            .get(plan_name)
            .map(|r| r.value().clone())
            .ok_or_else(|| format!("Plan '{}' not found", plan_name))?;
        if plan.r#type == PlanType::Key {
            return Err(format!(
                "Plan '{}' is type=key, cannot be assigned to team '{}'. \
                 Use a type=team plan.",
                plan_name, team_id
            ));
        }
        self.team_assignments
            .insert(team_id.to_string(), plan_name.to_string());
        Ok(())
    }

    pub fn unassign_team(&self, team_id: &str) -> bool {
        self.team_assignments.remove(team_id).is_some()
    }

    pub fn list_team_assignments(&self) -> Vec<(String, String)> {
        self.team_assignments
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    // ── Persistence helpers ───────────────────────────────────

    /// Snapshot all key→plan assignments for DB persistence.
    pub fn snapshot_assignments(&self) -> Vec<(String, String)> {
        self.key_assignments
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    /// Snapshot all team→plan assignments for DB persistence.
    pub fn snapshot_team_assignments(&self) -> Vec<(String, String)> {
        self.team_assignments
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    /// Restore a single key→plan assignment from DB into memory.
    /// Called at startup. Does NOT validate plan existence (plan may be loaded later).
    pub fn restore_assignment(&self, key_hash: &str, plan_name: &str) {
        self.key_assignments
            .insert(key_hash.to_string(), plan_name.to_string());
    }

    /// Restore a single team→plan assignment from DB into memory.
    pub fn restore_team_assignment(&self, team_id: &str, plan_name: &str) {
        self.team_assignments
            .insert(team_id.to_string(), plan_name.to_string());
    }

    /// Remove an assignment from memory only (DB deletion handled separately).
    /// Returns true if the assignment existed.
    pub fn remove_assignment_persisted(&self, key_hash: &str) -> bool {
        self.key_assignments.remove(key_hash).is_some()
    }

    /// Remove a team assignment from memory only (DB deletion handled separately).
    pub fn remove_team_assignment_persisted(&self, team_id: &str) -> bool {
        self.team_assignments.remove(team_id).is_some()
    }

    /// Clear all plan definitions and default_plan/default_team_plan, but keep
    /// key/team assignments and concurrency counters intact. Used during hot-reload.
    pub fn clear_plans(&self) {
        self.plans.clear();
        let mut guard = self.default_plan_name.lock().unwrap();
        *guard = None;
        let mut team_guard = self.default_team_plan_name.lock().unwrap();
        *team_guard = None;
    }

    /// Remove assignments pointing to plans that no longer exist.
    /// Call after reloading plans to clean up orphaned entries.
    pub fn cleanup_assignments(&self) {
        self.key_assignments
            .retain(|_, plan_name| self.plans.contains_key(plan_name));
        self.team_assignments
            .retain(|_, plan_name| self.plans.contains_key(plan_name));
    }

    /// Remove concurrency entries with count==0 to free memory.
    /// Returns the count of removed entries.
    pub fn cleanup_concurrency(&self) -> usize {
        let before = self.key_concurrency_counters.len() + self.team_concurrency_counters.len();
        self.key_concurrency_counters.retain(|_, counter| {
            counter.load(Ordering::Relaxed) > 0
        });
        self.team_concurrency_counters.retain(|_, counter| {
            counter.load(Ordering::Relaxed) > 0
        });
        let after = self.key_concurrency_counters.len() + self.team_concurrency_counters.len();
        before - after
    }
}

impl Default for PlanStore {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════
// DB operations (boom_limiter owns plan + assignment tables)
// ═══════════════════════════════════════════════════════════

/// Row for plan snapshot queries.
///
/// Schema assumption: `boom_rate_limit_plan` has a single unprefixed set of
/// limit columns (a plan is a generic template — `type` only gates which
/// entity it can be assigned to). The old `key_*` / `team_*` columns were
/// dropped/renamed by `plan_alter_ddl`.
#[derive(Debug, sqlx::FromRow)]
pub struct PlanRow {
    pub name: String,
    pub r#type: Option<String>,
    pub member_plan: Option<String>,
    pub concurrency_limit: Option<i32>,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub cost_limit_micros: Option<i64>,
    pub window_limits: serde_json::Value,
    pub total_tpm_limit: Option<i64>,
    pub total_cost_limit_micros: Option<i64>,
    pub schedule: serde_json::Value,
    pub is_default: Option<bool>,
}

impl PlanStore {
    /// Sync YAML plans to DB: delete source='yaml', delete conflicts, insert, cleanup orphans.
    pub async fn sync_yaml_to_db(
        pool: &sqlx::PgPool,
        yaml_plans: &[(String, &RateLimitPlan)],
        default_plan_name: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        // 1. Delete all source='yaml' rows.
        sqlx::query(r#"DELETE FROM boom_rate_limit_plan WHERE source = 'yaml'"#)
            .execute(pool)
            .await?;

        // 2. Delete source='db' plans that conflict with YAML names.
        if !yaml_plans.is_empty() {
            let yaml_names: Vec<String> = yaml_plans.iter().map(|(n, _)| n.clone()).collect();
            let result = sqlx::query(
                r#"DELETE FROM boom_rate_limit_plan WHERE source = 'db' AND name = ANY($1)"#,
            )
            .bind(&yaml_names)
            .execute(pool)
            .await?;
            if result.rows_affected() > 0 {
                tracing::info!("Removed {} conflicting source='db' plan(s)", result.rows_affected());
            }
        }

        // 3. Insert YAML plans.
        for (name, pc) in yaml_plans {
            let wl = serde_json::to_value(&pc.window_limits).unwrap_or(serde_json::json!([]));
            let schedule_json = serde_json::to_value(
                pc.schedule.iter().map(|s| serde_json::json!({
                    "hours": s.hours,
                    "concurrency_limit": s.concurrency_limit,
                    "rpm_limit": s.rpm_limit,
                    "tpm_limit": s.tpm_limit,
                    "cost_limit": s.cost_limit,
                    "window_limits": s.window_limits,
                })).collect::<Vec<_>>(),
            ).unwrap_or(serde_json::json!([]));
            let is_default = default_plan_name == Some(name.as_str());
            let type_str = match pc.r#type { PlanType::Key => "key", PlanType::Team => "team" };

            sqlx::query(
                r#"INSERT INTO boom_rate_limit_plan
                   (name, type, member_plan,
                    concurrency_limit, rpm_limit, tpm_limit, cost_limit_micros,
                    window_limits, total_tpm_limit, total_cost_limit_micros,
                    schedule, is_default, source)
                   VALUES ($1, $2, $3,
                           $4, $5, $6, $7,
                           $8, $9, $10,
                           $11, $12, 'yaml')"#,
            )
            .bind(name)
            .bind(type_str)
            .bind(&pc.member_plan)
            .bind(pc.concurrency_limit.map(|v| v as i32))
            .bind(pc.rpm_limit.map(|v| v as i64))
            .bind(pc.tpm_limit.map(|v| v as i64))
            .bind(pc.cost_limit.map(decimal_to_micros))
            .bind(&wl)
            .bind(pc.total_tpm_limit.map(|v| v as i64))
            .bind(pc.total_cost_limit.map(decimal_to_micros))
            .bind(&schedule_json)
            .bind(is_default)
            .execute(pool)
            .await?;
        }

        tracing::info!("Synced {} plan(s) from YAML to DB", yaml_plans.len());

        // 4. Clean up orphaned assignments (key + team).
        sqlx::query(
            r#"DELETE FROM boom_key_plan_assignment
               WHERE plan_name NOT IN (SELECT name FROM boom_rate_limit_plan)"#,
        )
        .execute(pool)
        .await?;
        sqlx::query(
            r#"DELETE FROM boom_team_plan_assignment
               WHERE plan_name NOT IN (SELECT name FROM boom_rate_limit_plan)"#,
        )
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Load source='db' plans from DB into memory.
    pub async fn load_db_only_plans(&self, pool: &sqlx::PgPool) {
        let rows: Vec<PlanRow> = match sqlx::query_as::<_, PlanRow>(
            r#"SELECT name, type, member_plan,
                      concurrency_limit, rpm_limit, tpm_limit, cost_limit_micros,
                      window_limits, total_tpm_limit, total_cost_limit_micros,
                      schedule, is_default
               FROM boom_rate_limit_plan WHERE source = 'db'"#,
        )
        .fetch_all(pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to load DB-only plans: {}", e);
                return;
            }
        };

        for row in &rows {
            let plan = row_to_plan(row);
            self.upsert_plan(plan);
        }

        tracing::info!("Loaded {} DB-only plan(s)", rows.len());
    }

    /// Restore key→plan assignments from DB.
    pub async fn restore_assignments_from_db(&self, pool: &sqlx::PgPool) {
        match sqlx::query_as::<_, (String, String)>(
            r#"SELECT key_hash, plan_name FROM boom_key_plan_assignment"#,
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                let count = rows.len();
                for (key_hash, plan_name) in rows {
                    self.restore_assignment(&key_hash, &plan_name);
                }
                tracing::info!("Restored {} key→plan assignment(s) from DB", count);
            }
            Err(e) => {
                tracing::error!("Failed to restore assignments: {}", e);
            }
        }
    }

    /// Restore team→plan assignments from DB.
    pub async fn restore_team_assignments_from_db(&self, pool: &sqlx::PgPool) {
        match sqlx::query_as::<_, (String, String)>(
            r#"SELECT team_id, plan_name FROM boom_team_plan_assignment"#,
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                let count = rows.len();
                for (team_id, plan_name) in rows {
                    self.restore_team_assignment(&team_id, &plan_name);
                }
                tracing::info!("Restored {} team→plan assignment(s) from DB", count);
            }
            Err(e) => {
                tracing::error!("Failed to restore team assignments: {}", e);
            }
        }
    }

    /// Upsert a plan in DB (source='db') and update memory.
    pub async fn upsert_plan_db(&self, pool: &sqlx::PgPool, plan: &RateLimitPlan) -> Result<(), sqlx::Error> {
        let wl = serde_json::to_value(&plan.window_limits).unwrap_or(serde_json::json!([]));
        let schedule_json = serde_json::to_value(
            plan.schedule.iter().map(|s| serde_json::json!({
                "hours": s.hours,
                "concurrency_limit": s.concurrency_limit,
                "rpm_limit": s.rpm_limit,
                "tpm_limit": s.tpm_limit,
                "cost_limit": s.cost_limit,
                "window_limits": s.window_limits,
            })).collect::<Vec<_>>(),
        ).unwrap_or(serde_json::json!([]));

        let name = &plan.name;
        let type_str = match plan.r#type { PlanType::Key => "key", PlanType::Team => "team" };

        boom_core::gaussdb_upsert!(
            pool,
            || sqlx::query(
                r#"UPDATE boom_rate_limit_plan
                   SET type = $2, member_plan = $3,
                       concurrency_limit = $4, rpm_limit = $5,
                       tpm_limit = $6, cost_limit_micros = $7,
                       window_limits = $8,
                       total_tpm_limit = $9, total_cost_limit_micros = $10,
                       schedule = $11, source = 'db', updated_at = NOW()
                   WHERE name = $1"#,
            )
            .bind(name)
            .bind(type_str)
            .bind(&plan.member_plan)
            .bind(plan.concurrency_limit.map(|v| v as i32))
            .bind(plan.rpm_limit.map(|v| v as i64))
            .bind(plan.tpm_limit.map(|v| v as i64))
            .bind(plan.cost_limit.map(decimal_to_micros))
            .bind(&wl)
            .bind(plan.total_tpm_limit.map(|v| v as i64))
            .bind(plan.total_cost_limit.map(decimal_to_micros))
            .bind(&schedule_json),
            || sqlx::query(
                r#"INSERT INTO boom_rate_limit_plan
                   (name, type, member_plan,
                    concurrency_limit, rpm_limit, tpm_limit, cost_limit_micros,
                    window_limits, total_tpm_limit, total_cost_limit_micros,
                    schedule, is_default, source)
                   VALUES ($1, $2, $3,
                           $4, $5, $6, $7,
                           $8, $9, $10,
                           $11, false, 'db')"#,
            )
            .bind(name)
            .bind(type_str)
            .bind(&plan.member_plan)
            .bind(plan.concurrency_limit.map(|v| v as i32))
            .bind(plan.rpm_limit.map(|v| v as i64))
            .bind(plan.tpm_limit.map(|v| v as i64))
            .bind(plan.cost_limit.map(decimal_to_micros))
            .bind(&wl)
            .bind(plan.total_tpm_limit.map(|v| v as i64))
            .bind(plan.total_cost_limit.map(decimal_to_micros))
            .bind(&schedule_json)
        )?;

        self.upsert_plan(plan.clone());
        Ok(())
    }

    /// Delete a plan from DB and memory.
    pub async fn delete_plan_db(&self, pool: &sqlx::PgPool, name: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(r#"DELETE FROM boom_rate_limit_plan WHERE name = $1"#)
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() > 0 {
            self.delete_plan(name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Assign a key to a plan in DB and memory.
    /// Writes DB first, then updates memory — avoids concurrent dirty reads on failure.
    pub async fn assign_key_db(&self, pool: &sqlx::PgPool, key_hash: &str, plan_name: &str) -> Result<(), String> {
        // GaussDB-compatible upsert: UPDATE → INSERT → UPDATE (no ON CONFLICT support).
        let updated = sqlx::query(
            r#"UPDATE boom_key_plan_assignment SET plan_name = $2 WHERE key_hash = $1"#,
        )
        .bind(key_hash)
        .bind(plan_name)
        .execute(pool)
        .await
        .map_err(|e| format!("DB error: {}", e))?;
        if updated.rows_affected() == 0 {
            if let Err(_) = sqlx::query(
                r#"INSERT INTO boom_key_plan_assignment (key_hash, plan_name, assigned_at)
                   VALUES ($1, $2, NOW())"#,
            )
            .bind(key_hash)
            .bind(plan_name)
            .execute(pool)
            .await
            {
                sqlx::query(
                    r#"UPDATE boom_key_plan_assignment SET plan_name = $2 WHERE key_hash = $1"#,
                )
                .bind(key_hash)
                .bind(plan_name)
                .execute(pool)
                .await
                .map_err(|e| format!("DB error: {}", e))?;
            }
        }

        // DB succeeded — now safe to update memory.
        self.assign_key(key_hash, plan_name)?;
        Ok(())
    }

    /// Unassign a key from its plan in DB and memory.
    pub async fn unassign_key_db(&self, pool: &sqlx::PgPool, key_hash: &str) -> Result<bool, String> {
        let result = sqlx::query(r#"DELETE FROM boom_key_plan_assignment WHERE key_hash = $1"#)
            .bind(key_hash)
            .execute(pool)
            .await
            .map_err(|e| format!("DB error: {}", e))?;

        if result.rows_affected() > 0 {
            self.unassign_key(key_hash);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Assign a team to a plan in DB and memory.
    pub async fn assign_team_db(&self, pool: &sqlx::PgPool, team_id: &str, plan_name: &str) -> Result<(), String> {
        let updated = sqlx::query(
            r#"UPDATE boom_team_plan_assignment SET plan_name = $2 WHERE team_id = $1"#,
        )
        .bind(team_id)
        .bind(plan_name)
        .execute(pool)
        .await
        .map_err(|e| format!("DB error: {}", e))?;
        if updated.rows_affected() == 0 {
            if let Err(_) = sqlx::query(
                r#"INSERT INTO boom_team_plan_assignment (team_id, plan_name, assigned_at)
                   VALUES ($1, $2, NOW())"#,
            )
            .bind(team_id)
            .bind(plan_name)
            .execute(pool)
            .await
            {
                sqlx::query(
                    r#"UPDATE boom_team_plan_assignment SET plan_name = $2 WHERE team_id = $1"#,
                )
                .bind(team_id)
                .bind(plan_name)
                .execute(pool)
                .await
                .map_err(|e| format!("DB error: {}", e))?;
            }
        }
        self.assign_team(team_id, plan_name)?;
        Ok(())
    }

    /// Unassign a team from its plan in DB and memory.
    pub async fn unassign_team_db(&self, pool: &sqlx::PgPool, team_id: &str) -> Result<bool, String> {
        let result = sqlx::query(r#"DELETE FROM boom_team_plan_assignment WHERE team_id = $1"#)
            .bind(team_id)
            .execute(pool)
            .await
            .map_err(|e| format!("DB error: {}", e))?;
        if result.rows_affected() > 0 {
            self.unassign_team(team_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Sync in-memory key assignments to DB (periodic background task).
    pub async fn sync_assignments_to_db(&self, pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
        let assignments = self.snapshot_assignments();
        for (key_hash, plan_name) in &assignments {
            boom_core::gaussdb_upsert!(
                pool,
                || sqlx::query(
                    r#"UPDATE boom_key_plan_assignment SET plan_name = $2 WHERE key_hash = $1"#,
                )
                .bind(key_hash)
                .bind(plan_name),
                || sqlx::query(
                    r#"INSERT INTO boom_key_plan_assignment (key_hash, plan_name, assigned_at)
                       VALUES ($1, $2, NOW())"#,
                )
                .bind(key_hash)
                .bind(plan_name)
            )?;
        }
        Ok(())
    }

    /// Sync in-memory team assignments to DB (periodic background task).
    pub async fn sync_team_assignments_to_db(&self, pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
        let assignments = self.snapshot_team_assignments();
        for (team_id, plan_name) in &assignments {
            boom_core::gaussdb_upsert!(
                pool,
                || sqlx::query(
                    r#"UPDATE boom_team_plan_assignment SET plan_name = $2 WHERE team_id = $1"#,
                )
                .bind(team_id)
                .bind(plan_name),
                || sqlx::query(
                    r#"INSERT INTO boom_team_plan_assignment (team_id, plan_name, assigned_at)
                       VALUES ($1, $2, NOW())"#,
                )
                .bind(team_id)
                .bind(plan_name)
            )?;
        }
        Ok(())
    }

    /// Snapshot all plans from DB (for config export).
    pub async fn snapshot_plans_db(pool: &sqlx::PgPool) -> Result<Vec<PlanRow>, sqlx::Error> {
        sqlx::query_as::<_, PlanRow>(
            r#"SELECT name, type, member_plan,
                      concurrency_limit, rpm_limit, tpm_limit, cost_limit_micros,
                      window_limits, total_tpm_limit, total_cost_limit_micros,
                      schedule, is_default
               FROM boom_rate_limit_plan ORDER BY name"#,
        )
        .fetch_all(pool)
        .await
    }
}

/// Convert a DB plan row to a RateLimitPlan.
///
/// Backward compat: if `type` column is NULL (pre-migration DB), defaults to `Key`.
fn row_to_plan(row: &PlanRow) -> RateLimitPlan {
    let plan_type = match row.r#type.as_deref() {
        Some("team") => PlanType::Team,
        _ => PlanType::Key,
    };
    RateLimitPlan {
        name: row.name.clone(),
        r#type: plan_type,
        member_plan: row.member_plan.clone(),
        concurrency_limit: row.concurrency_limit.map(|v| v as u32),
        rpm_limit: row.rpm_limit.map(|v| v as u64),
        tpm_limit: row.tpm_limit.map(|v| v as u64),
        cost_limit: row.cost_limit_micros.map(micros_to_decimal),
        window_limits: parse_window_limits(&row.window_limits),
        total_tpm_limit: row.total_tpm_limit.map(|v| v as u64),
        total_cost_limit: row.total_cost_limit_micros.map(micros_to_decimal),
        schedule: parse_schedule(&row.schedule),
    }
}

fn parse_window_limits(value: &serde_json::Value) -> Vec<WindowLimit> {
    // The DB column stores the serde-serialized form of `Vec<WindowLimit>`.
    // We round-trip via the same custom deserializer the YAML path uses, so
    // both compact-array and verbose-object forms survive a write-then-read.
    // Old rows written as `[[count, window_secs]]` (legacy 2-element form)
    // won't fit the 4-element array helper; fall back to interpreting them
    // as `(counts=count, window_secs)` entries with the other dims None.
    let arr = match value.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Some(a) = item.as_array() {
            // Compact array form.
            match a.len() {
                4 => {
                    // [counts, tokens, costs, window_secs] — null for unused dims.
                    let counts = a[0].as_u64();
                    let tokens = a[1].as_u64();
                    let costs = a[2]
                        .as_str()
                        .and_then(|s| Decimal::from_str(s).ok())
                        .or_else(|| a[2].as_f64().and_then(Decimal::from_f64));
                    let window_secs = a[3].as_u64().unwrap_or(60);
                    out.push(WindowLimit {
                        counts,
                        tokens,
                        costs,
                        window_secs,
                    });
                }
                2 => {
                    // Legacy 2-element form: [count, window_secs].
                    if let (Some(c), Some(s)) = (a[0].as_u64(), a[1].as_u64()) {
                        out.push(WindowLimit {
                            counts: Some(c),
                            tokens: None,
                            costs: None,
                            window_secs: s,
                        });
                    }
                }
                _ => continue,
            }
        } else if let Some(obj) = item.as_object() {
            // Verbose object form.
            let counts = obj.get("counts").and_then(|v| v.as_u64());
            let tokens = obj.get("tokens").and_then(|v| v.as_u64());
            let costs = obj
                .get("costs")
                .and_then(|v| v.as_str())
                .and_then(|s| Decimal::from_str(s).ok())
                .or_else(|| {
                    obj.get("costs")
                        .and_then(|v| v.as_f64())
                        .and_then(Decimal::from_f64)
                });
            let window_secs = obj.get("window_secs").and_then(|v| v.as_u64()).unwrap_or(60);
            out.push(WindowLimit {
                counts,
                tokens,
                costs,
                window_secs,
            });
        }
    }
    out
}

fn parse_schedule(value: &serde_json::Value) -> Vec<ScheduleSlot> {
    value.as_array().map(|arr| {
        arr.iter().filter_map(|item| {
            let obj = item.as_object()?;
            let empty_arr = serde_json::Value::Array(vec![]);
            let conc = obj.get("concurrency_limit")
                .and_then(|v| v.as_u64()).map(|v| v as u32);
            let rpm = obj.get("rpm_limit")
                .and_then(|v| v.as_u64());
            let tpm = obj.get("tpm_limit").and_then(|v| v.as_u64());
            let cost = obj.get("cost_limit")
                .and_then(|v| v.as_str())
                .and_then(|s| Decimal::from_str(s).ok())
                .or_else(|| {
                    obj.get("cost_limit")
                        .and_then(|v| v.as_f64())
                        .and_then(Decimal::from_f64)
                });
            let wl = parse_window_limits(obj.get("window_limits").unwrap_or(&empty_arr));
            Some(ScheduleSlot {
                hours: obj.get("hours")?.as_str()?.to_string(),
                concurrency_limit: conc,
                rpm_limit: rpm,
                tpm_limit: tpm,
                cost_limit: cost,
                window_limits: wl,
            })
        }).collect()
    }).unwrap_or_default()
}

// ────────────────────────────────────────────────────────────
// ConcurrencyGuard — RAII auto-decrement
// ────────────────────────────────────────────────────────────

/// RAII guard that decrements the concurrency counter on drop.
pub struct ConcurrencyGuard {
    counter: Arc<AtomicU32>,
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

// ────────────────────────────────────────────────────────────
// GuardedStream — drops guard when stream ends
// ────────────────────────────────────────────────────────────

/// Stream wrapper that holds a concurrency guard.
/// When the stream ends (returns `None`) or is dropped (client disconnect),
/// the guard is released automatically.
pub struct GuardedStream<S> {
    inner: S,
    guard: Option<ConcurrencyGuard>,
}

impl<S> GuardedStream<S> {
    pub fn new(inner: S, guard: Option<ConcurrencyGuard>) -> Self {
        Self {
            inner,
            guard,
        }
    }
}

impl<S: Stream + Unpin> Stream for GuardedStream<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Safety: GuardedStream<S> is Unpin when S: Unpin (all fields are Unpin).
        let this = self.get_mut();

        // Safety: S: Unpin.
        let result = Pin::new(&mut this.inner).poll_next(cx);

        if matches!(result, Poll::Ready(None)) {
            // Stream finished — release the guard.
            this.guard.take();
        }
        result
    }
}
