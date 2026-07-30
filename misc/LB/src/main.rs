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
    /// Path to a blacklist file (one IP/CIDR per line). Optional.
    blacklist: Option<String>,
    routes: Vec<RouteRaw>,
}

#[derive(Debug)]
struct Config {
    listen_port: Option<u16>,
    tls: Option<TlsConfig>,
    default_backend: SocketAddr,
    /// Path to a blacklist file. The parsed entries live in `Gateway::blacklist`.
    blacklist: Option<String>,
    routes: Vec<Route>,
}

#[derive(Debug, Deserialize, Clone)]
struct TlsConfig {
    port: u16,
    cert: String,
    key: String,
}

/// Selection policy for a multi-backend (`backends`) route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LbMode {
    /// Consistent-hash across all healthy backends (default; load-spreading).
    #[default]
    ActiveActive,
    /// Primary-standby: traffic goes to backends[0], failing over in order
    /// only when the earlier backends are unhealthy.
    ActiveStandby,
}

#[derive(Debug)]
struct Route {
    host: Option<String>,
    path: Option<String>,
    client_ip: Option<IpNet>,
    backend: Option<SocketAddr>,
    backends: Option<Vec<SocketAddr>>,
    mode: LbMode,
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
                mode: raw.mode,
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
                    mode: raw.mode,
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
    #[serde(default)]
    mode: LbMode,
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

/// One blacklist entry: either a single IP / CIDR network, or an inclusive
/// `start..=end` IP range (e.g. `10.123.181.128-10.123.181.255`).
#[derive(Debug, Clone)]
enum BlacklistEntry {
    Net(IpNet),
    Range { start: IpAddr, end: IpAddr },
}

impl BlacklistEntry {
    /// True if `ip` is covered by this entry. A range of one address family
    /// never matches a query of the other (IpAddr ordering puts all IPv4
    /// before all IPv6, so the comparison naturally yields false).
    fn matches(&self, ip: IpAddr) -> bool {
        match self {
            BlacklistEntry::Net(net) => net.contains(&ip),
            BlacklistEntry::Range { start, end } => ip >= *start && ip <= *end,
        }
    }
}

/// Parse a single range `start-end` (already split) into a `BlacklistEntry`.
/// Both ends must be the same family and `start <= end`.
fn parse_range(start: &str, end: &str) -> std::result::Result<BlacklistEntry, String> {
    let s: IpAddr = start
        .parse()
        .map_err(|e: std::net::AddrParseError| format!("range start '{start}': {e}"))?;
    let e: IpAddr = end
        .parse()
        .map_err(|e: std::net::AddrParseError| format!("range end '{end}': {e}"))?;
    let same_family = s.is_ipv4() == e.is_ipv4();
    if !same_family {
        return Err(format!("range mixes IPv4 and IPv6: {start}-{end}"));
    }
    if s > e {
        return Err(format!("range start {s} > end {e}"));
    }
    Ok(BlacklistEntry::Range { start: s, end: e })
}

/// Parse one cleaned blacklist token: a range (`a-b`), a single IP, or a CIDR.
fn parse_blacklist_entry(line: &str) -> std::result::Result<BlacklistEntry, String> {
    let parts: Vec<&str> = line.split('-').map(str::trim).collect();
    match parts.as_slice() {
        // exactly two `-`-separated parts -> range
        [start, end] => parse_range(start, end),
        // no `-` (or >2, which won't parse as a network) -> single IP / CIDR
        _ => parse_client_ip(line).map(BlacklistEntry::Net),
    }
}

/// True if `ip` is covered by any blacklist entry.
fn is_blacklisted(ip: IpAddr, entries: &[BlacklistEntry]) -> bool {
    entries.iter().any(|e| e.matches(ip))
}

/// Parse blacklist file content: one entry per line. Each line is a single IP,
/// a CIDR, or an inclusive range `start-end`. `#` introduces comments (full-line
/// or trailing), blank lines are ignored, and invalid entries are skipped with a
/// warning so a single bad line never disables the blacklist.
fn parse_blacklist(content: &str) -> Vec<BlacklistEntry> {
    content
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .filter_map(|line| match parse_blacklist_entry(line) {
            Ok(e) => Some(e),
            Err(err) => {
                log::warn!("blacklist: skipping invalid entry '{line}': {err}");
                None
            }
        })
        .collect()
}

/// Read and parse a blacklist file. Errors only on file-read failure.
fn load_blacklist(path: &str) -> std::result::Result<Vec<BlacklistEntry>, String> {
    let content =
        fs::read_to_string(path).map_err(|e| format!("failed to read blacklist {path}: {e}"))?;
    Ok(parse_blacklist(&content))
}

/// Load the blacklist for a path that may be `None`. A missing/unreadable file
/// is logged and treated as an empty list — availability over strictness, so a
/// transient read error never blocks all traffic.
fn load_blacklist_state(path: Option<&str>) -> Vec<BlacklistEntry> {
    match path {
        Some(p) => match load_blacklist(p) {
            Ok(v) => v,
            Err(e) => {
                log::error!("{e}; treating blacklist as empty");
                Vec::new()
            }
        },
        None => Vec::new(),
    }
}

/// Build the structured "request dispatched to backend" log line.
fn dispatch_line(
    method: &str,
    host: &str,
    path: &str,
    client: Option<IpAddr>,
    backend: SocketAddr,
) -> String {
    let client = client
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "-".into());
    format!("dispatch method={method} host={host} path={path} client={client} backend={backend}")
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
            blacklist: raw.blacklist,
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

/// Primary-standby selection: the first healthy backend in list order, so all
/// traffic prefers `backends[0]` and only fails over when earlier ones are down.
/// If every backend is unhealthy, fall back to `backends[0]` (best-effort).
fn pick_primary(backends: &[SocketAddr], unhealthy: &HashSet<SocketAddr>) -> SocketAddr {
    for addr in backends {
        if !unhealthy.contains(addr) {
            return *addr;
        }
    }
    backends[0]
}

/// Affinity key: `Authorization: Bearer <key>` -> `x-api-key` -> client IP -> "unknown".
fn extract_api_key(
    auth: Option<&str>,
    x_api_key: Option<&str>,
    client_ip: Option<IpAddr>,
) -> String {
    if let Some(a) = auth {
        let trimmed = a.trim();
        if let Some(prefix) = trimmed.get(..6) {
            if prefix.eq_ignore_ascii_case("Bearer") {
                if let Some(rest) = trimmed.get(6..) {
                    let rest = rest.trim();
                    if !rest.is_empty() {
                        return rest.to_string();
                    }
                }
            }
        }
        return trimmed.to_string();
    }
    if let Some(k) = x_api_key {
        return k.trim().to_string();
    }
    match client_ip {
        Some(ip) => ip.to_string(),
        None => "unknown".to_string(),
    }
}

const PROBE_INTERVAL: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const FAIL_THRESHOLD: u32 = 3;

struct HealthState {
    backends: Arc<RwLock<Vec<SocketAddr>>>,
    unhealthy: Arc<RwLock<HashSet<SocketAddr>>>,
}

impl HealthState {
    fn new(backends: Vec<SocketAddr>) -> Arc<Self> {
        Arc::new(HealthState {
            backends: Arc::new(RwLock::new(backends)),
            unhealthy: Arc::new(RwLock::new(HashSet::new())),
        })
    }
}

/// (prev_consecutive_failures, probe_healthy) -> (new_failures, mark_unhealthy)
fn classify_health(prev_failures: u32, probe_healthy: bool) -> (u32, bool) {
    if probe_healthy {
        (0, false)
    } else {
        let n = prev_failures + 1;
        (n, n >= FAIL_THRESHOLD)
    }
}

fn probe_once(addr: SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}

/// Background TCP prober: every PROBE_INTERVAL, probe each known backend and
/// mark/unmark unhealthy in the shared set.
fn run_health_probe(state: Arc<HealthState>) {
    std::thread::spawn(move || {
        let mut failures: HashMap<SocketAddr, u32> = HashMap::new();
        loop {
            let backends = state.backends.read().unwrap().clone();
            for addr in &backends {
                let healthy = probe_once(*addr);
                let prev = *failures.get(addr).unwrap_or(&0);
                let (nf, mark) = classify_health(prev, healthy);
                failures.insert(*addr, nf);
                let mut set = state.unhealthy.write().unwrap();
                if mark {
                    set.insert(*addr);
                } else {
                    set.remove(addr);
                }
            }
            std::thread::sleep(PROBE_INTERVAL);
        }
    });
}

pub struct Gateway {
    config: Arc<RwLock<Config>>,
    rings: Arc<RwLock<HashMap<usize, Ring>>>,
    health: Arc<HealthState>,
    blacklist: Arc<RwLock<Vec<BlacklistEntry>>>,
}

#[async_trait]
impl ProxyHttp for Gateway {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<bool> {
        // Block blacklisted client IPs before any routing. Uses the TCP peer
        // address (not X-Forwarded-For) so the value can't be spoofed.
        let blocked = session
            .client_addr()
            .and_then(|a| a.as_inet())
            .map(|a| a.ip())
            .map(|ip| {
                let bl = self.blacklist.read().unwrap();
                (ip, is_blacklisted(ip, &bl))
            });
        if let Some((ip, true)) = blocked {
            log::warn!("blocked request from blacklisted client IP {ip}");
            session.respond_error(403).await?;
            return Ok(true);
        }
        Ok(false)
    }

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
            Some((idx, route)) => match &route.backends {
                Some(list) => {
                    let unhealthy = self.health.unhealthy.read().unwrap();
                    match route.mode {
                        LbMode::ActiveStandby => pick_primary(list, &unhealthy),
                        LbMode::ActiveActive => {
                            let key = extract_api_key(
                                header
                                    .headers
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok()),
                                header.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
                                client_ip,
                            );
                            let rings = self.rings.read().unwrap();
                            let ring = rings
                                .get(&idx)
                                .expect("ring must exist for an active-active backends-route");
                            ring.pick(&key, &*unhealthy)
                        }
                    }
                }
                None => route
                    .backend
                    .expect("single-backend route must have backend"),
            },
            None => config.default_backend,
        };

        log::info!(
            "{}",
            dispatch_line(header.method.as_str(), host, path, client_ip, addr)
        );

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

fn build_rings(config: &Config) -> HashMap<usize, Ring> {
    config
        .routes
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            // Active-standby routes select by ordered failover, no ring needed.
            if r.mode == LbMode::ActiveActive {
                r.backends.as_ref().map(|bs| (i, Ring::new(bs)))
            } else {
                None
            }
        })
        .collect()
}

fn collect_all_backends(config: &Config) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in &config.routes {
        if let Some(list) = &r.backends {
            for a in list {
                if seen.insert(*a) {
                    out.push(*a);
                }
            }
        }
    }
    out
}

/// Classify a watcher event by whether it touches the config file and/or the
/// blacklist file (matched by file name). Only create/modify-like events count.
fn event_targets(
    event: &notify::Event,
    config_name: &str,
    blacklist_name: Option<&str>,
) -> (bool, bool) {
    let kind_ok = matches!(
        event.kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Modify(_)
            | notify::EventKind::Any
            | notify::EventKind::Other
    );
    if !kind_ok {
        return (false, false);
    }
    let mut cfg = false;
    let mut bl = false;
    for p in &event.paths {
        let name = match p.file_name().and_then(|n| n.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if name == config_name {
            cfg = true;
        }
        if let Some(bn) = blacklist_name {
            if bn == name {
                bl = true;
            }
        }
    }
    (cfg, bl)
}

fn watch_config(
    path: String,
    config: Arc<RwLock<Config>>,
    rings: Arc<RwLock<HashMap<usize, Ring>>>,
    health: Arc<HealthState>,
    blacklist: Arc<RwLock<Vec<BlacklistEntry>>>,
) {
    use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};

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

        let config_file_name = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Also watch the blacklist file's directory so edits to it hot-reload.
        let mut watched_blacklist_dir: Option<std::path::PathBuf> = None;
        {
            let bl_path = config.read().unwrap().blacklist.clone();
            if let Some(bp) = bl_path.as_deref() {
                if let Some(dir) = Path::new(bp).parent() {
                    if dir != watch_dir.as_path() {
                        let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
                    }
                    watched_blacklist_dir = Some(dir.to_path_buf());
                }
            }
        }

        loop {
            let event = match rx.recv() {
                Ok(e) => e,
                Err(_) => break,
            };

            // Blacklist file name is read fresh; the path may change after a reload.
            let bl_name = config
                .read()
                .unwrap()
                .blacklist
                .as_deref()
                .and_then(|p| Path::new(p).file_name())
                .map(|n| n.to_string_lossy().into_owned());

            let (mut cfg_changed, mut bl_changed) =
                event_targets(&event, &config_file_name, bl_name.as_deref());

            // Debounce: drain rapid successive events, merging their targets.
            while let Ok(e) = rx.recv_timeout(Duration::from_millis(200)) {
                let (c, b) = event_targets(&e, &config_file_name, bl_name.as_deref());
                cfg_changed |= c;
                bl_changed |= b;
            }

            if !cfg_changed && !bl_changed {
                continue;
            }

            if cfg_changed {
                match Config::load(&path) {
                    Ok(new_config) => {
                        let new_rings = build_rings(&new_config);
                        let new_backends = collect_all_backends(&new_config);
                        let backend_set: HashSet<SocketAddr> =
                            new_backends.iter().copied().collect();
                        {
                            let mut u = health.unhealthy.write().unwrap();
                            u.retain(|a| backend_set.contains(a));
                        }
                        *health.backends.write().unwrap() = new_backends;

                        // If the blacklist file path changed, reload its content
                        // and (best-effort) watch the new directory.
                        let prev_bl = config.read().unwrap().blacklist.clone();
                        let new_bl = new_config.blacklist.clone();
                        if new_bl != prev_bl {
                            *blacklist.write().unwrap() = load_blacklist_state(new_bl.as_deref());
                            let new_dir = new_bl
                                .as_deref()
                                .and_then(|p| Path::new(p).parent())
                                .map(|p| p.to_path_buf());
                            if new_dir != watched_blacklist_dir {
                                if let Some(d) = &new_dir {
                                    let _ = watcher.watch(d, RecursiveMode::NonRecursive);
                                }
                                watched_blacklist_dir = new_dir;
                            }
                            log::info!("blacklist path changed to {:?}", new_bl);
                        }

                        *config.write().unwrap() = new_config;
                        *rings.write().unwrap() = new_rings;
                        log::info!("config reloaded from {path}");
                    }
                    Err(e) => {
                        log::error!("failed to reload config: {e}");
                    }
                }
            }

            if bl_changed {
                let bl_path = config.read().unwrap().blacklist.clone();
                if let Some(p) = bl_path.as_deref() {
                    match load_blacklist(p) {
                        Ok(v) => {
                            *blacklist.write().unwrap() = v;
                            log::info!("blacklist reloaded from {p}");
                        }
                        Err(e) => log::error!("failed to reload blacklist {p}: {e}"),
                    }
                }
            }
        }
    });
}

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::os::unix::net::UnixDatagram;

/// syslog LOG_USER facility code.
const FACILITY_USER: u8 = 1;

/// Map a `log::Level` to a syslog PRI value = facility * 8 + severity.
fn syslog_priority(level: Level) -> u8 {
    let severity = match level {
        Level::Error => 3, // ERR
        Level::Warn => 4,  // WARNING
        Level::Info => 6,  // INFO
        Level::Debug => 7, // DEBUG
        Level::Trace => 7, // (no severity below DEBUG)
    };
    FACILITY_USER * 8 + severity
}

/// Writes log records to a local syslog daemon over a connected Unix datagram
/// socket, in RFC 3164 form: `<PRI>ident[pid]: message`.
struct SyslogLogger {
    sock: UnixDatagram,
    ident: String,
    pid: u32,
}

impl Log for SyslogLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let line = format!(
            "<{}>{}[{}]: {}",
            syslog_priority(record.level()),
            self.ident,
            self.pid,
            record.args()
        );
        // Best-effort: syslog is fire-and-forget, never block the data path.
        let _ = self.sock.send(line.as_bytes());
    }

    fn flush(&self) {}
}

/// Last-resort logger: writes to stderr so records are never silently dropped
/// when no local syslog socket is available (e.g. some containers).
struct StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        eprintln!("{} {}", record.level().as_str().to_lowercase(), record.args());
    }

    fn flush(&self) {}
}

/// Install the global `log` backend: prefer a local syslog Unix socket
/// (`/dev/log`, then the macOS/BSD variants), otherwise fall back to stderr.
/// pingora's own `log::` calls route through here too.
fn init_logging(ident: &str) {
    let pid = std::process::id();
    for path in ["/dev/log", "/var/run/syslog", "/var/run/log"] {
        // Create an unbound datagram socket and connect it to the syslog path.
        let sock = match UnixDatagram::unbound().and_then(|s| s.connect(path).map(|()| s)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let logger = SyslogLogger {
            sock,
            ident: ident.to_string(),
            pid,
        };
        if log::set_boxed_logger(Box::new(logger)).is_ok() {
            log::set_max_level(LevelFilter::Info);
            return;
        }
    }
    // No syslog socket reachable — log to stderr instead of dropping records.
    let _ = log::set_boxed_logger(Box::new(StderrLogger));
    log::set_max_level(LevelFilter::Info);
    eprintln!("{ident}: no syslog socket found, logging to stderr");
}

fn main() {
    let args = Args::parse();
    let config_path = args.config;
    init_logging("gateway-lb");
    let config = Config::load(&config_path).expect("failed to load config");

    let listen_port = config.listen_port.unwrap_or(6198);
    let tls_config = config.tls.clone();

    let mut server = Server::new(None).unwrap();
    server.bootstrap();

    let config = Arc::new(RwLock::new(config));
    let rings = Arc::new(RwLock::new(build_rings(&config.read().unwrap())));
    let health = HealthState::new(collect_all_backends(&config.read().unwrap()));
    let blacklist = Arc::new(RwLock::new(load_blacklist_state(
        config.read().unwrap().blacklist.as_deref(),
    )));
    run_health_probe(health.clone());
    watch_config(
        config_path,
        config.clone(),
        rings.clone(),
        health.clone(),
        blacklist.clone(),
    );

    let gateway = Gateway {
        config,
        rings,
        health,
        blacklist,
    };

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

    #[test]
    fn api_key_extraction_priority() {
        let ip = Some(IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(extract_api_key(Some("Bearer abc123"), None, ip), "abc123");
        assert_eq!(extract_api_key(Some("bearer XYZ"), None, ip), "XYZ");
        assert_eq!(extract_api_key(None, Some("sk-xyz"), ip), "sk-xyz");
        assert_eq!(extract_api_key(Some("Bearer a"), Some("b"), ip), "a");
        assert_eq!(extract_api_key(None, None, ip), "10.0.0.1");
        assert_eq!(extract_api_key(None, None, None), "unknown");
    }

    #[test]
    fn classify_health_threshold_and_recovery() {
        assert_eq!(classify_health(0, true), (0, false));
        assert_eq!(classify_health(1, false), (2, false));
        assert_eq!(classify_health(2, false), (3, true));
        assert_eq!(classify_health(9, true), (0, false));
    }

    #[test]
    fn probe_once_alive_then_closed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(probe_once(addr));
        drop(listener);
        assert!(!probe_once(addr));
    }

    #[test]
    fn pick_primary_prefers_healthiest_in_order() {
        let bs: Vec<SocketAddr> = ["10.0.0.1:80", "10.0.0.2:80", "10.0.0.3:80"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        // all healthy -> primary (backends[0])
        assert_eq!(pick_primary(&bs, &HashSet::new()), bs[0]);
        // primary down -> first standby in order
        let mut un = HashSet::new();
        un.insert(bs[0]);
        assert_eq!(pick_primary(&bs, &un), bs[1]);
        // first two down -> third
        un.insert(bs[1]);
        assert_eq!(pick_primary(&bs, &un), bs[2]);
        // all down -> best-effort primary (still attempt backends[0])
        un.insert(bs[2]);
        assert_eq!(pick_primary(&bs, &un), bs[0]);
    }

    #[test]
    fn config_backends_mode_parse() {
        let yaml = r#"
default_backend: "127.0.0.1:8080"
routes:
  - host: aa.com
    backends: ["10.0.0.1:80", "10.0.0.2:80"]
  - host: as.com
    backends: ["10.0.0.3:80", "10.0.0.4:80"]
    mode: active-standby
"#;
        let cfg = Config::from_str(yaml).unwrap();
        assert_eq!(cfg.routes[0].mode, LbMode::ActiveActive, "default is multi-active");
        assert_eq!(cfg.routes[1].mode, LbMode::ActiveStandby);
    }

    #[test]
    fn build_rings_and_collect_backends() {
        let yaml = r#"
default_backend: "127.0.0.1:8080"
routes:
  - host: a.com
    backend: "10.0.0.1:80"
  - host: b.com
    backends: ["10.0.0.2:80", "10.0.0.3:80"]
  - host: c.com
    backends: ["10.0.0.2:80", "10.0.0.4:80"]
"#;
        let cfg = Config::from_str(yaml).unwrap();
        let rings = build_rings(&cfg);
        assert!(rings.contains_key(&1) && rings.contains_key(&2));
        assert!(!rings.contains_key(&0), "single-backend route has no ring");
        let all = collect_all_backends(&cfg);
        let set: HashSet<SocketAddr> = all.iter().copied().collect();
        assert_eq!(set.len(), 3, "dedup across routes: .2/.3/.4");
        assert!(set.contains(&"10.0.0.2:80".parse().unwrap()));
    }

    #[test]
    fn config_blacklist_is_optional_path() {
        let with_path = r#"
default_backend: "127.0.0.1:8080"
blacklist: "/etc/gateway/blacklist.txt"
routes: []
"#;
        let cfg = Config::from_str(with_path).unwrap();
        assert_eq!(cfg.blacklist.as_deref(), Some("/etc/gateway/blacklist.txt"));

        let without = r#"
default_backend: "127.0.0.1:8080"
routes: []
"#;
        let cfg2 = Config::from_str(without).unwrap();
        assert!(cfg2.blacklist.is_none());
    }

    #[test]
    fn parse_blacklist_handles_comments_blanks_invalid() {
        let content = "\
# full-line comment
10.0.5.100

192.168.66.0/24
   # indented comment
not-an-ip
1.2.3.0/24   # trailing comment
";
        let nets = parse_blacklist(content);
        assert_eq!(nets.len(), 3, "3 valid entries; invalid line skipped");
        let contains = |ip: &str| {
            let ip: IpAddr = ip.parse().unwrap();
            nets.iter().any(|n| n.matches(ip))
        };
        assert!(contains("10.0.5.100"));
        assert!(contains("192.168.66.42"));
        assert!(contains("1.2.3.9"));
    }

    #[test]
    fn parse_blacklist_empty_or_comments_only() {
        assert!(parse_blacklist("").is_empty());
        assert!(parse_blacklist("# only comments\n\n   \n").is_empty());
    }

    #[test]
    fn load_blacklist_reads_file() {
        let path = format!(
            "{}/lb_bl_test_{}.txt",
            std::env::temp_dir().to_string_lossy(),
            std::process::id()
        );
        std::fs::write(&path, "# header\n10.0.0.9\n10.0.0.0/24\n").unwrap();
        let nets = load_blacklist(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(nets.len(), 2);
        let nine: IpAddr = "10.0.0.9".parse().unwrap();
        assert!(nets.iter().any(|n| n.matches(nine)));
    }

    #[test]
    fn load_blacklist_missing_file_errors() {
        assert!(load_blacklist("/nonexistent/lb_bl_missing.txt").is_err());
    }

    #[test]
    fn dispatch_line_contains_all_fields() {
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        let backend: SocketAddr = "10.0.1.2:8080".parse().unwrap();
        let line = dispatch_line("POST", "api.lb.local", "/v1/chat", Some(ip), backend);
        assert!(line.contains("method=POST"));
        assert!(line.contains("host=api.lb.local"));
        assert!(line.contains("path=/v1/chat"));
        assert!(line.contains("client=10.0.0.9"));
        assert!(line.contains("backend=10.0.1.2:8080"));

        // Missing client IP is rendered as "-".
        let line2 = dispatch_line("GET", "h", "/p", None, backend);
        assert!(line2.contains("client=-"));
    }

    #[test]
    fn syslog_priority_maps_levels_to_user_facility() {
        use log::Level;
        assert_eq!(syslog_priority(Level::Error), 1 * 8 + 3);
        assert_eq!(syslog_priority(Level::Warn), 1 * 8 + 4);
        assert_eq!(syslog_priority(Level::Info), 1 * 8 + 6);
        assert_eq!(syslog_priority(Level::Debug), 1 * 8 + 7);
        assert_eq!(syslog_priority(Level::Trace), 1 * 8 + 7);
    }

    #[test]
    fn syslog_logger_emits_rfc3164_to_socket() {
        // Stand up a local datagram sink, point the logger at it, and read back
        // exactly what it emits — a deterministic end-to-end check of the wire
        // format without depending on the host syslogd.
        let sock_path = std::env::temp_dir()
            .join(format!("lb_syslog_test_{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixDatagram::bind(&sock_path).unwrap();

        let sock = UnixDatagram::unbound().unwrap();
        sock.connect(&sock_path).unwrap();
        let logger = SyslogLogger {
            sock,
            ident: "gateway-lb".to_string(),
            pid: 4242,
        };

        logger.log(
            &Record::builder()
                .args(format_args!("dispatch method=GET backend=10.0.0.1:80"))
                .level(Level::Info)
                .target("gateway-lb")
                .build(),
        );

        let mut buf = [0u8; 256];
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        let msg = std::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(msg, "<14>gateway-lb[4242]: dispatch method=GET backend=10.0.0.1:80");
        let _ = std::fs::remove_file(&sock_path);
    }

    #[test]
    fn is_blacklisted_matches_single_ip_and_cidr() {
        let nets: Vec<BlacklistEntry> = vec![
            BlacklistEntry::Net(parse_client_ip("10.0.5.100").unwrap()),
            BlacklistEntry::Net(parse_client_ip("192.168.66.0/24").unwrap()),
        ];
        // exact single-IP hit
        assert!(is_blacklisted("10.0.5.100".parse().unwrap(), &nets));
        // inside CIDR
        assert!(is_blacklisted("192.168.66.42".parse().unwrap(), &nets));
        // outside everything
        assert!(!is_blacklisted("10.0.5.99".parse().unwrap(), &nets));
        assert!(!is_blacklisted("192.168.99.1".parse().unwrap(), &nets));
        // empty list never blocks
        assert!(!is_blacklisted("10.0.5.100".parse().unwrap(), &[]));
    }

    #[test]
    fn parse_blacklist_handles_ranges() {
        let content = "\
10.123.181.128-10.123.181.255
# comment
192.168.0.0-192.168.0.10
";
        let entries = parse_blacklist(content);
        assert_eq!(entries.len(), 2);
        let in_list = |ip: &str| {
            let ip: IpAddr = ip.parse().unwrap();
            entries.iter().any(|e| e.matches(ip))
        };
        // first range: both boundaries and inside
        assert!(in_list("10.123.181.128"));
        assert!(in_list("10.123.181.200"));
        assert!(in_list("10.123.181.255"));
        assert!(!in_list("10.123.181.127"));
        assert!(!in_list("10.123.182.1"));
        // second range
        assert!(in_list("192.168.0.5"));
        assert!(!in_list("192.168.0.11"));
        // an IPv6 query never matches an IPv4 range
        assert!(!in_list("::1"));
    }

    #[test]
    fn parse_blacklist_rejects_bad_ranges() {
        let content = "\
10.0.0.5-10.0.0.1
1.2.3.4-foo
1.2.3.4-::1
";
        let entries = parse_blacklist(content);
        assert!(entries.is_empty(), "all three are invalid ranges");
    }
}
