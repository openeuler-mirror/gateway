use ipnet::IpNet;
use serde::Deserialize;
use std::net::SocketAddr;

use crate::client::parse_client_ip;
use crate::config::{parse_addr, LbMode};

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
    pub(crate) mode: LbMode,
}

impl Route {
    pub(crate) fn from_raw(raw: RouteRaw) -> std::result::Result<Self, String> {
        let client_ip = raw.client_ip.as_deref().map(parse_client_ip).transpose()?;

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
pub(crate) struct RouteRaw {
    pub(crate) host: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) client_ip: Option<String>,
    pub(crate) backend: Option<String>,
    pub(crate) backends: Option<Vec<String>>,
    pub(crate) redirect: Option<String>,
    pub(crate) redirect_code: Option<u16>,
    #[serde(default)]
    pub(crate) mode: LbMode,
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
}
