//! Model detail report: single-model drill-down from the analytics
//! overview's model table. `model` is the *requested* model literal (same
//! column the overview groups by), so the numbers reconcile exactly with
//! the overview row that was clicked.
//!
//! Averages are computed in Rust from `SUM + COUNT` pairs rather than SQL
//! `AVG` — PG's avg(int) returns numeric, which sqlx maps to Decimal not
//! f64 (repo convention, see handlers_admin.rs deployment summary), and
//! error-path rows leave the latency columns NULL so COUNT < requests.
//! GaussDB constraints: CASE-based conditional aggregation only.

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
struct KpiRow {
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    errors: i64,
    sum_duration_ms: i64,
    cnt_duration_ms: i64,
    sum_ttft_ms: i64,
    cnt_ttft_ms: i64,
    sum_queue_wait_ms: i64,
    cnt_queue_wait_ms: i64,
}

#[derive(Debug, Clone, FromRow)]
struct SeriesRow {
    slot_epoch: i64,
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

#[derive(Debug, Clone, FromRow)]
struct HistRow {
    bucket: i64,
    cnt: i64,
}

#[derive(Debug, Clone, FromRow)]
struct DeploymentRow {
    deployment_id: String,
    model_name: String,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    errors: i64,
    sum_duration_ms: i64,
    cnt_duration_ms: i64,
    sum_ttft_ms: i64,
    cnt_ttft_ms: i64,
}

/// Per-bucket series point for the model detail charts.
#[derive(Debug, Clone, Serialize)]
pub struct ModelSeriesPoint {
    pub slot_epoch: i64,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub avg_duration_ms: Option<f64>,
    pub avg_ttft_ms: Option<f64>,
}

/// Resolved-deployment split for the requested model. Empty
/// `deployment_id` / `model_name` means error-path rows that never
/// resolved a deployment (frontend renders "(unattributed)").
#[derive(Debug, Clone, Serialize)]
pub struct DeploymentAgg {
    pub deployment_id: String,
    pub model_name: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub errors: i64,
    pub avg_duration_ms: Option<f64>,
    pub avg_ttft_ms: Option<f64>,
}

/// Immutable model-detail snapshot for one (model, range) window.
#[derive(Debug, Clone, Serialize)]
pub struct ModelDetailSnapshot {
    /// The requested model this report covers (may be `"*"`).
    pub model: String,
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
    pub avg_duration_ms: Option<f64>,
    pub avg_ttft_ms: Option<f64>,
    pub avg_queue_wait_ms: Option<f64>,
    /// cached_tokens / input_tokens when input_tokens > 0 (provider-side
    /// KV-cache prefix hit), else null.
    pub cache_hit_ratio: Option<f64>,
    pub series: Vec<ModelSeriesPoint>,
    /// Fixed 6-bucket latency histogram of duration_ms:
    /// <100ms, <300ms, <1s, <3s, <10s, >=10s.
    pub latency_hist: [i64; 6],
    /// Rows with non-NULL duration_ms (histogram denominator).
    pub timed_requests: i64,
    pub deployments: Vec<DeploymentAgg>,
}

/// Aggregate the model detail report for one (model, range) window.
/// All four queries share `WHERE model = $1 AND created_at >= $2`.
pub async fn query_model_detail(
    model: &str,
    range: StatsRange,
    pool: &PgPool,
) -> Result<ModelDetailSnapshot, sqlx::Error> {
    let (since, bucket, total_slots) = range.window();

    let kpi = sqlx::query_as::<_, KpiRow>(
        // CASE-based conditional aggregation: the aggregate FILTER clause
        // needs PG 9.4+ and GaussDB (PG 9.2-based) rejects it.
        r#"SELECT COUNT(*)::BIGINT AS requests,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                  COALESCE(SUM(cached_tokens), 0)::BIGINT AS cached_tokens,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors,
                  COALESCE(SUM(duration_ms), 0)::BIGINT AS sum_duration_ms,
                  COUNT(duration_ms)::BIGINT AS cnt_duration_ms,
                  COALESCE(SUM(ttft_ms), 0)::BIGINT AS sum_ttft_ms,
                  COUNT(ttft_ms)::BIGINT AS cnt_ttft_ms,
                  COALESCE(SUM(queue_wait_ms), 0)::BIGINT AS sum_queue_wait_ms,
                  COUNT(queue_wait_ms)::BIGINT AS cnt_queue_wait_ms
           FROM boom_request_log
           WHERE model = $1 AND created_at >= $2"#,
    )
    .bind(model)
    .bind(since)
    .fetch_one(pool)
    .await?;

    let series_rows = sqlx::query_as::<_, SeriesRow>(
        r#"SELECT (FLOOR(EXTRACT(EPOCH FROM created_at) / $3) * $3)::BIGINT AS slot_epoch,
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
           WHERE model = $1 AND created_at >= $2
           GROUP BY 1
           ORDER BY 1"#,
    )
    .bind(model)
    .bind(since)
    .bind(bucket)
    .fetch_all(pool)
    .await?;

    let hist_rows = sqlx::query_as::<_, HistRow>(
        r#"SELECT CASE WHEN duration_ms < 100 THEN 0
                      WHEN duration_ms < 300 THEN 1
                      WHEN duration_ms < 1000 THEN 2
                      WHEN duration_ms < 3000 THEN 3
                      WHEN duration_ms < 10000 THEN 4
                      ELSE 5 END::BIGINT AS bucket,
                  COUNT(*)::BIGINT AS cnt
           FROM boom_request_log
           WHERE model = $1 AND created_at >= $2 AND duration_ms IS NOT NULL
           GROUP BY 1
           ORDER BY 1"#,
    )
    .bind(model)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let deployment_rows = sqlx::query_as::<_, DeploymentRow>(
        r#"SELECT COALESCE(deployment_id, '') AS deployment_id,
                  COALESCE(model_name, '') AS model_name,
                  COUNT(*)::BIGINT AS requests,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors,
                  COALESCE(SUM(duration_ms), 0)::BIGINT AS sum_duration_ms,
                  COUNT(duration_ms)::BIGINT AS cnt_duration_ms,
                  COALESCE(SUM(ttft_ms), 0)::BIGINT AS sum_ttft_ms,
                  COUNT(ttft_ms)::BIGINT AS cnt_ttft_ms
           FROM boom_request_log
           WHERE model = $1 AND created_at >= $2
           GROUP BY 1, 2
           ORDER BY requests DESC"#,
    )
    .bind(model)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let mut latency_hist = [0i64; 6];
    let mut timed_requests = 0i64;
    for row in hist_rows {
        if (0..6).contains(&row.bucket) {
            latency_hist[row.bucket as usize] = row.cnt;
        }
        timed_requests += row.cnt;
    }

    Ok(ModelDetailSnapshot {
        model: model.to_string(),
        range: range.as_str().to_string(),
        since_epoch: since.timestamp(),
        bucket_secs: bucket,
        total_slots,
        generated_at: chrono::Utc::now().timestamp(),
        requests: kpi.requests,
        input_tokens: kpi.input_tokens,
        output_tokens: kpi.output_tokens,
        cached_tokens: kpi.cached_tokens,
        errors: kpi.errors,
        avg_duration_ms: avg(kpi.sum_duration_ms, kpi.cnt_duration_ms),
        avg_ttft_ms: avg(kpi.sum_ttft_ms, kpi.cnt_ttft_ms),
        avg_queue_wait_ms: avg(kpi.sum_queue_wait_ms, kpi.cnt_queue_wait_ms),
        cache_hit_ratio: if kpi.input_tokens > 0 {
            Some(kpi.cached_tokens as f64 / kpi.input_tokens as f64)
        } else {
            None
        },
        series: series_rows
            .into_iter()
            .map(|r| ModelSeriesPoint {
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
        latency_hist,
        timed_requests,
        deployments: deployment_rows
            .into_iter()
            .map(|r| DeploymentAgg {
                deployment_id: r.deployment_id,
                model_name: r.model_name,
                requests: r.requests,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                errors: r.errors,
                avg_duration_ms: avg(r.sum_duration_ms, r.cnt_duration_ms),
                avg_ttft_ms: avg(r.sum_ttft_ms, r.cnt_ttft_ms),
            })
            .collect(),
    })
}
