mod backends;
mod blacklist;
mod client;
mod config;
mod health;
mod logging;
mod proxy;
mod routes;

use arc_swap::ArcSwap;
use clap::Parser;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::server::Server;
use pingora_proxy::http_proxy_service;
use std::collections::HashSet;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::backends::{build_rings, collect_all_backends};
use crate::blacklist::load_blacklist_state;
use crate::config::Config;
use crate::health::run_health_probe;
use crate::logging::init_logging;
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
    let args = Args::parse();
    // Resolve to an absolute path so hot-reload watching works no matter how
    // the process was invoked (e.g. `-c config.yaml` from a subdirectory).
    let config_path = fs::canonicalize(&args.config)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| args.config);
    init_logging("gateway-lb");
    let config = Config::load(&config_path).expect("failed to load config");

    let listen_port = config.listen_port;
    let tls_config = config.tls.clone();

    let mut server = Server::new(None).unwrap();
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
