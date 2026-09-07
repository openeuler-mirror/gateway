//! Time-window helpers for stats endpoints.
//!
//! Centralises the parsing of `?range=1h|4h|8h|24h|custom` plus `from`/`to`
//! ISO-8601 strings, the bucket-size selection rules, and the PostgreSQL
//! aggregation helpers shared by every DB-backed chart on the dashboard.
//!
//! Future charts reuse this module: parse the query → call `to_window()` →
//! use `bucket_secs` as the SQL divisor and `expected_buckets()` to backfill
//! empty buckets → pass each bucket's ISO `ts` to the frontend, which formats
//! it in the viewer's local timezone.

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::Deserialize;

/// Maximum lookback permitted for aggregation queries. Logs themselves are
/// never TTL'd — this clamp exists purely to keep dashboard queries bounded.
pub const MAX_RANGE_DAYS: i64 = 90;

/// Preset quick-range buttons. Memory path is only `Hour1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Hour1,
    Hour4,
    Hour8,
    Hour24,
}

/// A fully resolved range — either a preset (anchored to "now") or a custom
/// window with explicit from/to. Custom windows are clamped to MAX_RANGE_DAYS.
#[derive(Debug, Clone)]
pub enum ResolvedRange {
    Preset(Preset),
    Custom(TimeWindow),
}

/// A bounded time window with bucket granularity.
#[derive(Debug, Clone)]
pub struct TimeWindow {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub bucket_secs: i64,
}

/// Query-string shape shared by every stats endpoint.
#[derive(Debug, Deserialize)]
pub struct StatsRangeQuery {
    #[serde(default = "default_range")]
    pub range: String,
    pub from: Option<String>,
    pub to: Option<String>,
}

fn default_range() -> String {
    "1h".to_string()
}

/// Result of `ResolvedRange::parse`: the resolved range, plus a flag telling
/// the handler whether to consult the in-memory tracker (`Hour1` preset only)
/// or hit the DB.
pub struct ParsedRange {
    pub resolved: ResolvedRange,
    pub use_memory: bool,
}

impl ResolvedRange {
    /// Parse query params into a resolved range.
    ///
    /// Returns `use_memory = true` only when `range=1h` and no custom override
    /// is supplied — the in-memory tracker covers exactly the last hour and is
    /// always fresher than a DB aggregation.
    pub fn parse(q: &StatsRangeQuery) -> Result<ParsedRange, String> {
        let has_custom = q.from.is_some() || q.to.is_some();
        let preset = match q.range.as_str() {
            "1h" => Some(Preset::Hour1),
            "4h" => Some(Preset::Hour4),
            "8h" => Some(Preset::Hour8),
            "24h" => Some(Preset::Hour24),
            "custom" => None,
            other => return Err(format!("unknown range '{}'", other)),
        };

        // If custom from/to supplied, build a custom window regardless of preset.
        if has_custom {
            let from_raw = q.from.as_deref().ok_or("missing 'from'")?;
            let to_raw = q.to.as_deref().ok_or("missing 'to'")?;
            let from = parse_iso(from_raw)?;
            let to = parse_iso(to_raw)?;
            if to <= from {
                return Err("'to' must be strictly after 'from'".to_string());
            }
            let window = build_custom_window(from, to);
            return Ok(ParsedRange {
                resolved: ResolvedRange::Custom(window),
                use_memory: false,
            });
        }

        match preset {
            Some(Preset::Hour1) => Ok(ParsedRange {
                resolved: ResolvedRange::Preset(Preset::Hour1),
                use_memory: true,
            }),
            Some(p) => Ok(ParsedRange {
                resolved: ResolvedRange::Preset(p),
                use_memory: false,
            }),
            None => Err("range=custom requires 'from' and 'to'".to_string()),
        }
    }

    pub fn to_window(&self) -> TimeWindow {
        let now = Utc::now();
        match self {
            ResolvedRange::Preset(Preset::Hour1) => TimeWindow {
                from: now - Duration::minutes(60),
                to: now,
                bucket_secs: 60,
            },
            ResolvedRange::Preset(Preset::Hour4) => TimeWindow {
                from: now - Duration::hours(4),
                to: now,
                bucket_secs: 300,
            },
            ResolvedRange::Preset(Preset::Hour8) => TimeWindow {
                from: now - Duration::hours(8),
                to: now,
                bucket_secs: 600,
            },
            ResolvedRange::Preset(Preset::Hour24) => TimeWindow {
                from: now - Duration::hours(24),
                to: now,
                bucket_secs: 1800,
            },
            ResolvedRange::Custom(w) => w.clone(),
        }
    }
}

impl TimeWindow {
    /// Generate every bucket timestamp that should exist in `[from, to)`.
    ///
    /// Buckets are anchored on `from` (NOT aligned to wall-clock boundaries)
    /// so each bar represents an exactly equal `bucket_secs` slice of the
    /// requested range. The last bucket may extend slightly past `to` — that's
    /// fine, the SQL filters with `created_at < $to` so no records beyond `to`
    /// ever land in it.
    pub fn expected_buckets(&self) -> Vec<DateTime<Utc>> {
        let mut v = Vec::new();
        let mut ts = self.from;
        while ts < self.to {
            v.push(ts);
            ts = ts + Duration::seconds(self.bucket_secs);
        }
        v
    }

    /// Compute the bucket timestamp for a given epoch second.
    /// Mirrors the SQL `FLOOR((epoch - from_epoch) / bucket_secs) * bucket_secs + from_epoch`.
    pub fn bucket_of(&self, epoch_secs: i64) -> DateTime<Utc> {
        let from_epoch = self.from.timestamp();
        let idx = (epoch_secs - from_epoch).div_euclid(self.bucket_secs);
        let bucket_epoch = from_epoch + idx * self.bucket_secs;
        DateTime::<Utc>::from_timestamp(bucket_epoch, 0).unwrap_or(self.from)
    }
}

/// Parse a few common ISO/local formats accepted from the URL.
/// Accepts RFC3339 with offset, `YYYY-MM-DDTHH:MM:SS`, `YYYY-MM-DDTHH:MM`,
/// and `YYYY-MM-DD HH:MM:SS` (treating naive values as UTC).
fn parse_iso(s: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Ok(d.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
        .map_err(|e| format!("invalid datetime '{}': {}", s, e))?;
    Ok(naive.and_utc())
}

/// Build a custom window, clamping to MAX_RANGE_DAYS from `to`.
/// Bucket size targets ~48 buckets for the (possibly clamped) duration.
fn build_custom_window(from: DateTime<Utc>, to: DateTime<Utc>) -> TimeWindow {
    let max_secs = MAX_RANGE_DAYS * 86400;
    let raw_secs = (to - from).num_seconds();
    let effective_from = if raw_secs > max_secs {
        to - Duration::seconds(max_secs)
    } else {
        from
    };
    let duration_secs = (to - effective_from).num_seconds();
    let bucket_secs = pick_bucket_secs(duration_secs);
    TimeWindow {
        from: effective_from,
        to,
        bucket_secs,
    }
}

/// Pick the smallest "nice" bucket >= duration/48. Bucket sizes are chosen so
/// SQL `date_trunc`-style rounding isn't needed — the handler does its own
/// integer division on the epoch.
fn pick_bucket_secs(duration_secs: i64) -> i64 {
    const NICE: &[i64] = &[
        60,      // 1 min
        300,     // 5 min
        600,     // 10 min
        900,     // 15 min
        1800,    // 30 min
        3600,    // 1 hour
        7200,    // 2 hours
        14400,   // 4 hours
        21600,   // 6 hours
        43200,   // 12 hours
        86400,   // 1 day
        172800,  // 2 days
    ];
    let target = (duration_secs / 48).max(60);
    for &n in NICE {
        if n >= target {
            return n;
        }
    }
    172800
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_1h_uses_memory() {
        let q = StatsRangeQuery {
            range: "1h".to_string(),
            from: None,
            to: None,
        };
        let parsed = ResolvedRange::parse(&q).unwrap();
        assert!(parsed.use_memory);
    }

    #[test]
    fn parse_4h_does_not_use_memory() {
        let q = StatsRangeQuery {
            range: "4h".to_string(),
            from: None,
            to: None,
        };
        let parsed = ResolvedRange::parse(&q).unwrap();
        assert!(!parsed.use_memory);
        let w = parsed.resolved.to_window();
        assert_eq!(w.bucket_secs, 300);
    }

    #[test]
    fn custom_window_picks_bucket_size() {
        let now = Utc::now();
        let q = StatsRangeQuery {
            range: "custom".to_string(),
            from: Some((now - Duration::hours(2)).to_rfc3339()),
            to: Some(now.to_rfc3339()),
        };
        let parsed = ResolvedRange::parse(&q).unwrap();
        assert!(!parsed.use_memory);
        let w = parsed.resolved.to_window();
        // 2h / 48 ≈ 150s → smallest nice >= 150 is 300s.
        assert_eq!(w.bucket_secs, 300);
    }

    #[test]
    fn custom_window_clamps_to_90_days() {
        let now = Utc::now();
        let q = StatsRangeQuery {
            range: "custom".to_string(),
            from: Some((now - Duration::days(365)).to_rfc3339()),
            to: Some(now.to_rfc3339()),
        };
        let parsed = ResolvedRange::parse(&q).unwrap();
        let w = parsed.resolved.to_window();
        let actual_days = (w.to - w.from).num_days();
        assert_eq!(actual_days, MAX_RANGE_DAYS);
    }

    #[test]
    fn custom_requires_from_and_to() {
        let q = StatsRangeQuery {
            range: "custom".to_string(),
            from: None,
            to: None,
        };
        assert!(ResolvedRange::parse(&q).is_err());
    }

    #[test]
    fn custom_rejects_inverted_window() {
        let now = Utc::now();
        let q = StatsRangeQuery {
            range: "custom".to_string(),
            from: Some(now.to_rfc3339()),
            to: Some((now - Duration::hours(1)).to_rfc3339()),
        };
        assert!(ResolvedRange::parse(&q).is_err());
    }

    #[test]
    fn expected_buckets_anchor_on_from_uniform() {
        // Buckets anchor on `from` and are exactly `bucket_secs` wide.
        // For a 60-minute window with 5-minute buckets, that's exactly 12 buckets.
        let from = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let to = from + Duration::minutes(60);
        let w = TimeWindow {
            from,
            to,
            bucket_secs: 300,
        };
        let buckets = w.expected_buckets();
        assert_eq!(buckets.len(), 12);
        // First bucket starts exactly at `from`.
        assert_eq!(buckets[0], from);
        // Each bucket is exactly 300s after the previous.
        for i in 1..buckets.len() {
            let delta = buckets[i].timestamp() - buckets[i - 1].timestamp();
            assert_eq!(delta, 300);
        }
        // All buckets strictly less than `to`.
        for ts in &buckets {
            assert!(*ts < to);
        }
    }

    #[test]
    fn expected_buckets_handles_unaligned_from() {
        // `from` not on a bucket boundary — buckets still anchor on `from` as-is,
        // so each bar represents the same duration and `from` is preserved.
        let from = DateTime::from_timestamp(1_700_000_042, 0).unwrap();
        let from_offset = from.timestamp().rem_euclid(60);
        let to = from + Duration::minutes(60);
        let w = TimeWindow {
            from,
            to,
            bucket_secs: 300,
        };
        let buckets = w.expected_buckets();
        assert_eq!(buckets.len(), 12);
        assert_eq!(buckets[0], from);
        // Each bucket boundary carries the same sub-minute offset as `from`.
        for ts in &buckets {
            assert_eq!(ts.timestamp().rem_euclid(60), from_offset);
        }
    }

    #[test]
    fn bucket_of_snaps_to_nearest_anchor() {
        let from = DateTime::from_timestamp(1_000_000_000, 0).unwrap();
        let w = TimeWindow {
            from,
            to: from + Duration::hours(1),
            bucket_secs: 300,
        };
        // An epoch 700s after `from` lands in bucket index 2 (covers [600, 900)).
        assert_eq!(w.bucket_of(1_000_000_700).timestamp(), 1_000_000_600);
        // An epoch before `from` snaps to a negative-index bucket boundary.
        assert_eq!(w.bucket_of(1_000_000_000 - 100).timestamp(), 1_000_000_000 - 300);
    }
}
