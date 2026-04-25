use boom_core::provider::Provider;
use boom_dashboard::state::AdminCommand;
use boom_flowcontrol::{FlowControlConfig, FlowController};
use boom_routing::DeploymentStore;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::state::AppState;

/// Background task: receives AdminCommand from dashboard and executes writes.
/// Has access to AppState (db_pool, deployment_store, boom-provider, boom-config).
pub async fn admin_command_handler(mut rx: tokio::sync::mpsc::Receiver<AdminCommand>, state: AppState) {
    tracing::info!("Admin command handler started");
    while let Some(cmd) = rx.recv().await {
        match cmd {
            AdminCommand::CreateModel { req, reply } => {
                let result = handle_create_model(&state, req).await;
                let _ = reply.send(result);
                state.dump_config_snapshot().await;
            }
            AdminCommand::UpdateModel { id, req, reply } => {
                let result = handle_update_model(&state, id, req).await;
                let _ = reply.send(result);
                state.dump_config_snapshot().await;
            }
            AdminCommand::DeleteModel { id, reply } => {
                let result = handle_delete_model(&state, id).await;
                let _ = reply.send(result);
                state.dump_config_snapshot().await;
            }
            AdminCommand::ConfigChanged => {
                state.dump_config_snapshot().await;
            }
            AdminCommand::ReloadConfig { reply } => {
                match state.reload().await {
                    Ok(summary) => {
                        tracing::info!("Config hot-reloaded via dashboard: {}", summary);
                        let _ = reply.send(Ok(summary));
                    }
                    Err(e) => {
                        tracing::error!("Config hot-reload failed: {}", e);
                        let _ = reply.send(Err(format!("Reload failed: {}", e)));
                    }
                }
            }
        }
    }
    tracing::warn!("Admin command handler stopped (channel closed)");
}

async fn handle_create_model(
    state: &AppState,
    req: boom_dashboard::handlers_admin::CreateDeploymentRequest,
) -> Result<Value, String> {
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;
    let headers_json = serde_json::to_value(&req.headers).unwrap_or(json!({}));

    let input = boom_routing::DeploymentInput {
        model_name: req.model_name.clone(),
        litellm_model: req.litellm_model.clone(),
        api_key: req.api_key.clone(),
        api_key_env: req.api_key_env,
        api_base: req.api_base.clone(),
        api_version: req.api_version.clone(),
        aws_region_name: req.aws_region_name.clone(),
        aws_access_key_id: req.aws_access_key_id.clone(),
        aws_secret_access_key: req.aws_secret_access_key.clone(),
        rpm: req.rpm,
        tpm: req.tpm,
        timeout: req.timeout,
        headers: headers_json,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        enabled: req.enabled,
        deployment_id: req.deployment_id.clone(),
        quota_count_ratio: req.quota_count_ratio.unwrap_or(1),
        max_inflight_queue_len: req.max_inflight_queue_len,
        max_context_len: req.max_context_len,
    };

    let id = DeploymentStore::create_db(db_pool, &input)
        .await
        .map_err(|e| format!("DB insert failed: {}", e))?;

    // Build provider and add to memory (if enabled).
    if req.enabled {
        if let Some(provider) = build_provider(&req) {
            state.deployment_store.add_deployment(&req.model_name, provider);
            let ratio = req.quota_count_ratio.unwrap_or(1) as u64;
            state.deployment_store.set_quota_ratio(&req.model_name, ratio);
            tracing::info!(model = %req.model_name, "Model deployment created and loaded");
        }
    }

    // Sync flow control config.
    sync_flow_control(&state.flow_controller, &req.deployment_id, req.max_inflight_queue_len, req.max_context_len);

    Ok(json!({"ok": true, "id": id, "model_name": req.model_name}))
}

async fn handle_update_model(
    state: &AppState,
    id: Uuid,
    req: boom_dashboard::handlers_admin::CreateDeploymentRequest,
) -> Result<Value, String> {
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;
    let headers_json = serde_json::to_value(&req.headers).unwrap_or(json!({}));

    let input = boom_routing::DeploymentInput {
        model_name: req.model_name.clone(),
        litellm_model: req.litellm_model.clone(),
        api_key: req.api_key.clone(),
        api_key_env: req.api_key_env,
        api_base: req.api_base.clone(),
        api_version: req.api_version.clone(),
        aws_region_name: req.aws_region_name.clone(),
        aws_access_key_id: req.aws_access_key_id.clone(),
        aws_secret_access_key: req.aws_secret_access_key.clone(),
        rpm: req.rpm,
        tpm: req.tpm,
        timeout: req.timeout,
        headers: headers_json,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        enabled: req.enabled,
        deployment_id: req.deployment_id.clone(),
        quota_count_ratio: req.quota_count_ratio.unwrap_or(1),
        max_inflight_queue_len: req.max_inflight_queue_len,
        max_context_len: req.max_context_len,
    };

    let updated = DeploymentStore::update_db(db_pool, id, &input)
        .await
        .map_err(|e| format!("DB update failed: {}", e))?;

    if !updated {
        return Err("Model deployment not found".to_string());
    }

    // Rebuild provider list for this model from DB (handles enable/disable/rename).
    reload_model_deployments(db_pool, &state.deployment_store, &req.model_name).await;

    if req.enabled {
        let ratio = req.quota_count_ratio.unwrap_or(1) as u64;
        state.deployment_store.set_quota_ratio(&req.model_name, ratio);
    }

    // Sync flow control config.
    sync_flow_control(&state.flow_controller, &req.deployment_id, req.max_inflight_queue_len, req.max_context_len);

    Ok(json!({"ok": true}))
}

async fn handle_delete_model(
    state: &AppState,
    id: Uuid,
) -> Result<Value, String> {
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;

    let info = DeploymentStore::delete_db(db_pool, id)
        .await
        .map_err(|e| format!("DB delete failed: {}", e))?;

    let (model_name, old_deployment_id) = match info {
        Some(t) => t,
        None => return Err("Model deployment not found".to_string()),
    };

    // Reload deployments for this model_name from DB to keep memory in sync.
    reload_model_deployments(db_pool, &state.deployment_store, &model_name).await;

    // Remove flow control slot (in-flight requests drain naturally).
    if let Some(did) = old_deployment_id {
        state.flow_controller.remove_slot(&did);
    }

    tracing::info!(model = %model_name, "Model deployment deleted");
    Ok(json!({"ok": true, "model_name": model_name}))
}

/// Reload all deployments for a specific model_name from DB into the deployment store.
pub async fn reload_model_deployments(
    pool: &sqlx::PgPool,
    deployment_store: &Arc<DeploymentStore>,
    model_name: &str,
) {
    let rows = match DeploymentStore::load_model_rows(pool, model_name).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to reload deployments for '{}': {}", model_name, e);
            return;
        }
    };

    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
    for row in &rows {
        if let Some(p) = build_provider_from_row(row) {
            providers.push(p);
        }
    }

    // Always set (even empty) so resolve_candidates can distinguish
    // "configured but all down" from "never configured". An empty provider
    // list prevents silent fallthrough to the wildcard catch-all.
    deployment_store.set_deployments(model_name.to_string(), providers);
}

/// Auto-disable a faulty deployment: mark `enabled = false, auto_disabled = true` in DB,
/// then reload the deployment store so the node is immediately excluded from routing.
/// Uses the actual model_name from the DB record (not the requested model name)
/// so that wildcard `*` deployments are correctly reloaded.
pub async fn auto_disable_deployment(
    pool: &sqlx::PgPool,
    deployment_store: &Arc<DeploymentStore>,
    deployment_id: &str,
    _requested_model: &str,
) {
    tracing::warn!(
        deployment_id = %deployment_id,
        "Auto-disabling deployment due to consecutive failures"
    );

    let actual_model_name = match DeploymentStore::auto_disable_db(pool, deployment_id).await {
        Ok(Some(name)) => name,
        Ok(None) => {
            tracing::warn!(deployment_id = %deployment_id, "No rows updated — deployment_id may not exist in DB");
            return;
        }
        Err(e) => {
            tracing::error!(deployment_id = %deployment_id, "Failed to auto-disable deployment in DB: {}", e);
            return;
        }
    };

    // Reload deployments for the ACTUAL model_name from DB (removes the disabled one from memory).
    reload_model_deployments(pool, deployment_store, &actual_model_name).await;

    tracing::warn!(
        deployment_id = %deployment_id,
        model = %actual_model_name,
        "Deployment auto-disabled and removed from routing"
    );
}

/// Build a Provider from a DB deployment row (from DeploymentStore::load_model_rows).
fn build_provider_from_row(row: &boom_routing::DeploymentProviderRow) -> Option<Arc<dyn Provider>> {
    let mut extra = HashMap::new();
    if let Some(obj) = row.headers.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                extra.insert(k.clone(), s.to_string());
            }
        }
    }
    if let Some(ref v) = row.api_version {
        extra.insert("api_version".to_string(), v.clone());
    }
    if let Some(ref r) = row.aws_region_name {
        extra.insert("aws_region_name".to_string(), r.clone());
    }

    let api_key = row.api_key.as_ref().map(|k| {
        if row.api_key_env.unwrap_or(false) {
            boom_config::resolve_env_value(k)
        } else {
            k.clone()
        }
    });

    match boom_provider::create_provider(
        &row.litellm_model,
        api_key,
        row.api_base.clone(),
        row.timeout as u64,
        &extra,
        row.deployment_id.clone(),
    ) {
        Ok(provider) => Some(provider),
        Err(e) => {
            tracing::error!("Failed to build provider for '{}': {}", row.model_name, e);
            None
        }
    }
}

/// Build a Provider from a CreateDeploymentRequest (dashboard API).
fn build_provider(req: &boom_dashboard::handlers_admin::CreateDeploymentRequest) -> Option<Arc<dyn Provider>> {
    let mut extra = req.headers.clone();
    if let Some(ref v) = req.api_version {
        extra.insert("api_version".to_string(), v.clone());
    }
    if let Some(ref r) = req.aws_region_name {
        extra.insert("aws_region_name".to_string(), r.clone());
    }

    // Resolve api_key (may be env reference).
    let api_key = req.api_key.as_ref().map(|k| {
        if req.api_key_env.unwrap_or(false) {
            boom_config::resolve_env_value(k)
        } else {
            k.clone()
        }
    });

    match boom_provider::create_provider(
        &req.litellm_model,
        api_key,
        req.api_base.clone(),
        req.timeout as u64,
        &extra,
        req.deployment_id.clone(),
    ) {
        Ok(provider) => Some(provider),
        Err(e) => {
            tracing::error!("Failed to build provider for '{}': {}", req.model_name, e);
            None
        }
    }
}

/// Sync flow control config for a deployment.
fn sync_flow_control(
    flow_controller: &Arc<FlowController>,
    deployment_id: &Option<String>,
    max_inflight: Option<i32>,
    max_context: Option<i64>,
) {
    if let Some(ref did) = deployment_id {
        let cfg = FlowControlConfig {
            max_inflight: max_inflight.unwrap_or(0) as u32,
            max_context: max_context.unwrap_or(0) as u64,
        };
        flow_controller.ensure_slot(did, &cfg);
    }
}
