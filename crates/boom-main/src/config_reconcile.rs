//! DB-authoritative config: seed + incremental reconcile.
//!
//! The DB is the single runtime authority for business config (model
//! deployments, aliases, plans, block rules, workflows, cost templates).
//! This module owns the two mechanisms that keep memory in step with the DB:
//!
//! - [`seed_if_empty`] — boot-time one-shot import of YAML runtime sections
//!   into an empty DB (fresh install), plus a one-time adopt pass for
//!   databases predating the authority flip. A marker row in `boom_config_meta`
//!   makes the seed idempotent: it never runs twice, so deleting all models
//!   and restarting must not resurrect them from YAML.
//! - [`reconcile_db_config`] — incremental diff of DB state into the live
//!   stores. Used at boot (empty memory → full build) and on every reload
//!   (diff, never clear — no routing gap). This is the future NOTIFY
//!   handler body for multi-instance config propagation.
//!
//! YAML remains authoritative for process/host-scoped sections (server,
//! general_settings, prompt_log, trace, hooks, router scheduling params).

use std::collections::HashSet;
use std::sync::Arc;

use boom_config::Config;
use boom_flowcontrol::FlowController;
use boom_gatekeeper::BlockRuleStore;
use boom_limiter::{PlanStore, RateLimitPlan};
use boom_routing::{
    AliasStore, CostTemplateStore, DeploymentStore, DeploymentInput, RequestRateTracker,
    WorkflowStore,
};
use sqlx::PgPool;

// ═══════════════════════════════════════════════════════════
// Seed (boot-time, one-shot, marker-guarded)
// ═══════════════════════════════════════════════════════════

/// Marker key in `boom_config_meta` recording that the seed/adopt pass has
/// run. Presence means "never import YAML runtime sections again".
const SEED_MARKER_KEY: &str = "seed";

/// Advisory lock serializing concurrent seeds (two instances booting against
/// the same fresh DB). The value is arbitrary but must be stable across
/// releases. GaussDB may not support advisory locks — the seed degrades to
/// running without the lock (single-instance boot is naturally serialized).
const SEED_ADVISORY_LOCK_KEY: i64 = 7_243_104_219;

/// Outcome of the boot-time seed check.
#[derive(Debug, Default)]
pub struct SeedOutcome {
    /// True when at least one table was imported from YAML (fresh DB). False
    /// means the DB already carried config (pre-flip database or previously
    /// seeded) — callers use this to decide whether YAML runtime sections
    /// should now be reported as inert.
    pub imported: bool,
}

/// One-shot YAML → DB seed. For each business-config table: if empty, import
/// the YAML section (rows tagged `source='yaml'`); when the plan table is
/// pre-populated (upgrade from the dual-source model), adopt the
/// `default_team_plan` pointer that the old sync never persisted.
///
/// Idempotency: the marker row is written last, inside the same transaction
/// as the imports. A crash mid-seed leaves the marker absent and partially
/// filled tables — the next boot re-runs the per-table empty checks and only
/// fills the gaps.
pub async fn seed_if_empty(pool: &PgPool, config: &Config) -> anyhow::Result<SeedOutcome> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| anyhow::anyhow!("seed: begin transaction: {}", e))?;

    if let Err(e) = sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SEED_ADVISORY_LOCK_KEY)
        .execute(&mut *tx)
        .await
    {
        tracing::warn!(
            "pg_advisory_xact_lock unavailable — seeding without lock (single-instance safe): {}",
            e
        );
    }

    let marker: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM boom_config_meta WHERE key = $1")
            .bind(SEED_MARKER_KEY)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("seed: read marker: {}", e))?;
    if marker.is_some() {
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("seed: commit: {}", e))?;
        return Ok(SeedOutcome { imported: false });
    }

    let mut outcome = SeedOutcome::default();

    // ── Deployments ──
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM boom_model_deployment")
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("seed: count deployments: {}", e))?;
    if count == 0 {
        let inputs = deployment_inputs_from_config(config);
        let n = DeploymentStore::seed_rows_db(&mut tx, &inputs)
            .await
            .map_err(|e| anyhow::anyhow!("seed: deployments: {}", e))?;
        outcome.imported |= n > 0;
    }

    // ── Aliases ──
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM boom_model_alias")
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("seed: count aliases: {}", e))?;
    if count == 0 {
        let aliases: Vec<(String, String, bool)> = config
            .router_settings
            .model_group_alias
            .iter()
            .map(|(alias, cfg)| {
                (
                    alias.clone(),
                    cfg.target_model().to_string(),
                    cfg.is_hidden(),
                )
            })
            .collect();
        let n = AliasStore::seed_rows_db(&mut tx, &aliases)
            .await
            .map_err(|e| anyhow::anyhow!("seed: aliases: {}", e))?;
        outcome.imported |= n > 0;
    }

    // ── Block rules ──
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM boom_client_block_rule")
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("seed: count block rules: {}", e))?;
    if count == 0 {
        let rules = config
            .client_blocklist
            .as_ref()
            .map(|bl| bl.rules.clone())
            .unwrap_or_default();
        let n = BlockRuleStore::seed_rows_db(&mut tx, &rules)
            .await
            .map_err(|e| anyhow::anyhow!("seed: block rules: {}", e))?;
        outcome.imported |= n > 0;
    }

    // ── Plans (+ default pointers) ──
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM boom_rate_limit_plan")
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("seed: count plans: {}", e))?;
    if count == 0 {
        // Reuse the YAML → RateLimitPlan conversion via a scratch store.
        let scratch = Arc::new(PlanStore::new());
        crate::state::load_plans_from_config(&scratch, config);
        let plan_list = scratch.list_plans();
        let plans: Vec<(String, &RateLimitPlan)> = plan_list
            .iter()
            .map(|p| (p.name.clone(), p))
            .collect();
        let n = PlanStore::seed_plans_db(
            &mut tx,
            &plans,
            config.plan_settings.default_plan.as_deref(),
            config.plan_settings.default_team_plan.as_deref(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("seed: plans: {}", e))?;
        outcome.imported |= n > 0;
    } else if let Some(dtp) = config.plan_settings.default_team_plan.as_deref() {
        // Adopt pass for databases from the dual-source era: the old sync
        // persisted is_default (key plans) but default_team_plan lived only
        // in memory + YAML. Backfill the flag exactly once, before the
        // marker lands. is_default needs no adopt — the old sync maintained
        // it on every boot.
        sqlx::query("UPDATE boom_rate_limit_plan SET is_default_team = false WHERE is_default_team")
            .execute(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("seed: adopt default_team_plan: {}", e))?;
        let adopted = sqlx::query(
            "UPDATE boom_rate_limit_plan SET is_default_team = true, updated_at = NOW() WHERE name = $1",
        )
        .bind(dtp)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("seed: adopt default_team_plan: {}", e))?;
        if adopted.rows_affected() == 0 {
            tracing::warn!(
                plan = %dtp,
                "default_team_plan not found in DB — team default will be unset"
            );
        }
    }

    // ── Workflows (definitions + model bindings) ──
    // Skipped while the feature is parked (enable_workflow=false): seeding
    // would write dead data. Note the seed marker below still lands — enabling
    // the feature later on an already-seeded database means adding workflows
    // via CRUD, not re-running the YAML import.
    if config.router_settings.enable_workflow {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM boom_workflow_definition")
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("seed: count workflow definitions: {}", e))?;
        let binding_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM boom_workflow_model")
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("seed: count workflow models: {}", e))?;
        if count == 0 && binding_count == 0 {
            let n = WorkflowStore::seed_rows_db(&mut tx, &config.workflow_settings)
                .await
                .map_err(|e| anyhow::anyhow!("seed: workflows: {}", e))?;
            outcome.imported |= n > 0;
        }
    }

    // ── Cost templates ──
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM boom_cost_template")
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("seed: count cost templates: {}", e))?;
    if count == 0 {
        let n = CostTemplateStore::seed_rows_db(&mut tx, &config.cost_templates)
            .await
            .map_err(|e| anyhow::anyhow!("seed: cost templates: {}", e))?;
        outcome.imported |= n > 0;
    }

    // ── Marker (last, same transaction) ──
    sqlx::query("INSERT INTO boom_config_meta (key, value) VALUES ($1, $2)")
        .bind(SEED_MARKER_KEY)
        .bind(serde_json::json!({ "seeded_by": env!("CARGO_PKG_VERSION") }))
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("seed: write marker: {}", e))?;

    tx.commit()
        .await
        .map_err(|e| anyhow::anyhow!("seed: commit: {}", e))?;

    if outcome.imported {
        tracing::info!("Seeded business config from YAML into fresh DB tables");
    } else {
        tracing::info!("DB already carries business config — seed skipped (marker set)");
    }
    Ok(outcome)
}

// ═══════════════════════════════════════════════════════════
// Reconcile (boot + reload + future NOTIFY handler body)
// ═══════════════════════════════════════════════════════════

/// Stores the reconciler needs. Cost templates reconcile FIRST because
/// deployment cost-rate derivation resolves through the CostTemplateStore.
pub struct ReconcileDeps<'a> {
    pub pool: &'a PgPool,
    pub deployment_store: &'a Arc<DeploymentStore>,
    pub alias_store: &'a Arc<AliasStore>,
    pub plan_store: &'a Arc<PlanStore>,
    pub block_rule_store: &'a Arc<BlockRuleStore>,
    pub flow_controller: &'a Arc<FlowController>,
    pub request_rate: &'a Arc<RequestRateTracker>,
    pub cost_template_store: &'a Arc<CostTemplateStore>,
    pub workflow_store: &'a Arc<WorkflowStore>,
}

/// Incrementally reconcile the live stores with DB state. Never clears a
/// store: every section is a per-key upsert + removal diff, so routing
/// continues on the old state while the diff applies. On a boot (empty
/// stores) this is a full build; on reload it is a diff — the same code path
/// a future LISTEN/NOTIFY handler will run when another instance writes.
pub async fn reconcile_db_config(deps: &ReconcileDeps<'_>) -> anyhow::Result<()> {
    reconcile_cost_templates(deps).await?;
    reconcile_deployments(deps).await?;
    reconcile_aliases(deps).await?;
    reconcile_plans(deps).await?;
    reconcile_block_rules(deps).await?;
    reconcile_workflows(deps).await?;
    Ok(())
}

async fn reconcile_deployments(deps: &ReconcileDeps<'_>) -> anyhow::Result<()> {
    // Distinct names across ALL rows (enabled and disabled): a model whose
    // rows are all disabled must keep an EMPTY provider list ("configured
    // but all down"), not be removed ("never configured" falls through to
    // the '*' catch-all).
    let desired = DeploymentStore::distinct_model_names(deps.pool).await?;
    let current = deps.deployment_store.model_names();

    // Apply each desired model (per-model reload — same primitive the CRUD
    // path uses, so both stay behaviourally identical).
    for name in &desired {
        apply_model_rows_from_db(
            deps.pool,
            name,
            deps.deployment_store,
            deps.flow_controller,
            deps.cost_template_store,
        )
        .await;
    }
    reload_wildcard_deployments(deps.pool, deps.deployment_store).await;

    // Removals: names in memory but gone from the DB. Skip '*' (rebuilt by
    // the wildcard reload above) and exclusive models (workflow fusion
    // providers — re-registered from workflow_settings, never DB-backed).
    let removals = keys_to_remove(&desired, &current);
    for name in removals {
        if name == "*" || deps.deployment_store.is_exclusive_model(&name) {
            continue;
        }
        if let Some(providers) = deps.deployment_store.get_providers(&name) {
            for p in providers.iter() {
                if let Some(did) = p.deployment_id() {
                    deps.flow_controller.remove_slot(did);
                    deps.request_rate.remove(did);
                }
            }
        }
        deps.deployment_store.remove_deployments(&name);
        tracing::info!(model = %name, "Removed model no longer present in DB");
    }
    Ok(())
}

/// Apply all DB rows for one model_name into the live stores — providers,
/// visibility, flow-control slots, per-model quota ratio, and cost rate.
/// This is the single apply point shared by the CRUD path
/// (`reload_model_deployments`) and the reconciler; the two must stay
/// behaviourally identical or web edits and reloads diverge.
///
/// No wildcard reload — callers batching several models (reconcile) reload
/// the wildcard once at the end instead of per model.
pub async fn apply_model_rows_from_db(
    pool: &PgPool,
    model_name: &str,
    deployment_store: &Arc<DeploymentStore>,
    flow_controller: &Arc<FlowController>,
    cost_template_store: &Arc<CostTemplateStore>,
) {
    use std::collections::HashSet as StdHashSet;

    let rows = match DeploymentStore::load_model_rows(pool, model_name).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to reload deployments for '{}': {}", model_name, e);
            return;
        }
    };

    // Snapshot live deployment_ids before the swap so flow-control slots of
    // removed deployments (auto-disable, deployment_id change) get released.
    let old_ids: StdHashSet<String> = deployment_store
        .get_providers(model_name)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.deployment_id().map(str::to_string))
        .collect();

    let mut providers: Vec<Arc<dyn boom_core::Provider>> = Vec::new();
    for row in &rows {
        if let Some(p) = build_provider_from_row(row) {
            providers.push(p);
        }
    }

    // Visibility at model_name granularity: the newest non-normal row wins
    // (rows are ordered by created_at) — same convention as the quota-ratio
    // derivation below.
    let vis = rows.iter().rev().find_map(|r| {
        match boom_routing::visibility_from_db(&r.visibility, &r.allowed_teams) {
            boom_routing::VisibilityState::Normal => None,
            other => Some(other),
        }
    });
    deployment_store.set_visibility(
        model_name,
        vis.unwrap_or(boom_routing::VisibilityState::Normal),
    );

    // Always set (even empty) so resolve_candidates can distinguish
    // "configured but all down" from "never configured". An empty provider
    // list prevents silent fallthrough to the wildcard catch-all.
    if !deployment_store.set_deployments(model_name.to_string(), providers) {
        tracing::error!(
            model = model_name,
            "refused to reload deployments for an exclusive model"
        );
    }

    // FlowController slots: update-or-create for rows still present;
    // ensure_slot removes the slot when both limits are 0, so clearing the
    // limits via the web UI is honoured here too.
    let mut new_ids: StdHashSet<String> = StdHashSet::new();
    for row in &rows {
        if let Some(did) = row.deployment_id.as_deref() {
            flow_controller.ensure_slot(
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
        flow_controller.remove_slot(gone);
    }

    // Per-model quota ratio (newest row wins; reset to 1 when no rows remain).
    let ratio = rows.last().and_then(|r| r.quota_count_ratio).unwrap_or(1).max(0) as u64;
    deployment_store.set_quota_ratio(model_name, ratio);

    // Per-model cost rate from model_info.cost_template — same shared
    // derivation as the YAML build path. Clear any stale rate when no row
    // yields one (set_cost_rate removes the entry on a zero rate).
    let mut applied_cost = false;
    for row in &rows {
        let Some(info_value) = row.model_info.as_ref() else { continue };
        let Ok(info) = serde_json::from_value::<boom_config::ModelInfo>(info_value.clone()) else {
            tracing::warn!(model = model_name, "failed to parse model_info JSON on deployment row");
            continue;
        };
        if let Some(rate) =
            crate::state::cost_rate_from_model_info(cost_template_store, model_name, &info)
        {
            deployment_store.set_cost_rate(model_name, rate);
            applied_cost = true;
        }
    }
    if !applied_cost {
        deployment_store.set_cost_rate(model_name, Default::default());
    }
}

/// Rebuild the "*" wildcard key from every enabled serve_not_match deployment
/// in DB. Called after per-model applies so the wildcard stays in sync
/// without a full store clear+rebuild.
pub async fn reload_wildcard_deployments(
    pool: &PgPool,
    deployment_store: &Arc<DeploymentStore>,
) {
    let rows = match DeploymentStore::load_wildcard_rows(pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to reload wildcard deployments: {}", e);
            return;
        }
    };
    let mut providers: Vec<Arc<dyn boom_core::Provider>> = Vec::new();
    for row in &rows {
        if let Some(p) = build_provider_from_row(row) {
            providers.push(p);
        }
    }
    if !deployment_store.set_deployments("*".to_string(), providers) {
        tracing::error!("refused to reload wildcard '*' deployments (exclusive model conflict)");
    }
}

async fn reconcile_aliases(deps: &ReconcileDeps<'_>) -> anyhow::Result<()> {
    let rows = AliasStore::list_all_db(deps.pool).await?;
    let desired: Vec<String> = rows.iter().map(|r| r.alias_name.clone()).collect();

    for row in &rows {
        deps.alias_store.set_alias(
            row.alias_name.clone(),
            row.target_model.clone(),
            row.hidden.unwrap_or(false),
        );
    }

    let current: Vec<String> = deps
        .alias_store
        .all_aliases()
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    for name in keys_to_remove(&desired, &current) {
        deps.alias_store.remove_alias(&name);
        tracing::info!(alias = %name, "Removed alias no longer present in DB");
    }
    Ok(())
}

async fn reconcile_plans(deps: &ReconcileDeps<'_>) -> anyhow::Result<()> {
    let rows = PlanStore::snapshot_plans_db(deps.pool).await?;
    let desired: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();

    for row in &rows {
        deps.plan_store.upsert_plan(boom_limiter::row_to_plan(row));
    }

    // Default-plan pointers are DB flags now (seeded from YAML on first
    // boot; maintained by dashboard CRUD once endpoints land). First row
    // carrying each flag wins; no row = no default.
    let default_key = rows
        .iter()
        .find(|r| r.is_default.unwrap_or(false))
        .map(|r| r.name.clone());
    deps.plan_store.set_default_plan(default_key);
    let default_team = rows
        .iter()
        .find(|r| r.is_default_team.unwrap_or(false))
        .map(|r| r.name.clone());
    deps.plan_store.set_default_team_plan(default_team);

    let current: Vec<String> = deps
        .plan_store
        .list_plans()
        .into_iter()
        .map(|p| p.name)
        .collect();
    for name in keys_to_remove(&desired, &current) {
        deps.plan_store.delete_plan(&name);
        tracing::info!(plan = %name, "Removed plan no longer present in DB");
    }
    Ok(())
}

async fn reconcile_block_rules(deps: &ReconcileDeps<'_>) -> anyhow::Result<()> {
    let rows = BlockRuleStore::list_all_db(deps.pool).await?;
    let desired: Vec<String> = rows.iter().map(|r| r.rule_name.clone()).collect();

    // Broken rows are skipped with a warn — a corrupt row must not take down
    // rule loading.
    let mut loaded = 0usize;
    let mut skipped = 0usize;
    for row in &rows {
        let rule = match row.to_rule() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Skip DB block rule: {}", e);
                skipped += 1;
                continue;
            }
        };
        match deps.block_rule_store.insert_rule(&rule) {
            Ok(()) => loaded += 1,
            Err(e) => {
                tracing::warn!("Skip DB block rule '{}': {}", row.rule_name, e);
                skipped += 1;
            }
        }
    }

    let current = deps.block_rule_store.rule_names();
    for name in keys_to_remove(&desired, &current) {
        deps.block_rule_store.remove(&name);
        tracing::info!(rule = %name, "Removed block rule no longer present in DB");
    }

    tracing::info!(
        "Reconciled {} block rule(s){}",
        loaded,
        if skipped > 0 {
            format!(", skipped {}", skipped)
        } else {
            String::new()
        }
    );
    Ok(())
}

async fn reconcile_cost_templates(deps: &ReconcileDeps<'_>) -> anyhow::Result<()> {
    let rows = CostTemplateStore::list_all_db(deps.pool).await?;
    let desired: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();

    for row in rows {
        deps.cost_template_store.upsert(row.into());
    }

    let current: Vec<String> = deps
        .cost_template_store
        .list()
        .into_iter()
        .map(|t| t.name)
        .collect();
    for name in keys_to_remove(&desired, &current) {
        deps.cost_template_store.remove(&name);
        tracing::info!(template = %name, "Removed cost template no longer present in DB");
    }
    Ok(())
}

async fn reconcile_workflows(deps: &ReconcileDeps<'_>) -> anyhow::Result<()> {
    let definition_rows = WorkflowStore::list_definitions_db(deps.pool).await?;
    let binding_rows = WorkflowStore::list_models_db(deps.pool).await?;

    // Definitions: upsert every parseable row; a corrupt row is skipped with
    // a warn (must not take down workflow loading).
    let mut broken_definitions = 0usize;
    let mut loaded_definitions: HashSet<String> = HashSet::new();
    for row in &definition_rows {
        match serde_json::from_value::<boom_config::WorkflowDefinitionConfig>(row.definition.clone())
        {
            Ok(definition) => {
                deps.workflow_store
                    .upsert_definition(&row.name, definition);
                loaded_definitions.insert(row.name.clone());
            }
            Err(e) => {
                tracing::warn!(
                    workflow = %row.name,
                    "Skip unparseable workflow definition in DB: {}",
                    e
                );
                broken_definitions += 1;
            }
        }
    }

    // Bindings: skip rows referencing an unloaded definition — keeping them
    // would fail validation ("references unknown workflow") and abort the
    // boot/reload.
    let mut skipped_bindings = 0usize;
    let mut desired_models: Vec<String> = Vec::new();
    for row in &binding_rows {
        if !loaded_definitions.contains(&row.workflow_name) {
            tracing::warn!(
                model = %row.model_name,
                workflow = %row.workflow_name,
                "Skip workflow model binding — referenced definition is missing or broken"
            );
            skipped_bindings += 1;
            continue;
        }
        deps.workflow_store
            .upsert_model(&row.model_name, &row.workflow_name);
        desired_models.push(row.model_name.clone());
    }

    // Removals: memory keys gone from the DB.
    let desired_definitions: Vec<String> = loaded_definitions.into_iter().collect();
    let current_definitions = deps.workflow_store.definition_names();
    for name in keys_to_remove(&desired_definitions, &current_definitions) {
        deps.workflow_store.remove_definition(&name);
        tracing::info!(workflow = %name, "Removed workflow no longer present in DB");
    }
    let current_models = deps.workflow_store.model_names();
    for name in keys_to_remove(&desired_models, &current_models) {
        deps.workflow_store.remove_model(&name);
        tracing::info!(model = %name, "Removed workflow model no longer present in DB");
    }
    if broken_definitions > 0 || skipped_bindings > 0 {
        tracing::warn!(
            definitions = broken_definitions,
            bindings = skipped_bindings,
            "Skipped broken workflow row(s) — fix or delete them via the Dashboard"
        );
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// YAML ingestion helpers (seed + no-DB build share these)
// ═══════════════════════════════════════════════════════════

/// Effective visibility for a YAML model entry after legacy inference:
/// `general_settings.public_models` membership forces Public; a NON-EMPTY
/// `allowed_teams` list without an explicit visibility infers Private
/// (pre-visibility YAML format). An EMPTY list (`[]`) is treated as config
/// residue — it must not privatize the model, or a stale marker left in
/// YAML by an older export silently locks every model on the next seed.
pub(crate) fn effective_yaml_visibility(
    entry: &boom_config::ModelEntry,
    legacy_public: &std::collections::HashSet<&str>,
) -> boom_config::ModelVisibility {
    if legacy_public.contains(entry.model_name.as_str()) {
        boom_config::ModelVisibility::Public
    } else if entry.visibility == boom_config::ModelVisibility::Normal
        && entry
            .allowed_teams
            .as_ref()
            .is_some_and(|teams| !teams.is_empty())
    {
        boom_config::ModelVisibility::Private
    } else {
        entry.visibility
    }
}

/// Build deployment DB inputs from YAML model_list (seed source). Entries
/// flagged `serve_not_match` additionally produce a `"*"` copy so the
/// wildcard key has rows to load from — matching what the pre-flip sync
/// wrote.
pub(crate) fn deployment_inputs_from_config(config: &Config) -> Vec<DeploymentInput> {
    let legacy_public: std::collections::HashSet<&str> = config
        .general_settings
        .public_models
        .iter()
        .map(|m| m.as_str())
        .collect();
    let has_explicit_wildcard = config
        .model_list
        .iter()
        .any(|e| e.model_name == "*");

    let mut inputs: Vec<DeploymentInput> = Vec::new();
    for entry in &config.model_list {
        let p = &entry.litellm_params;
        // Effective visibility once, shared by the row fields below — the
        // ACL is persisted only when it is actually in force, mirroring the
        // CRUD path. Otherwise a leftover `allowed_teams` on a normal entry
        // lands in the DB and re-privatizes the model on every boot via the
        // legacy-marker migration UPDATE.
        let vis = effective_yaml_visibility(entry, &legacy_public);
        let d = DeploymentInput {
            model_name: entry.model_name.clone(),
            litellm_model: p.model.clone(),
            api_key: p.api_key.clone(),
            // YAML path resolves env vars before this point — value is literal.
            api_key_env: Some(false),
            api_base: p.api_base.clone(),
            api_version: p.api_version.clone(),
            aws_region_name: p.aws_region_name.clone(),
            aws_access_key_id: p.aws_access_key_id.clone(),
            aws_secret_access_key: p.aws_secret_access_key.clone(),
            rpm: p.rpm.map(|v| v as i64),
            tpm: p.tpm.map(|v| v as i64),
            timeout: p.timeout as i64,
            headers: serde_json::to_value(&p.headers).unwrap_or(serde_json::json!({})),
            temperature: p.temperature,
            max_tokens: p.max_tokens.map(|v| v as i32),
            deployment_id: entry.model_info.as_ref().and_then(|mi| mi.id.clone()),
            quota_count_ratio: entry
                .model_info
                .as_ref()
                .and_then(|mi| mi.quota_count_ratio)
                .map(|v| v as i64)
                .unwrap_or(1),
            max_inflight_queue_len: entry
                .flow_control
                .as_ref()
                .and_then(|fc| fc.model_queue_limit)
                .map(|v| v as i32),
            max_context_len: entry
                .flow_control
                .as_ref()
                .and_then(|fc| fc.model_context_limit)
                .map(|v| v as i64),
            enabled: entry.enabled,
            client_type_header: entry.client_type_header,
            serve_not_match: entry.serve_not_match,
            model_info: entry
                .model_info
                .as_ref()
                .map(|mi| serde_json::to_value(mi).unwrap_or(serde_json::Value::Null)),
            allowed_teams: if vis == boom_config::ModelVisibility::Private {
                entry.allowed_teams.clone()
            } else {
                None
            },
            visibility: vis,
        };
        let serve_not_match = entry.serve_not_match;
        inputs.push(d);

        // serve_not_match: also write a wildcard "*" record to DB.
        if serve_not_match && !has_explicit_wildcard {
            let mut wildcard = inputs.last().unwrap().clone();
            wildcard.model_name = "*".to_string();
            inputs.push(wildcard);
        }
    }
    inputs
}

/// DB-backed blocklist switch: read `boom_client_blocklist_state`, seeding
/// the row from the YAML value on first contact (fresh DB). After that the
/// row is authoritative — the YAML section only provides the initial value.
/// An absent YAML section seeds ENABLED: with rules living in DB, "no YAML
/// section" must not silently switch blocking off.
pub async fn adopt_blocklist_enabled_db(
    pool: &PgPool,
    store: &BlockRuleStore,
    config: &Config,
) -> Result<(), sqlx::Error> {
    let yaml_default = config
        .client_blocklist
        .as_ref()
        .map(|bl| bl.enabled)
        .unwrap_or(true);
    store.adopt_enabled_db(pool, yaml_default).await?;
    Ok(())
}

/// DB-backed user_tag header list: read `boom_user_tag_config`, seeding the
/// row from the YAML `general_settings.user_tag_headers` value on first
/// contact (fresh DB). After that the row is authoritative — the YAML value
/// only provides the initial seed.
pub async fn adopt_user_tag_headers_db(
    pool: &PgPool,
    store: &boom_audit::UserTagStore,
    config: &Config,
) -> Result<(), sqlx::Error> {
    store
        .adopt_headers_db(pool, config.general_settings.user_tag_headers.clone())
        .await?;
    Ok(())
}

/// 防呆: warn when YAML still carries runtime-section content that is now
/// inert (DB is the authority). Called on boot (when the DB already carried
/// config) and on every reload in DB mode.
pub fn warn_inert_yaml_runtime_sections(config: &Config) {
    if !config.model_list.is_empty() {
        tracing::warn!(
            count = config.model_list.len(),
            "model_list in YAML is inert — models are managed in the DB via the Dashboard; use the config export/write-back buttons to refresh the file"
        );
    }
    if !config.plan_settings.plans.is_empty() {
        tracing::warn!(
            count = config.plan_settings.plans.len(),
            "plan_settings.plans in YAML is inert — plans are managed in the DB via the Dashboard"
        );
    }
    if let Some(bl) = &config.client_blocklist {
        if !bl.rules.is_empty() {
            tracing::warn!(
                count = bl.rules.len(),
                "client_blocklist.rules in YAML is inert — rules are managed in the DB via the Dashboard"
            );
        }
    }
    if !config.router_settings.model_group_alias.is_empty() {
        tracing::warn!(
            count = config.router_settings.model_group_alias.len(),
            "router_settings.model_group_alias in YAML is inert — aliases are managed in the DB via the Dashboard"
        );
    }
    if let Some(headers) = &config.general_settings.user_tag_headers {
        if !headers.is_empty() {
            tracing::warn!(
                count = headers.len(),
                "general_settings.user_tag_headers in YAML is inert — the header list is managed in the DB via the Dashboard"
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Pure diff
// ═══════════════════════════════════════════════════════════

/// Names present in `current` but absent from `desired` — the removal set of
/// an incremental reconcile. Pure so the diff logic is unit-testable without
/// a DB.
pub(crate) fn keys_to_remove(desired: &[String], current: &[String]) -> Vec<String> {
    let desired_set: HashSet<&str> = desired.iter().map(String::as_str).collect();
    current
        .iter()
        .filter(|n| !desired_set.contains(n.as_str()))
        .cloned()
        .collect()
}

/// Build a Provider from a DB deployment row (from DeploymentStore loaders).
pub(crate) fn build_provider_from_row(
    row: &boom_routing::DeploymentProviderRow,
) -> Option<Arc<dyn boom_core::Provider>> {
    use std::collections::HashMap;

    let mut custom_headers = HashMap::new();
    if let Some(obj) = row.headers.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                custom_headers.insert(k.clone(), s.to_string());
            }
        }
    }
    let mut extra = HashMap::new();
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
        &custom_headers,
        row.deployment_id.clone(),
        row.client_type_header.unwrap_or(false),
    ) {
        Ok(provider) => Some(provider),
        Err(e) => {
            tracing::error!(
                "Failed to create provider for model '{}': {}",
                row.model_name,
                e
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boom_config::ModelVisibility;

    fn entry(allowed_teams: Option<Vec<String>>, visibility: ModelVisibility) -> boom_config::ModelEntry {
        let mut e: boom_config::ModelEntry = serde_yaml::from_str(
            "model_name: m\nlitellm_params:\n  model: openai/gpt-4",
        )
        .expect("parse entry");
        e.allowed_teams = allowed_teams;
        e.visibility = visibility;
        e
    }

    #[test]
    fn effective_yaml_visibility_empty_acl_is_residue_not_private() {
        let no_legacy = std::collections::HashSet::new();
        // Empty ACL list without an explicit visibility: config residue from
        // older exports — must stay Normal, not privatize the model.
        assert_eq!(
            effective_yaml_visibility(&entry(Some(Vec::new()), ModelVisibility::Normal), &no_legacy),
            ModelVisibility::Normal
        );
    }

    #[test]
    fn effective_yaml_visibility_nonempty_acl_infers_private() {
        let no_legacy = std::collections::HashSet::new();
        assert_eq!(
            effective_yaml_visibility(
                &entry(Some(vec!["t1".to_string()]), ModelVisibility::Normal),
                &no_legacy
            ),
            ModelVisibility::Private
        );
        // An explicit private stays private even with an empty list.
        assert_eq!(
            effective_yaml_visibility(&entry(Some(Vec::new()), ModelVisibility::Private), &no_legacy),
            ModelVisibility::Private
        );
        // Explicit public wins over a stale ACL.
        assert_eq!(
            effective_yaml_visibility(
                &entry(Some(vec!["t1".to_string()]), ModelVisibility::Public),
                &no_legacy
            ),
            ModelVisibility::Public
        );
    }

    #[test]
    fn seed_inputs_drop_acl_on_non_private_entries() {
        let yaml = r#"
model_list:
  - model_name: m-empty-acl
    litellm_params:
      model: openai/gpt-4
    allowed_teams: []
  - model_name: m-real-acl
    litellm_params:
      model: openai/gpt-4
    allowed_teams: [team-1]
  - model_name: m-clean
    litellm_params:
      model: openai/gpt-4
"#;
        let config: Config = serde_yaml::from_str(yaml).expect("parse yaml");
        let inputs = deployment_inputs_from_config(&config);
        let by_name = |n: &str| {
            inputs
                .iter()
                .find(|d| d.model_name == n)
                .unwrap_or_else(|| panic!("missing {}", n))
        };

        let empty_acl = by_name("m-empty-acl");
        assert_eq!(empty_acl.visibility, ModelVisibility::Normal);
        assert!(
            empty_acl.allowed_teams.is_none(),
            "empty ACL must not be persisted — it re-privatizes the model via the legacy marker"
        );

        let real_acl = by_name("m-real-acl");
        assert_eq!(real_acl.visibility, ModelVisibility::Private);
        assert_eq!(real_acl.allowed_teams, Some(vec!["team-1".to_string()]));

        let clean = by_name("m-clean");
        assert_eq!(clean.visibility, ModelVisibility::Normal);
        assert!(clean.allowed_teams.is_none());
    }

    #[test]
    fn keys_to_remove_returns_disjoint_current() {
        let desired = vec!["a".to_string(), "b".to_string()];
        let current = vec!["a".to_string(), "c".to_string(), "d".to_string()];
        assert_eq!(keys_to_remove(&desired, &current), vec!["c", "d"]);
    }

    #[test]
    fn keys_to_remove_empty_desired_removes_all() {
        let current = vec!["x".to_string(), "y".to_string()];
        assert_eq!(keys_to_remove(&[], &current), vec!["x", "y"]);
    }

    #[test]
    fn keys_to_remove_empty_current_removes_nothing() {
        let desired = vec!["x".to_string()];
        assert!(keys_to_remove(&desired, &[]).is_empty());
    }

    #[test]
    fn keys_to_remove_handles_duplicates_and_overlap() {
        let desired = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        let current = vec!["b".to_string(), "b".to_string()];
        assert!(keys_to_remove(&desired, &current).is_empty());
    }
}
