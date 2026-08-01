use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use pingora_core::upstreams::peer::{HttpPeer, Peer};
use pingora_core::Result;
use pingora_proxy::{ProxyHttp, Session};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::backends::{build_rings, collect_all_backends, pick_active_active, pick_primary, Ring};
use crate::blacklist::{load_blacklist, load_blacklist_state, Blacklist};
use crate::client::effective_client_ip;
use crate::config::{Config, LbMode, UpstreamTimeouts};
use crate::logging::{complete_line, dispatch_line, redirect_line};
use crate::metrics::Metrics;
use crate::routes::request_host;

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
/// Hot-swappable snapshot: the config plus its purely-derived `rings` and
/// `backends` list. Swapped atomically on reload, so a reader that `.load()`s
/// this guard sees a consistent config+rings+backends triple (no torn reads).
pub(crate) struct ConfigSnapshot {
    pub(crate) config: Config,
    pub(crate) rings: HashMap<usize, Ring>,
    pub(crate) backends: Vec<SocketAddr>,
}

pub(crate) struct Gateway {
    pub(crate) snapshot: Arc<ArcSwap<ConfigSnapshot>>,
    pub(crate) unhealthy: Arc<ArcSwap<HashSet<SocketAddr>>>,
    pub(crate) blacklist: Arc<ArcSwap<Blacklist>>,
    pub(crate) metrics: Metrics,
}

/// re-resolving (and without re-scanning the route table).
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
    /// Set when the request will be access-logged: request identity for the
    /// completion line emitted from `logging()`.
    log_ctx: Option<LogCtx>,
    /// Bytes of request body seen so far (for `max_body_size` enforcement).
    body_seen: u64,
    /// Wall-clock start of the request, for the duration in the completion log.
    started: Instant,
}

impl Default for RoutingCtx {
    fn default() -> Self {
        RoutingCtx {
            backend: None,
            attempted: Vec::new(),
            client_ip: None,
            route: None,
            timeouts: UpstreamTimeouts::default(),
            log_ctx: None,
            body_seen: 0,
            started: Instant::now(),
        }
    }
}

/// Minimal request identity kept until `logging()` so the completion line can
/// correlate status/duration/bytes back to a dispatched request.
struct LogCtx {
    method: String,
    host: String,
    path: String,
    backend: SocketAddr,
}

/// Built-in LB health endpoint, used by the container HEALTHCHECK (and handy
/// for any orchestration probe). Handled before blacklist/routing so probes are
/// never blocked, and never proxied to a backend.
const LB_HEALTH_PATH: &str = "/__lb_healthz";

/// Built-in metrics endpoint (Prometheus text-ish format). Like the health
/// endpoint, it is answered by the LB itself before blacklist/routing.
const LB_METRICS_PATH: &str = "/__lb_metrics";

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

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        self.metrics.requests_total.fetch_add(1, Ordering::Relaxed);

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

        // Built-in metrics endpoint, answered before blacklist/routing so
        // monitoring always works even when a route is misbehaving.
        if path == LB_METRICS_PATH {
            let unhealthy_len = self.unhealthy.load().len();
            let body = self.metrics.render(unhealthy_len);
            let mut resp = pingora_http::ResponseHeader::build(200, None)?;
            resp.insert_header("Content-Type", "text/plain; version=0.0.4")?;
            resp.insert_header("Content-Length", body.len().to_string())?;
            session.write_response_header(Box::new(resp), false).await?;
            session
                .write_response_body(Some(Bytes::from(body)), true)
                .await?;
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
            if bl.contains(ip) {
                self.metrics.blocked_total.fetch_add(1, Ordering::Relaxed);
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

            // Reject oversized bodies up front when a Content-Length is known;
            // chunked bodies are capped streaming in `request_body_filter`.
            if let Some(limit) = snap.config.max_body_size {
                let cl = header
                    .headers
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
                if cl.is_some_and(|n| n > limit) {
                    self.metrics
                        .body_rejected_total
                        .fetch_add(1, Ordering::Relaxed);
                    session.respond_error(413).await?;
                    return Ok(true);
                }
            }

            match snap.config.resolve_route(host, path, client_ip) {
                Some((idx, route)) => match &route.redirect {
                    Some(location) => {
                        let code = route.redirect_code;
                        self.metrics.redirects_total.fetch_add(1, Ordering::Relaxed);
                        if snap
                            .config
                            .access_log
                            .should_log(method, host, path, client_ip)
                        {
                            log::info!("{}", redirect_line(code, host, path, location));
                        }
                        let mut resp = pingora_http::ResponseHeader::build(code, None)?;
                        resp.insert_header("Location", location)?;
                        session.write_response_header(Box::new(resp), true).await?;
                        return Ok(true);
                    }
                    None => {
                        self.metrics.proxied_total.fetch_add(1, Ordering::Relaxed);
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
                                LbMode::ActiveStandby => pick_primary(list, &unhealthy, &[]),
                                LbMode::ActiveActive => pick_active_active(
                                    idx,
                                    list,
                                    &snap.rings,
                                    &key,
                                    &unhealthy,
                                    &[],
                                ),
                            },
                            None => route
                                .backend
                                .expect("single-backend route must have backend"),
                        };
                        let route_ctx = route.backends.as_ref().map(|list| RouteCtx {
                            idx,
                            mode: route.mode,
                            backends: list.clone(),
                            key,
                        });
                        if snap
                            .config
                            .access_log
                            .should_log(method, host, path, client_ip)
                        {
                            log::info!("{}", dispatch_line(method, host, path, client_ip, addr));
                            ctx.log_ctx = Some(LogCtx {
                                method: method.to_string(),
                                host: host.to_string(),
                                path: path.to_string(),
                                backend: addr,
                            });
                        }
                        ctx.backend = Some(addr);
                        ctx.route = route_ctx;
                        ctx.timeouts = snap.config.timeouts;
                        return Ok(false);
                    }
                },
                None => {
                    self.metrics.proxied_total.fetch_add(1, Ordering::Relaxed);
                    let addr = pick_primary(&snap.config.default_backends, &unhealthy, &[]);
                    ctx.backend = Some(addr);
                    // Keep a route context so retries fail over across the
                    // default list instead of hammering the same dead node.
                    ctx.route = Some(RouteCtx {
                        idx: usize::MAX,
                        mode: LbMode::ActiveStandby,
                        backends: snap.config.default_backends.clone(),
                        key: String::new(),
                    });
                    ctx.timeouts = snap.config.timeouts;
                    if snap
                        .config
                        .access_log
                        .should_log(method, host, path, client_ip)
                    {
                        log::info!("{}", dispatch_line(method, host, path, client_ip, addr));
                        ctx.log_ctx = Some(LogCtx {
                            method: method.to_string(),
                            host: host.to_string(),
                            path: path.to_string(),
                            backend: addr,
                        });
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
                LbMode::ActiveStandby => pick_primary(&route.backends, &unhealthy, &ctx.attempted),
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
            ctx.backend
                .expect("backend must be set before upstream_peer")
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

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let limit = self.snapshot.load().config.max_body_size;
        if let Some(limit) = limit {
            if let Some(b) = body {
                ctx.body_seen = ctx.body_seen.saturating_add(b.len() as u64);
            }
            if ctx.body_seen > limit {
                self.metrics
                    .body_rejected_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(pingora_core::Error::explain(
                    pingora_core::ErrorType::ReadError,
                    format!("request body exceeds max_body_size={limit}"),
                ));
            }
        }
        Ok(())
    }

    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora_core::Error>,
        ctx: &mut Self::CTX,
    ) {
        let Some(log_ctx) = &ctx.log_ctx else {
            return;
        };
        let snap = self.snapshot.load();
        if !snap.config.access_log.should_log(
            &log_ctx.method,
            &log_ctx.host,
            &log_ctx.path,
            ctx.client_ip,
        ) {
            return;
        }
        let status = session.response_written().map_or(0, |r| r.status.as_u16());
        let bytes = session.body_bytes_sent() as u64;
        let ms = ctx.started.elapsed().as_millis();
        log::info!("{}", complete_line(log_ctx.backend, status, ms, bytes));
    }
}

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

pub(crate) fn watch_config(
    path: String,
    snapshot: Arc<ArcSwap<ConfigSnapshot>>,
    blacklist: Arc<ArcSwap<Blacklist>>,
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
                                        **d != watch_dir && Some(d.as_path()) != new_dir.as_deref()
                                    })
                                    .cloned()
                                    .collect();
                                for d in stale {
                                    let _ = watcher.unwatch(&d);
                                    watched_dirs.remove(&d);
                                }
                                if let Some(d) = &new_dir {
                                    if watched_dirs.insert(d.clone()) {
                                        let _ = watcher.watch(d, RecursiveMode::NonRecursive);
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
                            let entries = v.len();
                            blacklist.store(Arc::new(v));
                            log::info!("blacklist reloaded from {p}: {entries} entries");
                        }
                        Err(e) => log::error!("failed to reload blacklist {p}: {e}"),
                    }
                }
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;

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
}
