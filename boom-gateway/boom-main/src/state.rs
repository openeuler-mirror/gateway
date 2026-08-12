use arc_swap::ArcSwap;
use boom_auth::DbAuthenticator;
use boom_config::Config;
use boom_core::provider::{Authenticator, KeyAliasLookup};
use boom_core::DebugErrorStore;
use boom_kvindex::{TokenPrefixIndex};
use boom_core::kv_event::KvIndexBackend;
use boom_limiter::{PlanStore, RateLimitPlan, ScheduleSlot, SlidingWindowLimiter};
use boom_flowcontrol::{FlowControlConfig, FlowController};
use boom_routing::{register_fusion_providers, AliasStore, DeploymentStore, FusionRuntime, HybridRouter, InFlightTracker, KeyAffinityPolicy, MlServiceClient, RebalanceMoveTracker, RequestRateTracker, Router, RoundRobinPolicy, SchedulePolicy, StrategyRegistry, TierClassifier};
use boom_ctxaware::AgentStatsTracker;
use boom_promptlog::PromptLogWriter;
use boom_provider;
use sqlx::PgPool;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64};

use crate::health_monitor::DeploymentHealthStore;

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
    /// Dashboard-only DB pool with tiny max_connections so heavy stats
    /// aggregations can never starve the forwarding path. max=3, acquire_timeout=10s.
    pub dashboard_db_pool: Option<PgPool>,
    /// Audit log writer — single-task, batch INSERT, dedicated pool.
    /// Cross-reload (DB pool itself survives). None only when no DB configured.
    pub log_writer: Option<Arc<crate::request_log::LogWriter>>,
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
    /// Deployment health counters for metric-driven auto offline/recovery.
    pub deployment_health: Arc<DeploymentHealthStore>,
    /// Request-failure consecutive counters for request-driven auto-disable.
    pub request_failure_counter: Arc<DashMap<String, AtomicU32>>,
    /// Per-deployment flow controller (survives reloads).
    pub flow_controller: Arc<FlowController>,
    /// Debug error store — captures upstream error details on demand.
    pub debug_store: Arc<DebugErrorStore>,
    /// Prompt log writer — captures full request/response for audit.
    pub prompt_log_writer: PromptLogWriter,
    /// Read-only prompt-log query API (entry lookup + config snapshot),
    /// shared with the dashboard. Wraps the same `Arc<ArcSwap<_>>` config
    /// the writer mutates, so hot-reloads are visible without re-binding.
    pub prompt_log_query: Arc<dyn boom_promptlog::PromptLogQueryApi>,
    /// Per-deployment rebalance move tracker (in/out counts, survives reloads).
    pub rebalance_move_tracker: Arc<RebalanceMoveTracker>,
    /// Per-deployment request rate tracker (survives reloads).
    pub request_rate: Arc<RequestRateTracker>,
    /// Agent (client-type) statistics tracker (survives reloads).
    pub agent_stats: Arc<AgentStatsTracker>,
    /// KV-cache prefix index, hot-swappable across reloads.
    ///
    /// THIRD lifecycle (distinct from AppState's other fields): unlike
    /// deployment_store / plan_store / limiter — which survive reloads with
    /// their contents intact — this is rebuilt EMPTY on every reload. Any
    /// kvc_aware config change (policy, weights, block_size) swaps in a fresh
    /// index; the old trie is dropped and the new one starts empty, repopulated
    /// by the orchestrator recording routed requests (self-contained learning —
    /// no ZMQ subscriber). The transient moment (queries hit an empty trie →
    /// 0 hit → route by load) is intentional ("rebuild = clear cache"). `None`
    /// when kvc_aware is disabled. See `reload()`.
    pub kv_index: Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
    /// Handle to the running TTL prune task (sweeps expired approximate blocks).
    /// None when kvc_aware disabled. Held so reload can abort before respawning.
    pub kv_prune_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// kvc-aware routing orchestrator (prefix-serialize + route + DFX + record).
    /// Handler calls kvc_orchestrator.route() — all kvc business logic is here,
    /// not in routes.rs. Returns None when kvc is disabled.
    pub kvc_orchestrator: crate::kvc::KvcOrchestrator,
    /// Real-time pressure metrics collector (1 Hz sample ring buffer).
    /// Survives reloads — sampler task is spawned once at startup and not
    /// respawned (it doesn't depend on any per-config resource). Concrete
    /// type (not `Arc<dyn>`) so the sampler in main.rs can call
    /// `record_sample` (the trait is read-only for the dashboard). Erased
    /// to `Arc<dyn StressmonApi>` at the point where DashboardState is
    /// built — dashboard stays leaf-of-boom-core that way.
    pub stressmon: Arc<boom_stressmon::StressmonCollector>,
}

/// The state that gets swapped on config reload.
/// Only contains config, auth, and health — deployments/aliases live in stores.
pub struct AppStateInner {
    pub config: Config,
    pub auth: Arc<dyn Authenticator>,
    /// Narrow view for Dashboard — only exposes key alias lookups, not full auth.
    pub key_alias_lookup: Arc<dyn KeyAliasLookup>,
    /// Loaded hook plugins. `None`-equivalent (empty registry) when `hooks`
    /// config block is absent or all entries disabled — the hot path then
    /// short-circuits with `is_empty()` and pays zero hook cost.
    pub hooks: Arc<crate::hooks::HookRegistry>,
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

        // Dashboard-only pool: max=3 so dashboard aggregations cannot starve
        // forwarding (which uses `db_pool` above with max=30). Same DSN, separate
        // connection set; the forwarding path never touches this pool.
        let dashboard_db_pool = match &config.general_settings.database_url {
            Some(url) => Some(
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(3)
                    .acquire_timeout(std::time::Duration::from_secs(10))
                    .idle_timeout(std::time::Duration::from_secs(600))
                    .max_lifetime(std::time::Duration::from_secs(1800))
                    .connect(url)
                    .await?,
            ),
            None => None,
        };

        // Audit log writer: dedicated 8-conn pool + single batch-INSERT task.
        // See `request_log::LogWriter` for architecture. Replaces the old
        // per-request `tokio::spawn + sqlx::query` pattern which competed
        // with auth/routing for the main pool's 30 connections and silently
        // dropped logs at high QPS (5s timeout fired more than warn! revealed).
        let log_writer = match &config.general_settings.database_url {
            Some(url) => {
                let log_pool = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(8)
                    .acquire_timeout(std::time::Duration::from_secs(10))
                    .idle_timeout(std::time::Duration::from_secs(600))
                    .max_lifetime(std::time::Duration::from_secs(1800))
                    .connect(url)
                    .await?;
                Some(crate::request_log::start_log_writer(log_pool))
            }
            None => None,
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

        // Rebalance move tracker survives across reloads (lifetime cumulative).
        let rebalance_move_tracker = Arc::new(RebalanceMoveTracker::new());
        let request_rate = Arc::new(RequestRateTracker::new());
        let agent_stats = Arc::new(AgentStatsTracker::new());

        // KV-cache index + tokenizer pool.
        // Driven by schedule_policy == "kvc_aware", not a separate enabled flag.
        // Built fresh on startup and on every reload (see reload()): any change
        // to kvc_aware settings rebuilds an empty trie, so the old trie is dropped.
        let kv_index_val = Self::build_kvc_subsystems(&config);

        // Create scheduling policy from config (may reference inflight, rebalance_move_tracker, kv_index).
        let policy = create_policy(&config, &inflight, &flow_controller, &rebalance_move_tracker, &kv_index_val);

        // Build hybrid router classifier (optional, content-based model routing).
        let hybrid_classifier = build_hybrid_router(&config);

        // Router wraps stores + policy + classifier for routing decisions.
        let router = Arc::new(Router::with_classifier(
            deployment_store.clone(),
            alias_store.clone(),
            policy,
            hybrid_classifier,
        ));

        // 5. Build from YAML first, then layer DB-only records on top.
        build_deployments_from_config(&config, &deployment_store);
        build_aliases_from_config(&config, &alias_store, &deployment_store);
        load_plans_from_config(&plan_store, &config);
        seed_flow_controller_from_config(&config, &flow_controller);

        if let Some(ref pool) = db_pool {
            // Run migrations. boom_dashboard::run_migrations calls
            // boom_audit::run_request_log_migration internally (with
            // lock_timeout set on its connection).
            if let Err(e) = boom_dashboard::migrations::run_migrations(pool).await {
                tracing::error!("Failed to run migrations: {}", e);
            } else {
                validate_db_workflow_namespace(pool, &config.workflow_settings).await?;
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
            plan_store.restore_team_assignments_from_db(pool).await;
            limiter.restore_counters_from_db(pool).await;
        }

        // 6. Build inner state (config + auth + health).
        let prompt_log_config = config.prompt_log.as_ref()
            .and_then(|v| serde_json::from_value::<boom_promptlog::PromptLogConfig>(v.clone()).ok())
            .unwrap_or_default();

        // Wire the OTLP exporter when the feature is on. The exporter is
        // always constructed at startup (cheap: just an HTTP client + queue),
        // and the runtime `otlp.enabled` toggle in YAML is consulted on every
        // entry in the background writer — so flipping the dashboard switch
        // off→on (or on→off) takes effect immediately without a restart.
        #[cfg(feature = "otlp")]
        let prompt_log_writer = {
            let exporter = boom_promptlog::OtelExporter::new(&prompt_log_config.otlp);
            exporter.spawn_flush_task();
            PromptLogWriter::spawn_with_otlp(prompt_log_config, exporter)
        };
        #[cfg(not(feature = "otlp"))]
        let prompt_log_writer = PromptLogWriter::spawn(prompt_log_config);

        // Read-only query handle: shares the writer's `Arc<ArcSwap<config>>`,
        // so dashboard sees hot-reloads without touching the writer directly.
        let prompt_log_query: Arc<dyn boom_promptlog::PromptLogQueryApi> = Arc::new(
            boom_promptlog::FilePromptLogQuery::new(prompt_log_writer.config_handle()),
        );

        let inner = Self::build_inner(config, &db_pool, chrono::Utc::now(), 0)?;

        // Single shared Arc<ArcSwap> for kv_index — AppState and
        // KvcOrchestrator MUST observe the same swap. reload() stores a fresh
        // empty trie into this ArcSwap; both holders get an Arc clone of the
        // SAME ArcSwap so the orchestrator sees the swap.
        let kv_index = Arc::new(ArcSwap::from_pointee(kv_index_val));
        let kvc_orchestrator = crate::kvc::KvcOrchestrator::new(
            kv_index.clone(),
            router.clone(),
        );

        // Stressmon collector — worker count is read off the live tokio
        // runtime (we're already inside `runtime.block_on`), then cached on
        // the collector so the dashboard snapshot doesn't need the runtime
        // handle. The 1 Hz sampler task is spawned from main.rs::async_main.
        let num_workers = tokio::runtime::Handle::current()
            .metrics()
            .num_workers()
            .max(1);
        let stressmon = boom_stressmon::StressmonCollector::new(num_workers);

        let state = Self {
            config_path,
            inner: Arc::new(ArcSwap::from_pointee(inner)),
            db_pool,
            dashboard_db_pool,
            log_writer,
            limiter,
            plan_store,
            deployment_store,
            alias_store,
            router,
            inflight,
            request_count: Arc::new(AtomicU64::new(0)),
            deployment_health: Arc::new(DeploymentHealthStore::new()),
            request_failure_counter: Arc::new(DashMap::new()),
            flow_controller: flow_controller.clone(),
            debug_store,
            prompt_log_writer,
            prompt_log_query,
            rebalance_move_tracker,
            request_rate,
            agent_stats,
            kv_index,
            kv_prune_handle: Arc::new(std::sync::Mutex::new(None)),
            kvc_orchestrator,
            stressmon,
        };
        state.register_fusion_models(&state.inner.load().config)?;
        Ok(state)
    }

    /// Hot-reload safety wrapper: bounds total wall time at 60s and converts
    /// panics into `Err`. The original reload logic lives in [`reload_inner`];
    /// this function exists so callers (SIGHUP listener, HTTP handler) always
    /// get a returned `Result` within bounded time, regardless of which step
    /// inside fails, hangs, or panics.
    ///
    /// On `Err`: the previous `inner` config still routes traffic — only the
    /// in-memory stores may be partially rebuilt. The atomic swap at the end
    /// of `reload_inner` is what would have committed the new state; an early
    /// `Err` skips that swap, so the OLD config object stays live. See
    /// `reload_inner` doc for the partial-mutation caveat.
    pub async fn reload(&self) -> anyhow::Result<String> {
        use futures::future::FutureExt;
        use std::panic::AssertUnwindSafe;

        let started = std::time::Instant::now();
        let inner = AssertUnwindSafe(self.reload_inner()).catch_unwind();
        match tokio::time::timeout(std::time::Duration::from_secs(60), inner).await {
            // Outer Ok = timeout didn't fire.
            // Middle Ok = no panic (catch_unwind recovered).
            // Inner Ok = reload_inner returned success.
            Ok(Ok(Ok(summary))) => Ok(summary),
            Ok(Ok(Err(e))) => {
                // reload_inner returned Err — pass through with elapsed log.
                tracing::error!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %e,
                    "reload aborted: step returned Err"
                );
                Err(e)
            }
            Ok(Err(panic_payload)) => {
                let msg = panic_payload
                    .downcast_ref::<String>()
                    .map(|s| s.clone())
                    .or_else(|| panic_payload.downcast_ref::<&'static str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "(non-string panic payload)".to_string());
                tracing::error!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    panic = %msg,
                    "reload aborted: panic captured"
                );
                Err(anyhow::anyhow!(
                    "reload aborted: panic captured ({}). \
                     Previous config still active; stores may be partially rebuilt.",
                    msg
                ))
            }
            Err(_elapsed) => {
                tracing::error!(
                    elapsed_secs = started.elapsed().as_secs(),
                    "reload aborted: total timeout 60s exceeded"
                );
                Err(anyhow::anyhow!(
                    "reload aborted: timed out after 60s. \
                     Previous config still active; stores may be partially rebuilt."
                ))
            }
        }
    }

    /// Hot-reload implementation. See [`reload`] for the safety wrapper.
    ///
    /// Unified YAML-priority flow (same as startup, minus DB reconnection):
    ///   1. Rebuild deployments/aliases/plans from YAML → memory stores
    ///   2. sync_yaml_to_db() → persist YAML to DB, handle conflicts
    ///   3. load_db_only_*() → load source='db' records on top
    ///   4. Clean up orphaned assignments
    ///
    /// Failure window: once step 4's `deployment_store.clear()` runs, the
    /// stores are mutable targets. Any later failure leaves them in a
    /// partially-built state while the old `inner` config still routes.
    /// Wrapping with timeout + catch_unwind (in `reload`) bounds *how long*
    /// this can take but does NOT roll back partial mutations — that requires
    /// shadow-build + atomic swap (tracked as TODO).
    async fn reload_inner(&self) -> anyhow::Result<String> {
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
        if let Some(ref pool) = db_pool {
            validate_db_workflow_namespace(pool, &new_config.workflow_settings).await?;
        }

        let new_reload_count = old_reload_count + 1;

        // 4. Rebuild stores: YAML first, then DB-only on top.
        self.deployment_store.clear();
        build_deployments_from_config(&new_config, &self.deployment_store);

        self.alias_store.clear();
        build_aliases_from_config(&new_config, &self.alias_store, &self.deployment_store);

        self.plan_store.clear_plans();
        load_plans_from_config(&self.plan_store, &new_config);
        seed_flow_controller_from_config(&new_config, &self.flow_controller);

        // Rebuild KV-cache subsystem (index + tokenizer pool + subscriber)
        // ONLY when the kvc-relevant config actually changed. The trie is a
        // learned cache of vLLM block events; rebuilding wipes it empty, which
        // forces every subsequent request into full_report + lowest-load until
        // the trie refills. A reload that only touched models/limits/plans must
        // not pay that cost. (CLAUDE.md documents kv_index as a separate
        // lifecycle — this gate narrows "any reload" to "kvc config change".)
        let old_router = self.inner.load().config.router_settings.clone();
        // Signature of kvc-relevant config that requires a trie rebuild.
        //   schedule_policy  → enables/disables kvc_aware entirely.
        //   block_size       → changes the hash algorithm (all entries invalid).
        //   max_blocks       → LRU capacity; LruCache can't resize in-place.
        //   router_ttl_secs  → prune task TTL/interval, fixed at spawn time.
        // Excluded (hot-updatable via policy recreate, no trie wipe):
        //   cache_weight / load_weight / overload_threshold_pct / rebalance_threshold.
        // Only schedule_policy and block_size require a trie wipe (hash algo
        // changes invalidate all entries). max_blocks / router_ttl_secs don't
        // affect existing hashes — the prune task is restarted below to pick
        // up the new TTL, and the LRU capacity adapts on the next batch.
        let kvc_sig = |r: &boom_config::RouterSettings| {
            let k = &r.kvc_aware;
            (
                r.schedule_policy.clone(),
                k.block_size,
            )
        };
        if kvc_sig(&old_router) == kvc_sig(&new_config.router_settings) {
            tracing::info!("KV-aware subsystem unchanged — preserving learned trie (no rebuild)");
            // Restart prune task to pick up new router_ttl_secs (cheap: abort + spawn)
            self.stop_kv_prune_task();
            if new_config.router_settings.schedule_policy == "kvc_aware" {
                self.spawn_kv_prune_task(&new_config);
            }
        } else {
            let new_kvc_enabled = new_config.router_settings.schedule_policy == "kvc_aware";
            let old_kvc_enabled = self.kv_index.load().is_some();
            tracing::info!(
                before = old_kvc_enabled,
                after = new_kvc_enabled,
                "KV-aware config changed: rebuilding index (trie will be empty)"
            );
            self.stop_kv_prune_task();
            let new_kv_index = Self::build_kvc_subsystems(&new_config);
            self.kv_index.store(Arc::new(new_kv_index));
            if new_kvc_enabled {
                self.spawn_kv_prune_task(&new_config);
            }
        }

        // Recreate policy (fresh counters etc.) — router reuses same stores.
        // Policy reads the (possibly just-swapped) kv_index via the ArcSwap.
        let new_policy = create_policy(
            &new_config,
            &self.inflight,
            &self.flow_controller,
            &self.rebalance_move_tracker,
            &**self.kv_index.load(),
        );
        self.router.set_policy(new_policy);

        // Rebuild hybrid router classifier.
        self.router.set_classifier(build_hybrid_router(&new_config));

        if let Some(ref pool) = db_pool {
            // Sync YAML config to DB (upsert source='yaml', handle conflicts).
            // Errors are non-fatal — log and continue. The follow-up
            // load_db_only_* steps still need to run against whatever DB
            // state we have. Timeout bounds DB hangs from blocking reload.
            if let Err(e) = with_db_timeout(
                "sync_yaml_to_db",
                sync_yaml_to_db(pool, &new_config, &self.plan_store),
            ).await {
                tracing::error!("Failed to sync YAML to DB: {}", e);
            }

            // Load source='db' records on top of YAML-built stores. Each step
            // is independently timeout-bounded; failure aborts the reload
            // (surfaces via reload's overall Err return), but the previous
            // inner config still routes.
            with_db_timeout_void(
                "load_db_only_deployments",
                load_db_only_deployments(pool, &self.deployment_store, &self.flow_controller),
            ).await?;
            with_db_timeout_void(
                "load_db_only_aliases",
                load_db_only_aliases(pool, &self.alias_store),
            ).await?;
            with_db_timeout_void(
                "load_db_only_plans",
                self.plan_store.load_db_only_plans(pool),
            ).await?;
        }

        self.register_fusion_models(&new_config)?;

        // Clean up assignments pointing to plans that no longer exist.
        self.plan_store.cleanup_assignments();

        // 5. Update prompt log config (hot-reload) + hot-swap OTLP exporter
        //    when its sub-config changes. update_config covers the runtime
        //    fields read per-entry by the background writer (dir, max_size,
        //    enabled toggle, excluded_*, record_headers). replace_otlp
        //    rebuilds the exporter when endpoint/headers/batch/timeout/etc.
        //    differ — those are frozen in OtelExporter at construction time
        //    and only take effect via rebuild.
        if let Some(ref v) = new_config.prompt_log {
            if let Ok(pc) = serde_json::from_value::<boom_promptlog::PromptLogConfig>(v.clone()) {
                let old_otlp = self.prompt_log_writer.config().otlp;
                self.prompt_log_writer.update_config(pc.clone());
                if old_otlp != pc.otlp {
                    self.prompt_log_writer.replace_otlp(&pc.otlp).await;
                }
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

    /// Build the KV-cache index for the given config (self-contained — no
    /// tokenizer pool, no ZMQ subscriber).
    ///
    /// Returns `None` when `schedule_policy != "kvc_aware"`. Shared by startup
    /// (`from_config`) and every `reload()` so both paths build the subsystem
    /// identically. Any change to kvc_aware settings produces a fresh empty
    /// trie — the caller drops the old one.
    fn build_kvc_subsystems(config: &Config) -> Option<Arc<dyn KvIndexBackend>> {
        if config.router_settings.schedule_policy != "kvc_aware" {
            return None;
        }
        let kv_settings = &config.router_settings.kvc_aware;
        let index: Arc<dyn KvIndexBackend> = Arc::new(TokenPrefixIndex::new(
            kv_settings.block_size,
            kv_settings.cache_weight,
            kv_settings.load_weight,
            kv_settings.max_blocks,
        ));
        tracing::info!(
            block_size = kv_settings.block_size,
            cache_weight = kv_settings.cache_weight,
            load_weight = kv_settings.load_weight,
            max_blocks = kv_settings.max_blocks,
            router_ttl_secs = kv_settings.router_ttl_secs,
            "KV-aware routing enabled (self-contained byte-prefix affinity)"
        );
        Some(index)
    }

    /// Spawn the TTL prune task: periodically sweeps approximate-mode blocks
    /// older than `router_ttl_secs` (the trie is self-learned, so blocks have
    /// no real evict signal — expire by wall-clock). Records the handle so
    /// reload can abort it. The task loads the CURRENT kv_index each tick (via
    /// the shared ArcSwap) so it keeps pruning the live trie across reloads
    /// until aborted.
    pub fn spawn_kv_prune_task(&self, config: &Config) {
        if self.kv_index.load().is_none() {
            return;
        }
        let ttl_secs = config.router_settings.kvc_aware.router_ttl_secs;
        // 0 = TTL prune disabled: rely on LRU (max_blocks) alone. Skip spawning the sweeper
        // (also avoids Duration::from_secs_f64 on a non-positive value).
        if !(ttl_secs > 0.0) {
            tracing::info!("KV TTL prune disabled (router_ttl_secs=0), using LRU only");
            return;
        }
        let ttl = std::time::Duration::from_secs_f64(ttl_secs);
        // Sweep at half the TTL so a block lives at most ~ttl.
        let interval = std::time::Duration::from_secs_f64((ttl_secs / 2.0).max(5.0));
        let kv_index = self.kv_index.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // first tick is immediate
            loop {
                ticker.tick().await;
                let g = kv_index.load();
                if let Some(idx) = (**g).as_ref() {
                    idx.prune_expired(ttl);
                }
            }
        });
        *self.kv_prune_handle.lock().unwrap() = Some(handle);
        tracing::info!(ttl_secs, sweep_secs = interval.as_secs_f64(), "KV TTL prune task spawned");
    }

    /// Abort the running TTL prune task (if any). Called before respawning on reload.
    fn stop_kv_prune_task(&self) {
        if let Some(h) = self.kv_prune_handle.lock().unwrap().take() {
            h.abort();
        }
    }

    /// Build AppStateInner from config.
    fn build_inner(
        config: Config,
        db_pool: &Option<PgPool>,
        started_at: chrono::DateTime<chrono::Utc>,
        reload_count: u64,
    ) -> Result<AppStateInner, anyhow::Error> {
        // Build authenticator — store concrete type in Arc so we can derive both trait objects.
        let auth_impl = Arc::new(DbAuthenticator::new(
            db_pool.clone(),
            config.general_settings.master_key.clone(),
        ));
        let auth: Arc<dyn Authenticator> = auth_impl.clone();
        let key_alias_lookup: Arc<dyn KeyAliasLookup> = auth_impl;

        let health = HealthStatus {
            started_at,
            last_reload_at: chrono::Utc::now(),
            db_connected: db_pool.is_some(),
            reload_count,
        };

        let hooks = Arc::new(crate::hooks::HookRegistry::from_config(&config.hooks)?);

        Ok(AppStateInner {
            config,
            auth,
            key_alias_lookup,
            hooks,
            health,
        })
    }

    fn register_fusion_models(&self, config: &Config) -> Result<(), boom_core::GatewayError> {
        let runtime = FusionRuntime::new(
            Arc::downgrade(&self.router),
            self.deployment_store.clone(),
            self.flow_controller.clone(),
            self.inflight.clone(),
            self.request_rate.clone(),
            self.kv_index.clone(),
            config.router_settings.enable_priority_header,
            config
                .router_settings
                .flow_control_queue_timeout_secs(),
        );
        register_fusion_providers(
            &config.workflow_settings,
            &self.deployment_store,
            &self.alias_store,
            runtime,
        )
    }

    /// Persist current runtime model/alias/plan state to the live `config.yaml`.
    ///
    /// v3 design: web edits mutate the live config file in place, replacing
    /// the old "write to timestamped backup file" behavior. The live YAML is
    /// the single source of truth — DB tables are runtime indexes, not the
    /// authority.
    ///
    /// Reads the raw YAML (preserving `${VAR}` references in untouched
    /// sections — `load_config` resolves env vars at parse time, so dumping
    /// the typed Config would leak real secrets to disk). Updates only the
    /// sections owned by runtime tables:
    ///   - `model_list` (from boom_model_deployment)
    ///   - `router_settings.model_group_alias` (from boom_model_alias)
    ///   - `plan_settings.plans` and optionally `plan_settings.default_plan`
    ///     (from boom_rate_limit_plan)
    ///
    /// Other singleton sections (`server`, `router_settings.schedule_policy`,
    /// `general_settings`, etc.) are preserved verbatim. Then triggers a
    /// reload so the new config takes effect.
    ///
    /// Before writing, rolls a single `.bak` copy so a bad edit can be undone
    /// by hand (serde_yaml serialization drops comments, so the .bak is also
    /// the only record of pre-edit annotation).
    ///
    /// Returns `Err(message)` if any step fails so the caller can surface
    /// the failure to the user. The DB write that triggered this persist has
    /// already committed by the time we run, so a persist failure leaves a
    /// real divergence: DB has the new state, YAML/memory don't. The caller
    /// must NOT silently report success in that case — see
    /// [`admin_command_handler`] for the warning-augmented reply pattern.
    pub async fn persist_config_in_place(&self) -> Result<(), String> {
        let pool = match &self.db_pool {
            Some(p) => p,
            None => return Err("Database not available".to_string()),
        };

        backup_yaml(&self.config_path);

        let mut root: serde_yaml::Value = boom_config::read_raw_yaml(&self.config_path)
            .map_err(|e| format!("read config: {}", e))?;

        let snapshot = build_config_snapshot_value(pool)
            .await
            .map_err(|e| format!("build snapshot from DB: {}", e))?;

        merge_runtime_sections(&mut root, &snapshot)
            .map_err(|e| format!("merge runtime sections: {}", e))?;

        boom_config::write_yaml_atomic(&self.config_path, &root)
            .map_err(|e| format!("write config: {}", e))?;

        tracing::info!(path = %self.config_path, "Config persisted in place");

        // Reload after the write succeeded. A reload failure here is less
        // bad than a write failure (YAML is current; memory just lags and
        // the next manual reload picks it up), but still report it so the
        // user knows to retry.
        self.reload()
            .await
            .map_err(|e| format!("reload after persist: {}", e))?;

        Ok(())
    }

    /// Update a single config section in the live `config.yaml` and reload.
    ///
    /// `path` is dotted (`server`, `router_settings.kvc_aware`). Reads raw
    /// YAML, sets the path to `value` (converted from JSON), writes back
    /// atomically, then reloads.
    ///
    /// Returns a summary string on success or an error message. Used by the
    /// Config page's section editors.
    pub async fn update_config_section(
        &self,
        path: &str,
        value: serde_json::Value,
    ) -> Result<String, String> {
        let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return Err("Empty config path".to_string());
        }

        backup_yaml(&self.config_path);

        let mut root: serde_yaml::Value = boom_config::read_raw_yaml(&self.config_path)
            .map_err(|e| format!("Failed to read config: {}", e))?;

        let yaml_value = json_to_yaml(&value).map_err(|e| format!("JSON→YAML: {}", e))?;
        boom_config::set_yaml_path(&mut root, &segments, yaml_value)
            .map_err(|e| format!("Failed to set path: {}", e))?;

        boom_config::write_yaml_atomic(&self.config_path, &root)
            .map_err(|e| format!("Failed to write config: {}", e))?;

        tracing::info!(path = %path, "Config section updated");

        self.reload()
            .await
            .map_err(|e| format!("Saved but reload failed: {}", e))
    }
}

/// Merge runtime-derived sections (model_list, aliases, plans) from a JSON
/// snapshot into the raw YAML value. Singleton sections are preserved as-is.
fn merge_runtime_sections(
    root: &mut serde_yaml::Value,
    snapshot: &serde_json::Value,
) -> Result<(), String> {
    let obj = snapshot
        .as_object()
        .ok_or_else(|| "snapshot is not a JSON object".to_string())?;

    if let Some(model_list) = obj.get("model_list") {
        let yaml_val = json_to_yaml(model_list)?;
        boom_config::set_yaml_path(root, &["model_list"], yaml_val)
            .map_err(|e| format!("set model_list: {}", e))?;
    }

    if let Some(aliases) = obj.get("router_settings").and_then(|r| r.get("model_group_alias")) {
        let yaml_val = json_to_yaml(aliases)?;
        boom_config::set_yaml_path(root, &["router_settings", "model_group_alias"], yaml_val)
            .map_err(|e| format!("set model_group_alias: {}", e))?;
    }

    if let Some(plan_settings) = obj.get("plan_settings") {
        if let Some(plans) = plan_settings.get("plans") {
            let yaml_val = json_to_yaml(plans)?;
            boom_config::set_yaml_path(root, &["plan_settings", "plans"], yaml_val)
                .map_err(|e| format!("set plan_settings.plans: {}", e))?;
        }
        if let Some(default_plan) = plan_settings.get("default_plan") {
            let yaml_val = json_to_yaml(default_plan)?;
            boom_config::set_yaml_path(root, &["plan_settings", "default_plan"], yaml_val)
                .map_err(|e| format!("set plan_settings.default_plan: {}", e))?;
        }
    }

    Ok(())
}

/// Convert a `serde_json::Value` to `serde_yaml::Value` via string round-trip.
/// Both libraries speak the same data model, so this is lossless for our cases
/// (no inf/NaN, no numbers beyond u64).
fn json_to_yaml(value: &serde_json::Value) -> Result<serde_yaml::Value, String> {
    let s = serde_json::to_string(value).map_err(|e| format!("json serialize: {}", e))?;
    serde_yaml::from_str(&s).map_err(|e| format!("yaml deserialize: {}", e))
}

/// Copy `{path}` to `{path}.bak`, overwriting any prior backup. Single-slot
/// rolling backup — the previous .bak is lost, but no timestamped files
/// accumulate. Best-effort: a failed copy logs a warning but does not block
/// the write, since the in-memory state can still recover via reload.
fn backup_yaml(path: &str) {
    let bak = format!("{}.bak", path);
    match std::fs::copy(path, &bak) {
        Ok(_) => tracing::debug!(from = %path, to = %bak, "Config backed up"),
        Err(e) => tracing::warn!(from = %path, to = %bak, "Config backup failed: {}", e),
    }
}

// ═══════════════════════════════════════════════════════════
// YAML → DB sync (delegates to owning modules' Store methods)
// ═══════════════════════════════════════════════════════════

/// Sync YAML config to DB: replace source='yaml' rows, handle same-name conflicts.
///
/// Delegates SQL to owning modules (boom-routing, boom-limiter).
/// Only plan sync remains here (will move to boom-limiter in Phase 1b).
/// Wrap a Result-returning DB operation with a 15s timeout. Bounds each
/// `sync_yaml_to_db` call so a slow DB can't hang the entire reload future.
/// 15s leaves headroom for slow networks without letting a stuck query block
/// reload indefinitely.
async fn with_db_timeout<F, T>(op_name: &str, f: F) -> Result<T, anyhow::Error>
where
    F: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    match tokio::time::timeout(std::time::Duration::from_secs(15), f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(anyhow::anyhow!("{}: {}", op_name, e)),
        Err(_) => Err(anyhow::anyhow!("{} timed out after 15s", op_name)),
    }
}

/// Wrap a `()`-returning DB operation with a 15s timeout. Used for the
/// `load_db_only_*` helpers that swallow errors internally — we can't surface
/// their internal failures, but we can at least bound their wall time.
/// Returns `Err` only on timeout.
async fn with_db_timeout_void<F>(op_name: &str, f: F) -> Result<(), anyhow::Error>
where
    F: std::future::Future<Output = ()>,
{
    match tokio::time::timeout(std::time::Duration::from_secs(15), f).await {
        Ok(()) => Ok(()),
        Err(_) => Err(anyhow::anyhow!("{} timed out after 15s", op_name)),
    }
}

async fn sync_yaml_to_db(pool: &PgPool, config: &Config, plan_store: &Arc<PlanStore>) -> Result<(), sqlx::Error> {
    // ── Deployments (delegated to DeploymentStore) ──
    let yaml_model_names: Vec<String> = config.model_list.iter()
        .map(|e| e.model_name.clone()).collect();
    let mut yaml_deployments: Vec<boom_routing::DeploymentInput> = Vec::new();
    for entry in &config.model_list {
        let p = &entry.litellm_params;
        let d = boom_routing::DeploymentInput {
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
            quota_count_ratio: entry.model_info.as_ref()
                .and_then(|mi| mi.quota_count_ratio)
                .map(|v| v as i64)
                .unwrap_or(1),
            max_inflight_queue_len: entry.flow_control.as_ref()
                .and_then(|fc| fc.model_queue_limit).map(|v| v as i32),
            max_context_len: entry.flow_control.as_ref()
                .and_then(|fc| fc.model_context_limit).map(|v| v as i64),
            enabled: entry.enabled,
            client_type_header: entry.client_type_header,
            serve_not_match: entry.serve_not_match,
            model_info: entry.model_info.as_ref().map(|mi| {
                serde_json::to_value(mi).unwrap_or(serde_json::Value::Null)
            }),
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
            row.client_type_header.unwrap_or(false),
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

async fn validate_db_workflow_namespace(
    pool: &PgPool,
    settings: &boom_config::WorkflowSettings,
) -> anyhow::Result<()> {
    let workflow_models = settings.models.keys().cloned().collect::<Vec<_>>();
    if workflow_models.is_empty() {
        return Ok(());
    }

    let deployment_conflicts =
        DeploymentStore::db_only_model_conflicts(pool, &workflow_models).await?;
    let alias_conflicts = AliasStore::db_only_name_conflicts(pool, &workflow_models).await?;
    if deployment_conflicts.is_empty() && alias_conflicts.is_empty() {
        return Ok(());
    }

    Err(anyhow::anyhow!(
        "workflow model namespace conflicts with DB-only resources: deployments=[{}], aliases=[{}]",
        deployment_conflicts.join(", "),
        alias_conflicts.join(", ")
    ))
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
            entry.client_type_header,
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

                // Per-model cost rates for billing/quota accounting.
                // Pricing is sourced exclusively from `cost_templates` —
                // model_info no longer carries inline cost fields (v3 single-
                // source-of-truth refactor). The model_info.cost_template name
                // selects which template applies; missing template = no rate.
                //
                // YAML rates are CNY per million tokens (e.g. `0.27` = ¥0.27/1M
                // tokens). Convert to per-token Decimal internally for accurate
                // accounting on small requests.
                if let Some(info) = entry.model_info.as_ref() {
                    use rust_decimal::prelude::FromPrimitive;
                    let per_million_to_per_token = |v: Option<f64>| -> Option<rust_decimal::Decimal> {
                        v.and_then(rust_decimal::Decimal::from_f64)
                            .map(|d| d / rust_decimal::Decimal::from(1_000_000))
                    };

                    // Template lookup is the only source of rates now.
                    let template_rates = info.cost_template.as_ref().and_then(|tn| {
                        config.lookup_cost_template(tn).map(|t| {
                            (
                                t.input_cost_per_million_tokens,
                                t.cached_input_cost_per_million_tokens,
                                t.output_cost_per_million_tokens,
                            )
                        })
                    });

                    if let (Some(ref tn), None) = (&info.cost_template, &template_rates) {
                        tracing::warn!(
                            model = %entry.model_name,
                            template = %tn,
                            "cost_template not found in cost_templates — no rate registered"
                        );
                    }

                    if let Some((src_input, src_cached, src_output)) = template_rates {
                        let input_rate = per_million_to_per_token(src_input);
                        let cached_rate = per_million_to_per_token(src_cached);
                        let output_rate = per_million_to_per_token(src_output);

                        if input_rate.is_some() || output_rate.is_some() || cached_rate.is_some() {
                            let rate = boom_routing::ModelCostRate::with_cached(
                                input_rate.unwrap_or_default(),
                                cached_rate.unwrap_or_default(),
                                output_rate.unwrap_or_default(),
                            );
                            deployment_store.set_cost_rate(&entry.model_name, rate);
                            tracing::info!(
                                model = %entry.model_name,
                                template = ?info.cost_template,
                                input_per_1m = src_input.unwrap_or(0.0),
                                cached_per_1m = src_cached.unwrap_or(0.0),
                                output_per_1m = src_output.unwrap_or(0.0),
                                "Registered cost rate (CNY per million tokens)"
                            );
                        }
                    }
                }
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
        // `window_limits` is already a multi-dim `Vec<WindowLimit>` in the
        // config struct — pass through as-is. A plan is a generic template;
        // `type` only gates which entity it may be assigned to.
        let plan = RateLimitPlan {
            name: name.clone(),
            r#type: pc.r#type,
            member_plan: pc.member_plan.clone(),
            concurrency_limit: pc.concurrency_limit,
            rpm_limit: pc.rpm_limit,
            tpm_limit: pc.tpm_limit,
            window_limits: pc.window_limits.clone(),
            total_token_limit: pc.total_token_limit,
            total_cost_limit: pc.total_cost_limit,
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

    match &config.plan_settings.default_team_plan {
        Some(dtp) => {
            if let Some(plan) = plan_store.get_plan(dtp) {
                if plan.r#type == boom_core::types::PlanType::Team {
                    plan_store.set_default_team_plan(Some(dtp.clone()));
                    tracing::info!(default_team_plan = %dtp, "Default team plan set");
                } else {
                    tracing::warn!(
                        default_team_plan = %dtp,
                        "default_team_plan '{}' is not type=team, ignoring",
                        dtp
                    );
                    plan_store.set_default_team_plan(None);
                }
            } else {
                tracing::warn!(
                    default_team_plan = %dtp,
                    "default_team_plan '{}' not found in configured plans, ignoring",
                    dtp
                );
                plan_store.set_default_team_plan(None);
            }
        }
        None => {
            plan_store.set_default_team_plan(None);
            tracing::info!("没有默认 team 套餐配置，未显式分配 plan 的 team 将不受 team 维度限制。");
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════

/// Build a HybridRouter from config, if hybrid_router section is present.
fn build_hybrid_router(config: &Config) -> Option<Arc<HybridRouter>> {
    let hr_config = config.router_settings.hybrid_router.as_ref()?;

    let mut registry = StrategyRegistry::new();
    registry.register(Arc::new(TierClassifier));
    if let Some(ml) = &hr_config.ml_service {
        tracing::info!(
            url = %ml.url,
            timeout_ms = ml.timeout_ms,
            "Registering ML service classification strategy"
        );
        let valid_tiers: std::collections::HashSet<String> =
            hr_config.tiers.keys().cloned().collect();
        registry.register(Arc::new(MlServiceClient::new(
            &ml.url,
            ml.timeout_ms,
            valid_tiers,
        )));
    }

    let strategy = match registry.get(&hr_config.strategy) {
        Some(s) => s.clone(),
        None => {
            tracing::error!(
                strategy = %hr_config.strategy,
                "Unknown hybrid_router strategy, disabling hybrid router"
            );
            return None;
        }
    };

    let tiers: std::collections::HashMap<String, String> = hr_config
        .tiers
        .iter()
        .map(|(name, tier)| (name.clone(), tier.target_model.clone()))
        .collect();

    tracing::info!(
        model_name = %hr_config.model_name,
        strategy = %hr_config.strategy,
        default_tier = %hr_config.default_tier,
        tiers = ?tiers,
        "Hybrid router enabled"
    );

    Some(Arc::new(HybridRouter::new(
        hr_config.model_name.clone(),
        strategy,
        hr_config.default_tier.clone(),
        tiers,
    )))
}

/// Create a scheduling policy from config.
fn create_policy(
    config: &Config,
    inflight: &Arc<InFlightTracker>,
    flow_controller: &Arc<FlowController>,
    rebalance_move_tracker: &Arc<RebalanceMoveTracker>,
    kv_index: &Option<Arc<dyn KvIndexBackend>>,
) -> Arc<dyn SchedulePolicy> {
    match config.router_settings.schedule_policy.as_str() {
        "round_robin" | "" => Arc::new(RoundRobinPolicy::new()),
        "key_affinity" => {
            let ctx_threshold = config.router_settings.key_affinity_context_threshold;
            let rebalance_threshold = config.router_settings.rebalance_threshold;
            tracing::info!(
                "Using key_affinity policy: context_threshold={}, rebalance_threshold={}",
                ctx_threshold,
                rebalance_threshold,
            );
            let mut policy = KeyAffinityPolicy::new(
                inflight.clone(),
                ctx_threshold,
                rebalance_threshold,
                Some(rebalance_move_tracker.clone()),
            );
            policy.set_queue_info(flow_controller.clone());
            Arc::new(policy)
        }
        "kvc_aware" => {
            let kv = match kv_index {
                Some(idx) => idx.clone(),
                None => {
                    tracing::warn!("kvc_aware policy selected but kv_index not initialized — falling back to round_robin");
                    return Arc::new(RoundRobinPolicy::new());
                }
            };
            tracing::info!("Using kvc_aware policy (self-contained)");
            let mut policy = boom_routing::KvcAwarePolicy::new(
                kv,
                inflight.clone(),
                Some(rebalance_move_tracker.clone()),
            );
            policy.set_queue_info(flow_controller.clone());
            // Unified-score weights + hard overload gate. No key_affinity
            // fallback (the trie self-learns from routed requests); no
            // KV-sharing groups (PD topology removed).
            let kvc = &config.router_settings.kvc_aware;
            policy.set_scoring(
                kvc.cache_weight,
                kvc.load_weight,
                kvc.overload_threshold_pct,
                config.router_settings.rebalance_threshold as u64,
            );
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
            tpm_limit: s.tpm_limit,
            window_limits: s.window_limits.clone(),
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════
// Config snapshot (DB → YAML)
// ═══════════════════════════════════════════════════════════

/// Build a serde_json::Value representing the current runtime config
/// Normalize raw JSONB `window_limits` into the canonical object form that
/// `WindowLimit`'s Serialize derive emits and the untagged-enum Helper can
/// re-parse.
///
/// Why: `plan_settings.plans.X.window_limits` is stored in DB as JSONB. Old
/// rows (or hand-edited SQL) can carry shapes the Helper at
/// `boom_core::types::deserialize_window_limit_vec` no longer accepts — e.g.
/// legacy 2-element tuples, objects missing `window_secs`, or wrong field
/// names. The previous dump path cloned such entries verbatim into YAML,
/// which then failed reload with "did not match any variant of untagged enum
/// Helper". This function runs every entry through the same Helper so the
/// same schema is enforced on the way OUT of the DB as on the way IN — any
/// shape that wouldn't survive a YAML round-trip is dropped here instead of
/// breaking reload later. Mirrors how `model_info` already routes through
/// `ModelInfo` for the same reason.
fn normalize_window_limits(raw: &serde_json::Value) -> Vec<serde_json::Value> {
    #[derive(serde::Deserialize)]
    struct WindowLimitsWrapper {
        #[serde(deserialize_with = "boom_core::types::deserialize_window_limit_vec")]
        inner: Vec<boom_core::types::WindowLimit>,
    }

    let wrapper = serde_json::from_value::<WindowLimitsWrapper>(serde_json::json!({ "inner": raw }));
    match wrapper {
        Ok(w) => w
            .inner
            .iter()
            .filter_map(|wl| serde_json::to_value(wl).ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

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
            // NOTE: max_inflight_queue_len / max_context_len are deliberately
            // NOT placed under litellm_params — ProviderParams doesn't have
            // those fields, so serde would silently drop them on reload,
            // breaking the flow control wiring that auto-generated
            // deployment_id was meant to enable. They're emitted under
            // entry.flow_control below.
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

            // ── flow_control: emit as a dedicated node. Include only when
            // at least one limit is set so we don't litter YAML with empty
            // `flow_control: {}` blocks on every row.
            if r.max_inflight_queue_len.is_some() || r.max_context_len.is_some() {
                let mut fc = serde_json::Map::new();
                if let Some(v) = r.max_inflight_queue_len {
                    fc.insert("model_queue_limit".into(), serde_json::Value::Number(v.into()));
                }
                if let Some(v) = r.max_context_len {
                    fc.insert("model_context_limit".into(), serde_json::Value::Number(v.into()));
                }
                entry.as_object_mut().unwrap().insert(
                    "flow_control".into(),
                    serde_json::Value::Object(fc),
                );
            }

            // ── model_info: deserialize DB JSONB through the ModelInfo schema
            // so unknown fields (including the v3-removed inline cost fields)
            // are dropped automatically — the schema is the whitelist, no
            // hardcoded blacklist to keep in sync. Then layer in the
            // canonical `id` (deployment_id column) and quota_count_ratio.
            // Emit the node only when at least one piece is present so legacy
            // rows without metadata stay clean.
            let mut mi = serde_json::Map::new();
            if let Some(info) = r
                .model_info
                .as_ref()
                .and_then(|v| serde_json::from_value::<boom_config::ModelInfo>(v.clone()).ok())
            {
                if let Some(ct) = info.cost_template {
                    mi.insert("cost_template".into(), serde_json::Value::String(ct));
                }
            }
            if let Some(ref did) = r.deployment_id {
                if !did.is_empty() {
                    mi.insert("id".into(), serde_json::Value::String(did.clone()));
                }
            }
            // quota_count_ratio lives in its own DB column but YAML schema
            // wants it under model_info. Emit only when != 1 (default) to
            // match how a hand-written YAML would look.
            if let Some(ratio) = r.quota_count_ratio {
                if ratio != 1 {
                    mi.insert(
                        "quota_count_ratio".into(),
                        serde_json::Value::Number(ratio.into()),
                    );
                }
            }
            if !mi.is_empty() {
                entry.as_object_mut().unwrap().insert(
                    "model_info".into(),
                    serde_json::Value::Object(mi),
                );
            }

            // ── Behavior toggles. serde defaults are false, so emit only
            // when true to keep YAML noise-free.
            if r.serve_not_match {
                entry.as_object_mut().unwrap().insert(
                    "serve_not_match".into(),
                    serde_json::Value::Bool(true),
                );
            }
            if r.client_type_header.unwrap_or(false) {
                entry.as_object_mut().unwrap().insert(
                    "client_type_header".into(),
                    serde_json::Value::Bool(true),
                );
            }

            // ── Enabled flag. Unlike the toggles above, ModelEntry.enabled
            // has serde default true, so emit only when false — omitting it
            // would make reload fall back to the default and silently lose
            // the disabled state (the very bug we're fixing).
            if !r.enabled.unwrap_or(true) {
                entry.as_object_mut().unwrap().insert(
                    "enabled".into(),
                    serde_json::Value::Bool(false),
                );
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

        let window_limits = normalize_window_limits(&r.window_limits);

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
                        if let Some(raw_wl) = obj.get("window_limits") {
                            let normalized = normalize_window_limits(raw_wl);
                            if !normalized.is_empty() {
                                slot.insert("window_limits".into(), serde_json::Value::Array(normalized));
                            }
                        }
                        Some(serde_json::Value::Object(slot))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut plan_obj = serde_json::Map::new();
        let type_str = r.r#type.as_deref().unwrap_or("key");
        plan_obj.insert("type".into(), serde_json::Value::String(type_str.to_string()));
        if let Some(mp) = &r.member_plan {
            plan_obj.insert("member_plan".into(), serde_json::Value::String(mp.clone()));
        }
        if let Some(cl) = r.concurrency_limit {
            plan_obj.insert("concurrency_limit".into(), serde_json::Value::Number(cl.into()));
        }
        if let Some(rpm) = r.rpm_limit {
            plan_obj.insert("rpm_limit".into(), serde_json::Value::Number(rpm.into()));
        }
        if let Some(tpm) = r.tpm_limit {
            plan_obj.insert("tpm_limit".into(), serde_json::Value::Number(tpm.into()));
        }
        if !window_limits.is_empty() {
            plan_obj.insert("window_limits".into(), serde_json::Value::Array(window_limits));
        }
        if let Some(tok) = r.total_token_limit {
            plan_obj.insert("total_token_limit".into(), serde_json::Value::Number(tok.into()));
        }
        if let Some(cost_micros) = r.total_cost_limit_micros {
            let cost = rust_decimal::Decimal::from(cost_micros.max(0))
                / rust_decimal::Decimal::from(1_000_000);
            plan_obj.insert(
                "total_cost_limit".into(),
                serde_json::Value::String(cost.to_string()),
            );
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
