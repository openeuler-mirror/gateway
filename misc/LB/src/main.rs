use async_trait::async_trait;
use clap::Parser;
use ipnet::IpNet;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Parser, Debug)]
#[clap(version, about, long_about = None)]
struct Args {
    #[arg(short, long, env = "CONFIG_PATH", default_value = "/etc/gateway/routes.yaml")]
    config: String,
}

#[derive(Debug, Deserialize)]
struct ConfigRaw {
    listen_port: Option<u16>,
    tls: Option<TlsConfig>,
    default_backend: String,
    routes: Vec<RouteRaw>,
}

#[derive(Debug)]
struct Config {
    listen_port: Option<u16>,
    tls: Option<TlsConfig>,
    default_backend: SocketAddr,
    routes: Vec<Route>,
}

#[derive(Debug, Deserialize, Clone)]
struct TlsConfig {
    port: u16,
    cert: String,
    key: String,
}

#[derive(Debug)]
struct Route {
    host: Option<String>,
    path: Option<String>,
    client_ip: Option<IpNet>,
    backend: Option<SocketAddr>,
    backends: Option<Vec<SocketAddr>>,
}

impl Route {
    fn from_raw(raw: RouteRaw) -> std::result::Result<Self, String> {
        let client_ip = raw
            .client_ip
            .as_deref()
            .map(parse_client_ip)
            .transpose()?;
        match (raw.backend, raw.backends) {
            (Some(b), None) => Ok(Route {
                host: raw.host,
                path: raw.path,
                client_ip,
                backend: Some(parse_addr(&b)?),
                backends: None,
            }),
            (None, Some(list)) => {
                if list.is_empty() {
                    return Err("route with `backends` has empty list".into());
                }
                let mut addrs = Vec::with_capacity(list.len());
                for s in list {
                    addrs.push(parse_addr(&s)?);
                }
                Ok(Route {
                    host: raw.host,
                    path: raw.path,
                    client_ip,
                    backend: None,
                    backends: Some(addrs),
                })
            }
            (Some(_), Some(_)) => Err("route has both `backend` and `backends`".into()),
            (None, None) => Err("route has neither `backend` nor `backends`".into()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RouteRaw {
    host: Option<String>,
    path: Option<String>,
    client_ip: Option<String>,
    backend: Option<String>,
    backends: Option<Vec<String>>,
}

fn parse_addr(s: &str) -> std::result::Result<SocketAddr, String> {
    s.parse()
        .map_err(|e: std::net::AddrParseError| format!("invalid address '{s}': {e}"))
}

fn parse_client_ip(s: &str) -> std::result::Result<IpNet, String> {
    if !s.contains('/') {
        let ip: IpAddr = s.parse().map_err(|e: std::net::AddrParseError| e.to_string())?;
        Ok(IpNet::from(ip))
    } else {
        s.parse().map_err(|e: ipnet::AddrParseError| e.to_string())
    }
}

impl Config {
    fn load(path: &str) -> std::result::Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {path}: {e}"))?;
        Self::from_str(&content)
    }

    fn from_str(content: &str) -> std::result::Result<Self, String> {
        let raw: ConfigRaw =
            serde_yaml::from_str(content).map_err(|e| format!("failed to parse config: {e}"))?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: ConfigRaw) -> std::result::Result<Self, String> {
        let default_backend = parse_addr(&raw.default_backend)?;
        let mut routes = Vec::with_capacity(raw.routes.len());
        for r in raw.routes {
            routes.push(Route::from_raw(r)?);
        }
        Ok(Config {
            listen_port: raw.listen_port,
            tls: raw.tls,
            default_backend,
            routes,
        })
    }

    /// First-match routing. Returns (route_index, &Route); None => use default_backend.
    fn resolve_route(
        &self,
        host: &str,
        path: &str,
        client_ip: Option<IpAddr>,
    ) -> Option<(usize, &Route)> {
        for (idx, route) in self.routes.iter().enumerate() {
            if let Some(ref r_host) = route.host {
                if !host_matches(host, r_host) {
                    continue;
                }
            }
            if let Some(ref r_path) = route.path {
                if !path.starts_with(r_path) {
                    continue;
                }
            }
            if let Some(ref r_net) = route.client_ip {
                match client_ip {
                    Some(ip) if r_net.contains(&ip) => {}
                    _ => continue,
                }
            }
            return Some((idx, route));
        }
        None
    }
}

fn host_matches(request_host: &str, pattern: &str) -> bool {
    let req = request_host.to_lowercase();
    let pat = pattern.to_lowercase();
    if let Some(suffix) = pat.strip_prefix("*.") {
        req.ends_with(&format!(".{suffix}"))
    } else {
        req == pat
    }
}

const VNODES: usize = 150;

fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Consistent-hash ring over a route's backends.
#[derive(Debug, Clone)]
struct Ring {
    nodes: Vec<(u64, SocketAddr)>,
}

impl Ring {
    fn new(backends: &[SocketAddr]) -> Self {
        let mut nodes = Vec::with_capacity(backends.len() * VNODES);
        for (bi, addr) in backends.iter().enumerate() {
            for vi in 0..VNODES {
                nodes.push((hash_str(&format!("{addr}#{bi}#{vi}")), *addr));
            }
        }
        nodes.sort_unstable_by_key(|(h, _)| *h);
        Ring { nodes }
    }

    /// Clockwise from hash(key): return first healthy node. If all are unhealthy,
    /// best-effort return the primary (start) node so traffic still attempts.
    fn pick(&self, key: &str, unhealthy: &HashSet<SocketAddr>) -> SocketAddr {
        debug_assert!(!self.nodes.is_empty(), "Ring must have >=1 backend");
        let h = hash_str(key);
        let n = self.nodes.len();
        let start = match self.nodes.binary_search_by_key(&h, |(p, _)| *p) {
            Ok(i) => i,
            Err(i) => i % n,
        };
        for off in 0..n {
            let (_, addr) = self.nodes[(start + off) % n];
            if !unhealthy.contains(&addr) {
                return addr;
            }
        }
        self.nodes[start].1
    }
}

pub struct Gateway {
    config: Arc<RwLock<Config>>,
}

#[async_trait]
impl ProxyHttp for Gateway {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let header = session.req_header();
        let host = header
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        let path = header.uri.path();

        let client_ip = session
            .client_addr()
            .and_then(|a| a.as_inet())
            .map(|a| a.ip());

        let config = self.config.read().unwrap();
        let addr = match config.resolve_route(host, path, client_ip) {
            Some((_idx, route)) => match route.backend {
                Some(b) => b,
                None => route
                    .backends
                    .as_ref()
                    .and_then(|bs| bs.first().copied())
                    .unwrap_or(config.default_backend),
            },
            None => config.default_backend,
        };

        Ok(Box::new(HttpPeer::new(addr, false, String::new())))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut pingora_http::RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Extract client IP from the TCP connection.
        let client_ip = session
            .client_addr()
            .and_then(|a| a.as_inet())
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Set X-Real-IP to the immediate client (overwrite if present).
        let _ = upstream_request.insert_header("X-Real-IP", &client_ip);
        // Append to X-Forwarded-For to preserve the full proxy chain.
        let _ = upstream_request.append_header("X-Forwarded-For", &client_ip);
        Ok(())
    }
}

fn watch_config(path: String, config: Arc<RwLock<Config>>) {
    use notify::{Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel::<Event>();

        let mut watcher = match RecommendedWatcher::new(
            move |res: std::result::Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            },
            NotifyConfig::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                log::error!("failed to create file watcher: {e}");
                return;
            }
        };

        let watch_dir = Path::new(&path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| "/etc/gateway".into());

        if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
            log::error!("failed to watch config directory: {e}");
            return;
        }

        let file_name = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        loop {
            let event = match rx.recv() {
                Ok(e) => e,
                Err(_) => break,
            };

            let relevant = event.paths.iter().any(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy() == file_name)
                    .unwrap_or(false)
            });

            if !relevant {
                continue;
            }

            match event.kind {
                EventKind::Create(_)
                | EventKind::Modify(_)
                | EventKind::Any
                | EventKind::Other => {}
                _ => continue,
            }

            // Debounce: drain rapid successive events
            while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}

            match Config::load(&path) {
                Ok(new_config) => {
                    let mut guard = config.write().unwrap();
                    *guard = new_config;
                    log::info!("config reloaded from {path}");
                }
                Err(e) => {
                    log::error!("failed to reload config: {e}");
                }
            }
        }
    });
}

fn main() {
    let args = Args::parse();
    let config_path = args.config;
    let config = Config::load(&config_path).expect("failed to load config");

    let listen_port = config.listen_port.unwrap_or(6198);
    let tls_config = config.tls.clone();

    let mut server = Server::new(None).unwrap();
    server.bootstrap();

    let config = Arc::new(RwLock::new(config));
    watch_config(config_path, config.clone());

    let gateway = Gateway { config };

    let mut proxy = pingora_proxy::http_proxy_service(&server.configuration, gateway);

    if let Some(ref tls) = tls_config {
        let mut tls_settings =
            TlsSettings::intermediate(&tls.cert, &tls.key).expect("failed to create TLS settings");
        tls_settings.enable_h2();
        proxy.add_tls_with_settings(&format!("0.0.0.0:{}", tls.port), None, tls_settings);
    } else {
        proxy.add_tcp(&format!("0.0.0.0:{listen_port}"));
    }

    server.add_service(proxy);
    server.run_forever();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_single_and_multi_backend() {
        let yaml = r#"
default_backend: "127.0.0.1:8080"
routes:
  - host: a.com
    backend: "10.0.0.1:80"
  - host: b.com
    backends: ["10.0.0.2:80", "10.0.0.3:80"]
"#;
        let cfg = Config::from_str(yaml).unwrap();
        assert_eq!(cfg.default_backend, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(cfg.routes[0].backend, Some("10.0.0.1:80".parse().unwrap()));
        assert!(cfg.routes[0].backends.is_none());
        assert!(cfg.routes[1].backend.is_none());
        assert_eq!(cfg.routes[1].backends.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn config_rejects_invalid_routes() {
        let both = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    backend: \"1.1.1.1:80\"\n    backends: [\"2.2.2.2:80\"]\n";
        let empty = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    backends: []\n";
        let neither = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n";
        let bad = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    backends: [\"not-an-addr\"]\n";
        assert!(Config::from_str(both).is_err());
        assert!(Config::from_str(empty).is_err());
        assert!(Config::from_str(neither).is_err());
        assert!(Config::from_str(bad).is_err());
    }

    #[test]
    fn ring_same_key_same_backend_and_skip_unhealthy() {
        let backends: Vec<SocketAddr> = ["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let ring = Ring::new(&backends);
        let a = ring.pick("user-key-1", &HashSet::new());
        let b = ring.pick("user-key-1", &HashSet::new());
        assert_eq!(a, b, "same key must map to same backend");

        let mut un = HashSet::new();
        un.insert(a);
        let c = ring.pick("user-key-1", &un);
        assert_ne!(c, a, "must skip unhealthy primary");
        assert!(!un.contains(&c));

        let all: HashSet<SocketAddr> = backends.iter().copied().collect();
        let d = ring.pick("user-key-1", &all);
        assert!(all.contains(&d), "all-unhealthy best-effort still returns a node");
    }

    #[test]
    fn ring_consistency_on_eviction() {
        let backends: Vec<SocketAddr> = (8001..=8004)
            .map(|p| format!("127.0.0.1:{p}").parse().unwrap())
            .collect();
        let ring = Ring::new(&backends);
        let full: HashMap<String, SocketAddr> = (0..2000)
            .map(|i| {
                let k = format!("k{i}");
                (k.clone(), ring.pick(&k, &HashSet::new()))
            })
            .collect();
        let mut un = HashSet::new();
        un.insert(backends[1]);
        let on_evicted = full.values().filter(|a| **a == backends[1]).count();
        let moved = full
            .iter()
            .filter(|(k, orig)| ring.pick(k, &un) != **orig)
            .count();
        assert_eq!(moved, on_evicted, "only keys on the evicted node should move");
    }
}
