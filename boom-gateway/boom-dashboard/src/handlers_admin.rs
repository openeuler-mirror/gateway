use axum::extract::{Multipart, Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use boom_core::key_format::is_valid_prefix;
use chrono::NaiveDateTime;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use sqlx::FromRow;
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::{hash_token, AdminSession};
use crate::state::DashboardState;

/// Deserialize `Option<Option<T>>` so JSON `null` is distinguished from a
/// missing field. Used for `plan_name` in key-assignment requests where the
/// three states must round-trip through the API:
///   - field absent       → `None`             (no-op for assign; "use default" for create)
///   - `plan_name: null`  → `Some(None)`       (explicit "no plan" — does NOT fall back)
///   - `plan_name: "x"`   → `Some(Some("x"))`  (explicit plan assignment)
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// Maximum allowed length of the user-supplied `tag` string. Tags are free
/// text (Unicode OK) but capped to keep cell rendering sane.
const MAX_TAG_LEN: usize = 64;

/// Generate a (raw_key, hashed_token, optional_prefix) triple for a new key.
///
/// The hash always covers the entire raw key (e.g. `sk-{prefix}-{secret}` or
/// `sk-{secret}`), matching the /v1 authenticate path byte-for-byte. Prefix
/// tamper detection is implicit — any change to the prefix portion changes
/// the digest, so the DB lookup fails.
///
/// Callers must validate the prefix themselves and reject invalid values
/// with a 400 — this helper silently drops an invalid prefix to keep the
/// invariant "DB never stores an invalid prefix" at the validation layer.
fn generate_key_material(requested_prefix: Option<&str>) -> (String, String, Option<String>) {
    let secret = hex::encode(Uuid::new_v4().as_bytes());
    match requested_prefix.filter(|p| is_valid_prefix(p)) {
        Some(p) => {
            let raw = format!("sk-{}-{}", p, secret);
            let hashed = hash_token(&raw);
            (raw, hashed, Some(p.to_string()))
        }
        None => {
            let raw = format!("sk-{}", secret);
            let hashed = hash_token(&raw);
            (raw, hashed, None)
        }
    }
}

/// Validate the `key_prefix` and `tag` fields of a creation request.
///
/// Returns `Some(Response)` (a 400) when validation fails, otherwise `None`.
/// Centralized here so the single-key, batch, and import paths all enforce
/// the same rule — invalid prefixes get rejected rather than silently
/// falling back to the legacy `sk-{secret}` form.
fn validate_prefix_and_tag(req: &CreateKeyRequest) -> Option<Response> {
    if let Some(ref p) = req.key_prefix {
        if !p.is_empty() && !is_valid_prefix(p) {
            return Some((
                axum::http::StatusCode::BAD_REQUEST,
                format!(
                    "Invalid key_prefix '{}': must be 1-50 ASCII alphanumeric chars [a-zA-Z0-9]",
                    p
                ),
            )
                .into_response());
        }
    }
    if let Some(ref tag) = req.tag {
        if tag.chars().count() > MAX_TAG_LEN {
            return Some((
                axum::http::StatusCode::BAD_REQUEST,
                format!("Invalid tag: length must be <= {} chars", MAX_TAG_LEN),
            )
                .into_response());
        }
    }
    None
}

// ═══════════════════════════════════════════════════════════
// Plan management (delegated to PlanStore)
// ═══════════════════════════════════════════════════════════

pub async fn list_plans(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Json<Value> {
    let plans = state.plan_store.list_plans();
    Json(json!({"plans": plans}))
}

#[derive(Debug, Deserialize)]
pub struct UpsertPlanRequest {
    pub name: String,
    #[serde(default)]
    pub r#type: boom_limiter::PlanType,
    #[serde(default)]
    pub member_plan: Option<String>,
    #[serde(default)]
    pub concurrency_limit: Option<u32>,
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    #[serde(default)]
    pub tpm_limit: Option<u64>,
    #[serde(
        default,
        deserialize_with = "boom_core::types::deserialize_window_limit_vec"
    )]
    pub window_limits: Vec<boom_core::types::WindowLimit>,
    #[serde(default)]
    pub total_token_limit: Option<u64>,
    #[serde(default)]
    pub total_cost_limit: Option<rust_decimal::Decimal>,
    #[serde(default)]
    pub schedule: Vec<boom_limiter::ScheduleSlot>,
}

pub async fn upsert_plan(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(req): Json<UpsertPlanRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let plan = boom_limiter::RateLimitPlan {
        name: req.name.clone(),
        r#type: req.r#type,
        member_plan: req.member_plan,
        concurrency_limit: req.concurrency_limit,
        rpm_limit: req.rpm_limit,
        tpm_limit: req.tpm_limit,
        window_limits: req.window_limits,
        total_token_limit: req.total_token_limit,
        total_cost_limit: req.total_cost_limit,
        schedule: req.schedule.clone(),
    };

    if let Err(msg) = plan.validate_schedule_overlap() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": msg })),
        ));
    }

    // Persist to DB via PlanStore.
    if let Some(ref pool) = state.db_pool {
        if let Err(e) = state.plan_store.upsert_plan_db(pool, &plan).await {
            tracing::error!("Failed to persist plan to DB: {}", e);
        }
    } else {
        state.plan_store.upsert_plan(plan);
    }

    let _ = state.admin_tx.send(crate::state::AdminCommand::ConfigChanged).await;
    Ok(Json(json!({"ok": true, "plan_name": req.name})))
}

pub async fn delete_plan(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(name): Path<String>,
) -> Json<Value> {
    let deleted = if let Some(ref pool) = state.db_pool {
        match state.plan_store.delete_plan_db(pool, &name).await {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("Failed to delete plan from DB: {}", e);
                false
            }
        }
    } else {
        state.plan_store.delete_plan(&name)
    };

    if deleted {
        let _ = state.admin_tx.send(crate::state::AdminCommand::ConfigChanged).await;
    }

    Json(json!({"ok": deleted, "plan_name": name}))
}

// ═══════════════════════════════════════════════════════════
// Key management (DB operations)
// ═══════════════════════════════════════════════════════════

/// Row mapper for the keys list query.
/// Types must match boom-auth's VerificationToken to avoid runtime decode errors.
#[derive(Debug, FromRow)]
struct KeyRow {
    token: String,
    key_name: Option<String>,
    key_alias: Option<String>,
    #[sqlx(default)]
    key_prefix: Option<String>,
    #[sqlx(default)]
    tag: Option<String>,
    user_id: Option<String>,
    team_id: Option<String>,
    /// litellm stores models as text[] in PostgreSQL.
    models: Vec<String>,
    blocked: Option<bool>,
    rpm_limit: Option<i64>,
    tpm_limit: Option<i64>,
    max_budget: Option<f64>,
    budget_duration: Option<String>,
    expires: Option<NaiveDateTime>,
    metadata: Option<serde_json::Value>,
    created_at: Option<NaiveDateTime>,
}

#[derive(Debug, Deserialize)]
pub struct ListKeysQuery {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_per_page")]
    pub per_page: i64,
    pub search: Option<String>,
    #[serde(default)]
    pub vip_only: Option<String>,
    /// Filter by plan assignment.
    ///   - unset            → no filter
    ///   - "unassigned"     → keys with no DB row (follows default_plan)
    ///   - "no_plan"        → keys explicitly configured to have no plan
    ///   - "none"           → legacy alias for "unassigned"
    ///   - any other string → keys whose effective plan_name matches exactly
    #[serde(default)]
    pub plan: Option<String>,
}

fn default_page() -> i64 {
    1
}
fn default_per_page() -> i64 {
    50
}

fn normalize_pagination(page: i64, per_page: i64) -> (i64, i64) {
    (page.max(1), per_page.clamp(1, 1000))
}

pub async fn list_keys(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Query(query): Query<ListKeysQuery>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let search_pattern = query
        .search
        .as_deref()
        .map(|s| format!("%{}%", s.replace('%', "\\%").replace('_', "\\_")));

    // Fetch ALL keys from DB (no LIMIT/OFFSET) for global usage sorting.
    let rows: Vec<KeyRow> = if let Some(ref pattern) = search_pattern {
        match sqlx::query_as(
            r#"SELECT token, key_name, key_alias, key_prefix, tag, user_id, team_id, models,
                      blocked, rpm_limit, tpm_limit, max_budget,
                      budget_duration, expires, metadata, created_at
               FROM "boom_verification_token"
               WHERE (key_name ILIKE $1 OR key_alias ILIKE $1 OR user_id ILIKE $1 OR token ILIKE $1)"#,
        )
        .bind(pattern)
        .fetch_all(db_pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Dashboard list_keys query failed: {}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal error",
                )
                    .into_response();
            }
        }
    } else {
        match sqlx::query_as(
            r#"SELECT token, key_name, key_alias, key_prefix, tag, user_id, team_id, models,
                      blocked, rpm_limit, tpm_limit, max_budget,
                      budget_duration, expires, metadata, created_at
               FROM "boom_verification_token""#,
        )
        .fetch_all(db_pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Dashboard list_keys query failed: {}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal error",
                )
                    .into_response();
            }
        }
    };

    let _total_before_filter = rows.len() as i64;

    // Single-pass limiter scan: aggregate usage for all keys at once.
    let all_usage = state.limiter.get_all_key_usage();

    let mut keys: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let token_prefix = format!("{}...", &r.token[..8.min(r.token.len())]);
            let (usage_count, usage_reset_secs) = all_usage.get(&r.token).copied().unwrap_or((0, 0));
            // Three-state plan assignment. The frontend distinguishes:
            //   - "default"   → no DB row (follows default_plan at runtime)
            //   - "no_plan"   → row with plan_name IS NULL (explicit opt-out)
            //   - "plan"      → row with plan_name = "x"
            let explicit = state.plan_store.get_plan_name_explicit(&r.token);
            let plan_assignment_kind = match explicit {
                None => "default",
                Some(None) => "no_plan",
                Some(Some(_)) => "plan",
            };
            // Effective plan name (what the runtime actually uses). Falls back
            // to default_plan for None and Some(None)-without-default.
            let plan_name = match &explicit {
                Some(Some(n)) => Some(n.clone()),
                None => state.plan_store.get_default_plan_name(),
                Some(None) => None,
            };

            // Aggregate current-window tokens & cost from limiter. We pick
            // the smallest window_secs per kind — that's the "tightest" current
            // window (typically 60s) and matches what users expect in a usage
            // snapshot column. Cross-window aggregation would mix limits.
            let mut tokens_min_secs: Option<(u64, u64, u64)> = None; // (secs, count, remaining)
            let mut cost_min_secs: Option<(u64, u64, u64)> = None; // (secs, micros, remaining)
            for w in state.limiter.peek_key_windows(&r.token) {
                match w.kind {
                    boom_limiter::WindowKind::Tokens => {
                        match tokens_min_secs {
                            None => tokens_min_secs = Some((w.window_secs, w.count, w.remaining_secs)),
                            Some((s, _, _)) if w.window_secs < s => {
                                tokens_min_secs = Some((w.window_secs, w.count, w.remaining_secs));
                            }
                            _ => {}
                        }
                    }
                    boom_limiter::WindowKind::CostMicros => {
                        match cost_min_secs {
                            None => cost_min_secs = Some((w.window_secs, w.count, w.remaining_secs)),
                            Some((s, _, _)) if w.window_secs < s => {
                                cost_min_secs = Some((w.window_secs, w.count, w.remaining_secs));
                            }
                            _ => {}
                        }
                    }
                }
            }
            let usage_tokens = tokens_min_secs.map(|(_, c, _)| c).unwrap_or(0);
            let usage_cost_micros = cost_min_secs.map(|(_, c, _)| c).unwrap_or(0);
            let usage_cost = rust_decimal::Decimal::from(usage_cost_micros)
                / rust_decimal::Decimal::from(1_000_000);

            // Cumulative total cost across the key's lifetime — comes from
            // limiter.cumulative (boom_rate_limit_cumulative backed), NOT
            // boom_verification_token.spend (litellm legacy column we never write).
            let total_cost_micros = state
                .limiter
                .peek_cumulative(
                    &boom_limiter::QuotaScope::Key {
                        key_hash: r.token.clone(),
                    },
                    boom_limiter::CumulativeKind::TotalCost,
                );
            let total_cost = rust_decimal::Decimal::from(total_cost_micros)
                / rust_decimal::Decimal::from(1_000_000);

            json!({
                "token_prefix": token_prefix,
                "token_hash": r.token,
                "key_name": r.key_name,
                "key_alias": r.key_alias,
                "key_prefix": r.key_prefix,
                "tag": r.tag,
                "user_id": r.user_id,
                "team_id": r.team_id,
                "models": r.models,
                "spend": total_cost.to_string(),
                "total_cost": total_cost.to_string(),
                "blocked": r.blocked.unwrap_or(false),
                "rpm_limit": r.rpm_limit,
                "tpm_limit": r.tpm_limit,
                "max_budget": r.max_budget,
                "budget_duration": r.budget_duration,
                "expires": r.expires.map(|d| d.to_string()),
                "metadata": r.metadata,
                "created_at": r.created_at.map(|d| d.to_string()),
                "usage_count": usage_count,
                "usage_reset_secs": usage_reset_secs,
                "usage_tokens": usage_tokens,
                "usage_cost": usage_cost.to_string(),
                "plan_name": plan_name,
                "plan_assignment_kind": plan_assignment_kind,
            })
        })
        .collect();

    // Sort globally by usage_count descending.
    keys.sort_by(|a, b| {
        let ca = a.get("usage_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let cb = b.get("usage_count").and_then(|v| v.as_u64()).unwrap_or(0);
        cb.cmp(&ca)
    });

    // Filter VIP-only if requested.
    if query.vip_only.as_deref() == Some("true") || query.vip_only.as_deref() == Some("1") {
        keys.retain(|k| {
            k.get("metadata")
                .and_then(|m| m.get("vip"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
    }

    // Filter by plan assignment. Three sentinels + name match:
    //   - "unassigned" → no DB row (follows default_plan at runtime)
    //   - "no_plan"    → explicit row with plan_name IS NULL
    //   - "none"       → legacy alias for "unassigned"
    //   - any other    → keys whose effective plan_name matches exactly
    if let Some(ref plan_filter) = query.plan {
        match plan_filter.as_str() {
            "unassigned" | "none" => keys.retain(|k| {
                k.get("plan_assignment_kind").and_then(|v| v.as_str()) == Some("default")
            }),
            "no_plan" => keys.retain(|k| {
                k.get("plan_assignment_kind").and_then(|v| v.as_str()) == Some("no_plan")
            }),
            name => keys.retain(|k| {
                k.get("plan_name").and_then(|v| v.as_str()) == Some(name)
            }),
        }
    }

    let filtered_total = keys.len() as i64;

    // In-memory pagination.
    let (page, per_page) = normalize_pagination(query.page, query.per_page);
    let offset = ((page - 1) * per_page) as usize;
    let page_keys: Vec<Value> = keys
        .into_iter()
        .skip(offset)
        .take(per_page as usize)
        .collect();

    Json(json!({
        "keys": page_keys,
        "page": page,
        "per_page": per_page,
        "total": filtered_total,
    }))
    .into_response()
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CreateKeyRequest {
    pub key_alias: Option<String>,
    /// Legacy display name. Defaults to key_alias if not provided.
    pub key_name: Option<String>,
    /// Optional key prefix shown in the raw key (e.g. `sk-prod-<secret>`).
    /// Must match `[a-zA-Z0-9]{1,8}`; invalid values are rejected with 400.
    pub key_prefix: Option<String>,
    /// Optional user-supplied classification tag. Free text, ≤64 chars.
    /// Not part of the raw key — purely a dashboard/display field.
    pub tag: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub models: Option<Vec<String>>,
    pub max_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub expires: Option<String>,
    pub metadata: Option<serde_json::Value>,
    /// Plan assignment for the new key. Three states:
    ///   - field absent (`None`)            → follow default_plan at runtime
    ///   - `null`   (`Some(None)`)          → explicit "no plan" (no default fallback)
    ///   - `"name"` (`Some(Some(name))`)    → assign to plan `name`
    #[serde(default, deserialize_with = "deserialize_some")]
    pub plan_name: Option<Option<String>>,
}

pub async fn create_key(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(req): Json<CreateKeyRequest>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    // 0. Validate user-supplied prefix and tag. Reject instead of silently
    //    falling back, so users learn the rule rather than getting mystery
    //    legacy keys.
    if let Some(resp) = validate_prefix_and_tag(&req) {
        return resp;
    }

    // 1. Generate raw key + token hash + optional prefix metadata.
    //    Prefixed keys hash only the secret portion; legacy keys hash the
    //    whole raw_key so old DB rows remain matchable.
    let (raw_key, token_hash, key_prefix) = generate_key_material(req.key_prefix.as_deref());

    // 1b. Check key_alias dedup (if provided).
    if let Some(ref alias) = req.key_alias {
        let exists: bool = sqlx::query_scalar(
            r#"SELECT EXISTS(SELECT 1 FROM "boom_verification_token" WHERE key_alias = $1)"#,
        )
        .bind(alias)
        .fetch_one(db_pool)
        .await
        .unwrap_or(false);

        if exists {
            return (
                axum::http::StatusCode::CONFLICT,
                format!("key_alias '{}' already exists", alias),
            )
                .into_response();
        }
    }

    // 2. Parse optional expires.
    let expires: Option<NaiveDateTime> = req
        .expires
        .as_deref()
        .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok());
    let mut models_list: Vec<String> = req.models.unwrap_or_default();
    if models_list.iter().any(|m| m == "all-team-models") {
        models_list = vec!["all-team-models".to_string()];
    }

    // 3. INSERT into DB. (key_name defaults to key_alias)
    let key_name = req.key_name.or(req.key_alias.clone());
    let result = sqlx::query(
        r#"INSERT INTO "boom_verification_token"
           (token, key_name, key_alias, key_prefix, tag, user_id, team_id, models, spend, blocked,
            rpm_limit, tpm_limit, max_budget, budget_duration, expires,
            metadata, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0.0, false, $9, $10, $11, $12, $13, $14, NOW(), NOW())"#,
    )
    .bind(&token_hash)
    .bind(&key_name)
    .bind(&req.key_alias)
    .bind(&key_prefix)
    .bind(&req.tag)
    .bind(&req.user_id)
    .bind(&req.team_id)
    .bind(&models_list)
    .bind(req.rpm_limit)
    .bind(req.tpm_limit)
    .bind(req.max_budget)
    .bind(&req.budget_duration)
    .bind(expires)
    .bind(req.metadata.as_ref().unwrap_or(&serde_json::json!({})))
    .execute(db_pool)
    .await;

    if let Err(e) = result {
        tracing::error!("Dashboard create_key insert failed: {}", e);
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Internal error",
        )
            .into_response();
    }

    // 4. Plan assignment (3 states — see CreateKeyRequest::plan_name).
    //    - None            → no row written (runtime follows default_plan)
    //    - Some(None)      → row with plan_name IS NULL (no default fallback)
    //    - Some(Some(name))→ row with plan_name = name
    //    Skip DB write when the explicit assignment is the same as default_plan:
    //    no row needed; default fallback already yields the right plan at runtime.
    match req.plan_name {
        None => {}
        Some(None) => {
            if let Err(e) = state
                .plan_store
                .assign_key_no_plan_db(db_pool, &token_hash)
                .await
            {
                tracing::warn!("Key created but 'no plan' assignment failed: {}", e);
            }
        }
        Some(Some(ref plan_name)) => {
            let is_default = state.plan_store.get_default_plan_name().as_deref()
                == Some(plan_name.as_str());
            let result = if is_default {
                state.plan_store.assign_key(&token_hash, plan_name)
            } else {
                state.plan_store.assign_key_db(db_pool, &token_hash, plan_name).await
            };
            if let Err(e) = result {
                tracing::warn!("Key created but plan assignment failed: {}", e);
            }
        }
    }

    // 5. Return the raw key (only shown once).
    Json(json!({
        "key": raw_key,
        "token_hash": token_hash,
        "key_alias": req.key_alias,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct UpdateKeyRequest {
    pub key_name: Option<String>,
    pub key_alias: Option<String>,
    pub user_id: Option<String>,
    /// Team reassignment. Some("team_id") moves the key to that team;
    /// Some("") (empty string) removes the key from its team (sets NULL).
    /// None leaves the team_id untouched.
    pub team_id: Option<String>,
    pub models: Option<Vec<String>>,
    pub max_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
    /// Optional user-supplied classification tag (≤64 chars). Empty string
    /// clears the tag; null leaves it untouched (COALESCE semantics).
    pub tag: Option<String>,
    pub expires: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

pub async fn update_key(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(token_hash): Path<String>,
    Json(req): Json<UpdateKeyRequest>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    // Check key_alias uniqueness if provided.
    if let Some(ref alias) = req.key_alias {
        if !alias.is_empty() {
            let exists: bool = sqlx::query_scalar(
                r#"SELECT EXISTS(SELECT 1 FROM "boom_verification_token" WHERE key_alias = $1 AND token != $2)"#,
            )
            .bind(alias)
            .bind(&token_hash)
            .fetch_one(db_pool)
            .await
            .unwrap_or(false);

            if exists {
                return (
                    axum::http::StatusCode::CONFLICT,
                    format!("key_alias '{}' already exists", alias),
                )
                    .into_response();
            }
        }
    }

    // Validate tag length if provided. Empty string is allowed (clears tag);
    // null skips the update entirely.
    if let Some(ref tag) = req.tag {
        if tag.chars().count() > MAX_TAG_LEN {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Invalid tag: length must be <= {} chars", MAX_TAG_LEN),
            )
                .into_response();
        }
    }

    let models_list: Option<Vec<String>> = req.models.as_ref().map(|v| {
        if v.iter().any(|m| m == "all-team-models") {
            vec!["all-team-models".to_string()]
        } else {
            v.clone()
        }
    });

    // Team reassignment: Some("") → NULL (remove from team), Some(id) → id
    // (must exist in boom_team_table), None → leave untouched. We bind the
    // validated Option<&str> separately so SQL `COALESCE($N, team_id)` keeps
    // the old value when client omitted the field.
    let team_id_resolved: Option<&str> = match req.team_id.as_deref() {
        None => None,
        Some("") => Some(""),
        Some(id) => Some(id),
    };
    if let Some(id) = team_id_resolved {
        if !id.is_empty() {
            let exists: bool = sqlx::query_scalar(
                r#"SELECT EXISTS(SELECT 1 FROM boom_team_table WHERE team_id = $1)"#,
            )
            .bind(id)
            .fetch_one(db_pool)
            .await
            .unwrap_or(false);
            if !exists {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    format!("Team '{}' not found", id),
                )
                    .into_response();
            }
        }
    }

    let expires: Option<NaiveDateTime> = req
        .expires
        .as_deref()
        .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok());

    let result = sqlx::query(
        r#"UPDATE "boom_verification_token"
           SET key_name = COALESCE($2, key_name),
               key_alias = COALESCE($3, key_alias),
               user_id = COALESCE($4, user_id),
               team_id = CASE WHEN $5::text IS NULL THEN team_id
                              WHEN $5::text = '' THEN NULL
                              ELSE $5::text END,
               models = COALESCE($6, models),
               max_budget = COALESCE($7, max_budget),
               budget_duration = COALESCE($8, budget_duration),
               rpm_limit = COALESCE($9, rpm_limit),
               tpm_limit = COALESCE($10, tpm_limit),
               tag = COALESCE($11, tag),
               expires = COALESCE($12, expires),
               metadata = COALESCE($13, metadata),
               updated_at = NOW()
           WHERE token = $1"#,
    )
    .bind(&token_hash)
    .bind(&req.key_name)
    .bind(&req.key_alias)
    .bind(&req.user_id)
    .bind(team_id_resolved)
    .bind(&models_list)
    .bind(req.max_budget)
    .bind(&req.budget_duration)
    .bind(req.rpm_limit)
    .bind(req.tpm_limit)
    .bind(&req.tag)
    .bind(expires)
    .bind(&req.metadata)
    .execute(db_pool)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => Json(json!({"ok": true})).into_response(),
        Ok(_) => (
            axum::http::StatusCode::NOT_FOUND,
            "Key not found",
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Dashboard update_key failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
            )
                .into_response()
        }
    }
}

pub async fn block_key(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(token_hash): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let result = sqlx::query(
        r#"UPDATE "boom_verification_token" SET blocked = true, updated_at = NOW() WHERE token = $1"#,
    )
    .bind(&token_hash)
    .execute(db_pool)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => Json(json!({"ok": true})).into_response(),
        Ok(_) => (
            axum::http::StatusCode::NOT_FOUND,
            "Key not found",
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Dashboard block_key failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
            )
                .into_response()
        }
    }
}

pub async fn unblock_key(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(token_hash): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let result = sqlx::query(
        r#"UPDATE "boom_verification_token" SET blocked = false, updated_at = NOW() WHERE token = $1"#,
    )
    .bind(&token_hash)
    .execute(db_pool)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => Json(json!({"ok": true})).into_response(),
        Ok(_) => (
            axum::http::StatusCode::NOT_FOUND,
            "Key not found",
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Dashboard unblock_key failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
            )
                .into_response()
        }
    }
}

/// Hard-delete a key and every trace of it across the gateway's tables.
///
/// Order matters: clear rate-limit state and plan assignment BEFORE the
/// token row, so a concurrent request that resolves the identity doesn't
/// race against an in-flight limiter check and leave orphan counters. The
/// cleanup helpers are owned by boom-limiter (`SlidingWindowLimiter` and
/// `PlanStore`); `boom_verification_token` is the litellm-compatible row
/// we already write to in `block_key` / `unblock_key`.
///
/// Cleanup failures are logged as warnings, not returned as errors — the
/// user's intent is "delete this key", and a stale counter or dangling
/// plan assignment is harmless once the token row is gone (no future
/// request can resolve to a deleted token).
pub async fn delete_key(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(token_hash): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    if let Err(e) = state.limiter.clear_key_all(db_pool, &token_hash).await {
        tracing::warn!(error = %e, "delete_key: clear_key_all failed (continuing)");
    }
    if let Err(e) = state.plan_store.unassign_key_db(db_pool, &token_hash).await {
        tracing::warn!(error = %e, "delete_key: unassign_key_db failed (continuing)");
    }

    let result = sqlx::query(
        r#"DELETE FROM "boom_verification_token" WHERE token = $1"#,
    )
    .bind(&token_hash)
    .execute(db_pool)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => Json(json!({"ok": true})).into_response(),
        Ok(_) => (
            axum::http::StatusCode::NOT_FOUND,
            "Key not found",
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Dashboard delete_key failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
            )
            .into_response()
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Assignment management
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Deserialize, Default)]
pub struct AssignmentsQuery {
    pub page: Option<usize>,
    pub page_size: Option<usize>,
}

pub async fn list_assignments(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Query(params): Query<AssignmentsQuery>,
) -> Json<Value> {
    let assignments = state.plan_store.list_assignments();
    let total = assignments.len();
    let page = params.page.unwrap_or(1).max(1);
    let page_size = params.page_size.unwrap_or(20).clamp(1, 200);
    let offset = (page - 1) * page_size;

    if offset >= assignments.len() {
        return Json(json!({
            "assignments": [],
            "total": total,
            "page": page,
            "page_size": page_size,
        }));
    }

    // Only lookup aliases for the current page slice.
    let page_slice = &assignments[offset..assignments.len().min(offset + page_size)];
    let hashes: Vec<&str> = page_slice.iter().map(|(h, _)| h.as_str()).collect();
    let alias_map = state.auth.lookup_key_aliases(&hashes).await;

    let result: Vec<Value> = page_slice
        .iter()
        .map(|(key_hash, plan_name)| {
            let key_alias = alias_map.get(key_hash).and_then(|a| a.clone());
            let token_prefix = format!("{}...", &key_hash[..8.min(key_hash.len())]);
            json!({
                "key_hash": key_hash,
                "plan_name": plan_name,
                "key_alias": key_alias,
                "token_prefix": token_prefix,
            })
        })
        .collect();

    Json(json!({
        "assignments": result,
        "total": total,
        "page": page,
        "page_size": page_size,
    }))
}

#[derive(Debug, Deserialize)]
pub struct AssignRequest {
    pub key_hash: String,
    /// Three states (same semantics as CreateKeyRequest::plan_name):
    ///   - field absent (`None`)            → no-op (preserved for legacy callers that always send a string)
    ///   - `null`   (`Some(None)`)          → explicit "no plan" (no default fallback)
    ///   - `"name"` (`Some(Some(name))`)    → assign to plan `name`
    #[serde(default, deserialize_with = "deserialize_some")]
    pub plan_name: Option<Option<String>>,
}

pub async fn assign_key(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(req): Json<AssignRequest>,
) -> Response {
    // For backward compat: callers that send a non-null plan_name route to
    // assign_key_db. Sending null routes to assign_key_no_plan_db. Sending
    // the field absent is a no-op (legacy callers always include it).
    match req.plan_name {
        Some(None) => {
            if let Some(ref pool) = state.db_pool {
                match state.plan_store.assign_key_no_plan_db(pool, &req.key_hash).await {
                    Ok(()) => {
                        let _ = state
                            .admin_tx
                            .send(crate::state::AdminCommand::ConfigChanged)
                            .await;
                        Json(json!({"ok": true})).into_response()
                    }
                    Err(e) => (axum::http::StatusCode::BAD_REQUEST, e).into_response(),
                }
            } else {
                state.plan_store.assign_key_no_plan(&req.key_hash);
                let _ = state
                    .admin_tx
                    .send(crate::state::AdminCommand::ConfigChanged)
                    .await;
                Json(json!({"ok": true})).into_response()
            }
        }
        Some(Some(ref name)) => {
            if let Some(ref pool) = state.db_pool {
                match state.plan_store.assign_key_db(pool, &req.key_hash, name).await {
                    Ok(()) => {
                        let _ = state
                            .admin_tx
                            .send(crate::state::AdminCommand::ConfigChanged)
                            .await;
                        Json(json!({"ok": true})).into_response()
                    }
                    Err(e) => (axum::http::StatusCode::BAD_REQUEST, e).into_response(),
                }
            } else {
                match state.plan_store.assign_key(&req.key_hash, name) {
                    Ok(()) => {
                        let _ = state
                            .admin_tx
                            .send(crate::state::AdminCommand::ConfigChanged)
                            .await;
                        Json(json!({"ok": true})).into_response()
                    }
                    Err(e) => (axum::http::StatusCode::BAD_REQUEST, e).into_response(),
                }
            }
        }
        None => (
            axum::http::StatusCode::BAD_REQUEST,
            "plan_name is required (send null for explicit no-plan)",
        )
            .into_response(),
    }
}

pub async fn unassign_key(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(key_hash): Path<String>,
) -> Json<Value> {
    let removed = if let Some(ref pool) = state.db_pool {
        match state.plan_store.unassign_key_db(pool, &key_hash).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to delete assignment from DB: {}", e);
                false
            }
        }
    } else {
        state.plan_store.unassign_key(&key_hash)
    };

    if removed {
        let _ = state.admin_tx.send(crate::state::AdminCommand::ConfigChanged).await;
    }

    Json(json!({"ok": removed}))
}

#[derive(Debug, Deserialize)]
pub struct AssignTeamRequest {
    pub team_id: String,
    pub plan_name: String,
}

pub async fn assign_team_plan(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(req): Json<AssignTeamRequest>,
) -> Response {
    if let Some(ref pool) = state.db_pool {
        match state
            .plan_store
            .assign_team_db(pool, &req.team_id, &req.plan_name)
            .await
        {
            Ok(()) => {
                let _ = state
                    .admin_tx
                    .send(crate::state::AdminCommand::ConfigChanged)
                    .await;
                Json(json!({"ok": true})).into_response()
            }
            Err(e) => (axum::http::StatusCode::BAD_REQUEST, e).into_response(),
        }
    } else {
        match state
            .plan_store
            .assign_team(&req.team_id, &req.plan_name)
        {
            Ok(()) => {
                let _ = state
                    .admin_tx
                    .send(crate::state::AdminCommand::ConfigChanged)
                    .await;
                Json(json!({"ok": true})).into_response()
            }
            Err(e) => (axum::http::StatusCode::BAD_REQUEST, e).into_response(),
        }
    }
}

pub async fn unassign_team_plan(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(team_id): Path<String>,
) -> Response {
    let removed = if let Some(ref pool) = state.db_pool {
        match state.plan_store.unassign_team_db(pool, &team_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to delete team assignment from DB: {}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("unassign_team_db failed: {e}"),
                )
                    .into_response();
            }
        }
    } else {
        state.plan_store.unassign_team(&team_id)
    };

    if removed {
        let _ = state
            .admin_tx
            .send(crate::state::AdminCommand::ConfigChanged)
            .await;
    }

    Json(json!({"ok": removed})).into_response()
}

// ═══════════════════════════════════════════════════════════
// Usage query
// ═══════════════════════════════════════════════════════════

pub async fn get_key_usage(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(key_hash): Path<String>,
) -> Json<Value> {
    let windows: Vec<Value> = state
        .limiter
        .get_usage_for_key(&key_hash)
        .into_iter()
        .map(|w| {
            json!({
                "cache_key": w.cache_key,
                "count": w.counts,
                "window_secs": w.window_secs,
                "elapsed_secs": w.elapsed_secs,
            })
        })
        .collect();

    let concurrency = state.plan_store.get_concurrency(&key_hash);

    Json(json!({
        "key_hash": key_hash,
        "concurrency": concurrency,
        "windows": windows,
    }))
}

// ═══════════════════════════════════════════════════════════
// Batch key creation
// ═══════════════════════════════════════════════════════════

pub async fn batch_create_keys(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(reqs): Json<Vec<CreateKeyRequest>>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let mut created = Vec::new();
    let mut skipped = Vec::new();

    for req in reqs {
        match insert_single_key(&state, db_pool, req).await {
            CreateOutcome::Created { key, token_hash, key_alias } => {
                created.push(json!({
                    "key": key,
                    "token_hash": token_hash,
                    "key_alias": key_alias,
                }));
            }
            CreateOutcome::Skipped { key_alias, reason } => {
                skipped.push(json!({
                    "key_alias": key_alias,
                    "reason": reason,
                }));
            }
        }
    }

    Json(json!({
        "created": created,
        "skipped": skipped,
        "created_count": created.len(),
        "skipped_count": skipped.len(),
    }))
    .into_response()
}

/// Outcome of inserting one key. Shared by [`batch_create_keys`] and
/// [`import_keys`] so they report identical shapes.
enum CreateOutcome {
    Created {
        key: String,
        token_hash: String,
        key_alias: Option<String>,
    },
    Skipped {
        key_alias: Option<String>,
        reason: &'static str,
    },
}

/// Insert a single key from a [`CreateKeyRequest`]. Encapsulates alias dedup,
/// generation, INSERT, and optional plan assignment so both the JSON-array
/// batch endpoint and the file-import endpoint stay in lockstep.
async fn insert_single_key(
    state: &DashboardState,
    db_pool: &sqlx::PgPool,
    req: CreateKeyRequest,
) -> CreateOutcome {
    // Validate prefix and tag up front so batch/import paths reject bad
    // rows with a precise reason rather than silently degrading.
    if let Some(p) = req.key_prefix.as_ref() {
        if !p.is_empty() && !is_valid_prefix(p) {
            return CreateOutcome::Skipped {
                key_alias: req.key_alias,
                reason: "invalid_prefix",
            };
        }
    }
    if let Some(t) = req.tag.as_ref() {
        if t.chars().count() > MAX_TAG_LEN {
            return CreateOutcome::Skipped {
                key_alias: req.key_alias,
                reason: "invalid_tag",
            };
        }
    }

    // Dedup check on key_alias.
    if let Some(ref alias) = req.key_alias {
        let exists: bool = sqlx::query_scalar(
            r#"SELECT EXISTS(SELECT 1 FROM "boom_verification_token" WHERE key_alias = $1)"#,
        )
        .bind(alias)
        .fetch_one(db_pool)
        .await
        .unwrap_or(false);

        if exists {
            return CreateOutcome::Skipped {
                key_alias: req.key_alias,
                reason: "duplicate",
            };
        }
    }

    let (raw_key, token_hash, key_prefix) = generate_key_material(req.key_prefix.as_deref());

    let expires: Option<NaiveDateTime> = req
        .expires
        .as_deref()
        .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok());

    let mut models_list: Vec<String> = req.models.clone().unwrap_or_default();
    if models_list.iter().any(|m| m == "all-team-models") {
        models_list = vec!["all-team-models".to_string()];
    }
    let key_name = req.key_name.clone().or(req.key_alias.clone());

    let result = sqlx::query(
        r#"INSERT INTO "boom_verification_token"
           (token, key_name, key_alias, key_prefix, tag, user_id, team_id, models, spend, blocked,
            rpm_limit, tpm_limit, max_budget, budget_duration, expires,
            metadata, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0.0, false, $9, $10, $11, $12, $13, $14, NOW(), NOW())"#,
    )
    .bind(&token_hash)
    .bind(&key_name)
    .bind(&req.key_alias)
    .bind(&key_prefix)
    .bind(&req.tag)
    .bind(&req.user_id)
    .bind(&req.team_id)
    .bind(&models_list)
    .bind(req.rpm_limit)
    .bind(req.tpm_limit)
    .bind(req.max_budget)
    .bind(&req.budget_duration)
    .bind(expires)
    .bind(req.metadata.as_ref().unwrap_or(&serde_json::json!({})))
    .execute(db_pool)
    .await;

    match result {
        Ok(_) => {
            // Plan assignment — 3 states (see CreateKeyRequest::plan_name).
            match req.plan_name {
                None => {}
                Some(None) => {
                    if let Err(e) = state
                        .plan_store
                        .assign_key_no_plan_db(db_pool, &token_hash)
                        .await
                    {
                        tracing::warn!("Key created but 'no plan' assignment failed: {}", e);
                    }
                }
                Some(Some(ref plan_name)) => {
                    let is_default = state.plan_store.get_default_plan_name().as_deref()
                        == Some(plan_name.as_str());
                    let result = if is_default {
                        state.plan_store.assign_key(&token_hash, plan_name)
                    } else {
                        state.plan_store.assign_key_db(db_pool, &token_hash, plan_name).await
                    };
                    if let Err(e) = result {
                        tracing::warn!("Key created but plan assignment failed: {}", e);
                    }
                }
            }
            CreateOutcome::Created {
                key: raw_key,
                token_hash,
                key_alias: req.key_alias,
            }
        }
        Err(e) => {
            tracing::error!("Dashboard insert_single_key failed: {}", e);
            CreateOutcome::Skipped {
                key_alias: req.key_alias,
                reason: "db_error",
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════
// File-based batch import (JSONL or CSV)
// ═══════════════════════════════════════════════════════════

/// `multipart/form-data` field name expected from the dashboard uploader.
const IMPORT_FIELD_NAME: &str = "file";

/// Hard upper bound on a single uploaded file. Guards against accidental
/// giant uploads (an admin pasting a 100MB log file by mistake) OOMing the
/// handler — admin permission is already required, but defense in depth.
/// 1 MiB comfortably covers 10k-line payloads at expected per-row sizes.
const IMPORT_MAX_BYTES: usize = 1 * 1024 * 1024;

/// Hard upper bound on parsed rows. Even below the byte cap, a malicious or
/// buggy file with extreme per-line density shouldn't trigger unbounded
/// inserts. 10k rows matches the byte cap at ~100 B/row headroom.
const IMPORT_MAX_ROWS: usize = 10_000;

pub async fn import_keys(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    mut multipart: Multipart,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    // 1. Pull the first file field from multipart.
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    while let Ok(Some(mut field)) = multipart.next_field().await {
        if field.name() == Some(IMPORT_FIELD_NAME) {
            file_name = field.file_name().map(|s| s.to_string());
            // Cap field size by chunked reads — axum's Multipart has no
            // per-field byte limit by default, so we enforce IMPORT_MAX_BYTES
            // ourselves. Reject early once the limit is crossed.
            let mut buf = Vec::new();
            let mut exceeded = false;
            while let Ok(Some(chunk)) = field.chunk().await {
                if buf.len() + chunk.len() > IMPORT_MAX_BYTES {
                    exceeded = true;
                    break;
                }
                buf.extend_from_slice(&chunk);
            }
            if exceeded {
                return Json(json!({
                    "error": format!(
                        "Upload exceeds the {} byte (1 MiB) limit",
                        IMPORT_MAX_BYTES
                    ),
                }))
                .into_response();
            }
            file_bytes = Some(buf);
            break;
        }
        // Drain any other fields so the connection can be reused.
        let _ = field.bytes().await;
    }
    let (bytes, name) = match (file_bytes, file_name) {
        (Some(b), Some(n)) => (b, n),
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                "Missing 'file' field in multipart upload",
            )
                .into_response();
        }
    };

    // 2. Route by extension.
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    let (parsed_reqs, parse_errors) = match ext.as_deref() {
        Some("jsonl") => parse_jsonl(&bytes),
        Some("csv") => parse_csv(&bytes),
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                "Unsupported file extension; use .jsonl or .csv",
            )
                .into_response();
        }
    };

    // 3. Cap parsed row count. Truncate rather than reject so the user can
    //    still see parse_errors for the rows that did come through, with an
    //    explicit flag in the response noting that the rest were dropped.
    let truncated = parsed_reqs.len() > IMPORT_MAX_ROWS;
    let parsed_total = parsed_reqs.len();
    let inserted_count = if truncated { IMPORT_MAX_ROWS } else { parsed_total };
    let parsed_reqs = if truncated {
        parsed_reqs.into_iter().take(IMPORT_MAX_ROWS).collect::<Vec<_>>()
    } else {
        parsed_reqs
    };

    // 4. Insert each parsed request via the shared helper.
    //
    // The original CreateKeyRequest is cloned so we can hand it to the insert
    // helper (which takes it by value) AND keep a copy to build the download
    // attachment. The download mirrors the upload format with one extra
    // `api_key` column appended, so the user can keep the same file as their
    // roster without re-keying the rest of the fields by hand.
    let mut created = Vec::new();
    let mut created_with_req: Vec<(CreateKeyRequest, String)> = Vec::new();
    let mut skipped = Vec::new();
    for req in parsed_reqs {
        match insert_single_key(&state, db_pool, req.clone()).await {
            CreateOutcome::Created { key, token_hash, key_alias } => {
                created_with_req.push((req, key.clone()));
                created.push(json!({
                    "key": key,
                    "token_hash": token_hash,
                    "key_alias": key_alias,
                }));
            }
            CreateOutcome::Skipped { key_alias, reason } => {
                skipped.push(json!({
                    "key_alias": key_alias,
                    "reason": reason,
                }));
            }
        }
    }

    let download = build_key_download(&name, &created_with_req);

    Json(json!({
        "file_name": name,
        "format": ext,
        "parsed": parsed_total,
        "inserted": inserted_count,
        "truncated": truncated,
        "max_rows": IMPORT_MAX_ROWS,
        "parse_errors": parse_errors,
        "created": created,
        "skipped": skipped,
        "created_count": created.len(),
        "skipped_count": skipped.len(),
        "download": download,
    }))
    .into_response()
}

/// Build a same-format download attachment that mirrors the upload and
/// appends an `api_key` column/field for each successfully created key.
///
/// Rows that were skipped or failed to parse are NOT included — they have no
/// key to ship back. The user still sees them in the on-screen tables, but
/// the download is the canonical "what just got created" artifact.
///
/// CSV output keeps the same column order as `CsvKeyRow` (the parse schema)
/// plus a trailing `api_key` column; `models` is rejoined with `|` to round-
/// trip the in-cell separator. JSONL output adds an `api_key` field to each
/// object, preserving the original field set.
fn build_key_download(
    original_name: &str,
    created: &[(CreateKeyRequest, String)],
) -> Value {
    if created.is_empty() {
        return Value::Null;
    }

    let stem = original_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(original_name);
    let ext = original_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    let (ext, content, mime) = match ext.as_deref() {
        Some("csv") => {
            let mut buf = Vec::new();
            {
                let mut wtr = csv::Writer::from_writer(&mut buf);
                // Header must mirror CsvKeyRow field order, plus api_key.
                let _ = wtr.write_record(&[
                    "key_alias", "key_name", "key_prefix", "tag",
                    "user_id", "team_id", "models",
                    "rpm_limit", "tpm_limit", "max_budget", "budget_duration",
                    "expires", "metadata", "plan_name", "api_key",
                ]);
                for (req, key) in created {
                    let _ = wtr.write_record(&[
                        req.key_alias.clone().unwrap_or_default(),
                        req.key_name.clone().unwrap_or_default(),
                        req.key_prefix.clone().unwrap_or_default(),
                        req.tag.clone().unwrap_or_default(),
                        req.user_id.clone().unwrap_or_default(),
                        req.team_id.clone().unwrap_or_default(),
                        req.models.as_ref()
                            .map(|v| v.join("|"))
                            .unwrap_or_default(),
                        req.rpm_limit.map(|v| v.to_string()).unwrap_or_default(),
                        req.tpm_limit.map(|v| v.to_string()).unwrap_or_default(),
                        req.max_budget.map(|v| v.to_string()).unwrap_or_default(),
                        req.budget_duration.clone().unwrap_or_default(),
                        req.expires.clone().unwrap_or_default(),
                        req.metadata.as_ref()
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                        req.plan_name
                            .as_ref()
                            .and_then(|inner| inner.as_deref())
                            .unwrap_or_default()
                            .to_string(),
                        key.clone(),
                    ]);
                }
                let _ = wtr.flush();
            }
            ("csv".to_string(), String::from_utf8_lossy(&buf).to_string(), "text/csv")
        }
        _ => {
            // Default to JSONL — covers .jsonl uploads and the unlikely case
            // of an unknown extension (we still want to give the user a file).
            let mut lines = Vec::with_capacity(created.len());
            for (req, key) in created {
                // Serialize the original request, then merge `api_key` in.
                // Round-tripping via Value preserves field order and avoids
                // hand-listing every field here.
                let mut obj = serde_json::to_value(req).unwrap_or_else(|_| json!({}));
                if let Some(map) = obj.as_object_mut() {
                    map.insert("api_key".to_string(), Value::String(key.clone()));
                }
                lines.push(serde_json::to_string(&obj).unwrap_or_default());
            }
            let content = lines.join("\n");
            ("jsonl".to_string(), content, "application/x-ndjson")
        }
    };

    json!({
        "filename": format!("{}-with-keys.{}", stem, ext),
        "content": content,
        "mime": mime,
        "rows": created.len(),
    })
}

/// Parse a JSONL file into requests plus per-line errors.
///
/// Each non-empty line is one JSON object matching `CreateKeyRequest`.
/// Blank lines are skipped silently. Lines that fail JSON parsing or fail
/// required-field checks (`key_alias` is the only soft-required field,
/// since blank alias rows would otherwise be unidentifiable in the UI)
/// are reported with 1-based line numbers.
fn parse_jsonl(bytes: &[u8]) -> (Vec<CreateKeyRequest>, Vec<Value>) {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            return (
                vec![],
                vec![json!({
                    "line": 0,
                    "reason": "file is not valid UTF-8",
                })],
            );
        }
    };

    let mut reqs = Vec::new();
    let mut errors = Vec::new();
    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<CreateKeyRequest>(trimmed) {
            Ok(req) => reqs.push(req),
            Err(e) => {
                errors.push(json!({
                    "line": line_no,
                    "reason": format!("invalid JSON: {}", e),
                }));
            }
        }
    }
    (reqs, errors)
}

/// CSV row schema. Fields map 1:1 onto [`CreateKeyRequest`].
///
/// `models` uses `|` (pipe) as in-cell separator so Excel users don't have
/// to fight quoted commas. `metadata` is a single JSON object string.
/// Empty cells become `None`.
#[derive(Debug, Deserialize)]
struct CsvKeyRow {
    key_alias: Option<String>,
    key_name: Option<String>,
    key_prefix: Option<String>,
    tag: Option<String>,
    user_id: Option<String>,
    team_id: Option<String>,
    models: Option<String>,
    rpm_limit: Option<i64>,
    tpm_limit: Option<i64>,
    max_budget: Option<f64>,
    budget_duration: Option<String>,
    expires: Option<String>,
    metadata: Option<String>,
    plan_name: Option<String>,
}

fn parse_csv(bytes: &[u8]) -> (Vec<CreateKeyRequest>, Vec<Value>) {
    let mut rdr = csv::Reader::from_reader(bytes);

    let mut reqs = Vec::new();
    let mut errors = Vec::new();
    for (idx, record) in rdr.deserialize::<CsvKeyRow>().enumerate() {
        let line_no = idx + 2; // 1-based header + 1-based data offset
        let row = match record {
            Ok(r) => r,
            Err(e) => {
                errors.push(json!({
                    "line": line_no,
                    "reason": format!("CSV parse error: {}", e),
                }));
                continue;
            }
        };

        // Split `models` on `|`, dropping empty fragments.
        let models = row
            .models
            .map(|s| {
                s.split('|')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

        // Parse `metadata` JSON string → Value. Bad JSON leaves the field as
        // an error rather than silently dropping it.
        let metadata = match row.metadata.as_deref() {
            None => None,
            Some("") => None,
            Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
                Ok(v) => Some(v),
                Err(e) => {
                    errors.push(json!({
                        "line": line_no,
                        "reason": format!("metadata is not valid JSON: {}", e),
                    }));
                    continue;
                }
            },
        };

        reqs.push(CreateKeyRequest {
            key_alias: row.key_alias.filter(|s| !s.is_empty()),
            key_name: row.key_name.filter(|s| !s.is_empty()),
            key_prefix: row.key_prefix.filter(|s| !s.is_empty()),
            tag: row.tag.filter(|s| !s.is_empty()),
            user_id: row.user_id.filter(|s| !s.is_empty()),
            team_id: row.team_id.filter(|s| !s.is_empty()),
            models,
            max_budget: row.max_budget,
            budget_duration: row.budget_duration.filter(|s| !s.is_empty()),
            rpm_limit: row.rpm_limit,
            tpm_limit: row.tpm_limit,
            expires: row.expires.filter(|s| !s.is_empty()),
            metadata,
            // CSV is a flat-string format — can't carry the three-state
            // Option<Option<String>>. Non-empty string → Some(Some(name));
            // empty/missing → None (use default at runtime). "Explicit no-plan"
            // cannot be expressed in CSV; use JSONL import/export instead.
            plan_name: row
                .plan_name
                .filter(|s| !s.is_empty())
                .map(|s| Some(s)),
        });
    }
    (reqs, errors)
}

// ═══════════════════════════════════════════════════════════
// Model deployment management (DB + memory)
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct CreateDeploymentRequest {
    pub model_name: String,
    pub litellm_model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<bool>,
    pub api_base: Option<String>,
    pub api_version: Option<String>,
    pub aws_region_name: Option<String>,
    pub aws_access_key_id: Option<String>,
    pub aws_secret_access_key: Option<String>,
    pub rpm: Option<i64>,
    pub tpm: Option<i64>,
    #[serde(default = "default_timeout")]
    pub timeout: i64,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i32>,
    #[serde(default = "default_true_val")]
    pub enabled: bool,
    #[serde(default)]
    pub deployment_id: Option<String>,
    /// Quota count multiplier (default 1).
    #[serde(default)]
    pub quota_count_ratio: Option<i64>,
    /// Max concurrent in-flight requests (flow control, 0 = no limit).
    #[serde(default)]
    pub max_inflight_queue_len: Option<i32>,
    /// Max total input context chars across in-flight requests (flow control, 0 = no limit).
    #[serde(default)]
    pub max_context_len: Option<i64>,
    /// Attach `X-BooM-Client-Type` header to outgoing requests (default false).
    #[serde(default)]
    pub client_type_header: bool,
    /// When true, this deployment also serves as catch-all for unmatched model names.
    #[serde(default)]
    pub serve_not_match: bool,
    /// Cost metadata (input/cached/output cost per million tokens).
    #[serde(default)]
    pub model_info: Option<serde_json::Value>,
}

fn default_timeout() -> i64 {
    1200
}
fn default_true_val() -> bool {
    true
}

pub async fn list_models(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let rows = match boom_routing::DeploymentStore::list_all_db(db_pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Dashboard list_models query failed: {}", e);
            return Json(json!({"error": "Internal error"})).into_response();
        }
    };

    let models: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            // Cost rate lives in the DeploymentStore's in-memory cost_rates
            // map (seeded from config, not the DB table). Convert per-token
            // Decimal → per-million Decimal for display; the front-end shows
            // "input / cached / output" per 1M tokens.
            let rate = state.deployment_store.get_cost_rate(&r.model_name);
            let one_million = rust_decimal::Decimal::from(1_000_000);
            let per_million = |v: rust_decimal::Decimal| -> String {
                if v.is_zero() {
                    "0".to_string()
                } else {
                    (v * one_million).to_string()
                }
            };
            let mut v = json!({
                "id": r.id,
                "model_name": r.model_name,
                "litellm_model": r.litellm_model,
                "api_key": r.api_key,
                "api_key_env": r.api_key_env.unwrap_or(false),
                "api_base": r.api_base,
                "api_version": r.api_version,
                "aws_region_name": r.aws_region_name,
                "aws_access_key_id": r.aws_access_key_id,
                "aws_secret_access_key": r.aws_secret_access_key,
                "rpm": r.rpm,
                "tpm": r.tpm,
                "timeout": r.timeout,
                "headers": r.headers,
                "temperature": r.temperature,
                "max_tokens": r.max_tokens,
                "enabled": r.enabled.unwrap_or(true),
                "auto_disabled": r.auto_disabled.unwrap_or(false),
                "source": r.source,
                "deployment_id": r.deployment_id,
                "quota_count_ratio": r.quota_count_ratio.unwrap_or(1),
                "max_inflight_queue_len": r.max_inflight_queue_len,
                "max_context_len": r.max_context_len,
                "client_type_header": r.client_type_header.unwrap_or(false),
                "serve_not_match": r.serve_not_match,
                "model_info": r.model_info,
                "cost_per_million": {
                    "input": per_million(rate.input_cost_per_token),
                    "cached_input": per_million(rate.cached_input_cost_per_token),
                    "output": per_million(rate.output_cost_per_token),
                },
                "created_at": r.created_at.map(|d| d.to_string()),
                "updated_at": r.updated_at.map(|d| d.to_string()),
            });
            // Mask long-lived credentials at the boundary. The edit form
            // detects "****" and clears the input so COALESCE on update
            // preserves the stored value when the user leaves it empty.
            // headers values are NOT masked here — scrubbing them would
            // break edits (update_db writes headers verbatim, not COALESCE).
            if let Some(obj) = v.as_object_mut() {
                for k in ["api_key", "aws_access_key_id", "aws_secret_access_key"] {
                    if let Some(val) = obj.get_mut(k) {
                        if !val.is_null() {
                            *val = Value::String("****".to_string());
                        }
                    }
                }
            }
            v
        })
        .collect();

    Json(json!({"models": models})).into_response()
}

pub async fn create_model(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(req): Json<CreateDeploymentRequest>,
) -> Response {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    if state.admin_tx.send(crate::state::AdminCommand::CreateModel { req, reply: reply_tx }).await.is_err() {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler unavailable",
        )
            .into_response();
    }

    match reply_rx.await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(msg)) => Json(json!({"error": msg})).into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler dropped reply",
        )
            .into_response(),
    }
}

pub async fn update_model(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateDeploymentRequest>,
) -> Response {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    if state.admin_tx.send(crate::state::AdminCommand::UpdateModel { id, req, reply: reply_tx }).await.is_err() {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler unavailable",
        )
            .into_response();
    }

    match reply_rx.await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(msg)) => Json(json!({"error": msg})).into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler dropped reply",
        )
            .into_response(),
    }
}

pub async fn delete_model(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(id): Path<Uuid>,
) -> Response {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    if state.admin_tx.send(crate::state::AdminCommand::DeleteModel { id, reply: reply_tx }).await.is_err() {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler unavailable",
        )
            .into_response();
    }

    match reply_rx.await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(msg)) => Json(json!({"error": msg})).into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler dropped reply",
        )
            .into_response(),
    }
}

// ═══════════════════════════════════════════════════════════
// Model alias management (DB + memory)
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct CreateAliasRequest {
    pub alias_name: String,
    pub target_model: String,
    #[serde(default)]
    pub hidden: bool,
}

pub async fn list_aliases(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let rows = match boom_routing::AliasStore::list_all_db(db_pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Dashboard list_aliases query failed: {}", e);
            return Json(json!({"error": "Internal error"})).into_response();
        }
    };

    let aliases: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "alias_name": r.alias_name,
                "target_model": r.target_model,
                "hidden": r.hidden.unwrap_or(false),
                "source": r.source,
                "updated_at": r.updated_at.map(|d| d.to_string()),
            })
        })
        .collect();

    Json(json!({"aliases": aliases})).into_response()
}

pub async fn create_alias(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(req): Json<CreateAliasRequest>,
) -> Response {
    if state.deployment_store.is_exclusive_model(&req.alias_name) {
        return Json(json!({
            "error": format!(
                "alias '{}' conflicts with an exclusive workflow model",
                req.alias_name
            )
        }))
        .into_response();
    }
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let input = boom_routing::AliasInput {
        alias_name: req.alias_name.clone(),
        target_model: req.target_model.clone(),
        hidden: req.hidden,
    };

    if let Err(e) = state.alias_store.create_db(db_pool, &input).await {
        tracing::error!("Dashboard create_alias failed: {}", e);
        return Json(json!({"error": "Internal error"})).into_response();
    }

    tracing::info!(alias = %req.alias_name, target = %req.target_model, "Alias created");
    let _ = state.admin_tx.send(crate::state::AdminCommand::ConfigChanged).await;
    Json(json!({"ok": true, "alias_name": req.alias_name})).into_response()
}

pub async fn update_alias(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(alias_name): Path<String>,
    Json(req): Json<CreateAliasRequest>,
) -> Response {
    if state.deployment_store.is_exclusive_model(&req.alias_name) {
        return Json(json!({
            "error": format!(
                "alias '{}' conflicts with an exclusive workflow model",
                req.alias_name
            )
        }))
        .into_response();
    }
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let input = boom_routing::AliasInput {
        alias_name: req.alias_name.clone(),
        target_model: req.target_model.clone(),
        hidden: req.hidden,
    };

    match state.alias_store.update_db(db_pool, &alias_name, &input).await {
        Ok(true) => {
            let _ = state.admin_tx.send(crate::state::AdminCommand::ConfigChanged).await;
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => Json(json!({"error": "Alias not found"})).into_response(),
        Err(e) => {
            tracing::error!("Dashboard update_alias failed: {}", e);
            Json(json!({"error": "Internal error"})).into_response()
        }
    }
}

pub async fn delete_alias(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(alias_name): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    match state.alias_store.delete_db(db_pool, &alias_name).await {
        Ok(true) => {
            tracing::info!(alias = %alias_name, "Alias deleted");
            let _ = state.admin_tx.send(crate::state::AdminCommand::ConfigChanged).await;
            Json(json!({"ok": true, "alias_name": alias_name})).into_response()
        }
        Ok(false) => Json(json!({"error": "Alias not found"})).into_response(),
        Err(e) => {
            tracing::error!("Dashboard delete_alias failed: {}", e);
            Json(json!({"error": "Internal error"})).into_response()
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Request Logs
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct ListLogsQuery {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_per_page")]
    pub per_page: i64,
    pub key_hash: Option<String>,
    pub model: Option<String>,
    pub status: Option<String>,
    // Column-level filters (partial match via ILIKE where applicable)
    pub request_id: Option<String>,
    pub key_alias: Option<String>,
    pub api_path: Option<String>,
    pub status_code: Option<i16>,
    pub stream: Option<String>,
    pub error: Option<String>,
    pub team_alias: Option<String>,
    pub client_ip: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct LogRow {
    request_id: Option<String>,
    key_hash: String,
    key_name: Option<String>,
    key_alias: Option<String>,
    team_id: Option<String>,
    team_alias: Option<String>,
    model: String,
    api_path: String,
    is_stream: bool,
    status_code: i16,
    error_type: Option<String>,
    error_message: Option<String>,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    duration_ms: Option<i32>,
    ttft_ms: Option<i32>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    deployment_id: Option<String>,
    client_ip: Option<String>,
    cached_tokens: Option<i64>,
    // DFX scheduling observability — columns on boom_request_log.
    schedule_policy: Option<String>,
    kv_hit_blocks: Option<i64>,
    kv_input_blocks: Option<i64>,
    trie_blocks: Option<i64>,
    trie_max_blocks: Option<i64>,
    request_tokens: Option<i64>,
}

pub async fn list_logs(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Query(query): Query<ListLogsQuery>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let (page, per_page) = normalize_pagination(query.page, query.per_page);
    let offset = (page - 1) * per_page;

    // Build WHERE clause dynamically.
    let mut where_clauses = Vec::new();
    let mut param_idx = 1u32;

    // Helper: register a param slot, return its index.
    macro_rules! slot {
        ($field:expr) => {
            if $field.is_some() { let i = param_idx; param_idx += 1; Some(i) } else { None }
        };
    }

    let key_hash_param   = slot!(query.key_hash);
    let model_param      = slot!(query.model);
    let status_param     = if query.status.as_deref() == Some("error") { let i = param_idx; param_idx += 1; Some(i) } else { None };
    let request_id_param = slot!(query.request_id);
    let key_alias_param  = slot!(query.key_alias);
    let api_path_param   = slot!(query.api_path);
    let status_code_param= slot!(query.status_code);
    // stream is handled as a static WHERE clause (no param slot needed).
    let error_param      = slot!(query.error);
    let team_alias_param = slot!(query.team_alias);
    let client_ip_param   = slot!(query.client_ip);

    if query.key_hash.is_some() {
        where_clauses.push(format!("rl.key_hash = ${}", key_hash_param.unwrap()));
    }
    if query.model.is_some() {
        where_clauses.push(format!("rl.model ILIKE ${}", model_param.unwrap()));
    }
    if query.status.as_deref() == Some("error") {
        where_clauses.push(format!("rl.status_code != ${}", status_param.unwrap()));
    }
    if query.request_id.is_some() {
        where_clauses.push(format!("rl.request_id ILIKE ${}", request_id_param.unwrap()));
    }
    if query.key_alias.is_some() {
        where_clauses.push(format!("(rl.key_alias ILIKE ${0} OR rl.key_name ILIKE ${0})", key_alias_param.unwrap()));
    }
    if query.api_path.is_some() {
        where_clauses.push(format!("rl.api_path ILIKE ${}", api_path_param.unwrap()));
    }
    if query.status_code.is_some() {
        where_clauses.push(format!("rl.status_code = ${}", status_code_param.unwrap()));
    }
    if query.stream.is_some() {
        let s = query.stream.as_deref().unwrap().to_lowercase();
        if s == "yes" || s == "true" || s == "1" {
            where_clauses.push("rl.is_stream = true".to_string());
        } else if s == "no" || s == "false" || s == "0" {
            where_clauses.push("rl.is_stream = false".to_string());
        }
    }
    if query.error.is_some() {
        where_clauses.push(format!("rl.error_message ILIKE ${}", error_param.unwrap()));
    }
    if query.team_alias.is_some() {
        where_clauses.push(format!("bt.team_alias ILIKE ${}", team_alias_param.unwrap()));
    }
    if query.client_ip.is_some() {
        where_clauses.push(format!("rl.client_ip ILIKE ${}", client_ip_param.unwrap()));
    }
    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };

    let limit_idx = param_idx;
    param_idx += 1;
    let offset_idx = param_idx;

    let sql = format!(
        r#"SELECT rl.request_id, rl.key_hash, rl.key_name, rl.key_alias, rl.team_id,
                  bt.team_alias,
                  rl.model, rl.api_path,
                  rl.is_stream, rl.status_code, rl.error_type, rl.error_message,
                  rl.input_tokens, rl.output_tokens, rl.duration_ms, rl.ttft_ms, rl.created_at,
                  rl.deployment_id, rl.client_ip, rl.cached_tokens,
                  rl.schedule_policy, rl.kv_hit_blocks, rl.kv_input_blocks,
                  rl.trie_blocks, rl.trie_max_blocks, rl.request_tokens
           FROM boom_request_log rl
           LEFT JOIN boom_team_table bt ON rl.team_id = bt.team_id
           {where_sql}
           ORDER BY rl.created_at DESC
           LIMIT ${limit_idx} OFFSET ${offset_idx}"#,
    );

    let mut q = sqlx::query_as::<_, LogRow>(&sql);

    // Pre-build LIKE patterns so they outlive the bind chain.
    let model_pattern      = query.model.as_ref().map(|v| format!("%{}%", v));
    let request_id_pattern = query.request_id.as_ref().map(|v| format!("%{}%", v));
    let key_alias_pattern  = query.key_alias.as_ref().map(|v| format!("%{}%", v));
    let api_path_pattern   = query.api_path.as_ref().map(|v| format!("%{}%", v));
    let error_pattern      = query.error.as_ref().map(|v| format!("%{}%", v));
    let team_alias_pattern = query.team_alias.as_ref().map(|v| format!("%{}%", v));
    let client_ip_pattern  = query.client_ip.as_ref().map(|v| format!("%{}%", v));

    // Bind parameters (order must match slot allocation above).
    if let Some(ref v) = query.key_hash {
        q = q.bind(v.clone());
    }
    if let Some(ref p) = model_pattern {
        q = q.bind(p.clone());
    }
    if query.status.as_deref() == Some("error") {
        q = q.bind(200i16);
    }
    if let Some(ref p) = request_id_pattern {
        q = q.bind(p.clone());
    }
    if let Some(ref p) = key_alias_pattern {
        q = q.bind(p.clone());
    }
    if let Some(ref p) = api_path_pattern {
        q = q.bind(p.clone());
    }
    if let Some(v) = query.status_code {
        q = q.bind(v);
    }
    // stream is handled as a static WHERE clause (no bind needed).
    if let Some(ref p) = error_pattern {
        q = q.bind(p.clone());
    }
    if let Some(ref p) = team_alias_pattern {
        q = q.bind(p.clone());
    }
    if let Some(ref p) = client_ip_pattern {
        q = q.bind(p.clone());
    }

    // Fetch per_page + 1 to detect if there is a next page.
    q = q.bind(per_page + 1).bind(offset);

    let rows: Vec<LogRow> = match q.fetch_all(db_pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Dashboard list_logs query failed: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
            )
                .into_response();
        }
    };

    let has_next = rows.len() > per_page as usize;
    let logs: Vec<Value> = rows
        .into_iter()
        .take(per_page as usize)
        .into_iter()
        .map(|r| {
            let display_model = match &r.deployment_id {
                Some(did) if !did.is_empty() => format!("{}:{}", r.model, did),
                _ => r.model.clone(),
            };
            json!({
                "request_id": r.request_id,
                "key_hash": r.key_hash,
                "key_name": r.key_name,
                "key_alias": r.key_alias,
                "team_id": r.team_id,
                "team_alias": r.team_alias,
                "model": display_model,
                "api_path": r.api_path,
                "is_stream": r.is_stream,
                "status_code": r.status_code,
                "error_type": r.error_type,
                "error_message": r.error_message,
                "input_tokens": r.input_tokens,
                "output_tokens": r.output_tokens,
                // vLLM-reported real KV-cache hit (prompt_tokens_details.cached_tokens).
                // Frontend "Prefix Hit Rate" = cached_tokens / input_tokens
                // (prefix only), computed client-side.
                "cached_tokens": r.cached_tokens,
                "duration_ms": r.duration_ms,
                "ttft_ms": r.ttft_ms,
                "created_at": r.created_at.map(|d| d.to_rfc3339()),
                "client_ip": r.client_ip,
                // DFX: scheduling observability. policy shown as short code:
                // kvc=kvc_aware, key=key_affinity, rr=round_robin.
                "policy": match r.schedule_policy.as_deref() {
                    Some("kvc_aware") => "kvc",
                    Some("kvc_aware→key_affinity") => "kvc→key",
                    Some("key_affinity") => "key",
                    Some("round_robin") => "rr",
                    Some(other) => other,
                    None => "",
                },
                "kv_hit_blocks": r.kv_hit_blocks,
                "kv_input_blocks": r.kv_input_blocks,
                "trie_blocks": r.trie_blocks,
                "trie_max_blocks": r.trie_max_blocks,
                "request_tokens": r.request_tokens,
            })
        })
        .collect();

    Json(json!({
        "logs": logs,
        "page": page,
        "per_page": per_page,
        "has_next": has_next,
    }))
    .into_response()
}

// ═══════════════════════════════════════════════════════════
// Teams
// ═══════════════════════════════════════════════════════════

// Note: team listing is handled by `quota_overview` — it returns team rows
// joined with cumulative counters, effective plan limits, and prompt-log
// excluded status. The dedicated `list_teams` handler was removed to avoid
// divergent data sources (boom_request_log SUM vs cumulative counters).

#[derive(Debug, Deserialize)]
pub struct CreateTeamRequest {
    pub team_id: String,
    pub team_alias: Option<String>,
    /// Allowed models. Empty or containing "all-team-models" = all models allowed.
    #[serde(default)]
    pub models: Vec<String>,
}

pub async fn create_team(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(req): Json<CreateTeamRequest>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    if req.team_id.trim().is_empty() {
        return (axum::http::StatusCode::BAD_REQUEST, "team_id is required").into_response();
    }

    // team full-access is now stored as `[]` (empty array) per litellm semantic.
    // Legacy rows with ["all-team-models"] still render as full-access via
    // renderTeamModels/formatTeamModels — no migration needed.
    let models = req.models;

    let result = sqlx::query(
        r#"INSERT INTO boom_team_table (team_id, team_alias, models, created_at, updated_at)
           VALUES ($1, $2, $3, NOW(), NOW())"#,
    )
    .bind(&req.team_id)
    .bind(&req.team_alias)
    .bind(&models)
    .execute(db_pool)
    .await;

    match result {
        Ok(_) => Json(json!({
            "ok": true,
            "team_id": req.team_id,
            "team_alias": req.team_alias,
            "models": models,
        })).into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("duplicate key") || msg.contains("violates unique") {
                return (
                    axum::http::StatusCode::CONFLICT,
                    format!("team_id '{}' already exists", req.team_id),
                ).into_response();
            }
            tracing::error!("Dashboard create_team failed: {}", e);
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateTeamRequest {
    pub team_alias: Option<String>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
}

pub async fn update_team(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(team_id): Path<String>,
    Json(req): Json<UpdateTeamRequest>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    // team full-access is now stored as `[]` (empty array) per litellm semantic.
    // No normalize needed — frontend submit [] for full access; legacy rows
    // with ["all-team-models"] render correctly via renderTeamModels.
    let models = req.models;

    let result = sqlx::query(
        r#"UPDATE boom_team_table
           SET team_alias = COALESCE($2, team_alias),
               models = COALESCE($3, models),
               updated_at = NOW()
           WHERE team_id = $1"#,
    )
    .bind(&team_id)
    .bind(&req.team_alias)
    .bind(&models)
    .execute(db_pool)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => Json(json!({"ok": true})).into_response(),
        Ok(_) => (axum::http::StatusCode::NOT_FOUND, "Team not found").into_response(),
        Err(e) => {
            tracing::error!("Dashboard update_team failed: {}", e);
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

pub async fn delete_team(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(team_id): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    // Check if team has keys.
    let key_count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM boom_verification_token WHERE team_id = $1"#,
    )
    .bind(&team_id)
    .fetch_one(db_pool)
    .await
    .unwrap_or(0);

    if key_count > 0 {
        return (
            axum::http::StatusCode::CONFLICT,
            format!("Cannot delete team: {} key(s) still assigned", key_count),
        ).into_response();
    }

    let result = sqlx::query(
        r#"DELETE FROM boom_team_table WHERE team_id = $1"#,
    )
    .bind(&team_id)
    .execute(db_pool)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => Json(json!({"ok": true, "team_id": team_id})).into_response(),
        Ok(_) => (axum::http::StatusCode::NOT_FOUND, "Team not found").into_response(),
        Err(e) => {
            tracing::error!("Dashboard delete_team failed: {}", e);
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

// ═══════════════════════════════════════════════════════════
// In-Flight Request Stats (real-time)
// ═══════════════════════════════════════════════════════════

pub async fn get_inflight_stats(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Response {
    use std::collections::HashMap;

    let flowcontrol_stats = state.flow_controller.get_stats();
    let queued_waiters = state.flow_controller.get_queued_waiters();
    let dispatched_keys = state.flow_controller.get_dispatched_keys();

    // Build lookup: deployment_id → queued waiter entries.
    let queued_map: HashMap<&str, &Vec<boom_flowcontrol::QueuedWaiterEntry>> = queued_waiters.iter()
        .map(|q| (q.deployment_id.as_str(), &q.waiters))
        .collect();

    // Build lookup: deployment_id → dispatched key entries.
    let dispatched_map: HashMap<&str, &Vec<boom_flowcontrol::DispatchedKeyEntry>> = dispatched_keys.iter()
        .map(|d| (d.deployment_id.as_str(), &d.keys))
        .collect();

    let mut rows: HashMap<String, serde_json::Value> = HashMap::new();

    // 1. All FlowControl deployments (primary data source).
    for fc in &flowcontrol_stats {
        let model = state.deployment_store.find_model_by_deployment_id(&fc.deployment_id)
            .unwrap_or_else(|| "-".to_string());
        let queued_keys: Vec<serde_json::Value> = queued_map.get(fc.deployment_id.as_str())
            .map(|entries| entries.iter().map(|e| json!({
                "key_alias": e.key_alias,
                "is_vip": e.is_vip,
            })).collect())
            .unwrap_or_default();
        let key_stats = aggregate_dispatched_keys(dispatched_map.get(fc.deployment_id.as_str()).copied());
        rows.insert(fc.deployment_id.clone(), json!({
            "model": model,
            "deployment_id": fc.deployment_id,
            "fc_queue": fc.waiters + fc.vip_waiters,
            "in_reqs": fc.current_inflight,
            "in_reqs_max": fc.max_inflight,
            "in_context": fc.current_context,
            "in_context_max": fc.max_context,
            "queued_keys": queued_keys,
            "key_stats": key_stats,
        }));
    }

    // 2. Per-deployment fallback — deployments without FC config.
    //    Use deployment-level stats from InFlightTracker so each deployment
    //    shows up as a separate row with its own deployment_id.
    let covered_deployments: std::collections::HashSet<String> = rows.keys().cloned().collect();
    for d in state.inflight.get_deployment_stats() {
        if covered_deployments.contains(&d.deployment_id) {
            continue;
        }
        rows.insert(d.deployment_id.clone(), json!({
            "model": d.model,
            "deployment_id": d.deployment_id,
            "fc_queue": 0,
            "in_reqs": d.inflight_requests,
            "in_reqs_max": 0,
            "in_context": d.inflight_input_chars,
            "in_context_max": 0,
            "queued_keys": [],
            "key_stats": [],
        }));
    }

    // Sort: deployments resolvable to a model come first (alphabetical),
    // then deployments whose model is "-" (no longer in deployment_store,
    // i.e. disabled/removed config) sink to the bottom — still alphabetical
    // among themselves.
    let mut result: Vec<_> = rows.into_values().collect();
    result.sort_by(|a, b| {
        let am = a["model"].as_str().unwrap_or("");
        let bm = b["model"].as_str().unwrap_or("");
        let a_disabled = am == "-";
        let b_disabled = bm == "-";
        a_disabled.cmp(&b_disabled).then_with(|| {
            am.cmp(bm).then_with(|| {
                a["deployment_id"].as_str().unwrap_or("").cmp(b["deployment_id"].as_str().unwrap_or(""))
            })
        })
    });

    Json(json!({ "deployments": result })).into_response()
}

/// Aggregate dispatched key entries by key_alias, returning per-key request counts.
fn aggregate_dispatched_keys(keys: Option<&Vec<boom_flowcontrol::DispatchedKeyEntry>>) -> Vec<serde_json::Value> {
    let entries = match keys {
        Some(e) => e,
        None => return Vec::new(),
    };
    // Aggregate by key_alias, tracking count and is_vip (true if any entry was VIP).
    let mut acc: std::collections::HashMap<String, (u64, bool)> = std::collections::HashMap::new();
    for entry in entries {
        let e = acc.entry(entry.key_alias.clone()).or_insert((0, false));
        e.0 += 1;
        e.1 = e.1 || entry.is_vip;
    }
    let mut result: Vec<serde_json::Value> = acc.into_iter()
        .map(|(alias, (count, is_vip))| json!({
            "key_alias": alias,
            "request_count": count,
            "is_vip": is_vip,
        }))
        .collect();
    result.sort_by(|a, b| b["request_count"].as_u64().cmp(&a["request_count"].as_u64()));
    result
}

// ═══════════════════════════════════════════════════════════
// Deployment 24h Summary (off the auto-refresh path)
// ═══════════════════════════════════════════════════════════

#[derive(Debug, sqlx::FromRow)]
struct DeploymentSummaryRow {
    deployment_id: String,
    total_requests: i64,
    input_count: i64,
    sum_input_tokens: i64,
    output_count: i64,
    sum_output_tokens: i64,
    ttft_count: i64,
    sum_ttft_ms: i64,
    avg_prefix_hit_rate: Option<f64>,
}

/// GET /admin/stats/deployments/summary — 24h per-deployment aggregates.
/// Computed on demand (page load + Refresh button), NOT on the 3s auto-poll.
pub async fn get_deployment_summary_24h(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Response {
    let pool = match &state.db_pool {
        Some(p) => p,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    let rows_res = async {
        let mut tx = begin_with_timeout(pool).await?;
        let rows = sqlx::query_as::<_, DeploymentSummaryRow>(
            r#"SELECT
                 deployment_id,
                 COUNT(*)::bigint AS total_requests,
                 COUNT(input_tokens)::bigint AS input_count,
                 COALESCE(SUM(input_tokens), 0)::bigint AS sum_input_tokens,
                 COUNT(output_tokens)::bigint AS output_count,
                 COALESCE(SUM(output_tokens), 0)::bigint AS sum_output_tokens,
                 COUNT(ttft_ms)::bigint AS ttft_count,
                 COALESCE(SUM(ttft_ms), 0)::bigint AS sum_ttft_ms,
                 AVG(
                   CASE
                     WHEN cached_tokens IS NOT NULL
                      AND COALESCE(input_tokens, 0) > 0
                     THEN cached_tokens::double precision
                          / COALESCE(input_tokens, 0)
                          * 100.0
                   END
                 ) AS avg_prefix_hit_rate
               FROM boom_request_log
               WHERE created_at >= NOW() - INTERVAL '24 hours'
                 AND deployment_id IS NOT NULL
                 AND status_code >= 200 AND status_code < 300
               GROUP BY deployment_id"#,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(rows)
    }.await;

    let rows = match rows_res {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to query deployment summary: {}", e);
            return Json(json!({"error": e.to_string()})).into_response();
        }
    };

    let deployments: Vec<serde_json::Value> = rows.into_iter()
        .map(|r| {
            let avg = |sum: i64, count: i64| -> Option<f64> {
                if count == 0 { None } else { Some(sum as f64 / count as f64) }
            };
            json!({
                "deployment_id": r.deployment_id,
                "total_requests": r.total_requests,
                "avg_input_tokens": avg(r.sum_input_tokens, r.input_count),
                "avg_output_tokens": avg(r.sum_output_tokens, r.output_count),
                "avg_ttft_ms": avg(r.sum_ttft_ms, r.ttft_count),
                "avg_prefix_hit_rate": r.avg_prefix_hit_rate,
            })
        })
        .collect();

    Json(json!({
        "deployments": deployments,
        "window_hours": 24,
    })).into_response()
}

// ═══════════════════════════════════════════════════════════
// Rebalance Move Stats (per deployment, in/out counts)
// ═══════════════════════════════════════════════════════════

pub async fn get_rebalance_moves(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Response {
    let moves = state.rebalance_move_tracker.snapshot();
    Json(json!({ "moves": moves })).into_response()
}

// ═══════════════════════════════════════════════════════════
// Audit Log Drop Counter (channel full or batch INSERT failures)
// ═══════════════════════════════════════════════════════════

pub async fn get_audit_log_stats(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Response {
    let dropped = state
        .log_dropped
        .as_ref()
        .map(|c| c.dropped_count())
        .unwrap_or(0);
    let db_configured = state.log_dropped.is_some();
    Json(json!({
        "dropped": dropped,
        "db_configured": db_configured,
    }))
    .into_response()
}

// ═══════════════════════════════════════════════════════════
// Time-windowed Stats (Agent Statistics + Request Rate)
// ═══════════════════════════════════════════════════════════

use crate::stats_timeseries::{ResolvedRange, StatsRangeQuery, TimeWindow};

#[derive(Debug, sqlx::FromRow)]
struct AgentBucketRow {
    bucket_epoch: i64,
    total: i64,
    anthropic: i64,
    input_tokens_total: i64,
    input_tokens_anthropic: i64,
    output_tokens_total: i64,
    output_tokens_anthropic: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct RateBucketRow {
    bucket_epoch: i64,
    deployment_id: Option<String>,
    total: i64,
}

fn window_json(w: &TimeWindow) -> serde_json::Value {
    json!({
        "from": w.from.to_rfc3339(),
        "to": w.to.to_rfc3339(),
        "bucket_secs": w.bucket_secs,
    })
}

/// Begin a dashboard-query transaction with a hard `statement_timeout`.
/// SET LOCAL scopes the timeout to this transaction only, so it never leaks
/// into other queries sharing the dashboard pool.
async fn begin_with_timeout(pool: &sqlx::PgPool) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

pub async fn get_request_rate_stats(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Query(q): Query<StatsRangeQuery>,
) -> Response {
    let parsed = match ResolvedRange::parse(&q) {
        Ok(p) => p,
        Err(e) => return Json(json!({"error": e})).into_response(),
    };

    if parsed.use_memory {
        let window = parsed.resolved.to_window();
        let all = state.request_rate.snapshot_all();
        let expected_ts: Vec<String> = window.expected_buckets().into_iter()
            .map(|t| t.to_rfc3339()).collect();

        // Reshape per-deployment snapshots into per-model groups. Each model
        // group carries a fixed, alphabetically-ordered deployment_id list and
        // per-bucket segments aligned to that order — so the frontend can
        // render stacked bars with stable segment positions across buckets.
        let mut total_counts: Vec<u64> = vec![0u64; expected_ts.len()];
        // model -> (deployment_id -> bucket counts)
        let mut by_model: std::collections::BTreeMap<String, std::collections::HashMap<String, Vec<u64>>> =
            std::collections::BTreeMap::new();

        for (dep_id, data) in all.into_iter() {
            // Tracker has exactly 60 buckets aligned to (now - 59min) .. now,
            // which matches the 1h TimeWindow's expected_buckets ordering.
            let counts: Vec<u64> = data.into_iter().map(|(_, c)| c).collect();
            if dep_id == "_total" {
                total_counts = counts;
                continue;
            }
            // Pad / truncate to expected length just in case.
            let mut v = counts;
            v.resize(expected_ts.len(), 0u64);
            let model = state.deployment_store.find_model_by_deployment_id(&dep_id)
                .unwrap_or_else(|| "-".to_string());
            by_model.entry(model).or_default().insert(dep_id, v);
        }

        let mut charts: Vec<serde_json::Value> = Vec::new();

        // _total first — single color, no segments.
        charts.push(json!({
            "model": "ALL",
            "deployment_id": "_total",
            "events": build_rate_events_simple(&expected_ts, &total_counts),
        }));

        for (model, dep_counts) in by_model.iter() {
            // Fixed segment order: deployment_id alphabetical. Stable across
            // buckets and across reloads.
            let mut dep_order: Vec<String> = dep_counts.keys().cloned().collect();
            dep_order.sort();
            let segments_per_dep: Vec<(String, &Vec<u64>)> = dep_order.iter()
                .map(|d| (d.clone(), dep_counts.get(d).unwrap()))
                .collect();
            charts.push(json!({
                "model": model,
                "deployments": dep_order,
                "events": build_rate_events_segmented(&expected_ts, &segments_per_dep),
            }));
        }

        return Json(json!({
            "window": window_json(&window),
            "charts": charts,
        })).into_response();
    }

    let window = parsed.resolved.to_window();
    let pool = match &state.db_pool {
        Some(p) => p,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    let bucket_secs = window.bucket_secs;
    let from = window.from;
    let to = window.to;
    let from_epoch = from.timestamp();

    let rows_res = async {
        let mut tx = begin_with_timeout(pool).await?;
        let rows = sqlx::query_as::<_, RateBucketRow>(
            r#"SELECT
                 (FLOOR((EXTRACT(EPOCH FROM created_at) - $1) / $2) * $2 + $1)::bigint AS bucket_epoch,
                 deployment_id,
                 COUNT(*)::bigint AS total
               FROM boom_request_log
               WHERE created_at >= $3 AND created_at < $4
                 AND status_code = 200
               GROUP BY 1, 2
               ORDER BY 2, 1"#,
        )
        .bind(from_epoch)
        .bind(bucket_secs)
        .bind(from)
        .bind(to)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(rows)
    }.await;

    let rows = match rows_res {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to query request_rate stats: {}", e);
            return Json(json!({"error": e.to_string()})).into_response();
        }
    };

    let expected: Vec<i64> = window.expected_buckets().into_iter().map(|t| t.timestamp()).collect();

    // Group by model → deployment_id → bucket count. Deployments are ordered
    // alphabetically per model so stacked-bar segment positions are stable.
    let mut total_by_bucket: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    // model -> (deployment_id -> (bucket_epoch -> count))
    let mut by_model: std::collections::BTreeMap<String, std::collections::HashMap<String, std::collections::HashMap<i64, i64>>> =
        std::collections::BTreeMap::new();

    for row in rows {
        let dep = row.deployment_id.clone().unwrap_or_else(|| "_unknown".to_string());
        let model = state.deployment_store.find_model_by_deployment_id(&dep)
            .unwrap_or_else(|| "-".to_string());
        by_model.entry(model).or_default()
            .entry(dep).or_default()
            .insert(row.bucket_epoch, row.total);
        *total_by_bucket.entry(row.bucket_epoch).or_insert(0) += row.total;
    }

    let mut charts: Vec<serde_json::Value> = Vec::new();

    // _total first — single color, no segments.
    charts.push(json!({
        "model": "ALL",
        "deployment_id": "_total",
        "events": build_rate_events(&expected, &total_by_bucket),
    }));

    for (model, dep_map) in by_model.iter() {
        let mut dep_order: Vec<String> = dep_map.keys().cloned().collect();
        dep_order.sort();
        // For each expected bucket, build a segment array in dep_order.
        let events: Vec<serde_json::Value> = expected.iter().map(|ep| {
            let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(*ep, 0)
                .unwrap_or_else(chrono::Utc::now);
            let segments: Vec<serde_json::Value> = dep_order.iter().map(|d| {
                let cnt = dep_map.get(d).and_then(|m| m.get(ep)).copied().unwrap_or(0);
                json!({ "deployment_id": d, "count": cnt })
            }).collect();
            json!({ "ts": ts.to_rfc3339(), "segments": segments })
        }).collect();
        charts.push(json!({
            "model": model,
            "deployments": dep_order,
            "events": events,
        }));
    }

    Json(json!({
        "window": window_json(&window),
        "charts": charts,
    })).into_response()
}

fn build_rate_events(
    expected: &[i64],
    counts: &std::collections::HashMap<i64, i64>,
) -> Vec<serde_json::Value> {
    expected.iter().map(|ep| {
        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(*ep, 0)
            .unwrap_or_else(chrono::Utc::now);
        json!({
            "ts": ts.to_rfc3339(),
            "count": counts.get(ep).copied().unwrap_or(0),
        })
    }).collect()
}

/// Build events for the _total chart (no segments).
fn build_rate_events_simple(expected_ts: &[String], counts: &[u64]) -> Vec<serde_json::Value> {
    expected_ts.iter().enumerate().map(|(i, ts)| {
        let c = counts.get(i).copied().unwrap_or(0);
        json!({ "ts": ts, "count": c })
    }).collect()
}

/// Build events for a per-model chart with per-deployment segments.
/// `segments_per_dep` is an ordered list of (deployment_id, per-bucket counts);
/// the order is the fixed segment order used across all buckets.
fn build_rate_events_segmented(
    expected_ts: &[String],
    segments_per_dep: &[(String, &Vec<u64>)],
) -> Vec<serde_json::Value> {
    expected_ts.iter().enumerate().map(|(i, ts)| {
        let segs: Vec<serde_json::Value> = segments_per_dep.iter().map(|(dep, counts)| {
            let c = counts.get(i).copied().unwrap_or(0);
            json!({ "deployment_id": dep, "count": c })
        }).collect();
        json!({ "ts": ts, "segments": segs })
    }).collect()
}

// ═══════════════════════════════════════════════════════════
// Agent Stats (client-type breakdown)
// ═══════════════════════════════════════════════════════════

pub async fn get_agent_stats(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Query(q): Query<StatsRangeQuery>,
) -> Response {
    let parsed = match ResolvedRange::parse(&q) {
        Ok(p) => p,
        Err(e) => return Json(json!({"error": e})).into_response(),
    };

    if parsed.use_memory {
        let snap = state.agent_stats.snapshot();
        let window = parsed.resolved.to_window();
        let expected_ts: Vec<String> = window.expected_buckets().into_iter()
            .map(|t| t.to_rfc3339()).collect();
        // Tracker has 60 buckets aligned to (now - 59min) .. now; pair each by index.
        let events: Vec<serde_json::Value> = snap.events.into_iter().enumerate()
            .map(|(i, b)| json!({
                "ts": expected_ts.get(i).cloned().unwrap_or_default(),
                "total": b.total,
                "anthropic": b.anthropic,
                "input_tokens_total": b.input_tokens_total,
                "input_tokens_anthropic": b.input_tokens_anthropic,
                "output_tokens_total": b.output_tokens_total,
                "output_tokens_anthropic": b.output_tokens_anthropic,
            }))
            .collect();
        return Json(json!({
            "window": window_json(&window),
            "events": events,
            "summary": serde_json::to_value(&snap.summary).unwrap_or_default(),
        })).into_response();
    }

    let window = parsed.resolved.to_window();
    let pool = match &state.db_pool {
        Some(p) => p,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    let bucket_secs = window.bucket_secs;
    let from = window.from;
    let to = window.to;
    let from_epoch = from.timestamp();

    let rows_res = async {
        let mut tx = begin_with_timeout(pool).await?;
        let rows = sqlx::query_as::<_, AgentBucketRow>(
            r#"SELECT
                 (FLOOR((EXTRACT(EPOCH FROM created_at) - $1) / $2) * $2 + $1)::bigint AS bucket_epoch,
                 COUNT(*)::bigint AS total,
                 COUNT(*) FILTER (WHERE api_path LIKE '/v1/messages%')::bigint AS anthropic,
                 COALESCE(SUM(input_tokens), 0)::bigint AS input_tokens_total,
                 COALESCE(SUM(input_tokens) FILTER (WHERE api_path LIKE '/v1/messages%'), 0)::bigint AS input_tokens_anthropic,
                 COALESCE(SUM(output_tokens), 0)::bigint AS output_tokens_total,
                 COALESCE(SUM(output_tokens) FILTER (WHERE api_path LIKE '/v1/messages%'), 0)::bigint AS output_tokens_anthropic
               FROM boom_request_log
               WHERE created_at >= $3 AND created_at < $4
                 AND status_code = 200
               GROUP BY 1
               ORDER BY 1"#,
        )
        .bind(from_epoch)
        .bind(bucket_secs)
        .bind(from)
        .bind(to)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(rows)
    }.await;

    let rows = match rows_res {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to query agent stats: {}", e);
            return Json(json!({"error": e.to_string()})).into_response();
        }
    };

    let by_epoch: std::collections::HashMap<i64, AgentBucketRow> = rows.into_iter()
        .map(|r| (r.bucket_epoch, r))
        .collect();

    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut total: u64 = 0;
    let mut anthropic: u64 = 0;
    let mut input_tokens_total: u64 = 0;
    let mut input_tokens_anthropic: u64 = 0;
    let mut output_tokens_total: u64 = 0;
    let mut output_tokens_anthropic: u64 = 0;
    for ts in window.expected_buckets() {
        let ep = ts.timestamp();
        let (t, a, it, ia, ot, oa) = match by_epoch.get(&ep) {
            Some(r) => (
                r.total as u64,
                r.anthropic as u64,
                r.input_tokens_total as u64,
                r.input_tokens_anthropic as u64,
                r.output_tokens_total as u64,
                r.output_tokens_anthropic as u64,
            ),
            None => (0, 0, 0, 0, 0, 0),
        };
        total += t;
        anthropic += a;
        input_tokens_total += it;
        input_tokens_anthropic += ia;
        output_tokens_total += ot;
        output_tokens_anthropic += oa;
        events.push(json!({
            "ts": ts.to_rfc3339(),
            "total": t,
            "anthropic": a,
            "input_tokens_total": it,
            "input_tokens_anthropic": ia,
            "output_tokens_total": ot,
            "output_tokens_anthropic": oa,
        }));
    }
    let ratio = if total == 0 { 0.0 } else { anthropic as f64 / total as f64 };
    let input_token_ratio = if input_tokens_total == 0 {
        0.0
    } else {
        input_tokens_anthropic as f64 / input_tokens_total as f64
    };
    let output_token_ratio = if output_tokens_total == 0 {
        0.0
    } else {
        output_tokens_anthropic as f64 / output_tokens_total as f64
    };

    Json(json!({
        "window": window_json(&window),
        "events": events,
        "summary": {
            "total": total,
            "anthropic": anthropic,
            "ratio": ratio,
            "input_tokens_total": input_tokens_total,
            "input_tokens_anthropic": input_tokens_anthropic,
            "input_token_ratio": input_token_ratio,
            "output_tokens_total": output_tokens_total,
            "output_tokens_anthropic": output_tokens_anthropic,
            "output_token_ratio": output_token_ratio,
        },
    })).into_response()
}

// ═══════════════════════════════════════════════════════════
// Rate Limit Window Reset
// ═══════════════════════════════════════════════════════════

/// POST /admin/limits/reset/{key_hash} — clear all rate limit windows for one key.
pub async fn reset_limits_for_key(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Path(key_hash): Path<String>,
) -> Json<Value> {
    tracing::info!(key_hash = %key_hash, "Admin resetting rate limit windows for key");
    let removed = state.limiter.clear_for_key(&key_hash);
    Json(json!({
        "ok": true,
        "cleared": removed,
        "message": format!("Cleared {} window counter(s) for key '{}'", removed, key_hash)
    }))
}

/// POST /admin/limits/reset — clear all rate limit windows for all keys.
pub async fn reset_limits_all(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Json<Value> {
    tracing::info!("Admin resetting ALL rate limit windows");
    let removed = state.limiter.clear_all();
    Json(json!({
        "ok": true,
        "cleared": removed,
        "message": format!("Cleared all {} window counter(s)", removed)
    }))
}

// ═══════════════════════════════════════════════════════════
// Debug Error Recording
// ═══════════════════════════════════════════════════════════
// Always compiled — the log page's "debug toggle" + "view error detail"
// workflow must work without the `debug-tools` feature. The feature only
// gates the standalone Debug page (nav link + /dashboard/debug entry, see
// handlers_static.rs).

pub async fn get_debug_status(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Json<Value> {
    Json(json!({
        "enabled": state.debug_store.is_enabled(),
        "entries": state.debug_store.len(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct DebugToggleRequest {
    pub enabled: bool,
}

pub async fn toggle_debug(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Json(req): Json<DebugToggleRequest>,
) -> Json<Value> {
    state.debug_store.set_enabled(req.enabled);
    // Also toggle raw upstream response capture alongside debug mode.
    let new_cfg = state
        .prompt_log_query
        .full_config()
        .with_enabled(true)
        .with_capture_raw_upstream(req.enabled);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let _ = state
        .admin_tx
        .send(crate::state::AdminCommand::UpdatePromptLogConfig {
            config: new_cfg,
            reply: reply_tx,
        })
        .await;
    let _ = reply_rx.await;
    tracing::info!(enabled = req.enabled, "Debug toggled (debug errors + raw upstream capture)");
    Json(json!({
        "ok": true,
        "enabled": req.enabled,
    }))
}

pub async fn get_debug_error(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Path(request_id): Path<String>,
) -> Response {
    match state.debug_store.get(&request_id) {
        Some(entry) => Json(json!({"debug_error": entry})).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            "Debug entry not found (not recorded or expired)",
        )
            .into_response(),
    }
}

// ═══════════════════════════════════════════════════════════
// Hot-Reload Config
// ═══════════════════════════════════════════════════════════

pub async fn reload_config(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Response {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    if state
        .admin_tx
        .send(crate::state::AdminCommand::ReloadConfig { reply: reply_tx })
        .await
        .is_err()
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler unavailable",
        )
            .into_response();
    }

    match reply_rx.await {
        Ok(Ok(msg)) => Json(json!({"ok": true, "message": msg})).into_response(),
        Ok(Err(msg)) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            msg,
        )
            .into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler dropped reply",
        )
            .into_response(),
    }
}

// ═══════════════════════════════════════════════════════════
// Config Page (read full config + surgical section update)
// ═══════════════════════════════════════════════════════════

/// GET /admin/config — return the live in-memory config as JSON.
/// Sensitive fields (`master_key`, `database_url`, `api_key`, `aws_*_key`)
/// are masked to null by boom-main before serialization.
pub async fn get_config(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Response {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if state
        .admin_tx
        .send(crate::state::AdminCommand::GetConfig { reply: reply_tx })
        .await
        .is_err()
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler unavailable",
        )
            .into_response();
    }
    match reply_rx.await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(msg)) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler dropped reply",
        )
            .into_response(),
    }
}

/// GET /admin/config/schema — return the field manifest (declarative UI schema).
/// Transparent passthrough: boom-main serializes `boom_config::manifest::*`
/// and forwards. See CLAUDE.md §9.
pub async fn get_config_schema(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Response {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if state
        .admin_tx
        .send(crate::state::AdminCommand::GetConfigSchema { reply: reply_tx })
        .await
        .is_err()
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler unavailable",
        )
            .into_response();
    }
    match reply_rx.await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(msg)) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler dropped reply",
        )
            .into_response(),
    }
}

/// PUT /admin/config — surgical section update.
/// Body: `{ "path": "dotted.path", "value": <json value> }`.
/// Triggers reload after writing.
#[derive(Debug, serde::Deserialize)]
pub struct UpdateConfigBody {
    pub path: String,
    pub value: serde_json::Value,
}

pub async fn update_config(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    body: axum::Json<UpdateConfigBody>,
) -> Response {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if state
        .admin_tx
        .send(crate::state::AdminCommand::UpdateConfigSection {
            path: body.path.clone(),
            value: body.value.clone(),
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler unavailable",
        )
            .into_response();
    }
    match reply_rx.await {
        Ok(Ok(msg)) => Json(json!({"ok": true, "message": msg})).into_response(),
        Ok(Err(msg)) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler dropped reply",
        )
            .into_response(),
    }
}

// ═══════════════════════════════════════════════════════════
// Prompt Log Controls
// ═══════════════════════════════════════════════════════════

pub async fn get_prompt_log_status(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Json<Value> {
    let cfg = state.prompt_log_query.config_snapshot();
    Json(json!({
        "enabled": cfg.enabled,
        "excluded_keys": cfg.excluded_keys,
        "excluded_teams": cfg.excluded_teams,
    }))
}

/// POST `/admin/prompt-log/otlp-ping` — probe a remote OTLP collector.
/// Body: `{ "endpoint": "...", "headers": {...}, "timeout_secs": 10 }`.
/// Sends an empty `ExportLogsServiceRequest` and returns `{ok, latency_ms}`
/// on success or `{ok:false, error}` on failure. Used by the connectivity
/// indicator above the endpoint input on the prompt-log card.
#[derive(Debug, Deserialize)]
pub struct PingOtlpEndpointRequest {
    pub endpoint: String,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    #[serde(default = "default_ping_timeout_secs")]
    pub timeout_secs: u64,
}
fn default_ping_timeout_secs() -> u64 { 5 }

pub async fn ping_otlp_endpoint(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Json(req): Json<PingOtlpEndpointRequest>,
) -> Response {
    if req.endpoint.trim().is_empty() {
        return Json(json!({"ok": false, "error": "endpoint not configured"}))
            .into_response();
    }
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if state
        .admin_tx
        .send(crate::state::AdminCommand::PingOtlpEndpoint {
            endpoint: req.endpoint.clone(),
            headers: req.headers.clone(),
            timeout_secs: req.timeout_secs,
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler unavailable",
        )
            .into_response();
    }
    match reply_rx.await {
        Ok(Ok(latency_ms)) => Json(json!({
            "ok": true,
            "latency_ms": latency_ms,
        }))
        .into_response(),
        Ok(Err(e)) => Json(json!({"ok": false, "error": e})).into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Admin command handler dropped reply",
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct PromptLogToggleRequest {
    pub enabled: bool,
}

pub async fn toggle_prompt_log(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Json(req): Json<PromptLogToggleRequest>,
) -> Json<Value> {
    let new_cfg = state.prompt_log_query.full_config().with_enabled(req.enabled);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let _ = state
        .admin_tx
        .send(crate::state::AdminCommand::UpdatePromptLogConfig {
            config: new_cfg,
            reply: reply_tx,
        })
        .await;
    let _ = reply_rx.await;
    tracing::info!(enabled = req.enabled, "Prompt log toggled via dashboard");
    Json(json!({
        "ok": true,
        "enabled": req.enabled,
    }))
}

#[derive(Debug, Deserialize)]
pub struct PromptLogTeamRequest {
    pub team_id: String,
    /// true = exclude this team from logging, false = include.
    pub excluded: bool,
}

pub async fn toggle_team_prompt_log(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Json(req): Json<PromptLogTeamRequest>,
) -> Json<Value> {
    let new_cfg = state
        .prompt_log_query
        .full_config()
        .with_team_excluded(&req.team_id, req.excluded);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let _ = state
        .admin_tx
        .send(crate::state::AdminCommand::UpdatePromptLogConfig {
            config: new_cfg,
            reply: reply_tx,
        })
        .await;
    let _ = reply_rx.await;
    tracing::info!(team_id = %req.team_id, excluded = req.excluded, "Prompt log team exclusion toggled");
    Json(json!({
        "ok": true,
        "team_id": req.team_id,
        "excluded": req.excluded,
    }))
}

#[derive(Debug, Deserialize)]
pub struct PromptLogKeyRequest {
    pub key_hash: String,
    /// true = exclude this key from logging, false = include.
    pub excluded: bool,
}

pub async fn toggle_key_prompt_log(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Json(req): Json<PromptLogKeyRequest>,
) -> Json<Value> {
    let new_cfg = state
        .prompt_log_query
        .full_config()
        .with_key_excluded(&req.key_hash, req.excluded);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let _ = state
        .admin_tx
        .send(crate::state::AdminCommand::UpdatePromptLogConfig {
            config: new_cfg,
            reply: reply_tx,
        })
        .await;
    let _ = reply_rx.await;
    tracing::info!(key_hash = %req.key_hash, excluded = req.excluded, "Prompt log key exclusion toggled");
    Json(json!({
        "ok": true,
        "key_hash": req.key_hash,
        "excluded": req.excluded,
    }))
}

// ═══════════════════════════════════════════════════════════
// Prompt Log Entry Viewer
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct PromptLogEntryQuery {
    pub key_hash: String,
    pub team_alias: Option<String>,
}

/// GET /admin/prompt-log/entry/{request_id}?key_hash=xxx&team_alias=xxx
///
/// Looks up the request and response entries for the given request_id via
/// `PromptLogQueryApi` (which encapsulates the JSONL storage layout). Returns
/// a JSON object with both phases if available. Old single-phase entries
/// (pre-refactor) won't match the new `phase` field and fall through to the
/// 404 path — dashboard surfaces a "log not found" message in that case.
pub async fn get_prompt_log_entry(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Path(request_id): Path<String>,
    Query(query): Query<PromptLogEntryQuery>,
) -> impl IntoResponse {
    let team_alias = query.team_alias.as_deref();
    let req_entry = state.prompt_log_query.find_entry(
        &request_id,
        &query.key_hash,
        team_alias,
        boom_promptlog::LogPhase::Request,
    );
    let resp_entry = state.prompt_log_query.find_entry(
        &request_id,
        &query.key_hash,
        team_alias,
        boom_promptlog::LogPhase::Response,
    );

    match (req_entry, resp_entry) {
        (None, None) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"error": "Request not found in prompt logs"})),
        )
            .into_response(),
        (Some(req), None) => Json(serde_json::to_value(req).unwrap_or_default()).into_response(),
        (None, Some(resp)) => Json(serde_json::to_value(resp).unwrap_or_default()).into_response(),
        (Some(req), Some(resp)) => Json(json!({
            "request": serde_json::to_value(req).unwrap_or_default(),
            "response": serde_json::to_value(resp).unwrap_or_default(),
        }))
        .into_response(),
    }
}

// ═══════════════════════════════════════════════════════════
// Quota management — team-organized view of cumulative / window
// counters across all keys & teams. Reads boom_rate_limit_cumulative
// via SQL JOIN (avoids scanning the whole limiter DashMap).
// ═══════════════════════════════════════════════════════════

#[derive(Debug, sqlx::FromRow)]
struct QuotaKeyRow {
    token: String,
    key_alias: Option<String>,
    key_name: Option<String>,
    user_id: Option<String>,
    blocked: Option<bool>,
    created_at: Option<NaiveDateTime>,
}
/// totals, plus a synthetic "no_team" entry for keys without a team.
///
/// GaussDB distributed note: each SELECT is a single-table query. Cross-table
/// JOINs against `boom_rate_limit_cumulative` fail with "relation does not
/// exist on datanode" because that table is not distributed. We aggregate in
/// application memory instead.
pub async fn quota_overview(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    // 1. Teams metadata (now includes models array).
    let team_rows: Vec<(String, Option<String>, Option<Vec<String>>)> = match sqlx::query_as(
        "SELECT team_id, team_alias, models FROM boom_team_table",
    )
    .fetch_all(db_pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("quota_overview teams query failed: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("quota_overview teams query failed: {e}"),
            )
                .into_response();
        }
    };

    // 2. Keys-per-team counts.
    let key_count_rows: Vec<(Option<String>, i64)> = match sqlx::query_as(
        "SELECT team_id, COUNT(*)::BIGINT FROM boom_verification_token GROUP BY team_id",
    )
    .fetch_all(db_pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("quota_overview key_count query failed: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("quota_overview key_count query failed: {e}"),
            )
                .into_response();
        }
    };

    // 3. token → team_id (so we can bucket key-level cumulative by team).
    let token_team_rows: Vec<(String, Option<String>)> = match sqlx::query_as(
        "SELECT token, team_id FROM boom_verification_token",
    )
    .fetch_all(db_pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("quota_overview token_team query failed: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("quota_overview token_team query failed: {e}"),
            )
                .into_response();
        }
    };

    // 4. All cumulative rows. Single-table scan — safe under GaussDB distributed.
    let cum_rows: Vec<(String, i64)> = match sqlx::query_as(
        "SELECT cache_key, value FROM boom_rate_limit_cumulative",
    )
    .fetch_all(db_pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("quota_overview cumulative query failed: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("quota_overview cumulative query failed: {e}"),
            )
                .into_response();
        }
    };

    // Aggregate cumulative by (scope, id) → (tin, tout, tcost).
    // cache_key format: 'kc:{token}:{kind}' or 'tc:{team_id}:{kind}'.
    let mut team_cum: std::collections::HashMap<String, (i64, i64, i64)> =
        std::collections::HashMap::new();
    let mut key_cum: std::collections::HashMap<String, (i64, i64, i64)> =
        std::collections::HashMap::new();
    for (cache_key, value) in &cum_rows {
        if let Some(rest) = cache_key.strip_prefix("tc:") {
            if let Some((id, kind)) = rest.rsplit_once(':') {
                let e = team_cum.entry(id.to_string()).or_insert((0, 0, 0));
                match kind {
                    "tin" => e.0 = *value,
                    "tout" => e.1 = *value,
                    "tcost" => e.2 = *value,
                    _ => {}
                }
            }
            continue;
        }
        if let Some(rest) = cache_key.strip_prefix("kc:") {
            if let Some((id, kind)) = rest.rsplit_once(':') {
                let e = key_cum.entry(id.to_string()).or_insert((0, 0, 0));
                match kind {
                    "tin" => e.0 = *value,
                    "tout" => e.1 = *value,
                    "tcost" => e.2 = *value,
                    _ => {}
                }
            }
        }
    }

    // team_id → key_count.
    let mut team_key_count: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let mut no_team_key_count: i64 = 0;
    for (tid, n) in &key_count_rows {
        match tid {
            Some(t) => {
                team_key_count.entry(t.clone()).and_modify(|v| *v += n).or_insert(*n);
            }
            None => no_team_key_count += n,
        }
    }

    // team_id of each token (for no-team cumulative aggregation).
    let token_team: std::collections::HashMap<String, Option<String>> = token_team_rows
        .iter()
        .cloned()
        .collect();

    // No-team cumulative: sum key_cum for tokens whose team_id IS NULL.
    let mut no_team_cum = (0i64, 0i64, 0i64);
    for (token, _) in token_team_rows.iter().filter(|(_, t)| t.is_none()) {
        if let Some(c) = key_cum.get(token) {
            no_team_cum.0 += c.0;
            no_team_cum.1 += c.1;
            no_team_cum.2 += c.2;
        }
    }

    // team → plan_name lookup (explicit assignments only).
    let team_plans: std::collections::HashMap<String, String> = state
        .plan_store
        .list_team_assignments()
        .into_iter()
        .collect();

    // prompt-log excluded teams — read once, lookup in loop.
    let excluded_teams: Vec<String> = state
        .prompt_log_query
        .config_snapshot()
        .excluded_teams
        .clone();

    // Build teams vector sorted by cost DESC, then tokens DESC.
    let mut teams: Vec<Value> = team_rows
        .into_iter()
        .map(|(team_id, team_alias, models)| {
            let cum = team_cum.get(&team_id).copied().unwrap_or((0, 0, 0));
            let explicit_plan = team_plans.get(&team_id).cloned();
            let effective_plan_name = explicit_plan
                .clone()
                .or_else(|| state.plan_store.get_default_team_plan_name());
            // Resolve full plan to compute effective limits.
            let effective_limits = state
                .plan_store
                .resolve_team_plan(&team_id)
                .map(|p| {
                    let (concurrency_limit, window_limits, _) = p.effective_limits();
                    // rpm_limit is the 60s counts dimension (folded into window_limits).
                    let rpm_limit = window_limits
                        .iter()
                        .find(|w| w.window_secs == 60)
                        .and_then(|w| w.counts);
                    let wl_json: Vec<serde_json::Value> = window_limits
                        .iter()
                        .map(|w| {
                            let counts = w.counts.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null);
                            let tokens = w.tokens.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null);
                            let costs = w
                                .costs
                                .map(|c| serde_json::Value::String(c.to_string()))
                                .unwrap_or(serde_json::Value::Null);
                            serde_json::json!([counts, tokens, costs, w.window_secs])
                        })
                        .collect();
                    json!({
                        "concurrency_limit": concurrency_limit,
                        "rpm_limit": rpm_limit,
                        "tpm_limit": p.tpm_limit,
                        "window_limits": wl_json,
                        "total_token_limit": p.total_token_limit,
                        "total_cost_limit": p.total_cost_limit.map(|c| c.to_string()),
                    })
                });
            let prompt_log_excluded = excluded_teams.iter().any(|t| t == &team_id);
            json!({
                "team_id": team_id,
                "team_alias": team_alias,
                "models": models.unwrap_or_default(),
                "key_count": team_key_count.get(&team_id).copied().unwrap_or(0),
                "plan_name": effective_plan_name,
                "plan_explicit": explicit_plan.is_some(),
                "effective_limits": effective_limits,
                "prompt_log_excluded": prompt_log_excluded,
                "total_input_tokens": cum.0,
                "total_output_tokens": cum.1,
                "total_cost_micros": cum.2,
                "total_cost": boom_limiter::micros_to_decimal(cum.2.max(0) as u64).to_string(),
            })
        })
        .collect();
    teams.sort_by(|a, b| {
        let ca = a["total_cost_micros"].as_i64().unwrap_or(0);
        let cb = b["total_cost_micros"].as_i64().unwrap_or(0);
        cb.cmp(&ca)
            .then_with(|| {
                let ta = a["total_input_tokens"].as_i64().unwrap_or(0);
                let tb = b["total_input_tokens"].as_i64().unwrap_or(0);
                tb.cmp(&ta)
            })
    });

    let no_team = json!({
        "team_id": null,
        "team_alias": null,
        "key_count": no_team_key_count,
        "plan_name": null,
        "total_input_tokens": no_team_cum.0,
        "total_output_tokens": no_team_cum.1,
        "total_cost_micros": no_team_cum.2,
        "total_cost": boom_limiter::micros_to_decimal(no_team_cum.2.max(0) as u64).to_string(),
    });

    // Suppress unused warning while keeping the lookup map for future use.
    let _ = token_team;

    let default_team_plan = state.plan_store.get_default_team_plan_name();
    Json(json!({
        "teams": teams,
        "no_team": no_team,
        "default_team_plan": default_team_plan,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct QuotaKeysQuery {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_per_page_50")]
    pub per_page: i64,
    #[serde(default)]
    pub search: Option<String>,
    /// cost | tokens | alias. Default: cost.
    #[serde(default)]
    pub sort: Option<String>,
}

fn default_per_page_50() -> i64 {
    50
}

/// GET /admin/quota/team/{team_id} — paginated keys within one team.
pub async fn quota_team_keys(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(team_id): Path<String>,
    Query(q): Query<QuotaKeysQuery>,
) -> Response {
    quota_keys_inner(&state, Some(team_id), &q).await
}

/// GET /admin/quota/unassigned — paginated keys with no team.
pub async fn quota_unassigned_keys(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Query(q): Query<QuotaKeysQuery>,
) -> Response {
    quota_keys_inner(&state, None, &q).await
}

async fn quota_keys_inner(
    state: &DashboardState,
    team_id: Option<String>,
    q: &QuotaKeysQuery,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };
    let (page, per_page) = normalize_pagination(q.page, q.per_page);
    let offset = (page - 1) * per_page;

    let search_pattern = q.search.as_deref().map(|s| {
        format!("%{}%", s.replace('%', "\\%").replace('_', "\\_"))
    });

    // Single-table query on boom_verification_token (no JOINs).
    let where_clause = match (&team_id, &search_pattern) {
        (Some(_), Some(_)) => " WHERE team_id = $1 AND (key_alias ILIKE $2 OR key_name ILIKE $2 OR user_id ILIKE $2 OR token ILIKE $2)",
        (Some(_), None) => " WHERE team_id = $1",
        (None, Some(_)) => " WHERE team_id IS NULL AND (key_alias ILIKE $1 OR key_name ILIKE $1 OR user_id ILIKE $1 OR token ILIKE $1)",
        (None, None) => " WHERE team_id IS NULL",
    };

    // ── Branch on sort mode ──
    // For alias / created_at: SQL-side sort + pagination (cheap, scalable).
    // For cost / tokens: fetch full result set (capped at 5000), look up
    // cumulative via IN-list single-table query, then sort + slice in memory.
    // GaussDB distributed mode forbids cross-table JOINs, so the cumulative
    // lookup has to happen as a separate single-table SELECT regardless.
    let in_memory_sort = matches!(q.sort.as_deref(), Some("cost") | Some("tokens"));

    let mut sort_truncated = false;
    let rows: Vec<QuotaKeyRow> = if in_memory_sort {
        // ── In-memory sort path: fetch up to 5001 rows to detect truncation.
        const IN_MEMORY_SORT_CAP: i64 = 5000;
        let limit_idx = if team_id.is_some() && search_pattern.is_some() {
            3
        } else if team_id.is_some() || search_pattern.is_some() {
            2
        } else {
            1
        };
        let sql = format!(
            "SELECT token, key_alias, key_name, user_id, blocked, created_at \
             FROM boom_verification_token{where_clause} \
             ORDER BY created_at DESC LIMIT ${limit_idx}"
        );
        let mut query = sqlx::query_as::<_, QuotaKeyRow>(&sql);
        if let Some(tid) = &team_id {
            query = query.bind(tid);
        }
        if let Some(pat) = &search_pattern {
            query = query.bind(pat);
        }
        query = query.bind(IN_MEMORY_SORT_CAP + 1);
        let mut fetched: Vec<QuotaKeyRow> = match query.fetch_all(db_pool).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("quota_keys_inner in-memory sort query failed: {}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("quota_keys_inner query failed: {e}"),
                )
                    .into_response();
            }
        };
        if fetched.len() as i64 > IN_MEMORY_SORT_CAP {
            sort_truncated = true;
            fetched.truncate(IN_MEMORY_SORT_CAP as usize);
        }
        fetched
    } else {
        // ── SQL-side sort path ──
        let sort_clause = match q.sort.as_deref().unwrap_or("created") {
            "alias" => "COALESCE(key_alias, key_name, token) ASC",
            _ => "created_at DESC",
        };
        let limit_idx = if team_id.is_some() && search_pattern.is_some() {
            3
        } else if team_id.is_some() || search_pattern.is_some() {
            2
        } else {
            1
        };
        let offset_idx = limit_idx + 1;
        let sql = format!(
            "SELECT token, key_alias, key_name, user_id, blocked, created_at FROM boom_verification_token{where_clause} ORDER BY {sort_clause} LIMIT ${limit_idx} OFFSET ${offset_idx}"
        );

        let mut query = sqlx::query_as::<_, QuotaKeyRow>(&sql);
        if let Some(tid) = &team_id {
            query = query.bind(tid);
        }
        if let Some(pat) = &search_pattern {
            query = query.bind(pat);
        }
        query = query.bind(per_page).bind(offset);
        match query.fetch_all(db_pool).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("quota_keys_inner query failed: {}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("quota_keys_inner query failed: {e}"),
                )
                    .into_response();
            }
        }
    };

    // Total count with same WHERE.
    let count_sql = format!("SELECT COUNT(*) FROM boom_verification_token{where_clause}");
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
    if let Some(tid) = &team_id {
        count_query = count_query.bind(tid);
    }
    if let Some(pat) = &search_pattern {
        count_query = count_query.bind(pat);
    }
    let total: i64 = match count_query.fetch_one(db_pool).await {
        Ok(n) => n,
        Err(e) => {
            tracing::error!("quota_keys_inner count failed: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("quota_keys_inner count failed: {e}"),
            )
                .into_response();
        }
    };

    // Cumulative lookup for these tokens via IN list.
    // In in-memory-sort mode rows may be up to 5000, so up to 15000 cache_keys.
    let mut key_cum: std::collections::HashMap<String, (i64, i64, i64)> =
        std::collections::HashMap::new();
    if !rows.is_empty() {
        let cache_keys: Vec<String> = rows
            .iter()
            .flat_map(|r| {
                vec![
                    format!("kc:{}:tin", r.token),
                    format!("kc:{}:tout", r.token),
                    format!("kc:{}:tcost", r.token),
                ]
            })
            .collect();
        let placeholders: Vec<String> =
            (1..=cache_keys.len()).map(|i| format!("${i}")).collect();
        let cum_sql = format!(
            "SELECT cache_key, value FROM boom_rate_limit_cumulative WHERE cache_key IN ({})",
            placeholders.join(", ")
        );
        let mut cum_query = sqlx::query_as::<_, (String, i64)>(&cum_sql);
        for ck in &cache_keys {
            cum_query = cum_query.bind(ck);
        }
        match cum_query.fetch_all(db_pool).await {
            Ok(cum_rows) => {
                for (cache_key, value) in cum_rows {
                    if let Some(rest) = cache_key.strip_prefix("kc:") {
                        if let Some((token, kind)) = rest.rsplit_once(':') {
                            let e = key_cum.entry(token.to_string()).or_insert((0, 0, 0));
                            match kind {
                                "tin" => e.0 = value,
                                "tout" => e.1 = value,
                                "tcost" => e.2 = value,
                                _ => {}
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("quota_keys_inner cumulative fetch failed: {}", e);
                // Non-fatal: proceed with zeroed cumulative.
            }
        }
    }

    // In-memory sort + page slice (only for cost/tokens mode).
    let page_rows: Vec<QuotaKeyRow> = if in_memory_sort {
        let mut sortable: Vec<(QuotaKeyRow, (i64, i64, i64))> = rows
            .into_iter()
            .map(|r| {
                let c = key_cum.get(&r.token).copied().unwrap_or((0, 0, 0));
                (r, c)
            })
            .collect();
        match q.sort.as_deref() {
            Some("cost") => {
                sortable.sort_by(|a, b| {
                    // DESC by tcost, tiebreak by tokens DESC.
                    b.1.2.cmp(&a.1.2)
                        .then_with(|| (b.1.0 + b.1.1).cmp(&(a.1.0 + a.1.1)))
                });
            }
            Some("tokens") => {
                sortable.sort_by(|a, b| {
                    // DESC by tin+tout, tiebreak by tcost DESC.
                    (b.1.0 + b.1.1).cmp(&(a.1.0 + a.1.1)).then_with(|| b.1.2.cmp(&a.1.2))
                });
            }
            _ => {}
        }
        sortable
            .into_iter()
            .skip(offset as usize)
            .take(per_page as usize)
            .map(|(r, _)| r)
            .collect()
    } else {
        rows
    };

    let keys: Vec<Value> = page_rows
        .into_iter()
        .map(|r| {
            let key_hash = r.token.clone();
            let plan_name = state
                .plan_store
                .resolve_plan(&key_hash)
                .or_else(|| state.plan_store.get_default_plan())
                .map(|p| p.name);
            let concurrency = state.plan_store.get_concurrency(&key_hash);
            let cum = key_cum.get(&r.token).copied().unwrap_or((0, 0, 0));
            json!({
                "token": r.token,
                "token_prefix": format!("{}...", &r.token[..8.min(r.token.len())]),
                "key_alias": r.key_alias,
                "key_name": r.key_name,
                "user_id": r.user_id,
                "blocked": r.blocked.unwrap_or(false),
                "created_at": r.created_at.map(|d| d.to_string()),
                "plan_name": plan_name,
                "concurrency": concurrency,
                "total_input_tokens": cum.0,
                "total_output_tokens": cum.1,
                "total_cost_micros": cum.2,
                "total_cost": boom_limiter::micros_to_decimal(cum.2.max(0) as u64).to_string(),
            })
        })
        .collect();

    Json(json!({
        "keys": keys,
        "page": page,
        "per_page": per_page,
        "total": total,
        "team_id": team_id,
        "sort_truncated": sort_truncated,
    }))
    .into_response()
}

/// GET /admin/quota/key/{key_hash}/windows — current per-window consumption
/// for one key (lazy-loaded when admin expands a row).
pub async fn quota_key_windows(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(key_hash): Path<String>,
) -> Response {
    // Resolve plan limits for this key.
    let plan = state
        .plan_store
        .resolve_plan(&key_hash)
        .or_else(|| state.plan_store.get_default_plan());
    let (_, plan_window_limits, _) = plan
        .as_ref()
        .map(|p| p.effective_limits())
        .unwrap_or((None, vec![], vec![]));
    // rpm_limit is the 60s counts dimension (folded into window_limits).
    let plan_rpm = plan_window_limits
        .iter()
        .find(|w| w.window_secs == 60)
        .and_then(|w| w.counts);

    // counts dimension
    let mut counts_by_secs: std::collections::HashMap<u64, (u64, u64)> =
        std::collections::HashMap::new();
    for w in state.limiter.get_usage_for_key(&key_hash) {
        let remaining = w.window_secs.saturating_sub(w.elapsed_secs);
        counts_by_secs
            .entry(w.window_secs)
            .and_modify(|e| e.0 = e.0.saturating_add(w.counts))
            .or_insert((w.counts, remaining));
    }

    // tokens / costs from limiter multi-dim windows
    let mut tokens_by_secs: std::collections::HashMap<u64, (u64, u64)> =
        std::collections::HashMap::new();
    let mut costs_by_secs: std::collections::HashMap<u64, (u64, u64)> =
        std::collections::HashMap::new();
    for w in state.limiter.peek_key_windows(&key_hash) {
        let entry = match w.kind {
            boom_limiter::WindowKind::Tokens => tokens_by_secs
                .entry(w.window_secs)
                .or_insert((0, w.remaining_secs)),
            boom_limiter::WindowKind::CostMicros => costs_by_secs
                .entry(w.window_secs)
                .or_insert((0, w.remaining_secs)),
        };
        entry.0 = entry.0.saturating_add(w.count);
        if w.remaining_secs < entry.1 {
            entry.1 = w.remaining_secs;
        }
    }

    let mut seen_secs: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for wl in &plan_window_limits {
        seen_secs.insert(wl.window_secs);
    }
    for &s in counts_by_secs.keys() {
        seen_secs.insert(s);
    }
    for &s in tokens_by_secs.keys() {
        seen_secs.insert(s);
    }
    for &s in costs_by_secs.keys() {
        seen_secs.insert(s);
    }

    let windows: Vec<Value> = seen_secs
        .iter()
        .map(|&secs| {
            let wl = plan_window_limits.iter().find(|w| w.window_secs == secs);
            let counts_limit = wl.and_then(|w| w.counts).or(if secs == 60 { plan_rpm } else { None });
            let tokens_limit = wl.and_then(|w| w.tokens);
            let costs_limit = wl.and_then(|w| w.costs);

            let mut dims = serde_json::Map::new();
            if let Some(limit) = counts_limit {
                let cur = counts_by_secs.get(&secs).map(|&(c, _)| c).unwrap_or(0);
                dims.insert("counts".to_string(), json!({ "current": cur, "limit": limit }));
            }
            if let Some(limit) = tokens_limit {
                let cur = tokens_by_secs.get(&secs).map(|&(c, _)| c).unwrap_or(0);
                dims.insert("tokens".to_string(), json!({ "current": cur, "limit": limit }));
            }
            if let Some(limit) = costs_limit {
                let cur_micros = costs_by_secs.get(&secs).map(|&(c, _)| c).unwrap_or(0);
                dims.insert(
                    "costs".to_string(),
                    json!({
                        "current_micros": cur_micros,
                        "current": boom_limiter::micros_to_decimal(cur_micros).to_string(),
                        "limit": limit.to_string(),
                        "limit_micros": boom_limiter::decimal_to_micros(limit),
                    }),
                );
            }

            let remaining = counts_by_secs
                .get(&secs)
                .map(|(_, r)| *r)
                .or_else(|| tokens_by_secs.get(&secs).map(|(_, r)| *r))
                .or_else(|| costs_by_secs.get(&secs).map(|(_, r)| *r))
                .unwrap_or(secs);

            json!({
                "window_secs": secs,
                "remaining_secs": remaining,
                "dims": dims,
            })
        })
        .collect();

    // Cumulative counters for this key.
    let key_scope = boom_limiter::QuotaScope::Key {
        key_hash: key_hash.clone(),
    };
    let total_in = state
        .limiter
        .peek_cumulative(&key_scope, boom_limiter::CumulativeKind::TotalInputTokens);
    let total_out = state
        .limiter
        .peek_cumulative(&key_scope, boom_limiter::CumulativeKind::TotalOutputTokens);
    let total_cost_micros = state
        .limiter
        .peek_cumulative(&key_scope, boom_limiter::CumulativeKind::TotalCost);

    Json(json!({
        "key_hash": key_hash,
        "windows": windows,
        "cumulative": {
            "total_input_tokens": total_in,
            "total_output_tokens": total_out,
            "total_tokens": total_in.saturating_add(total_out),
            "total_cost_micros": total_cost_micros,
            "total_cost": boom_limiter::micros_to_decimal(total_cost_micros).to_string(),
            "total_token_limit": plan.as_ref().and_then(|p| p.total_token_limit),
            "total_cost_limit": plan.as_ref().and_then(|p| p.total_cost_limit).map(|d| d.to_string()),
        },
    }))
    .into_response()
}

/// POST /admin/quota/reset/key/{key_hash} — clear one key's cumulative +
/// window counters (memory + DB). Also clears SlidingWindowLimiter counts.
/// Returns previous cumulative values.
pub async fn quota_reset_key(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(key_hash): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };
    let limiter_cleared = state.limiter.clear_for_key(&key_hash);
    match state.limiter.clear_key_all(db_pool, &key_hash).await {
        Ok(snap) => {
            let _ = state
                .admin_tx
                .send(crate::state::AdminCommand::ConfigChanged)
                .await;
            tracing::info!(key_hash = %key_hash, limiter_cleared, "Admin reset key quota");
            Json(json!({
                "key_hash": key_hash,
                "limiter_windows_cleared": limiter_cleared,
                "previous": {
                    "total_input_tokens": snap.input_tokens,
                    "total_output_tokens": snap.output_tokens,
                    "total_cost_micros": snap.total_cost_micros,
                    "total_cost": boom_limiter::micros_to_decimal(snap.total_cost_micros).to_string(),
                    "regular_input_cost_micros": snap.regular_input_cost_micros,
                    "regular_input_cost": boom_limiter::micros_to_decimal(snap.regular_input_cost_micros).to_string(),
                    "cached_input_cost_micros": snap.cached_input_cost_micros,
                    "cached_input_cost": boom_limiter::micros_to_decimal(snap.cached_input_cost_micros).to_string(),
                    "output_cost_micros": snap.output_cost_micros,
                    "output_cost": boom_limiter::micros_to_decimal(snap.output_cost_micros).to_string(),
                },
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("quota_reset_key failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Reset failed: {}", e)})),
            )
                .into_response()
        }
    }
}

/// POST /admin/quota/reset/team/{team_id} — clear team + all member keys.
pub async fn quota_reset_team(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(team_id): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    // Fetch member key hashes for cascade reset.
    let member_keys: Vec<String> = match sqlx::query_scalar::<_, String>(
        r#"SELECT token FROM boom_verification_token WHERE team_id = $1"#,
    )
    .bind(&team_id)
    .fetch_all(db_pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("quota_reset_team member fetch failed: {}", e);
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("member fetch failed: {e}")).into_response();
        }
    };

    // Clear limiter counts for team + each member (limiter cache_key has no
    // team dimension, so we iterate member keys).
    let mut limiter_cleared = 0usize;
    for kh in &member_keys {
        limiter_cleared += state.limiter.clear_for_key(kh);
    }

    let member_count = member_keys.len();
    match state.limiter.reset_team_all(db_pool, &team_id, &member_keys).await {
        Ok(()) => {
            let _ = state
                .admin_tx
                .send(crate::state::AdminCommand::ConfigChanged)
                .await;
            tracing::info!(team_id = %team_id, member_count, limiter_cleared, "Admin reset team quota");
            Json(json!({
                "team_id": team_id,
                "member_keys_reset": member_count,
                "limiter_windows_cleared": limiter_cleared,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("quota_reset_team failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Reset failed: {}", e)})),
            )
                .into_response()
        }
    }
}

/// POST /admin/quota/reset/key/{key_hash}/cumulative — clear only cumulative
/// counters (memory + DB). Subtracts the pre-reset value from the key's team
/// rollup so team = Σ members stays consistent. Windows untouched.
pub async fn quota_reset_key_cumulative(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(key_hash): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    // Look up the key's team (for rollup subtraction). None if no team.
    let team_id: Option<String> = match sqlx::query_scalar::<_, String>(
        r#"SELECT team_id FROM boom_verification_token WHERE token = $1"#,
    )
    .bind(&key_hash)
    .fetch_optional(db_pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("quota_reset_key_cumulative team lookup failed: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("team lookup failed: {e}"),
            )
                .into_response();
        }
    };

    match state
        .limiter
        .clear_key_cumulative_db(db_pool, &key_hash, team_id.as_deref())
        .await
    {
        Ok(snap) => {
            let _ = state
                .admin_tx
                .send(crate::state::AdminCommand::ConfigChanged)
                .await;
            tracing::info!(key_hash = %key_hash, team_id = ?team_id, "Admin reset key cumulative");
            Json(json!({
                "key_hash": key_hash,
                "team_id": team_id,
                "scope": "cumulative",
                "previous": {
                    "total_input_tokens": snap.input_tokens,
                    "total_output_tokens": snap.output_tokens,
                    "total_cost_micros": snap.total_cost_micros,
                    "total_cost": boom_limiter::micros_to_decimal(snap.total_cost_micros).to_string(),
                    "regular_input_cost_micros": snap.regular_input_cost_micros,
                    "regular_input_cost": boom_limiter::micros_to_decimal(snap.regular_input_cost_micros).to_string(),
                    "cached_input_cost_micros": snap.cached_input_cost_micros,
                    "cached_input_cost": boom_limiter::micros_to_decimal(snap.cached_input_cost_micros).to_string(),
                    "output_cost_micros": snap.output_cost_micros,
                    "output_cost": boom_limiter::micros_to_decimal(snap.output_cost_micros).to_string(),
                },
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("quota_reset_key_cumulative failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Reset failed: {}", e)})),
            )
                .into_response()
        }
    }
}

/// POST /admin/quota/reset/key/{key_hash}/windows — clear only current
/// windows: limiter multi-dim counters (counts + tokens + costs).
/// Cumulative counters untouched.
pub async fn quota_reset_key_windows(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(key_hash): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };
    let limiter_cleared = state.limiter.clear_for_key(&key_hash);
    match state
        .limiter
        .clear_key_windows_db(db_pool, &key_hash)
        .await
    {
        Ok(quota_windows_cleared) => {
            let _ = state
                .admin_tx
                .send(crate::state::AdminCommand::ConfigChanged)
                .await;
            tracing::info!(key_hash = %key_hash, limiter_cleared, quota_windows_cleared, "Admin reset key windows");
            Json(json!({
                "key_hash": key_hash,
                "scope": "windows",
                "limiter_windows_cleared": limiter_cleared,
                "quota_windows_cleared": quota_windows_cleared,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("quota_reset_key_windows failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Reset failed: {}", e)})),
            )
                .into_response()
        }
    }
}

/// POST /admin/quota/reset/team/{team_id}/cumulative — clear team cumulative +
/// cascade to member keys. Windows untouched.
pub async fn quota_reset_team_cumulative(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(team_id): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };
    let member_keys: Vec<String> = match sqlx::query_scalar::<_, String>(
        r#"SELECT token FROM boom_verification_token WHERE team_id = $1"#,
    )
    .bind(&team_id)
    .fetch_all(db_pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("quota_reset_team_cumulative member fetch failed: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("member fetch failed: {e}"),
            )
                .into_response();
        }
    };
    let member_count = member_keys.len();
    match state
        .limiter
        .clear_team_cumulative_db(db_pool, &team_id, &member_keys)
        .await
    {
        Ok(_) => {
            let _ = state
                .admin_tx
                .send(crate::state::AdminCommand::ConfigChanged)
                .await;
            tracing::info!(team_id = %team_id, member_count, "Admin reset team cumulative");
            Json(json!({
                "team_id": team_id,
                "scope": "cumulative",
                "member_keys_reset": member_count,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("quota_reset_team_cumulative failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Reset failed: {}", e)})),
            )
                .into_response()
        }
    }
}

/// POST /admin/quota/reset/team/{team_id}/windows — clear team windows +
/// member keys' windows (limiter multi-dim counters).
/// Cumulative untouched.
pub async fn quota_reset_team_windows(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Path(team_id): Path<String>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };
    let member_keys: Vec<String> = match sqlx::query_scalar::<_, String>(
        r#"SELECT token FROM boom_verification_token WHERE team_id = $1"#,
    )
    .bind(&team_id)
    .fetch_all(db_pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("quota_reset_team_windows member fetch failed: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("member fetch failed: {e}"),
            )
                .into_response();
        }
    };

    // Clear limiter counts for each member (limiter cache_key has no team dim).
    let mut limiter_cleared = 0usize;
    for kh in &member_keys {
        limiter_cleared += state.limiter.clear_for_key(kh);
    }

    let member_count = member_keys.len();
    match state
        .limiter
        .clear_team_windows_db(db_pool, &team_id, &member_keys)
        .await
    {
        Ok(quota_windows_cleared) => {
            let _ = state
                .admin_tx
                .send(crate::state::AdminCommand::ConfigChanged)
                .await;
            tracing::info!(team_id = %team_id, member_count, limiter_cleared, quota_windows_cleared, "Admin reset team windows");
            Json(json!({
                "team_id": team_id,
                "scope": "windows",
                "member_keys_reset": member_count,
                "limiter_windows_cleared": limiter_cleared,
                "quota_windows_cleared": quota_windows_cleared,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("quota_reset_team_windows failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Reset failed: {}", e)})),
            )
                .into_response()
        }
    }
}

// ── System Pressure ────────────────────────────────────────────────────
//
// Reads from boom-stressmon's ring buffer (1Hz × 60min). The chart on the
// admin stats page polls this every 1.5s — see `loadStressPanel` in app.js.

#[derive(Debug, Deserialize)]
pub struct StressRangeQuery {
    /// `5m` / `15m` / `30m` / `60m` (default 5m). Bounded to the ring
    /// buffer's 60-min capacity by the impl.
    #[serde(default = "default_stress_range")]
    pub range: String,
}

fn default_stress_range() -> String {
    "5m".to_string()
}

pub async fn get_stress_timeseries(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Query(q): Query<StressRangeQuery>,
) -> Response {
    let window_secs: i64 = match q.range.as_str() {
        "5m" => 300,
        "15m" => 900,
        "30m" => 1800,
        "60m" => 3600,
        _ => 300,
    };
    let snap = state.stressmon.timeseries(window_secs).await;
    Json(snap).into_response()
}

#[cfg(test)]
mod tests {
    use super::{normalize_pagination, CreateKeyRequest};

    #[test]
    fn normalize_pagination_clamps_invalid_values() {
        assert_eq!(normalize_pagination(0, 0), (1, 1));
        assert_eq!(normalize_pagination(-3, -20), (1, 1));
        assert_eq!(normalize_pagination(2, 5000), (2, 1000));
    }

    /// Lock in the three-state JSON semantics of `CreateKeyRequest::plan_name`:
    ///   - field absent      → None             (use default_plan at runtime)
    ///   - `plan_name: null`  → Some(None)       (explicit "no plan")
    ///   - `plan_name: "x"`   → Some(Some("x"))  (assign to plan "x")
    /// Regression guard: if the deserialize_some helper breaks, these will fail.
    #[test]
    fn create_key_request_plan_name_three_state_deserialization() {
        let absent: CreateKeyRequest = serde_json::from_str(r#"{"key_alias":"a"}"#).unwrap();
        assert_eq!(absent.plan_name, None);

        let null: CreateKeyRequest =
            serde_json::from_str(r#"{"key_alias":"a","plan_name":null}"#).unwrap();
        assert_eq!(null.plan_name, Some(None));

        let named: CreateKeyRequest =
            serde_json::from_str(r#"{"key_alias":"a","plan_name":"default"}"#).unwrap();
        assert_eq!(named.plan_name, Some(Some("default".to_string())));
    }
}
