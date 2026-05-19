mod admin_command;
mod extractor;
mod request_log;
mod routes;
mod state;

use axum::routing::{delete, get, post, put};
use axum::Router;
use clap::Parser;
use state::AppState;
use tower_http::cors::CorsLayer;

#[derive(Parser, Debug)]
#[command(name = "boom-gateway", about = "BooMGateway — High-performance LLM API Gateway")]
struct Args {
    /// Path to config YAML file.
    #[arg(short, long, default_value = "config.yaml")]
    config: String,

    /// Bind host (overrides config file).
    #[arg(long)]
    host: Option<String>,

    /// Bind port (overrides config file).
    #[arg(long)]
    port: Option<u16>,

    /// Gracefully stop any running boom-gateway instance before starting.
    /// Probes /health endpoint: if the old process is frozen (no response),
    /// force-kills it immediately instead of waiting for a fixed timeout.
    #[arg(long)]
    reboot: bool,
}

#[tokio::main(worker_threads = 32)]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let _tracing_guard = init_tracing();
    tracing::info!("BooMGateway starting...");

    // Handle --reboot: gracefully stop any existing instance.
    if args.reboot {
        graceful_restart(args.port)?;
    }

    // Load config.
    let config = boom_config::load_config(&args.config)?;

    // CLI overrides.
    let host = args.host.unwrap_or(config.server.host.clone());
    let port = args.port.unwrap_or(config.server.port);

    // Build state (connects DB, initializes providers).
    let mut state = AppState::from_config(config, args.config.clone()).await?;

    // Shutdown broadcast channel: send once to cancel all background tasks.
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    // Spawn SIGHUP reload listener.
    spawn_sighup_listener(state.clone(), shutdown_tx.subscribe());

    // Spawn background sync task (persist rate limit state + cleanup memory).
    spawn_sync_task(state.clone(), shutdown_tx.subscribe());

    // Spawn request summary logger (every 60s).
    spawn_request_summary(state.request_count.clone(), shutdown_tx.subscribe());

    // Spawn periodic FC dispatch (every 1s — prevents idle capacity).
    spawn_periodic_fc_dispatch(state.flow_controller.clone(), shutdown_tx.subscribe());

    // Build router.
    let app = build_router(state.clone());

    // Start server.
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("BooMGateway listening on {}", addr);

    let server = axum::serve(listener, app);

    // Ctrl+C: stop server → close DB → exit.
    // tokio::select! drops the losing future, so when shutdown_signal wins,
    // the server (and its Router / Extension<DashboardState> / admin_tx) is dropped,
    // which causes admin_command_handler's mpsc channel to close and exit.
    tokio::select! {
        result = server => {
            if let Err(e) = result {
                tracing::error!("Server error: {}", e);
            }
        }
        _ = shutdown_signal() => {
            tracing::info!("Shutting down...");

            // 1. Signal all background tasks to stop.
            let _ = shutdown_tx.send(());

            // 2. Close DB pool — releases all connections and table locks.
            //    Server future was just dropped by select!, so admin_tx is dropped,
            //    admin_command_handler exits, returns its DB connections.
            //    Background tasks received shutdown signal and will exit promptly.
            if let Some(pool) = state.db_pool.take() {
                pool.close().await;
                tracing::info!("Database pool closed");
            }

            tracing::info!("Shutdown complete");
        }
    }

    Ok(())
}

fn build_router(state: AppState) -> Router {
    let api_routes = Router::new()
        // Primary routes (with /v1 prefix)
        .route("/v1/chat/completions", post(routes::chat_completions))
        .route("/v1/messages", post(routes::messages))
        .route("/v1/models", get(routes::list_models))
        .route("/v1/models/{id}", get(routes::get_model))
        .route("/v1/completions", post(routes::completions))
        // Alias routes (without /v1 prefix — OpenAI client compatibility)
        .route("/chat/completions", post(routes::chat_completions))
        .route("/completions", post(routes::completions))
        .route("/models", get(routes::list_models))
        .route("/models/{id}", get(routes::get_model))
        // Unsupported endpoints (return proper errors)
        .route("/v1/embeddings", post(routes::embeddings))
        .route("/v1/audio/speech", post(routes::audio_speech))
        .route("/v1/audio/transcriptions", post(routes::audio_transcriptions))
        .route("/v1/moderations", post(routes::moderations));

    // Health check routes (no auth required).
    let health_routes = Router::new()
        .route("/health", get(routes::health_check))
        .route("/health/live", get(routes::liveness_check))
        .route("/health/ready", get(routes::readiness_check));

    // Admin routes (require master key).
    let admin_routes = Router::new()
        .route("/admin/config/reload", post(routes::admin_reload_config))
        // Plan management.
        .route(
            "/admin/plans",
            put(routes::admin_upsert_plan).get(routes::admin_list_plans),
        )
        .route("/admin/plans/{name}", delete(routes::admin_delete_plan))
        .route("/admin/plans/assign", post(routes::admin_assign_key))
        .route(
            "/admin/plans/assign/{key_hash}",
            delete(routes::admin_unassign_key),
        )
        .route(
            "/admin/plans/assignments",
            get(routes::admin_list_assignments),
        );

    // Dashboard router (Web UI + dashboard API).
    // Returns Router<()> — state injected via Extension<Arc<DashboardState>>.
    let master_key = state.inner.load().config.general_settings.master_key.clone();

    // Admin command channel: dashboard sends write ops, boom-main handles them.
    let (admin_tx, admin_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(admin_command::admin_command_handler(admin_rx, state.clone()));

    let dashboard_state = boom_dashboard::DashboardState::new(
        state.db_pool.clone(),
        state.plan_store.clone(),
        state.limiter.clone(),
        state.deployment_store.clone(),
        state.alias_store.clone(),
        state.inflight.clone(),
        state.flow_controller.clone(),
        admin_tx,
        master_key,
        state.debug_store.clone(),
        state.prompt_log_writer.clone(),
    );
    let dashboard_router = boom_dashboard::build_router(dashboard_state);

    let request_count = state.request_count.clone();
    Router::new()
        .merge(api_routes)
        .merge(health_routes)
        .merge(admin_routes)
        .merge(dashboard_router)
        .with_state(state)
        .layer(CorsLayer::permissive())
        .layer(axum::middleware::from_fn(move |req: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| {
            let count = request_count.clone();
            async move {
                let path = req.uri().path();
                if path.starts_with("/v1/") || path.starts_with("/admin/")
                    || path.starts_with("/chat/") || path.starts_with("/completions")
                    || path.starts_with("/models")
                {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                next.run(req).await
            }
        }))
}

/// Listen for SIGHUP and trigger hot-reload.
fn spawn_sighup_listener(state: AppState, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    #[cfg(unix)]
    {
        tokio::spawn(async move {
            let mut stream = match tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::hangup(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to install SIGHUP handler: {}", e);
                    return;
                }
            };

            loop {
                tokio::select! {
                    _ = stream.recv() => {
                        tracing::info!("Received SIGHUP — triggering hot-reload...");
                        match state.reload().await {
                            Ok(summary) => tracing::info!("SIGHUP reload: {}", summary),
                            Err(e) => tracing::error!("SIGHUP reload failed: {}", e),
                        }
                    }
                    _ = shutdown.recv() => {
                        tracing::debug!("SIGHUP listener shutting down");
                        return;
                    }
                }
            }
        });
        tracing::info!("SIGHUP hot-reload listener installed");
    }

    #[cfg(not(unix))]
    {
        tracing::info!("SIGHUP not supported on this platform — use POST /admin/config/reload");
        let _ = state; // suppress unused warning
        let _ = shutdown;
    }
}

/// Background task: every 60s, log a summary of requests processed in the last minute.
fn spawn_request_summary(
    request_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    use std::sync::atomic::Ordering;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
                _ = shutdown.recv() => {
                    tracing::debug!("Request summary task shutting down");
                    return;
                }
            }

            let count = request_count.swap(0, Ordering::Relaxed);
            if count > 0 {
                tracing::info!("Requests in last minute: {}", count);
            }
        }
    });
    tracing::info!("Request summary logger spawned (every 60s)");
}

/// Background task: every 1s, trigger dispatch on all flow control slots.
/// Ensures queued requests are dispatched promptly even if no guard drops occur.
fn spawn_periodic_fc_dispatch(
    flow_controller: std::sync::Arc<boom_flowcontrol::FlowController>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    flow_controller.periodic_dispatch();
                }
                _ = shutdown.recv() => {
                    tracing::debug!("Periodic FC dispatch task shutting down");
                    return;
                }
            }
        }
    });
    tracing::info!("Periodic FC dispatch task spawned (every 1s)");
}

/// Background task: every 10 minutes, snapshot in-memory state to DB and cleanup.
fn spawn_sync_task(state: AppState, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    let db_pool = state.db_pool.clone();
    let limiter = state.limiter.clone();
    let plan_store = state.plan_store.clone();

    tokio::spawn(async move {
        let pool = match db_pool {
            Some(p) => p,
            None => return, // No DB — nothing to persist.
        };

        // Initial delay to let startup traffic settle.
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
            _ = shutdown.recv() => {
                tracing::debug!("Sync task shutting down during initial delay");
                return;
            }
        }

        loop {
            // Wait for next sync cycle or shutdown signal.
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(600)) => {}
                _ = shutdown.recv() => {
                    tracing::debug!("Sync task shutting down");
                    return;
                }
            }

            // 1. Snapshot rate limit counters → upsert into DB (with timeout).
            let entries = limiter.snapshot();
            if !entries.is_empty() {
                let count = entries.len();
                let batch_result = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    limiter.sync_counters_to_db(&pool),
                )
                .await;
                if batch_result.is_err() {
                    tracing::warn!("Rate limit state sync timed out after 30s, {} entries skipped", count);
                } else {
                    tracing::debug!("Synced {} rate limit counter(s) to DB", count);
                }
            }

            // 2. Snapshot assignments → upsert into DB (with timeout).
            let assignments = plan_store.snapshot_assignments();
            if !assignments.is_empty() {
                let assign_count = assignments.len();
                let batch_result = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    plan_store.sync_assignments_to_db(&pool),
                )
                .await;
                if batch_result.is_err() {
                    tracing::warn!("Assignment sync timed out after 30s, {} entries skipped", assign_count);
                }
            }

            // 3. Cleanup stale in-memory entries.
            let expired = limiter.cleanup_expired();
            let concurrency_freed = plan_store.cleanup_concurrency();
            if expired > 0 || concurrency_freed > 0 {
                tracing::info!(
                    "Cleanup: removed {} expired window(s), {} idle concurrency counter(s)",
                    expired,
                    concurrency_freed
                );
            }
        }
    });
    tracing::info!("Background sync task spawned (every 10 min)");
}

/// Initialize tracing with a non-blocking writer.
///
/// `tracing_appender::non_blocking` offloads log I/O to a dedicated thread,
/// so stdout writes never block the tokio runtime (critical for Docker where
/// the log driver may introduce back-pressure).
fn init_tracing() -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::EnvFilter;

    let (non_blocking, guard) = tracing_appender::non_blocking(std::io::stdout());

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .with_target(false)
        .with_writer(non_blocking)
        .init();

    guard
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C");
        },
        _ = terminate => {
            tracing::info!("Received SIGTERM");
        },
    }
}

/// Find running boom-gateway processes, send SIGTERM, then use HTTP health probing
/// to determine if the old process is still responsive:
///   - /health responds   → runtime alive, graceful shutdown in progress, keep waiting
///   - Connection refused → listener closed, shutting down, keep waiting
///   - Timeout (no response) → runtime frozen, SIGKILL immediately
fn graceful_restart(port_hint: Option<u16>) -> anyhow::Result<()> {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let self_pid = std::process::id();

    // Find boom-gateway processes via pgrep (exact name match).
    let output = match Command::new("pgrep").args(["-x", "boom-gateway"]).output() {
        Ok(o) => o,
        Err(_) => {
            tracing::warn!("pgrep not available, skipping graceful restart");
            return Ok(());
        }
    };

    let pids: Vec<u32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .filter(|&pid| pid != self_pid)
        .collect();

    if pids.is_empty() {
        tracing::info!("No existing boom-gateway process found");
        return Ok(());
    }

    tracing::info!(
        "Found {} running boom-gateway process(es): {:?}",
        pids.len(),
        pids
    );

    // Determine port for health probing: --port arg → old process cmdline.
    let port = port_hint
        .or_else(|| pids.first().and_then(|&pid| parse_port_from_pid(pid)));

    // Send SIGTERM for graceful shutdown.
    for &pid in &pids {
        let _ = Command::new("kill")
            .args(["-s", "TERM", &pid.to_string()])
            .output();
    }
    tracing::info!("Sent SIGTERM, waiting for graceful shutdown...");

    // Hard limit: 30 seconds maximum (should never hit this in practice).
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        if Instant::now() > deadline {
            tracing::warn!("Hard timeout (30s), force killing remaining process(es)");
            force_kill_alive(&pids);
            return Ok(());
        }

        let alive: Vec<u32> = pids
            .iter()
            .filter(|&&pid| is_process_alive(pid))
            .copied()
            .collect();

        if alive.is_empty() {
            tracing::info!("Old process(es) exited gracefully");
            return Ok(());
        }

        // Probe health endpoint to detect frozen process.
        if let Some(p) = port {
            match probe_health(p) {
                HealthProbe::Responsive | HealthProbe::ConnectionRefused => {
                    // Process is alive — either still serving or closing listener.
                    // Normal graceful shutdown in progress, keep waiting.
                }
                HealthProbe::Timeout => {
                    // Process alive but not responding → frozen.
                    tracing::warn!(
                        "Health check timed out — old process frozen, force killing: {:?}",
                        alive
                    );
                    force_kill_alive(&alive);
                    return Ok(());
                }
            }
        }

        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Check if a process is still alive (signal 0 = existence check, no signal delivered).
fn is_process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Force kill (SIGKILL) all alive processes in the list.
fn force_kill_alive(pids: &[u32]) {
    for &pid in pids {
        if is_process_alive(pid) {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
}

/// Parse --port from a running process's command line via /proc.
fn parse_port_from_pid(pid: u32) -> Option<u16> {
    let cmdline = std::fs::read_to_string(format!("/proc/{}/cmdline", pid)).ok()?;
    let args: Vec<&str> = cmdline.split('\0').filter(|s| !s.is_empty()).collect();
    for i in 0..args.len() {
        if args[i] == "--port" && i + 1 < args.len() {
            return args[i + 1].parse().ok();
        }
    }
    None
}

/// Result of an HTTP health probe against the gateway.
enum HealthProbe {
    /// Gateway returned an HTTP response — runtime is functional.
    Responsive,
    /// TCP connection refused — listener closed (shutting down or exited).
    ConnectionRefused,
    /// Connection or response timed out — gateway is frozen.
    Timeout,
}

/// Probe the gateway's /health endpoint to determine if it's responsive.
/// Uses raw TCP + HTTP to avoid pulling in an HTTP client dependency.
fn probe_health(port: u16) -> HealthProbe {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let addr = format!("127.0.0.1:{}", port);
    let addr: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(_) => return HealthProbe::ConnectionRefused,
    };

    let mut stream = match TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)) {
        Ok(s) => s,
        Err(e) => {
            return if e.kind() == std::io::ErrorKind::TimedOut {
                HealthProbe::Timeout
            } else {
                HealthProbe::ConnectionRefused
            };
        }
    };

    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(1)));

    if stream
        .write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .is_err()
    {
        return HealthProbe::ConnectionRefused;
    }

    let mut buf = [0u8; 128];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => HealthProbe::Responsive,
        _ => HealthProbe::Timeout,
    }
}
