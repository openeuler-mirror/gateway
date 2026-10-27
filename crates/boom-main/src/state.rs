use arc_swap::ArcSwap;
use boom_auth::DbAuthenticator;
use boom_config::Config;
use boom_core::provider::{Authenticator, KeyAliasLookup};
use boom_core::DebugErrorStore;
use boom_kvindex::{TokenPrefixIndex};
use boom_core::kv_event::KvIndexBackend;
use boom_limiter::{PlanStore, RateLimitPlan, ScheduleSlot, SlidingWindowLimiter};
use boom_flowcontrol::{FlowControlConfig, FlowController};
use boom_routing::{register_fusion_providers, AliasStore, CostTemplateStore, DeploymentStore, FusionRuntime, AutoRouter, InFlightTracker, KeyAffinityPolicy, KvcSchedulerStats, MlServiceClient, RebalanceMoveTracker, RequestRateTracker, Router, RoundRobinPolicy, SchedulePolicy, ShufflePolicy, StrategyRegistry, TierClassifier, VisibilityState, WorkflowStore, parse_allowed_teams};
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
    /// Config file path (stored for reload). `None` = env-bootstrap mode:
    /// no YAML file exists; process config came from
    /// `boom_config::bootstrap_from_env` and stays fixed across reloads
    /// (SIGHUP still rebuilds policy/classifier and reconciles DB business
    /// config). The write-back button generates `./config.yaml` from this
    /// state; a restart with that file adopts file mode.
    pub config_path: Option<String>,
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
    /// Workflow definition + binding store survives reloads. Content is
    /// DB-authoritative (DB mode) or YAML-seeded (no-DB mode); fusion
    /// registration consumes its `settings()` snapshot.
    pub workflow_store: Arc<WorkflowStore>,
    /// Cost template store survives reloads. Resolves
    /// `model_info.cost_template` references for rate derivation — never the
    /// YAML config, which is only the bootstrap seed.
    pub cost_template_store: Arc<CostTemplateStore>,
    /// Client blocklist store (boom-gatekeeper) survives reloads. Rules are
    /// rebuilt from YAML + DB on reload; dashboard CRUD updates it in place.
    pub block_rule_store: Arc<boom_gatekeeper::BlockRuleStore>,
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
    /// Global kvc_aware scheduling counters — total / affinity / cold /
    /// lowest-load fallback / rebalance (debug page; survives reloads).
    /// The policy is recreated on every reload, so the counters must live here.
    pub kvc_stats: Arc<KvcSchedulerStats>,
    /// Per-deployment request rate tracker (survives reloads).
    pub request_rate: Arc<RequestRateTracker>,
    /// Agent (client-type) statistics tracker (survives reloads).
    pub agent_stats: Arc<AgentStatsTracker>,
    /// KV-cache prefix index, hot-swappable across reloads.
    ///
    /// THIRD lifecycle (distinct from AppState's other fields): unlike
    /// deployment_store / plan_store / limiter — which survive reloads with
    /// their contents intact — this is rebuilt EMPTY on every reload. Any
    /// kvc_aware config change (policy switch) swaps in a fresh
    /// index; the old trie is dropped and the new one starts empty, repopulated
    /// by the orchestrator recording routed requests (self-contained learning —
    /// no ZMQ subscriber). The transient moment (queries hit an empty trie →
    /// 0 hit → route by load) is intentional ("rebuild = clear cache"). `None`
    /// when kvc_aware is disabled. See `reload()`.
    pub kv_index: Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
    /// Handle to the running TTL prune task (sweeps expired approximate blocks).
    /// None when kvc_aware disabled. Held so reload can abort before respawning.
    pub kv_prune_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Response store for /v1/responses continuation (previous_response_id).
    pub response_store: Arc<boom_responses::ResponseStore>,
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
    /// OTel traces registry — active span table + slow ring + OTLP traces
    /// exporter. Survives reloads (top-level Arc, CLAUDE.md §4). The exporter
    /// is hot-swapped via `replace_otlp` when `trace.otlp` config changes.
    /// Dashboard reads through the `TraceApi` trait (boom-core) so
    /// boom-dashboard doesn't depend on boom-trace.
    pub trace: Arc<boom_trace::TraceRegistry>,
    /// Alert manager — active alerts + history ring. Survives reloads
    /// (top-level Arc, CLAUDE.md §4). Populated by the alert reconciler task
    /// (boom-main) which re-derives alert state each cycle from the
    /// deployment store's `auto_disabled` rows and the trace registry's OTLP
    /// exporter status; health-monitor auto-disable/enable hooks raise/clear
    /// immediately for low latency.
    pub alerts: Arc<boom_alert::AlertManager>,
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

/// KV prefix-trie chunk granularity, in BYTES. A code constant, NOT a config
/// knob: tiny blocks (e.g. 1B) turn every long request into ~100k trie nodes,
/// starving the runtime (DB pool timeouts observed) and fragmenting the
/// allocator (~1.4G RSS pinned above live heap). 512B ≈ 128-token granularity.
const KV_BLOCK_SIZE: usize = 512;

impl AppState {
    /// Build state from config. Called once at startup.
    ///
    /// Business-config sourcing depends on the deployment mode:
    ///   - DB mode: run migrations, seed the DB from YAML runtime sections
    ///     when the tables are fresh (marker-guarded, once ever), then build
    ///     the stores entirely from the DB. YAML keeps only process-level
    ///     sections (server, router policy, prompt log, trace, hooks).
    ///   - No-DB mode (no database_url): build the stores from YAML — the
    ///     Kong dbless-style deployment choice, not a dual-authority merge.
    ///
    /// Runtime state (assignments, counters) is restored on top in both modes.
    pub async fn from_config(config: Config, config_path: Option<String>) -> anyhow::Result<Self> {
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

        // 4a. Workflow + cost template stores survive across reloads. Same
        // lifecycle as the stores above: DB-authoritative content in DB mode,
        // YAML-seeded in no-DB mode.
        let workflow_store = Arc::new(WorkflowStore::new());
        let cost_template_store = Arc::new(CostTemplateStore::new());

        // 4b. Client blocklist store survives across reloads.
        let block_rule_store = Arc::new(boom_gatekeeper::BlockRuleStore::new());

        // In-flight tracker survives across reloads — must be created before policy.
        let inflight = Arc::new(InFlightTracker::new());

        // Flow controller survives across reloads.
        let flow_controller = Arc::new(FlowController::new());

        // Debug error store survives across reloads.
        let debug_store = Arc::new(DebugErrorStore::new());

        // Rebalance move tracker survives across reloads (lifetime cumulative).
        let rebalance_move_tracker = Arc::new(RebalanceMoveTracker::new());
        // Global kvc scheduling counters survive across reloads (the policy is
        // recreated on every reload — per-policy counters would reset).
        let kvc_stats = Arc::new(KvcSchedulerStats::new());
        let request_rate = Arc::new(RequestRateTracker::new());
        let agent_stats = Arc::new(AgentStatsTracker::new());

        // KV-cache index + tokenizer pool.
        // Driven by schedule_policy == "kvc_aware", not a separate enabled flag.
        // Built fresh on startup and on every reload (see reload()): any change
        // to kvc_aware settings rebuilds an empty trie, so the old trie is dropped.
        let kv_index_val = Self::build_kvc_subsystems(&config);

        // Create scheduling policy from config (may reference inflight, rebalance_move_tracker, kv_index).
        let policy = create_policy(&config, &inflight, &flow_controller, &rebalance_move_tracker, &kvc_stats, &kv_index_val);

        // Build auto router classifier (optional, content-based model routing).
        let auto_classifier = build_auto_router(&config);

        // Router wraps stores + policy + classifier for routing decisions.
        let router = Arc::new(Router::with_classifier(
            deployment_store.clone(),
            alias_store.clone(),
            policy,
            auto_classifier,
        ));

        // 5. Build stores. With a DB, the DB is the single runtime authority
        // for business config (models, aliases, plans, block rules): seed
        // YAML runtime sections once into empty tables (fresh install), then
        // load everything from the DB. YAML runtime sections are inert from
        // then on. Without a DB, YAML remains the build source (master-key-
        // only mode — a deployment form choice, not a second authority).
        if let Some(ref pool) = db_pool {
            // Run migrations. boom_dashboard::run_migrations calls
            // boom_audit::run_request_log_migration internally (with
            // lock_timeout set on its connection).
            //
            // Failures are fatal: with the DB as the runtime authority,
            // seed/reconcile below hard-depends on every table existing.
            // Swallowing the error here would only resurface it later as an
            // unrelated "relation does not exist" from the seed. Optional
            // DDL (lock_timeout, DISTRIBUTE BY on GaussDB) already degrades
            // to warn inside run_migrations.
            boom_dashboard::migrations::run_migrations(pool).await?;
            // Response store DDL (boom-responses owns boom_response* tables).
            if let Err(e) = boom_responses::migrations::run(pool).await {
                tracing::error!("Failed to run responses-store migration: {}", e);
            }

            crate::migrations::ensure_config_meta_table(pool).await?;

            let seed = crate::config_reconcile::seed_if_empty(pool, &config).await?;
            if !seed.imported {
                crate::config_reconcile::warn_inert_yaml_runtime_sections(&config);
            }

            crate::config_reconcile::reconcile_db_config(
                &crate::config_reconcile::ReconcileDeps {
                    pool,
                    deployment_store: &deployment_store,
                    alias_store: &alias_store,
                    plan_store: &plan_store,
                    block_rule_store: &block_rule_store,
                    flow_controller: &flow_controller,
                    request_rate: &request_rate,
                    cost_template_store: &cost_template_store,
                    workflow_store: &workflow_store,
                },
            )
            .await?;
            crate::config_reconcile::adopt_blocklist_enabled_db(pool, &block_rule_store, &config)
                .await?;

            // Restore runtime state.
            plan_store.restore_assignments_from_db(pool).await;
            plan_store.restore_team_assignments_from_db(pool).await;
            limiter.restore_counters_from_db(pool).await;
            plan_store.cleanup_assignments();
        } else {
            build_deployments_from_config(&config, &deployment_store, &cost_template_store);
            build_aliases_from_config(&config, &alias_store, &deployment_store);
            build_block_rules_from_config(&config, &block_rule_store);
            load_plans_from_config(&plan_store, &config);
            seed_flow_controller_from_config(&config, &flow_controller);
            load_workflows_from_config(&config, &workflow_store);
            load_cost_templates_from_config(&config, &cost_template_store);
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

        // Trace registry — top-level Arc, survives reloads (CLAUDE.md §4).
        // Active span table + slow ring live here; the OTLP traces exporter
        // is constructed lazily via `replace_otlp` once we resolve the final
        // TraceConfig (default = disabled, no exporter). The exporter is
        // feature-gated to `otlp` — without the feature, TraceRegistry still
        // works as an in-memory active/recent table for the dashboard, just
        // no OTLP push.
        let trace_registry = boom_trace::TraceRegistry::new();
        let trace_config = config.trace.clone().unwrap_or_default();
        #[cfg(feature = "otlp")]
        {
            trace_registry
                .replace_otlp(&trace_config.otlp, trace_config.enabled)
                .await;
        }

        // Process start time — single origin shared by `/health`'s uptime_secs
        // and the stressmon snapshot, so both tick in lockstep. The reload
        // path (build_inner at the reload call site) passes `old_started_at`
        // to preserve this origin across hot reloads.
        let started_at = chrono::Utc::now();
        let inner = Self::build_inner(config, &db_pool, started_at, 0)?;

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
        // `started_at` is the same origin as health.started_at above, so the
        // dashboard's uptime matches `/health`.
        let num_workers = tokio::runtime::Handle::current()
            .metrics()
            .num_workers()
            .max(1);
        let stressmon = boom_stressmon::StressmonCollector::new(num_workers, started_at);

        let response_store =
            Arc::new(boom_responses::ResponseStore::new(db_pool.clone()));
        let state = Self {
            response_store,
            config_path,
            inner: Arc::new(ArcSwap::from_pointee(inner)),
            db_pool,
            dashboard_db_pool,
            log_writer,
            limiter,
            plan_store,
            deployment_store,
            alias_store,
            workflow_store,
            cost_template_store,
            block_rule_store,
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
            kvc_stats,
            request_rate,
            agent_stats,
            kv_index,
            kv_prune_handle: Arc::new(std::sync::Mutex::new(None)),
            kvc_orchestrator,
            stressmon,
            trace: trace_registry,
            alerts: Arc::new(boom_alert::AlertManager::new()),
        };
        state.register_fusion_models(&state.inner.load().config).await?;
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
    /// Both modes re-read YAML for process-level sections (server, router
    /// policy, prompt log, trace) and rebuild the derived runtime objects
    /// (policy, classifier, fusion). They differ on business config:
    ///
    ///   - DB mode: the DB is the runtime authority. YAML runtime sections
    ///     (model_list / plans / blocklist rules / model_group_alias) are
    ///     inert; stores are reconciled against the DB via a per-key diff —
    ///     never cleared — so routing continues on the old state while the
    ///     diff applies. A reconcile failure aborts the reload with the
    ///     stores holding their pre-diff state.
    ///   - No-DB mode: YAML is the build source — full clear + rebuild,
    ///     same as a fresh boot.
    ///
    /// Failure window (no-DB mode only): once the clear+rebuild step runs,
    /// the stores are mutable targets. Any later failure leaves them in a
    /// partially-built state while the old `inner` config still routes.
    /// Wrapping with timeout + catch_unwind (in `reload`) bounds *how long*
    /// this can take but does NOT roll back partial mutations — that requires
    /// shadow-build + atomic swap (tracked as TODO).
    async fn reload_inner(&self) -> anyhow::Result<String> {
        tracing::info!(
            "Hot-reloading config from {}...",
            self.config_path.as_deref().unwrap_or("<env bootstrap>")
        );

        // 1. Re-read config. Env-bootstrap mode has no file to re-read:
        // keep the current process config and let the reconcile step pull
        // the DB business state (SIGHUP still rebuilds policy/classifier
        // and reconciles the stores, so it remains useful in this mode).
        let new_config = match &self.config_path {
            Some(p) => boom_config::load_config(p)?,
            None => self.inner.load().config.clone(),
        };

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

        // 4. Rebuild stores.
        // No-DB mode: YAML is the build source — full clear+rebuild.
        // DB mode: deferred to the reconcile step below (the DB is the
        // runtime authority; a diff, not a clear+rebuild).
        if db_pool.is_none() {
            self.deployment_store.clear();
            build_deployments_from_config(&new_config, &self.deployment_store, &self.cost_template_store);

            self.alias_store.clear();
            build_aliases_from_config(&new_config, &self.alias_store, &self.deployment_store);
            build_block_rules_from_config(&new_config, &self.block_rule_store);

            self.plan_store.clear_plans();
            load_plans_from_config(&self.plan_store, &new_config);
            seed_flow_controller_from_config(&new_config, &self.flow_controller);

            self.workflow_store.clear();
            load_workflows_from_config(&new_config, &self.workflow_store);
            self.cost_template_store.clear();
            load_cost_templates_from_config(&new_config, &self.cost_template_store);
        }

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
        //   max_blocks       → LRU capacity; LruCache can't resize in-place.
        //   router_ttl_secs  → prune task TTL/interval, fixed at spawn time.
        // block_size is a code constant now (512B), so only the policy switch
        // itself invalidates entries. max_blocks / router_ttl_secs don't
        // affect existing hashes — the prune task is restarted below to pick
        // up the new TTL, and the LRU capacity adapts on the next batch.
        let kvc_sig = |r: &boom_config::RouterSettings| r.schedule_policy.clone();
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
        // kvc_stats is NOT recreated — it is AppState-owned so the global
        // counters survive this reload.
        let new_policy = create_policy(
            &new_config,
            &self.inflight,
            &self.flow_controller,
            &self.rebalance_move_tracker,
            &self.kvc_stats,
            &**self.kv_index.load(),
        );
        self.router.set_policy(new_policy);

        // Rebuild auto router classifier.
        self.router.set_classifier(build_auto_router(&new_config));

        if let Some(ref pool) = db_pool {
            // DB is the runtime authority: reconcile the live stores with DB
            // state — a per-key diff, never a clear, so routing continues on
            // the old state while the diff applies. YAML runtime sections are
            // inert; warn when they still carry content. The seed check runs
            // here too (marker-guarded, one SELECT when already seeded) so a
            // reload that just gained a database_url imports YAML instead of
            // reconciling against an empty DB. Timeout bounds DB hangs from
            // blocking reload; failure aborts the reload (previous inner
            // config still routes).
            crate::config_reconcile::warn_inert_yaml_runtime_sections(&new_config);
            crate::config_reconcile::seed_if_empty(pool, &new_config).await?;
            with_db_timeout(
                "reconcile_db_config",
                crate::config_reconcile::reconcile_db_config(
                    &crate::config_reconcile::ReconcileDeps {
                        pool,
                        deployment_store: &self.deployment_store,
                        alias_store: &self.alias_store,
                        plan_store: &self.plan_store,
                        block_rule_store: &self.block_rule_store,
                        flow_controller: &self.flow_controller,
                        request_rate: &self.request_rate,
                        cost_template_store: &self.cost_template_store,
                        workflow_store: &self.workflow_store,
                    },
                ),
            )
            .await?;
            crate::config_reconcile::adopt_blocklist_enabled_db(
                pool,
                &self.block_rule_store,
                &new_config,
            )
            .await?;
        }

        self.register_fusion_models(&new_config).await?;

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

        // 5b. Hot-reload trace config — same pattern as prompt_log. Rebuilds
        //     the OTLP traces exporter when its sub-config changes. The
        //     registry itself (active table + recent ring) is untouched —
        //     only the exporter + enabled flag swap.
        let new_trace_config = new_config.trace.clone().unwrap_or_default();
        #[cfg(feature = "otlp")]
        {
            // `replace_otlp` is the canonical hot-swap entry: aborts the old
            // flush task, runs a best-effort final flush, constructs a new
            // exporter (or None if disabled). Calling it unconditionally on
            // reload is correct (idempotent for "no change") — the flush on an
            // unchanged config is typically a no-op (queue empty).
            self.trace
                .replace_otlp(&new_trace_config.otlp, new_trace_config.enabled)
                .await;
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
            KV_BLOCK_SIZE,
            kv_settings.max_blocks,
        ));
        tracing::info!(
            block_size = KV_BLOCK_SIZE,
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
        // 0 = TTL prune disabled: rely on LRU (max_blocks) alone. The task is
        // still spawned so stale-shell sweeping (sweep_stale) keeps running —
        // LRU eviction also leaves shells behind. It throttles internally, so
        // a fixed interval is fine.
        if !(ttl_secs > 0.0) {
            tracing::info!("KV TTL prune disabled (router_ttl_secs=0), using LRU + stale sweep only");
            let kv_index = self.kv_index.clone();
            let handle = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                ticker.tick().await; // first tick is immediate
                loop {
                    ticker.tick().await;
                    // Best-effort tick: a panic inside sweep_stale kills only
                    // THIS pass — without the catch the whole task would die
                    // silently and cleaning would stop until the next reload.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let g = kv_index.load();
                        if let Some(idx) = (**g).as_ref() {
                            idx.sweep_stale();
                        }
                    }));
                    if r.is_err() {
                        tracing::error!("KV sweep tick panicked — task continues on next tick");
                    }
                }
            });
            *self.kv_prune_handle.lock().unwrap() = Some(handle);
            return;
        }
        let ttl = std::time::Duration::from_secs_f64(ttl_secs);
        // Tick at min(ttl/2, 60s) — a block lives at most ~ttl + 60s.
        // Capping at 60s matters for TTL > 120: TTL-expired shells are
        // reclaimed by the sweep in the SAME tick that prunes them (zero
        // lag), but LRU shells are produced on the record path whenever the
        // trie is at max_blocks — they accumulate between ticks. With a
        // ttl/2-only interval a 1200s TTL would leave LRU shells piling up
        // for up to 600s under pressure. prune_expired on a not-yet-due heap
        // is an O(1) peek, so the extra wakeups are free. The tighter tick
        // also makes TTL expiry more precise (ttl + 60s instead of
        // ttl + ttl/2) — accepted as within the configured window.
        let interval = std::time::Duration::from_secs_f64((ttl_secs / 2.0).min(60.0).max(5.0));
        let kv_index = self.kv_index.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // first tick is immediate
            loop {
                ticker.tick().await;
                // Best-effort tick: a panic inside prune/sweep kills only
                // THIS pass — without the catch the whole task would die
                // silently and TTL/shell cleanup would stop until the next
                // reload (symptom: memory grows with no error anywhere).
                // Cleanup is idempotent: a half-finished pass is simply
                // finished by the next tick.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let g = kv_index.load();
                    if let Some(idx) = (**g).as_ref() {
                        idx.prune_expired(ttl);
                        // Reclaim empty shell nodes left by non-cascading
                        // eviction (internal throttle caps actual sweep rate).
                        idx.sweep_stale();
                    }
                }));
                if r.is_err() {
                    tracing::error!(
                        "KV prune/sweep tick panicked — task continues on next tick"
                    );
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

    /// (Re-)register workflow virtual models from the WorkflowStore.
    ///
    /// The store is the authority (DB in DB mode, YAML in no-DB mode); the
    /// YAML config's workflow_settings is only the bootstrap seed. Validation
    /// runs here via `workflow_validation_namespaces` — see its doc for why
    /// the universe includes disabled models.
    pub(crate) async fn register_fusion_models(&self, config: &Config) -> Result<(), boom_core::GatewayError> {
        let settings = self.workflow_store.settings();
        // Feature-parked gate (router_settings.enable_workflow, default false).
        // When disabled we register an EMPTY set: the teardown pass inside
        // register_fusion_providers then unregisters any previous fusion
        // registrations, so flipping the flag off + reload deactivates
        // cleanly instead of leaving stale virtual models routing.
        let settings = if config.router_settings.enable_workflow {
            settings
        } else {
            if !settings.models.is_empty() {
                tracing::warn!(
                    count = settings.models.len(),
                    "workflow bindings present but router_settings.enable_workflow=false — workflow virtual models are disabled"
                );
            }
            Default::default()
        };
        let namespaces = self.workflow_validation_namespaces(config).await;
        settings.validate_with_namespaces(&namespaces)?;

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
            &settings,
            &self.deployment_store,
            &self.alias_store,
            runtime,
        )
    }

    /// The model-name universe workflow settings validate against: every
    /// KNOWN model — including disabled deployments — minus the workflow-
    /// owned exclusive registrations (their own previous registrations must
    /// not self-collide on re-registration).
    ///
    /// Disabled models stay in the universe on purpose: a disabled panel or
    /// aggregator is a runtime 502 by design (the fusion runtime resolves
    /// children per request and reports `Model not found`), NOT a boot-time
    /// config error — YAML-era validation checked `model_list`, which also
    /// kept disabled entries. This also catches latent name collisions: a
    /// workflow model named after a disabled deployment would shadow it the
    /// moment it is re-enabled.
    pub(crate) async fn workflow_validation_namespaces(
        &self,
        config: &Config,
    ) -> boom_config::WorkflowNamespaces {
        let names: Vec<String> = if let Some(pool) = &self.db_pool {
            match DeploymentStore::all_model_names_db(pool).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(
                        "workflow namespace assembly: all_model_names_db failed ({}); \
                         falling back to routable store names",
                        e
                    );
                    self.deployment_store.model_names()
                }
            }
        } else {
            config
                .model_list
                .iter()
                .map(|m| m.model_name.clone())
                .collect()
        };
        boom_config::WorkflowNamespaces {
            deployment_names: names
                .into_iter()
                .filter(|n| !self.deployment_store.is_exclusive_model(n))
                .collect(),
            aliases: self.alias_store.all_aliases().into_iter().collect(),
        }
    }

    /// Write the current DB snapshot + process config back to the YAML file
    /// (the write-back button). Rolls a `.bak` of the previous file first —
    /// serde_yaml serialization drops comments, so the `.bak` is also the
    /// only record of pre-edit annotation. Untouched sections keep their
    /// raw-file form (`${VAR}` secret references preserved; see
    /// [`export_config_yaml`]).
    ///
    /// Env-bootstrap mode (no startup file): generates a fresh
    /// `./config.yaml` in the working directory instead — no `.bak` roll
    /// (nothing to back up), mode 0600 (the document carries plaintext
    /// upstream api keys). A restart with the generated file adopts it as
    /// the boot config; until then the process keeps running env-bootstrapped.
    ///
    /// Returns the path written, for the caller's response message.
    pub async fn persist_config_in_place(&self) -> Result<String, String> {
        let yaml = self.export_config_yaml().await?;
        match &self.config_path {
            Some(path) => {
                backup_yaml(path);
                boom_config::write_yaml_string_atomic(path, &yaml)
                    .map_err(|e| format!("write config: {}", e))?;
                tracing::info!(path = %path, "Config persisted in place");
                Ok(path.clone())
            }
            None => {
                let path = "config.yaml";
                boom_config::write_yaml_string_atomic(path, &yaml)
                    .map_err(|e| format!("write config: {}", e))?;
                restrict_file_permissions(path);
                tracing::info!(path = %path, "Config generated from env bootstrap + DB snapshot");
                Ok(path.to_string())
            }
        }
    }

    /// Compose the full current config as a YAML document: runtime sections
    /// (models/aliases/plans/block rules/workflows/cost templates) from the
    /// DB snapshot, everything else preserved verbatim from the raw YAML on
    /// disk (including `${VAR}` secret references — see `read_raw_yaml`).
    ///
    /// Env-bootstrap mode has no file to preserve: the root is synthesized
    /// from the live process config, with master_key/database_url written as
    /// `${BOOM_*}` env references for a clean round-trip.
    ///
    /// Shared pipeline for the export button (returns the string, no file
    /// touched) and the write-back button (writes it to the startup file
    /// after a `.bak` roll).
    pub async fn export_config_yaml(&self) -> Result<String, String> {
        let pool = match &self.db_pool {
            Some(p) => p,
            None => return Err("Database not available".to_string()),
        };

        let mut root: serde_yaml::Value = match &self.config_path {
            Some(p) => match boom_config::read_raw_yaml(p) {
                Ok(v) => v,
                Err(e) => return Err(format!("read config: {}", e)),
            },
            None => synthesize_bootstrap_root(&self.inner.load().config)?,
        };

        let snapshot = match build_config_snapshot_value(pool).await {
            Ok(v) => v,
            Err(e) => return Err(format!("build snapshot from DB: {}", e)),
        };

        compose_export_yaml(&mut root, &snapshot, self.block_rule_store.is_enabled())
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

        let Some(config_path) = &self.config_path else {
            return Err(
                "no config file to update — the gateway was bootstrapped from environment \
                 variables; use the write-back button to generate config.yaml, then \
                 restart to adopt it as the boot config"
                    .to_string(),
            );
        };

        // Business config is DB-owned. Writing these sections to YAML would
        // create a second authority that the reconcile path deliberately
        // ignores (or, pre-M4, partially consumes for workflow/cost) — reject
        // with a pointer to the right surface instead of silently doing the
        // wrong thing on the next reload.
        const DB_OWNED_PATHS: [&str; 6] = [
            "model_list",
            "plan_settings",
            "client_blocklist",
            "router_settings.model_group_alias",
            "workflow_settings",
            "cost_templates",
        ];
        if DB_OWNED_PATHS.contains(&path) {
            return Err(format!(
                "'{path}' is managed in the database (dashboard CRUD), not in \
                 config.yaml; use the dashboard's export/write-back buttons to \
                 refresh the YAML snapshot"
            ));
        }

        backup_yaml(config_path);

        let mut root: serde_yaml::Value = boom_config::read_raw_yaml(config_path)
            .map_err(|e| format!("Failed to read config: {}", e))?;

        let yaml_value = json_to_yaml(&value).map_err(|e| format!("JSON→YAML: {}", e))?;
        boom_config::set_yaml_path(&mut root, &segments, yaml_value)
            .map_err(|e| format!("Failed to set path: {}", e))?;

        boom_config::write_yaml_atomic(config_path, &root)
            .map_err(|e| format!("Failed to write config: {}", e))?;

        tracing::info!(path = %path, "Config section updated");

        self.reload()
            .await
            .map_err(|e| format!("Saved but reload failed: {}", e))
    }
}

/// Merge runtime-derived sections (model_list, aliases, plans) from a JSON
/// snapshot into the raw YAML value. Singleton sections are preserved as-is.
/// `blocklist_enabled` is the running gateway's blocklist switch, used as a
/// fallback when the snapshot carries no `enabled` (no-DB mode / missing
/// state row).
fn merge_runtime_sections(
    root: &mut serde_yaml::Value,
    snapshot: &serde_json::Value,
    blocklist_enabled: bool,
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
        if let Some(default_team_plan) = plan_settings.get("default_team_plan") {
            let yaml_val = json_to_yaml(default_team_plan)?;
            boom_config::set_yaml_path(root, &["plan_settings", "default_team_plan"], yaml_val)
                .map_err(|e| format!("set plan_settings.default_team_plan: {}", e))?;
        }
    }

    // Client blocklist: rules and the `enabled` switch are DB-owned runtime
    // state. The snapshot carries the DB switch when the state row exists;
    // `blocklist_enabled` (the running gateway's value) covers the no-DB
    // fallback and any transient where the row is missing.
    if let Some(bl) = obj.get("client_blocklist") {
        if let Some(rules) = bl.get("rules") {
            let yaml_val = json_to_yaml(rules)?;
            boom_config::set_yaml_path(root, &["client_blocklist", "rules"], yaml_val)
                .map_err(|e| format!("set client_blocklist.rules: {}", e))?;
        }
        let enabled_value = bl.get("enabled").cloned().unwrap_or_else(|| {
            serde_json::Value::Bool(blocklist_enabled)
        });
        if !enabled_value.is_null() {
            let yaml_enabled = json_to_yaml(&enabled_value)?;
            boom_config::set_yaml_path(root, &["client_blocklist", "enabled"], yaml_enabled)
                .map_err(|e| format!("set client_blocklist.enabled: {}", e))?;
        }
    }

    // Workflows + cost templates are DB-owned runtime state — same wholesale
    // replacement as the other runtime sections.
    if let Some(workflow_settings) = obj.get("workflow_settings") {
        let yaml_val = json_to_yaml(workflow_settings)?;
        boom_config::set_yaml_path(root, &["workflow_settings"], yaml_val)
            .map_err(|e| format!("set workflow_settings: {}", e))?;
    }
    if let Some(cost_templates) = obj.get("cost_templates") {
        let yaml_val = json_to_yaml(cost_templates)?;
        boom_config::set_yaml_path(root, &["cost_templates"], yaml_val)
            .map_err(|e| format!("set cost_templates: {}", e))?;
    }

    // Strip the deprecated general_settings.public_models list. Visibility is
    // exported per-model (model_list above) and the DB rows already carry
    // visibility=public from the startup sync_yaml_to_db, so removing the
    // legacy key here is a safe one-way migration — it never comes back.
    if let Some(gs) = root
        .get_mut("general_settings")
        .and_then(|g| g.as_mapping_mut())
    {
        if gs.remove("public_models").is_some() {
            tracing::info!(
                "Stripped deprecated general_settings.public_models from YAML (superseded by per-model visibility)"
            );
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

/// Compose the final export document: merge the DB runtime snapshot into the
/// raw YAML value, then serialize to a YAML string. Pure (no I/O) so the
/// merge semantics stay unit-testable — see `merge_runtime_sections_tests`.
fn compose_export_yaml(
    root: &mut serde_yaml::Value,
    snapshot: &serde_json::Value,
    blocklist_enabled: bool,
) -> Result<String, String> {
    merge_runtime_sections(root, snapshot, blocklist_enabled)?;
    serde_yaml::to_string(root).map_err(|e| format!("serialize yaml: {}", e))
}

/// Build the export root document for env-bootstrap mode (no YAML file on
/// disk). The live `Config` is the only source — but its secrets are already
/// env-resolved, so master_key / database_url are written back as `${BOOM_*}`
/// references instead of plaintext (load_config re-resolves them on the next
/// start, giving a clean round-trip). Null top-level sections and the legacy
/// serde alias field are dropped to keep the generated document clean.
fn synthesize_bootstrap_root(config: &Config) -> Result<serde_yaml::Value, String> {
    let mut root =
        serde_yaml::to_value(config).map_err(|e| format!("serialize config: {}", e))?;

    if let Some(gs) = root
        .get_mut("general_settings")
        .and_then(|g| g.as_mapping_mut())
    {
        if config.general_settings.master_key.is_some() {
            gs.insert("master_key".into(), "${BOOM_MASTER_KEY}".into());
        }
        if config.general_settings.database_url.is_some() {
            gs.insert("database_url".into(), "${BOOM_DATABASE_URL}".into());
        }
    }

    if let Some(map) = root.as_mapping_mut() {
        let noise: Vec<serde_yaml::Value> = map
            .iter()
            .filter(|(k, v)| {
                v.is_null() || k.as_str() == Some("_legacy_store_model_in_db")
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in noise {
            map.remove(&key);
        }
    }

    // Same cleanup inside general_settings (user_tag_headers: null, the
    // legacy alias field) — the section serializes all fields, unlike a
    // hand-written file that only carries what's set.
    if let Some(gs) = root
        .get_mut("general_settings")
        .and_then(|g| g.as_mapping_mut())
    {
        let noise: Vec<serde_yaml::Value> = gs
            .iter()
            .filter(|(k, v)| {
                v.is_null() || k.as_str() == Some("_legacy_store_model_in_db")
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in noise {
            gs.remove(&key);
        }
    }
    Ok(root)
}

/// Best-effort chmod 0600 for generated config files — the document carries
/// plaintext upstream api keys. (Unix-only crate: jemalloc global allocator.)
fn restrict_file_permissions(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(path = %path, "chmod 600 failed: {}", e);
    }
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
/// reload-time DB step so a slow DB can't hang the entire reload future.
/// 15s leaves headroom for slow networks without letting a stuck query block
/// reload indefinitely.
async fn with_db_timeout<F, T, E>(op_name: &str, f: F) -> Result<T, anyhow::Error>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    match tokio::time::timeout(std::time::Duration::from_secs(15), f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(anyhow::anyhow!("{}: {}", op_name, e)),
        Err(_) => Err(anyhow::anyhow!("{} timed out after 15s", op_name)),
    }
}


// ═══════════════════════════════════════════════════════════
// YAML → Memory (no-DB fallback)
// ═══════════════════════════════════════════════════════════

/// Derive a model's cost rate from its `model_info.cost_template`.
///
/// Pricing is sourced exclusively from the CostTemplateStore (DB-authoritative
/// in DB mode, YAML-seeded in no-DB mode) — model_info no longer carries
/// inline cost fields (v3 single-source-of-truth refactor). The
/// `cost_template` name selects which template applies; missing template =
/// no rate. Rates are per million tokens in the operator's billing currency
/// (see `boom_config::CostTemplate`), converted to per-token Decimal
/// internally for accurate accounting on small requests.
///
/// Shared by the YAML build path (`build_deployments_from_config`), the
/// reconciler (`apply_model_rows_from_db`), and the CRUD apply path
/// (`reload_model_deployments`) so all three stay identical.
pub(crate) fn cost_rate_from_model_info(
    templates: &CostTemplateStore,
    model_name: &str,
    info: &boom_config::ModelInfo,
) -> Option<boom_routing::ModelCostRate> {
    use rust_decimal::prelude::FromPrimitive;
    let per_million_to_per_token = |v: Option<f64>| -> Option<rust_decimal::Decimal> {
        v.and_then(rust_decimal::Decimal::from_f64)
            .map(|d| d / rust_decimal::Decimal::from(1_000_000))
    };

    // Template lookup is the only source of rates now.
    let template_rates = info.cost_template.as_ref().and_then(|tn| {
        templates.lookup(tn).map(|t| {
            (
                t.input_cost_per_million_tokens,
                t.cached_input_cost_per_million_tokens,
                t.output_cost_per_million_tokens,
            )
        })
    });

    if let (Some(ref tn), None) = (&info.cost_template, &template_rates) {
        tracing::warn!(
            model = %model_name,
            template = %tn,
            "cost_template not found — no rate registered"
        );
    }

    let (src_input, src_cached, src_output) = template_rates?;
    let input_rate = per_million_to_per_token(src_input);
    let cached_rate = per_million_to_per_token(src_cached);
    let output_rate = per_million_to_per_token(src_output);

    if input_rate.is_some() || output_rate.is_some() || cached_rate.is_some() {
        tracing::info!(
            model = %model_name,
            template = ?info.cost_template,
            input_per_1m = src_input.unwrap_or(0.0),
            cached_per_1m = src_cached.unwrap_or(0.0),
            output_per_1m = src_output.unwrap_or(0.0),
            "Registered cost rate (per million tokens)"
        );
        Some(boom_routing::ModelCostRate::with_cached(
            input_rate.unwrap_or_default(),
            cached_rate.unwrap_or_default(),
            output_rate.unwrap_or_default(),
        ))
    } else {
        None
    }
}

/// Build deployments directly from YAML config into DeploymentStore.
/// Cost rates resolve through the CostTemplateStore (YAML-seeded in this mode).
fn build_deployments_from_config(
    config: &Config,
    deployment_store: &Arc<DeploymentStore>,
    cost_template_store: &Arc<CostTemplateStore>,
) {
    deployment_store.clear();

    // Legacy general_settings.public_models: merge into the unified
    // visibility state (Public) on matching deployments. Deprecated — the
    // dashboard no longer edits this key; it disappears from YAML on the
    // next persist once models carry visibility: public directly.
    let legacy_public: std::collections::HashSet<&str> = config
        .general_settings
        .public_models
        .iter()
        .map(|m| m.as_str())
        .collect();
    if !legacy_public.is_empty() {
        tracing::warn!(
            count = legacy_public.len(),
            "general_settings.public_models is deprecated; migrating entries to model visibility=public (remove the key from YAML)"
        );
    }

    for entry in &config.model_list {
        let p = &entry.litellm_params;

        let mut extra = std::collections::HashMap::new();
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

        // Visibility applies at model_name granularity and is registered
        // before the enabled check so a disabled-but-public/private model
        // keeps its status — visibility filtering (list_models) consults
        // the state even when no provider is routable.
        let vis = crate::config_reconcile::effective_yaml_visibility(entry, &legacy_public);
        deployment_store.set_visibility(
            &entry.model_name,
            VisibilityState::from((vis, entry.allowed_teams.clone())),
        );

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
            &p.headers,
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
                // Derivation lives in `cost_rate_from_model_info` so the YAML
                // build path and the CRUD apply path stay identical.
                if let Some(info) = entry.model_info.as_ref() {
                    if let Some(rate) =
                        cost_rate_from_model_info(cost_template_store, &entry.model_name, info)
                    {
                        deployment_store.set_cost_rate(&entry.model_name, rate);
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

/// Load workflow definitions + bindings from YAML into the WorkflowStore
/// (no-DB mode build path; DB mode seeds the tables from the same section
/// once and is authoritative afterwards).
fn load_workflows_from_config(config: &Config, workflow_store: &Arc<WorkflowStore>) {
    for (name, definition) in &config.workflow_settings.workflows {
        workflow_store.upsert_definition(name, definition.clone());
    }
    for (model, workflow_name) in &config.workflow_settings.models {
        workflow_store.upsert_model(model, workflow_name);
    }
    let n = config.workflow_settings.models.len();
    if n > 0 {
        tracing::info!("Loaded {n} workflow virtual model(s) from YAML");
    }
}

/// Load cost templates from YAML into the CostTemplateStore (no-DB mode
/// build path; DB mode seeds the table from the same list once).
fn load_cost_templates_from_config(
    config: &Config,
    cost_template_store: &Arc<CostTemplateStore>,
) {
    for template in &config.cost_templates {
        cost_template_store.upsert(template.clone());
    }
    if !config.cost_templates.is_empty() {
        tracing::info!("Loaded {} cost template(s) from YAML", config.cost_templates.len());
    }
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

/// Build client blocklist rules from YAML config into BlockRuleStore.
/// An absent section disables blocking entirely (enabled=false, no rules).
/// Broken rules are skipped with a warn — they must not block config load.
fn build_block_rules_from_config(
    config: &Config,
    store: &Arc<boom_gatekeeper::BlockRuleStore>,
) {
    store.clear();
    let bl = match &config.client_blocklist {
        Some(bl) => bl,
        None => {
            store.set_enabled(false);
            return;
        }
    };
    store.set_enabled(bl.enabled);
    let mut loaded = 0usize;
    for rule in &bl.rules {
        match store.insert_rule(rule) {
            Ok(()) => loaded += 1,
            Err(e) => tracing::warn!("Skip block rule '{}': {}", rule.name, e),
        }
    }
    tracing::info!(
        "Loaded {} block rule(s), blocklist {}",
        loaded,
        if bl.enabled { "enabled" } else { "disabled" },
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
pub(crate) fn load_plans_from_config(plan_store: &Arc<PlanStore>, config: &Config) {
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

/// Build an AutoRouter from config, if auto_router section is present.
fn build_auto_router(config: &Config) -> Option<Arc<AutoRouter>> {
    let hr_config = config.router_settings.auto_router.as_ref()?;

    // ── Strategy / section consistency checks ──────────────────────
    let wants_ml = hr_config.strategy == "ml_service";
    let has_ml = hr_config.ml_service.is_some();
    if wants_ml && !has_ml {
        tracing::error!(
            strategy = %hr_config.strategy,
            "auto_router.strategy=ml_service requires the ml_service sub-section \
             (url, timeout_ms). Add it or switch strategy to tier_classifier. \
             Disabling auto router."
        );
        return None;
    }
    if has_ml && !wants_ml {
        tracing::warn!(
            strategy = %hr_config.strategy,
            "ml_service section is configured but auto_router.strategy is not \
             'ml_service' — the ml_service section will be ignored"
        );
    }

    let mut registry = StrategyRegistry::new();
    registry.register(Arc::new(TierClassifier));
    if let Some(ml) = &hr_config.ml_service {
        // `timeout_ms == 0` is rejected up-front in MlServiceClient::try_new
        // (reqwest treats 0 as no-timeout). Surface the same condition here
        // as an error so the operator sees the actionable message before
        // registration is even attempted.
        if ml.timeout_ms == 0 {
            tracing::error!(
                url = %ml.url,
                timeout_ms = ml.timeout_ms,
                "ml_service.timeout_ms must be > 0 (reqwest treats 0 as 'no timeout', \
                 which would let a hung ML service block every classifier call \
                 indefinitely). Skipping ml_service strategy registration."
            );
        } else {
            tracing::info!(
                url = %ml.url,
                timeout_ms = ml.timeout_ms,
                "Registering ML service classification strategy"
            );
            let valid_tiers: std::collections::HashSet<String> =
                hr_config.tiers.keys().cloned().collect();
            match MlServiceClient::try_new(&ml.url, ml.timeout_ms, valid_tiers) {
                Ok(client) => registry.register(Arc::new(client)),
                Err(e) => tracing::error!(
                    url = %ml.url,
                    error = %e,
                    "Failed to build ML service client; ml_service strategy \
                     unavailable (other strategies remain registered)"
                ),
            }
        }
    }

    let strategy = match registry.get(&hr_config.strategy) {
        Some(s) => s.clone(),
        None => {
            tracing::error!(
                strategy = %hr_config.strategy,
                "Unknown auto_router strategy, disabling auto router"
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
        "Auto router enabled"
    );

    Some(Arc::new(AutoRouter::new(
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
    kvc_stats: &Arc<KvcSchedulerStats>,
    kv_index: &Option<Arc<dyn KvIndexBackend>>,
) -> Arc<dyn SchedulePolicy> {
    match config.router_settings.schedule_policy.as_str() {
        "round_robin" | "" => Arc::new(RoundRobinPolicy::new()),
        "shuffle" => {
            tracing::info!("Using shuffle policy (random pick, litellm simple-shuffle compatible)");
            Arc::new(ShufflePolicy::new())
        }
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
                Some(kvc_stats.clone()),
            );
            policy.set_queue_info(flow_controller.clone());
            // Pure-affinity scoring (hit_ratio); the router-level
            // rebalance_threshold is the only load-based intervention.
            policy.set_rebalance_threshold(
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
pub(crate) async fn build_config_snapshot_value(pool: &PgPool) -> Result<serde_json::Value, sqlx::Error> {
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

            // ── Visibility + team ACL. Emit `visibility` only when non-normal
            // so normal rows stay clean; `allowed_teams` only when private —
            // Some([]) (locked) is emitted as an empty array, a distinct,
            // intentional state. A stale allowed_teams on a normal row is NOT
            // round-tripped (the visibility column is authoritative).
            let vis_str = match r.visibility.as_deref() {
                Some("public") => Some("public"),
                Some("private") => Some("private"),
                _ => None,
            };
            if let Some(v) = vis_str {
                entry.as_object_mut().unwrap().insert(
                    "visibility".into(),
                    serde_json::Value::String(v.to_string()),
                );
            }
            if vis_str == Some("private") {
                if let Some(acl) = parse_allowed_teams(&r.allowed_teams) {
                    entry.as_object_mut().unwrap().insert(
                        "allowed_teams".into(),
                        serde_json::json!(acl),
                    );
                }
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
    let mut default_team_plan: Option<String> = None;
    let mut plans_map = serde_json::Map::new();

    for r in &plan_rows {
        if r.is_default.unwrap_or(false) && default_plan.is_none() {
            default_plan = Some(r.name.clone());
        }
        if r.is_default_team.unwrap_or(false) && default_team_plan.is_none() {
            default_team_plan = Some(r.name.clone());
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

    // ── Client blocklist rules (delegated to BlockRuleStore) ──
    // Rules are rebuilt from DB; the `enabled` switch now lives in
    // boom_client_blocklist_state and is part of the snapshot too (the
    // config page toggle and the export/write-back both read it here).
    let block_rule_rows = boom_gatekeeper::BlockRuleStore::list_all_db(pool).await?;
    let block_rules: Vec<serde_json::Value> = block_rule_rows
        .iter()
        .filter_map(|row| match row.to_rule() {
            Ok(rule) => serde_json::to_value(&rule).ok(),
            Err(e) => {
                tracing::warn!("Skip block rule in config snapshot: {}", e);
                None
            }
        })
        .collect();
    let blocklist_enabled: Option<bool> = sqlx::query_scalar::<_, bool>(
        r#"SELECT enabled FROM boom_client_blocklist_state WHERE id = 1"#,
    )
    .fetch_optional(pool)
    .await?;

    // ── Workflows (delegated to WorkflowStore tables) ──
    let definition_rows = boom_routing::WorkflowStore::list_definitions_db(pool).await?;
    let model_binding_rows = boom_routing::WorkflowStore::list_models_db(pool).await?;
    let mut workflows_map = serde_json::Map::new();
    for row in &definition_rows {
        workflows_map.insert(row.name.clone(), row.definition.clone());
    }
    let models_map: serde_json::Map<String, serde_json::Value> = model_binding_rows
        .into_iter()
        .map(|r| (r.model_name, serde_json::Value::String(r.workflow_name)))
        .collect();

    // ── Cost templates (delegated to CostTemplateStore table) ──
    let cost_template_rows = boom_routing::CostTemplateStore::list_all_db(pool).await?;
    let cost_templates: Vec<serde_json::Value> = cost_template_rows
        .into_iter()
        .map(|r| {
            let mut obj = serde_json::Map::new();
            obj.insert("name".into(), serde_json::Value::String(r.name));
            if let Some(v) = r.input_cost_per_million_tokens {
                obj.insert(
                    "input_cost_per_million_tokens".into(),
                    serde_json::Value::from(v),
                );
            }
            if let Some(v) = r.cached_input_cost_per_million_tokens {
                obj.insert(
                    "cached_input_cost_per_million_tokens".into(),
                    serde_json::Value::from(v),
                );
            }
            if let Some(v) = r.output_cost_per_million_tokens {
                obj.insert(
                    "output_cost_per_million_tokens".into(),
                    serde_json::Value::from(v),
                );
            }
            serde_json::Value::Object(obj)
        })
        .collect();

    // ── Assemble top-level ──
    let mut plan_settings = serde_json::Map::new();
    if let Some(dp) = default_plan {
        plan_settings.insert("default_plan".into(), serde_json::Value::String(dp));
    }
    if let Some(dtp) = default_team_plan {
        plan_settings.insert("default_team_plan".into(), serde_json::Value::String(dtp));
    }
    plan_settings.insert("plans".into(), serde_json::Value::Object(plans_map));

    Ok(serde_json::json!({
        "model_list": model_list,
        "router_settings": {
            "model_group_alias": model_group_alias,
        },
        "plan_settings": plan_settings,
        "client_blocklist": {
            "enabled": blocklist_enabled,
            "rules": block_rules,
        },
        "workflow_settings": {
            "models": models_map,
            "workflows": workflows_map,
        },
        "cost_templates": cost_templates,
    }))
}

#[cfg(test)]
mod merge_runtime_sections_tests {
    use super::merge_runtime_sections;

    fn snapshot_with_rules() -> serde_json::Value {
        serde_json::json!({
            "client_blocklist": {
                "rules": [
                    {
                        "name": "r1",
                        "enabled": true,
                        "conditions": [{"field": "body", "op": "contains", "value": "x"}],
                        "action": {"message": "blocked"}
                    }
                ]
            }
        })
    }

    fn parse(yaml: &str) -> serde_yaml::Value {
        serde_yaml::from_str(yaml).expect("parse yaml")
    }

    #[test]
    fn materializes_enabled_fallback_when_snapshot_lacks_it() {
        // No-DB mode / missing state row: the snapshot carries no `enabled`,
        // so the running switch (here: off) is materialized. A restart would
        // otherwise parse the section with the serde default (true) and
        // silently activate rules the operator never turned on.
        let mut root = parse("model_list: []\n");
        merge_runtime_sections(&mut root, &snapshot_with_rules(), false).expect("merge");
        let bl = root.get("client_blocklist").expect("section created");
        assert_eq!(bl.get("enabled"), Some(&serde_yaml::Value::Bool(false)));
        assert!(bl.get("rules").is_some());
    }

    #[test]
    fn snapshot_enabled_overrides_stale_yaml_value() {
        // The DB state row says enabled: false; a stale hand-edited YAML says
        // true. The DB is the authority — the merged document records false.
        let mut root = parse("client_blocklist:\n  enabled: true\n");
        let mut snapshot = snapshot_with_rules();
        snapshot["client_blocklist"]["enabled"] = serde_json::Value::Bool(false);
        merge_runtime_sections(&mut root, &snapshot, true).expect("merge");
        let bl = root.get("client_blocklist").expect("section kept");
        assert_eq!(bl.get("enabled"), Some(&serde_yaml::Value::Bool(false)));
    }

    #[test]
    fn compose_export_yaml_serializes_merged_document() {
        // The export pipeline must return a parseable YAML document that
        // carries both the merged runtime sections and the untouched
        // singleton sections (with ${VAR} references intact).
        use super::compose_export_yaml;
        let mut root = parse("server:\n  host: 0.0.0.0\n\ngeneral_settings:\n  master_key: ${MASTER}\n");
        let snapshot = serde_json::json!({
            "model_list": [
                {"model_name": "m1", "litellm_params": {"model": "gpt", "timeout": 30}}
            ],
            "client_blocklist": {"rules": []},
        });
        let yaml = compose_export_yaml(&mut root, &snapshot, true).expect("compose");
        let reparsed = parse(&yaml);
        assert!(reparsed.get("server").is_some(), "singleton section preserved");
        assert_eq!(
            reparsed
                .get("general_settings")
                .and_then(|g| g.get("master_key"))
                .and_then(|v| v.as_str()),
            Some("${MASTER}"),
            "env reference must survive the round-trip unexpanded"
        );
        let model_list = reparsed
            .get("model_list")
            .and_then(|v| v.as_sequence())
            .expect("model_list merged in");
        assert_eq!(model_list.len(), 1);
        assert_eq!(
            reparsed
                .get("client_blocklist")
                .and_then(|c| c.get("enabled"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn synthesize_bootstrap_root_writes_env_references_and_drops_noise() {
        // Env-bootstrap export: the live config's resolved secrets must come
        // out as ${BOOM_*} references (clean round-trip through
        // load_config's env resolution), and null sections / the legacy
        // alias field must not appear in the generated document.
        use super::synthesize_bootstrap_root;
        use serde_yaml::Value;

        // Built the same way bootstrap_from_env builds it — via the serde
        // path, so field defaults match the real env-bootstrap state.
        let config: boom_config::Config = serde_yaml::from_str(
            "general_settings:\n  master_key: sk-live\n  database_url: postgres://u:p@h/db\n",
        )
        .expect("parse config");

        let root = synthesize_bootstrap_root(&config).expect("synthesize");
        let gs = root.get("general_settings").and_then(|g| g.as_mapping());
        let gs = gs.expect("general_settings mapping");
        assert_eq!(
            gs.get(&Value::String("master_key".into())),
            Some(&Value::String("${BOOM_MASTER_KEY}".into()))
        );
        assert_eq!(
            gs.get(&Value::String("database_url".into())),
            Some(&Value::String("${BOOM_DATABASE_URL}".into()))
        );
        assert!(
            !root
                .as_mapping()
                .unwrap()
                .contains_key(&Value::String("_legacy_store_model_in_db".into())),
            "legacy alias field must not be exported"
        );
        assert!(
            !root
                .as_mapping()
                .unwrap()
                .contains_key(&Value::String("prompt_log".into())),
            "null section must be dropped"
        );
        assert!(
            root.as_mapping().unwrap().contains_key(&Value::String("server".into())),
            "non-null sections survive"
        );
        // Nested cleanup inside general_settings.
        let gs = root
            .get("general_settings")
            .and_then(|g| g.as_mapping())
            .expect("general_settings mapping");
        assert!(
            !gs.contains_key(&Value::String("_legacy_store_model_in_db".into())),
            "legacy alias field must not leak into general_settings"
        );
        assert!(
            !gs.contains_key(&Value::String("user_tag_headers".into())),
            "null field must not leak into general_settings"
        );
    }
}
