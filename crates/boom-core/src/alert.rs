//! Alert types — shared between boom-alert (producer) and boom-dashboard
//! (consumer). Defined here so the dashboard can read the snapshot via
//! `Arc<dyn AlertApi>` without depending on boom-alert (CLAUDE.md §5
//! trait-over-concrete-type pattern, same shape as `TraceApi`).
//!
//! Alerts are purely in-memory: `AlertManager` lives on AppState's top-level
//! Arc (survives hot reload) but not across process restarts. The reconciler
//! task in boom-main rebuilds alert state within one poll cycle after a
//! restart, so no DB persistence is needed.

use async_trait::async_trait;
use serde::Serialize;

/// Category of an alert. Serialized snake_case for the dashboard/API JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// A deployment whose `enabled=true` was auto-disabled by the health
    /// monitor (request-failure threshold or offline probe). Manual disables
    /// (`auto_disabled=false`) never produce this alert.
    DeploymentAutoDisabled,
    /// The OTLP traces exporter is configured (`otlp.enabled=true`) but the
    /// exporter's connection state machine went offline.
    OtelExporterOffline,
}

impl AlertKind {
    /// Stable string id used in alert keys, e.g. `deployment:{id}` /
    /// `otel:exporter`.
    pub fn prefix(&self) -> &'static str {
        match self {
            AlertKind::DeploymentAutoDisabled => "deployment",
            AlertKind::OtelExporterOffline => "otel",
        }
    }
}

/// Lifecycle state of an `Alert`. Active alerts live in the manager's active
/// map; cleared alerts move to the history ring buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertStatus {
    Active,
    Cleared,
}

/// One alert episode. `raised_at` marks episode start; `cleared_at` is set
/// when the alert resolves (moved to history).
#[derive(Debug, Clone, Serialize)]
pub struct Alert {
    /// Unique alert key, e.g. `deployment:{deployment_id}` or
    /// `otel:exporter`. Raise on an existing active key is idempotent.
    pub key: String,
    pub kind: AlertKind,
    /// Human-facing target: model_name for deployments, endpoint for OTel.
    pub target: String,
    pub message: String,
    pub status: AlertStatus,
    /// Unix milliseconds.
    pub raised_at: i64,
    /// Unix milliseconds; `None` while active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<i64>,
}

/// Snapshot returned by `AlertApi::snapshot`. Read-only, point-in-time.
#[derive(Debug, Clone, Serialize)]
pub struct AlertSnapshot {
    /// `true` when there are no active alerts (green light).
    pub healthy: bool,
    pub active_count: usize,
    /// Currently active alerts, sorted by `raised_at` descending.
    pub active: Vec<Alert>,
    /// Recent alert episodes (both raised-then-cleared and still-active),
    /// sorted by `raised_at` descending. Bounded by the manager's history
    /// ring capacity.
    pub history: Vec<Alert>,
}

/// Read-only alert API surfaced to the dashboard. Producer is
/// `boom_alert::AlertManager`; dashboard holds `Arc<dyn AlertApi>` without
/// depending on the boom-alert crate (§5 pattern).
#[async_trait]
pub trait AlertApi: Send + Sync + 'static {
    async fn snapshot(&self) -> AlertSnapshot;
}

/// Extension point for push notifications (webhook / dingtalk / email ...).
/// Fired fire-and-forget by `AlertManager` on state transitions only (a
/// redundant raise or a clear of an unknown key does not fire). Not wired
/// up yet; implement and pass to `AlertManager::set_notifier` when needed.
pub trait AlertNotifier: Send + Sync + 'static {
    fn on_raise(&self, alert: &Alert);
    fn on_clear(&self, alert: &Alert);
}

/// Unix milliseconds now.
pub fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
