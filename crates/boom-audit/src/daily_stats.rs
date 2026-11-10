//! Range business-volume snapshot ("数据看板"): aggregates over
//! `boom_request_log`, kept as in-memory caches refreshed from the DB.
//!
//! Four windows: today (CST midnight, 10-min buckets) and rolling
//! 24h/72h/168h windows at 10-min/30-min/1h buckets — every window keeps
//! ~144 points so chart resolution stays uniform. Lifecycle:
//! `DailyStatsTracker` is owned by AppState (top-level Arc, survives
//! reloads). All windows are lazy — there is no background sync task; the
//! dashboard handler loads a window on first request and re-aggregates at
//! most once per TTL (5 min) while the page is being viewed. Nobody
//! viewing → no requests → zero DB sync. Every refresh re-aggregates the
//! full window — idempotent and correct under multi-instance deployment
//! (rows written by peer instances are picked up on the next cycle). No
//! watermark/incremental reads: the audit log writer batches INSERTs from
//! multiple instances, so id-based watermarks have commit-order gaps and
//! would silently undercount.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};

/// Dashboard timezone for "today 00:00" boundaries. Fixed UTC+8 (same
/// convention as the limiter's Beijing-time day key) — no tz-database
/// dependency, day boundaries stay stable across DST-less operation.
fn cst() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("valid offset")
}

/// Start of the current CST day, expressed in UTC (DB comparison value).
pub fn cst_today_start_utc() -> DateTime<Utc> {
    let now_cst = Utc::now().with_timezone(&cst());
    let midnight_cst = cst()
        .from_local_datetime(&now_cst.date_naive().and_hms_opt(0, 0, 0).expect("valid midnight"))
        .single()
        .expect("midnight is unambiguous");
    midnight_cst.with_timezone(&Utc)
}

/// Current CST date as "YYYY-MM-DD" (snapshot `day` label).
fn cst_today_label() -> String {
    Utc::now().with_timezone(&cst()).date_naive().to_string()
}

#[derive(Debug, Clone, FromRow)]
struct ModelAggRow {
    model: String,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    errors: i64,
}

#[derive(Debug, Clone, FromRow)]
struct ActiveRow {
    active_keys: i64,
    active_teams: i64,
}

#[derive(Debug, Clone, FromRow)]
struct SeriesRow {
    slot_epoch: i64,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
}

/// Query window selector. Bucket sizes are chosen so every window renders
/// ~144 points (uniform chart resolution): today/24h at 10 min, 72h at
/// 30 min, 168h at 1 h.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsRange {
    Today,
    Last1d,
    Last3d,
    Last7d,
}

impl StatsRange {
    pub const ALL: [StatsRange; 4] = [Self::Today, Self::Last1d, Self::Last3d, Self::Last7d];

    /// Accepts `""`/`"today"`/`"1d"`/`"3d"`/`"7d"` (None = bad parameter).
    pub fn from_param(s: &str) -> Option<Self> {
        match s {
            "" | "today" => Some(Self::Today),
            "1d" => Some(Self::Last1d),
            "3d" => Some(Self::Last3d),
            "7d" => Some(Self::Last7d),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Last1d => "1d",
            Self::Last3d => "3d",
            Self::Last7d => "7d",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Today => 0,
            Self::Last1d => 1,
            Self::Last3d => 2,
            Self::Last7d => 3,
        }
    }

    /// (window start UTC, bucket seconds, axis slot count for the full
    /// window). Rolling-window starts are floored to the bucket grid so
    /// `slot_epoch - since` is always a whole number of buckets.
    fn window(self) -> (DateTime<Utc>, i64, i64) {
        let now = Utc::now().timestamp();
        match self {
            Self::Today => (cst_today_start_utc(), 600, 144),
            Self::Last1d => (floor_to_bucket(now - 86400, 600), 600, 144),
            Self::Last3d => (floor_to_bucket(now - 259200, 1800), 1800, 144),
            Self::Last7d => (floor_to_bucket(now - 604800, 3600), 3600, 168),
        }
    }
}

/// Largest bucket-grid instant not after `epoch`.
fn floor_to_bucket(epoch: i64, bucket: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(epoch - epoch.rem_euclid(bucket), 0)
        .single()
        .expect("valid epoch")
}

/// Per-model totals for the window.
#[derive(Debug, Clone, Serialize)]
pub struct ModelDailyAgg {
    pub model: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    /// Requests with status >= 400 (includes 429 / auth failures — they are
    /// logged rows too and reflect real traffic pressure).
    pub errors: i64,
}

/// Time buckets (CST-aligned: UTC+8 is a whole 10-minute multiple, so
/// bucket-floored epochs line up with CST wall-clock marks).
#[derive(Debug, Clone, Serialize)]
pub struct SeriesPoint {
    pub slot_epoch: i64,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Immutable aggregate snapshot for one query window.
#[derive(Debug, Clone, Serialize)]
pub struct DailySnapshot {
    /// Window selector: "today" | "1d" | "3d" | "7d".
    pub range: String,
    /// CST day this snapshot covers, "YYYY-MM-DD"; empty for rolling
    /// windows (their subtitle is range-based, not day-based).
    pub day: String,
    /// Window start, UTC epoch seconds (bucket-aligned).
    pub since_epoch: i64,
    /// Bucket size in seconds (600 | 1800 | 3600).
    pub bucket_secs: i64,
    /// Axis slots in the full window. The today chart draws only the
    /// elapsed subset; rolling windows draw all of them.
    pub total_slots: i64,
    /// When this snapshot was generated (epoch seconds).
    pub generated_at: i64,
    pub total_requests: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cached_tokens: i64,
    pub total_errors: i64,
    pub active_keys: i64,
    pub active_teams: i64,
    /// Per-model rows, sorted by requests DESC.
    pub models: Vec<ModelDailyAgg>,
    /// Time buckets, sorted by slot ASC.
    pub series: Vec<SeriesPoint>,
}

struct RangeSlot {
    snapshot: ArcSwapOption<DailySnapshot>,
    refresh_lock: tokio::sync::Mutex<()>,
}

/// In-memory per-range snapshot cache. Lock-free reads (ArcSwap); refreshes
/// for one range are serialized through its own tokio Mutex so a manual
/// force-refresh, dashboard polling and the periodic sync never run
/// duplicate queries concurrently.
pub struct DailyStatsTracker {
    slots: Vec<RangeSlot>,
}

impl Default for DailyStatsTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DailyStatsTracker {
    pub fn new() -> Self {
        Self {
            slots: StatsRange::ALL
                .iter()
                .map(|_| RangeSlot {
                    snapshot: ArcSwapOption::new(None),
                    refresh_lock: tokio::sync::Mutex::new(()),
                })
                .collect(),
        }
    }

    /// Latest snapshot for the range (None before the first successful
    /// refresh, e.g. right after boot or in no-DB mode).
    pub fn current(&self, range: StatsRange) -> Option<Arc<DailySnapshot>> {
        self.slots[range.index()].snapshot.load_full()
    }

    /// Snapshot for the range, re-aggregating when stale. Concurrent calls
    /// coalesce on the per-range lock: late callers wait, then re-read the
    /// winner's snapshot (their own query is skipped — same window). A
    /// cache younger than `max_age_secs` is returned as-is; `force`
    /// bypasses the age check (manual refresh button). All windows
    /// lazy-load here and re-aggregate at most once per `max_age_secs` —
    /// with no background sync task, the DB is only touched when a
    /// dashboard request actually arrives. `max_age_secs = 0` always
    /// re-aggregates.
    pub async fn snapshot_for(
        &self,
        range: StatsRange,
        pool: &PgPool,
        max_age_secs: i64,
        force: bool,
    ) -> Result<Arc<DailySnapshot>, sqlx::Error> {
        let slot = &self.slots[range.index()];
        let _guard = slot.refresh_lock.lock().await;
        if !force {
            if let Some(existing) = slot.snapshot.load_full() {
                let age = Utc::now().timestamp() - existing.generated_at;
                if age >= 0 && age < max_age_secs && existing.range == range.as_str() {
                    return Ok(existing);
                }
            }
        }
        let snap = Arc::new(query_snapshot(range, pool).await?);
        slot.snapshot.store(Some(snap.clone()));
        Ok(snap)
    }
}

/// Run the three aggregate queries and assemble the snapshot.
async fn query_snapshot(
    range: StatsRange,
    pool: &PgPool,
) -> Result<DailySnapshot, sqlx::Error> {
    let (since, bucket, total_slots) = range.window();

    let model_rows = sqlx::query_as::<_, ModelAggRow>(
        // CASE-based conditional aggregation: the aggregate FILTER clause
        // needs PG 9.4+ and GaussDB (PG 9.2-based) rejects it.
        r#"SELECT model,
                  COUNT(*)::BIGINT AS requests,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens,
                  COALESCE(SUM(cached_tokens), 0)::BIGINT AS cached_tokens,
                  COUNT(CASE WHEN status_code >= 400 THEN 1 END)::BIGINT AS errors
           FROM boom_request_log
           WHERE created_at >= $1
           GROUP BY model
           ORDER BY requests DESC"#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    let active = sqlx::query_as::<_, ActiveRow>(
        r#"SELECT COUNT(DISTINCT key_hash)::BIGINT AS active_keys,
                  COUNT(DISTINCT team_id)::BIGINT AS active_teams
           FROM boom_request_log
           WHERE created_at >= $1"#,
    )
    .bind(since)
    .fetch_one(pool)
    .await?;

    let series_rows = sqlx::query_as::<_, SeriesRow>(
        r#"SELECT (FLOOR(EXTRACT(EPOCH FROM created_at) / $2) * $2)::BIGINT AS slot_epoch,
                  COUNT(*)::BIGINT AS requests,
                  COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens,
                  COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens
           FROM boom_request_log
           WHERE created_at >= $1
           GROUP BY 1
           ORDER BY 1"#,
    )
    .bind(since)
    .bind(bucket)
    .fetch_all(pool)
    .await?;

    let mut snapshot = DailySnapshot {
        range: range.as_str().to_string(),
        day: if matches!(range, StatsRange::Today) {
            cst_today_label()
        } else {
            String::new()
        },
        since_epoch: since.timestamp(),
        bucket_secs: bucket,
        total_slots,
        generated_at: Utc::now().timestamp(),
        total_requests: 0,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cached_tokens: 0,
        total_errors: 0,
        active_keys: active.active_keys,
        active_teams: active.active_teams,
        models: model_rows
            .into_iter()
            .map(|r| ModelDailyAgg {
                model: r.model,
                requests: r.requests,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                cached_tokens: r.cached_tokens,
                errors: r.errors,
            })
            .collect(),
        series: series_rows
            .into_iter()
            .map(|r| SeriesPoint {
                slot_epoch: r.slot_epoch,
                requests: r.requests,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
            })
            .collect(),
    };
    for m in &snapshot.models {
        snapshot.total_requests += m.requests;
        snapshot.total_input_tokens += m.input_tokens;
        snapshot.total_output_tokens += m.output_tokens;
        snapshot.total_cached_tokens += m.cached_tokens;
        snapshot.total_errors += m.errors;
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cst_day_start_is_midnight_beijing() {
        use chrono::Timelike;
        let start = cst_today_start_utc();
        let start_cst = start.with_timezone(&cst());
        assert_eq!(start_cst.hour(), 0);
        assert_eq!(start_cst.minute(), 0);
        assert_eq!(start_cst.second(), 0);
        // And it's in the past.
        assert!(start <= Utc::now());
    }

    #[test]
    fn range_param_roundtrip() {
        assert_eq!(StatsRange::from_param(""), Some(StatsRange::Today));
        assert_eq!(StatsRange::from_param("today"), Some(StatsRange::Today));
        assert_eq!(StatsRange::from_param("1d"), Some(StatsRange::Last1d));
        assert_eq!(StatsRange::from_param("3d"), Some(StatsRange::Last3d));
        assert_eq!(StatsRange::from_param("7d"), Some(StatsRange::Last7d));
        assert_eq!(StatsRange::from_param("30d"), None);
        for range in StatsRange::ALL {
            assert_eq!(StatsRange::from_param(range.as_str()), Some(range));
        }
    }

    #[test]
    fn windows_are_bucket_aligned_and_sized() {
        let now = Utc::now().timestamp();
        for range in StatsRange::ALL {
            let (since, bucket, total) = range.window();
            let since_ts = since.timestamp();
            assert!(since_ts <= now);
            assert_eq!(since_ts.rem_euclid(bucket), 0, "start on bucket grid");
            let span = now - since_ts;
            match range {
                StatsRange::Today => assert!(span >= 0 && span <= 86400),
                // Flooring the start to the bucket grid grows the span by
                // up to one bucket beyond the nominal window.
                StatsRange::Last1d => assert!((86400..86400 + bucket).contains(&span)),
                StatsRange::Last3d => assert!((259200..259200 + bucket).contains(&span)),
                StatsRange::Last7d => assert!((604800..604800 + bucket).contains(&span)),
            }
            let expected_window = match range {
                StatsRange::Today | StatsRange::Last1d => 86400,
                StatsRange::Last3d => 259200,
                StatsRange::Last7d => 604800,
            };
            assert_eq!(total * bucket, expected_window);
        }
    }
}
