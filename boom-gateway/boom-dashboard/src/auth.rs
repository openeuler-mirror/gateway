use crate::state::DashboardState;
use axum::extract::FromRequestParts;
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
}

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub user_id: String,
    pub role: String,
}

// ── IP Extraction ─────────────────────────────────────────

/// Extract client IP from request headers (reverse-proxy aware).
fn extract_client_ip(headers: &axum::http::HeaderMap) -> String {
    // Try X-Real-IP first (set by nginx etc.)
    if let Some(val) = headers.get("X-Real-IP").and_then(|v| v.to_str().ok()) {
        let ip = val.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    // Try X-Forwarded-For (first IP in the list).
    if let Some(val) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok()) {
        if let Some(ip) = val.split(',').next() {
            let ip = ip.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }
    }
    "unknown".to_string()
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
    req: axum::http::Request<axum::body::Body>,
) -> Response {
    let client_ip = extract_client_ip(req.headers());

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
        return sign_and_respond(&state, "admin".to_string(), "admin".to_string(), String::new());
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

    // All keys are stored as SHA-256 hash in DB.
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

    sign_and_respond(&state, display_name, "user".to_string(), token_hash)
}

fn sign_and_respond(
    state: &DashboardState,
    user_id: String,
    role: String,
    key_hash: String,
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

    let body = Json(LoginResponse { role, user_id });

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
