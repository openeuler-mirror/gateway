//! Cross-instance mechanism tasks (boom-state consumers).
//!
//! - [`spawn_instance_heartbeat`]: upsert this process's liveness row every
//!   [`boom_state::HEARTBEAT_INTERVAL_SECS`]. DB-side NOW() only — instance
//!   clocks are never trusted for liveness.
//! - [`spawn_instance_telemetry`]: publish this instance's in-flight
//!   telemetry snapshot into the same row every
//!   `instance_telemetry.interval_secs` (default 3s), so any instance's
//!   dashboard can render peer state.
//! - [`spawn_revision_watcher`]: poll `boom_config_revision` every
//!   [`boom_state::REVISION_POLL_INTERVAL_SECS`] and react to changed
//!   categories by reconciling the matching store from the DB. The local
//!   instance already applied its own writes in memory (dashboard handlers
//!   mutate memory directly); this path is how PEERS converge. All
//!   reconciles are idempotent diffs, so a redundant trigger is harmless.
//!   Convergence is signal-driven but not signal-dependent: a failed
//!   reconcile withholds its revision (retried next poll) and a periodic
//!   full re-assertion bounds the staleness of any lost signal.

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
            match heartbeat(&pool, &info, None).await {
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
                    // Fencing: the id now belongs to the other process. This
                    // one must stop writing shared state (quota deltas and
                    // watermarks especially) — two writers in one seq
                    // namespace corrupt each other's ambiguous-failure
                    // recovery. Sticky: a process that lost its identity
                    // does not get it back.
                    state.limiter.seal_push();
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

/// Publish this instance's telemetry snapshot (counters only — no per-user
/// detail) into `boom_instance_state.snapshot` every
/// `instance_telemetry.interval_secs` (default 3s, reload-effective). The
/// snapshot piggybacks the pid-guarded heartbeat UPDATE: liveness stays at
/// least as fresh as the telemetry cadence, and identity fencing is the
/// heartbeat's concern (it seals shared-state writes on takeover —
/// telemetry is display-only and the pid guard already no-ops our writes).
///
/// Snapshot semantics: overwrite in place, no history — this is a
/// dashboard view of "now", not an audit source.
pub fn spawn_instance_telemetry(state: AppState, mut shutdown: Receiver<()>) {
    let Some(pool) = state.db_pool.clone() else {
        return; // No DB — no cluster to share telemetry with.
    };
    let info = {
        let server = &state.inner.load().config.server;
        InstanceInfo::current(&state.instance_id, &server.host, server.port)
    };

    tokio::spawn(async move {
        let mut identity_lost = false;
        loop {
            // base_min captured before snapshot_all(): buckets are
            // wall-clock minute-aligned, counts[i] maps to minute
            // base_min - 59 + i. A minute-boundary straddle mid-call
            // shifts one bucket for one 3s cycle — display-only, self-heals.
            let rate_base_min = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                / 60;
            let telemetry = build_instance_telemetry(
                state.flow_controller.get_stats(),
                state.inflight.get_deployment_stats(),
                state.request_rate.snapshot_all(),
                rate_base_min,
            );
            match serde_json::to_string(&telemetry) {
                Ok(json) => match heartbeat(&pool, &info, Some(&json)).await {
                    Ok(boom_state::registry::Heartbeat::Own)
                    | Ok(boom_state::registry::Heartbeat::ReRegistered) => {}
                    Ok(boom_state::registry::Heartbeat::TakenOver { holder_pid }) => {
                        // The heartbeat task already fences shared-state
                        // writes; telemetry needs no more than a sticky log
                        // (pid-guarded writes are no-ops for us now).
                        if !identity_lost {
                            tracing::error!(
                                instance_id = %info.instance_id,
                                holder_pid,
                                "instance telemetry refusing to publish — row held by \
                                 another process (duplicate instance id)"
                            );
                        } else {
                            tracing::debug!(
                                instance_id = %info.instance_id,
                                holder_pid,
                                "instance telemetry still withheld (row held elsewhere)"
                            );
                        }
                        identity_lost = true;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "instance telemetry write failed (will retry)");
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "instance telemetry serialization failed");
                }
            }
            let interval = state
                .inner
                .load()
                .config
                .instance_telemetry
                .effective_interval_secs();
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                _ = shutdown.recv() => {
                    tracing::debug!("instance telemetry task shutting down");
                    return;
                }
            }
        }
    });
}

/// Merge the two in-flight views into the cross-instance wire snapshot.
/// FlowControl stats are authoritative (they carry queue depth and caps);
/// deployments without an FC slot fall back to the raw tracker (caps
/// unknown → 0). Deployments absent from both have no activity — omitted,
/// readers default missing keys to zero.
fn build_instance_telemetry(
    fc_stats: Vec<boom_flowcontrol::FlowControlStat>,
    tracker_stats: Vec<boom_routing::DeploymentInFlightStat>,
    rate_stats: Vec<(String, String, Vec<(String, u64)>)>,
    rate_base_min: u64,
) -> boom_core::InstanceTelemetry {
    let inflight = fc_stats
        .into_iter()
        .map(|s| {
            (
                s.deployment_id,
                boom_core::InflightStat {
                    fc_queue: s.waiters + s.vip_waiters,
                    in_reqs: s.current_inflight as u64,
                    in_context: s.current_context,
                    in_reqs_max: s.max_inflight,
                    in_context_max: s.max_context,
                },
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let inflight = tracker_stats
        .into_iter()
        .fold(inflight, |mut acc, d| {
            acc.entry(d.deployment_id).or_insert_with(|| {
                boom_core::InflightStat {
                    in_reqs: d.inflight_requests,
                    in_context: d.inflight_input_chars,
                    ..Default::default()
                }
            });
            acc
        });
    // `_total` is skipped (readers recompute the grand total); all-zero
    // series (traffic aged out of the window) stay out of the blob.
    let series = rate_stats
        .into_iter()
        .filter(|(dep_id, _, _)| dep_id != "_total")
        .filter_map(|(dep_id, model, data)| {
            let counts: Vec<u64> = data.into_iter().map(|(_, c)| c).collect();
            (!counts.iter().all(|&c| c == 0)).then(|| {
                (
                    dep_id,
                    boom_core::RateSeries { model, counts },
                )
            })
        })
        .collect::<std::collections::HashMap<_, _>>();
    boom_core::InstanceTelemetry {
        inflight,
        rate_1h: boom_core::RateTelemetry {
            base_min: rate_base_min,
            series,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_merges_fc_and_tracker_views() {
        let fc = vec![boom_flowcontrol::FlowControlStat {
            deployment_id: "dep-fc".into(),
            current_inflight: 3,
            current_context: 3000,
            waiters: 1,
            vip_waiters: 2,
            max_inflight: 10,
            max_context: 100_000,
        }];
        let tracker = vec![
            // FC-covered deployment: tracker must NOT override FC values.
            boom_routing::DeploymentInFlightStat {
                model: "m".into(),
                deployment_id: "dep-fc".into(),
                inflight_requests: 999,
                inflight_input_chars: 999,
            },
            // Tracker-only deployment: no FC slot → caps 0.
            boom_routing::DeploymentInFlightStat {
                model: "m2".into(),
                deployment_id: "dep-raw".into(),
                inflight_requests: 2,
                inflight_input_chars: 500,
            },
        ];
        let rate = vec![
            (
                "_total".into(),
                "_total".into(),
                vec![("now".into(), 7)],
            ),
            (
                "dep-fc".into(),
                "m".into(),
                vec![("-59m".into(), 0), ("-1m".into(), 3), ("now".into(), 4)],
            ),
            // All-zero series (traffic aged out) stays out of the snapshot.
            (
                "dep-old".into(),
                "m2".into(),
                vec![("now".into(), 0)],
            ),
        ];

        let t = build_instance_telemetry(fc, tracker, rate, 29_000_059);

        let fc = &t.inflight["dep-fc"];
        assert_eq!(fc.fc_queue, 3); // waiters + vip_waiters
        assert_eq!(fc.in_reqs, 3); // FC wins over tracker's 999
        assert_eq!(fc.in_reqs_max, 10);
        assert_eq!(fc.in_context_max, 100_000);
        let raw = &t.inflight["dep-raw"];
        assert_eq!(raw.in_reqs, 2);
        assert_eq!(raw.in_context, 500);
        assert_eq!(raw.fc_queue, 0);
        assert_eq!(raw.in_reqs_max, 0);
        assert_eq!(t.inflight.len(), 2);

        assert_eq!(t.rate_1h.base_min, 29_000_059);
        assert_eq!(t.rate_1h.series.len(), 1); // _total + all-zero dropped
        let s = &t.rate_1h.series["dep-fc"];
        assert_eq!(s.model, "m");
        assert_eq!(s.counts, vec![0, 3, 4]);
    }
}

/// Poll the revision counters and reconcile changed categories. The
/// baseline snapshot is taken AFTER startup seed/reconcile finished (this
/// task spawns later), so boot-time bumps do not trigger redundant work.
///
/// Two guarantees keep convergence bounded:
/// - A failed reconcile withholds the baseline for that category — the
///   next poll (5s) retries instead of waiting for the next bump.
/// - Every [`boom_state::FULL_RECONCILE_EVERY_POLLS`] polls all
///   signal-driven categories (auth excluded) re-read the DB regardless
///   of the counters, so a lost signal (failed bump after the business
///   write, a deleted revision row) self-heals within one interval.
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

        let mut polls: u64 = 0;
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
            polls += 1;
            // Periodic full re-assert: the revision signal only accelerates
            // convergence, it is not the sole carrier. Every Nth poll all
            // signal-driven categories (auth excepted — invalidating the
            // whole auth cache is too costly to run unconditionally) are
            // reconciled regardless of the counters, bounding the staleness
            // of any lost signal (failed bump, failed dispatch, a deleted
            // revision row) to one full-reconcile interval.
            let full = polls % boom_state::FULL_RECONCILE_EVERY_POLLS == 0;
            for (category, rev) in &current {
                let changed = last.get(category) != Some(rev);
                let force = full && category != boom_state::CATEGORY_AUTH;
                if !changed && !force {
                    continue;
                }
                match dispatch(category, &state, &pool).await {
                    Ok(()) => {
                        // Consume the signal only after a successful
                        // reconcile: a failure withholds the baseline, so
                        // the next poll still sees it changed and retries.
                        last.insert(category.clone(), *rev);
                    }
                    Err(e) => {
                        tracing::error!(
                            category,
                            error = %e,
                            "revision reconcile failed — signal withheld, will retry next poll"
                        );
                    }
                }
            }
            // A category whose row vanished from the table carries no
            // signal anymore; drop it so a recreated row counts as changed.
            last.retain(|k, _| current.contains_key(k));
        }
    });
}

/// React to one changed category. Each branch re-reads the DB and applies
/// an idempotent diff to memory. Errors propagate to the caller, which
/// withholds the baseline — the retry happens on the next poll, not on
/// the next bump of the category.
async fn dispatch(category: &str, state: &AppState, pool: &PgPool) -> anyhow::Result<()> {
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
            crate::config_reconcile::reconcile_plans(&deps).await?;
        }
        boom_state::CATEGORY_ASSIGNMENT => {
            state.plan_store.reconcile_assignments_from_db(pool).await?;
        }
        boom_state::CATEGORY_BLOCKLIST => {
            crate::config_reconcile::reconcile_block_rules(&deps).await?;
            let config = state.inner.load().config.clone();
            crate::config_reconcile::adopt_blocklist_enabled_db(
                pool,
                &state.block_rule_store,
                &config,
            )
            .await?;
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
            let shared = boom_dashboard::config_kv::get_bool(
                pool,
                boom_dashboard::config_kv::DECISION_BASE_SHARED,
            )
            .await?
            .unwrap_or(false);
            state.limiter.set_decision_shared(shared);
            tracing::info!(shared, "quota decision base switched (revision bump)");
        }
        boom_state::CATEGORY_ROUTING => {
            // Peer model/alias/workflow/cost write: apply the same diff the
            // reload path runs. Idempotent, never clears a store.
            crate::config_reconcile::reconcile_routing_config(&deps).await?;
        }
        boom_state::CATEGORY_USER_TAG => {
            let config = state.inner.load().config.clone();
            crate::config_reconcile::adopt_user_tag_headers_db(
                pool,
                &state.user_tag_store,
                &config,
            )
            .await?;
        }
        boom_state::CATEGORY_TRACE => {
            let enabled = boom_dashboard::config_kv::get_bool(
                pool,
                boom_dashboard::config_kv::TRACE_METRICS_ENABLED,
            )
            .await?
            .unwrap_or(false);
            state.trace.set_metrics_enabled(enabled);
            tracing::info!(enabled, "trace metrics switch re-adopted (revision bump)");
        }
        other => {
            // warn, not debug: a bumped category without a dispatch arm means
            // someone added a write-side bump but forgot the reconcile —
            // propagation is silently broken and must be discoverable.
            // Ok, not Err: there is no reconcile action that could succeed
            // later, so withholding the baseline would only spam the warn.
            tracing::warn!(category = other, "revision bump for unknown category — no reconcile arm, propagation skipped");
        }
    }
    Ok(())
}
