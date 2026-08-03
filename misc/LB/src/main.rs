mod backends;
mod blacklist;
mod client;
mod config;
mod health;
mod logging;
mod metrics;
mod proxy;
mod routes;

use arc_swap::ArcSwap;
use clap::Parser;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::server::Server;
use pingora_proxy::http_proxy_service;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use crate::backends::{build_rings, collect_all_backends};
use crate::blacklist::load_blacklist_state;
use crate::config::Config;
use crate::health::run_health_probe;
use crate::logging::init_logging;
use crate::metrics::Metrics;
use crate::proxy::{watch_config, ConfigSnapshot, Gateway};

#[derive(Parser, Debug)]
#[clap(version, about, long_about = None)]
struct Args {
    #[arg(
        short,
        long,
        env = "CONFIG_PATH",
        default_value = "/etc/gateway/routes.yaml"
    )]
    config: String,
}

fn main() {
    // Pingora creates one tokio runtime whose worker count comes from
    // `worker_threads` (default: CPU cores). Every tokio worker is a std
    // thread, and Rust's default thread stack is only 2 MiB (RUST_MIN_STACK).
    // On high-core machines (e.g. 192 CPUs => 192 workers) the combined
    // proxy + TLS/HTTP-2 async state machine has been observed to overflow
    // that 2 MiB stack ("thread ... has overflowed its stack"), especially
    // with the toolchain used by the Docker build. Bump the default to 8 MiB
    // before any thread is spawned, unless the operator already set it.
    if std::env::var_os("RUST_MIN_STACK").is_none() {
        std::env::set_var("RUST_MIN_STACK", (8 * 1024 * 1024).to_string());
    }

    let args = Args::parse();
    // Resolve to an absolute path so hot-reload watching works no matter how
    // the process was invoked (e.g. `-c config.yaml` from a subdirectory).
    let config_path = fs::canonicalize(&args.config)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| args.config);
    init_logging("gateway-lb");
    let config = Config::load(&config_path).expect("failed to load config");
    log::info!(
        "starting gateway-lb: worker_threads={} max_retries={} threads_stack_mib={}",
        config.worker_threads,
        config.max_retries,
        std::env::var_os("RUST_MIN_STACK")
            .and_then(|v| v.to_string_lossy().parse::<u64>().ok())
            .map(|bytes| bytes / (1024 * 1024))
            .unwrap_or(2),
    );

    let listen_port = config.listen_port;
    let tls_config = config.tls.clone();

    let mut server = Server::new(None).unwrap();
    // Pingora defaults to a single worker thread and 16 retries; both are
    // tuned from config (threads = CPU count, retries = 3 unless overridden).
    // The config Arc is not shared with any service yet, so get_mut is safe.
    let server_conf = Arc::get_mut(&mut server.configuration)
        .expect("server configuration must not be shared before tuning");
    server_conf.threads = config.worker_threads;
    server_conf.max_retries = config.max_retries;
    server.bootstrap();

    let blacklist_init = load_blacklist_state(config.blacklist.as_deref());
    let snapshot = Arc::new(ArcSwap::from_pointee(ConfigSnapshot {
        rings: build_rings(&config),
        backends: collect_all_backends(&config),
        config,
    }));
    let unhealthy = Arc::new(ArcSwap::from_pointee(HashSet::<SocketAddr>::new()));
    let blacklist = Arc::new(ArcSwap::from_pointee(blacklist_init));
    run_health_probe(snapshot.clone(), unhealthy.clone());
    watch_config(config_path, snapshot.clone(), blacklist.clone());

    let gateway = Gateway {
        snapshot,
        unhealthy,
        blacklist,
        metrics: Metrics::default(),
        block_warn: Mutex::new(HashMap::new()),
    };

    let mut proxy = http_proxy_service(&server.configuration, gateway);

    // Bind HTTP and HTTPS listeners independently: the TLS port is served when
    // `tls` is configured, and the plaintext port whenever `listen_port` is
    // set (or as the default when no TLS is configured).
    if let Some(ref tls) = tls_config {
        let mut tls_settings =
            TlsSettings::intermediate(&tls.cert, &tls.key).expect("failed to create TLS settings");
        tls_settings.enable_h2();
        proxy.add_tls_with_settings(&format!("0.0.0.0:{}", tls.port), None, tls_settings);
    }
    match listen_port {
        Some(port) => proxy.add_tcp(&format!("0.0.0.0:{port}")),
        None if tls_config.is_none() => proxy.add_tcp("0.0.0.0:6198"),
        None => {}
    }

    server.add_service(proxy);
    server.run_forever();
}
