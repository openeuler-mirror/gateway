use chrono::Timelike;
use dashmap::DashMap;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

/// A rate limit plan definition.
///
/// Plans define concurrency and sliding-window limits that can be
/// assigned to API keys via the admin API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitPlan {
    pub name: String,
    #[serde(default)]
    pub concurrency_limit: Option<u32>,
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    #[serde(default)]
    pub window_limits: Vec<(u64, u64)>,
    /// Optional time-based schedule overrides.
    #[serde(default)]
    pub schedule: Vec<ScheduleSlot>,
}

impl RateLimitPlan {
    /// Return the effective limits for the current time.
    ///
    /// Returns (concurrency, rpm, active_windows, stale_windows).
    /// `stale_windows` contains counters from the OTHER schedule period that
    /// should be cleared so the user starts fresh on every schedule switch.
    pub fn effective_limits(&self) -> (Option<u32>, Option<u64>, Vec<(u64, u64)>, Vec<(u64, u64)>) {
        for slot in &self.schedule {
            if slot.is_active_now() {
                // Merge: slot fields override plan base, unset fields fall back to base.
                let concurrency = slot.concurrency_limit.or(self.concurrency_limit);
                let rpm = slot.rpm_limit.or(self.rpm_limit);
                let windows = if slot.window_limits.is_empty() {
                    self.window_limits.clone()
                } else {
                    slot.window_limits.clone()
                };
                // Clear base counters whose window_secs doesn't overlap with active.
                let active_secs: Vec<u64> = windows.iter().map(|&(_, s)| s).collect();
                let stale: Vec<(u64, u64)> = self.window_limits.iter()
                    .filter(|(_, s)| !active_secs.contains(s))
                    .copied()
                    .collect();
                return (concurrency, rpm, windows, stale);
            }
        }
        // No schedule active: clear all schedule slot counters that don't overlap with base.
        let base_secs: Vec<u64> = self.window_limits.iter().map(|&(_, s)| s).collect();
        let stale: Vec<(u64, u64)> = self.schedule.iter()
            .flat_map(|s| s.window_limits.iter())
            .filter(|(_, s)| !base_secs.contains(s))
            .copied()
            .collect();
        (
            self.concurrency_limit,
            self.rpm_limit,
            self.window_limits.clone(),
            stale,
        )
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
    pub window_limits: Vec<(u64, u64)>,
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

/// In-memory store for rate limit plans and key assignments.
/// Survives config reloads.
#[derive(Debug)]
pub struct PlanStore {
    plans: DashMap<String, RateLimitPlan>,
    key_assignments: DashMap<String, String>,
    concurrency_counters: DashMap<String, Arc<AtomicU32>>,
    default_plan_name: std::sync::Mutex<Option<String>>,
}

impl PlanStore {
    pub fn new() -> Self {
        Self {
            plans: DashMap::new(),
            key_assignments: DashMap::new(),
            concurrency_counters: DashMap::new(),
            default_plan_name: std::sync::Mutex::new(None),
        }
    }

    /// Set the default plan name (called during config load / reload).
    pub fn set_default_plan(&self, name: Option<String>) {
        let mut guard = self.default_plan_name.lock().unwrap();
        *guard = name;
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

    /// Resolve the plan assigned to a key.
    pub fn resolve_plan(&self, key_hash: &str) -> Option<RateLimitPlan> {
        let plan_name = self.key_assignments.get(key_hash)?;
        let plan = self.plans.get(plan_name.value())?;
        Some(plan.value().clone())
    }

    /// Get the plan name assigned to a key (for display purposes).
    pub fn get_plan_name(&self, key_hash: &str) -> Option<String> {
        self.key_assignments.get(key_hash).map(|n| n.value().clone())
    }

    /// Try to acquire a concurrency slot for a key.
    /// Returns a guard that decrements on drop, or None if limit exceeded.
    pub fn try_acquire(&self, key_hash: &str, limit: u32) -> Option<ConcurrencyGuard> {
        let counter_ref = self
            .concurrency_counters
            .entry(key_hash.to_string())
            .or_insert_with(|| Arc::new(AtomicU32::new(0)));

        let counter = counter_ref.value().clone();
        let prev = counter.fetch_add(1, Ordering::Relaxed);

        if prev >= limit {
            // Over limit — roll back.
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

    /// Delete a plan and remove all key assignments referencing it.
    /// In-flight concurrency counters are untouched — they drain naturally
    /// as guards are dropped.
    pub fn delete_plan(&self, name: &str) -> bool {
        self.key_assignments
            .retain(|_, plan_name| plan_name != name);
        self.plans.remove(name).is_some()
    }

    // ── Key assignment CRUD ────────────────────────────────────

    pub fn assign_key(&self, key_hash: &str, plan_name: &str) -> Result<(), String> {
        if !self.plans.contains_key(plan_name) {
            return Err(format!("Plan '{}' not found", plan_name));
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
        self.concurrency_counters
            .get(key_hash)
            .map(|c| c.value().load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    // ── Persistence helpers ───────────────────────────────────

    /// Snapshot all key→plan assignments for DB persistence.
    pub fn snapshot_assignments(&self) -> Vec<(String, String)> {
        self.key_assignments
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

    /// Remove an assignment from memory only (DB deletion handled separately).
    /// Returns true if the assignment existed.
    pub fn remove_assignment_persisted(&self, key_hash: &str) -> bool {
        self.key_assignments.remove(key_hash).is_some()
    }

    /// Clear all plan definitions and default_plan, but keep key assignments
    /// and concurrency counters intact. Used during hot-reload.
    pub fn clear_plans(&self) {
        self.plans.clear();
        let mut guard = self.default_plan_name.lock().unwrap();
        *guard = None;
    }

    /// Remove assignments pointing to plans that no longer exist.
    /// Call after reloading plans to clean up orphaned entries.
    pub fn cleanup_assignments(&self) {
        self.key_assignments
            .retain(|_, plan_name| self.plans.contains_key(plan_name));
    }

    /// Remove concurrency entries with count==0 to free memory.
    /// Returns the count of removed entries.
    pub fn cleanup_concurrency(&self) -> usize {
        let before = self.concurrency_counters.len();
        self.concurrency_counters.retain(|_, counter| {
            counter.load(Ordering::Relaxed) > 0
        });
        before - self.concurrency_counters.len()
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
#[derive(Debug, sqlx::FromRow)]
pub struct PlanRow {
    pub name: String,
    pub concurrency_limit: Option<i32>,
    pub rpm_limit: Option<i64>,
    pub window_limits: serde_json::Value,
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
            let window_limits_json = serde_json::to_value(&pc.window_limits).unwrap_or(serde_json::json!([]));
            let schedule_json = serde_json::to_value(
                pc.schedule.iter().map(|s| serde_json::json!({
                    "hours": s.hours,
                    "concurrency_limit": s.concurrency_limit,
                    "rpm_limit": s.rpm_limit,
                    "window_limits": s.window_limits,
                })).collect::<Vec<_>>(),
            ).unwrap_or(serde_json::json!([]));
            let is_default = default_plan_name == Some(name.as_str());

            sqlx::query(
                r#"INSERT INTO boom_rate_limit_plan
                   (name, concurrency_limit, rpm_limit, window_limits, schedule, is_default, source)
                   VALUES ($1, $2, $3, $4, $5, $6, 'yaml')"#,
            )
            .bind(name)
            .bind(pc.concurrency_limit.map(|v| v as i32))
            .bind(pc.rpm_limit.map(|v| v as i64))
            .bind(&window_limits_json)
            .bind(&schedule_json)
            .bind(is_default)
            .execute(pool)
            .await?;
        }

        tracing::info!("Synced {} plan(s) from YAML to DB", yaml_plans.len());

        // 4. Clean up orphaned assignments.
        sqlx::query(
            r#"DELETE FROM boom_key_plan_assignment
               WHERE plan_name NOT IN (SELECT name FROM boom_rate_limit_plan)"#,
        )
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Load source='db' plans from DB into memory.
    pub async fn load_db_only_plans(&self, pool: &sqlx::PgPool) {
        let rows: Vec<PlanRow> = match sqlx::query_as::<_, PlanRow>(
            r#"SELECT name, concurrency_limit, rpm_limit, window_limits, schedule, is_default
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

    /// Upsert a plan in DB (source='db') and update memory.
    pub async fn upsert_plan_db(&self, pool: &sqlx::PgPool, plan: &RateLimitPlan) -> Result<(), sqlx::Error> {
        let window_limits_json = serde_json::to_value(&plan.window_limits).unwrap_or(serde_json::json!([]));
        let schedule_json = serde_json::to_value(
            plan.schedule.iter().map(|s| serde_json::json!({
                "hours": s.hours,
                "concurrency_limit": s.concurrency_limit,
                "rpm_limit": s.rpm_limit,
                "window_limits": s.window_limits,
            })).collect::<Vec<_>>(),
        ).unwrap_or(serde_json::json!([]));

        sqlx::query(
            r#"INSERT INTO boom_rate_limit_plan (name, concurrency_limit, rpm_limit, window_limits, schedule, is_default, source)
               VALUES ($1, $2, $3, $4, $5, false, 'db')
               ON CONFLICT (name) DO UPDATE
               SET concurrency_limit = EXCLUDED.concurrency_limit,
                   rpm_limit = EXCLUDED.rpm_limit,
                   window_limits = EXCLUDED.window_limits,
                   schedule = EXCLUDED.schedule,
                   source = 'db',
                   updated_at = NOW()"#,
        )
        .bind(&plan.name)
        .bind(plan.concurrency_limit.map(|v| v as i32))
        .bind(plan.rpm_limit.map(|v| v as i64))
        .bind(&window_limits_json)
        .bind(&schedule_json)
        .execute(pool)
        .await?;

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
    pub async fn assign_key_db(&self, pool: &sqlx::PgPool, key_hash: &str, plan_name: &str) -> Result<(), String> {
        self.assign_key(key_hash, plan_name)?;

        if let Err(e) = sqlx::query(
            r#"INSERT INTO boom_key_plan_assignment (key_hash, plan_name, assigned_at)
               VALUES ($1, $2, NOW())
               ON CONFLICT (key_hash) DO UPDATE
               SET plan_name = EXCLUDED.plan_name"#,
        )
        .bind(key_hash)
        .bind(plan_name)
        .execute(pool)
        .await
        {
            // Roll back memory on DB failure.
            self.unassign_key(key_hash);
            return Err(format!("DB error: {}", e));
        }
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

    /// Sync in-memory assignments to DB (periodic background task).
    pub async fn sync_assignments_to_db(&self, pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
        let assignments = self.snapshot_assignments();
        for (key_hash, plan_name) in &assignments {
            sqlx::query(
                r#"INSERT INTO boom_key_plan_assignment (key_hash, plan_name, assigned_at)
                   VALUES ($1, $2, NOW())
                   ON CONFLICT (key_hash) DO UPDATE
                   SET plan_name = EXCLUDED.plan_name"#,
            )
            .bind(key_hash)
            .bind(plan_name)
            .execute(pool)
            .await?;
        }
        Ok(())
    }

    /// Snapshot all plans from DB (for config export).
    pub async fn snapshot_plans_db(pool: &sqlx::PgPool) -> Result<Vec<PlanRow>, sqlx::Error> {
        sqlx::query_as::<_, PlanRow>(
            r#"SELECT name, concurrency_limit, rpm_limit, window_limits, schedule, is_default
               FROM boom_rate_limit_plan ORDER BY name"#,
        )
        .fetch_all(pool)
        .await
    }
}

/// Convert a DB plan row to a RateLimitPlan.
fn row_to_plan(row: &PlanRow) -> RateLimitPlan {
    RateLimitPlan {
        name: row.name.clone(),
        concurrency_limit: row.concurrency_limit.map(|v| v as u32),
        rpm_limit: row.rpm_limit.map(|v| v as u64),
        window_limits: parse_window_limits(&row.window_limits),
        schedule: parse_schedule(&row.schedule),
    }
}

fn parse_window_limits(value: &serde_json::Value) -> Vec<(u64, u64)> {
    value.as_array().map(|arr| {
        arr.iter().filter_map(|item| {
            let a = item.as_array()?;
            if a.len() >= 2 { Some((a[0].as_u64()?, a[1].as_u64()?)) } else { None }
        }).collect()
    }).unwrap_or_default()
}

fn parse_schedule(value: &serde_json::Value) -> Vec<ScheduleSlot> {
    value.as_array().map(|arr| {
        arr.iter().filter_map(|item| {
            let obj = item.as_object()?;
            Some(ScheduleSlot {
                hours: obj.get("hours")?.as_str()?.to_string(),
                concurrency_limit: obj.get("concurrency_limit").and_then(|v| v.as_u64()).map(|v| v as u32),
                rpm_limit: obj.get("rpm_limit").and_then(|v| v.as_u64()),
                window_limits: parse_window_limits(obj.get("window_limits")?),
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
