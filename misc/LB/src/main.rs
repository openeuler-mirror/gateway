use arc_swap::ArcSwap;
use async_trait::async_trait;
use clap::Parser;
use ipnet::IpNet;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::server::Server;
use pingora_core::upstreams::peer::{HttpPeer, Peer};
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
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
    /// CIDRs of trusted reverse proxies. When the direct peer is inside one of
    /// these, X-Forwarded-For is used to recover the real client IP for
    /// blacklist and `client_ip` routing. Default: empty (never trust XFF).
    trusted_proxies: Option<Vec<String>>,
    /// Upstream timeouts in seconds (optional; see `UpstreamTimeouts::default`).
    upstream_connect_timeout: Option<u64>,
    upstream_total_connect_timeout: Option<u64>,
    upstream_read_timeout: Option<u64>,
    upstream_idle_timeout: Option<u64>,
    /// Optional HTTP health check used by the backend prober.
    health_check: Option<HealthCheckConfig>,
    /// Per-request access log (dispatch/redirect lines) control.
    access_log: Option<AccessLogConfig>,
    routes: Vec<RouteRaw>,
}

/// Backend health-check settings. `path` absent => TCP connect probe (default);
/// `path` set => issue an HTTP GET and require `expected_status`.
#[derive(Debug, Clone, Deserialize)]
struct HealthCheckConfig {
    path: Option<String>,
    #[serde(default = "default_health_status")]
    expected_status: u16,
}

fn default_health_status() -> u16 {
    200
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        HealthCheckConfig {
            path: None,
            expected_status: default_health_status(),
        }
    }
}

/// Per-request access-log control. Sampling is deterministic per request
/// signature, so the same request is always logged (or not) consistently.
#[derive(Debug, Clone, Deserialize)]
struct AccessLogConfig {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_sample_rate")]
    sample_rate: f64,
}

fn default_true() -> bool {
    true
}

fn default_sample_rate() -> f64 {
    1.0
}

impl Default for AccessLogConfig {
    fn default() -> Self {
        AccessLogConfig {
            enabled: true,
            sample_rate: 1.0,
        }
    }
}

impl AccessLogConfig {
    fn should_log(
        &self,
        method: &str,
        host: &str,
        path: &str,
        client: Option<IpAddr>,
    ) -> bool {
        if !self.enabled || self.sample_rate <= 0.0 {
            return false;
        }
        if self.sample_rate >= 1.0 {
            return true;
        }
        let signature = format!("{method}|{host}|{path}|{client:?}");
        (hash_str(&signature) % 10_000) < ((self.sample_rate * 10_000.0) as u64)
    }
}

/// Timeouts applied to every upstream `HttpPeer`. These bound the request data
/// path so a hung backend can never occupy a worker indefinitely.
#[derive(Debug, Clone, Copy)]
struct UpstreamTimeouts {
    connect: Duration,
    total_connect: Duration,
    read: Duration,
    idle: Duration,
}

impl Default for UpstreamTimeouts {
    fn default() -> Self {
        UpstreamTimeouts {
            connect: Duration::from_secs(3),
            total_connect: Duration::from_secs(5),
            read: Duration::from_secs(30),
            idle: Duration::from_secs(60),
        }
    }
}

#[derive(Debug)]
struct Config {
    listen_port: Option<u16>,
    tls: Option<TlsConfig>,
    default_backend: SocketAddr,
    /// Path to a blacklist file. The parsed entries live in `Gateway::blacklist`.
    blacklist: Option<String>,
    trusted_proxies: Vec<IpNet>,
    timeouts: UpstreamTimeouts,
    health_check: HealthCheckConfig,
    access_log: AccessLogConfig,
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
    /// If set, the LB returns a 3xx redirect to this URL instead of proxying.
    redirect: Option<String>,
    /// Redirect status code (3xx). Defaults to 302.
    redirect_code: u16,
    mode: LbMode,
}

impl Route {
    fn from_raw(raw: RouteRaw) -> std::result::Result<Self, String> {
        let client_ip = raw
            .client_ip
            .as_deref()
            .map(parse_client_ip)
            .transpose()?;

        // Exactly one of backend / backends / redirect must be set.
        let set_count = [
            raw.backend.is_some(),
            raw.backends.is_some(),
            raw.redirect.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        if set_count != 1 {
            return Err(format!(
                "route must set exactly one of `backend`, `backends`, `redirect` (found {set_count})"
            ));
        }

        // redirect must point somewhere (reject empty / whitespace-only).
        if let Some(r) = raw.redirect.as_ref() {
            if r.trim().is_empty() {
                return Err("redirect target must not be empty".into());
            }
        }

        // redirect_code must be a standard 3xx redirect status when provided.
        let redirect_code = match raw.redirect_code {
            Some(c) if [301u16, 302, 303, 307, 308].contains(&c) => c,
            Some(c) => {
                return Err(format!(
                    "redirect_code must be one of 301/302/303/307/308, got {c}"
                ));
            }
            None => 302,
        };

        let (backend, backends) = match (raw.backend, raw.backends) {
            (Some(b), None) => (Some(parse_addr(&b)?), None),
            (None, Some(list)) => {
                if list.is_empty() {
                    return Err("route with `backends` has empty list".into());
                }
                let mut addrs = Vec::with_capacity(list.len());
                for s in list {
                    addrs.push(parse_addr(&s)?);
                }
                (None, Some(addrs))
            }
            // redirect-only route: neither backend nor backends.
            (None, None) => (None, None),
            _ => unreachable!("set_count == 1 rules out both backend and backends"),
        };

        Ok(Route {
            host: raw.host,
            path: raw.path,
            client_ip,
            backend,
            backends,
            redirect: raw.redirect,
            redirect_code,
            mode: raw.mode,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RouteRaw {
    host: Option<String>,
    path: Option<String>,
    client_ip: Option<String>,
    backend: Option<String>,
    backends: Option<Vec<String>>,
    redirect: Option<String>,
    redirect_code: Option<u16>,
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

/// Resolve the effective client IP for blacklist / `client_ip` routing:
/// the direct peer unless it is a trusted proxy, in which case walk
/// X-Forwarded-For right-to-left and return the first hop not covered by a
/// trusted CIDR. Malformed hops are skipped; if every hop is trusted or
/// unparsable, the direct peer is returned as a fallback.
fn effective_client_ip(
    peer_ip: Option<IpAddr>,
    xff: Option<&str>,
    trusted: &[IpNet],
) -> Option<IpAddr> {
    let peer = peer_ip?;
    if trusted.iter().any(|n| n.contains(&peer)) {
        if let Some(header) = xff {
            for hop in header.split(',').rev() {
                let hop = hop.trim();
                if let Ok(ip) = hop.parse::<IpAddr>() {
                    if !trusted.iter().any(|n| n.contains(&ip)) {
                        return Some(ip);
                    }
                }
            }
        }
        // All hops trusted (or unusable): fall back to the peer itself.
        Some(peer)
    } else {
        Some(peer)
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
        // 3+ parts (e.g. a typo'd `a-b-c`) -> explicit error, not a silent fallthrough
        [_, _, _, ..] => Err(format!("invalid range '{line}' (expected exactly one '-')")),
        // no `-` -> single IP / CIDR
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
        for (i, r) in raw.routes.into_iter().enumerate() {
            if r.host.is_none() && r.path.is_none() && r.client_ip.is_none() {
                log::warn!(
                    "route #{i} has no host/path/client_ip matcher; it is a catch-all \
                     and shadows every later route"
                );
            }
            routes.push(Route::from_raw(r)?);
        }
        let secs = |v: Option<u64>, default: u64| Duration::from_secs(v.unwrap_or(default));
        let trusted_proxies = raw
            .trusted_proxies
            .unwrap_or_default()
            .into_iter()
            .map(|s| parse_client_ip(&s))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let health_check = raw.health_check.unwrap_or_default();
        if let Some(path) = health_check.path.as_deref() {
            if !path.starts_with('/') {
                return Err(format!(
                    "health_check.path must start with '/', got '{path}'"
                ));
            }
            if path.contains(['\r', '\n']) {
                return Err("health_check.path must not contain CR/LF characters".into());
            }
        }
        let mut access_log = raw.access_log.unwrap_or_default();
        if !access_log.sample_rate.is_finite() {
            return Err("access_log.sample_rate must be a finite number between 0.0 and 1.0".into());
        }
        access_log.sample_rate = access_log.sample_rate.clamp(0.0, 1.0);
        Ok(Config {
            listen_port: raw.listen_port,
            tls: raw.tls,
            default_backend,
            blacklist: raw.blacklist,
            trusted_proxies,
            timeouts: UpstreamTimeouts {
                connect: secs(raw.upstream_connect_timeout, 3),
                total_connect: secs(raw.upstream_total_connect_timeout, 5),
                read: secs(raw.upstream_read_timeout, 30),
                idle: secs(raw.upstream_idle_timeout, 60),
            },
            health_check,
            access_log,
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

/// Strip a `:port` suffix from a host, keeping IPv6 literals intact
/// (`[::1]:8080` -> `[::1]`, `example.com:8080` -> `example.com`).
fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        // IPv6 literal: keep through the closing bracket, drop any `:port`.
        if let Some(end) = host.find(']') {
            return &host[..=end];
        }
        return host;
    }
    // Bare IPv6 literal (no brackets, e.g. what `Uri::host()` yields): it
    // contains colons but no port, so leave it untouched.
    let is_bare_ipv6 = host.matches(':').count() >= 2
        && host
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == ':');
    if is_bare_ipv6 {
        return host;
    }
    // DNS / IPv4 host with an optional `:port`.
    match host.rsplit_once(':') {
        Some((head, port))
            if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            head
        }
        _ => host,
    }
}

/// Resolve the request host: prefer the URI authority (HTTP/2 puts
/// `:authority` there, and the `host` header is absent), then fall back to the
/// HTTP/1 `Host` header. IPv6 literals are normalized to bare form (`::1`), the
/// same representation both protocols yield after `Uri::host()`.
fn request_host<'a>(uri_host: Option<&'a str>, host_header: Option<&'a str>) -> &'a str {
    let host = strip_port(uri_host.or(host_header).unwrap_or(""));
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
}

const VNODES: usize = 150;

/// Stable 64-bit FNV-1a. Deterministic across processes and Rust versions,
/// unlike `std::collections::hash_map::DefaultHasher`, so multiple LB
/// instances always agree on the ring layout.
fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Consistent-hash ring over a route's backends.
#[derive(Debug, Clone)]
struct Ring {
    nodes: Vec<(u64, SocketAddr)>,
}

impl Ring {
    fn new(backends: &[SocketAddr]) -> Self {
        let mut nodes = Vec::with_capacity(backends.len() * VNODES);
        // Keyed by address (not by index), so inserting/removing a backend
        // elsewhere in the list does not reshuffle every other backend's vnodes.
        for addr in backends {
            for vi in 0..VNODES {
                nodes.push((hash_str(&format!("{addr}#{vi}")), *addr));
            }
        }
        nodes.sort_unstable_by_key(|(h, _)| *h);
        Ring { nodes }
    }

    /// Clockwise from hash(key): return the first node that is neither
    /// unhealthy nor already attempted this request. If every remaining node is
    /// unhealthy, still prefer an untried node (best-effort); only when all
    /// nodes were attempted does it fall back to the start node.
    fn pick(
        &self,
        key: &str,
        unhealthy: &HashSet<SocketAddr>,
        attempted: &[SocketAddr],
    ) -> SocketAddr {
        debug_assert!(!self.nodes.is_empty(), "Ring must have >=1 backend");
        let h = hash_str(key);
        let n = self.nodes.len();
        let start = match self.nodes.binary_search_by_key(&h, |(p, _)| *p) {
            Ok(i) => i,
            Err(i) => i % n,
        };
        let mut fallback: Option<SocketAddr> = None;
        for off in 0..n {
            let (_, addr) = self.nodes[(start + off) % n];
            if !attempted.contains(&addr) {
                if fallback.is_none() {
                    fallback = Some(addr);
                }
                if !unhealthy.contains(&addr) {
                    return addr;
                }
            }
        }
        fallback.unwrap_or(self.nodes[start].1)
    }
}

/// Primary-standby selection: the first healthy backend in list order, so all
/// traffic prefers `backends[0]` and only fails over when earlier ones are down.
/// Untried-but-unhealthy backends are still preferred over retrying an already
/// attempted one; only when every backend was attempted does it fall back to
/// `backends[0]`.
fn pick_primary(
    backends: &[SocketAddr],
    unhealthy: &HashSet<SocketAddr>,
    attempted: &[SocketAddr],
) -> SocketAddr {
    let mut fallback: Option<SocketAddr> = None;
    for addr in backends {
        if !attempted.contains(addr) {
            if fallback.is_none() {
                fallback = Some(*addr);
            }
            if !unhealthy.contains(addr) {
                return *addr;
            }
        }
    }
    fallback.unwrap_or(backends[0])
}

/// Active-active selection with a graceful fallback: consistent-hash via the
/// ring when present; if the ring is missing for `idx` (e.g. a config reload
/// invalidated the index between resolve and lookup), degrade to ordered
/// primary selection so routing never panics.
fn pick_active_active(
    idx: usize,
    backends: &[SocketAddr],
    rings: &HashMap<usize, Ring>,
    key: &str,
    unhealthy: &HashSet<SocketAddr>,
    attempted: &[SocketAddr],
) -> SocketAddr {
    match rings.get(&idx) {
        Some(ring) => ring.pick(key, unhealthy, attempted),
        None => pick_primary(backends, unhealthy, attempted),
    }
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

/// Hot-swappable snapshot: the config plus its purely-derived `rings` and
/// `backends` list. Swapped atomically on reload, so a reader that `.load()`s
/// this guard sees a consistent config+rings+backends triple (no torn reads).
struct ConfigSnapshot {
    config: Config,
    rings: HashMap<usize, Ring>,
    backends: Vec<SocketAddr>,
}

pub struct Gateway {
    snapshot: Arc<ArcSwap<ConfigSnapshot>>,
    unhealthy: Arc<ArcSwap<HashSet<SocketAddr>>>,
    blacklist: Arc<ArcSwap<Vec<BlacklistEntry>>>,
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

/// HTTP GET probe: open the connection, send a minimal request, and require the
/// configured status code in the response head. Reads until the end of headers
/// (bounded), so slow/hung backends fail within PROBE_TIMEOUT.
fn probe_http(addr: SocketAddr, path: &str, expected: u16) -> bool {
    use std::io::{Read, Write};

    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let host = addr.ip().to_string();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut buf = [0u8; 4096];
    let mut got = 0;
    loop {
        match stream.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => {
                got += n;
                if got >= buf.len() || buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return false,
        }
    }
    let head = std::str::from_utf8(&buf[..got]).unwrap_or("");
    head.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        == Some(expected)
}

fn probe_once(addr: SocketAddr, check: &HealthCheckConfig) -> bool {
    match &check.path {
        Some(path) => probe_http(addr, path, check.expected_status),
        None => std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok(),
    }
}

/// Background TCP prober: every PROBE_INTERVAL, probe each known backend in
/// parallel and publish a fresh `unhealthy` set. It is the SOLE writer of
/// `unhealthy`, so a plain `.store()` is safe. Rebuilding from the current
/// `backends` each cycle also self-prunes backends that were removed from the
/// config.
fn run_health_probe(
    snapshot: Arc<ArcSwap<ConfigSnapshot>>,
    unhealthy: Arc<ArcSwap<HashSet<SocketAddr>>>,
) {
    std::thread::spawn(move || {
        let mut failures: HashMap<SocketAddr, u32> = HashMap::new();
        loop {
            let (backends, check) = {
                let snap = snapshot.load();
                (snap.backends.clone(), snap.config.health_check.clone())
            };
            // Probe all backends concurrently so a large (or fully-down) pool
            // is still checked every PROBE_INTERVAL instead of serially.
            let results: Vec<(SocketAddr, bool)> = std::thread::scope(|s| {
                backends
                    .iter()
                    .map(|addr| {
                        let addr = *addr;
                        let check = &check;
                        s.spawn(move || (addr, probe_once(addr, check)))
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|h| h.join().expect("probe thread panicked"))
                    .collect()
            });
            // Drop counters for backends no longer in the config.
            failures.retain(|addr, _| backends.contains(addr));
            let mut new_unhealthy = HashSet::new();
            for (addr, healthy) in results {
                let prev = *failures.get(&addr).unwrap_or(&0);
                let (nf, mark) = classify_health(prev, healthy);
                failures.insert(addr, nf);
                if mark {
                    new_unhealthy.insert(addr);
                }
            }
            unhealthy.store(Arc::new(new_unhealthy));
            std::thread::sleep(PROBE_INTERVAL);
        }
    });
}

/// Per-request routing context. `request_filter` resolves the route once and
/// stores the chosen backend here, so `upstream_peer` can reuse it without
/// re-resolving (and without re-scanning the route table).
#[derive(Default)]
pub struct RoutingCtx {
    backend: Option<SocketAddr>,
    /// Backends already attempted for this request (populated by
    /// `fail_to_connect`); `upstream_peer` re-picks excluding them on retries.
    attempted: Vec<SocketAddr>,
    /// Effective client IP after trusted-proxy XFF resolution.
    client_ip: Option<IpAddr>,
    /// Route details needed to re-pick on retry (only for multi-backend routes).
    route: Option<RouteCtx>,
    /// Upstream timeouts captured at request_filter time (stable snapshot).
    timeouts: UpstreamTimeouts,
}

/// Built-in LB health endpoint, used by the container HEALTHCHECK (and handy
/// for any orchestration probe). Handled before blacklist/routing so probes are
/// never blocked, and never proxied to a backend.
const LB_HEALTH_PATH: &str = "/__lb_healthz";

/// Snapshot of a multi-backend route for retry re-selection. Kept in ctx so a
/// config reload mid-request never tears the route index from its ring.
#[derive(Clone)]
struct RouteCtx {
    idx: usize,
    mode: LbMode,
    backends: Vec<SocketAddr>,
    key: String,
}

#[async_trait]
impl ProxyHttp for Gateway {
    type CTX = RoutingCtx;

    fn new_ctx(&self) -> Self::CTX {
        RoutingCtx::default()
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<bool> {
        // Gather request fields once; reused for blacklist, routing, and logging.
        let header = session.req_header();
        // `uri.host()` carries `:authority` for HTTP/2 (which has no Host
        // header); the Host header is the HTTP/1 source.
        let host = request_host(
            header.uri.host(),
            header.headers.get("host").and_then(|v| v.to_str().ok()),
        );
        let path = header.uri.path();
        let method = header.method.as_str();

        // Built-in LB health endpoint: answered by the LB itself, before any
        // blacklist or routing decision.
        if path == LB_HEALTH_PATH {
            let mut resp = pingora_http::ResponseHeader::build(200, None)?;
            resp.insert_header("Content-Type", "text/plain")?;
            resp.insert_header("Content-Length", "0")?;
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(true);
        }

        // `as_inet()` is None for non-INET (e.g. Unix-socket) clients, so for a
        // UDS listener the IP-based blacklist and client_ip routes silently no-op.
        let peer_ip = session
            .client_addr()
            .and_then(|a| a.as_inet())
            .map(|a| a.ip());
        // Trusted-proxy resolution: X-Forwarded-For is consulted only when the
        // direct peer is a configured proxy, so clients cannot spoof it.
        let client_ip = {
            let snap = self.snapshot.load();
            effective_client_ip(
                peer_ip,
                header
                    .headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok()),
                &snap.config.trusted_proxies,
            )
        };
        ctx.client_ip = client_ip;

        // 1. Blacklist: block before any routing. Uses the TCP peer address
        //    (or the effective client IP behind trusted proxies).
        if let Some(ip) = client_ip {
            let bl = self.blacklist.load();
            if is_blacklisted(ip, &bl) {
                log::warn!("blocked request from blacklisted client IP {ip}");
                session.respond_error(403).await?;
                return Ok(true);
            }
        }

        // 2. Resolve the route ONCE and decide redirect vs backend. All reads
        //    here are lock-free ArcSwap `.load()`s; the guards are dropped at
        //    the end of this sync block, before any `.await` below. Because
        //    `config` and `rings` come from the SAME snapshot guard, the route
        //    index and its ring are always coherent (no torn read on reload).
        {
            let snap = self.snapshot.load();
            let unhealthy = self.unhealthy.load();
            match snap.config.resolve_route(host, path, client_ip) {
                Some((idx, route)) => match &route.redirect {
                    Some(location) => {
                        let code = route.redirect_code;
                        if snap.config.access_log.should_log(method, host, path, client_ip) {
                            log::info!("redirect {code} {host}{path} -> {location}");
                        }
                        let mut resp = pingora_http::ResponseHeader::build(code, None)?;
                        resp.insert_header("Location", location)?;
                        session.write_response_header(Box::new(resp), true).await?;
                        return Ok(true);
                    }
                    None => {
                        // Affinity key is only needed for active-active rings.
                        let key = match route.mode {
                            LbMode::ActiveActive => extract_api_key(
                                header
                                    .headers
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok()),
                                header
                                    .headers
                                    .get("x-api-key")
                                    .and_then(|v| v.to_str().ok()),
                                client_ip,
                            ),
                            LbMode::ActiveStandby => String::new(),
                        };
                        let addr = match &route.backends {
                            Some(list) => match route.mode {
                                LbMode::ActiveStandby => {
                                    pick_primary(list, &unhealthy, &[])
                                }
                                LbMode::ActiveActive => pick_active_active(
                                    idx, list, &snap.rings, &key, &unhealthy, &[],
                                ),
                            },
                            None => {
                                route.backend.expect("single-backend route must have backend")
                            }
                        };
                        let route_ctx = route.backends.as_ref().map(|list| RouteCtx {
                            idx,
                            mode: route.mode,
                            backends: list.clone(),
                            key,
                        });
                        if snap.config.access_log.should_log(method, host, path, client_ip) {
                            log::info!(
                                "{}",
                                dispatch_line(method, host, path, client_ip, addr)
                            );
                        }
                        ctx.backend = Some(addr);
                        ctx.route = route_ctx;
                        ctx.timeouts = snap.config.timeouts;
                        return Ok(false);
                    }
                },
                None => {
                    let addr = snap.config.default_backend;
                    ctx.backend = Some(addr);
                    ctx.timeouts = snap.config.timeouts;
                    if snap.config.access_log.should_log(method, host, path, client_ip) {
                        log::info!("{}", dispatch_line(method, host, path, client_ip, addr));
                    }
                    return Ok(false);
                }
            }
        }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Normal path: reuse the route resolved once in `request_filter`.
        // Retry path (`attempted` non-empty): re-pick a backend that has not
        // been tried yet for this request, so pingora's retries actually
        // fail over instead of hammering the same dead node.
        let addr = if ctx.attempted.is_empty() {
            ctx.backend
                .unwrap_or_else(|| self.snapshot.load().config.default_backend)
        } else if let Some(route) = &ctx.route {
            let snap = self.snapshot.load();
            let unhealthy = self.unhealthy.load();
            match route.mode {
                LbMode::ActiveStandby => {
                    pick_primary(&route.backends, &unhealthy, &ctx.attempted)
                }
                LbMode::ActiveActive => pick_active_active(
                    route.idx,
                    &route.backends,
                    &snap.rings,
                    &route.key,
                    &unhealthy,
                    &ctx.attempted,
                ),
            }
        } else {
            // Single-backend route or default backend: no alternative exists,
            // so retries target the same node.
            ctx.backend.expect("backend must be set before upstream_peer")
        };

        let mut peer = HttpPeer::new(addr, false, String::new());
        peer.options.connection_timeout = Some(ctx.timeouts.connect);
        peer.options.total_connection_timeout = Some(ctx.timeouts.total_connect);
        peer.options.read_timeout = Some(ctx.timeouts.read);
        peer.options.idle_timeout = Some(ctx.timeouts.idle);
        Ok(Box::new(peer))
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        peer: &HttpPeer,
        ctx: &mut Self::CTX,
        e: Box<pingora_core::Error>,
    ) -> Box<pingora_core::Error> {
        // Record the failed peer so the next `upstream_peer()` call (pingora
        // retries connect errors by default) can pick a different backend.
        if let Some(addr) = peer.address().as_inet() {
            let addr = *addr;
            if !ctx.attempted.contains(&addr) {
                ctx.attempted.push(addr);
            }
        }
        e
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut pingora_http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // X-Real-IP carries the effective client (trusted-proxy resolved).
        let real_ip = ctx
            .client_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        // XFF appends the immediate TCP peer to preserve the full proxy chain.
        let client_ip = session
            .client_addr()
            .and_then(|a| a.as_inet())
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let _ = upstream_request.insert_header("X-Real-IP", &real_ip);
        // Append to X-Forwarded-For as a single comma-joined header line so
        // downstream parsers that only read one line see the full chain.
        let existing = upstream_request
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty());
        match existing {
            Some(prev) => {
                let joined = format!("{prev}, {client_ip}");
                let _ = upstream_request.insert_header("X-Forwarded-For", &joined);
            }
            None => {
                let _ = upstream_request.insert_header("X-Forwarded-For", &client_ip);
            }
        }
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

/// Resolve the directory containing a blacklist file, canonicalizing when the
/// file exists so the watched path always matches the paths events report.
fn blacklist_dir(blacklist: Option<&str>) -> Option<std::path::PathBuf> {
    blacklist.and_then(|p| {
        let abs = fs::canonicalize(p).unwrap_or_else(|_| Path::new(p).to_path_buf());
        abs.parent().map(|p| p.to_path_buf())
    })
}

fn watch_config(
    path: String,
    snapshot: Arc<ArcSwap<ConfigSnapshot>>,
    blacklist: Arc<ArcSwap<Vec<BlacklistEntry>>>,
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

        // Track every directory we watch so blacklist path changes can unwatch
        // the stale directory instead of accumulating watches.
        let mut watched_dirs: HashSet<std::path::PathBuf> = HashSet::new();
        watched_dirs.insert(watch_dir.clone());
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
            let bl_path = snapshot.load().config.blacklist.clone();
            if let Some(dir) = blacklist_dir(bl_path.as_deref()) {
                if dir != watch_dir {
                    let _ = watcher.watch(&dir, RecursiveMode::NonRecursive);
                    watched_dirs.insert(dir.clone());
                }
                watched_blacklist_dir = Some(dir);
            }
        }

        while let Ok(event) = rx.recv() {
            // Blacklist file name is read fresh; the path may change after a reload.
            let bl_name = snapshot
                .load()
                .config
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

                        // If the blacklist file path changed, reload its content
                        // and (best-effort) watch the new directory.
                        let prev_bl = snapshot.load().config.blacklist.clone();
                        let new_bl = new_config.blacklist.clone();
                        if new_bl != prev_bl {
                            blacklist.store(Arc::new(load_blacklist_state(new_bl.as_deref())));
                            let new_dir = blacklist_dir(new_bl.as_deref());
                            if new_dir != watched_blacklist_dir {
                                // Drop watches on directories no longer used
                                // (only the blacklist dir can change; the
                                // config dir is permanent).
                                let stale: Vec<std::path::PathBuf> = watched_dirs
                                    .iter()
                                    .filter(|d| {
                                        **d != watch_dir
                                            && Some(d.as_path()) != new_dir.as_deref()
                                    })
                                    .cloned()
                                    .collect();
                                for d in stale {
                                    let _ = watcher.unwatch(&d);
                                    watched_dirs.remove(&d);
                                }
                                if let Some(d) = &new_dir {
                                    if watched_dirs.insert(d.clone()) {
                                        let _ =
                                            watcher.watch(d, RecursiveMode::NonRecursive);
                                    }
                                }
                                watched_blacklist_dir = new_dir;
                            }
                            log::info!("blacklist path changed to {:?}", new_bl);
                        }

                        // Atomic swap: config + rings + backends together, so
                        // readers never see a torn (new config, old rings) pair.
                        snapshot.store(Arc::new(ConfigSnapshot {
                            config: new_config,
                            rings: new_rings,
                            backends: new_backends,
                        }));
                        log::info!("config reloaded from {path}");
                    }
                    Err(e) => {
                        log::error!("failed to reload config: {e}");
                    }
                }
            }

            if bl_changed {
                let bl_path = snapshot.load().config.blacklist.clone();
                if let Some(p) = bl_path.as_deref() {
                    match load_blacklist(p) {
                        Ok(v) => {
                            blacklist.store(Arc::new(v));
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

/// Create a datagram socket connected to a syslog path. The socket is
/// non-blocking so a slow/jammed syslogd can never stall the data path: a full
/// buffer drops the log line (best-effort) instead of blocking the worker.
fn connect_syslog(path: &str) -> Option<UnixDatagram> {
    let sock = UnixDatagram::unbound().ok()?;
    sock.set_nonblocking(true).ok()?;
    sock.connect(path).ok()?;
    Some(sock)
}

/// Install the global `log` backend: prefer a local syslog Unix socket
/// (`/dev/log`, then the macOS/BSD variants), otherwise fall back to stderr.
/// pingora's own `log::` calls route through here too.
fn init_logging(ident: &str) {
    let pid = std::process::id();
    for path in ["/dev/log", "/var/run/syslog", "/var/run/log"] {
        let sock = match connect_syslog(path) {
            Some(s) => s,
            None => continue,
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

    let mut proxy = pingora_proxy::http_proxy_service(&server.configuration, gateway);

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
        let a = ring.pick("user-key-1", &HashSet::new(), &[]);
        let b = ring.pick("user-key-1", &HashSet::new(), &[]);
        assert_eq!(a, b, "same key must map to same backend");

        let mut un = HashSet::new();
        un.insert(a);
        let c = ring.pick("user-key-1", &un, &[]);
        assert_ne!(c, a, "must skip unhealthy primary");
        assert!(!un.contains(&c));

        let all: HashSet<SocketAddr> = backends.iter().copied().collect();
        let d = ring.pick("user-key-1", &all, &[]);
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
                (k.clone(), ring.pick(&k, &HashSet::new(), &[]))
            })
            .collect();
        let mut un = HashSet::new();
        un.insert(backends[1]);
        let on_evicted = full.values().filter(|a| **a == backends[1]).count();
        let moved = full
            .iter()
            .filter(|(k, orig)| ring.pick(k, &un, &[]) != **orig)
            .count();
        assert_eq!(moved, on_evicted, "only keys on the evicted node should move");
    }

    #[test]
    fn ring_retry_excludes_attempted_backends() {
        let backends: Vec<SocketAddr> = ["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let ring = Ring::new(&backends);
        let first = ring.pick("key", &HashSet::new(), &[]);
        let second = ring.pick("key", &HashSet::new(), &[first]);
        assert_ne!(second, first, "retry must avoid the already attempted backend");

        let third = ring.pick("key", &HashSet::new(), &[first, second]);
        assert!(third != first && third != second, "third pick avoids both attempts");

        // All backends attempted (and all unhealthy): best-effort fallback
        // still returns a node instead of panicking.
        let all: HashSet<SocketAddr> = backends.iter().copied().collect();
        let _ = ring.pick("key", &all, &backends);
    }

    #[test]
    fn ring_untried_backend_preferred_over_unhealthy_when_all_unhealthy() {
        let backends: Vec<SocketAddr> = ["127.0.0.1:8001", "127.0.0.1:8002"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let ring = Ring::new(&backends);
        let all_unhealthy: HashSet<SocketAddr> = backends.iter().copied().collect();
        let first = ring.pick("key", &all_unhealthy, &[]);
        // Retry should prefer the untried backend even though it is marked
        // unhealthy, rather than re-hitting the attempted one.
        let second = ring.pick("key", &all_unhealthy, &[first]);
        assert_ne!(first, second);
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
    fn request_host_prefers_uri_authority_and_strips_port() {
        // HTTP/2: authority lives in the URI, no Host header present.
        assert_eq!(request_host(Some("api.example.com"), None), "api.example.com");
        assert_eq!(request_host(Some("api.example.com:8443"), None), "api.example.com");
        // HTTP/1: authority absent, Host header is the source.
        assert_eq!(request_host(None, Some("app.example.com:8080")), "app.example.com");
        assert_eq!(request_host(None, Some("app.example.com")), "app.example.com");
        // IPv6 literals normalize to the same bare form on both paths:
        // HTTP/2 `Uri::host()` yields `::1`; an HTTP/1 Host header carries
        // brackets plus an optional port.
        assert_eq!(request_host(None, Some("[::1]:8080")), "::1");
        assert_eq!(request_host(Some("::1"), None), "::1");
        // Neither present -> empty.
        assert_eq!(request_host(None, None), "");
    }

    #[test]
    fn hash_str_is_stable_and_distributes() {
        let a = hash_str("user-key-1");
        let b = hash_str("user-key-1");
        assert_eq!(a, b, "same input must hash identically (process-stable)");
        let c = hash_str("user-key-2");
        assert_ne!(a, c);
        // FNV-1a is a known vector: hash of "abc" is deterministic.
        assert_eq!(hash_str("abc"), 0xe71f_a219_0541_574b);
    }

    #[test]
    fn ring_layout_is_index_independent() {
        // Vnodes are keyed by address only: inserting a backend at the front of
        // the list must not move keys that already map to the other backends.
        let a = ["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"];
        let b = ["127.0.0.1:8000", "127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"];
        let ra = Ring::new(&a.iter().map(|s| s.parse().unwrap()).collect::<Vec<_>>());
        let rb = Ring::new(&b.iter().map(|s| s.parse().unwrap()).collect::<Vec<_>>());
        let moved = (0..2000)
            .filter(|i| {
                let k = format!("k{i}");
                ra.pick(&k, &HashSet::new(), &[]) != rb.pick(&k, &HashSet::new(), &[])
            })
            .count();
        // Only keys that land on the new node (1/4 of the ring on average) may
        // move; an index-dependent layout would move ~3/4 of keys.
        assert!(moved < 1000, "too many keys moved: {moved}");
    }

    #[test]
    fn classify_health_threshold_and_recovery() {
        assert_eq!(classify_health(0, true), (0, false));
        assert_eq!(classify_health(1, false), (2, false));
        assert_eq!(classify_health(2, false), (3, true));
        assert_eq!(classify_health(9, true), (0, false));
    }

    #[test]
    fn config_timeouts_default_and_override() {
        let yaml = r#"
default_backend: "127.0.0.1:8080"
routes: []
"#;
        let cfg = Config::from_str(yaml).unwrap();
        assert_eq!(cfg.timeouts.connect, Duration::from_secs(3));
        assert_eq!(cfg.timeouts.total_connect, Duration::from_secs(5));
        assert_eq!(cfg.timeouts.read, Duration::from_secs(30));
        assert_eq!(cfg.timeouts.idle, Duration::from_secs(60));

        let yaml2 = r#"
default_backend: "127.0.0.1:8080"
upstream_connect_timeout: 1
upstream_total_connect_timeout: 2
upstream_read_timeout: 10
upstream_idle_timeout: 20
routes: []
"#;
        let cfg2 = Config::from_str(yaml2).unwrap();
        assert_eq!(cfg2.timeouts.connect, Duration::from_secs(1));
        assert_eq!(cfg2.timeouts.total_connect, Duration::from_secs(2));
        assert_eq!(cfg2.timeouts.read, Duration::from_secs(10));
        assert_eq!(cfg2.timeouts.idle, Duration::from_secs(20));
    }

    #[test]
    fn probe_once_alive_then_closed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(probe_once(addr, &HealthCheckConfig::default()));
        drop(listener);
        assert!(!probe_once(addr, &HealthCheckConfig::default()));
    }

    #[test]
    fn effective_client_ip_trusts_xff_only_behind_trusted_proxy() {
        let trusted: Vec<IpNet> = ["10.0.0.0/8"]
            .iter()
            .map(|s| parse_client_ip(s).unwrap())
            .collect();
        let proxy: IpAddr = "10.0.0.5".parse().unwrap();
        let client: IpAddr = "203.0.113.9".parse().unwrap();
        let spoof: IpAddr = "198.51.100.7".parse().unwrap();

        // Direct client (not a trusted proxy): XFF ignored.
        assert_eq!(
            effective_client_ip(Some(client), Some("1.2.3.4"), &trusted),
            Some(client)
        );
        // Behind the trusted proxy: rightmost untrusted XFF hop wins.
        assert_eq!(
            effective_client_ip(
                Some(proxy),
                Some("1.2.3.4, 10.0.0.9, 203.0.113.9"),
                &trusted
            ),
            Some(client)
        );
        // Untrusted hop present but a spoof attempt comes last after the proxy.
        assert_eq!(
            effective_client_ip(
                Some(proxy),
                Some("198.51.100.7, 10.0.0.9"),
                &trusted
            ),
            Some(spoof),
            "untrusted hops are honored even when the proxy appended itself"
        );
        // All hops trusted / unparsable: fall back to the peer.
        assert_eq!(
            effective_client_ip(Some(proxy), Some("10.0.0.8, 10.0.0.9"), &trusted),
            Some(proxy)
        );
        assert_eq!(
            effective_client_ip(Some(proxy), Some("garbage"), &trusted),
            Some(proxy)
        );
        // No peer: no client.
        assert_eq!(effective_client_ip(None, Some("1.2.3.4"), &trusted), None);
    }

    #[test]
    fn access_log_sampling_is_deterministic_and_bounded() {
        let ip = Some(IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)));
        let full = AccessLogConfig {
            enabled: true,
            sample_rate: 1.0,
        };
        assert!(full.should_log("GET", "a.com", "/x", ip));

        let off = AccessLogConfig {
            enabled: false,
            sample_rate: 1.0,
        };
        assert!(!off.should_log("GET", "a.com", "/x", ip));

        let half = AccessLogConfig {
            enabled: true,
            sample_rate: 0.5,
        };
        // Same request signature must sample identically every time.
        assert_eq!(
            half.should_log("POST", "api.lb.local", "/v1/chat", ip),
            half.should_log("POST", "api.lb.local", "/v1/chat", ip)
        );
        // A 0.5 rate on a representative mix should hit both sides.
        let yes = (0..2000).any(|i| {
            half.should_log("GET", "a.com", &format!("/p{i}"), ip)
        });
        let no = (0..2000).any(|i| {
            !half.should_log("GET", "a.com", &format!("/p{i}"), ip)
        });
        assert!(yes && no, "0.5 sampling must produce both logged and skipped");
    }

    #[test]
    fn config_health_check_and_trusted_proxies_and_access_log() {
        let yaml = r#"
default_backend: "127.0.0.1:8080"
trusted_proxies:
  - "10.0.0.0/8"
health_check:
  path: "/healthz"
  expected_status: 204
access_log:
  enabled: false
  sample_rate: 1.5
routes: []
"#;
        let cfg = Config::from_str(yaml).unwrap();
        assert_eq!(cfg.trusted_proxies.len(), 1);
        assert!(cfg.trusted_proxies[0].contains(&"10.1.2.3".parse::<IpAddr>().unwrap()));
        assert_eq!(cfg.health_check.path.as_deref(), Some("/healthz"));
        assert_eq!(cfg.health_check.expected_status, 204);
        assert!(!cfg.access_log.enabled);
        assert_eq!(cfg.access_log.sample_rate, 1.0, "rate clamped to 1.0");

        // Defaults when the sections are absent.
        let bare = "default_backend: \"127.0.0.1:8080\"\nroutes: []\n";
        let cfg2 = Config::from_str(bare).unwrap();
        assert!(cfg2.trusted_proxies.is_empty());
        assert_eq!(cfg2.health_check.path, None);
        assert_eq!(cfg2.health_check.expected_status, 200);
        assert!(cfg2.access_log.enabled);
        assert_eq!(cfg2.access_log.sample_rate, 1.0);
    }

    #[test]
    fn config_rejects_health_path_without_leading_slash() {
        let bad = "default_backend: \"127.0.0.1:8080\"\nhealth_check:\n  path: \"healthz\"\nroutes: []\n";
        assert!(Config::from_str(bad).is_err());
    }

    #[test]
    fn config_rejects_crlf_in_health_path() {
        // YAML double-quoted scalars fold literal newlines into spaces, so the
        // realistic vector is the \r/\n escape sequences, which serde_yaml
        // decodes into real CR/LF bytes in the resulting string.
        for path in ["/healthz\\r\\nX-Injected: 1", "/healthz\\nX-Injected: 1"] {
            let bad = format!(
                "default_backend: \"127.0.0.1:8080\"\nhealth_check:\n  path: \"{path}\"\nroutes: []\n"
            );
            assert!(
                Config::from_str(&bad).is_err(),
                "path {path:?} should be rejected"
            );
        }
    }

    #[test]
    fn config_rejects_non_finite_sample_rate() {
        for rate in [".nan", ".inf", "-.inf"] {
            let bad = format!(
                "default_backend: \"127.0.0.1:8080\"\naccess_log:\n  enabled: true\n  sample_rate: {rate}\nroutes: []\n"
            );
            assert!(
                Config::from_str(&bad).is_err(),
                "sample_rate {rate} should be rejected"
            );
        }
    }

    #[test]
    fn pick_primary_prefers_healthiest_in_order() {
        let bs: Vec<SocketAddr> = ["10.0.0.1:80", "10.0.0.2:80", "10.0.0.3:80"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        // all healthy -> primary (backends[0])
        assert_eq!(pick_primary(&bs, &HashSet::new(), &[]), bs[0]);
        // primary down -> first standby in order
        let mut un = HashSet::new();
        un.insert(bs[0]);
        assert_eq!(pick_primary(&bs, &un, &[]), bs[1]);
        // first two down -> third
        un.insert(bs[1]);
        assert_eq!(pick_primary(&bs, &un, &[]), bs[2]);
        // all down -> best-effort primary (still attempt backends[0])
        un.insert(bs[2]);
        assert_eq!(pick_primary(&bs, &un, &[]), bs[0]);

        // Retry: attempted primary must be skipped even when it is healthy.
        assert_eq!(pick_primary(&bs, &HashSet::new(), &[bs[0]]), bs[1]);
        // All unhealthy but only primary attempted -> try standby next.
        assert_eq!(pick_primary(&bs, &un, &[bs[0]]), bs[1]);
        // Everything attempted -> fall back to primary.
        assert_eq!(pick_primary(&bs, &un, &bs), bs[0]);
    }

    #[test]
    fn pick_active_active_uses_ring_or_falls_back() {
        let bs: Vec<SocketAddr> = ["10.0.0.1:80", "10.0.0.2:80", "10.0.0.3:80"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let mut rings: HashMap<usize, Ring> = HashMap::new();
        rings.insert(7, Ring::new(&bs));
        let un = HashSet::new();

        // ring present -> consistent-hash pick (deterministic per key)
        let a = pick_active_active(7, &bs, &rings, "some-key", &un, &[]);
        let b = pick_active_active(7, &bs, &rings, "some-key", &un, &[]);
        assert_eq!(a, b, "same key via ring -> same backend");
        assert!(bs.contains(&a));

        // ring missing for idx (e.g. stale after a reload) -> graceful fallback
        // to ordered primary selection instead of panicking.
        let c = pick_active_active(999, &bs, &rings, "some-key", &un, &[]);
        assert_eq!(c, bs[0], "missing ring falls back to pick_primary -> backends[0]");

        // Retry excludes the first pick.
        let d = pick_active_active(7, &bs, &rings, "some-key", &un, &[a]);
        assert_ne!(d, a, "retry must pick a different backend");
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
    fn config_parses_redirect_route() {
        let yaml = r#"
default_backend: "127.0.0.1:8080"
routes:
  - host: "old.example.com"
    redirect: "https://new.example.com/"
  - path: "/go"
    redirect: "https://example.com/dest"
    redirect_code: 301
"#;
        let cfg = Config::from_str(yaml).unwrap();
        assert_eq!(cfg.routes[0].redirect.as_deref(), Some("https://new.example.com/"));
        assert_eq!(cfg.routes[0].redirect_code, 302, "default redirect code is 302");
        assert!(cfg.routes[0].backend.is_none() && cfg.routes[0].backends.is_none());
        assert_eq!(cfg.routes[1].redirect.as_deref(), Some("https://example.com/dest"));
        assert_eq!(cfg.routes[1].redirect_code, 301);
    }

    #[test]
    fn config_redirect_validates_mutual_exclusive_and_code() {
        let both = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    backend: \"1.1.1.1:80\"\n    redirect: \"https://x\"\n";
        let bad_code = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    redirect: \"https://x\"\n    redirect_code: 200\n";
        let nonstandard = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    redirect: \"https://x\"\n    redirect_code: 304\n";
        assert!(Config::from_str(both).is_err(), "redirect + backend is ambiguous");
        assert!(Config::from_str(bad_code).is_err(), "redirect_code must be a standard redirect status");
        assert!(Config::from_str(nonstandard).is_err(), "304 is not a redirect status");
    }

    #[test]
    fn config_rejects_empty_redirect() {
        let empty = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    redirect: \"\"\n";
        let blank = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    redirect: \"   \"\n";
        assert!(Config::from_str(empty).is_err(), "empty redirect target rejected");
        assert!(Config::from_str(blank).is_err(), "whitespace-only redirect target rejected");
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
        assert_eq!(syslog_priority(Level::Error), FACILITY_USER * 8 + 3);
        assert_eq!(syslog_priority(Level::Warn), FACILITY_USER * 8 + 4);
        assert_eq!(syslog_priority(Level::Info), FACILITY_USER * 8 + 6);
        assert_eq!(syslog_priority(Level::Debug), FACILITY_USER * 8 + 7);
        assert_eq!(syslog_priority(Level::Trace), FACILITY_USER * 8 + 7);
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
    fn connect_syslog_socket_is_nonblocking() {
        // Bind a listener that never reads, so its receive buffer fills. A
        // non-blocking sender must then error (drop) instead of blocking the
        // worker forever under syslog pressure.
        let sock_path = std::env::temp_dir()
            .join(format!("lb_syslog_nb_{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixDatagram::bind(&sock_path).unwrap();

        let sock = connect_syslog(sock_path.to_str().unwrap())
            .expect("should connect to the bound listener");

        let payload = b"<14>t[0]: padded-log-message-bytes-aaaaaaaaaaaaaaaa";
        let mut errored = false;
        for _ in 0..200_000 {
            if sock.send(payload).is_err() {
                errored = true;
                break;
            }
        }
        assert!(
            errored,
            "non-blocking syslog socket must drop under pressure, not block"
        );
        drop(listener);
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
10.0.0.1-10.0.0.2-3
";
        let entries = parse_blacklist(content);
        assert!(entries.is_empty(), "all four are invalid ranges");
    }
}
