//! boom-alert — in-memory alert manager for the gateway.
//!
//! Leaf crate: depends only on boom-core (where `AlertApi` / `AlertNotifier`
//! and the shared `Alert` types live). Producers (boom-main's reconciler and
//! health-monitor hooks) raise/clear alerts through [`AlertManager`]; the
//! dashboard consumes read-only snapshots via `Arc<dyn boom_core::AlertApi>`.
//!
//! State is process-local: active alerts in a `DashMap`, cleared episodes in
//! a bounded history ring. A reconciler task in boom-main re-derives alert
//! state from the source of truth each cycle, so nothing needs to survive a
//! restart on disk.

mod manager;

pub use boom_core::alert::{Alert, AlertApi, AlertKind, AlertNotifier, AlertSnapshot, AlertStatus};
pub use manager::AlertManager;
