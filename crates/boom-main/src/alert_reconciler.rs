//! Alert reconciler — periodically re-derives alert state from the sources
//! of truth and raises/clears alerts to match.
//!
//! This is the primary alert mechanism (the auto-disable/auto-enable hooks
//! in `admin_command.rs` are just a low-latency accelerator): alerts are
//! in-memory only, so a restart wipes them; the next reconcile pass rebuilds
//! them. Sources of truth:
//!
//! 1. `boom_model_deployment` rows with `enabled=false AND auto_disabled=true`
//!    — i.e. the health monitor took the node offline. Manual disables have
//!    `auto_disabled=false` and never alert.
//! 2. `TraceApi::otlp_status()` — `Some(status="offline")` means the OTLP
//!    traces exporter is configured but disconnected; `None` (exporter not
//!    assembled, e.g. after the user turned `trace.otlp` off and reloaded)
//!    or `"online"` clears the alert.

use std::time::Duration;

use boom_core::alert::AlertKind;
use boom_routing::DeploymentStore;

use crate::state::AppState;

pub const OTEL_ALERT_KEY: &str = "otel:exporter";

pub fn spawn_alert_reconciler(state: AppState, shutdown: tokio::sync::broadcast::Receiver<()>) {
    tokio::spawn(run_alert_reconciler(state, shutdown));
}

async fn run_alert_reconciler(
    state: AppState,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
            _ = shutdown.recv() => break,
        }
        reconcile(&state).await;
    }

    tracing::info!("Alert reconciler stopped");
}

const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

async fn reconcile(state: &AppState) {
    reconcile_deployments(state).await;
    reconcile_otlp(state).await;
}

async fn reconcile_deployments(state: &AppState) {
    let Some(pool) = state.db_pool.as_ref() else {
        // No DB — deployment alerts can't be evaluated; drop any stale ones
        // from before a config change removed the database.
        for key in state.alerts.active_keys() {
            if key.starts_with("deployment:") {
                state.alerts.clear_alert(&key);
            }
        }
        return;
    };

    let auto_disabled = match DeploymentStore::list_auto_disabled(pool).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "Alert reconciler: failed to list auto-disabled deployments");
            return;
        }
    };

    let expected: std::collections::HashSet<String> = auto_disabled
        .iter()
        .map(|t| format!("deployment:{}", t.deployment_id))
        .collect();

    for target in &auto_disabled {
        state.alerts.raise_alert(
            AlertKind::DeploymentAutoDisabled,
            format!("deployment:{}", target.deployment_id),
            target.model_name.clone(),
            format!(
                "Deployment '{}' (model {}) was auto-disabled by health monitoring and is out of routing",
                target.deployment_id, target.model_name
            ),
        );
    }

    // Reverse pass: alerts for deployments that are no longer auto-disabled
    // (auto-recovered, manually re-enabled, or deleted) must clear.
    for key in state.alerts.active_keys() {
        if key.starts_with("deployment:") && !expected.contains(&key) {
            state.alerts.clear_alert(&key);
        }
    }
}

async fn reconcile_otlp(state: &AppState) {
    let status: Option<boom_core::trace::ExporterStatusSnapshot> =
        boom_core::trace::TraceApi::otlp_status(state.trace.as_ref()).await;

    match status {
        // Exporter not configured (or user turned the otlp config off) —
        // an alert here must not survive.
        None => {
            state.alerts.clear_alert(OTEL_ALERT_KEY);
        }
        Some(snapshot) => {
            if snapshot.status.eq_ignore_ascii_case("offline") {
                state.alerts.raise_alert(
                    AlertKind::OtelExporterOffline,
                    OTEL_ALERT_KEY,
                    snapshot.endpoint.clone(),
                    format!(
                        "OTLP traces exporter at '{}' is offline (consecutive probe failures: {}, traces dropped during outage: {})",
                        snapshot.endpoint, snapshot.consecutive_probe_failures, snapshot.total_dropped_during_offline
                    ),
                );
            } else {
                state.alerts.clear_alert(OTEL_ALERT_KEY);
            }
        }
    }
}
