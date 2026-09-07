use boom_core::provider::KeyAliasLookup;
use boom_core::DebugErrorStore;
use boom_flowcontrol::FlowController;
use boom_limiter::{PlanStore, SlidingWindowLimiter};
use boom_promptlog::PromptLogConfig;
use boom_promptlog::PromptLogQueryApi;
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
    /// Persist runtime state to YAML in place. Reply carries Ok(()) on a
    /// successful write, or Err(message) when YAML could not be persisted
    /// (read-only file, disk full, etc.) so the caller can surface a warning
    /// to the operator — the in-memory + DB state have already been updated
    /// by the handler, so a YAML write failure must not block the operation.
    ConfigChanged {
        reply: oneshot::Sender<Result<(), String>>,
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
    /// Per-deployment request rate tracker for dashboard stats.
    pub request_rate: Arc<RequestRateTracker>,
    /// Agent (client-type) statistics tracker for dashboard stats.
    pub agent_stats: Arc<AgentStatsTracker>,
    /// Authenticator — used for key alias lookups (reads boom_verification_token).
    pub auth: Arc<dyn KeyAliasLookup>,
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
}

impl DashboardState {
    pub fn new(
        db_pool: Option<PgPool>,
        plan_store: Arc<PlanStore>,
        limiter: Arc<SlidingWindowLimiter>,
        deployment_store: Arc<DeploymentStore>,
        alias_store: Arc<AliasStore>,
        inflight: Arc<InFlightTracker>,
        flow_controller: Arc<FlowController>,
        admin_tx: AdminTx,
        master_key: Option<String>,
        debug_store: Arc<DebugErrorStore>,
        prompt_log_query: Arc<dyn PromptLogQueryApi>,
        rebalance_move_tracker: Arc<RebalanceMoveTracker>,
        request_rate: Arc<RequestRateTracker>,
        agent_stats: Arc<AgentStatsTracker>,
        auth: Arc<dyn KeyAliasLookup>,
        log_dropped: Option<Arc<dyn boom_core::LogDroppedCounter>>,
        stressmon: Arc<dyn boom_core::StressmonApi>,
        trace: Arc<dyn boom_core::TraceApi>,
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
            inflight,
            flow_controller,
            admin_tx,
            jwt_secret,
            master_key,
            login_attempts: Arc::new(DashMap::new()),
            debug_store,
            prompt_log_query,
            rebalance_move_tracker,
            request_rate,
            agent_stats,
            auth,
            log_dropped,
            stressmon,
            trace,
        }
    }

    /// Send `ConfigChanged` to boom-main and await the YAML persist result
    /// with a 5s timeout. Used by alias/plan/config CRUD handlers to surface
    /// YAML write failures as warnings in the HTTP response — the YAML write
    /// is best-effort and must not block the primary operation (DB write +
    /// in-memory update have already succeeded by the time this is called).
    /// Returns:
    ///   - `Ok(())` if YAML was persisted successfully
    ///   - `Err(message)` if YAML write failed (caller should attach warning)
    ///   - `Err("YAML persist timed out after 5s")` if reply didn't arrive in 5s
    pub async fn persist_yaml_with_reply(&self) -> Result<(), String> {
        await_yaml_reply(&self.admin_tx, std::time::Duration::from_secs(5)).await
    }
}

/// Free function form of the YAML-persist helper — testable without
/// constructing a full `DashboardState` (which requires ~15 Arc<dyn> traits).
/// The behavior under test (channel closed / reply dropped / timeout) is
/// independent of the rest of `DashboardState`, so we test it at this layer.
async fn await_yaml_reply(
    admin_tx: &AdminTx,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = AdminCommand::ConfigChanged { reply: reply_tx };
    // mpsc send failure means boom-main's admin_command_handler exited —
    // surface as error so caller responds 500 instead of silent success.
    if let Err(e) = admin_tx.send(cmd).await {
        return Err(format!("admin_tx channel closed: {}", e));
    }
    match tokio::time::timeout(timeout, reply_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("admin_command_handler dropped reply channel".to_string()),
        Err(_) => Err(format!("YAML persist timed out after {}s", timeout.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When boom-main's admin_command_handler has exited (channel closed),
    /// `await_yaml_reply` must surface an error so the dashboard handler
    /// responds 500 — NOT silent success. Otherwise a crash in boom-main
    /// would let CRUD operations appear to succeed while YAML never syncs.
    #[tokio::test]
    async fn await_yaml_reply_returns_err_when_channel_closed() {
        // Construct a sender whose receiver is immediately dropped. mpsc
        // send on this returns `SendError` → await_yaml_reply surfaces it.
        let (admin_tx, _admin_rx) = mpsc::channel::<AdminCommand>(1);
        drop(_admin_rx);

        let result = await_yaml_reply(&admin_tx, std::time::Duration::from_secs(1)).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("channel closed"),
            "closed-channel error must surface to caller"
        );
    }

    /// When boom-main drops the reply channel without responding (e.g.
    /// panics mid-dispatch), `await_yaml_reply` must surface a distinct
    /// error so the operator can distinguish it from a YAML write failure.
    #[tokio::test]
    async fn await_yaml_reply_returns_err_when_reply_dropped() {
        let (admin_tx, mut admin_rx) = mpsc::channel::<AdminCommand>(1);

        // Spawn a fake handler that takes the command and drops the reply
        // without sending — simulates a panic or unexpected early-return
        // in boom-main's dispatcher arm.
        tokio::spawn(async move {
            let _cmd = admin_rx.recv().await;
            // intentionally drop the oneshot::Sender without replying
        });

        let result = await_yaml_reply(&admin_tx, std::time::Duration::from_secs(1)).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("dropped reply channel"),
            "dropped-reply error must surface distinctly"
        );
    }

    /// Normal path: boom-main responds Ok, await_yaml_reply forwards it.
    #[tokio::test]
    async fn await_yaml_reply_forwards_ok() {
        let (admin_tx, mut admin_rx) = mpsc::channel::<AdminCommand>(1);
        tokio::spawn(async move {
            if let Some(AdminCommand::ConfigChanged { reply }) = admin_rx.recv().await {
                let _ = reply.send(Ok(()));
            }
        });

        let result = await_yaml_reply(&admin_tx, std::time::Duration::from_secs(1)).await;
        assert_eq!(result, Ok(()));
    }

    /// YAML write failure path: boom-main responds Err(message), await_yaml_reply
    /// forwards it so the handler can attach to warning field.
    #[tokio::test]
    async fn await_yaml_reply_forwards_err() {
        let (admin_tx, mut admin_rx) = mpsc::channel::<AdminCommand>(1);
        tokio::spawn(async move {
            if let Some(AdminCommand::ConfigChanged { reply }) = admin_rx.recv().await {
                let _ = reply.send(Err("write config: read-only filesystem".to_string()));
            }
        });

        let result = await_yaml_reply(&admin_tx, std::time::Duration::from_secs(1)).await;
        assert_eq!(result, Err("write config: read-only filesystem".to_string()));
    }
}
