//! Schedule & cache-history report: how requests were routed (schedule
//! policy distribution over time) and what the caches did (KV prefix hit
//! ratio, trie utilization, provider cache hit ratio), plus a per-policy
//! comparison table — answers "is key-affinity routing actually improving
//! cache hit and TTFT".
//!
//! Same conventions as `model.rs` / `consumption.rs`: GaussDB-safe CASE
//! aggregation, averages computed in Rust from `SUM + COUNT` pairs, ratios
//! computed in Rust from numerator/denominator sums (denominator 0 → null →
//! frontend renders "—"). The window totals are derived by summing the
//! policy×bucket rows (every in-window row lands in exactly one bucket and
//! one policy).
//!
//! Column semantics (written by boom-main's KvcOrchestrator):
//! - `kv_hit_blocks` / `kv_input_blocks` — per-request prefix blocks hit /
//!   total input blocks (NULL when kvc didn't produce them, e.g. policies
//!   without prefix lookup).
//! - `trie_blocks` = whole-gateway trie block count at request time,
//!   `trie_max_blocks` = its capacity — both gauges. SUM/SUM equals
//!   AVG(trie_blocks)/trie_max when capacity is constant, and stays
//!   meaningful across a mid-window capacity change.
//! - `cached_tokens` / `input_tokens` — provider-side prompt cache.

use serde::Serialize;
use sqlx::{FromRow, PgPool};

use crate::daily_stats::StatsRange;

fn ratio(num: i64, den: i64) -> Option<f64> {
    if den > 0 {
        Some(num as f64 / den as f64)
    } else {
        None
    }
}

fn avg(sum: i64, cnt: i64) -> Option<f64> {
    if cnt > 0 {
        Some(sum as f64 / cnt as f64)
    } else {
        None
    }
}

#[derive(Debug, Clone, FromRow)]
struct PolicyBucketRow {
    slot_epoch: i64,
    policy: String,
    requests: i64,
    errors: i64,
}

#[derive(Debug, Clone, FromRow)]
struct RatioRow {
    slot_epoch: i64,
    sum_kv_hit: i64,
    sum_kv_input: i64,
    sum_trie: i64,
    sum_trie_max: i64,
    sum_cached: i64,
    sum_input: i64,
}

#[derive(Debug, Clone, FromRow)]
struct PolicyAggRow {
    policy: String,
    requests: i64,
    errors: i64,
    sum_kv_hit: i64,
    sum_kv_input: i64,
    sum_trie: i64,
    sum_trie_max: i64,
    sum_cached: i64,
    sum_input: i64,
    sum_duration_ms: i64,
    cnt_duration_ms: i64,
    sum_ttft_ms: i64,
    cnt_ttft_ms: i64,
    sum_queue_ms: i64,
    cnt_queue_ms: i64,
}

/// One policy's requests in one bucket (policy trend chart).
#[derive(Debug, Clone, Serialize)]
pub struct PolicyBucketPoint {
    pub slot_epoch: i64,
    pub policy: String,
    pub requests: i64,
    pub errors: i64,
}

/// Per-bucket cache ratios (numerators/denominators summed per bucket).
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleSeriesPoint {
    pub slot_epoch: i64,
    /// SUM(kv_hit_blocks) / SUM(kv_input_blocks), null when no kvc data.
    pub kv_hit_ratio: Option<f64>,
    /// SUM(trie_blocks) / SUM(trie_max_blocks), null when no trie data.
    pub trie_util_ratio: Option<f64>,
    /// SUM(cached_tokens) / SUM(input_tokens), null when no traffic.
    pub cache_hit_ratio: Option<f64>,
}

/// One policy's window aggregates for the comparison table.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyAgg {
    pub policy: String,
    pub requests: i64,
    pub errors: i64,
    pub avg_duration_ms: Option<f64>,
    pub avg_ttft_ms: Option<f64>,
    pub avg_queue_wait_ms: Option<f64>,
    pub kv_hit_ratio: Option<f64>,
    pub trie_util_ratio: Option<f64>,
    pub cache_hit_ratio: Option<f64>,
}

/// Immutable schedule & cache snapshot for one range window.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleSnapshot {
    /// Window selector: "today" | "1d" | "3d" | "7d".
    pub range: String,
    pub since_epoch: i64,
    pub bucket_secs: i64,
    pub total_slots: i64,
    pub generated_at: i64,
    pub requests: i64,
    pub errors: i64,
    /// Window-wide SUM(kv_hit_blocks) / SUM(kv_input_blocks).
    pub kv_hit_ratio: Option<f64>,
    /// Window-wide SUM(trie_blocks) / SUM(trie_max_blocks).
    pub trie_util_ratio: Option<f64>,
    /// Window-wide SUM(cached_tokens) / SUM(input_tokens).
    pub cache_hit_ratio: Option<f64>,
    pub policies: Vec<PolicyAgg>,
    /// Per-policy bucket series (policy trend chart).
    pub policy_series: Vec<PolicyBucketPoint>,
    /// Per-bucket cache ratio series.
    pub series: Vec<ScheduleSeriesPoint>,
}

/// Aggregate the schedule & cache report for one range window.
/// All queries share `WHERE created_at >= $1`.
pub async fn query_schedule(
    range: StatsRange,
    pool: &PgPool,
) -> Result<ScheduleSnapshot, sqlx::Error> {
    let (since, bucket, total_slots) = range.window();

    let policy_rows = sqlx::query_as::<_, PolicyBucketRow>(
        r#"SELECT (FLOOR(EXTRACT(EPOCH FROM created_at) / $2) * $2)::BIGINT AS slot_epoch,
                  COALESCE(schedule_policy, 'unknown') AS policy,
                  COUNT(*)::BIGINT AS requests,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors
           FROM boom_request_log
           WHERE created_at >= $1
           GROUP BY 1, 2
           ORDER BY 1, 2"#,
    )
    .bind(since)
    .bind(bucket)
    .fetch_all(pool)
    .await?;

    let ratio_rows = sqlx::query_as::<_, RatioRow>(
        r#"SELECT (FLOOR(EXTRACT(EPOCH FROM created_at) / $2) * $2)::BIGINT AS slot_epoch,
                  COALESCE(SUM(kv_hit_blocks), 0)::BIGINT AS sum_kv_hit,
                  COALESCE(SUM(kv_input_blocks), 0)::BIGINT AS sum_kv_input,
                  COALESCE(SUM(trie_blocks), 0)::BIGINT AS sum_trie,
                  COALESCE(SUM(trie_max_blocks), 0)::BIGINT AS sum_trie_max,
                  COALESCE(SUM(cached_tokens), 0)::BIGINT AS sum_cached,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS sum_input
           FROM boom_request_log
           WHERE created_at >= $1
           GROUP BY 1
           ORDER BY 1"#,
    )
    .bind(since)
    .bind(bucket)
    .fetch_all(pool)
    .await?;

    let agg_rows = sqlx::query_as::<_, PolicyAggRow>(
        r#"SELECT COALESCE(schedule_policy, 'unknown') AS policy,
                  COUNT(*)::BIGINT AS requests,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors,
                  COALESCE(SUM(kv_hit_blocks), 0)::BIGINT AS sum_kv_hit,
                  COALESCE(SUM(kv_input_blocks), 0)::BIGINT AS sum_kv_input,
                  COALESCE(SUM(trie_blocks), 0)::BIGINT AS sum_trie,
                  COALESCE(SUM(trie_max_blocks), 0)::BIGINT AS sum_trie_max,
                  COALESCE(SUM(cached_tokens), 0)::BIGINT AS sum_cached,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS sum_input,
                  COALESCE(SUM(duration_ms), 0)::BIGINT AS sum_duration_ms,
                  COUNT(duration_ms)::BIGINT AS cnt_duration_ms,
                  COALESCE(SUM(ttft_ms), 0)::BIGINT AS sum_ttft_ms,
                  COUNT(ttft_ms)::BIGINT AS cnt_ttft_ms,
                  COALESCE(SUM(queue_wait_ms), 0)::BIGINT AS sum_queue_ms,
                  COUNT(queue_wait_ms)::BIGINT AS cnt_queue_ms
           FROM boom_request_log
           WHERE created_at >= $1
           GROUP BY 1
           ORDER BY COUNT(*) DESC"#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    // Totals derived from the policy×bucket rows: every in-window row lands
    // in exactly one bucket and one policy. Ratio totals sum the per-bucket
    // numerator/denominator sums (not averages of ratios).
    let mut requests = 0i64;
    let mut errors = 0i64;
    for r in &policy_rows {
        requests += r.requests;
        errors += r.errors;
    }
    let mut sum_kv_hit = 0i64;
    let mut sum_kv_input = 0i64;
    let mut sum_trie = 0i64;
    let mut sum_trie_max = 0i64;
    let mut sum_cached = 0i64;
    let mut sum_input = 0i64;
    for r in &ratio_rows {
        sum_kv_hit += r.sum_kv_hit;
        sum_kv_input += r.sum_kv_input;
        sum_trie += r.sum_trie;
        sum_trie_max += r.sum_trie_max;
        sum_cached += r.sum_cached;
        sum_input += r.sum_input;
    }

    Ok(ScheduleSnapshot {
        range: range.as_str().to_string(),
        since_epoch: since.timestamp(),
        bucket_secs: bucket,
        total_slots,
        generated_at: chrono::Utc::now().timestamp(),
        requests,
        errors,
        kv_hit_ratio: ratio(sum_kv_hit, sum_kv_input),
        trie_util_ratio: ratio(sum_trie, sum_trie_max),
        cache_hit_ratio: ratio(sum_cached, sum_input),
        policies: agg_rows
            .into_iter()
            .map(|r| PolicyAgg {
                policy: r.policy,
                requests: r.requests,
                errors: r.errors,
                avg_duration_ms: avg(r.sum_duration_ms, r.cnt_duration_ms),
                avg_ttft_ms: avg(r.sum_ttft_ms, r.cnt_ttft_ms),
                avg_queue_wait_ms: avg(r.sum_queue_ms, r.cnt_queue_ms),
                kv_hit_ratio: ratio(r.sum_kv_hit, r.sum_kv_input),
                trie_util_ratio: ratio(r.sum_trie, r.sum_trie_max),
                cache_hit_ratio: ratio(r.sum_cached, r.sum_input),
            })
            .collect(),
        policy_series: policy_rows
            .into_iter()
            .map(|r| PolicyBucketPoint {
                slot_epoch: r.slot_epoch,
                policy: r.policy,
                requests: r.requests,
                errors: r.errors,
            })
            .collect(),
        series: ratio_rows
            .into_iter()
            .map(|r| ScheduleSeriesPoint {
                slot_epoch: r.slot_epoch,
                kv_hit_ratio: ratio(r.sum_kv_hit, r.sum_kv_input),
                trie_util_ratio: ratio(r.sum_trie, r.sum_trie_max),
                cache_hit_ratio: ratio(r.sum_cached, r.sum_input),
            })
            .collect(),
    })
}
