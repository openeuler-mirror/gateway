use boom_core::provider::Provider;
use boom_dashboard::state::AdminCommand;
use boom_routing::{DeploymentStore, VisibilityState, visibility_from_db};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
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
                // Best-effort YAML sync AFTER DB write + in-memory update have
                // succeeded (store.*_db + reload_model_deployments already ran
                // inside the handler). A YAML write failure does NOT block
                // routing — the deployment is already routable from the
                // in-memory store. Surface the failure as a warning so the
                // operator knows the DB↔YAML split exists; a subsequent reload
                // will reload from the stale YAML and OVERWRITE this DB change
                // (Scenario D — YAML absolutely wins on reload).
                if result.is_ok() {
                    if let Err(e) = state.persist_config_in_place().await {
                        augment_with_warning(&mut result, format!(
                            "DB write succeeded and the deployment is routable, \
                             but YAML sync failed: {e}. The next reload will \
                             rebuild the store from the stale YAML and overwrite \
                             this DB change. Fix the YAML file's write \
                             permissions, then re-apply this change."
                        ));
                    }
                }
                let _ = reply.send(result);
            }
            AdminCommand::UpdateModel { id, req, reply } => {
                let mut result = handle_update_model(&state, id, req).await;
                // Same best-effort YAML sync as CreateModel — see that arm for
                // the Scenario D warning rationale.
                if result.is_ok() {
                    if let Err(e) = state.persist_config_in_place().await {
                        augment_with_warning(&mut result, format!(
                            "DB write succeeded and the deployment is routable, \
                             but YAML sync failed: {e}. The next reload will \
                             rebuild the store from the stale YAML and overwrite \
                             this DB change. Fix the YAML file's write \
                             permissions, then re-apply this change."
                        ));
                    }
                }
                let _ = reply.send(result);
            }
            AdminCommand::DeleteModel { id, reply } => {
                let mut result = handle_delete_model(&state, id).await;
                // Same best-effort YAML sync as CreateModel — see that arm for
                // the Scenario D warning rationale.
                if result.is_ok() {
                    if let Err(e) = state.persist_config_in_place().await {
                        augment_with_warning(&mut result, format!(
                            "DB write succeeded and the deployment is removed \
                             from routing, but YAML sync failed: {e}. The next \
                             reload will rebuild the store from the stale YAML \
                             and re-add this deployment. Fix the YAML file's \
                             write permissions, then re-delete it."
                        ));
                    }
                }
                let _ = reply.send(result);
            }
            AdminCommand::ConfigChanged { reply } => {
                // Best-effort YAML persistence. The caller (alias/plan CRUD)
                // has already updated DB + in-memory state, so a YAML write
                // failure must not block the operation. Surface the result
                // back so the dashboard handler can attach a warning to the
                // HTTP response instead of silently dropping it.
                let result = state.persist_config_in_place().await;
                if let Err(e) = &result {
                    tracing::warn!("ConfigChanged YAML persist failed: {}", e);
                }
                let _ = reply.send(result);
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
            AdminCommand::UpdatePromptLogConfig { config, reply } => {
                // Hot-swap the live prompt-log config. The writer's
                // `Arc<ArcSwap<_>>` makes this observable to the dashboard's
                // `PromptLogQueryApi` handle on the next read.
                state.prompt_log_writer.update_config(config);
                let _ = reply.send(Ok(()));
            }
            AdminCommand::PingOtlpEndpoint { endpoint, headers, timeout_secs, reply } => {
                // Single attempt — no retry. The dashboard polls every 5s and
                // would compound backoff if we retried internally.
                let probe_config = boom_promptlog::OtlpConfig {
                    enabled: true,
                    endpoint,
                    service_name: "boom-gateway".to_string(),
                    service_version: None,
                    timeout_secs,
                    batch_size: 512,
                    flush_interval_secs: 5,
                    max_attribute_bytes: 4096,
                    headers,
                    max_queue_size: 10000,
                };
                let result = boom_promptlog::ping_endpoint(&probe_config).await;
                let _ = reply.send(result);
            }
            AdminCommand::PingTraceOtlpEndpoint { endpoint, headers, timeout_secs, reply } => {
                // Mirror of PingOtlpEndpoint but for the traces channel —
                // sends an empty ExportTraceServiceRequest to validate
                // collector connectivity before saving config. Single attempt,
                // no retry.
                let probe_config = boom_core::OtlpConfig {
                    enabled: true,
                    endpoint,
                    service_name: "boom-gateway".to_string(),
                    service_version: None,
                    timeout_secs,
                    batch_size: 512,
                    flush_interval_secs: 5,
                    max_attribute_bytes: 4096,
                    headers,
                    max_queue_size: 10000,
                };
                let result = boom_trace::ping_endpoint(&probe_config).await;
                let _ = reply.send(result);
            }
            AdminCommand::GetOtlpStatus { reply } => {
                // Read-only snapshot of the live exporter's state machine.
                // The writer's `otlp_status` takes an owned Arc out of the
                // ArcSwap guard before awaiting, so we don't hold the guard
                // across the await.
                let snap = state.prompt_log_writer.otlp_status().await;
                let _ = reply.send(snap);
            }
            AdminCommand::ProbeOtlp { reply } => {
                // Manual probe — drives Offline → Online on success.
                let result = state.prompt_log_writer.probe_otlp().await;
                let _ = reply.send(result);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Warning attaches to a successful JSON-object Result. The Result
    /// stays Ok because the primary DB write succeeded; the warning surfaces
    /// the secondary YAML failure so the frontend can prompt the operator
    /// to fix file permissions.
    #[test]
    fn augment_with_warning_attaches_to_ok_object() {
        let mut result: Result<Value, String> = Ok(json!({"ok": true, "id": "abc"}));
        augment_with_warning(
            &mut result,
            "YAML sync failed: read-only file system".to_string(),
        );
        let v = result.unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["id"], "abc");
        assert_eq!(v["warning"], "YAML sync failed: read-only file system");
    }

    /// Warning does NOT downgrade Ok to Err — that would make the frontend
    /// treat a routable deployment as a failed operation, undoing the whole
    /// point of the best-effort YAML write refactor.
    #[test]
    fn augment_with_warning_keeps_result_ok() {
        let mut result: Result<Value, String> = Ok(json!({"ok": true}));
        augment_with_warning(&mut result, "any warning".to_string());
        assert!(result.is_ok(), "augment_with_warning must not flip Ok → Err");
    }

    /// Warning on Err Result is a no-op — the error string already carries
    /// the primary failure; layering a YAML warning on top would confuse the
    /// operator about which step failed.
    #[test]
    fn augment_with_warning_no_op_on_err() {
        let mut result: Result<Value, String> = Err("DB insert failed".to_string());
        augment_with_warning(&mut result, "YAML also failed".to_string());
        assert_eq!(result.unwrap_err(), "DB insert failed");
    }

    /// Warning on non-object JSON (e.g. `json!(true)`) is a no-op — there's
    /// no object to insert the field into. The Result still stays Ok so the
    /// operation succeeds (the primary write did succeed), but the warning
    /// is lost. This matches the contract: handlers always return JSON
    /// objects, so this branch is defensive only.
    #[test]
    fn augment_with_warning_no_op_on_non_object() {
        let mut result: Result<Value, String> = Ok(json!(true));
        augment_with_warning(&mut result, "warning".to_string());
        let v = result.unwrap();
        assert!(v.get("warning").is_none());
    }
}

/// Parse the request's visibility string into the enum. System boundary —
/// unknown values are rejected rather than silently treated as normal.
fn parse_model_visibility(s: &Option<String>) -> Result<boom_config::ModelVisibility, String> {
    match s.as_deref() {
        None | Some("normal") => Ok(boom_config::ModelVisibility::Normal),
        Some("public") => Ok(boom_config::ModelVisibility::Public),
        Some("private") => Ok(boom_config::ModelVisibility::Private),
        Some(other) => Err(format!(
            "invalid visibility '{}': expected normal|public|private",
            other
        )),
    }
}

async fn handle_create_model(
    state: &AppState,
    req: boom_dashboard::handlers_admin::CreateDeploymentRequest,
) -> Result<Value, String> {
    ensure_not_workflow_model(state, &req.model_name)?;
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;
    let headers_json = serde_json::to_value(&req.headers).unwrap_or(json!({}));
    let visibility = parse_model_visibility(&req.visibility)?;

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
        allowed_teams: if visibility == boom_config::ModelVisibility::Private {
            req.allowed_teams.clone()
        } else {
            None
        },
        visibility,
    };

    let id = DeploymentStore::create_db(db_pool, &input)
        .await
        .map_err(|e| format!("DB insert failed: {}", e))?;

    // Reload the affected model + wildcard from DB into the live store so the
    // new deployment is routable immediately. YAML persistence (handled by
    // persist_config_in_place in the dispatcher) is best-effort and must not
    // block in-memory activation — a read-only YAML file must not cause
    // model_not_found for a deployment whose DB row already committed.
    reload_model_deployments(state, &req.model_name).await;

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
    let visibility = parse_model_visibility(&req.visibility)?;

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
        allowed_teams: if visibility == boom_config::ModelVisibility::Private {
            req.allowed_teams.clone()
        } else {
            None
        },
        visibility,
    };

    let updated = DeploymentStore::update_db(db_pool, id, &input)
        .await
        .map_err(|e| format!("DB update failed: {}", e))?;

    if !updated {
        return Err("Model deployment not found".to_string());
    }

    // Reload the affected model + wildcard from DB so the updated deployment
    // (new api_base/api_key/flow-control limits/serve_not_match toggle) takes
    // effect immediately. YAML persistence is best-effort in the dispatcher.
    reload_model_deployments(state, &req.model_name).await;

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

    let (model_name, old_deployment_id) = match info {
        Some(t) => t,
        None => return Err("Model deployment not found".to_string()),
    };

    // Remove the deleted deployment from the live store + wildcard key, and
    // release its flow-control slot. YAML persistence is best-effort in the
    // dispatcher; these in-memory updates make the deletion take effect
    // immediately even when YAML is read-only.
    if let Some(did) = old_deployment_id.as_deref() {
        state.deployment_store.remove_deployment_by_deployment_id(did);
        state.flow_controller.remove_slot(did);
    } else {
        // No deployment_id on the deleted row — fall back to a full reload of
        // the affected model + wildcard so the store stays consistent.
        reload_model_deployments(state, &model_name).await;
    }

    tracing::info!(model = %model_name, "Model deployment deleted");
    Ok(json!({"ok": true, "model_name": model_name}))
}

/// Reload all deployments for a specific model_name from DB into the live
/// stores — the single apply point for CRUD and auto-disable/enable changes.
///
/// Applies every in-memory consumer of a deployment row, not just the
/// provider list: FlowController slots, per-model quota ratio, and the
/// per-model cost rate. This is the CRUD-path counterpart of the YAML build
/// path (build_deployments_from_config + seed_flow_controller_from_config);
/// the two must stay behaviourally identical, or web edits end up "saved
/// but not effective until manual reload".
pub async fn reload_model_deployments(state: &AppState, model_name: &str) {
    let Some(pool) = state.db_pool.as_ref() else {
        tracing::error!(model = model_name, "reload_model_deployments: no DB pool available");
        return;
    };

    let rows = match DeploymentStore::load_model_rows(pool, model_name).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to reload deployments for '{}': {}", model_name, e);
            return;
        }
    };

    // Snapshot live deployment_ids before the swap so flow-control slots of
    // removed deployments (auto-disable, deployment_id change) get released.
    let old_ids: HashSet<String> = state
        .deployment_store
        .get_providers(model_name)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.deployment_id().map(str::to_string))
        .collect();

    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
    for row in &rows {
        if let Some(p) = build_provider_from_row(row) {
            providers.push(p);
        }
    }

    // Visibility at model_name granularity: the newest non-normal row wins
    // (rows are ordered by created_at) — same convention as the quota-ratio
    // derivation below.
    let vis = rows
        .iter()
        .rev()
        .find_map(|r| match visibility_from_db(&r.visibility, &r.allowed_teams) {
            VisibilityState::Normal => None,
            other => Some(other),
        });
    state
        .deployment_store
        .set_visibility(model_name, vis.unwrap_or(VisibilityState::Normal));

    // Always set (even empty) so resolve_candidates can distinguish
    // "configured but all down" from "never configured". An empty provider
    // list prevents silent fallthrough to the wildcard catch-all.
    if !state.deployment_store.set_deployments(model_name.to_string(), providers) {
        tracing::error!(
            model = model_name,
            "refused to reload deployments for an exclusive model"
        );
    }

    // FlowController slots: update-or-create for rows still present;
    // ensure_slot removes the slot when both limits are 0, so clearing the
    // limits via the web UI is honoured here too.
    let mut new_ids: HashSet<String> = HashSet::new();
    for row in &rows {
        if let Some(did) = row.deployment_id.as_deref() {
            state.flow_controller.ensure_slot(
                did,
                &boom_flowcontrol::FlowControlConfig {
                    max_inflight: row.max_inflight_queue_len.unwrap_or(0).max(0) as u32,
                    max_context: row.max_context_len.unwrap_or(0).max(0) as u64,
                },
            );
            new_ids.insert(did.to_string());
        }
    }
    for gone in old_ids.difference(&new_ids) {
        state.flow_controller.remove_slot(gone);
    }

    // Per-model quota ratio (newest row wins; reset to 1 when no rows remain).
    let ratio = rows
        .last()
        .and_then(|r| r.quota_count_ratio)
        .unwrap_or(1)
        .max(0) as u64;
    state.deployment_store.set_quota_ratio(model_name, ratio);

    // Per-model cost rate from model_info.cost_template — same shared
    // derivation as the YAML build path. Clear any stale rate when no row
    // yields one (set_cost_rate removes the entry on a zero rate).
    let config_guard = state.inner.load_full();
    let mut applied_cost = false;
    for row in &rows {
        let Some(info_value) = row.model_info.as_ref() else { continue };
        let Ok(info) = serde_json::from_value::<boom_config::ModelInfo>(info_value.clone()) else {
            tracing::warn!(model = model_name, "failed to parse model_info JSON on deployment row");
            continue;
        };
        if let Some(rate) = crate::state::cost_rate_from_model_info(&config_guard.config, model_name, &info) {
            state.deployment_store.set_cost_rate(model_name, rate);
            applied_cost = true;
        }
    }
    if !applied_cost {
        state.deployment_store.set_cost_rate(model_name, Default::default());
    }

    // Reload the "*" wildcard key whenever the affected model contributes
    // catch-all providers, so auto-disable/enable and CRUD stay consistent
    // with the YAML-seed path (which registers serve_not_match deployments
    // under both their real model_name and "*" — see build_deployments_from_config).
    // Skipping this leaves stale Arc<dyn Provider> entries in "*" that route
    // to deployments already removed from their owning model.
    reload_wildcard_deployments(pool, &state.deployment_store).await;
}

/// Rebuild the "*" wildcard key from every enabled serve_not_match deployment
/// in DB. Called by `reload_model_deployments` after a per-model reload so the
/// wildcard stays in sync without a full store clear+rebuild.
async fn reload_wildcard_deployments(
    pool: &sqlx::PgPool,
    deployment_store: &Arc<DeploymentStore>,
) {
    let rows = match DeploymentStore::load_wildcard_rows(pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to reload wildcard deployments: {}", e);
            return;
        }
    };
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
    for row in &rows {
        if let Some(p) = build_provider_from_row(row) {
            providers.push(p);
        }
    }
    if !deployment_store.set_deployments("*".to_string(), providers) {
        tracing::error!("refused to reload wildcard '*' deployments (exclusive model conflict)");
    }
}

/// Auto-disable a faulty deployment: mark `enabled = false, auto_disabled = true` in DB,
/// then reload the deployment store so the node is immediately excluded from routing.
/// Uses the actual model_name from the DB record (not the requested model name)
/// so that wildcard `*` deployments are correctly reloaded.
pub async fn auto_disable_deployment(state: &AppState, deployment_id: &str) {
    tracing::warn!(
        deployment_id = %deployment_id,
        "Auto-disabling deployment due to consecutive failures"
    );

    let Some(pool) = state.db_pool.as_ref() else {
        tracing::error!(deployment_id = %deployment_id, "auto_disable_deployment: no DB pool available");
        return;
    };

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

    // Reload deployments for the ACTUAL model_name from DB (removes the disabled
    // one from memory, releases its flow-control slot).
    reload_model_deployments(state, &actual_model_name).await;

    tracing::warn!(
        deployment_id = %deployment_id,
        model = %actual_model_name,
        "Deployment auto-disabled and removed from routing"
    );

    state.alerts.raise_alert(
        boom_core::alert::AlertKind::DeploymentAutoDisabled,
        format!("deployment:{deployment_id}"),
        actual_model_name.clone(),
        format!(
            "Deployment '{deployment_id}' (model {actual_model_name}) was auto-disabled by health monitoring and is out of routing"
        ),
    );
}

/// Auto-enable a deployment that was previously auto-disabled, then reload routing.
pub async fn auto_enable_deployment(state: &AppState, deployment_id: &str) {
    tracing::info!(
        deployment_id = %deployment_id,
        "Auto-enabling deployment after successful recovery checks"
    );

    let Some(pool) = state.db_pool.as_ref() else {
        tracing::error!(deployment_id = %deployment_id, "auto_enable_deployment: no DB pool available");
        return;
    };

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

    reload_model_deployments(state, &actual_model_name).await;

    tracing::info!(
        deployment_id = %deployment_id,
        model = %actual_model_name,
        "Deployment auto-enabled and restored to routing"
    );

    state
        .alerts
        .clear_alert(&format!("deployment:{deployment_id}"));
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
