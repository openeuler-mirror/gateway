//! boom-state: cross-instance mechanism primitives.
//!
//! Mechanism-only leaf crate — no business semantics. It moves *signals*
//! (instance liveness, config-revision changelog); business modules own
//! their reactions to those signals:
//!
//! - [`registry`]: stable instance identity (configured or derived), boot
//!   registration with duplicate-id detection, and pid-guarded heartbeat.
//!   All liveness timestamps are DB-side so liveness never trusts
//!   instance clocks.
//! - [`revision`]: DB-as-changelog. Writers bump a category counter after
//!   committing a business write; every instance polls the counters and
//!   reconciles changed categories. Reconciles are idempotent, so a
//!   redundant trigger is harmless, and a periodic full re-assertion
//!   ([`FULL_RECONCILE_EVERY_POLLS`]) bounds the staleness of lost signals.
//!
//! Hot-path request state does NOT belong here (windows stay in-memory in
//! boom-limiter); the quota-delta protocol lands in a later phase as
//! another module of this crate.

pub mod migrations;
pub mod registry;
pub mod revision;

pub use registry::{
    heartbeat, live_instances, register, resolve_hostname, Heartbeat, InstanceInfo, InstanceRow,
    Registration, RegisterError,
};
pub use revision::{
    bump, bump_best_effort, snapshot, CATEGORY_ASSIGNMENT, CATEGORY_AUTH, CATEGORY_BLOCKLIST,
    CATEGORY_PLAN, CATEGORY_PROMPTLOG, CATEGORY_QUOTA, CATEGORY_ROUTER_BEHAVIOR,
    CATEGORY_ROUTER_POLICY, CATEGORY_ROUTING, CATEGORY_TRACE, CATEGORY_USER_TAG,
};

/// Heartbeat cadence for `spawn_instance_heartbeat`.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 15;
/// Liveness TTL: an instance whose heartbeat is older is considered down
/// (rows linger for observability but stop counting as live).
pub const INSTANCE_TTL_SECS: u64 = 60;
/// Revision poll cadence — upper bound on config-change propagation.
pub const REVISION_POLL_INTERVAL_SECS: u64 = 5;
/// Every Nth poll, watchers re-run the full reconcile set regardless of the
/// counters. The revision signal only *accelerates* convergence; this
/// periodic re-assertion bounds the staleness of any lost signal (a bump
/// that failed after the business write, a dispatch that failed, a deleted
/// revision row) to one full-reconcile interval. 120 polls × 5s = 10 min.
pub const FULL_RECONCILE_EVERY_POLLS: u64 = 120;
