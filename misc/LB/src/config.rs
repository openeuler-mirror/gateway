use ipnet::IpNet;
use serde::Deserialize;
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use crate::backends::hash_str;
use crate::client::parse_client_ip;
use crate::routes::{host_matches, path_matches, Route, RouteRaw};

#[derive(Debug, Deserialize)]
struct ConfigRaw {
    listen_port: Option<u16>,
    tls: Option<TlsConfig>,
    /// Single default backend (shorthand). Mutually exclusive with
    /// `default_backends`.
    default_backend: Option<String>,
    /// Ordered failover list for unmatched requests: traffic prefers the first
    /// entry and fails over in order when earlier ones are unhealthy.
    default_backends: Option<Vec<String>>,
    /// Maximum request body bytes (None = unlimited). Content-Length larger
    /// than this is rejected with 413; chunked bodies are aborted when exceeded.
    max_body_size: Option<u64>,
    /// Upstream TLS (https backends). Optional; absent => plaintext upstreams.
    upstream_tls: Option<UpstreamTlsConfig>,
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
    /// Number of pingora worker threads. 0/unset => CPU core count.
    worker_threads: Option<usize>,
    /// Max upstream attempts per request (connect-failure failover). 0/unset => 3.
    max_retries: Option<usize>,
    routes: Vec<RouteRaw>,
}

/// Backend health-check settings. `path` absent => TCP connect probe (default);
/// `path` set => issue an HTTP GET and require `expected_status`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HealthCheckConfig {
    pub(crate) path: Option<String>,
    #[serde(default = "default_health_status")]
    pub(crate) expected_status: u16,
    /// Probe cycle in seconds (default 5).
    #[serde(default = "default_probe_interval")]
    pub(crate) interval_secs: u64,
    /// Consecutive failed probes before a backend is marked unhealthy (default 3).
    #[serde(default = "default_fail_threshold")]
    pub(crate) fail_threshold: u32,
}

fn default_health_status() -> u16 {
    200
}

fn default_probe_interval() -> u64 {
    5
}

fn default_fail_threshold() -> u32 {
    3
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        HealthCheckConfig {
            path: None,
            expected_status: default_health_status(),
            interval_secs: default_probe_interval(),
            fail_threshold: default_fail_threshold(),
        }
    }
}

/// TLS settings for upstream (backend) connections. Optional; when absent the
/// LB talks to backends over plaintext HTTP.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UpstreamTlsConfig {
    #[serde(default)]
    pub(crate) enabled: bool,
    /// SNI sent to the upstream. Empty => use the backend address's IP string.
    #[serde(default)]
    pub(crate) sni: String,
    /// Verify the upstream certificate chain and hostname (default true).
    #[serde(default = "default_true")]
    pub(crate) verify: bool,
}

/// Per-request access-log control. Sampling is deterministic per request
/// signature, so the same request is always logged (or not) consistently.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AccessLogConfig {
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
    pub(crate) fn should_log(
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
pub(crate) struct UpstreamTimeouts {
    pub(crate) connect: Duration,
    pub(crate) total_connect: Duration,
    pub(crate) read: Duration,
    pub(crate) idle: Duration,
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
pub(crate) struct Config {
    pub(crate) listen_port: Option<u16>,
    pub(crate) tls: Option<TlsConfig>,
    /// Ordered failover list for unmatched requests; never empty (`from_raw`
    /// guarantees at least one entry, so `default_backends[0]` is always safe).
    pub(crate) default_backends: Vec<SocketAddr>,
    pub(crate) max_body_size: Option<u64>,
    pub(crate) upstream_tls: Option<UpstreamTlsConfig>,
    /// Path to a blacklist file. The parsed entries live in `Gateway::blacklist`.
    pub(crate) blacklist: Option<String>,
    pub(crate) trusted_proxies: Vec<IpNet>,
    pub(crate) timeouts: UpstreamTimeouts,
    pub(crate) health_check: HealthCheckConfig,
    pub(crate) access_log: AccessLogConfig,
    pub(crate) worker_threads: usize,
    pub(crate) max_retries: usize,
    pub(crate) routes: Vec<Route>,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct TlsConfig {
    pub(crate) port: u16,
    pub(crate) cert: String,
    pub(crate) key: String,
}

/// Selection policy for a multi-backend (`backends`) route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LbMode {
    /// Consistent-hash across all healthy backends (default; load-spreading).
    #[default]
    ActiveActive,
    /// Primary-standby: traffic goes to backends[0], failing over in order
    /// only when the earlier backends are unhealthy.
    ActiveStandby,
}

pub(crate) fn parse_addr(s: &str) -> std::result::Result<SocketAddr, String> {
    s.parse()
        .map_err(|e: std::net::AddrParseError| format!("invalid address '{s}': {e}"))
}

impl Config {
    pub(crate) fn load(path: &str) -> std::result::Result<Self, String> {
        let content =
            fs::read_to_string(path).map_err(|e| format!("failed to read config {path}: {e}"))?;
        Self::from_str(&content)
    }

    pub(crate) fn from_str(content: &str) -> std::result::Result<Self, String> {
        let raw: ConfigRaw =
            serde_yaml::from_str(content).map_err(|e| format!("failed to parse config: {e}"))?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: ConfigRaw) -> std::result::Result<Self, String> {
        let default_backends = match (raw.default_backend, raw.default_backends) {
            (Some(s), None) => vec![parse_addr(&s)?],
            (None, Some(list)) if !list.is_empty() => {
                list.iter()
                    .map(|s| parse_addr(s))
                    .collect::<std::result::Result<Vec<_>, _>>()?
            }
            (None, Some(_)) => return Err("default_backends must not be empty".into()),
            (Some(_), Some(_)) => {
                return Err("set exactly one of `default_backend` / `default_backends`".into())
            }
            (None, None) => return Err("missing `default_backend` (or `default_backends`)".into()),
        };
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
            return Err(
                "access_log.sample_rate must be a finite number between 0.0 and 1.0".into(),
            );
        }
        access_log.sample_rate = access_log.sample_rate.clamp(0.0, 1.0);
        let worker_threads = raw.worker_threads.filter(|n| *n > 0).unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });
        let max_retries = raw.max_retries.filter(|n| *n > 0).unwrap_or(3);
        Ok(Config {
            listen_port: raw.listen_port,
            tls: raw.tls,
            default_backends,
            max_body_size: raw.max_body_size,
            upstream_tls: raw.upstream_tls,
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
            worker_threads,
            max_retries,
            routes,
        })
    }

    /// First-match routing. Returns (route_index, &Route); None => default_backends.
    pub(crate) fn resolve_route(
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
                if !path_matches(path, r_path) {
                    continue;
                }
            }
            if let Some(ref r_net) = route.client_ip {
                if !client_ip.is_some_and(|ip| r_net.contains(&ip)) {
                    continue;
                }
            }
            return Some((idx, route));
        }
        None
    }
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
        assert_eq!(cfg.default_backends[0], "127.0.0.1:8080".parse().unwrap());
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
        let yes = (0..2000).any(|i| half.should_log("GET", "a.com", &format!("/p{i}"), ip));
        let no = (0..2000).any(|i| !half.should_log("GET", "a.com", &format!("/p{i}"), ip));
        assert!(
            yes && no,
            "0.5 sampling must produce both logged and skipped"
        );
    }

    #[test]
    fn config_worker_threads_and_max_retries() {
        let yaml =
            "default_backend: \"127.0.0.1:8080\"\nworker_threads: 4\nmax_retries: 7\nroutes: []\n";
        let cfg = Config::from_str(yaml).unwrap();
        assert_eq!(cfg.worker_threads, 4);
        assert_eq!(cfg.max_retries, 7);

        let bare = "default_backend: \"127.0.0.1:8080\"\nroutes: []\n";
        let cfg2 = Config::from_str(bare).unwrap();
        assert_eq!(cfg2.max_retries, 3, "default retries = 3");
        assert!(cfg2.worker_threads >= 1, "default threads = CPU count");

        // 0 (or negative) values fall back to defaults instead of panicking.
        let zero =
            "default_backend: \"127.0.0.1:8080\"\nworker_threads: 0\nmax_retries: 0\nroutes: []\n";
        let cfg3 = Config::from_str(zero).unwrap();
        assert_eq!(cfg3.max_retries, 3);
        assert!(cfg3.worker_threads >= 1);
    }

    #[test]
    fn config_default_backends_list_and_conflicts() {
        let yaml = "default_backends: [\"10.0.0.1:80\", \"10.0.0.2:80\"]\nroutes: []\n";
        let cfg = Config::from_str(yaml).unwrap();
        assert_eq!(cfg.default_backends.len(), 2);
        assert_eq!(cfg.default_backends[0], "10.0.0.1:80".parse().unwrap());

        // Both forms at once is rejected; so are neither, or an empty list.
        let both =
            "default_backend: \"10.0.0.1:80\"\ndefault_backends: [\"10.0.0.2:80\"]\nroutes: []\n";
        assert!(Config::from_str(both).is_err());
        assert!(Config::from_str("routes: []\n").is_err());
        assert!(Config::from_str("default_backends: []\nroutes: []\n").is_err());
    }

    #[test]
    fn config_max_body_size_optional() {
        let yaml = "default_backend: \"127.0.0.1:8080\"\nmax_body_size: 1048576\nroutes: []\n";
        assert_eq!(Config::from_str(yaml).unwrap().max_body_size, Some(1048576));
        let bare = "default_backend: \"127.0.0.1:8080\"\nroutes: []\n";
        assert_eq!(Config::from_str(bare).unwrap().max_body_size, None);
    }

    #[test]
    fn config_health_check_interval_and_threshold_defaults() {
        let yaml = "default_backend: \"127.0.0.1:8080\"\nhealth_check:\n  path: \"/healthz\"\n  interval_secs: 10\n  fail_threshold: 5\nroutes: []\n";
        let cfg = Config::from_str(yaml).unwrap();
        assert_eq!(cfg.health_check.interval_secs, 10);
        assert_eq!(cfg.health_check.fail_threshold, 5);

        let bare = "default_backend: \"127.0.0.1:8080\"\nroutes: []\n";
        let cfg2 = Config::from_str(bare).unwrap();
        assert_eq!(cfg2.health_check.interval_secs, 5);
        assert_eq!(cfg2.health_check.fail_threshold, 3);
    }

    #[test]
    fn config_upstream_tls_parses_and_defaults_verify() {
        let yaml = "default_backend: \"127.0.0.1:8080\"\nupstream_tls:\n  enabled: true\n  sni: \"svc.internal\"\nroutes: []\n";
        let cfg = Config::from_str(yaml).unwrap();
        let tls = cfg.upstream_tls.unwrap();
        assert!(tls.enabled);
        assert_eq!(tls.sni, "svc.internal");
        assert!(tls.verify, "verify defaults to true");

        let bare = "default_backend: \"127.0.0.1:8080\"\nroutes: []\n";
        assert!(Config::from_str(bare).unwrap().upstream_tls.is_none());
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
        let bad =
            "default_backend: \"127.0.0.1:8080\"\nhealth_check:\n  path: \"healthz\"\nroutes: []\n";
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
        assert_eq!(
            cfg.routes[0].mode,
            LbMode::ActiveActive,
            "default is multi-active"
        );
        assert_eq!(cfg.routes[1].mode, LbMode::ActiveStandby);
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
        assert_eq!(
            cfg.routes[0].redirect.as_deref(),
            Some("https://new.example.com/")
        );
        assert_eq!(
            cfg.routes[0].redirect_code, 302,
            "default redirect code is 302"
        );
        assert!(cfg.routes[0].backend.is_none() && cfg.routes[0].backends.is_none());
        assert_eq!(
            cfg.routes[1].redirect.as_deref(),
            Some("https://example.com/dest")
        );
        assert_eq!(cfg.routes[1].redirect_code, 301);
    }

    #[test]
    fn config_redirect_validates_mutual_exclusive_and_code() {
        let both = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    backend: \"1.1.1.1:80\"\n    redirect: \"https://x\"\n";
        let bad_code = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    redirect: \"https://x\"\n    redirect_code: 200\n";
        let nonstandard = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    redirect: \"https://x\"\n    redirect_code: 304\n";
        assert!(
            Config::from_str(both).is_err(),
            "redirect + backend is ambiguous"
        );
        assert!(
            Config::from_str(bad_code).is_err(),
            "redirect_code must be a standard redirect status"
        );
        assert!(
            Config::from_str(nonstandard).is_err(),
            "304 is not a redirect status"
        );
    }

    #[test]
    fn config_rejects_empty_redirect() {
        let empty = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    redirect: \"\"\n";
        let blank =
            "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    redirect: \"   \"\n";
        assert!(
            Config::from_str(empty).is_err(),
            "empty redirect target rejected"
        );
        assert!(
            Config::from_str(blank).is_err(),
            "whitespace-only redirect target rejected"
        );
    }
}
