//! Cross-instance mechanism tasks (boom-state consumers).
//!
//! - [`spawn_instance_heartbeat`]: upsert this process's liveness row every
//!   [`boom_state::HEARTBEAT_INTERVAL_SECS`]. DB-side NOW() only — instance
//!   clocks are never trusted for liveness.
//! - [`spawn_revision_watcher`]: poll `boom_config_revision` every
//!   [`boom_state::REVISION_POLL_INTERVAL_SECS`] and react to changed
//!   categories by reconciling the matching store from the DB. The local
//!   instance already applied its own writes in memory (dashboard handlers
//!   mutate memory directly); this path is how PEERS converge. All
//!   reconciles are idempotent diffs, so a redundant trigger is harmless.

use sqlx::PgPool;
use tokio::sync::broadcast::Receiver;

use crate::config_reconcile::ReconcileDeps;
use crate::state::AppState;
use boom_state::registry::{heartbeat, InstanceInfo};

/// Upsert the heartbeat row every 15s. First beat fires immediately so a
/// freshly-booted instance is visible in `boom_instance_state` at once.
pub fn spawn_instance_heartbeat(state: AppState, mut shutdown: Receiver<()>) {
    let Some(pool) = state.db_pool.clone() else {
        return; // No DB — no cluster to be visible in.
    };
    let (host, port) = {
        let server = &state.inner.load().config.server;
        (server.host.clone(), server.port)
    };
    let info = InstanceInfo {
        instance_id: state.instance_id.to_string(),
        hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        // The configured bind address. When bound to 0.0.0.0 this is not
        // directly routable — operators set a real host for scatter
        // drill-down; refinement (auto-detect pod IP) is dashboard-phase.
        advertise_addr: Some(format!("{}:{}", host, port)),
    };

    tokio::spawn(async move {
        loop {
            if let Err(e) = heartbeat(&pool, &info).await {
                tracing::warn!(error = %e, "instance heartbeat failed (will retry)");
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(
                    boom_state::HEARTBEAT_INTERVAL_SECS,
                )) => {}
                _ = shutdown.recv() => {
                    tracing::debug!("instance heartbeat task shutting down");
                    return;
                }
            }
        }
    });
}

/// Poll the revision counters and reconcile changed categories. The
/// baseline snapshot is taken AFTER startup seed/reconcile finished (this
/// task spawns later), so boot-time bumps do not trigger redundant work.
pub fn spawn_revision_watcher(state: AppState, mut shutdown: Receiver<()>) {
    let Some(pool) = state.db_pool.clone() else {
        return; // No DB — nothing is shared, nothing to watch.
    };

    tokio::spawn(async move {
        // Baseline: retry until the DB answers. An empty-map fallback would
        // make the first successful poll look like "everything changed" and
        // trigger a full reconcile storm on every boot with a DB hiccup.
        let mut last = loop {
            match boom_state::revision::snapshot(&pool).await {
                Ok(map) => break map,
                Err(e) => {
                    tracing::warn!(error = %e, "revision snapshot failed; retrying");
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(
                            boom_state::REVISION_POLL_INTERVAL_SECS,
                        )) => {}
                        _ = shutdown.recv() => return,
                    }
                }
            }
        };

        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(
                    boom_state::REVISION_POLL_INTERVAL_SECS,
                )) => {}
                _ = shutdown.recv() => {
                    tracing::debug!("revision watcher shutting down");
                    return;
                }
            }
            let current = match boom_state::revision::snapshot(&pool).await {
                Ok(map) => map,
                Err(e) => {
                    tracing::warn!(error = %e, "revision poll failed; will retry");
                    continue;
                }
            };
            for (category, rev) in &current {
                if last.get(category) != Some(rev) {
                    dispatch(category, &state, &pool).await;
                }
            }
            last = current;
        }
    });
}

/// React to one changed category. Each branch re-reads the DB and applies
/// a diff to memory; failures log and wait for the next bump of that
/// category (the counter already moved, so the next change re-triggers).
async fn dispatch(category: &str, state: &AppState, pool: &PgPool) {
    let deps = ReconcileDeps {
        pool,
        deployment_store: &state.deployment_store,
        alias_store: &state.alias_store,
        plan_store: &state.plan_store,
        block_rule_store: &state.block_rule_store,
        flow_controller: &state.flow_controller,
        request_rate: &state.request_rate,
        cost_template_store: &state.cost_template_store,
        workflow_store: &state.workflow_store,
    };
    match category {
        boom_state::CATEGORY_PLAN => {
            if let Err(e) = crate::config_reconcile::reconcile_plans(&deps).await {
                tracing::error!(error = %e, "revision reconcile (plan) failed");
            }
        }
        boom_state::CATEGORY_ASSIGNMENT => {
            state.plan_store.reconcile_assignments_from_db(pool).await;
        }
        boom_state::CATEGORY_BLOCKLIST => {
            if let Err(e) = crate::config_reconcile::reconcile_block_rules(&deps).await {
                tracing::error!(error = %e, "revision reconcile (blocklist rules) failed");
            }
            let config = state.inner.load().config.clone();
            if let Err(e) = crate::config_reconcile::adopt_blocklist_enabled_db(
                pool,
                &state.block_rule_store,
                &config,
            )
            .await
            {
                tracing::error!(error = %e, "revision reconcile (blocklist switch) failed");
            }
        }
        boom_state::CATEGORY_AUTH => {
            // Coarse by design: which key changed is not encoded in the
            // revision signal, and the moka cache is cheap to rebuild
            // (one DB miss per live key on the next request).
            state.inner.load().auth.invalidate_all().await;
            tracing::info!("auth cache invalidated (revision bump)");
        }
        boom_state::CATEGORY_QUOTA => {
            // Decision-base flip: re-adopt the KV and switch the limiter's
            // enforcement view. Counter data itself needs no action — the
            // incremental push protocol converges it regardless of mode.
            let shared = match boom_dashboard::config_kv::get_bool(
                pool,
                boom_dashboard::config_kv::DECISION_BASE_SHARED,
            )
            .await
            {
                Ok(v) => v.unwrap_or(false),
                Err(e) => {
                    tracing::warn!(error = %e, "decision-base re-adopt failed; keeping current mode");
                    return;
                }
            };
            state.limiter.set_decision_shared(shared);
            tracing::info!(shared, "quota decision base switched (revision bump)");
        }
        other => {
            tracing::debug!(category = other, "revision bump for unknown category — ignored");
        }
    }
}
