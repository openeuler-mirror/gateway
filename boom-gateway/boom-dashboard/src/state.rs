use boom_core::provider::KeyAliasLookup;
use boom_core::DebugErrorStore;
use boom_flowcontrol::FlowController;
use boom_limiter::{PlanStore, SlidingWindowLimiter};
use boom_promptlog::PromptLogWriter;
use boom_quota::QuotaStore;
use boom_routing::{AliasStore, DeploymentStore, InFlightTracker, RebalanceMoveTracker, RequestRateTracker};
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
    /// Fire-and-forget: config changed, dump snapshot.
    ConfigChanged,
    /// Hot-reload config.yaml. Reply contains summary message.
    ReloadConfig {
        reply: oneshot::Sender<Result<String, String>>,
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
    /// Quota store — multi-dim (tokens/costs) window + cumulative counters.
    pub quota_store: Arc<QuotaStore>,
    /// Deployment store for model reads.
    pub deployment_store: Arc<DeploymentStore>,
    /// Alias store for alias reads.
    pub alias_store: Arc<AliasStore>,
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
    /// Prompt log writer — shared with boom-main for runtime config control.
    pub prompt_log_writer: PromptLogWriter,
    /// Per-deployment rebalance move tracker (in/out) for dashboard debug page.
    pub rebalance_move_tracker: Arc<RebalanceMoveTracker>,
    /// Per-deployment request rate tracker for dashboard stats.
    pub request_rate: Arc<RequestRateTracker>,
    /// Agent (client-type) statistics tracker for dashboard stats.
    pub agent_stats: Arc<AgentStatsTracker>,
    /// Authenticator — used for key alias lookups (reads boom_verification_token).
    pub auth: Arc<dyn KeyAliasLookup>,
}

impl DashboardState {
    pub fn new(
        db_pool: Option<PgPool>,
        plan_store: Arc<PlanStore>,
        limiter: Arc<SlidingWindowLimiter>,
        quota_store: Arc<QuotaStore>,
        deployment_store: Arc<DeploymentStore>,
        alias_store: Arc<AliasStore>,
        inflight: Arc<InFlightTracker>,
        flow_controller: Arc<FlowController>,
        admin_tx: AdminTx,
        master_key: Option<String>,
        debug_store: Arc<DebugErrorStore>,
        prompt_log_writer: PromptLogWriter,
        rebalance_move_tracker: Arc<RebalanceMoveTracker>,
        request_rate: Arc<RequestRateTracker>,
        agent_stats: Arc<AgentStatsTracker>,
        auth: Arc<dyn KeyAliasLookup>,
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
            quota_store,
            deployment_store,
            alias_store,
            inflight,
            flow_controller,
            admin_tx,
            jwt_secret,
            master_key,
            login_attempts: Arc::new(DashMap::new()),
            debug_store,
            prompt_log_writer,
            rebalance_move_tracker,
            request_rate,
            agent_stats,
            auth,
        }
    }
}
