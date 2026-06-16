use crate::routes::GatewayErrorReply;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use boom_core::types::AuthIdentity;
use boom_core::GatewayError;

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
        let raw_key = extract_api_key(parts);

        let raw_key = raw_key.ok_or_else(|| {
            GatewayErrorReply(GatewayError::AuthError("Missing API key".to_string()), false)
        })?;

        let inner = state.inner.load();
        let identity = inner
            .auth
            .authenticate(&raw_key)
            .await
            .map_err(|e| GatewayErrorReply(e, false))?;

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
