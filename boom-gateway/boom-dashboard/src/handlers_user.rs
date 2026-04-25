use axum::extract::Query;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::DashboardSession;
use crate::state::DashboardState;

// ── GET /dashboard/api/user/plan ───────────────────────────

pub async fn get_plan(
    session: DashboardSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Json<Value> {
    let key_hash = &session.claims.key_hash;

    // Resolve plan: explicit assignment → default plan → null.
    let plan = state
        .plan_store
        .resolve_plan(key_hash)
        .or_else(|| state.plan_store.get_default_plan());

    match plan {
        Some(p) => {
            let (concurrency_limit, rpm_limit, window_limits, _) = p.effective_limits();
            Json(json!({
                "plan_name": p.name,
                "concurrency_limit": concurrency_limit,
                "rpm_limit": rpm_limit,
                "window_limits": window_limits,
                "schedule": p.schedule,
            }))
        }
        None => Json(json!({
            "plan_name": null,
            "concurrency_limit": null,
            "rpm_limit": null,
            "window_limits": [],
            "schedule": [],
        })),
    }
}

// ── GET /dashboard/api/user/usage ──────────────────────────

pub async fn get_usage(
    session: DashboardSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Json<Value> {
    let key_hash = &session.claims.key_hash;

    // Resolve plan limits for this key.
    let plan = state
        .plan_store
        .resolve_plan(key_hash)
        .or_else(|| state.plan_store.get_default_plan());
    let (plan_concurrency, plan_rpm, plan_window_limits, _) = plan
        .as_ref()
        .map(|p| p.effective_limits())
        .unwrap_or((None, None, vec![], vec![]));

    let windows: Vec<Value> = state
        .limiter
        .get_usage_for_key(key_hash)
        .into_iter()
        .map(|w| {
            // cache_key format: {key_hash}:{model}:{window_secs}
            // Find the limit for this window from plan.
            let limit = if w.window_secs == 60 {
                plan_rpm
            } else {
                plan_window_limits
                    .iter()
                    .find(|(_, ws)| *ws == w.window_secs)
                    .map(|(limit, _)| *limit)
            };
            json!({
                "cache_key": w.cache_key,
                "count": w.count,
                "limit": limit,
                "window_secs": w.window_secs,
                "elapsed_secs": w.elapsed_secs,
            })
        })
        .collect();

    let concurrency = state.plan_store.get_concurrency(key_hash);

    Json(json!({
        "plan_name": plan.as_ref().map(|p| p.name.as_str()),
        "concurrency": concurrency,
        "concurrency_limit": plan_concurrency,
        "windows": windows,
    }))
}

// ── GET /dashboard/api/user/key-info ───────────────────────

pub async fn get_key_info(
    session: DashboardSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Json<Value> {
    let key_hash = &session.claims.key_hash;

    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})),
    };

    let row: Option<(
        Option<String>,
        Option<String>,
        f64,
        Option<chrono::NaiveDateTime>,
        Option<bool>,
        Option<i64>,
        Option<i64>,
        Option<f64>,
        Option<String>,
        Option<serde_json::Value>,
        Option<chrono::NaiveDateTime>,
    )> = sqlx::query_as(
        r#"SELECT key_name, key_alias, spend, expires, blocked,
                  rpm_limit, tpm_limit, max_budget, budget_duration,
                  metadata, created_at
           FROM "boom_verification_token" WHERE token = $1"#,
    )
    .bind(key_hash)
    .fetch_optional(db_pool)
    .await
    .unwrap_or(None);

    match row {
        Some((key_name, key_alias, spend, expires, blocked, rpm_limit, tpm_limit, max_budget, budget_duration, metadata, created_at)) => {
            // Query token usage from LiteLLM_SpendLogs (may not exist in all deployments).
            let (input_tokens, output_tokens) = query_token_usage(db_pool, key_hash).await;

            Json(json!({
                "key_name": key_name,
                "key_alias": key_alias,
                "spend": spend,
                "expires": expires.map(|d| d.to_string()),
                "blocked": blocked.unwrap_or(false),
                "rpm_limit": rpm_limit,
                "tpm_limit": tpm_limit,
                "max_budget": max_budget,
                "budget_duration": budget_duration,
                "metadata": metadata,
                "created_at": created_at.map(|d| d.to_string()),
                "token_prefix": format!("{}...", &key_hash[..8.min(key_hash.len())]),
                "total_input_tokens": input_tokens,
                "total_output_tokens": output_tokens,
            }))
        }
        None => Json(json!({"error": "Key not found"})),
    }
}

/// Query aggregated token usage from our own boom_request_log table.
/// Returns (input, output) token counts.
async fn query_token_usage(
    pool: &sqlx::PgPool,
    key_hash: &str,
) -> (Option<i64>, Option<i64>) {
    let row: (i64, i64) = sqlx::query_as(
        r#"SELECT COALESCE(SUM(input_tokens), 0)::BIGINT,
                  COALESCE(SUM(output_tokens), 0)::BIGINT
           FROM boom_request_log WHERE key_hash = $1"#,
    )
    .bind(key_hash)
    .fetch_one(pool)
    .await
    .unwrap_or((0, 0));

    (Some(row.0), Some(row.1))
}

// ── GET /dashboard/api/user/logs ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct UserLogsQuery {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_per_page")]
    pub per_page: i64,
}

fn default_page() -> i64 { 1 }
fn default_per_page() -> i64 { 50 }

#[derive(Debug, sqlx::FromRow)]
struct UserLogRow {
    model: String,
    api_path: String,
    is_stream: bool,
    status_code: i16,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    duration_ms: Option<i32>,
    error_type: Option<String>,
    error_message: Option<String>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn get_user_logs(
    session: DashboardSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    Query(query): Query<UserLogsQuery>,
) -> Response {
    let key_hash = &session.claims.key_hash;
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => return Json(json!({"error": "Database not available"})).into_response(),
    };

    let offset = (query.page - 1).max(0) * query.per_page;

    let rows: Vec<UserLogRow> = match sqlx::query_as(
        r#"SELECT model, api_path, is_stream, status_code,
                  input_tokens, output_tokens, duration_ms,
                  error_type, error_message, created_at
           FROM boom_request_log
           WHERE key_hash = $1
           ORDER BY created_at DESC
           LIMIT $2 OFFSET $3"#,
    )
    .bind(key_hash)
    .bind(query.per_page)
    .bind(offset)
    .fetch_all(db_pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("User get_user_logs query failed: {}", e);
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let total: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM boom_request_log WHERE key_hash = $1"#,
    )
    .bind(key_hash)
    .fetch_one(db_pool)
    .await
    .unwrap_or(0);

    let logs: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "model": r.model,
                "api_path": r.api_path,
                "is_stream": r.is_stream,
                "status_code": r.status_code,
                "input_tokens": r.input_tokens,
                "output_tokens": r.output_tokens,
                "duration_ms": r.duration_ms,
                "error_type": r.error_type,
                "error_message": r.error_message,
                "created_at": r.created_at.map(|d| d.to_rfc3339()),
            })
        })
        .collect();

    Json(json!({
        "logs": logs,
        "page": query.page,
        "per_page": query.per_page,
        "total": total,
    }))
    .into_response()
}
