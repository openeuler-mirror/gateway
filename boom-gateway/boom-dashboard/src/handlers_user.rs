use axum::extract::Query;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::DashboardSession;
use crate::state::DashboardState;
use boom_flowcontrol::UserRequestStage;

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
            // Serialize WindowLimit entries as compact arrays for front-end compat.
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
            Json(json!({
                "plan_name": p.name,
                "concurrency_limit": concurrency_limit,
                "rpm_limit": rpm_limit,
                "window_limits": wl_json,
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

    // Counts dimension: from SlidingWindowLimiter (per-window aggregated).
    // We dedupe by window_secs — multiple models under the same key collapse
    // into one counts window because the plan limit is keyed by secs only.
    let mut counts_by_secs: std::collections::HashMap<u64, (u64, u64, u64)> =
        std::collections::HashMap::new(); // secs → (count, elapsed, remaining)
    for w in state.limiter.get_usage_for_key(key_hash) {
        let remaining = w.window_secs.saturating_sub(w.elapsed_secs);
        counts_by_secs
            .entry(w.window_secs)
            .and_modify(|e| e.0 = e.0.saturating_add(w.count))
            .or_insert((w.count, w.elapsed_secs, remaining));
    }

    // Tokens / costs dimension: from QuotaStore. Aggregate count by (secs, kind)
    // — we don't surface per-model breakdown on the user usage card.
    let mut tokens_by_secs: std::collections::HashMap<u64, (u64, u64)> =
        std::collections::HashMap::new(); // secs → (count, remaining)
    let mut costs_by_secs: std::collections::HashMap<u64, (u64, u64)> =
        std::collections::HashMap::new(); // secs → (micros, remaining)
    for w in state.quota_store.peek_key_windows(key_hash) {
        let entry = match w.kind {
            boom_quota::WindowKind::Tokens => tokens_by_secs
                .entry(w.window_secs)
                .or_insert((0, w.remaining_secs)),
            boom_quota::WindowKind::CostMicros => costs_by_secs
                .entry(w.window_secs)
                .or_insert((0, w.remaining_secs)),
        };
        entry.0 = entry.0.saturating_add(w.count);
        // Keep the smallest remaining (i.e. the soonest-resetting window in
        // that secs bucket — defensive in case of clock skew between models).
        if w.remaining_secs < entry.1 {
            entry.1 = w.remaining_secs;
        }
    }

    // Assemble per-window_secs card. The plan's window_limits drives which
    // window_secs values exist; if a counter exists at a secs not in the plan
    // (legacy / stale), include it with no limit so the user still sees it.
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
            // Find plan config for this window_secs (if any).
            let wl = plan_window_limits.iter().find(|w| w.window_secs == secs);
            let counts_limit = wl.and_then(|w| w.counts).or(if secs == 60 { plan_rpm } else { None });
            let tokens_limit = wl.and_then(|w| w.tokens);
            let costs_limit = wl.and_then(|w| w.costs);

            let mut dims = serde_json::Map::new();

            // counts: only include when plan configured a counts limit.
            if let Some(limit) = counts_limit {
                let cur = counts_by_secs.get(&secs).map(|&(c, _, _)| c).unwrap_or(0);
                dims.insert(
                    "counts".to_string(),
                    json!({ "current": cur, "limit": limit }),
                );
            }

            // tokens: only include when plan configured a tokens limit.
            if let Some(limit) = tokens_limit {
                let cur = tokens_by_secs.get(&secs).map(|&(c, _)| c).unwrap_or(0);
                dims.insert(
                    "tokens".to_string(),
                    json!({ "current": cur, "limit": limit }),
                );
            }

            // costs: only include when plan configured a costs limit.
            if let Some(limit) = costs_limit {
                let cur_micros = costs_by_secs.get(&secs).map(|&(c, _)| c).unwrap_or(0);
                dims.insert(
                    "costs".to_string(),
                    json!({
                        "current_micros": cur_micros,
                        "current": boom_quota::micros_to_decimal(cur_micros).to_string(),
                        "limit": limit.to_string(),
                        "limit_micros": boom_quota::decimal_to_micros(limit),
                    }),
                );
            }

            // remaining_secs: pick the smallest non-zero across dims, fall
            // back to the secs itself when unknown.
            let remaining = counts_by_secs
                .get(&secs)
                .map(|(_, _, r)| *r)
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

    // Cumulative counters + plan cumulative limits.
    let key_scope = boom_quota::QuotaScope::Key {
        key_hash: key_hash.to_string(),
    };
    let total_in = state
        .quota_store
        .peek_cumulative(&key_scope, boom_quota::CumulativeKind::TotalInputTokens);
    let total_out = state
        .quota_store
        .peek_cumulative(&key_scope, boom_quota::CumulativeKind::TotalOutputTokens);
    let total_cost_micros = state
        .quota_store
        .peek_cumulative(&key_scope, boom_quota::CumulativeKind::TotalCost);
    let regular_input_cost_micros = state
        .quota_store
        .peek_cumulative(&key_scope, boom_quota::CumulativeKind::TotalRegularInputCost);
    let cached_input_cost_micros = state
        .quota_store
        .peek_cumulative(&key_scope, boom_quota::CumulativeKind::TotalCachedInputCost);
    let output_cost_micros = state
        .quota_store
        .peek_cumulative(&key_scope, boom_quota::CumulativeKind::TotalOutputCost);

    let concurrency = state.plan_store.get_concurrency(key_hash);

    Json(json!({
        "plan_name": plan.as_ref().map(|p| p.name.as_str()),
        "concurrency": concurrency,
        "concurrency_limit": plan_concurrency,
        "windows": windows,
        "cumulative": {
            "total_input_tokens": total_in,
            "total_output_tokens": total_out,
            "total_tokens": total_in.saturating_add(total_out),
            "total_cost_micros": total_cost_micros,
            "total_cost": boom_quota::micros_to_decimal(total_cost_micros).to_string(),
            "regular_input_cost_micros": regular_input_cost_micros,
            "regular_input_cost": boom_quota::micros_to_decimal(regular_input_cost_micros).to_string(),
            "cached_input_cost_micros": cached_input_cost_micros,
            "cached_input_cost": boom_quota::micros_to_decimal(cached_input_cost_micros).to_string(),
            "output_cost_micros": output_cost_micros,
            "output_cost": boom_quota::micros_to_decimal(output_cost_micros).to_string(),
            "total_token_limit": plan.as_ref().and_then(|p| p.total_token_limit),
            "total_cost_limit": plan.as_ref().and_then(|p| p.total_cost_limit).map(|d| d.to_string()),
        },
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
    client_ip: Option<String>,
    cached_tokens: Option<i64>,
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
                  error_type, error_message, created_at, client_ip,
                  cached_tokens
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
                "client_ip": r.client_ip,
                "cached_tokens": r.cached_tokens,
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

// ── GET /dashboard/api/user/request-status ─────────────────

pub async fn get_request_status(
    session: DashboardSession,
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
) -> Json<Value> {
    let key_hash = &session.claims.key_hash;
    let statuses = state.flow_controller.get_key_request_status(key_hash);

    let requests: Vec<Value> = statuses
        .into_iter()
        .map(|s| {
            let mut obj = json!({
                "model": s.model,
                "status": match &s.status {
                    UserRequestStage::Waiting { .. } => "waiting",
                    UserRequestStage::Processing { .. } => "processing",
                },
                "wait_time_secs": (s.wait_time_secs * 10.0).round() / 10.0,
                "is_vip": s.is_vip,
            });
            let map = obj.as_object_mut().unwrap();
            match &s.status {
                UserRequestStage::Waiting { ahead } => {
                    map.insert("ahead".to_string(), json!(*ahead));
                }
                UserRequestStage::Processing { processing_secs, parallel_count } => {
                    map.insert("processing_secs".to_string(), json!((*processing_secs * 10.0).round() / 10.0));
                    map.insert("parallel_count".to_string(), json!(*parallel_count));
                }
            }
            obj
        })
        .collect();

    Json(json!({"requests": requests}))
}

