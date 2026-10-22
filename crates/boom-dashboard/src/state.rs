use boom_core::provider::KeyAliasLookup;
use boom_core::DebugErrorStore;
use boom_flowcontrol::FlowController;
use boom_limiter::{PlanStore, SlidingWindowLimiter};
use boom_promptlog::PromptLogConfig;
use boom_promptlog::PromptLogQueryApi;
use boom_routing::{AliasStore, CostTemplateStore, DeploymentStore, InFlightTracker, KvcSchedulerStats, RebalanceMoveTracker, RequestRateTracker, WorkflowStore};
use boom_ctxaware::AgentStatsTracker;
use dashmap::DashMap;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::handlers_admin::CreateDeploymentRequest;

/// Tracks login failure state per IP for brute-force protection.
#[derive(Debug)]
pub struct LoginAttempt {
    pub fail_count: u32,
    pub locked_until: Option<Instant>,
}

// ═══════════════════════════════════════════════════════════
// Admin Command channel (write operations → boom-main)
// ═══════════════════════════════════════════════════════════

/// Commands sent from dashboard to boom-main for state-mutating operations.
/// Model CRUD requires boom-provider + boom-config, which dashboard must not depend on.
pub enum AdminCommand {
    CreateModel {
        req: CreateDeploymentRequest,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    UpdateModel {
        id: Uuid,
        req: CreateDeploymentRequest,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    DeleteModel {
        id: Uuid,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Explicit "import from YAML" pass: import model deployments present in
    /// the YAML model_list but missing from the DB (matched by model_name;
    /// existing models are never overwritten). Boom-main owns the YAML
    /// parsing; the reply reports imported / skipped / failed model names.
    ImportModelsFromYaml {
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Save a workflow definition (create or replace). Boom-main parses the
    /// raw definition JSON against `WorkflowDefinitionConfig`, validates the
    /// resulting settings snapshot, writes DB, then re-registers fusion.
    UpsertWorkflow {
        req: crate::handlers_admin::UpsertWorkflowRequest,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Delete a workflow definition. Rejected while model bindings still
    /// reference it (boom-main checks the live store).
    DeleteWorkflow {
        name: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Bind a virtual model name to a workflow (create or replace).
    /// Validation runs against the scratch snapshot before any write.
    UpsertWorkflowModel {
        req: crate::handlers_admin::UpsertWorkflowModelRequest,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Unbind a workflow model (tears down the exclusive deployment).
    DeleteWorkflowModel {
        model_name: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Save a cost template and re-derive the cost rate of every model
    /// bound to it (`model_info.cost_template = name`).
    UpsertCostTemplate {
        req: crate::handlers_admin::UpsertCostTemplateRequest,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Delete a cost template. Affected models keep their
    /// `model_info.cost_template` reference; their rates are re-derived
    /// (lookup stops resolving → rate clears).
    DeleteCostTemplate {
        name: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Hot-reload config.yaml. Reply contains summary message.
    ReloadConfig {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Read the live in-memory config as JSON (secrets masked).
    GetConfig {
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Read the field manifest — declarative list of which config fields
    /// are editable from the dashboard UI. See boom-config `manifest` module
    /// and CLAUDE.md §9. Used by `GET /admin/config/schema` and (future)
    /// auto-rendering frontend code.
    GetConfigSchema {
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// Compose the full current config as a YAML document (runtime sections
    /// from the DB snapshot, everything else verbatim from the raw YAML on
    /// disk — `${VAR}` references preserved). Reply carries the YAML text.
    /// Backs the config page's export/download button.
    ExportConfig {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Write the full current config to the startup YAML file (rolls a
    /// `.bak` first). Manual recovery-snapshot refresh — a rollback seed for
    /// older binaries and a disaster-recovery artifact, NOT a sync mechanism:
    /// the file starts going stale the moment this returns. Backs the config
    /// page's write-back button.
    WriteBackConfig {
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Update a singleton config section (e.g., `server`, `router_settings`)
    /// by replacing it wholesale in the live `config.yaml`. Path is dotted
    /// (`router_settings.kvc_aware`); value is the new JSON-serializable content.
    /// Triggers a reload after writing.
    UpdateConfigSection {
        path: String,
        value: Value,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Hot-swap the prompt-log config at runtime (toggle on/off, change
    /// exclusion lists, flip the otlp sub-config). Boom-main owns the
    /// `PromptLogWriter`; dashboard must not touch the writer handle directly
    /// (see CLAUDE.md §5 AdminCommand pattern).
    UpdatePromptLogConfig {
        config: PromptLogConfig,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Probe a remote OTLP/HTTP collector. The dashboard passes the live
    /// endpoint/headers/timeout (read from the prompt-log card form — not the
    /// committed YAML, so the operator can type a new endpoint and test it
    /// before saving). Reply carries the round-trip latency in ms on success
    /// or a one-line error string on failure.
    PingOtlpEndpoint {
        endpoint: String,
        headers: std::collections::HashMap<String, String>,
        timeout_secs: u64,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    /// Read the live OTLP exporter's state machine snapshot — Online/Offline
    /// status, endpoint, last_failure_ts, last_recovery_ts, episode counters,
    /// dropped counts. The dashboard polls this every 5s to drive the
    /// connectivity indicator (green=online, red=offline, gray=disabled).
    /// Returns `None` when OTLP is not configured (no exporter in the
    /// ArcSwap); the dashboard treats `None` as "disabled".
    GetOtlpStatus {
        reply: oneshot::Sender<Option<boom_promptlog::ExporterStatusSnapshot>>,
    },
    /// Manually trigger a probe on the live exporter. On success transitions
    /// Offline → Online; on failure records a probe failure (but does NOT
    /// drive Online → Offline — only repeated flush failures do that). Used
    /// by the dashboard's "Probe now" action when the operator wants to
    /// attempt recovery before the next periodic tick. Returns `None` when
    /// OTLP is not configured.
    ProbeOtlp {
        reply: oneshot::Sender<Option<boom_promptlog::ProbeResult>>,
    },
    /// Probe a remote OTLP/HTTP collector for traces. Same shape as
    /// `PingOtlpEndpoint` but sends an `ExportTraceServiceRequest` instead
    /// of an `ExportLogsServiceRequest`. The dashboard's trace card Test
    /// button calls this so the operator can type a new endpoint and test
    /// it before saving.
    PingTraceOtlpEndpoint {
        endpoint: String,
        headers: std::collections::HashMap<String, String>,
        timeout_secs: u64,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    /// Invalidate the authenticator's cached token rows after a dashboard
    /// write to `boom_verification_token` (update/block/unblock/delete),
    /// so team reassignment, block, budget or model-list changes take effect
    /// on the next request instead of waiting out the cache TTL. Fire-and-
    /// forget: the DB write already succeeded, so a dead channel (boom-main
    /// shutting down) needs no error surface.
    InvalidateAuthCache {
        token_hashes: Vec<String>,
    },
}

pub type AdminTx = mpsc::Sender<AdminCommand>;

// ═══════════════════════════════════════════════════════════
// Dashboard state
// ═══════════════════════════════════════════════════════════

/// Dashboard-specific state, injected via Extension layer.
/// Independent from boom-gateway's AppState to avoid type coupling.
#[derive(Clone)]
pub struct DashboardState {
    /// Dashboard-only DB pool (max=3), isolated from the forwarding path's
    /// pool (max=30). Heavy stats aggregations cannot starve request forwarding.
    pub db_pool: Option<PgPool>,
    pub plan_store: Arc<PlanStore>,
    pub limiter: Arc<SlidingWindowLimiter>,
    /// Deployment store for model reads.
    pub deployment_store: Arc<DeploymentStore>,
    /// Alias store for alias reads.
    pub alias_store: Arc<AliasStore>,
    /// Workflow store — list reads + alias-delete guards (workflows can
    /// reference aliases as panel/aggregator members). Writes go through
    /// `AdminCommand` (fusion re-registration needs boom-main).
    pub workflow_store: Arc<WorkflowStore>,
    /// Cost template store — list reads. Writes go through `AdminCommand`
    /// (re-rating affected models needs boom-main).
    pub cost_template_store: Arc<CostTemplateStore>,
    /// In-flight request tracker for real-time stats.
    pub inflight: Arc<InFlightTracker>,
    /// Per-deployment flow controller for real-time stats.
    pub flow_controller: Arc<FlowController>,
    /// Channel for model write operations (handled by boom-main).
    pub admin_tx: AdminTx,
    /// JWT signing key (derived from master_key at startup).
    pub jwt_secret: String,
    /// Master key for admin login (constant-time comparison).
    pub master_key: Option<String>,
    /// Login rate-limit state per client IP.
    pub login_attempts: Arc<DashMap<String, LoginAttempt>>,
    /// Debug error store — shared with boom-main for recording upstream errors.
    pub debug_store: Arc<DebugErrorStore>,
    /// Read-only prompt-log query API — entry lookup + config snapshot.
    /// Boom-main owns the writer; writes go through `AdminCommand::UpdatePromptLogConfig`.
    pub prompt_log_query: Arc<dyn PromptLogQueryApi>,
    /// Per-deployment rebalance move tracker (in/out) for dashboard debug page.
    pub rebalance_move_tracker: Arc<RebalanceMoveTracker>,
    /// Global kvc_aware scheduling counters (total / affinity / cold /
    /// lowest-load fallback / rebalance) for dashboard debug page.
    pub kvc_stats: Arc<KvcSchedulerStats>,
    /// Per-deployment request rate tracker for dashboard stats.
    pub request_rate: Arc<RequestRateTracker>,
    /// Agent (client-type) statistics tracker for dashboard stats.
    pub agent_stats: Arc<AgentStatsTracker>,
    /// Authenticator — used for key alias lookups (reads boom_verification_token).
    pub auth: Arc<dyn KeyAliasLookup>,
    /// Client blocklist store (boom-gatekeeper) — rule CRUD + hot-path check
    /// share the same compiled rules.
    pub block_rule_store: Arc<boom_gatekeeper::BlockRuleStore>,
    /// Audit-log drop counter (channel full or batch INSERT failures).
    /// None when DB not configured (no LogWriter). Surfaced on the debug page.
    pub log_dropped: Option<Arc<dyn boom_core::LogDroppedCounter>>,
    /// Real-time pressure metrics (CPU, RSS, tokio worker queue depth,
    /// blocking pool queue, inflight). Polled every 1.5s by the admin
    /// stats page's top sparkline chart.
    pub stressmon: Arc<dyn boom_core::StressmonApi>,
    /// Trace channel — active span table + slow ring + OTLP traces exporter
    /// status. Erased to `Arc<dyn TraceApi>` so boom-dashboard stays leaf-of-
    /// boom-core (no dep on boom-trace). Polled by the admin trace page.
    pub trace: Arc<dyn boom_core::TraceApi>,
    /// Alert state — active alerts + history ring, maintained by boom-main's
    /// alert reconciler. Erased to `Arc<dyn boom_core::AlertApi>` so
    /// boom-dashboard doesn't depend on boom-alert. Read by the status page.
    pub alerts: Arc<dyn boom_core::AlertApi>,
}

impl DashboardState {
    pub fn new(
        db_pool: Option<PgPool>,
        plan_store: Arc<PlanStore>,
        limiter: Arc<SlidingWindowLimiter>,
        deployment_store: Arc<DeploymentStore>,
        alias_store: Arc<AliasStore>,
        workflow_store: Arc<WorkflowStore>,
        cost_template_store: Arc<CostTemplateStore>,
        inflight: Arc<InFlightTracker>,
        flow_controller: Arc<FlowController>,
        admin_tx: AdminTx,
        master_key: Option<String>,
        debug_store: Arc<DebugErrorStore>,
        prompt_log_query: Arc<dyn PromptLogQueryApi>,
        rebalance_move_tracker: Arc<RebalanceMoveTracker>,
        kvc_stats: Arc<KvcSchedulerStats>,
        request_rate: Arc<RequestRateTracker>,
        agent_stats: Arc<AgentStatsTracker>,
        auth: Arc<dyn KeyAliasLookup>,
        block_rule_store: Arc<boom_gatekeeper::BlockRuleStore>,
        log_dropped: Option<Arc<dyn boom_core::LogDroppedCounter>>,
        stressmon: Arc<dyn boom_core::StressmonApi>,
        trace: Arc<dyn boom_core::TraceApi>,
        alerts: Arc<dyn boom_core::AlertApi>,
    ) -> Self {
        // Derive JWT secret from master_key, or use a random fallback.
        let jwt_secret = master_key
            .as_deref()
            .unwrap_or("boom-dashboard-default-secret")
            .to_string();
        Self {
            db_pool,
            plan_store,
            limiter,
            deployment_store,
            alias_store,
            workflow_store,
            cost_template_store,
            inflight,
            flow_controller,
            admin_tx,
            jwt_secret,
            master_key,
            login_attempts: Arc::new(DashMap::new()),
            debug_store,
            prompt_log_query,
            rebalance_move_tracker,
            kvc_stats,
            request_rate,
            agent_stats,
            auth,
            block_rule_store,
            log_dropped,
            stressmon,
            trace,
            alerts,
        }
    }
}
