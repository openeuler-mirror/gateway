use crate::hooks::PreAuthOutcome;
use crate::request_log::log_auth_error;
use crate::routes::{GatewayErrorReply, extract_client_ip};
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Request};
use boom_core::types::AuthIdentity;
use boom_core::GatewayError;
use std::time::Instant;
use uuid::Uuid;

/// Body bytes buffered by `buffer_request_body` middleware so that
/// `RequiredAuth` (a `FromRequestParts`) can probe the `model` field for
/// the pre_auth hook, and `CachedJson<T>` (a `FromRequest`) can
/// re-deserialize the body without re-consuming the request stream.
///
/// Both extractors share the same buffered bytes parked here by the
/// middleware. Without this, axum 0.8 forbids two `FromRequest` extractors
/// in one handler signature (its `Handler` trait bound can't tell that
/// `CachedJson` would have read from extensions instead of the body
/// stream). The middleware runs *before* any extractor, buffers once,
/// and replaces the body with an empty one — so downstream extractors
/// see a body that's already gone (the bytes are in extensions).
///
/// `Clone` is required by `http::Extensions` (it stores `Arc` internally
/// but the type bound still applies) — `Bytes` is cheap-clone (refcount
/// bump), so this is zero-cost.
#[derive(Clone)]
pub struct CachedBodyBytes(pub Bytes);

/// Lightweight probe used to extract the top-level `model` field from the
/// request body *before* running the pre_auth hook. We don't want to fully
/// deserialize into `ChatCompletionRequest` / `AnthropicMessagesRequest`
/// here — that's the job of the downstream `CachedJson<T>` extractor. This
/// struct uses `#[serde(default)]` + ignores unknown fields, so it works on
/// any JSON body that has (or lacks) a top-level `model` field.
#[derive(Debug, serde::Deserialize)]
struct ModelProbe {
    #[serde(default)]
    model: Option<String>,
}

/// Axum extractor that validates API key from the Authorization header.
///
/// Implemented as `FromRequestParts` (NOT `FromRequest`) — this is a hard
/// axum 0.8 requirement: only one body-consuming `FromRequest` extractor
/// is allowed per handler signature, and that slot is reserved for
/// `CachedJson<T>` at the LLM handler entry points. For `RequiredAuth` to
/// still see the request's `model` field for the pre_auth hook, the
/// [`buffer_request_body`] middleware buffers the body bytes into
/// `Request::extensions()` as [`CachedBodyBytes`] *before* any extractor
/// runs. `RequiredAuth` then reads them back here.
///
/// For non-LLM handlers that use `RequiredAuth` but never read the body
/// (e.g. `get_model`, `admin_delete_plan`), the middleware is a no-op
/// when no body is present (GET / DELETE / empty-body POST) — it just
/// parks empty `CachedBodyBytes` so the extension lookup doesn't fail.
pub struct RequiredAuth {
    identity: AuthIdentity,
    /// Set by the pre_auth hook when it returns `ReplaceModel`. The handler
    /// is expected to write this into `req.model` before `check_model_access`
    /// so the key's model whitelist is checked against the rewritten name.
    new_model: Option<String>,
}

impl RequiredAuth {
    pub fn identity(&self) -> &AuthIdentity {
        &self.identity
    }

    /// Take the hook-decided `new_model`, if any. Leaves `None` in its place
    /// so subsequent calls return `None` (write-once semantics — the handler
    /// applies the rewrite exactly once, before `check_model_access`).
    pub fn take_new_model(&mut self) -> Option<String> {
        self.new_model.take()
    }

    #[allow(dead_code)]
    pub fn into_identity(self) -> AuthIdentity {
        self.identity
    }
}

impl FromRequestParts<AppState> for RequiredAuth {
    type Rejection = GatewayErrorReply;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let start = Instant::now();
        let api_path = parts.uri.path().to_string();
        let client_ip = Some(extract_client_ip(&parts.headers, None));
        let raw_key_opt = extract_api_key(parts);

        let raw_key = match raw_key_opt {
            Some(k) => k,
            None => {
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

        // Probe the body for the top-level `model` field. The body bytes
        // are parked in `parts.extensions` by `buffer_request_body`
        // middleware. If absent (handler called outside middleware-wrapped
        // routes, or GET / DELETE), probe is None — same as a request with
        // no model field. Failure here is non-fatal — the hook just
        // doesn't get a model to look at. The downstream CachedJson<T>
        // will surface the real parse error if the body is genuinely
        // broken.
        let model_opt = parts
            .extensions
            .get::<CachedBodyBytes>()
            .and_then(|cb| {
                serde_json::from_slice::<ModelProbe>(&cb.0)
                    .ok()
                    .and_then(|p| p.model)
            });

        // — pre_auth hook (optional) —
        // NoHook / Continue → use raw_key. Replace → swap key. ReplaceModel
        // → swap key AND surface new_model to the handler via take_new_model.
        // Reject → 401. Deny → 500.
        let mut new_model_opt: Option<String> = None;
        let effective_key = match inner
            .hooks
            .pre_auth(&raw_key, &parts.headers, model_opt.as_deref())
        {
            PreAuthOutcome::NoHook | PreAuthOutcome::Continue => raw_key,
            PreAuthOutcome::Replace(new_key) => new_key,
            PreAuthOutcome::ReplaceModel { new_key, new_model } => {
                new_model_opt = Some(new_model);
                new_key
            }
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
                let err =
                    GatewayError::InternalError("pre_auth hook failure (deny mode)".into());
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

        // Drop inner borrow before returning — `state.inner` is ArcSwap,
        // holding the guard across the rest of the request would block
        // hot-reload. (matches the original extractor's pattern.)
        drop(inner);

        Ok(Self {
            identity,
            new_model: new_model_opt,
        })
    }
}

/// Body size limit applied by [`buffer_request_body`] middleware when
/// buffering the request body for the pre_auth hook. axum's
/// `DefaultBodyLimit` defaults to 2 MB; we use the same cap so behavior
/// matches the pre-existing limit on `Json<T>`. If a larger body is sent,
/// `axum::body::to_bytes` surfaces as a 413 to the client.
pub const BODY_BUF_LIMIT: usize = 2 * 1024 * 1024;

/// Middleware that buffers the request body once into
/// `Request::extensions()` as [`CachedBodyBytes`] so that
/// `RequiredAuth` (a `FromRequestParts` — cannot consume the body) can
/// still probe the request's `model` field for the pre_auth hook, and
/// `CachedJson<T>` (a `FromRequest`) can re-deserialize the body without
/// a second buffer pass.
///
/// The body is replaced with an empty `Body::default()` after buffering
/// — downstream extractors that try `Bytes::from_request` would now get
/// empty bytes, which is why LLM handler entry points use `CachedJson<T>`
/// (which reads from extensions) instead of `Json<T>` (which reads from
/// the body stream). Non-LLM handlers (e.g. `admin_upsert_plan`) using
/// `Json<T>` must NOT have this middleware applied — they would get an
/// empty body.
///
/// Apply only to routes whose handlers use `RequiredAuth` AND want the
/// pre_auth hook to see the `model` field — currently the LLM endpoints
/// (`/v1/chat/completions`, `/v1/messages`, `/v1/completions` and their
/// un-prefixed aliases). Admin / GET / DELETE routes do not need this
/// middleware.
pub async fn buffer_request_body(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Buffer the body now. On failure we let the request through with
    // empty cached bytes — `CachedJson<T>` will then surface the real
    // error (413 / BadRequest) on the slow path. The hook just doesn't
    // see a model on a broken-body request.
    let (mut parts, body) = req.into_parts();
    let body_bytes = axum::body::to_bytes(body, BODY_BUF_LIMIT)
        .await
        .unwrap_or_default();

    // Park the bytes for RequiredAuth (FromRequestParts probe) +
    // CachedJson<T> (FromRequest). `parts.extensions` IS the new
    // request's extensions after `from_parts`, so the insertion
    // survives the reassembly below.
    parts.extensions.insert(CachedBodyBytes(body_bytes));

    // Reassemble with an empty body — extractors that try
    // `Bytes::from_request` will get empty bytes (intentional; they
    // should use `CachedJson<T>` instead, which reads from extensions).
    let req = axum::http::Request::from_parts(parts, axum::body::Body::default());
    next.run(req).await
}

/// Drop-in replacement for `axum::Json<T>` at LLM handler entry points
/// (`chat_completions`, `completions`, `messages`). When the
/// `buffer_request_body` middleware ran on the route (parking body bytes
/// in `Request::extensions()` as [`CachedBodyBytes`]), this extractor
/// reuses them — no second buffer pass. When the middleware didn't run
/// (no hook configured on this route, or handler is shared with a path
/// that bypasses the middleware), it falls back to the standard
/// `Bytes::from_request` + `Json::from_bytes` path, matching `axum::Json`
/// behavior exactly.
///
/// Content-type check mirrors `axum::Json`: requests without a JSON
/// content-type are rejected with `MissingJsonContentType`, so this is a
/// strict replacement (no behavioral drift on non-JSON bodies).
///
/// This is the single `FromRequest` extractor in the LLM handler
/// signatures — axum 0.8's `Handler` trait bound requires that there be
/// at most one body-consuming extractor per handler, and it must be last.
/// `RequiredAuth` (`FromRequestParts`) doesn't count against that budget.
pub struct CachedJson<T>(pub T);

impl<T, S> FromRequest<S> for CachedJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = axum::extract::rejection::JsonRejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        // Strict content-type check, same as axum::Json. Do this BEFORE
        // touching extensions so a non-JSON request fails the same way
        // regardless of caching.
        if !json_content_type(req.headers()) {
            return Err(axum::extract::rejection::JsonRejection::MissingJsonContentType(
                axum::extract::rejection::MissingJsonContentType::default(),
            ));
        }

        // Fast path: middleware parked the body for us.
        if let Some(cached) = req.extensions().get::<CachedBodyBytes>() {
            let json = axum::Json::<T>::from_bytes(&cached.0)?;
            return Ok(Self(json.0));
        }

        // Slow path: no cached bytes — buffer the body ourselves and parse.
        let bytes = <axum::body::Bytes as FromRequest<S>>::from_request(req, state)
            .await
            .map_err(axum::extract::rejection::JsonRejection::from)?;
        let json = axum::Json::<T>::from_bytes(&bytes)?;
        Ok(Self(json.0))
    }
}

/// Mirrors axum::Json's content-type check (json.rs:138-155). Kept local
/// because the upstream function is private. We match axum's behavior:
/// `application/json`, `application/json; charset=utf-8`, and any
/// `application/<x>+json` (e.g. `application/cloudevents+json`).
fn json_content_type(headers: &axum::http::HeaderMap) -> bool {
    use axum::http::header;
    let Some(content_type) = headers.get(header::CONTENT_TYPE) else {
        return false;
    };
    let Ok(content_type) = content_type.to_str() else {
        return false;
    };
    // Split at `;` for parameters (charset=...), lowercase-compare.
    let primary = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if primary == "application/json" {
        return true;
    }
    // application/<x>+json — e.g. application/cloudevents+json
    if let Some(rest) = primary.strip_prefix("application/") {
        return rest.ends_with("+json");
    }
    false
}

fn extract_api_key(parts: &axum::http::request::Parts) -> Option<String> {
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
