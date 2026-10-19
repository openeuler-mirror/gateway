use boom_dashboard::state::AdminCommand;
use boom_routing::DeploymentStore;
use serde_json::{json, Value};
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
            }
            AdminCommand::UpdateModel { id, req, reply } => {
                let result = handle_update_model(&state, id, req).await;
                let _ = reply.send(result);
            }
            AdminCommand::DeleteModel { id, reply } => {
                let result = handle_delete_model(&state, id).await;
                let _ = reply.send(result);
            }
            AdminCommand::UpsertWorkflow { req, reply } => {
                let result = handle_upsert_workflow(&state, req).await;
                let _ = reply.send(result);
            }
            AdminCommand::DeleteWorkflow { name, reply } => {
                let result = handle_delete_workflow(&state, name).await;
                let _ = reply.send(result);
            }
            AdminCommand::UpsertWorkflowModel { req, reply } => {
                let result = handle_upsert_workflow_model(&state, req).await;
                let _ = reply.send(result);
            }
            AdminCommand::DeleteWorkflowModel { model_name, reply } => {
                let result = handle_delete_workflow_model(&state, model_name).await;
                let _ = reply.send(result);
            }
            AdminCommand::UpsertCostTemplate { req, reply } => {
                let result = handle_upsert_cost_template(&state, req).await;
                let _ = reply.send(result);
            }
            AdminCommand::DeleteCostTemplate { name, reply } => {
                let result = handle_delete_cost_template(&state, name).await;
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
                // Serialize + drop the inner guard BEFORE the DB await —
                // holding an ArcSwap guard across .await pins the old config
                // Arc for the whole query.
                let mut json = {
                    let inner = state.inner.load();
                    serde_json::to_value(&inner.config)
                        .map_err(|e| format!("Serialize config: {}", e))
                };
                if let Ok(ref mut v) = json {
                    // DB mode: overlay the DB-owned runtime sections (models,
                    // aliases, plans, blocklist switch+rules) on top of the
                    // process-level config. The YAML runtime sections are
                    // inert — showing them would be showing stale state.
                    if let Some(ref pool) = state.db_pool {
                        match crate::state::build_config_snapshot_value(pool).await {
                            Ok(snapshot) => {
                                overlay_db_owned_sections(v, &snapshot);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "GetConfig DB snapshot overlay failed: {} \
                                     (falling back to process config only)",
                                    e
                                );
                            }
                        }
                    }
                    boom_config::mask_secrets_in_place(v);
                }
                let _ = reply.send(json);
            }
            AdminCommand::GetConfigSchema { reply } => {
                let schema = json!({
                    "model_deployments": boom_config::manifest::model_deployment_fields(),
                    "general_settings": boom_config::manifest::general_settings_fields(),
                    "router_settings": boom_config::manifest::router_settings_fields(),
                    "cost_templates": boom_config::manifest::cost_template_fields(),
                    "workflows": boom_config::manifest::workflow_fields(),
                });
                let _ = reply.send(Ok(schema));
            }
            AdminCommand::ExportConfig { reply } => {
                let result = state.export_config_yaml().await;
                if let Err(ref e) = result {
                    tracing::warn!("Config export failed: {}", e);
                }
                let _ = reply.send(result);
            }
            AdminCommand::WriteBackConfig { reply } => {
                let result = state.persist_config_in_place().await.map(|path| {
                    if state.config_path.is_some() {
                        format!("Config written back to {}", path)
                    } else {
                        format!(
                            "Config generated at {} (mode 0600) — contains plaintext \
                             secrets; restart with this file to adopt it as the boot config",
                            path
                        )
                    }
                });
                if let Err(ref e) = result {
                    tracing::warn!("Config write-back failed: {}", e);
                }
                let _ = reply.send(result);
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
            AdminCommand::InvalidateAuthCache { token_hashes } => {
                // Take the Arc out of the ArcSwap guard before awaiting so the
                // guard is never held across an await point. load_full() gets
                // the *current* authenticator — if a reload races us and swaps
                // in a fresh one, its cache starts empty and the invalidation
                // is moot, so no lockstep is needed.
                let auth = state.inner.load_full().auth.clone();
                for hash in &token_hashes {
                    auth.invalidate_key(hash).await;
                }
                if token_hashes.len() > 1 {
                    tracing::debug!("Invalidated auth cache for {} token(s)", token_hashes.len());
                }
            }
        }
    }
    tracing::warn!("Admin command handler stopped (channel closed)");
}

/// Overlay the DB-owned runtime sections from a `build_config_snapshot_value`
/// snapshot onto the serialized live config (GetConfig path). The snapshot
/// keys replace their YAML counterparts wholesale — the DB is the authority;
/// process-level sections (server, router policy, prompt log, ...) are kept
/// from the live config.
fn overlay_db_owned_sections(config: &mut Value, snapshot: &Value) {
    let Some(cfg_obj) = config.as_object_mut() else { return };
    let Some(snap_obj) = snapshot.as_object() else { return };

    for key in [
        "model_list",
        "plan_settings",
        "client_blocklist",
        "workflow_settings",
        "cost_templates",
    ] {
        if let Some(v) = snap_obj.get(key) {
            cfg_obj.insert(key.to_string(), v.clone());
        }
    }
    // Aliases live under router_settings.model_group_alias — merge at the
    // sub-key so the rest of router_settings (process-level) stays intact.
    if let Some(aliases) = snapshot
        .get("router_settings")
        .and_then(|r| r.get("model_group_alias"))
    {
        let router = cfg_obj
            .entry("router_settings")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(router_obj) = router.as_object_mut() {
            router_obj.insert("model_group_alias".to_string(), aliases.clone());
        }
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

    let old_model_name = DeploymentStore::update_db(db_pool, id, &input)
        .await
        .map_err(|e| format!("DB update failed: {}", e))?;

    let old_model_name = match old_model_name {
        Some(name) => name,
        None => return Err("Model deployment not found".to_string()),
    };

    // Reload the affected model + wildcard from DB so the updated deployment
    // (new api_base/api_key/flow-control limits/serve_not_match toggle) takes
    // effect immediately. YAML persistence is best-effort in the dispatcher.
    reload_model_deployments(state, &req.model_name).await;

    // Rename: converge the OLD model group too. Without this the old key
    // keeps its stale providers — the old name stays visible in /v1/models,
    // keeps routing, and stats reverse-lookup flips nondeterministically
    // between old and new names. Reload (not blind removal) keeps any other
    // deployments still configured under the old name.
    if old_model_name != req.model_name {
        reload_model_deployments(state, &old_model_name).await;
    }

    Ok(json!({"ok": true}))
}

fn ensure_not_workflow_model(state: &AppState, model_name: &str) -> Result<(), String> {
    if state.workflow_store.contains_model(model_name) {
        return Err(format!(
            "model '{}' is reserved by a workflow virtual model",
            model_name
        ));
    }
    Ok(())
}

/// Reject deleting a deployment name (or an alias target's model) still
/// referenced by a workflow definition — the panel/aggregator would silently
/// break at request time. Names the referencing workflow(s) so the operator
/// knows what to fix first.
fn ensure_not_workflow_child(state: &AppState, model_name: &str) -> Result<(), String> {
    let referencing = state.workflow_store.workflows_referencing(model_name);
    if referencing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "model '{}' is referenced by workflow(s) [{}] — remove or edit the workflow(s) first",
            model_name,
            referencing.join(", ")
        ))
    }
}

async fn handle_delete_model(
    state: &AppState,
    id: Uuid,
) -> Result<Value, String> {
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;

    // Pre-check against the live workflow store (cheaper than post-delete
    // repair: the row is gone from the DB the moment delete_db commits).
    // Race window between check and delete is accepted — the workflow would
    // degrade to a request-time child-routing error, never a boot failure.
    match DeploymentStore::peek_model_name(db_pool, id).await {
        Ok(Some(model_name)) => ensure_not_workflow_child(state, &model_name)?,
        Ok(None) => {}
        Err(e) => tracing::warn!("workflow child pre-check failed (proceeding): {}", e),
    }

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
        // Drop the deleted deployment's rate series too — otherwise its
        // tracker entry lingers forever as an all-zero ghost chart card.
        state.request_rate.remove(did);
    } else {
        // No deployment_id on the deleted row — fall back to a full reload of
        // the affected model + wildcard so the store stays consistent.
        reload_model_deployments(state, &model_name).await;
    }

    tracing::info!(model = %model_name, "Model deployment deleted");
    Ok(json!({"ok": true, "model_name": model_name}))
}

/// Reload all deployments for a specific model_name from DB into the live
/// stores — the CRUD-path entry point. Delegates the per-model apply to
/// `config_reconcile::apply_model_rows_from_db` (shared with the
/// boot/reload reconciler so both stay behaviourally identical), then
/// refreshes the "*" wildcard key so serve_not_match changes take effect.
pub async fn reload_model_deployments(state: &AppState, model_name: &str) {
    let Some(pool) = state.db_pool.as_ref() else {
        tracing::error!(model = model_name, "reload_model_deployments: no DB pool available");
        return;
    };

    crate::config_reconcile::apply_model_rows_from_db(
        pool,
        model_name,
        &state.deployment_store,
        &state.flow_controller,
        &state.cost_template_store,
    )
    .await;

    // Reload the "*" wildcard key whenever the affected model contributes
    // catch-all providers, so auto-disable/enable and CRUD stay consistent
    // (serve_not_match deployments register under both their real model_name
    // and "*" — see build_deployments_from_config). Skipping this leaves
    // stale Arc<dyn Provider> entries in "*" that route to deployments
    // already removed from their owning model.
    crate::config_reconcile::reload_wildcard_deployments(pool, &state.deployment_store).await;
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

// ═══════════════════════════════════════════════════════════
// Workflow + cost template CRUD (DB-authoritative, M4)
// ═══════════════════════════════════════════════════════════

/// Validate a (possibly modified) workflow settings snapshot against the
/// live model universe. Shared by all workflow CRUD handlers: build the
/// scratch settings, run this, only then touch DB + memory + registration.
/// A rejected edit leaves the previous registration routing.
async fn validate_workflow_snapshot(
    state: &AppState,
    scratch: &boom_config::WorkflowSettings,
) -> Result<(), String> {
    let config = state.inner.load_full();
    let namespaces = state.workflow_validation_namespaces(&config.config).await;
    scratch
        .validate_with_namespaces(&namespaces)
        .map_err(|e| e.to_string())
}

/// Re-register fusion virtual models from the (already updated) workflow
/// store. Called by every workflow CRUD handler after its DB write + memory
/// update — the same registration path boot/reload use.
async fn re_register_fusion(state: &AppState) -> Result<(), String> {
    let config = state.inner.load_full();
    state
        .register_fusion_models(&config.config)
        .await
        .map_err(|e| e.to_string())
}

/// Feature-parked gate for the workflow CRUD surface: while
/// `router_settings.enable_workflow` is false (the default), writes are
/// rejected instead of silently no-op'ing — a created binding would never
/// route, so the caller deserves the error.
fn ensure_workflow_enabled(state: &AppState) -> Result<(), String> {
    if state.inner.load_full().config.router_settings.enable_workflow {
        Ok(())
    } else {
        Err(
            "workflow feature is disabled — set router_settings.enable_workflow=true \
             in config.yaml and reload to activate it"
                .to_string(),
        )
    }
}

async fn handle_upsert_workflow(
    state: &AppState,
    req: boom_dashboard::handlers_admin::UpsertWorkflowRequest,
) -> Result<Value, String> {
    ensure_workflow_enabled(state)?;
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;

    let definition: boom_config::WorkflowDefinitionConfig =
        serde_json::from_value(req.definition.clone())
            .map_err(|e| format!("invalid workflow definition: {}", e))?;

    // Scratch settings = current + this upsert; validate before any mutation.
    let mut scratch = state.workflow_store.settings();
    scratch
        .workflows
        .insert(req.name.clone(), definition.clone());
    validate_workflow_snapshot(state, &scratch).await?;

    state
        .workflow_store
        .upsert_definition_db(db_pool, &req.name, &definition)
        .await
        .map_err(|e| format!("DB write failed: {}", e))?;

    re_register_fusion(state).await?;
    tracing::info!(workflow = %req.name, "Workflow definition saved");
    Ok(json!({"ok": true}))
}

async fn handle_delete_workflow(
    state: &AppState,
    name: String,
) -> Result<Value, String> {
    ensure_workflow_enabled(state)?;
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;

    // Reject while bindings reference the definition — deleting it would
    // leave dangling rows that the reconciler then skips with warns.
    let referencing: Vec<String> = state
        .workflow_store
        .settings()
        .models
        .iter()
        .filter(|(_, w)| *w == &name)
        .map(|(m, _)| m.clone())
        .collect();
    if !referencing.is_empty() {
        return Err(format!(
            "workflow '{}' is still bound to model(s) [{}] — delete the bindings first",
            name,
            referencing.join(", ")
        ));
    }

    let existed = state
        .workflow_store
        .delete_definition_db(db_pool, &name)
        .await
        .map_err(|e| format!("DB delete failed: {}", e))?;
    if !existed {
        return Err(format!("workflow '{}' not found", name));
    }

    // No bindings changed → no registration change needed.
    tracing::info!(workflow = %name, "Workflow definition deleted");
    Ok(json!({"ok": true}))
}

async fn handle_upsert_workflow_model(
    state: &AppState,
    req: boom_dashboard::handlers_admin::UpsertWorkflowModelRequest,
) -> Result<Value, String> {
    ensure_workflow_enabled(state)?;
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;

    if !state
        .workflow_store
        .settings()
        .workflows
        .contains_key(&req.workflow_name)
    {
        return Err(format!(
            "unknown workflow '{}' — create the definition first",
            req.workflow_name
        ));
    }

    // Scratch settings = current + this binding; validate before any
    // mutation (catches name conflicts with deployments/aliases).
    let mut scratch = state.workflow_store.settings();
    scratch
        .models
        .insert(req.model_name.clone(), req.workflow_name.clone());
    validate_workflow_snapshot(state, &scratch).await?;

    state
        .workflow_store
        .upsert_model_db(db_pool, &req.model_name, &req.workflow_name)
        .await
        .map_err(|e| format!("DB write failed: {}", e))?;

    re_register_fusion(state).await?;
    tracing::info!(
        model = %req.model_name,
        workflow = %req.workflow_name,
        "Workflow model binding saved"
    );
    Ok(json!({"ok": true}))
}

async fn handle_delete_workflow_model(
    state: &AppState,
    model_name: String,
) -> Result<Value, String> {
    ensure_workflow_enabled(state)?;
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;

    let existed = state
        .workflow_store
        .delete_model_db(db_pool, &model_name)
        .await
        .map_err(|e| format!("DB delete failed: {}", e))?;
    if !existed {
        return Err(format!("workflow model '{}' not found", model_name));
    }

    // Registration teardown removes the exclusive deployment.
    re_register_fusion(state).await?;
    tracing::info!(model = %model_name, "Workflow model binding deleted");
    Ok(json!({"ok": true}))
}

async fn handle_upsert_cost_template(
    state: &AppState,
    req: boom_dashboard::handlers_admin::UpsertCostTemplateRequest,
) -> Result<Value, String> {
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;

    if req.name.trim().is_empty() {
        return Err("cost template name must not be empty".to_string());
    }
    for (field, value) in [
        ("input_cost_per_million_tokens", req.input_cost_per_million_tokens),
        (
            "cached_input_cost_per_million_tokens",
            req.cached_input_cost_per_million_tokens,
        ),
        ("output_cost_per_million_tokens", req.output_cost_per_million_tokens),
    ] {
        if value.is_some_and(|v| !v.is_finite() || v < 0.0) {
            return Err(format!("{} must be a finite non-negative number", field));
        }
    }

    let template = boom_config::CostTemplate {
        name: req.name.clone(),
        input_cost_per_million_tokens: req.input_cost_per_million_tokens,
        cached_input_cost_per_million_tokens: req.cached_input_cost_per_million_tokens,
        output_cost_per_million_tokens: req.output_cost_per_million_tokens,
    };

    state
        .cost_template_store
        .upsert_db(db_pool, &template)
        .await
        .map_err(|e| format!("DB write failed: {}", e))?;

    // Re-derive the cost rate of every model bound to this template so the
    // new rates take effect immediately (same primitive the CRUD model path
    // uses).
    let affected = DeploymentStore::models_using_cost_template(db_pool, &req.name)
        .await
        .unwrap_or_default();
    for model in &affected {
        reload_model_deployments(state, model).await;
    }

    tracing::info!(template = %req.name, models = affected.len(), "Cost template saved");
    Ok(json!({"ok": true, "affected_models": affected}))
}

async fn handle_delete_cost_template(
    state: &AppState,
    name: String,
) -> Result<Value, String> {
    let db_pool = state.db_pool.as_ref().ok_or("Database not available")?;

    // Models keep their model_info.cost_template reference; the lookup just
    // stops resolving (rate clears). Re-rate them so the store drops the
    // stale rate instead of billing at the old price.
    let affected = DeploymentStore::models_using_cost_template(db_pool, &name)
        .await
        .unwrap_or_default();

    let existed = state
        .cost_template_store
        .delete_db(db_pool, &name)
        .await
        .map_err(|e| format!("DB delete failed: {}", e))?;
    if !existed {
        return Err(format!("cost template '{}' not found", name));
    }

    for model in &affected {
        reload_model_deployments(state, model).await;
    }

    tracing::info!(template = %name, models = affected.len(), "Cost template deleted");
    Ok(json!({"ok": true, "affected_models": affected}))
}
