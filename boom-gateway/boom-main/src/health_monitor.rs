use crate::state::AppState;
use boom_routing::DeploymentHealthTarget;
use dashmap::DashMap;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct DeploymentHealthCounters {
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub last_checked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Default)]
pub struct DeploymentHealthStore {
    counters: DashMap<String, DeploymentHealthCounters>,
}

impl DeploymentHealthStore {
    pub fn new() -> Self { Self::default() }

    fn record_online_success(&self, deployment_id: &str) {
        self.counters
            .entry(deployment_id.to_string())
            .and_modify(|state| {
                state.consecutive_failures = 0;
                state.consecutive_successes = 0;
                state.last_checked_at = Some(chrono::Utc::now());
                state.last_error = None;
            })
            .or_insert_with(|| DeploymentHealthCounters {
                last_checked_at: Some(chrono::Utc::now()),
                ..Default::default()
            });
    }

    fn record_online_failure(&self, deployment_id: &str, error: String) -> u32 {
        let mut entry = self.counters.entry(deployment_id.to_string()).or_default();
        entry.consecutive_failures += 1;
        entry.consecutive_successes = 0;
        entry.last_checked_at = Some(chrono::Utc::now());
        entry.last_error = Some(error);
        entry.consecutive_failures
    }

    fn record_recovery_success(&self, deployment_id: &str) -> u32 {
        let mut entry = self.counters.entry(deployment_id.to_string()).or_default();
        entry.consecutive_successes += 1;
        entry.consecutive_failures = 0;
        entry.last_checked_at = Some(chrono::Utc::now());
        entry.last_error = None;
        entry.consecutive_successes
    }

    fn record_recovery_failure(&self, deployment_id: &str, error: String) {
        let mut entry = self.counters.entry(deployment_id.to_string()).or_default();
        entry.consecutive_successes = 0;
        entry.consecutive_failures += 1;
        entry.last_checked_at = Some(chrono::Utc::now());
        entry.last_error = Some(error);
    }

    fn clear(&self, deployment_id: &str) { self.counters.remove(deployment_id); }
}

pub fn spawn_deployment_health_monitor(
    state: AppState,
    shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    if state.db_pool.is_none() {
        tracing::info!("Deployment health monitor disabled: database is not configured");
        return;
    }

    let offline_shutdown = shutdown.resubscribe();
    tokio::spawn(run_offline_checker(state.clone(), offline_shutdown));
    tokio::spawn(run_recovery_checker(state, shutdown));
}

async fn run_offline_checker(state: AppState, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    let client = health_client();
    loop {
        let sleep_for = {
            let inner = state.inner.load();
            Duration::from_secs(inner.config.deployment_health_check.offline_check_interval_secs.max(1))
        };

        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => {}
            _ = shutdown.recv() => break,
        }

        let cfg = {
            let inner = state.inner.load();
            inner.config.deployment_health_check.clone()
        };
        if !cfg.auto_offline_enabled {
            continue;
        }

        let Some(pool) = state.db_pool.as_ref() else { continue; };

        let targets = match boom_routing::DeploymentStore::list_health_check_targets(pool).await {
            Ok(targets) => targets,
            Err(e) => {
                tracing::error!("Failed to load deployment health targets: {}", e);
                continue;
            }
        };

        for target in targets.into_iter().filter(|t| t.enabled.unwrap_or(true)) {
            let Some(url) = health_url(&target, &cfg.path) else {
                tracing::debug!(deployment_id = %target.deployment_id, "Skipping health check target without api_base");
                continue;
            };

            match probe(&client, &url).await {
                Ok(()) => state.deployment_health.record_online_success(&target.deployment_id),
                Err(e) => {
                    let count = state.deployment_health.record_online_failure(&target.deployment_id, e.clone());
                    tracing::warn!(
                        deployment_id = %target.deployment_id,
                        model = %target.model_name,
                        count,
                        error = %e,
                        "Deployment health check failed"
                    );
                    if count >= cfg.failure_threshold.max(1) {
                        crate::admin_command::auto_disable_deployment(
                            pool,
                            &state.deployment_store,
                            &target.deployment_id,
                            &target.model_name,
                        )
                        .await;
                        state.deployment_health.clear(&target.deployment_id);
                    }
                }
            }
        }
    }

    tracing::info!("Deployment offline checker stopped");
}

async fn run_recovery_checker(state: AppState, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    let client = health_client();
    loop {
        let sleep_for = {
            let inner = state.inner.load();
            Duration::from_secs(inner.config.deployment_health_check.recovery_check_interval_secs.max(1))
        };

        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => {}
            _ = shutdown.recv() => break,
        }

        let cfg = {
            let inner = state.inner.load();
            inner.config.deployment_health_check.clone()
        };
        if !cfg.auto_recovery_enabled {
            continue;
        }

        let Some(pool) = state.db_pool.as_ref() else { continue; };

        let targets = match boom_routing::DeploymentStore::list_health_check_targets(pool).await {
            Ok(targets) => targets,
            Err(e) => {
                tracing::error!("Failed to load deployment health targets: {}", e);
                continue;
            }
        };

        for target in targets.into_iter().filter(|t| !t.enabled.unwrap_or(true) && t.auto_disabled.unwrap_or(false)) {
            let Some(url) = health_url(&target, &cfg.path) else {
                tracing::debug!(deployment_id = %target.deployment_id, "Skipping recovery check target without api_base");
                continue;
            };

            match probe(&client, &url).await {
                Ok(()) => {
                    let count = state.deployment_health.record_recovery_success(&target.deployment_id);
                    tracing::info!(
                        deployment_id = %target.deployment_id,
                        model = %target.model_name,
                        count,
                        "Deployment recovery health check succeeded"
                    );
                    if count >= cfg.recovery_threshold.max(1) {
                        crate::admin_command::auto_enable_deployment(
                            pool,
                            &state.deployment_store,
                            &target.deployment_id,
                        )
                        .await;
                        state.deployment_health.clear(&target.deployment_id);
                    }
                }
                Err(e) => state.deployment_health.record_recovery_failure(&target.deployment_id, e),
            }
        }
    }

    tracing::info!("Deployment recovery checker stopped");
}

fn health_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn health_url(target: &DeploymentHealthTarget, path: &str) -> Option<String> {
    let api_base = target.api_base.as_ref()?.trim_end_matches('/');
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{}", path) };
    Some(format!("{}{}", api_base, path))
}

async fn probe(client: &reqwest::Client, url: &str) -> Result<(), String> {
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("health endpoint returned {}", response.status()))
    }
}
