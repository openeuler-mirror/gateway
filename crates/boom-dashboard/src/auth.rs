use crate::state::DashboardState;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

// ── Login rate-limit constants ─────────────────────────────

/// Failures before the first lockout.
const MAX_LOGIN_FAILURES: u32 = 5;
/// Lockout duration on the first lockout trigger (6th failure).
const INITIAL_LOCKOUT: Duration = Duration::from_secs(10);
/// Additional lockout per subsequent failure after initial lockout.
const PER_FAILURE_LOCKOUT: Duration = Duration::from_secs(30);

// ── JWT Claims ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardClaims {
    /// "admin" or user_id.
    pub sub: String,
    /// "admin" | "user".
    pub role: String,
    /// User's token hash (empty for admin).
    pub key_hash: String,
    pub exp: i64,
    pub iat: i64,
}

const SESSION_COOKIE_NAME: &str = "boom_session";
const SESSION_DURATION_SECS: i64 = 7200; // 2 hours

// ── Session Extractor ──────────────────────────────────────

/// Extractor that reads the session cookie and verifies the JWT.
#[derive(Debug, Clone)]
pub struct DashboardSession {
    pub claims: DashboardClaims,
}

impl<S: Send + Sync> FromRequestParts<S> for DashboardSession {
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let result = extract_session(parts);
        std::future::ready(result)
    }
}

fn extract_session(parts: &mut Parts) -> Result<DashboardSession, Response> {
    let state = parts
        .extensions
        .get::<std::sync::Arc<DashboardState>>()
        .cloned()
        .ok_or_else(|| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "DashboardState not found",
            )
                .into_response()
        })?;

    let cookie_header = parts
        .headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|cookie| {
            let cookie = cookie.trim();
            let (name, value) = cookie.split_once('=')?;
            if name.trim() == SESSION_COOKIE_NAME {
                Some(value.trim().to_string())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            (axum::http::StatusCode::UNAUTHORIZED, "No session cookie").into_response()
        })?;

    let token_data = decode::<DashboardClaims>(
        &cookie_header,
        &DecodingKey::from_secret(state.jwt_secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| {
        (axum::http::StatusCode::UNAUTHORIZED, "Invalid session").into_response()
    })?;

    Ok(DashboardSession {
        claims: token_data.claims,
    })
}

// ── Admin Session Extractor ────────────────────────────────

/// Extractor that requires admin role.
pub struct AdminSession {
    pub claims: DashboardClaims,
}

impl<S: Send + Sync> FromRequestParts<S> for AdminSession {
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let result = extract_session(parts).and_then(|session| {
            if session.claims.role == "admin" {
                Ok(AdminSession {
                    claims: session.claims,
                })
            } else {
                Err(
                    (axum::http::StatusCode::FORBIDDEN, "Admin access required")
                        .into_response(),
                )
            }
        });
        std::future::ready(result)
    }
}

// ── Login Request ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub user_id: String,
    pub api_key: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub role: String,
    pub user_id: String,
    /// Original API key, returned only for user logins so the dashboard chat
    /// window can call /v1/chat/completions with a Bearer header. None for admin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub user_id: String,
    pub role: String,
}

// ── IP Extraction ─────────────────────────────────────────

/// Extract the client IP used as the login rate-limit key.
///
/// The key must not be forgeable by the caller. Proxy headers
/// (`X-Real-IP` / `X-Forwarded-For`) are trivially spoofable, so they are only
/// honoured when the TCP peer is a trusted local proxy (loopback or a private
/// network address — the shapes a reverse proxy actually arrives from). For
/// any other peer the connection's own IP wins and the headers are ignored.
///
/// The TCP fallback also removes the old "every direct client shares
/// `unknown`" behaviour, which let one caller lock out everybody.
fn extract_client_ip(
    headers: &axum::http::HeaderMap,
    remote_addr: Option<std::net::SocketAddr>,
) -> String {
    let peer_ip = remote_addr.map(|addr| addr.ip());

    // Untrusted peer: the connection address is authoritative.
    if let Some(ip) = peer_ip {
        if !is_trusted_proxy(ip) {
            return ip.to_string();
        }
    }

    // Trusted proxy (or no peer information at all): read the forwarded chain.
    // Values must parse as real IPs — a forged header cannot be allowed to seed
    // the login-attempt map with arbitrary strings.
    if let Some(val) = headers.get("X-Real-IP").and_then(|v| v.to_str().ok()) {
        let ip = val.trim();
        if !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok() {
            return ip.to_string();
        }
    }
    if let Some(val) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok()) {
        // Standard reverse proxies *append* the real address:
        //   X-Forwarded-For: <client-supplied>, ..., <real peer>
        // so only the last segment was written by the proxy we trust. Taking the
        // first one hands the rate-limit key straight back to the client.
        if let Some(ip) = val.rsplit(',').next() {
            let ip = ip.trim();
            if !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok() {
                return ip.to_string();
            }
        }
    }

    peer_ip
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Whether an address is one a local reverse proxy would arrive from:
/// loopback, or an RFC 1918 / link-local / ULA private address.
///
/// An IPv4-mapped IPv6 peer (`::ffff:127.0.0.1`) is judged as the embedded IPv4
/// address: a dual-stack listener reports a loopback proxy in that form and
/// `Ipv6Addr::is_loopback()` is false for it. Misjudging it would push every
/// proxied login into one shared bucket — the DoS #75 fixed.
fn is_trusted_proxy(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_loopback() || v4.is_private() || v4.is_link_local();
            }
            let head = v6.segments()[0];
            v6.is_loopback()                  // ::1
                || (head & 0xfe00) == 0xfc00  // fc00::/7 unique local
                || (head & 0xffc0) == 0xfe80  // fe80::/10 link local
        }
    }
}

// ── Login Rate Limiting ───────────────────────────────────

/// Check if an IP is currently locked out. Returns remaining lockout time if locked.
/// If the lockout has expired, resets the counter so the IP gets a fresh start.
fn check_login_lockout(state: &DashboardState, client_ip: &str) -> Option<Duration> {
    let map = &state.login_attempts;
    if let Some(entry) = map.get(client_ip) {
        if let Some(locked_until) = entry.locked_until {
            let now = Instant::now();
            if now < locked_until {
                return Some(locked_until - now);
            }
            // Lockout expired — remove entry to reset fail_count.
            drop(entry);
            map.remove(client_ip);
        }
    }
    None
}

/// Record a failed login attempt.
///
/// Lockout strategy:
///   - Failures 1..5: no lockout.
///   - 6th failure: lock for INITIAL_LOCKOUT (10s).
///   - Each subsequent failure extends lockout by PER_FAILURE_LOCKOUT (30s).
fn record_login_failure(state: &DashboardState, client_ip: &str) -> bool {
    let map = &state.login_attempts;
    let now = Instant::now();

    map.entry(client_ip.to_string())
        .and_modify(|attempt| {
            attempt.fail_count += 1;
            if attempt.fail_count >= MAX_LOGIN_FAILURES {
                let extra = if attempt.fail_count == MAX_LOGIN_FAILURES {
                    INITIAL_LOCKOUT
                } else {
                    PER_FAILURE_LOCKOUT
                };
                attempt.locked_until = Some(
                    attempt.locked_until.map_or(now, |t| t.max(now)) + extra,
                );
            }
        })
        .or_insert(crate::state::LoginAttempt {
            fail_count: 1,
            locked_until: None,
        });

    map.get(client_ip)
        .map(|e| e.locked_until.is_some())
        .unwrap_or(false)
}

/// Clear login failure state on successful login.
fn clear_login_failures(state: &DashboardState, client_ip: &str) {
    state.login_attempts.remove(client_ip);
}

// ── Login Handler ──────────────────────────────────────────

pub async fn login(
    Extension(state): Extension<std::sync::Arc<DashboardState>>,
    ConnectInfo(remote_addr): ConnectInfo<std::net::SocketAddr>,
    req: axum::http::Request<axum::body::Body>,
) -> Response {
    let client_ip = extract_client_ip(req.headers(), Some(remote_addr));

    // 1. Rate-limit check.
    if let Some(remaining) = check_login_lockout(&state, &client_ip) {
        tracing::warn!(
            ip = %client_ip,
            remaining_secs = remaining.as_secs(),
            "Login blocked: too many attempts"
        );
        return json_error_response(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            &format!("Too many login attempts. Try again in {} seconds.", remaining.as_secs()),
        );
    }

    // 2. Deserialize body.
    let LoginRequest { user_id, api_key } = match axum::body::to_bytes(req.into_body(), 4096).await
    {
        Ok(bytes) => match serde_json::from_slice::<LoginRequest>(&bytes) {
            Ok(req) => req,
            Err(e) => {
                tracing::warn!(ip = %client_ip, "Login: invalid JSON body: {}", e);
                return json_error_response(
                    axum::http::StatusCode::BAD_REQUEST,
                    "Invalid request body",
                );
            }
        },
        Err(e) => {
            tracing::warn!(ip = %client_ip, "Login: failed to read body: {}", e);
            return json_error_response(
                axum::http::StatusCode::BAD_REQUEST,
                "Failed to read request body",
            );
        }
    };

    tracing::info!(ip = %client_ip, user_id = %user_id, "Login attempt");

    // 3. Admin login: user_id == "admin" + constant-time comparison with master_key.
    if user_id == "admin" {
        let master_key = match &state.master_key {
            Some(k) => k,
            None => {
                return json_error_response(
                    axum::http::StatusCode::FORBIDDEN,
                    "Admin login disabled",
                );
            }
        };

        // Constant-time comparison.
        let equal = constant_time_eq(api_key.as_bytes(), master_key.as_bytes());
        if !equal {
            let _locked = record_login_failure(&state, &client_ip);
            tracing::warn!(ip = %client_ip, "Admin login: invalid credentials");
            return json_error_response(
                axum::http::StatusCode::UNAUTHORIZED,
                "Invalid credentials",
            );
        }

        clear_login_failures(&state, &client_ip);
        tracing::info!(ip = %client_ip, "Admin login success");
        return sign_and_respond(&state, "admin".to_string(), "admin".to_string(), String::new(), None);
    }

    // 4. User login: hash the key, then lookup in DB.
    let db_pool = match &state.db_pool {
        Some(pool) => pool,
        None => {
            tracing::error!("Login failed: no database pool configured");
            return json_error_response(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Database not available",
            );
        }
    };

    // All keys are stored as SHA-256 of the entire raw key. This matches the
    // /v1 authenticate path (boom-auth) and the dashboard create-key path,
    // and is litellm-compatible. Prefix (if any) participates in the hash.
    let token_hash = hash_token(&api_key);

    let row_result: Result<Option<(Option<String>, Option<String>, Option<bool>)>, _> = sqlx::query_as(
        r#"SELECT user_id, key_alias, blocked FROM "boom_verification_token" WHERE token = $1"#,
    )
    .bind(&token_hash)
    .fetch_optional(db_pool)
    .await;

    let row = match row_result {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                ip = %client_ip,
                token_hash = &token_hash[..8.min(token_hash.len())],
                "Login DB query failed: {}",
                e
            );
            return json_error_response(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Database error during login",
            );
        }
    };

    let (uid, key_alias, blocked) = match row {
        Some((uid, alias, blk)) => (uid, alias, blk),
        None => {
            let _locked = record_login_failure(&state, &client_ip);
            tracing::warn!(
                ip = %client_ip,
                token_hash = &token_hash[..8.min(token_hash.len())],
                "Login: key not found in DB"
            );
            return json_error_response(
                axum::http::StatusCode::UNAUTHORIZED,
                "Invalid API key",
            );
        }
    };

    // Check blocked.
    if blocked.unwrap_or(false) {
        tracing::warn!(
            ip = %client_ip,
            token_hash = &token_hash[..8.min(token_hash.len())],
            "Login: key is blocked"
        );
        return json_error_response(
            axum::http::StatusCode::FORBIDDEN,
            "Key is blocked",
        );
    }

    clear_login_failures(&state, &client_ip);
    tracing::info!(
        ip = %client_ip,
        alias = ?key_alias,
        "User login success"
    );

    // Use key_alias as display name, fallback to user_id or "user".
    let display_name = key_alias
        .or(uid)
        .unwrap_or_else(|| "user".to_string());

    sign_and_respond(&state, display_name, "user".to_string(), token_hash, Some(api_key.clone()))
}

fn sign_and_respond(
    state: &DashboardState,
    user_id: String,
    role: String,
    key_hash: String,
    api_key: Option<String>,
) -> Response {
    let now = Utc::now().timestamp();
    let claims = DashboardClaims {
        sub: user_id.clone(),
        role: role.clone(),
        key_hash,
        exp: now + SESSION_DURATION_SECS,
        iat: now,
    };

    let token = match encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    ) {
        Ok(t) => t,
        Err(_) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create session",
            )
                .into_response();
        }
    };

    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/dashboard; Max-Age={}",
        SESSION_COOKIE_NAME, token, SESSION_DURATION_SECS
    );

    let body = Json(LoginResponse { role, user_id, api_key });

    ([(SET_COOKIE, cookie)], body).into_response()
}

// ── Logout Handler ─────────────────────────────────────────

pub async fn logout() -> Response {
    let cookie = format!(
        "{}=; HttpOnly; SameSite=Lax; Path=/dashboard; Max-Age=0",
        SESSION_COOKIE_NAME
    );
    ([(SET_COOKIE, cookie)], axum::http::StatusCode::NO_CONTENT).into_response()
}

// ── Me Handler ─────────────────────────────────────────────

pub async fn me(session: DashboardSession) -> Json<MeResponse> {
    Json(MeResponse {
        user_id: session.claims.sub,
        role: session.claims.role,
    })
}

// ── Helpers ────────────────────────────────────────────────

fn json_error_response(status: axum::http::StatusCode, message: &str) -> Response {
    let mut resp = Json(serde_json::json!({ "error": message })).into_response();
    *resp.status_mut() = status;
    resp
}

pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

#[cfg(test)]
mod tests {
    use super::extract_client_ip;
    use axum::http::{HeaderMap, HeaderValue};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn with(name: &'static str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_str(value).unwrap());
        h
    }

    fn peer(addr: [u8; 4]) -> Option<SocketAddr> {
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(addr)), 12345))
    }

    /// A public client cannot choose its rate-limit key: the TCP peer wins and
    /// the spoofable proxy headers are ignored.
    #[test]
    fn public_peer_overrides_spoofed_headers() {
        let headers = with("x-real-ip", "203.0.113.7");
        let ip = extract_client_ip(&headers, peer([198, 51, 100, 9]));
        println!("public peer -> {ip}");
        assert_eq!(ip, "198.51.100.9");
    }

    #[test]
    fn forwarded_for_is_ignored_for_public_peers() {
        let headers = with("x-forwarded-for", "203.0.113.7, 10.0.0.1");
        let ip = extract_client_ip(&headers, peer([198, 51, 100, 9]));
        assert_eq!(ip, "198.51.100.9");
    }

    /// Behind a local reverse proxy the forwarded chain is the only way to tell
    /// clients apart, so it is honoured.
    #[test]
    fn trusted_proxy_headers_are_honoured() {
        let headers = with("x-real-ip", "203.0.113.7");
        assert_eq!(
            extract_client_ip(&headers, peer([127, 0, 0, 1])),
            "203.0.113.7"
        );
        assert_eq!(
            extract_client_ip(&headers, peer([10, 0, 0, 5])),
            "203.0.113.7"
        );
    }

    /// Without any proxy header the real peer address is used, so two direct
    /// clients no longer share one lockout bucket.
    #[test]
    fn direct_clients_do_not_share_a_bucket() {
        let a = extract_client_ip(&HeaderMap::new(), peer([198, 51, 100, 9]));
        let b = extract_client_ip(&HeaderMap::new(), peer([198, 51, 100, 10]));
        println!("a={a} b={b}");
        assert_eq!(a, "198.51.100.9");
        assert_eq!(b, "198.51.100.10");
    }

    /// Last-resort fallback when the peer address is unavailable.
    #[test]
    fn missing_peer_falls_back_to_unknown() {
        assert_eq!(extract_client_ip(&HeaderMap::new(), None), "unknown");
    }

    fn peer_v6(addr: std::net::Ipv6Addr) -> Option<SocketAddr> {
        Some(SocketAddr::new(IpAddr::V6(addr), 12345))
    }

    /// A dual-stack listener reports a loopback reverse proxy as
    /// `::ffff:127.0.0.1`. `Ipv6Addr::is_loopback()` is false for it, so without
    /// unwrapping the mapped form every proxied login would share one bucket.
    #[test]
    fn ipv4_mapped_loopback_peer_is_trusted() {
        let headers = with("x-real-ip", "203.0.113.7");
        let mapped: std::net::Ipv6Addr = "::ffff:127.0.0.1".parse().unwrap();
        assert_eq!(
            extract_client_ip(&headers, peer_v6(mapped)),
            "203.0.113.7",
            "IPv4-mapped loopback must count as a trusted proxy"
        );

        let mapped_private: std::net::Ipv6Addr = "::ffff:10.1.2.3".parse().unwrap();
        assert_eq!(
            extract_client_ip(&headers, peer_v6(mapped_private)),
            "203.0.113.7"
        );
    }

    /// ULA (fc00::/7) and link-local (fe80::/10) proxies are local too.
    #[test]
    fn ipv6_ula_and_link_local_peers_are_trusted() {
        let headers = with("x-real-ip", "203.0.113.7");
        for proxy in ["fc00::1", "fd12:3456::7", "fe80::1"] {
            let addr: std::net::Ipv6Addr = proxy.parse().unwrap();
            assert_eq!(
                extract_client_ip(&headers, peer_v6(addr)),
                "203.0.113.7",
                "{proxy} should be treated as a trusted local proxy"
            );
        }
    }

    /// Reverse proxies append the real address, so the last segment is the one
    /// the proxy wrote. Using the first segment would honour the value the
    /// client injected itself.
    #[test]
    fn forwarded_for_uses_the_last_segment() {
        let headers = with("x-forwarded-for", "1.2.3.4, 10.0.0.1, 203.0.113.7");
        assert_eq!(
            extract_client_ip(&headers, peer([127, 0, 0, 1])),
            "203.0.113.7",
            "must not take the client-supplied first segment"
        );
    }

    /// A header that is not a valid IP falls through to the next source instead
    /// of becoming a rate-limit key (which would let a caller grow the map
    /// without bound with arbitrary strings).
    #[test]
    fn invalid_forwarded_headers_are_ignored() {
        let headers = with("x-forwarded-for", "not-an-ip");
        assert_eq!(extract_client_ip(&headers, peer([127, 0, 0, 1])), "127.0.0.1");

        let mut both = with("x-real-ip", "garbage");
        both.insert(
            "x-forwarded-for",
            HeaderValue::from_str("203.0.113.9").unwrap(),
        );
        assert_eq!(extract_client_ip(&both, peer([127, 0, 0, 1])), "203.0.113.9");
    }
}
