use crate::hooks::PreAuthOutcome;
use crate::request_log::log_auth_error;
use crate::routes::{GatewayErrorReply, extract_client_ip, extract_user_tags};
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::response::IntoResponse;
use boom_core::types::AuthIdentity;
use boom_core::GatewayError;
use http_body_util::LengthLimitError;
use std::error::Error as _;
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

    /// Build from an already-authenticated identity — used by the WebSocket
    /// transport, which authenticates once at the upgrade handshake and
    /// reuses the identity for every frame's request.
    pub fn from_identity(identity: AuthIdentity) -> Self {
        Self { identity, new_model: None }
    }

    /// Surface a per-frame `ReplaceModel` outcome to the handler. The WS
    /// transport runs the pre_auth hook per frame (outside the extractor),
    /// so unlike the HTTP path the rewrite arrives after construction.
    pub fn set_new_model(&mut self, new_model: Option<String>) {
        self.new_model = new_model;
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
        let hook_result = inner
            .hooks
            .pre_auth(&raw_key, &parts.headers, model_opt.as_deref());

        // Hook-constructed headers become ordinary request headers —
        // insert (override) any client-sent same-named value — before any
        // downstream consumer reads them. From here on they are
        // indistinguishable from client headers: user_tags attribution,
        // prompt-log capture, and the deployment client-header whitelist
        // for upstream forwarding all apply unchanged.
        apply_hook_headers(&mut parts.headers, &hook_result.headers);

        // Per-user attribution tag (configured headers) — extracted AFTER
        // hook header injection so hook-constructed tags are attributed on
        // every path, including auth failures (a 401 from a shared key
        // still shows which user sent it). No-op when user_tag_headers is
        // unconfigured.
        let user_tags = extract_user_tags(&state.user_tag_store.get(), &parts.headers);

        let mut new_model_opt: Option<String> = None;
        let effective_key = match hook_result.outcome {
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
                    user_tags.clone(),
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
                    user_tags.clone(),
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
                    user_tags.clone(),
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

/// Ingress body cap for the LLM routes. 10 MiB ≈ 2.4M tokens of
/// conversation (measured ~4.4 bytes/token for a mixed code/Chinese
/// agent workload, GLM tokenizer + chat template) — ~2.4× the 1M-context
/// models' full-history replay, absorbing image/base64 attachment
/// overhead and leaving room for larger future context windows. This is
/// an abuse ceiling, NOT a session cliff: a legitimate 1M-token session
/// (~4.5 MB) passes and its natural end is still adjudicated by the
/// model itself. A body above the cap could not fit the served models'
/// context anyway, so the rejection reuses the backend's own error
/// semantics (`ContextLengthExceeded` — 400, `context_length_exceeded`):
/// agent clients route it into their existing context-length handling
/// (compaction/give-up) instead of a generic-retry loop.
pub const MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Middleware that buffers the request body once into
/// `Request::extensions()` as [`CachedBodyBytes`] so that
/// `RequiredAuth` (a `FromRequestParts` — cannot consume the body) can
/// still probe the request's `model` field for the pre_auth hook, and
/// `CachedJson<T>` (a `FromRequest`) can re-deserialize the body without
/// a second buffer pass.
///
/// Size policy: bodies above [`MAX_REQUEST_BODY_BYTES`] are rejected at
/// ingress — BEFORE auth, without buffering when Content-Length declares
/// the oversize — reusing `GatewayError::ContextLengthExceeded` so the
/// client sees exactly the error the upstream model would have
/// produced. History: the original 2 MiB cap (axum `DefaultBodyLimit`
/// parity, inherited, never sized for LLM traffic) was the root cause of
/// the 2026-11 stepwise incident — it swallowed oversize bodies via
/// `to_bytes(..).unwrap_or_default()` and answered a fabricated
/// "Failed to parse the request body as JSON: EOF while parsing a value
/// at line 1 column 0" 400, killing every agent session whose
/// full-history replay crossed ~470K tokens; a 10 MiB ceiling sized from
/// measured bytes/token keeps it above the 1M-token workload.
///
/// Error contract: a body that fails to READ is never cached as empty
/// bytes either — the middleware rejects the request with the real
/// cause (`RequestBodyReadFailed`, 400) and logs a warn with
/// path/client-IP/limit. CachedJson only ever sees a body that was
/// actually read to the end.
///
/// The body is replaced with an empty `Body::default()` after
/// buffering — downstream extractors that try `Bytes::from_request`
/// would now get empty bytes, which is why LLM handler entry points
/// use `CachedJson<T>` (which reads from extensions) instead of
/// `Json<T>` (which reads from the body stream). Non-LLM handlers
/// (e.g. `admin_upsert_plan`) using `Json<T>` must NOT have this
/// middleware applied — they would get an empty body.
///
/// Apply only to routes whose handlers use `RequiredAuth` AND want the
/// pre_auth hook to see the `model` field — currently the LLM endpoints
/// (`/v1/chat/completions`, `/v1/messages`, `/v1/completions`,
/// `/v1/responses` and their un-prefixed aliases). Admin / GET /
/// DELETE routes do not need this middleware.
pub async fn buffer_request_body(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let (mut parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let client_ip = extract_client_ip(&parts.headers, None);

    // Fast reject on the declared Content-Length — before buffering,
    // before auth. Requests without one (chunked) fall through to the
    // capped buffered read below.
    let declared_len: Option<u64> = parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    if let Some(cl) = declared_len {
        if cl > MAX_REQUEST_BODY_BYTES as u64 {
            tracing::warn!(
                path = %path,
                client_ip = ?client_ip,
                declared_len = cl,
                limit = MAX_REQUEST_BODY_BYTES,
                "request body over ingress cap (Content-Length precheck) — rejecting as context_length_exceeded"
            );
            return GatewayErrorReply(
                GatewayError::ContextLengthExceeded(format!(
                    "request body of {cl} bytes (Content-Length) exceeds the gateway request limit \
                     of {MAX_REQUEST_BODY_BYTES} bytes; trim the conversation history and retry"
                )),
                false,
            )
            .into_response();
        }
    }

    // Buffer the body once, capped. On failure the request is rejected
    // with the real cause — never continued with empty cached bytes (see
    // the error contract). The pre_auth hook and CachedJson only ever
    // see a body that was actually read to the end.
    let body_bytes = match axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(err) => {
            let over_limit = err
                .source()
                .map(|s| s.is::<LengthLimitError>())
                .unwrap_or(false);
            tracing::warn!(
                path = %path,
                client_ip = ?client_ip,
                limit = MAX_REQUEST_BODY_BYTES,
                over_limit,
                error = ?err,
                "request body read failed at ingress — rejecting"
            );
            let error = if over_limit {
                GatewayError::ContextLengthExceeded(format!(
                    "request body exceeds the gateway request limit of {MAX_REQUEST_BODY_BYTES} bytes; \
                     trim the conversation history and retry"
                ))
            } else {
                GatewayError::RequestBodyReadFailed(err.to_string())
            };
            return GatewayErrorReply(error, false).into_response();
        }
    };

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

/// Apply hook-constructed headers (see `PreAuthResult::headers`) to the
/// request HeaderMap. Insert semantics: a hook header replaces any
/// client-sent value of the same (case-insensitive) name — the hook is
/// admin-installed trusted code, its value is authoritative.
///
/// Entries whose name or value fails HTTP validation (bad name syntax,
/// control characters) are skipped with a warn instead of failing the
/// request — a broken hook must not 500 every request. Values are
/// validated with `HeaderValue::from_bytes` (not `from_str`) so
/// non-ASCII UTF-8 passes as obs-text, matching `extract_user_tags`'s
/// lossy-decode handling of non-ASCII client values.
pub(crate) fn apply_hook_headers(
    headers: &mut axum::http::HeaderMap,
    hook_headers: &std::collections::HashMap<String, String>,
) {
    for (name, value) in hook_headers {
        let name_res = axum::http::HeaderName::from_bytes(name.as_bytes());
        let value_res = axum::http::HeaderValue::from_bytes(value.as_bytes());
        match (name_res, value_res) {
            (Ok(n), Ok(v)) => {
                headers.insert(n, v);
            }
            (name_res, value_res) => {
                tracing::warn!(
                    header = %name,
                    name_err = ?name_res.err(),
                    value_err = ?value_res.err(),
                    "pre_auth hook returned an invalid header, skipping"
                );
            }
        }
    }
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
    use super::{apply_hook_headers, extract_api_key};
    use axum::http::header::HeaderValue;
    use axum::http::Request;
    use std::collections::HashMap;

    fn hook_headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn apply_hook_headers_inserts_new_header() {
        let mut headers = axum::http::HeaderMap::new();
        apply_hook_headers(&mut headers, &hook_headers(&[("X-User-Tag", "alice")]));
        assert_eq!(headers.get("x-user-tag").unwrap(), "alice");
    }

    #[test]
    fn apply_hook_headers_overrides_client_header_case_insensitively() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::HeaderName::from_static("x-user-tag"),
            HeaderValue::from_static("client-claims"),
        );
        // Hook uses different casing — must still replace the client value.
        apply_hook_headers(&mut headers, &hook_headers(&[("X-User-Tag", "hook-truth")]));
        assert_eq!(headers.get("x-user-tag").unwrap(), "hook-truth");
        assert_eq!(headers.len(), 1, "no duplicate entry should remain");
    }

    #[test]
    fn apply_hook_headers_skips_invalid_name() {
        let mut headers = axum::http::HeaderMap::new();
        apply_hook_headers(&mut headers, &hook_headers(&[("Bad Header Name", "v")]));
        assert!(headers.is_empty());
    }

    #[test]
    fn apply_hook_headers_skips_control_chars_in_value() {
        let mut headers = axum::http::HeaderMap::new();
        apply_hook_headers(&mut headers, &hook_headers(&[("X-User-Tag", "bad\u{0000}value")]));
        assert!(headers.is_empty());
    }

    #[test]
    fn apply_hook_headers_allows_non_ascii_value() {
        // CJK values must survive — from_bytes treats them as obs-text,
        // consistent with extract_user_tags's from_utf8_lossy decode.
        let mut headers = axum::http::HeaderMap::new();
        apply_hook_headers(&mut headers, &hook_headers(&[("X-User-Tag", "张三")]));
        assert_eq!(headers.get("x-user-tag").unwrap().as_bytes(), "张三".as_bytes());
    }


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

/// Ingress behavior of [`buffer_request_body`]: bodies up to
/// [`MAX_REQUEST_BODY_BYTES`] (10 MiB) pass; larger bodies are rejected
/// with the BACKEND-PARITY error — `GatewayError::ContextLengthExceeded`
/// (400, `context_length_exceeded`), the same code a client gets when the
/// upstream model rejects an over-context request — so agent harnesses
/// handle ingress caps via their existing context-length path. Read
/// failures keep their honest 400 + warn. Never a fabricated
/// "unparseable JSON" 400 from cached-empty bytes (the 2026-11 stepwise
/// RCA).
#[cfg(test)]
mod body_limit_tests {
    use super::{CachedJson, MAX_REQUEST_BODY_BYTES, RequiredAuth, buffer_request_body};
    use crate::state::AppState;
    use axum::body::{Body, Bytes};
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use tower::ServiceExt;

    async fn state_from_yaml(yaml: &str) -> AppState {
        let config: boom_config::Config = serde_yaml::from_str(yaml).expect("parse config yaml");
        AppState::from_config(config, None)
            .await
            .expect("build AppState")
    }

    /// Production-shaped pipeline: middleware → RequiredAuth → CachedJson.
    fn llm_app(state: &AppState) -> Router {
        Router::new()
            .route(
                "/v1/chat/completions",
                post(
                    |_auth: RequiredAuth,
                     CachedJson(_body): CachedJson<serde_json::Value>|
                     async { "handler-reached" },
                ),
            )
            .with_state(state.clone())
            .layer(axum::middleware::from_fn(buffer_request_body))
    }

    /// Valid JSON body of exactly `total` bytes ("x"-padded user content).
    fn json_body(total: usize) -> Vec<u8> {
        let prefix = r#"{"model":"m","messages":[{"role":"user","content":""#;
        let suffix = r#""}]}"#;
        let pad = total - prefix.len() - suffix.len();
        let s = format!("{}{}{}", prefix, "x".repeat(pad), suffix);
        assert_eq!(s.len(), total);
        s.into_bytes()
    }

    async fn body_text(res: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
            .await
            .expect("read response body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn post_json(body: impl Into<Body>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer sk-test-m")
            .body(body.into())
            .unwrap()
    }

    /// THE regression test for the 2026-11 stepwise RCA: a multi-MiB
    /// full-history-replay body must flow through to auth and parsing —
    /// the old 2 MiB cap used to swallow it and answer a fabricated
    /// "EOF while parsing a value at line 1 column 0" 400. The 10 MiB
    /// ceiling sits ~2.4× above the 1M-token workload (~4.5 MB measured).
    #[tokio::test]
    async fn multi_mib_body_reaches_handler_up_to_cap() {
        let state = state_from_yaml("general_settings:\n  master_key: sk-test-m\n").await;
        let app = llm_app(&state);
        for size in [3 * 1024 * 1024, MAX_REQUEST_BODY_BYTES] {
            let res = app
                .clone()
                .oneshot(post_json(json_body(size)))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "body of {size} bytes");
            assert_eq!(body_text(res).await, "handler-reached");
        }
    }

    /// Over-cap bodies get the BACKEND-PARITY error: 400 with
    /// `type: context_length_exceeded` — identical to what the upstream
    /// model returns for an over-context request — NOT a 413 (agent
    /// harnesses don't route 413 into their context-length handling)
    /// and NOT a fabricated JSON parse error.
    #[tokio::test]
    async fn over_cap_body_gets_backend_style_context_error() {
        let state = state_from_yaml("general_settings:\n  master_key: sk-test-m\n").await;
        let app = llm_app(&state);
        // No content-length (tower bypasses framing) → capped buffered read.
        let res = app
            .clone()
            .oneshot(post_json(json_body(MAX_REQUEST_BODY_BYTES + 1)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let text = body_text(res).await;
        assert!(text.contains("context_length_exceeded"), "got: {text}");
        assert!(
            text.contains("exceeds the gateway request limit of 10485760 bytes"),
            "message names the cap, got: {text}"
        );
        assert!(!text.contains("EOF while parsing"), "got: {text}");
    }

    /// Declared oversize Content-Length is rejected without buffering and
    /// BEFORE auth (no Authorization header; an authenticated path would
    /// have answered 401) — and with the same backend-parity error.
    #[tokio::test]
    async fn content_length_precheck_rejects_before_auth_with_backend_error() {
        let state = state_from_yaml("general_settings:\n  master_key: sk-test-m\n").await;
        let app = llm_app(&state);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("content-length", (MAX_REQUEST_BODY_BYTES + 1).to_string())
            .body(Body::from(json_body(MAX_REQUEST_BODY_BYTES + 1)))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let text = body_text(res).await;
        assert!(text.contains("context_length_exceeded"), "got: {text}");
        assert!(
            text.contains("Content-Length") && text.contains("10485760"),
            "precheck message names declared size and cap, got: {text}"
        );
    }

    #[tokio::test]
    async fn body_read_failure_rejected_with_honest_400() {
        // A body stream that errors mid-read must surface as a 400 naming
        // the read failure — never as cached-empty bytes that
        // CachedJson would misreport as unparseable JSON.
        let state = state_from_yaml("general_settings:\n  master_key: sk-test-m\n").await;
        let app = llm_app(&state);
        let err_stream = futures::stream::once(async {
            Err::<Bytes, _>(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "client vanished mid-body",
            ))
        });
        let res = app
            .oneshot(post_json(Body::from_stream(err_stream)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let text = body_text(res).await;
        assert!(
            text.contains("Failed to read request body"),
            "got: {text}"
        );
        assert!(
            !text.contains("EOF while parsing"),
            "must not fabricate a JSON parse error: {text}"
        );
    }

    #[tokio::test]
    async fn small_garbage_json_keeps_parse_400_and_auth_first() {
        // Control: the middleware itself must not reject small bodies —
        // auth and CachedJson still own those. Garbage JSON gets the real
        // serde syntax message; missing auth gets 401 first.
        let state = state_from_yaml("general_settings:\n  master_key: sk-test-m\n").await;
        let app = llm_app(&state);
        let res = app.clone().oneshot(post_json("garbage!!!")).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let text = body_text(res).await;
        assert!(
            text.contains("expected value"),
            "serde's syntax message, got: {text}"
        );

        let no_auth = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let res = app.oneshot(no_auth).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
