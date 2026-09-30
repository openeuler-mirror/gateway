use ipnet::IpNet;
use serde::Deserialize;
use std::net::SocketAddr;

use crate::client::parse_client_ip;
use crate::config::{parse_addr, LbMode, RouteTimeouts};

/// Nginx-style `proxy_pass` target, parsed from `scheme://ip[:port][/prefix]`.
///
/// Semantics follow nginx: when the URL carries a path, the route's matched
/// path prefix is replaced by that path (`/osk1/foo` with target
/// `http://h:52341/` becomes `/foo`); a bare authority leaves the request URI
/// unchanged. The `Host` header is rewritten to the URL authority, and an
/// `https` target upgrades this route only (independent of the global
/// `upstream_tls` setting).
#[derive(Debug, Clone)]
pub(crate) struct ProxyPass {
    pub(crate) addr: SocketAddr,
    pub(crate) tls: bool,
    /// SNI for https targets (the bare host of the URL).
    pub(crate) sni: String,
    /// Value for the upstream `Host` header (the URL authority as written).
    pub(crate) host_header: String,
    /// Path prefix from the URL; `None` means bare authority (URI unchanged).
    pub(crate) path_prefix: Option<String>,
}

/// Parse a `proxy_pass` URL. Targets must be literal `ip[:port]` — the same
/// rule as `backend`/`backends`: the LB does no DNS resolution, and `HttpPeer`
/// needs a ready address. A missing port defaults to 80 (http) / 443 (https).
pub(crate) fn parse_proxy_pass(s: &str) -> std::result::Result<ProxyPass, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("proxy_pass target must not be empty".into());
    }
    if s.contains(['\r', '\n']) {
        return Err("proxy_pass target must not contain CR/LF".into());
    }
    let (scheme, rest) = s
        .split_once("://")
        .ok_or_else(|| "proxy_pass must look like http(s)://ip[:port][/path]".to_string())?;
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "http" => false,
        "https" => true,
        other => {
            return Err(format!(
                "proxy_pass scheme must be http or https, got '{other}'"
            ))
        }
    };
    // Authority runs to the first '/'; everything after is the path prefix.
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(format!("/{p}"))),
        None => (rest, None),
    };
    if let Some(p) = &path {
        if p.contains(['?', '#']) {
            return Err("proxy_pass path prefix must not contain '?' or '#'".into());
        }
    }
    let addr = if let Ok(a) = authority.parse::<SocketAddr>() {
        a
    } else {
        // No explicit port (or bare bracketed IPv6): parse the IP and apply
        // the scheme's default port.
        let bare = authority
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(authority);
        let ip = bare.parse::<std::net::IpAddr>().map_err(|_| {
            format!(
                "proxy_pass target must be a literal ip[:port] (DNS names are \
                 not supported): '{authority}'"
            )
        })?;
        SocketAddr::new(ip, if tls { 443 } else { 80 })
    };
    Ok(ProxyPass {
        addr,
        tls,
        sni: if tls { addr.ip().to_string() } else { String::new() },
        host_header: authority.to_string(),
        path_prefix: path,
    })
}

#[derive(Debug)]
pub(crate) struct Route {
    pub(crate) host: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) client_ip: Option<IpNet>,
    pub(crate) backend: Option<SocketAddr>,
    pub(crate) backends: Option<Vec<SocketAddr>>,
    /// If set, the LB returns a 3xx redirect to this URL instead of proxying.
    pub(crate) redirect: Option<String>,
    /// Redirect status code (3xx). Defaults to 302.
    pub(crate) redirect_code: u16,
    /// If set, requests are transparently proxied to this target (nginx
    /// `proxy_pass` semantics) instead of using `backend`/`backends`.
    pub(crate) proxy_pass: Option<ProxyPass>,
    pub(crate) mode: LbMode,
    /// Per-route timeout overrides (optional).
    pub(crate) timeouts: Option<RouteTimeouts>,
}

impl Route {
    pub(crate) fn from_raw(raw: RouteRaw) -> std::result::Result<Self, String> {
        let client_ip = raw.client_ip.as_deref().map(parse_client_ip).transpose()?;

        // Exactly one of backend / backends / redirect / proxy_pass must be set.
        let set_count = [
            raw.backend.is_some(),
            raw.backends.is_some(),
            raw.redirect.is_some(),
            raw.proxy_pass.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        if set_count != 1 {
            return Err(format!(
                "route must set exactly one of `backend`, `backends`, `redirect`, `proxy_pass` (found {set_count})"
            ));
        }

        // redirect must point somewhere (reject empty / whitespace-only).
        if let Some(r) = raw.redirect.as_ref() {
            if r.trim().is_empty() {
                return Err("redirect target must not be empty".into());
            }
        }

        let proxy_pass = raw
            .proxy_pass
            .as_deref()
            .map(parse_proxy_pass)
            .transpose()?;

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
            // redirect-only / proxy_pass-only route: neither backend nor backends.
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
            proxy_pass,
            mode: raw.mode,
            timeouts: raw.timeouts,
        })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RouteRaw {
    pub(crate) host: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) client_ip: Option<String>,
    pub(crate) backend: Option<String>,
    pub(crate) backends: Option<Vec<String>>,
    pub(crate) redirect: Option<String>,
    pub(crate) redirect_code: Option<u16>,
    pub(crate) proxy_pass: Option<String>,
    #[serde(default)]
    pub(crate) mode: LbMode,
    pub(crate) timeouts: Option<RouteTimeouts>,
}
pub(crate) fn host_matches(request_host: &str, pattern: &str) -> bool {
    let req = request_host.to_lowercase();
    let pat = pattern.to_lowercase();
    if let Some(suffix) = pat.strip_prefix("*.") {
        req.ends_with(&format!(".{suffix}"))
    } else {
        req == pat
    }
}

/// Match `path` against a route prefix at a segment boundary: `/api` matches
/// `/api` and `/api/...` but not `/api2`. A trailing slash in the pattern is
/// normalized away; `/` (or empty) matches everything.
pub(crate) fn path_matches(path: &str, pattern: &str) -> bool {
    let prefix = pattern.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
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
    let is_bare_ipv6 =
        host.matches(':').count() >= 2 && host.chars().all(|c| c.is_ascii_hexdigit() || c == ':');
    if is_bare_ipv6 {
        return host;
    }
    // DNS / IPv4 host with an optional `:port`.
    match host.rsplit_once(':') {
        Some((head, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => host,
    }
}

/// Resolve the request host: prefer the URI authority (HTTP/2 puts
/// `:authority` there, and the `host` header is absent), then fall back to the
/// HTTP/1 `Host` header. IPv6 literals are normalized to bare form (`::1`), the
/// same representation both protocols yield after `Uri::host()`.
pub(crate) fn request_host<'a>(uri_host: Option<&'a str>, host_header: Option<&'a str>) -> &'a str {
    let host = strip_port(uri_host.or(host_header).unwrap_or(""));
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_host_prefers_uri_authority_and_strips_port() {
        // HTTP/2: authority lives in the URI, no Host header present.
        assert_eq!(
            request_host(Some("api.example.com"), None),
            "api.example.com"
        );
        assert_eq!(
            request_host(Some("api.example.com:8443"), None),
            "api.example.com"
        );
        // HTTP/1: authority absent, Host header is the source.
        assert_eq!(
            request_host(None, Some("app.example.com:8080")),
            "app.example.com"
        );
        assert_eq!(
            request_host(None, Some("app.example.com")),
            "app.example.com"
        );
        // IPv6 literals normalize to the same bare form on both paths:
        // HTTP/2 `Uri::host()` yields `::1`; an HTTP/1 Host header carries
        // brackets plus an optional port.
        assert_eq!(request_host(None, Some("[::1]:8080")), "::1");
        assert_eq!(request_host(Some("::1"), None), "::1");
        // Neither present -> empty.
        assert_eq!(request_host(None, None), "");
    }

    #[test]
    fn path_matches_respects_segment_boundaries() {
        // exact and subtree
        assert!(path_matches("/api", "/api"));
        assert!(path_matches("/api/v1", "/api"));
        assert!(path_matches("/api/", "/api"));
        // no false positives on a longer segment
        assert!(!path_matches("/api2", "/api"));
        assert!(!path_matches("/api2/v1", "/api"));
        // trailing slash in the pattern is normalized
        assert!(path_matches("/api/v1", "/api/"));
        assert!(!path_matches("/api2", "/api/"));
        // root matches everything
        assert!(path_matches("/anything", "/"));
        assert!(path_matches("/anything", ""));
    }

    #[test]
    fn host_matches_exact_case_insensitive_and_wildcard() {
        assert!(host_matches("api.example.com", "api.example.com"));
        // Case-insensitive on both sides.
        assert!(host_matches("API.Example.COM", "api.example.com"));
        // Wildcard matches any subdomain, but not the bare apex.
        assert!(host_matches("www.example.com", "*.example.com"));
        assert!(host_matches("a.b.example.com", "*.example.com"));
        assert!(!host_matches("example.com", "*.example.com"));
        assert!(!host_matches("other.com", "api.example.com"));
    }

    #[test]
    fn strip_port_handles_ipv4_ipv6_and_dns() {
        assert_eq!(strip_port("example.com:8080"), "example.com");
        assert_eq!(strip_port("example.com"), "example.com");
        assert_eq!(strip_port("127.0.0.1:8080"), "127.0.0.1");
        // Bracketed IPv6 literal with port keeps the brackets.
        assert_eq!(strip_port("[::1]:8080"), "[::1]");
        // Bare IPv6 (what `Uri::host()` yields) is left untouched.
        assert_eq!(strip_port("::1"), "::1");
        assert_eq!(strip_port("[2001:db8::1]"), "[2001:db8::1]");
    }

    #[test]
    fn parse_proxy_pass_http_target_with_port_and_path() {
        let t = parse_proxy_pass("http://7.150.1.218:52341/").unwrap();
        assert_eq!(t.addr, "7.150.1.218:52341".parse().unwrap());
        assert!(!t.tls);
        assert_eq!(t.host_header, "7.150.1.218:52341");
        assert_eq!(t.path_prefix.as_deref(), Some("/"));
        assert!(t.sni.is_empty(), "SNI unused for http targets");

        let t = parse_proxy_pass("http://10.0.0.4:90/api/v2").unwrap();
        assert_eq!(t.addr, "10.0.0.4:90".parse().unwrap());
        assert_eq!(t.path_prefix.as_deref(), Some("/api/v2"));
    }

    #[test]
    fn parse_proxy_pass_bare_authority_means_uri_unchanged() {
        let t = parse_proxy_pass("http://10.0.0.1:8080").unwrap();
        assert!(t.path_prefix.is_none(), "no URL path => URI forwarded as-is");
        assert_eq!(t.addr, "10.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn parse_proxy_pass_https_defaults_port_and_sets_sni() {
        let t = parse_proxy_pass("https://10.0.0.2").unwrap();
        assert_eq!(t.addr, "10.0.0.2:443".parse().unwrap());
        assert!(t.tls);
        assert_eq!(t.sni, "10.0.0.2");
        assert_eq!(t.host_header, "10.0.0.2");

        // Scheme is case-insensitive; http without a port defaults to 80.
        let t = parse_proxy_pass("HTTP://10.0.0.3").unwrap();
        assert_eq!(t.addr, "10.0.0.3:80".parse().unwrap());
        assert!(!t.tls);
    }

    #[test]
    fn parse_proxy_pass_ipv6_forms() {
        let t = parse_proxy_pass("http://[::1]:8080/x").unwrap();
        assert_eq!(t.addr, "[::1]:8080".parse().unwrap());
        assert_eq!(t.host_header, "[::1]:8080");
        assert_eq!(t.path_prefix.as_deref(), Some("/x"));

        // Bracketed IPv6 without a port gets the scheme default.
        let t = parse_proxy_pass("http://[::1]").unwrap();
        assert_eq!(t.addr, "[::1]:80".parse().unwrap());
    }

    #[test]
    fn parse_proxy_pass_rejects_bad_targets() {
        assert!(parse_proxy_pass("").is_err());
        assert!(parse_proxy_pass("   ").is_err());
        assert!(parse_proxy_pass("7.150.1.218:52341").is_err(), "no scheme");
        assert!(parse_proxy_pass("ftp://1.2.3.4").is_err(), "scheme must be http(s)");
        assert!(
            parse_proxy_pass("http://example.com/").is_err(),
            "DNS names unsupported (same rule as backend)"
        );
        assert!(parse_proxy_pass("http://1.2.3.4/\rX-Inject: 1").is_err(), "CR/LF rejected");
        assert!(parse_proxy_pass("http://1.2.3.4:80/?a=1").is_err(), "query in target rejected");
    }
}
