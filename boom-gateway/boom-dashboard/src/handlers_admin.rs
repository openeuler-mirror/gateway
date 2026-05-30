use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use chrono::NaiveDateTime;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::FromRow;
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::{hash_token, AdminSession};
use crate::state::DashboardState;

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
    pub concurrency_limit: Option<u32>,
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    #[serde(default)]
    pub window_limits: Vec<(u64, u64)>,
    #[serde(default)]
    pub schedule: Vec<boom_limiter::ScheduleSlot>,
}

pub async fn upsert_plan(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(req): Json<UpsertPlanRequest>,
) -> Json<Value> {
    let plan = boom_limiter::RateLimitPlan {
        name: req.name.clone(),
        concurrency_limit: req.concurrency_limit,
        rpm_limit: req.rpm_limit,
        window_limits: req.window_limits,
        schedule: req.schedule.clone(),
    };

    // Persist to DB via PlanStore.
    if let Some(ref pool) = state.db_pool {
        if let Err(e) = state.plan_store.upsert_plan_db(pool, &plan).await {
            tracing::error!("Failed to persist plan to DB: {}", e);
        }
    } else {
        state.plan_store.upsert_plan(plan);
    }

    let _ = state.admin_tx.send(crate::state::AdminCommand::ConfigChanged).await;
    Json(json!({"ok": true, "plan_name": req.name}))
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
    user_id: Option<String>,
    team_id: Option<String>,
    /// litellm stores models as text[] in PostgreSQL.
    models: Vec<String>,
    /// spend has a NOT NULL DEFAULT 0.0 constraint.
    spend: f64,
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
            r#"SELECT token, key_name, key_alias, user_id, team_id, models,
                      spend, blocked, rpm_limit, tpm_limit, max_budget,
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
            r#"SELECT token, key_name, key_alias, user_id, team_id, models,
                      spend, blocked, rpm_limit, tpm_limit, max_budget,
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
            let plan_name = state.plan_store.get_plan_name(&r.token);

            json!({
                "token_prefix": token_prefix,
                "token_hash": r.token,
                "key_name": r.key_name,
                "key_alias": r.key_alias,
                "user_id": r.user_id,
                "team_id": r.team_id,
                "models": r.models,
                "spend": r.spend,
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
                "plan_name": plan_name,
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

#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
    pub key_alias: Option<String>,
    /// Legacy display name. Defaults to key_alias if not provided.
    pub key_name: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub models: Option<Vec<String>>,
    pub max_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub expires: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub plan_name: Option<String>,
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

    // 1. Generate raw key: sk- + 32 bytes random hex.
    let raw_key = format!("sk-{}", hex::encode(Uuid::new_v4().as_bytes()));
    let token_hash = hash_token(&raw_key);

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
           (token, key_name, key_alias, user_id, team_id, models, spend, blocked,
            rpm_limit, tpm_limit, max_budget, budget_duration, expires,
            metadata, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, 0.0, false, $7, $8, $9, $10, $11, $12, NOW(), NOW())"#,
    )
    .bind(&token_hash)
    .bind(&key_name)
    .bind(&req.key_alias)
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

    // 4. Optionally assign to plan.
    if let Some(ref plan_name) = req.plan_name {
        if let Err(e) = state.plan_store.assign_key_db(db_pool, &token_hash, plan_name).await {
            tracing::warn!("Key created but plan assignment failed: {}", e);
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
    pub models: Option<Vec<String>>,
    pub max_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
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

    let models_list: Option<Vec<String>> = req.models.as_ref().map(|v| {
        if v.iter().any(|m| m == "all-team-models") {
            vec!["all-team-models".to_string()]
        } else {
            v.clone()
        }
    });

    let expires: Option<NaiveDateTime> = req
        .expires
        .as_deref()
        .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok());

    let result = sqlx::query(
        r#"UPDATE "boom_verification_token"
           SET key_name = COALESCE($2, key_name),
               key_alias = COALESCE($3, key_alias),
               user_id = COALESCE($4, user_id),
               models = COALESCE($5, models),
               max_budget = COALESCE($6, max_budget),
               budget_duration = COALESCE($7, budget_duration),
               rpm_limit = COALESCE($8, rpm_limit),
               tpm_limit = COALESCE($9, tpm_limit),
               expires = COALESCE($10, expires),
               metadata = COALESCE($11, metadata),
               updated_at = NOW()
           WHERE token = $1"#,
    )
    .bind(&token_hash)
    .bind(&req.key_name)
    .bind(&req.key_alias)
    .bind(&req.user_id)
    .bind(&models_list)
    .bind(req.max_budget)
    .bind(&req.budget_duration)
    .bind(req.rpm_limit)
    .bind(req.tpm_limit)
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
    pub plan_name: String,
}

pub async fn assign_key(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(req): Json<AssignRequest>,
) -> Response {
    if let Some(ref pool) = state.db_pool {
        match state.plan_store.assign_key_db(pool, &req.key_hash, &req.plan_name).await {
            Ok(()) => {
                let _ = state.admin_tx.send(crate::state::AdminCommand::ConfigChanged).await;
                Json(json!({"ok": true})).into_response()
            }
            Err(e) => (axum::http::StatusCode::BAD_REQUEST, e).into_response(),
        }
    } else {
        match state.plan_store.assign_key(&req.key_hash, &req.plan_name) {
            Ok(()) => {
                let _ = state.admin_tx.send(crate::state::AdminCommand::ConfigChanged).await;
                Json(json!({"ok": true})).into_response()
            }
            Err(e) => (axum::http::StatusCode::BAD_REQUEST, e).into_response(),
        }
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
                "count": w.count,
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
                skipped.push(json!({
                    "key_alias": alias,
                    "reason": "duplicate",
                }));
                continue;
            }
        }

        let raw_key = format!("sk-{}", hex::encode(Uuid::new_v4().as_bytes()));
        let token_hash = hash_token(&raw_key);

        let expires: Option<NaiveDateTime> = req
            .expires
            .as_deref()
            .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok());

        let models_list: Vec<String> = req.models.clone().unwrap_or_default();
        let key_name = req.key_name.clone().or(req.key_alias.clone());

        let result = sqlx::query(
            r#"INSERT INTO "boom_verification_token"
               (token, key_name, key_alias, user_id, team_id, models, spend, blocked,
                rpm_limit, tpm_limit, max_budget, budget_duration, expires,
                metadata, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, 0.0, false, $7, $8, $9, $10, $11, $12, NOW(), NOW())"#,
        )
        .bind(&token_hash)
        .bind(&key_name)
        .bind(&req.key_alias)
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
                // Optionally assign to plan.
                if let Some(ref plan_name) = req.plan_name {
                    if let Err(e) = state.plan_store.assign_key_db(db_pool, &token_hash, plan_name).await {
                        tracing::warn!("Batch: key created but plan assignment failed: {}", e);
                    }
                }
                created.push(json!({
                    "key": raw_key,
                    "token_hash": token_hash,
                    "key_alias": req.key_alias,
                }));
            }
            Err(e) => {
                tracing::error!("Dashboard batch_create_keys insert failed: {}", e);
                skipped.push(json!({
                    "key_alias": req.key_alias,
                    "reason": "db_error",
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
            json!({
                "id": r.id,
                "model_name": r.model_name,
                "litellm_model": r.litellm_model,
                "api_key": r.api_key,
                "api_key_env": r.api_key_env.unwrap_or(false),
                "api_base": r.api_base,
                "api_version": r.api_version,
                "aws_region_name": r.aws_region_name,
                "rpm": r.rpm,
                "tpm": r.tpm,
                "timeout": r.timeout,
                "temperature": r.temperature,
                "max_tokens": r.max_tokens,
                "enabled": r.enabled.unwrap_or(true),
                "auto_disabled": r.auto_disabled.unwrap_or(false),
                "source": r.source,
                "deployment_id": r.deployment_id,
                "quota_count_ratio": r.quota_count_ratio.unwrap_or(1),
                "max_inflight_queue_len": r.max_inflight_queue_len,
                "max_context_len": r.max_context_len,
                "created_at": r.created_at.map(|d| d.to_string()),
                "updated_at": r.updated_at.map(|d| d.to_string()),
            })
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
// Config management (boom_config KV store)
// ═══════════════════════════════════════════════════════════

pub async fn get_config(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    #[derive(Debug, FromRow)]
    #[allow(dead_code)]
    struct ConfigRow {
        key: String,
        value: serde_json::Value,
        updated_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    let rows: Vec<ConfigRow> = match sqlx::query_as(
        r#"SELECT key, value, updated_at FROM boom_config ORDER BY key"#,
    )
    .fetch_all(db_pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Dashboard get_config query failed: {}", e);
            return Json(json!({"error": "Internal error"})).into_response();
        }
    };

    let config: std::collections::HashMap<String, Value> = rows
        .into_iter()
        .map(|r| (r.key, r.value))
        .collect();

    Json(json!({"config": config})).into_response()
}

#[derive(Debug, Deserialize)]
pub struct PatchConfigRequest {
    pub key: String,
    pub value: serde_json::Value,
}

pub async fn patch_config(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Json(req): Json<PatchConfigRequest>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let result: Result<(), sqlx::Error> = async {
        boom_core::gaussdb_upsert!(
            db_pool,
            || sqlx::query(
                r#"UPDATE boom_config SET value = $2, updated_at = NOW() WHERE key = $1"#,
            )
            .bind(&req.key)
            .bind(&req.value),
            || sqlx::query(
                r#"INSERT INTO boom_config (key, value) VALUES ($1, $2)"#,
            )
            .bind(&req.key)
            .bind(&req.value)
        )
    }.await;

    if let Err(e) = result {
        tracing::error!("Dashboard patch_config failed: {}", e);
        return Json(json!({"error": "Internal error"})).into_response();
    }

    tracing::info!(key = %req.key, "Config updated");
    Json(json!({"ok": true, "key": req.key})).into_response()
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
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    deployment_id: Option<String>,
    client_ip: Option<String>,
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
                  rl.input_tokens, rl.output_tokens, rl.duration_ms, rl.created_at,
                  rl.deployment_id, rl.client_ip
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
                "duration_ms": r.duration_ms,
                "created_at": r.created_at.map(|d| d.to_rfc3339()),
                "client_ip": r.client_ip,
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

#[derive(Debug, sqlx::FromRow)]
struct TeamUsageRow {
    team_id: String,
    team_alias: Option<String>,
    models: Option<Vec<String>>,
    key_count: i64,
    total_input_tokens: Option<i64>,
    total_output_tokens: Option<i64>,
    request_count: i64,
}

pub async fn list_teams(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Response {
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            return Json(json!({"error": "Database not available"})).into_response();
        }
    };

    let sql = r#"
        SELECT bt.team_id,
               bt.team_alias,
               bt.models,
               COALESCE(kc.cnt, 0) AS key_count,
               COALESCE(rl.total_input, 0) AS total_input_tokens,
               COALESCE(rl.total_output, 0) AS total_output_tokens,
               COALESCE(rl.cnt, 0) AS request_count
        FROM boom_team_table bt
        LEFT JOIN (
            SELECT team_id, COUNT(*) AS cnt FROM boom_verification_token GROUP BY team_id
        ) kc ON bt.team_id = kc.team_id
        LEFT JOIN (
            SELECT team_id,
                   SUM(input_tokens)  AS total_input,
                   SUM(output_tokens) AS total_output,
                   COUNT(*)           AS cnt
            FROM boom_request_log GROUP BY team_id
        ) rl ON bt.team_id = rl.team_id
        ORDER BY (COALESCE(rl.total_input, 0) + COALESCE(rl.total_output, 0)) DESC
    "#;

    let rows: Vec<TeamUsageRow> = match sqlx::query_as(sql).fetch_all(db_pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Dashboard list_teams query failed: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
            )
                .into_response();
        }
    };

    let teams: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "team_id": r.team_id,
                "team_alias": r.team_alias,
                "models": r.models,
                "key_count": r.key_count,
                "total_input_tokens": r.total_input_tokens,
                "total_output_tokens": r.total_output_tokens,
                "request_count": r.request_count,
            })
        })
        .collect();

    Json(json!({ "teams": teams })).into_response()
}

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

    // Normalize: if models contains "all-team-models", treat as full access.
    let models = if req.models.iter().any(|m| m == "all-team-models") {
        vec!["all-team-models".to_string()]
    } else {
        req.models
    };

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

    // Normalize models if provided.
    let models = req.models.map(|ms| {
        if ms.iter().any(|m| m == "all-team-models") {
            vec!["all-team-models".to_string()]
        } else {
            ms
        }
    });

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
// Model Statistics
// ═══════════════════════════════════════════════════════════

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
struct ModelStatsRow {
    model: String,
    total_requests: i64,
    success_count: i64,
    error_count: i64,
    total_input_tokens: i64,
    total_output_tokens: i64,
    avg_duration_ms: i32,
    last_request_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn get_model_stats(
    _session: AdminSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Response {
    let pool = match &state.db_pool {
        Some(p) => p,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    let stats = sqlx::query_as::<_, ModelStatsRow>(
        r#"SELECT COALESCE(model_name, model) AS model,
                  COUNT(*) as total_requests,
                  COUNT(*) FILTER (WHERE status_code = 200) as success_count,
                  COUNT(*) FILTER (WHERE status_code != 200) as error_count,
                  COALESCE(SUM(input_tokens), 0) as total_input_tokens,
                  COALESCE(SUM(output_tokens), 0) as total_output_tokens,
                  COALESCE(AVG(duration_ms), 0)::int as avg_duration_ms,
                  MAX(created_at) as last_request_at
           FROM boom_request_log
           GROUP BY COALESCE(model_name, model)
           ORDER BY total_requests DESC"#,
    )
    .fetch_all(pool)
    .await;

    match stats {
        Ok(rows) => Json(json!({"models": rows})).into_response(),
        Err(e) => {
            tracing::error!("Failed to query model stats: {}", e);
            Json(json!({"error": e.to_string()})).into_response()
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

    // Sort by model then deployment_id for stable display.
    let mut result: Vec<_> = rows.into_values().collect();
    result.sort_by(|a, b| {
        let am = a["model"].as_str().unwrap_or("");
        let bm = b["model"].as_str().unwrap_or("");
        am.cmp(bm).then_with(|| {
            a["deployment_id"].as_str().unwrap_or("").cmp(b["deployment_id"].as_str().unwrap_or(""))
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
    let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for entry in entries {
        *counts.entry(entry.key_alias.clone()).or_insert(0) += 1;
    }
    let mut result: Vec<serde_json::Value> = counts.into_iter()
        .map(|(alias, count)| json!({
            "key_alias": alias,
            "request_count": count,
        }))
        .collect();
    result.sort_by(|a, b| b["request_count"].as_u64().cmp(&a["request_count"].as_u64()));
    result
}

// ═══════════════════════════════════════════════════════════
// Rebalance Stats
// ═══════════════════════════════════════════════════════════

pub async fn get_rebalance_stats(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Response {
    let data = state.rebalance_counter.snapshot();
    let points: Vec<serde_json::Value> = data.into_iter()
        .map(|(label, count)| json!({ "minute": label, "count": count }))
        .collect();
    Json(json!({ "rebalance_events": points })).into_response()
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
    tracing::info!(enabled = req.enabled, "Debug error recording toggled");
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
// Prompt Log Controls
// ═══════════════════════════════════════════════════════════

pub async fn get_prompt_log_status(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
) -> Json<Value> {
    let cfg = state.prompt_log_writer.config();
    Json(json!({
        "enabled": cfg.enabled,
        "excluded_keys": cfg.excluded_keys,
        "excluded_teams": cfg.excluded_teams,
    }))
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
    let cfg = state.prompt_log_writer.config();
    let new_cfg = cfg.with_enabled(req.enabled);
    state.prompt_log_writer.update_config(new_cfg);
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
    let cfg = state.prompt_log_writer.config();
    let new_cfg = cfg.with_team_excluded(&req.team_id, req.excluded);
    state.prompt_log_writer.update_config(new_cfg);
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
    let cfg = state.prompt_log_writer.config();
    let new_cfg = cfg.with_key_excluded(&req.key_hash, req.excluded);
    state.prompt_log_writer.update_config(new_cfg);
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
/// Scans JSONL files under {dir}/{team_alias}/{key_hash}/ to find the entry
/// matching the given request_id. Returns the full JSON entry on match.
pub async fn get_prompt_log_entry(
    _session: AdminSession,
    Extension(state): Extension<Arc<DashboardState>>,
    Path(request_id): Path<String>,
    Query(query): Query<PromptLogEntryQuery>,
) -> impl IntoResponse {
    let cfg = state.prompt_log_writer.config();

    // Build the directory path: {dir}/{team_alias}/{key_hash}/
    let team_dir = query.team_alias.as_deref().unwrap_or("_no_team");
    let key_dir = std::path::PathBuf::from(&cfg.dir)
        .join(team_dir)
        .join(&query.key_hash);

    // Scan JSONL files in directory, newest first.
    let mut entries = match tokio::fs::read_dir(&key_dir).await {
        Ok(rd) => rd,
        Err(_) => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error": "Log directory not found"})),
            )
                .into_response();
        }
    };

    let mut files: Vec<String> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("log_") && name.ends_with(".jsonl") {
            files.push(name);
        }
    }
    // Sort descending (newest files first) for faster lookup on recent requests.
    files.sort_by(|a, b| b.cmp(a));

    for fname in files {
        let path = key_dir.join(&fname);
        let Ok(content) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                if val.get("request_id").and_then(|v| v.as_str()) == Some(&request_id) {
                    return Json(val).into_response();
                }
            }
        }
    }

    (
        axum::http::StatusCode::NOT_FOUND,
        Json(json!({"error": "Request not found in prompt logs"})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::normalize_pagination;

    #[test]
    fn normalize_pagination_clamps_invalid_values() {
        assert_eq!(normalize_pagination(0, 0), (1, 1));
        assert_eq!(normalize_pagination(-3, -20), (1, 1));
        assert_eq!(normalize_pagination(2, 5000), (2, 1000));
    }
}
