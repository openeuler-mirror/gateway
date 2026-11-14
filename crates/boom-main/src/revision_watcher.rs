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
///
/// The UPDATE is pid-guarded, so a beat whose row was taken over (another
/// process registered the same id) or deleted reports it instead of
/// silently refreshing a row we no longer own — a duplicate identity is
/// never silent.
pub fn spawn_instance_heartbeat(state: AppState, mut shutdown: Receiver<()>) {
    let Some(pool) = state.db_pool.clone() else {
        return; // No DB — no cluster to be visible in.
    };
    let info = {
        let server = &state.inner.load().config.server;
        InstanceInfo::current(&state.instance_id, &server.host, server.port)
    };

    tokio::spawn(async move {
        let mut identity_lost = false;
        loop {
            match heartbeat(&pool, &info).await {
                Ok(boom_state::registry::Heartbeat::Own) => {}
                Ok(boom_state::registry::Heartbeat::ReRegistered) => {
                    tracing::warn!("instance heartbeat row had vanished — re-registered");
                }
                Ok(boom_state::registry::Heartbeat::TakenOver { holder_pid }) => {
                    if !identity_lost {
                        tracing::error!(
                            instance_id = %info.instance_id,
                            holder_pid,
                            "our instance registration was taken over by another process — \
                             duplicate instance id: restart one of them with a unique \
                             server.instance_id"
                        );
                    } else {
                        tracing::debug!(
                            instance_id = %info.instance_id,
                            holder_pid,
                            "instance registration still held by another process"
                        );
                    }
                    identity_lost = true;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "instance heartbeat failed (will retry)");
                }
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
        boom_state::CATEGORY_ROUTING => {
            // Peer model/alias/workflow/cost write: apply the same diff the
            // reload path runs. Idempotent, never clears a store.
            if let Err(e) = crate::config_reconcile::reconcile_routing_config(&deps).await {
                tracing::error!(error = %e, "revision reconcile (routing) failed");
            }
        }
        boom_state::CATEGORY_USER_TAG => {
            let config = state.inner.load().config.clone();
            if let Err(e) = crate::config_reconcile::adopt_user_tag_headers_db(
                pool,
                &state.user_tag_store,
                &config,
            )
            .await
            {
                tracing::error!(error = %e, "revision reconcile (user_tag) failed");
            }
        }
        boom_state::CATEGORY_TRACE => {
            let enabled = match boom_dashboard::config_kv::get_bool(
                pool,
                boom_dashboard::config_kv::TRACE_METRICS_ENABLED,
            )
            .await
            {
                Ok(v) => v.unwrap_or(false),
                Err(e) => {
                    tracing::warn!(error = %e, "trace-metrics re-adopt failed; keeping current state");
                    return;
                }
            };
            state.trace.set_metrics_enabled(enabled);
            tracing::info!(enabled, "trace metrics switch re-adopted (revision bump)");
        }
        other => {
            // warn, not debug: a bumped category without a dispatch arm means
            // someone added a write-side bump but forgot the reconcile —
            // propagation is silently broken and must be discoverable.
            tracing::warn!(category = other, "revision bump for unknown category — no reconcile arm, propagation skipped");
        }
    }
}
