use crate::hooks::PreAuthOutcome;
use crate::request_log::log_auth_error;
use crate::routes::{extract_client_ip, GatewayErrorReply};
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use boom_core::types::AuthIdentity;
use boom_core::GatewayError;
use std::time::Instant;
use uuid::Uuid;

/// Axum extractor that validates API key from the Authorization header.
pub struct RequiredAuth {
    identity: AuthIdentity,
}

impl RequiredAuth {
    pub fn identity(&self) -> &AuthIdentity {
        &self.identity
    }

    #[allow(dead_code)]
    pub fn into_identity(self) -> AuthIdentity {
        self.identity
    }
}

impl FromRequestParts<AppState> for RequiredAuth {
    type Rejection = GatewayErrorReply;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let start = Instant::now();
        let api_path = parts.uri.path().to_string();
        let client_ip = Some(extract_client_ip(&parts.headers, None));
        let raw_key = extract_api_key(parts);

        let raw_key = match raw_key {
            Some(k) => k,
            None => {
                // No key at all — can't compute a stable hash, so we can't
                // write to boom_request_log (key_hash is NOT NULL). Surface
                // via tracing only.
                tracing::warn!(
                    status_code = 401,
                    error_type = "authentication_error",
                    path = %api_path,
                    "Missing API key"
                );
                return Err(GatewayErrorReply(
                    GatewayError::AuthError("Missing API key".to_string()),
                    false,
                ));
            }
        };

        let inner = state.inner.load();

        // — pre_auth hook (optional) —
        // When no plugin is configured for this point (`NoHook`), or the hook
        // said `Continue`, the original raw_key is forwarded to the
        // authenticator unchanged. `Replace` swaps the key. `Reject` short-
        // circuits with 401. `Deny` (only when failure_mode=deny and the call
        // failed) returns 500.
        let effective_key = match inner.hooks.pre_auth(&raw_key, &parts.headers) {
            PreAuthOutcome::NoHook | PreAuthOutcome::Continue => raw_key,
            PreAuthOutcome::Replace(new_key) => new_key,
            PreAuthOutcome::Reject(reason) => {
                let err = GatewayError::AuthError(reason);
                log_auth_error(
                    state,
                    &raw_key,
                    &api_path,
                    start,
                    &err,
                    Some(Uuid::new_v4().to_string()),
                    client_ip.clone(),
                );
                return Err(GatewayErrorReply(err, false));
            }
            PreAuthOutcome::Deny => {
                let err = GatewayError::InternalError("pre_auth hook failure (deny mode)".into());
                log_auth_error(
                    state,
                    &raw_key,
                    &api_path,
                    start,
                    &err,
                    Some(Uuid::new_v4().to_string()),
                    client_ip.clone(),
                );
                return Err(GatewayErrorReply(err, false));
            }
        };

        let identity = match inner.auth.authenticate(&effective_key).await {
            Ok(id) => id,
            Err(e) => {
                // KeyExpired / KeyBlocked / BudgetExceeded are deduped per
                // (key_hash, "<auth>", 60s); AuthError (401) is logged in
                // full. hash_token(effective_key) gives a stable key_hash
                // matching the DB's token column when no hook replaced the
                // key; with a Replace hook, the hash is of the replaced
                // key, which is what the authenticator actually saw.
                log_auth_error(
                    state,
                    &effective_key,
                    &api_path,
                    start,
                    &e,
                    Some(Uuid::new_v4().to_string()),
                    client_ip.clone(),
                );
                return Err(GatewayErrorReply(e, false));
            }
        };

        Ok(Self { identity })
    }
}

/// Extract API key from request headers.
/// Supports: Authorization: Bearer xxx, x-api-key: xxx, api-key: xxx.
fn extract_api_key(parts: &Parts) -> Option<String> {
    // 1. Authorization: Bearer xxx
    if let Some(auth) = parts.headers.get("authorization") {
        let val = auth.to_str().ok()?;
        if let Some((scheme, key)) = val.split_once(' ') {
            if scheme.eq_ignore_ascii_case("bearer") {
                let key = key.trim();
                if !key.is_empty() {
                    return Some(key.to_string());
                }
            }
        }
    }

    // 2. x-api-key (Anthropic-style)
    if let Some(key) = parts.headers.get("x-api-key") {
        return key.to_str().ok().map(|s| s.to_string());
    }

    // 3. api-key (Azure-style)
    if let Some(key) = parts.headers.get("api-key") {
        return key.to_str().ok().map(|s| s.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::extract_api_key;
    use axum::http::header::HeaderValue;
    use axum::http::Request;

    fn extract_from_header(name: &str, value: &str) -> Option<String> {
        let request = Request::builder()
            .uri("/")
            .header(name, HeaderValue::from_str(value).unwrap())
            .body(())
            .unwrap();
        let (parts, _) = request.into_parts();
        extract_api_key(&parts)
    }

    #[test]
    fn accepts_bearer_token_case_insensitively() {
        assert_eq!(
            extract_from_header("authorization", "Bearer demo-token"),
            Some("demo-token".to_string())
        );
        assert_eq!(
            extract_from_header("authorization", "bearer demo-token"),
            Some("demo-token".to_string())
        );
    }

    #[test]
    fn rejects_basic_authorization_header() {
        assert_eq!(
            extract_from_header("authorization", "Basic ZGVtbzpkZW1v"),
            None
        );
    }

    #[test]
    fn accepts_api_key_headers() {
        assert_eq!(
            extract_from_header("x-api-key", "demo-token"),
            Some("demo-token".to_string())
        );
        assert_eq!(
            extract_from_header("api-key", "demo-token"),
            Some("demo-token".to_string())
        );
    }
}
