use async_trait::async_trait;
use ipnet::IpNet;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use serde::Deserialize;
use std::env;
use std::fs;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct Config {
    listen_port: Option<u16>,
    tls: Option<TlsConfig>,
    default_backend: String,
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
    backend: String,
}

impl<'de> Deserialize<'de> for Route {
    fn deserialize<D>(de: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RouteRaw {
            host: Option<String>,
            path: Option<String>,
            client_ip: Option<String>,
            backend: String,
        }
        let raw = RouteRaw::deserialize(de)?;
        let client_ip = raw
            .client_ip
            .as_deref()
            .map(|s| parse_client_ip(s))
            .transpose()
            .map_err(|e| serde::de::Error::custom(format!("invalid client_ip: {e}")))?;
        Ok(Route {
            host: raw.host,
            path: raw.path,
            client_ip,
            backend: raw.backend,
        })
    }
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
        serde_yaml::from_str(&content)
            .map_err(|e| format!("failed to parse config {path}: {e}"))
    }

    fn resolve(&self, host: &str, path: &str, client_ip: Option<IpAddr>) -> &str {
        for route in &self.routes {
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
            return &route.backend;
        }
        &self.default_backend
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
        let backend = config.resolve(host, path, client_ip);
        let addr: std::net::SocketAddr = backend
            .parse()
            .map_err(|e| {
                pingora_core::Error::because(
                    pingora_core::ErrorType::InternalError,
                    format!("invalid backend address '{backend}': {e}"),
                    e,
                )
            })?;

        Ok(Box::new(HttpPeer::new(addr, false, String::new())))
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
    let config_path =
        env::var("CONFIG_PATH").unwrap_or_else(|_| "/etc/gateway/routes.yaml".into());
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
