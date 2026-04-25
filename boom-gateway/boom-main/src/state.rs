use arc_swap::ArcSwap;
use boom_auth::DbAuthenticator;
use boom_config::Config;
use boom_core::provider::Authenticator;
use boom_core::DebugErrorStore;
use boom_limiter::{PlanStore, RateLimitPlan, ScheduleSlot, SlidingWindowLimiter};
use boom_flowcontrol::{FlowControlConfig, FlowController};
use boom_routing::{AliasStore, DeploymentStore, InFlightTracker, KeyAffinityPolicy, Router, RoundRobinPolicy, SchedulePolicy};
use boom_promptlog::PromptLogWriter;
use boom_provider;
use dashmap::DashMap;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64};

/// Shared application state.
///
/// `inner` is wrapped in `ArcSwap` for lock-free atomic hot-swap:
///   - New requests immediately see the reloaded config.
///   - In-flight requests keep using the old state until done.
///   - Zero downtime, no races.
///
/// `db_pool`, `limiter`, `plan_store`, `deployment_store`, `alias_store`
/// live at this level — they survive reloads so DB connections,
/// rate-limit counters, deployments, and aliases are preserved.
#[derive(Clone)]
pub struct AppState {
    /// Config file path (stored for reload).
    pub config_path: String,
    /// Hot-swappable inner state (config + auth + health only).
    pub inner: Arc<ArcSwap<AppStateInner>>,
    /// DB pool survives reloads (avoids reconnection).
    pub db_pool: Option<PgPool>,
    /// Limiter survives reloads (preserves in-flight counters).
    pub limiter: Arc<SlidingWindowLimiter>,
    /// Plan store survives reloads (preserves plan definitions and key assignments).
    pub plan_store: Arc<PlanStore>,
    /// Deployment store survives reloads (preserves model deployments).
    pub deployment_store: Arc<DeploymentStore>,
    /// Alias store survives reloads (preserves model aliases).
    pub alias_store: Arc<AliasStore>,
    /// Router owns deployment + alias stores for routing decisions.
    pub router: Arc<Router>,
    /// In-flight request tracker (per-model count + input chars).
    pub inflight: Arc<InFlightTracker>,
    /// Request counter for periodic summary logging.
    pub request_count: Arc<AtomicU64>,
    /// deployment_id → consecutive failure count (auto-disable threshold).
    pub failure_counter: Arc<DashMap<String, Arc<AtomicU32>>>,
    /// Per-deployment flow controller (survives reloads).
    pub flow_controller: Arc<FlowController>,
    /// Debug error store — captures upstream error details on demand.
    pub debug_store: Arc<DebugErrorStore>,
    /// Prompt log writer — captures full request/response for audit.
    pub prompt_log_writer: PromptLogWriter,
}

/// The state that gets swapped on config reload.
/// Only contains config, auth, and health — deployments/aliases live in stores.
pub struct AppStateInner {
    pub config: Config,
    pub auth: Arc<dyn Authenticator>,
    pub health: HealthStatus,
}

#[derive(Debug, Clone)]
pub struct HealthStatus {
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub last_reload_at: chrono::DateTime<chrono::Utc>,
    pub db_connected: bool,
    pub reload_count: u64,
}

impl AppState {
    /// Build state from config. Called once at startup.
    ///
    /// Unified YAML-priority flow:
    ///   1. Build deployments/aliases/plans from YAML → memory stores
    ///   2. sync_yaml_to_db() → persist YAML to DB, handle same-name conflicts
    ///   3. load_db_only_*() → load source='db' records from DB on top
    ///   4. Restore runtime state (assignments, counters)
    pub async fn from_config(config: Config, config_path: String) -> anyhow::Result<Self> {
        // 1. Connect to database (optional).
        let db_pool = match &config.general_settings.database_url {
            Some(url) => {
                tracing::info!("Connecting to database...");
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(30)
                    .acquire_timeout(std::time::Duration::from_secs(10))
                    .idle_timeout(std::time::Duration::from_secs(600))
                    .max_lifetime(std::time::Duration::from_secs(1800))
                    .connect(url)
                    .await?;
                tracing::info!("Database connected");
                Some(pool)
            }
            None => {
                tracing::warn!("No database URL — running in master-key-only auth mode");
                None
            }
        };

        // 2. Limiter survives across reloads.
        let limiter = Arc::new(SlidingWindowLimiter::new());

        // 3. Plan store survives across reloads.
        let plan_store = Arc::new(PlanStore::new());

        // 4. Deployment store & alias store survive across reloads.
        let deployment_store = Arc::new(DeploymentStore::new());
        let alias_store = Arc::new(AliasStore::new());

        // In-flight tracker survives across reloads — must be created before policy.
        let inflight = Arc::new(InFlightTracker::new());

        // Flow controller survives across reloads.
        let flow_controller = Arc::new(FlowController::new());

        // Debug error store survives across reloads.
        let debug_store = Arc::new(DebugErrorStore::new());

        // Create scheduling policy from config (may reference inflight).
        let policy = create_policy(&config, &inflight, &flow_controller);

        // Router wraps stores + policy for routing decisions.
        let router = Arc::new(Router::new(deployment_store.clone(), alias_store.clone(), policy));

        // 5. Build from YAML first, then layer DB-only records on top.
        build_deployments_from_config(&config, &deployment_store);
        build_aliases_from_config(&config, &alias_store, &deployment_store);
        load_plans_from_config(&plan_store, &config);
        seed_flow_controller_from_config(&config, &flow_controller);

        if let Some(ref pool) = db_pool {
            // Run migrations (all tables).
            if let Err(e) = boom_dashboard::migrations::run_migrations(pool).await {
                tracing::error!("Failed to run migrations: {}", e);
            }

            // Sync YAML config to DB (upsert source='yaml', handle conflicts).
            if let Err(e) = sync_yaml_to_db(pool, &config, &plan_store).await {
                tracing::error!("Failed to sync YAML to DB: {}", e);
            }

            // Load source='db' records on top of YAML-built stores.
            load_db_only_deployments(pool, &deployment_store, &flow_controller).await;
            load_db_only_aliases(pool, &alias_store).await;
            plan_store.load_db_only_plans(pool).await;

            // Restore runtime state.
            plan_store.restore_assignments_from_db(pool).await;
            limiter.restore_counters_from_db(pool).await;
        }

        // 6. Build inner state (config + auth + health).
        let prompt_log_config = config.prompt_log.as_ref()
            .and_then(|v| serde_json::from_value::<boom_promptlog::PromptLogConfig>(v.clone()).ok())
            .unwrap_or_default();
        let prompt_log_writer = PromptLogWriter::spawn(prompt_log_config);

        let inner = Self::build_inner(config, &db_pool, chrono::Utc::now(), 0)?;

        Ok(Self {
            config_path,
            inner: Arc::new(ArcSwap::from_pointee(inner)),
            db_pool,
            limiter,
            plan_store,
            deployment_store,
            alias_store,
            router,
            inflight,
            request_count: Arc::new(AtomicU64::new(0)),
            failure_counter: Arc::new(DashMap::new()),
            flow_controller,
            debug_store,
            prompt_log_writer,
        })
    }

    /// Hot-reload: re-read config file and update state.
    ///
    /// Unified YAML-priority flow (same as startup, minus DB reconnection):
    ///   1. Rebuild deployments/aliases/plans from YAML → memory stores
    ///   2. sync_yaml_to_db() → persist YAML to DB, handle conflicts
    ///   3. load_db_only_*() → load source='db' records on top
    ///   4. Clean up orphaned assignments
    pub async fn reload(&self) -> anyhow::Result<String> {
        tracing::info!("Hot-reloading config from {}...", self.config_path);

        // 1. Re-read config.
        let new_config = boom_config::load_config(&self.config_path)?;

        // 2. Snapshot old state to get counts.
        let old_guard = self.inner.load();
        let old_started_at = old_guard.health.started_at;
        let old_reload_count = old_guard.health.reload_count;
        let old_db_url = old_guard.config.general_settings.database_url.clone();
        drop(old_guard);

        // 3. Check if DB URL changed.
        let db_pool = if old_db_url != new_config.general_settings.database_url {
            tracing::info!("Database URL changed, reconnecting...");
            match &new_config.general_settings.database_url {
                Some(url) => Some(
                    sqlx::postgres::PgPoolOptions::new()
                        .max_connections(30)
                        .acquire_timeout(std::time::Duration::from_secs(10))
                        .idle_timeout(std::time::Duration::from_secs(600))
                        .max_lifetime(std::time::Duration::from_secs(1800))
                        .connect(url)
                        .await?,
                ),
                None => None,
            }
        } else {
            self.db_pool.clone()
        };

        let new_reload_count = old_reload_count + 1;

        // 4. Rebuild stores: YAML first, then DB-only on top.
        self.deployment_store.clear();
        build_deployments_from_config(&new_config, &self.deployment_store);

        self.alias_store.clear();
        build_aliases_from_config(&new_config, &self.alias_store, &self.deployment_store);

        self.plan_store.clear_plans();
        load_plans_from_config(&self.plan_store, &new_config);
        seed_flow_controller_from_config(&new_config, &self.flow_controller);

        // Recreate policy (fresh counters etc.) — router reuses same stores.
        let new_policy = create_policy(&new_config, &self.inflight, &self.flow_controller);
        self.router.set_policy(new_policy);

        if let Some(ref pool) = db_pool {
            // Sync YAML config to DB (upsert source='yaml', handle conflicts).
            if let Err(e) = sync_yaml_to_db(pool, &new_config, &self.plan_store).await {
                tracing::error!("Failed to sync YAML to DB: {}", e);
            }

            // Load source='db' records on top of YAML-built stores.
            load_db_only_deployments(pool, &self.deployment_store, &self.flow_controller).await;
            load_db_only_aliases(pool, &self.alias_store).await;
            self.plan_store.load_db_only_plans(pool).await;
        }

        // Clean up assignments pointing to plans that no longer exist.
        self.plan_store.cleanup_assignments();

        // 5. Update prompt log config (hot-reload).
        if let Some(ref v) = new_config.prompt_log {
            if let Ok(pc) = serde_json::from_value::<boom_promptlog::PromptLogConfig>(v.clone()) {
                self.prompt_log_writer.update_config(pc);
            }
        } else {
            self.prompt_log_writer.update_config(boom_promptlog::PromptLogConfig::default());
        }

        // 6. Build new inner state.
        let new_inner =
            Self::build_inner(new_config, &db_pool, old_started_at, new_reload_count)?;

        // 7. Atomic swap.
        self.inner.store(Arc::new(new_inner));

        let model_count = self.deployment_store.len();
        let summary = format!(
            "Reloaded: {} model(s), reload #{}",
            model_count, new_reload_count,
        );
        tracing::info!("{}", summary);
        Ok(summary)
    }

    /// Build AppStateInner from config.
    fn build_inner(
        config: Config,
        db_pool: &Option<PgPool>,
        started_at: chrono::DateTime<chrono::Utc>,
        reload_count: u64,
    ) -> Result<AppStateInner, anyhow::Error> {
        // Build authenticator.
        let auth: Arc<dyn Authenticator> = Arc::new(DbAuthenticator::new(
            db_pool.clone(),
            config.general_settings.master_key.clone(),
        ));

        let health = HealthStatus {
            started_at,
            last_reload_at: chrono::Utc::now(),
            db_connected: db_pool.is_some(),
            reload_count,
        };

        Ok(AppStateInner {
            config,
            auth,
            health,
        })
    }

    /// Dump current runtime config (models, aliases, plans) to a timestamped YAML snapshot.
    /// Best-effort: errors are logged but not propagated.
    pub async fn dump_config_snapshot(&self) {
        let pool = match &self.db_pool {
            Some(p) => p,
            None => return,
        };

        let config_value = match build_config_snapshot_value(pool).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to build config snapshot: {}", e);
                return;
            }
        };

        let yaml_str = match serde_yaml::to_string(&config_value) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to serialize config snapshot to YAML: {}", e);
                return;
            }
        };

        let timestamp = chrono::Local::now().format("%Y%m%d%H%M%S");
        let snapshot_path = format!("{}.{}", self.config_path, timestamp);

        match tokio::fs::write(&snapshot_path, &yaml_str).await {
            Ok(_) => {
                tracing::info!(path = %snapshot_path, "Config snapshot saved");
            }
            Err(e) => {
                tracing::error!(path = %snapshot_path, "Failed to write config snapshot: {}", e);
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════
// YAML → DB sync (delegates to owning modules' Store methods)
// ═══════════════════════════════════════════════════════════

/// Sync YAML config to DB: replace source='yaml' rows, handle same-name conflicts.
///
/// Delegates SQL to owning modules (boom-routing, boom-limiter).
/// Only plan sync remains here (will move to boom-limiter in Phase 1b).
async fn sync_yaml_to_db(pool: &PgPool, config: &Config, plan_store: &Arc<PlanStore>) -> Result<(), sqlx::Error> {
    // ── Deployments (delegated to DeploymentStore) ──
    let yaml_model_names: Vec<String> = config.model_list.iter()
        .map(|e| e.model_name.clone()).collect();
    let mut yaml_deployments: Vec<boom_routing::YamlDeploymentData> = Vec::new();
    for entry in &config.model_list {
        let p = &entry.litellm_params;
        let d = boom_routing::YamlDeploymentData {
            model_name: entry.model_name.clone(),
            litellm_model: p.model.clone(),
            api_key: p.api_key.clone(),
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
            quota_count_ratio: entry.model_info.as_ref()
                .and_then(|mi| mi.quota_count_ratio)
                .map(|v| v as i64)
                .unwrap_or(1),
            max_inflight_queue_len: entry.flow_control.as_ref()
                .and_then(|fc| fc.model_queue_limit).map(|v| v as i32),
            max_context_len: entry.flow_control.as_ref()
                .and_then(|fc| fc.model_context_limit).map(|v| v as i64),
            enabled: entry.enabled,
        };
        yaml_deployments.push(d);

        // serve_not_match: also write a wildcard "*" record to DB.
        if entry.serve_not_match && !yaml_model_names.contains(&"*".to_string()) {
            let mut wildcard = yaml_deployments.last().unwrap().clone();
            wildcard.model_name = "*".to_string();
            yaml_deployments.push(wildcard);
        }
    }
    // Add "*" to yaml_model_names if any entry uses serve_not_match,
    // so sync_yaml_to_db cleans up conflicting source='db' rows.
    let mut all_model_names = yaml_model_names;
    if config.model_list.iter().any(|e| e.serve_not_match) {
        if !all_model_names.contains(&"*".to_string()) {
            all_model_names.push("*".to_string());
        }
    }
    DeploymentStore::sync_yaml_to_db(pool, &all_model_names, &yaml_deployments).await?;

    // ── Aliases (delegated to AliasStore) ──
    let yaml_aliases: Vec<(String, String, bool)> = config.router_settings.model_group_alias.iter()
        .map(|(alias, cfg)| (alias.clone(), cfg.target_model().to_string(), cfg.is_hidden()))
        .collect();
    AliasStore::sync_yaml_to_db(pool, &yaml_aliases).await?;

    // ── Plans (delegated to PlanStore) ──
    // plan_store already has RateLimitPlan objects loaded by load_plans_from_config.
    let all_plans = plan_store.list_plans();
    let yaml_plans: Vec<(String, &RateLimitPlan)> = all_plans.iter()
        .map(|p| (p.name.clone(), p))
        .collect();
    let default_plan = config.plan_settings.default_plan.as_deref();
    PlanStore::sync_yaml_to_db(pool, &yaml_plans, default_plan).await?;

    Ok(())
}

// ═══════════════════════════════════════════════════════════
// DB-only loading (source='db' records on top of YAML stores)
// ═══════════════════════════════════════════════════════════


/// Build providers from DB deployment rows and add to DeploymentStore.
/// Uses DeploymentStore::load_db_only_rows() for SQL, creates providers here
/// (because creating Arc<dyn Provider> requires boom-provider which boom-routing doesn't depend on).
async fn load_db_only_deployments(
    pool: &PgPool,
    deployment_store: &Arc<DeploymentStore>,
    flow_controller: &Arc<FlowController>,
) {
    let rows = match DeploymentStore::load_db_only_rows(pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to load DB-only deployments: {}", e);
            return;
        }
    };

    let mut deployment_count = 0;
    for row in &rows {
        let mut extra = std::collections::HashMap::new();
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
            Ok(provider) => {
                deployment_store.add_deployment(&row.model_name, provider);
                deployment_count += 1;
            }
            Err(e) => {
                tracing::error!("Failed to create provider for model '{}': {}", row.model_name, e);
            }
        }
    }

    // Seed flow control for DB-only deployments using full rows.
    let fc_rows = match DeploymentStore::list_all_db(pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to load DB deployments for flow control: {}", e);
            tracing::info!("Loaded {} DB-only deployment(s)", deployment_count);
            return;
        }
    };
    for row in &fc_rows {
        if row.source.as_deref() != Some("db") {
            continue;
        }
        if let Some(ref did) = row.deployment_id {
            let max_inflight = row.max_inflight_queue_len.unwrap_or(0) as u32;
            let max_context = row.max_context_len.unwrap_or(0) as u64;
            if max_inflight > 0 || max_context > 0 {
                flow_controller.ensure_slot(did, &FlowControlConfig {
                    max_inflight,
                    max_context,
                });
            }
        }
    }

    tracing::info!("Loaded {} DB-only deployment(s)", deployment_count);
}

/// Load source='db' aliases from DB (delegated to AliasStore).
async fn load_db_only_aliases(pool: &PgPool, alias_store: &Arc<AliasStore>) {
    alias_store.load_db_only(pool).await;
}


// ═══════════════════════════════════════════════════════════
// YAML → Memory (no-DB fallback)
// ═══════════════════════════════════════════════════════════

/// Build deployments directly from YAML config into DeploymentStore.
fn build_deployments_from_config(config: &Config, deployment_store: &Arc<DeploymentStore>) {
    deployment_store.clear();

    for entry in &config.model_list {
        let p = &entry.litellm_params;

        let mut extra = p.headers.clone();
        if let Some(ref v) = p.api_version {
            extra.insert("api_version".to_string(), v.clone());
        }
        if let Some(ref r) = p.aws_region_name {
            extra.insert("aws_region_name".to_string(), r.clone());
        }

        let deployment_id = entry.model_info.as_ref().and_then(|mi| mi.id.clone());

        // Extract quota_count_ratio from model_info (default 1).
        let ratio = entry.model_info.as_ref()
            .and_then(|mi| mi.quota_count_ratio)
            .unwrap_or(1);

        // Skip provider creation for disabled deployments.
        if !entry.enabled {
            tracing::info!(model = %entry.model_name, "Deployment disabled in YAML config, skipping routing");
            continue;
        }

        match boom_provider::create_provider(
            &p.model,
            p.api_key.clone(),
            p.api_base.clone(),
            p.timeout,
            &extra,
            deployment_id,
        ) {
            Ok(provider) => {
                // Also register as wildcard catch-all if flagged.
                if entry.serve_not_match {
                    deployment_store.add_deployment("*", provider.clone());
                    tracing::info!(model = %entry.model_name, "Registered as wildcard catch-all");
                }

                deployment_store.add_deployment(&entry.model_name, provider);

                if ratio != 1 {
                    tracing::info!(
                        model = %entry.model_name,
                        ratio = ratio,
                        "Setting quota count ratio"
                    );
                }
                deployment_store.set_quota_ratio(&entry.model_name, ratio);
            }
            Err(e) => {
                tracing::error!(
                    "Failed to create provider for model '{}': {}",
                    entry.model_name,
                    e
                );
            }
        }
    }

    tracing::info!(
        "Built {} model(s) with {} deployment(s) from YAML",
        deployment_store.len(),
        deployment_store.total_deployments(),
    );
}

/// Build aliases directly from YAML config into AliasStore.
fn build_aliases_from_config(
    config: &Config,
    alias_store: &Arc<AliasStore>,
    deployment_store: &Arc<DeploymentStore>,
) {
    alias_store.clear();

    for (alias, alias_cfg) in &config.router_settings.model_group_alias {
        let target = alias_cfg.target_model();
        if !deployment_store.contains(target) {
            tracing::warn!(
                "Skipping alias '{}' → '{}': target model not found in deployments",
                alias,
                target
            );
            continue;
        }
        tracing::info!("Model alias: '{}' → '{}'", alias, target);
        alias_store.set_alias(alias.clone(), target.to_string(), alias_cfg.is_hidden());
    }

    tracing::info!(
        "Loaded {} alias(es), {} hidden",
        alias_store.len(),
        alias_store.hidden_count(),
    );
}

/// Seed FlowController from YAML config.
/// Only creates slots for deployments that have flow control parameters set.
fn seed_flow_controller_from_config(config: &Config, flow_controller: &Arc<FlowController>) {
    let mut active_ids = Vec::new();

    for entry in &config.model_list {
        let deployment_id = match entry.model_info.as_ref().and_then(|mi| mi.id.as_ref()) {
            Some(id) if !id.is_empty() => id.clone(),
            _ => continue, // No deployment_id — skip flow control.
        };

        let fc = match entry.flow_control.as_ref() {
            Some(fc) => fc,
            None => continue, // No flow_control section — skip.
        };

        let max_inflight = fc.model_queue_limit.unwrap_or(0);
        let max_context = fc.model_context_limit.unwrap_or(0);

        if max_inflight > 0 || max_context > 0 {
            flow_controller.ensure_slot(&deployment_id, &FlowControlConfig {
                max_inflight,
                max_context,
            });
            active_ids.push(deployment_id.clone());
            tracing::info!(
                deployment_id = %deployment_id,
                max_inflight,
                max_context,
                "Flow control configured"
            );
        }
    }

    // Remove slots for deployments no longer in config.
    flow_controller.retain_slots(&active_ids);
}

/// Load plans from YAML config into PlanStore.
fn load_plans_from_config(plan_store: &Arc<PlanStore>, config: &Config) {
    for (name, pc) in &config.plan_settings.plans {
        let window_limits: Vec<(u64, u64)> = pc
            .window_limits
            .iter()
            .filter_map(|w| {
                if w.len() >= 2 {
                    Some((w[0], w[1]))
                } else {
                    None
                }
            })
            .collect();

        let plan = RateLimitPlan {
            name: name.clone(),
            concurrency_limit: pc.concurrency_limit,
            rpm_limit: pc.rpm_limit,
            window_limits,
            schedule: convert_schedule(&pc.schedule),
        };
        plan_store.upsert_plan(plan);
    }

    match &config.plan_settings.default_plan {
        Some(dp) => {
            if plan_store.get_plan(dp).is_some() {
                plan_store.set_default_plan(Some(dp.clone()));
                tracing::info!(default_plan = %dp, "Default plan set");
            } else {
                tracing::warn!(
                    default_plan = %dp,
                    "default_plan '{}' not found in configured plans, ignoring",
                    dp
                );
                plan_store.set_default_plan(None);
            }
        }
        None => {
            plan_store.set_default_plan(None);
            tracing::warn!("没有默认套餐配置，所有用户将无套餐限制。");
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════

/// Create a scheduling policy from config.
fn create_policy(config: &Config, inflight: &Arc<InFlightTracker>, flow_controller: &Arc<FlowController>) -> Arc<dyn SchedulePolicy> {
    match config.router_settings.schedule_policy.as_str() {
        "round_robin" | "" => Arc::new(RoundRobinPolicy::new()),
        "key_affinity" => {
            let ctx_threshold = config.router_settings.key_affinity_context_threshold;
            let rebalance_threshold = config.router_settings.key_affinity_rebalance_threshold;
            tracing::info!(
                "Using key_affinity policy: context_threshold={}, rebalance_threshold={}",
                ctx_threshold,
                rebalance_threshold,
            );
            let mut policy = KeyAffinityPolicy::new(
                inflight.clone(),
                ctx_threshold,
                rebalance_threshold,
            );
            policy.set_queue_info(flow_controller.clone());
            Arc::new(policy)
        }
        other => {
            tracing::warn!(
                "Unknown schedule_policy '{}', falling back to round_robin",
                other
            );
            Arc::new(RoundRobinPolicy::new())
        }
    }
}

/// Convert config schedule slots into limiter schedule slots.
fn convert_schedule(slots: &[boom_config::ScheduleSlotConfig]) -> Vec<ScheduleSlot> {
    slots
        .iter()
        .map(|s| ScheduleSlot {
            hours: s.hours.clone(),
            concurrency_limit: s.concurrency_limit,
            rpm_limit: s.rpm_limit,
            window_limits: s
                .window_limits
                .iter()
                .filter_map(|w| {
                    if w.len() >= 2 {
                        Some((w[0], w[1]))
                    } else {
                        None
                    }
                })
                .collect(),
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════
// Config snapshot (DB → YAML)
// ═══════════════════════════════════════════════════════════

/// Build a serde_json::Value representing the current runtime config
/// (model_list, router_settings.model_group_alias, plan_settings).
async fn build_config_snapshot_value(pool: &PgPool) -> Result<serde_json::Value, sqlx::Error> {
    // ── Model list (delegated to DeploymentStore) ──
    let model_rows = DeploymentStore::snapshot_db(pool).await?;

    let model_list: Vec<serde_json::Value> = model_rows
        .into_iter()
        .map(|r| {
            let mut litellm_params = serde_json::Map::new();
            litellm_params.insert("model".into(), serde_json::Value::String(r.litellm_model));
            if let Some(k) = r.api_key {
                litellm_params.insert("api_key".into(), serde_json::Value::String(k));
            }
            if let Some(b) = r.api_base {
                litellm_params.insert("api_base".into(), serde_json::Value::String(b));
            }
            if let Some(v) = r.api_version {
                litellm_params.insert("api_version".into(), serde_json::Value::String(v));
            }
            if let Some(r) = r.aws_region_name {
                litellm_params.insert("aws_region_name".into(), serde_json::Value::String(r));
            }
            if let Some(k) = r.aws_access_key_id {
                litellm_params.insert("aws_access_key_id".into(), serde_json::Value::String(k));
            }
            if let Some(k) = r.aws_secret_access_key {
                litellm_params.insert("aws_secret_access_key".into(), serde_json::Value::String(k));
            }
            if let Some(rpm) = r.rpm {
                litellm_params.insert("rpm".into(), serde_json::Value::Number(rpm.into()));
            }
            if let Some(tpm) = r.tpm {
                litellm_params.insert("tpm".into(), serde_json::Value::Number(tpm.into()));
            }
            litellm_params.insert("timeout".into(), serde_json::Value::Number(r.timeout.into()));
            if let Some(t) = r.temperature {
                litellm_params.insert(
                    "temperature".into(),
                    serde_json::Value::Number(
                        serde_json::Number::from_f64(t).unwrap_or(serde_json::Number::from(0)),
                    ),
                );
            }
            if let Some(m) = r.max_tokens {
                litellm_params.insert("max_tokens".into(), serde_json::Value::Number(m.into()));
            }
            if let Some(v) = r.max_inflight_queue_len {
                litellm_params.insert("max_inflight_queue_len".into(), serde_json::Value::Number(v.into()));
            }
            if let Some(v) = r.max_context_len {
                litellm_params.insert("max_context_len".into(), serde_json::Value::Number(v.into()));
            }
            // Only include headers if non-empty.
            if let Some(obj) = r.headers.as_object() {
                if !obj.is_empty() {
                    litellm_params.insert("headers".into(), r.headers);
                }
            }

            let mut entry = serde_json::json!({
                "model_name": r.model_name,
                "litellm_params": litellm_params,
            });

            // Include model_info if deployment_id is set.
            if let Some(ref did) = r.deployment_id {
                if !did.is_empty() {
                    entry.as_object_mut().unwrap().insert(
                        "model_info".into(),
                        serde_json::json!({ "id": did }),
                    );
                }
            }

            entry
        })
        .collect();

    // ── Aliases (delegated to AliasStore) ──
    let alias_rows = AliasStore::snapshot_db(pool).await?;

    let model_group_alias: serde_json::Map<String, serde_json::Value> = alias_rows
        .into_iter()
        .map(|(alias_name, target_model)| (alias_name, serde_json::Value::String(target_model)))
        .collect();

    // ── Plans (delegated to PlanStore) ──
    let plan_rows = PlanStore::snapshot_plans_db(pool).await?;

    let mut default_plan: Option<String> = None;
    let mut plans_map = serde_json::Map::new();

    for r in &plan_rows {
        if r.is_default.unwrap_or(false) && default_plan.is_none() {
            default_plan = Some(r.name.clone());
        }

        let window_limits: Vec<serde_json::Value> = r
            .window_limits
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let a = item.as_array()?;
                        if a.len() >= 2 {
                            Some(serde_json::json!([a[0], a[1]]))
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let schedule: Vec<serde_json::Value> = r
            .schedule
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let obj = item.as_object()?;
                        let mut slot = serde_json::Map::new();
                        if let Some(h) = obj.get("hours").and_then(|v| v.as_str()) {
                            slot.insert("hours".into(), serde_json::Value::String(h.to_string()));
                        }
                        if let Some(v) = obj.get("concurrency_limit").and_then(|v| v.as_u64()) {
                            slot.insert("concurrency_limit".into(), serde_json::Value::Number(v.into()));
                        }
                        if let Some(v) = obj.get("rpm_limit").and_then(|v| v.as_u64()) {
                            slot.insert("rpm_limit".into(), serde_json::Value::Number(v.into()));
                        }
                        if let Some(wl) = obj.get("window_limits") {
                            slot.insert("window_limits".into(), wl.clone());
                        }
                        Some(serde_json::Value::Object(slot))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut plan_obj = serde_json::Map::new();
        if let Some(cl) = r.concurrency_limit {
            plan_obj.insert("concurrency_limit".into(), serde_json::Value::Number(cl.into()));
        }
        if let Some(rpm) = r.rpm_limit {
            plan_obj.insert("rpm_limit".into(), serde_json::Value::Number(rpm.into()));
        }
        if !window_limits.is_empty() {
            plan_obj.insert("window_limits".into(), serde_json::Value::Array(window_limits));
        }
        if !schedule.is_empty() {
            plan_obj.insert("schedule".into(), serde_json::Value::Array(schedule));
        }

        plans_map.insert(r.name.clone(), serde_json::Value::Object(plan_obj));
    }

    // ── Assemble top-level ──
    let mut plan_settings = serde_json::Map::new();
    if let Some(dp) = default_plan {
        plan_settings.insert("default_plan".into(), serde_json::Value::String(dp));
    }
    plan_settings.insert("plans".into(), serde_json::Value::Object(plans_map));

    Ok(serde_json::json!({
        "model_list": model_list,
        "router_settings": {
            "model_group_alias": model_group_alias,
        },
        "plan_settings": plan_settings,
    }))
}
