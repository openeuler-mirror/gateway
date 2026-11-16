//! Per-instance usage breakdown for the analytics board: which gateway
//! instance recorded what share of the window's traffic. Answers "how is
//! the load split across my instances, and is one of them degrading" —
//! distinct request volume, token volume, error rate, and latency per
//! stable instance id.
//!
//! Same conventions as `schedule.rs`: GaussDB-safe CASE aggregation,
//! averages computed in Rust from `SUM + COUNT` pairs, window totals
//! derived by summing the grouped rows. NULL `instance_id` = rows written
//! by older binaries before the column existed → the "unknown" bucket.

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

fn ratio(num: i64, den: i64) -> Option<f64> {
    if den > 0 {
        Some(num as f64 / den as f64)
    } else {
        None
    }
}

#[derive(Debug, Clone, FromRow)]
struct InstanceAggRow {
    instance_id: String,
    requests: i64,
    errors: i64,
    sum_input: i64,
    sum_output: i64,
    sum_duration_ms: i64,
    cnt_duration_ms: i64,
    last_active_epoch: i64,
}

/// One instance's window aggregates for the comparison table.
#[derive(Debug, Clone, Serialize)]
pub struct InstanceAgg {
    pub instance_id: String,
    pub requests: i64,
    /// Share of window requests, 0..1 (share of a null total is null).
    pub share: Option<f64>,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub avg_duration_ms: Option<f64>,
    /// Epoch secs of the newest row this instance recorded in-window.
    pub last_active_epoch: i64,
}

/// Immutable per-instance snapshot for one range window.
#[derive(Debug, Clone, Serialize)]
pub struct InstanceSnapshot {
    /// Window selector: "today" | "1d" | "3d" | "7d".
    pub range: String,
    pub since_epoch: i64,
    pub generated_at: i64,
    /// Window totals (sum of the per-instance rows — every in-window row
    /// lands in exactly one bucket, including "unknown").
    pub requests: i64,
    pub errors: i64,
    pub instances: Vec<InstanceAgg>,
}

/// Aggregate the per-instance breakdown for one range window.
pub async fn query_instances(
    range: StatsRange,
    pool: &PgPool,
) -> Result<InstanceSnapshot, sqlx::Error> {
    let (since, _, _) = range.window();

    let rows = sqlx::query_as::<_, InstanceAggRow>(
        r#"SELECT COALESCE(instance_id, 'unknown') AS instance_id,
                  COUNT(*)::BIGINT AS requests,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS sum_input,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS sum_output,
                  COALESCE(SUM(duration_ms), 0)::BIGINT AS sum_duration_ms,
                  COUNT(duration_ms)::BIGINT AS cnt_duration_ms,
                  COALESCE(EXTRACT(EPOCH FROM MAX(created_at)), 0)::BIGINT AS last_active_epoch
           FROM boom_request_log
           WHERE created_at >= $1
           GROUP BY 1
           ORDER BY requests DESC"#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    let total_requests: i64 = rows.iter().map(|r| r.requests).sum();
    let total_errors: i64 = rows.iter().map(|r| r.errors).sum();

    Ok(InstanceSnapshot {
        range: range.as_str().to_string(),
        since_epoch: since.timestamp(),
        generated_at: chrono::Utc::now().timestamp(),
        requests: total_requests,
        errors: total_errors,
        instances: rows
            .into_iter()
            .map(|r| InstanceAgg {
                instance_id: r.instance_id,
                share: ratio(r.requests, total_requests),
                requests: r.requests,
                errors: r.errors,
                input_tokens: r.sum_input,
                output_tokens: r.sum_output,
                avg_duration_ms: avg(r.sum_duration_ms, r.cnt_duration_ms),
                last_active_epoch: r.last_active_epoch,
            })
            .collect(),
    })
}
