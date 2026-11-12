//! boom-state: cross-instance mechanism primitives.
//!
//! Mechanism-only leaf crate — no business semantics. It moves *signals*
//! (instance liveness, config-revision changelog); business modules own
//! their reactions to those signals:
//!
//! - [`registry`]: instance heartbeat + liveness. One row per live
//!   process, all timestamps DB-side so liveness never trusts instance
//!   clocks.
//! - [`revision`]: DB-as-changelog. Writers bump a category counter after
//!   committing a business write; every instance polls the counters and
//!   reconciles changed categories. Reconciles are idempotent, so a
//!   redundant trigger is harmless.
//!
//! Hot-path request state does NOT belong here (windows stay in-memory in
//! boom-limiter); the quota-delta protocol lands in a later phase as
//! another module of this crate.

pub mod migrations;
pub mod registry;
pub mod revision;

pub use registry::{generate_instance_id, heartbeat, live_instances, InstanceInfo, InstanceRow};
pub use revision::{
    bump, bump_best_effort, snapshot, CATEGORY_ASSIGNMENT, CATEGORY_AUTH, CATEGORY_BLOCKLIST,
    CATEGORY_PLAN, CATEGORY_QUOTA,
};

/// Heartbeat cadence for `spawn_instance_heartbeat`.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 15;
/// Liveness TTL: an instance whose heartbeat is older is considered down
/// (rows linger for observability but stop counting as live).
pub const INSTANCE_TTL_SECS: u64 = 60;
/// Revision poll cadence — upper bound on config-change propagation.
pub const REVISION_POLL_INTERVAL_SECS: u64 = 5;
