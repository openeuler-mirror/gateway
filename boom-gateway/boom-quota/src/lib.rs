//! Quota & billing counters — distinct from rate-limit windows.
//!
//! Owns `boom_rate_limit_cumulative` table. Cumulative token / cost counters
//! persist across restarts; multi-window token / cost windows (for TPM and
//! cost-per-window limits) live in `boom_rate_limit_state` alongside the
//! `SlidingWindowLimiter`'s request-count windows.
//!
//! Lifecycle:
//! - `settle_usage` is called after the upstream provider returns real token
//!   counts. Adds to in-memory counters and marks them dirty.
//! - A periodic background task calls `sync_to_db` every ~30s; only dirty
//!   entries are written. Crash loss is bounded to one sync interval.
//! - On startup, `restore_from_db` reloads all cumulative rows and any
//!   non-expired window rows.
//!
//! Recompute (key cleanup): `recompute_team_cumulative` SUMs member keys'
//! cumulative rows in one SQL query and overwrites the team row — O(team_size),
//! not O(log_count).

pub mod migrations;
pub mod store;

pub use store::{
    decimal_to_micros, micros_to_decimal, CumulativeKind, CumulativeSnapshot, QuotaScope,
    QuotaStore, TeamRecomputeResult, WindowInfo, WindowKind, WindowSnapshot,
};
