//! Consumption report: where did the tokens go — by key, team, end-user
//! tag (`user_tag1`) and client IP, plus a per-key time-series drill-down.
//!
//! Same conventions as `model.rs`: CASE-based conditional aggregation
//! (GaussDB), averages computed in Rust from `SUM + COUNT` pairs, ratios
//! computed in Rust from numerators/denominators. The report totals are
//! derived by summing the (unlimited) bucket series rather than a separate
//! KPI query — every row in the window lands in exactly one bucket, so the
//! sums are exact.
//!
//! The per-key drill-down is a direct query with no cache: key cardinality
//! is unbounded, so caching would only add prune pressure, and the query
//! rides the `idx_request_log_key_created (key_hash, created_at DESC)`
//! index.

use serde::Serialize;
use sqlx::{FromRow, PgPool};

use crate::daily_stats::StatsRange;

fn avg(sum: i64, cnt: i64) -> Option<f64> {
    if cnt > 0 {
        Some(sum as f64 / cnt as f64)
    } else {
        None
    }
}

#[derive(Debug, Clone, FromRow)]
struct KeyRow {
    key_hash: String,
    key_alias: Option<String>,
    key_name: Option<String>,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    errors: i64,
    first_seen_epoch: Option<i64>,
    last_seen_epoch: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
struct TeamRow {
    // GaussDB Oracle-compat: '' ≡ NULL, so COALESCE(team_id, '') can still return NULL
    team_id: Option<String>,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    errors: i64,
    active_keys: i64,
}

#[derive(Debug, Clone, FromRow)]
struct TagRow {
    user_tag1: String,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    errors: i64,
}

#[derive(Debug, Clone, FromRow)]
struct IpRow {
    client_ip: Option<String>,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    errors: i64,
}

#[derive(Debug, Clone, FromRow)]
struct SeriesRow {
    slot_epoch: i64,
    requests: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
}

#[derive(Debug, Clone, FromRow)]
struct KeySeriesRow {
    slot_epoch: i64,
    key_alias: Option<String>,
    key_name: Option<String>,
    requests: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    sum_duration_ms: i64,
    cnt_duration_ms: i64,
    sum_ttft_ms: i64,
    cnt_ttft_ms: i64,
}

/// One key's consumption in the window, ordered by total tokens DESC.
#[derive(Debug, Clone, Serialize)]
pub struct KeyAgg {
    pub key_hash: String,
    pub key_alias: Option<String>,
    pub key_name: Option<String>,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub errors: i64,
    /// cached_tokens / input_tokens when input_tokens > 0, else null.
    pub cache_hit_ratio: Option<f64>,
    pub first_seen_epoch: Option<i64>,
    pub last_seen_epoch: Option<i64>,
}

/// One team's consumption; empty `team_id` means unassigned keys
/// (frontend renders "(none)").
#[derive(Debug, Clone, Serialize)]
pub struct TeamAgg {
    pub team_id: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub errors: i64,
    pub active_keys: i64,
}

/// End-user attribution via `user_tag1`.
#[derive(Debug, Clone, Serialize)]
pub struct TagAgg {
    pub user_tag1: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub errors: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IpAgg {
    pub client_ip: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub errors: i64,
}

/// Per-bucket series point for the token volume chart.
#[derive(Debug, Clone, Serialize)]
pub struct ConsumptionSeriesPoint {
    pub slot_epoch: i64,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
}

/// Immutable consumption snapshot for one range window.
#[derive(Debug, Clone, Serialize)]
pub struct ConsumptionSnapshot {
    /// Window selector: "today" | "1d" | "3d" | "7d".
    pub range: String,
    pub since_epoch: i64,
    pub bucket_secs: i64,
    pub total_slots: i64,
    pub generated_at: i64,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub errors: i64,
    pub keys: Vec<KeyAgg>,
    pub teams: Vec<TeamAgg>,
    pub user_tags: Vec<TagAgg>,
    pub client_ips: Vec<IpAgg>,
    pub series: Vec<ConsumptionSeriesPoint>,
}

/// Per-bucket point for the single-key drill-down.
#[derive(Debug, Clone, Serialize)]
pub struct KeySeriesPoint {
    pub slot_epoch: i64,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub avg_duration_ms: Option<f64>,
    pub avg_ttft_ms: Option<f64>,
}

/// Single-key drill-down report (direct query, not cached).
#[derive(Debug, Clone, Serialize)]
pub struct KeyConsumptionDetail {
    pub key_hash: String,
    pub key_alias: Option<String>,
    pub key_name: Option<String>,
    pub range: String,
    pub since_epoch: i64,
    pub bucket_secs: i64,
    pub total_slots: i64,
    pub generated_at: i64,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub errors: i64,
    pub avg_duration_ms: Option<f64>,
    pub avg_ttft_ms: Option<f64>,
    pub series: Vec<KeySeriesPoint>,
}

/// Aggregate the consumption report for one range window.
/// All queries share `WHERE created_at >= $1`.
pub async fn query_consumption(
    range: StatsRange,
    pool: &PgPool,
) -> Result<ConsumptionSnapshot, sqlx::Error> {
    let (since, bucket, total_slots) = range.window();

    let key_rows = sqlx::query_as::<_, KeyRow>(
        r#"SELECT key_hash,
                  MAX(key_alias) AS key_alias,
                  MAX(key_name) AS key_name,
                  COUNT(*)::BIGINT AS requests,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                  COALESCE(SUM(cached_tokens), 0)::BIGINT AS cached_tokens,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors,
                  EXTRACT(EPOCH FROM MIN(created_at))::BIGINT AS first_seen_epoch,
                  EXTRACT(EPOCH FROM MAX(created_at))::BIGINT AS last_seen_epoch
           FROM boom_request_log
           WHERE created_at >= $1
           GROUP BY key_hash
           ORDER BY COALESCE(SUM(input_tokens), 0) + COALESCE(SUM(output_tokens), 0) DESC
           LIMIT 50"#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    let team_rows = sqlx::query_as::<_, TeamRow>(
        r#"SELECT COALESCE(team_id, '') AS team_id,
                  COUNT(*)::BIGINT AS requests,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors,
                  COUNT(DISTINCT key_hash)::BIGINT AS active_keys
           FROM boom_request_log
           WHERE created_at >= $1
           GROUP BY COALESCE(team_id, '')
           ORDER BY COALESCE(SUM(input_tokens), 0) + COALESCE(SUM(output_tokens), 0) DESC
           LIMIT 200"#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    let tag_rows = sqlx::query_as::<_, TagRow>(
        r#"SELECT user_tag1,
                  COUNT(*)::BIGINT AS requests,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors
           FROM boom_request_log
           WHERE created_at >= $1 AND user_tag1 IS NOT NULL
           GROUP BY user_tag1
           ORDER BY COALESCE(SUM(input_tokens), 0) + COALESCE(SUM(output_tokens), 0) DESC
           LIMIT 20"#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    let ip_rows = sqlx::query_as::<_, IpRow>(
        r#"SELECT COALESCE(client_ip, '') AS client_ip,
                  COUNT(*)::BIGINT AS requests,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors
           FROM boom_request_log
           WHERE created_at >= $1
           GROUP BY COALESCE(client_ip, '')
           ORDER BY COALESCE(SUM(input_tokens), 0) + COALESCE(SUM(output_tokens), 0) DESC
           LIMIT 20"#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    let series_rows = sqlx::query_as::<_, SeriesRow>(
        r#"SELECT (FLOOR(EXTRACT(EPOCH FROM created_at) / $2) * $2)::BIGINT AS slot_epoch,
                  COUNT(*)::BIGINT AS requests,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                  COALESCE(SUM(cached_tokens), 0)::BIGINT AS cached_tokens
           FROM boom_request_log
           WHERE created_at >= $1
           GROUP BY 1
           ORDER BY 1"#,
    )
    .bind(since)
    .bind(bucket)
    .fetch_all(pool)
    .await?;

    // Totals derived from the unlimited series: every in-window row lands
    // in exactly one bucket, so these sums equal a window-wide KPI query.
    let mut requests = 0i64;
    let mut errors = 0i64;
    let mut input_tokens = 0i64;
    let mut output_tokens = 0i64;
    let mut cached_tokens = 0i64;
    for r in &series_rows {
        requests += r.requests;
        errors += r.errors;
        input_tokens += r.input_tokens;
        output_tokens += r.output_tokens;
        cached_tokens += r.cached_tokens;
    }

    Ok(ConsumptionSnapshot {
        range: range.as_str().to_string(),
        since_epoch: since.timestamp(),
        bucket_secs: bucket,
        total_slots,
        generated_at: chrono::Utc::now().timestamp(),
        requests,
        input_tokens,
        output_tokens,
        cached_tokens,
        errors,
        keys: key_rows
            .into_iter()
            .map(|r| KeyAgg {
                cache_hit_ratio: if r.input_tokens > 0 {
                    Some(r.cached_tokens as f64 / r.input_tokens as f64)
                } else {
                    None
                },
                key_hash: r.key_hash,
                key_alias: r.key_alias,
                key_name: r.key_name,
                requests: r.requests,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                cached_tokens: r.cached_tokens,
                errors: r.errors,
                first_seen_epoch: r.first_seen_epoch,
                last_seen_epoch: r.last_seen_epoch,
            })
            .collect(),
        teams: team_rows
            .into_iter()
            .map(|r| TeamAgg {
                team_id: r.team_id.unwrap_or_default(),
                requests: r.requests,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                errors: r.errors,
                active_keys: r.active_keys,
            })
            .collect(),
        user_tags: tag_rows
            .into_iter()
            .map(|r| TagAgg {
                user_tag1: r.user_tag1,
                requests: r.requests,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                errors: r.errors,
            })
            .collect(),
        client_ips: ip_rows
            .into_iter()
            .map(|r| IpAgg {
                client_ip: r.client_ip.unwrap_or_default(),
                requests: r.requests,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                errors: r.errors,
            })
            .collect(),
        series: series_rows
            .into_iter()
            .map(|r| ConsumptionSeriesPoint {
                slot_epoch: r.slot_epoch,
                requests: r.requests,
                errors: r.errors,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                cached_tokens: r.cached_tokens,
            })
            .collect(),
    })
}

/// Single-key drill-down over one range window. Direct query (see module
/// docs for why it is not cached). `series` is bucketized on the same grid
/// as the other analytics pages; totals are derived from it.
pub async fn query_key_consumption_detail(
    key_hash: &str,
    range: StatsRange,
    pool: &PgPool,
) -> Result<KeyConsumptionDetail, sqlx::Error> {
    let (since, bucket, total_slots) = range.window();

    let rows = sqlx::query_as::<_, KeySeriesRow>(
        r#"SELECT (FLOOR(EXTRACT(EPOCH FROM created_at) / $3) * $3)::BIGINT AS slot_epoch,
                  MAX(key_alias) AS key_alias,
                  MAX(key_name) AS key_name,
                  COUNT(*)::BIGINT AS requests,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                  COALESCE(SUM(cached_tokens), 0)::BIGINT AS cached_tokens,
                  COALESCE(SUM(duration_ms), 0)::BIGINT AS sum_duration_ms,
                  COUNT(duration_ms)::BIGINT AS cnt_duration_ms,
                  COALESCE(SUM(ttft_ms), 0)::BIGINT AS sum_ttft_ms,
                  COUNT(ttft_ms)::BIGINT AS cnt_ttft_ms
           FROM boom_request_log
           WHERE key_hash = $1 AND created_at >= $2
           GROUP BY 1
           ORDER BY 1"#,
    )
    .bind(key_hash)
    .bind(since)
    .bind(bucket)
    .fetch_all(pool)
    .await?;

    let mut requests = 0i64;
    let mut errors = 0i64;
    let mut input_tokens = 0i64;
    let mut output_tokens = 0i64;
    let mut cached_tokens = 0i64;
    let mut sum_duration_ms = 0i64;
    let mut cnt_duration_ms = 0i64;
    let mut sum_ttft_ms = 0i64;
    let mut cnt_ttft_ms = 0i64;
    let mut key_alias = None;
    let mut key_name = None;
    for r in &rows {
        requests += r.requests;
        errors += r.errors;
        input_tokens += r.input_tokens;
        output_tokens += r.output_tokens;
        cached_tokens += r.cached_tokens;
        sum_duration_ms += r.sum_duration_ms;
        cnt_duration_ms += r.cnt_duration_ms;
        sum_ttft_ms += r.sum_ttft_ms;
        cnt_ttft_ms += r.cnt_ttft_ms;
        if key_alias.is_none() {
            key_alias = r.key_alias.clone();
        }
        if key_name.is_none() {
            key_name = r.key_name.clone();
        }
    }

    Ok(KeyConsumptionDetail {
        key_hash: key_hash.to_string(),
        key_alias,
        key_name,
        range: range.as_str().to_string(),
        since_epoch: since.timestamp(),
        bucket_secs: bucket,
        total_slots,
        generated_at: chrono::Utc::now().timestamp(),
        requests,
        input_tokens,
        output_tokens,
        cached_tokens,
        errors,
        avg_duration_ms: avg(sum_duration_ms, cnt_duration_ms),
        avg_ttft_ms: avg(sum_ttft_ms, cnt_ttft_ms),
        series: rows
            .into_iter()
            .map(|r| KeySeriesPoint {
                slot_epoch: r.slot_epoch,
                requests: r.requests,
                errors: r.errors,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                cached_tokens: r.cached_tokens,
                avg_duration_ms: avg(r.sum_duration_ms, r.cnt_duration_ms),
                avg_ttft_ms: avg(r.sum_ttft_ms, r.cnt_ttft_ms),
            })
            .collect(),
    })
}
