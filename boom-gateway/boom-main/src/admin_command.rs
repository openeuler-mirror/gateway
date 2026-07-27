use boom_core::provider::Provider;
use boom_dashboard::state::AdminCommand;
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
                let mut result = handle_create_model(&state, req).await;
                // Persist BEFORE replying so a failure can be surfaced.
                // DB write has already committed at this point — a persist
                // failure leaves DB ahead of YAML/memory, which the user
                // needs to know about (rather than seeing a fake 200 OK).
                if result.is_ok() {
                    if let Err(e) = state.persist_config_in_place().await {
                        augment_with_warning(&mut result, format!(
                            "DB write succeeded but config did not reload: {}. \
                             Use the Reload button to retry.", e
                        ));
                    }
                }
                let _ = reply.send(result);
            }
            AdminCommand::UpdateModel { id, req, reply } => {
                let mut result = handle_update_model(&state, id, req).await;
                if result.is_ok() {
                    if let Err(e) = state.persist_config_in_place().await {
                        augment_with_warning(&mut result, format!(
                            "DB write succeeded but config did not reload: {}. \
                             Use the Reload button to retry.", e
                        ));
                    }
                }
                let _ = reply.send(result);
            }
            AdminCommand::DeleteModel { id, reply } => {
                let mut result = handle_delete_model(&state, id).await;
                if result.is_ok() {
                    if let Err(e) = state.persist_config_in_place().await {
                        augment_with_warning(&mut result, format!(
                            "DB write succeeded but config did not reload: {}. \
                             Use the Reload button to retry.", e
                        ));
                    }
                }
                let _ = reply.send(result);
            }
            AdminCommand::ConfigChanged => {
                // Fire-and-forget: no reply channel. Just log on failure.
                if let Err(e) = state.persist_config_in_place().await {
                    tracing::error!(
                        "ConfigChanged persist failed (no reply channel to surface): {}",
                        e
                    );
                }
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
            AdminCommand::UpdateConfigSection { path, value, reply } => {
                let result = state.update_config_section(&path, value).await;
                let _ = reply.send(result);
            }
            AdminCommand::GetConfig { reply } => {
                let inner = state.inner.load();
                let mut json = serde_json::to_value(&inner.config)
                    .map_err(|e| format!("Serialize config: {}", e));
                if let Ok(ref mut v) = json {
                    boom_config::mask_secrets_in_place(v);
                }
                let _ = reply.send(json);
            }
            AdminCommand::GetConfigSchema { reply } => {
                let schema = json!({
                    "model_deployments": boom_config::manifest::model_deployment_fields(),
                    "general_settings": boom_config::manifest::general_settings_fields(),
                    "router_settings": boom_config::manifest::router_settings_fields(),
                });
                let _ = reply.send(Ok(schema));
            }
        }
    }
    tracing::warn!("Admin command handler stopped (channel closed)");
}

/// Attach a `warning` field to a successful Result<Value, String> so the
/// frontend can distinguish "fully applied" from "DB-only applied, reload
/// pending". The Result stays Ok because the user's primary intent (DB
/// write) succeeded; the warning surfaces the secondary failure.
fn augment_with_warning(result: &mut Result<Value, String>, warning: String) {
    if let Ok(json) = result {
        if let Some(obj) = json.as_object_mut() {
            obj.insert(
                "warning".into(),
                serde_json::Value::String(warning),
            );
        }
    }
}

async fn handle_create_model(
    state: &AppState,
    req: boom_dashboard::handlers_admin::CreateDeploymentRequest,
) -> Result<Value, String> {
    ensure_not_workflow_model(state, &req.model_name)?;
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
        client_type_header: req.client_type_header,
        serve_not_match: req.serve_not_match,
        model_info: req.model_info.clone(),
    };

    let id = DeploymentStore::create_db(db_pool, &input)
        .await
        .map_err(|e| format!("DB insert failed: {}", e))?;

    // Memory rebuild is handled by persist_config_in_place → reload, which
    // walks the YAML model_list via build_deployments_from_config. Doing it
    // manually here was both redundant (reload wipes + rebuilds the store)
    // and lossy (the manual path skipped serve_not_match wildcard registration
    // that the YAML path handles correctly).

    Ok(json!({"ok": true, "id": id, "model_name": req.model_name}))
}

async fn handle_update_model(
    state: &AppState,
    id: Uuid,
    req: boom_dashboard::handlers_admin::CreateDeploymentRequest,
) -> Result<Value, String> {
    ensure_not_workflow_model(state, &req.model_name)?;
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
        client_type_header: req.client_type_header,
        serve_not_match: req.serve_not_match,
        model_info: req.model_info.clone(),
    };

    let updated = DeploymentStore::update_db(db_pool, id, &input)
        .await
        .map_err(|e| format!("DB update failed: {}", e))?;

    if !updated {
        return Err("Model deployment not found".to_string());
    }

    // Memory rebuild deferred to persist_config_in_place → reload (called by
    // the admin command dispatcher after this handler returns). Doing it
    // manually here would be wiped and redone by reload anyway.

    Ok(json!({"ok": true}))
}

fn ensure_not_workflow_model(state: &AppState, model_name: &str) -> Result<(), String> {
    if state
        .inner
        .load()
        .config
        .workflow_settings
        .models
        .contains_key(model_name)
    {
        return Err(format!(
            "model '{}' is reserved by workflow_settings",
            model_name
        ));
    }
    Ok(())
}

async fn handle_delete_model(
    state: &AppState,
    id: Uuid,
) -> Result<Value, String> {
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;

    let info = DeploymentStore::delete_db(db_pool, id)
        .await
        .map_err(|e| format!("DB delete failed: {}", e))?;

    let (model_name, _old_deployment_id) = match info {
        Some(t) => t,
        None => return Err("Model deployment not found".to_string()),
    };

    // Memory rebuild + orphan flow-control slot cleanup deferred to
    // persist_config_in_place → reload. seed_flow_controller_from_config
    // walks the post-edit YAML and calls retain_slots(active_ids), which
    // removes any slot whose deployment_id is no longer present.

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
    if !deployment_store.set_deployments(model_name.to_string(), providers) {
        tracing::error!(
            model = model_name,
            "refused to reload deployments for an exclusive model"
        );
    }
}

/// Auto-disable a faulty deployment: mark `enabled = false, auto_disabled = true` in DB,
/// then reload the deployment store so the node is immediately excluded from routing.
/// Uses the actual model_name from the DB record (not the requested model name)
/// so that wildcard `*` deployments are correctly reloaded.
pub async fn auto_disable_deployment(
    pool: &sqlx::PgPool,
    deployment_store: &Arc<DeploymentStore>,
    deployment_id: &str,
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

/// Auto-enable a deployment that was previously auto-disabled, then reload routing.
pub async fn auto_enable_deployment(
    pool: &sqlx::PgPool,
    deployment_store: &Arc<DeploymentStore>,
    deployment_id: &str,
) {
    tracing::info!(
        deployment_id = %deployment_id,
        "Auto-enabling deployment after successful recovery checks"
    );

    let actual_model_name = match DeploymentStore::auto_enable_db(pool, deployment_id).await {
        Ok(Some(name)) => name,
        Ok(None) => {
            tracing::warn!(deployment_id = %deployment_id, "No rows updated — deployment may not exist or was not auto-disabled");
            return;
        }
        Err(e) => {
            tracing::error!(deployment_id = %deployment_id, "Failed to auto-enable deployment in DB: {}", e);
            return;
        }
    };

    reload_model_deployments(pool, deployment_store, &actual_model_name).await;

    tracing::info!(
        deployment_id = %deployment_id,
        model = %actual_model_name,
        "Deployment auto-enabled and restored to routing"
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
        row.client_type_header.unwrap_or(false),
    ) {
        Ok(provider) => Some(provider),
        Err(e) => {
            tracing::error!("Failed to build provider for '{}': {}", row.model_name, e);
            None
        }
    }
}
