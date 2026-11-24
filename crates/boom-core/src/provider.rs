use crate::types::{
    AuthIdentity, ChatCompletionRequest, ChatCompletionResponse, ChatStream, PromptTokensDetails,
    Usage,
};
use crate::GatewayError;
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Read the explicit provider prefix or detect it from a known model name.
pub fn resolve_model_provider(model: &str) -> Option<(&str, &str)> {
    if let Some(parts) = model.split_once('/') {
        return Some(parts);
    }

    let lower = model.to_lowercase();
    let provider = if lower.starts_with("gpt-")
        || lower.starts_with("o1-")
        || lower.starts_with("o3-")
        || lower.starts_with("o4-")
        || lower.starts_with("text-")
        || lower.starts_with("dall-e-")
        || lower.starts_with("chatgpt-")
        || lower.starts_with("ft:gpt-")
    {
        "openai"
    } else if lower.starts_with("claude-") {
        "anthropic"
    } else if lower.starts_with("gemini-") || lower.starts_with("gemma-") {
        "gemini"
    } else if lower.starts_with("anthropic.")
        || lower.starts_with("amazon.")
        || lower.starts_with("meta.")
        || lower.starts_with("mistral.")
    {
        "bedrock"
    } else if lower.starts_with("deepseek") {
        "deepseek"
    } else if lower.starts_with("llama") || lower.starts_with("qwen") || lower.starts_with("yi-") {
        // Common open-weights models typically served via vLLM/Ollama.
        // Default to OpenAI-compatible since vLLM uses OpenAI format.
        "openai"
    } else {
        return None;
    };
    Some((provider, model))
}

/// Actual provider cost for one logical request.
///
/// Ordinary providers leave this unset and the gateway computes cost from the
/// selected model's rate. Composite providers use it to return the sum of
/// successful child calls without exposing billing metadata in the API body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderCost {
    pub regular_input: Decimal,
    pub cached_input: Decimal,
    pub output: Decimal,
}

impl ProviderCost {
    pub fn total(&self) -> Decimal {
        self.regular_input + self.cached_input + self.output
    }

    pub fn add(&mut self, other: &Self) {
        self.regular_input += other.regular_input;
        self.cached_input += other.cached_input;
        self.output += other.output;
    }
}

/// Shared return channel for provider-specific usage and actual cost.
#[derive(Debug, Clone, Default)]
pub struct ProviderBilling {
    actual_cost: Arc<Mutex<Option<ProviderCost>>>,
    actual_usage: Arc<Mutex<Option<Usage>>>,
}

impl ProviderBilling {
    pub fn add_actual_cost(&self, cost: &ProviderCost) {
        if let Ok(mut actual_cost) = self.actual_cost.lock() {
            actual_cost
                .get_or_insert_with(ProviderCost::default)
                .add(cost);
        }
    }

    pub fn actual_cost(&self) -> Option<ProviderCost> {
        self.actual_cost
            .lock()
            .ok()
            .and_then(|actual_cost| actual_cost.clone())
    }

    pub fn add_actual_usage(&self, usage: &Usage) {
        if let Ok(mut actual_usage) = self.actual_usage.lock() {
            let target = actual_usage.get_or_insert_with(Usage::default);
            target.prompt_tokens = target.prompt_tokens.saturating_add(usage.prompt_tokens);
            target.completion_tokens = target
                .completion_tokens
                .saturating_add(usage.completion_tokens);
            target.total_tokens = target.total_tokens.saturating_add(usage.total_tokens);
            add_optional_usage(
                &mut target.cache_creation_input_tokens,
                usage.cache_creation_input_tokens,
            );
            add_optional_usage(
                &mut target.cache_read_input_tokens,
                usage.cache_read_input_tokens,
            );
            if let Some(details) = &usage.prompt_tokens_details {
                let target_details = target
                    .prompt_tokens_details
                    .get_or_insert_with(PromptTokensDetails::default);
                add_optional_usage(&mut target_details.cached_tokens, details.cached_tokens);
            }
        }
    }

    pub fn actual_usage(&self) -> Option<Usage> {
        self.actual_usage
            .lock()
            .ok()
            .and_then(|actual_usage| actual_usage.clone())
    }
}

fn add_optional_usage(target: &mut Option<u32>, value: Option<u32>) {
    if let Some(value) = value {
        *target = Some(target.unwrap_or(0).saturating_add(value));
    }
}

/// Optional per-request trace owned by a provider implementation.
///
/// The gateway only snapshots the trace into Prompt Log. Its schema and
/// mutation logic stay inside the provider that created it.
pub trait ProviderPromptTrace: Send + Sync {
    fn finalize(&self) {}
    fn snapshot(&self) -> Option<serde_json::Value>;
    fn as_any(&self) -> &dyn Any;
}

pub type SharedProviderPromptTrace = Arc<dyn ProviderPromptTrace>;

/// Side channel capturing the exact bodies exchanged with the upstream
/// provider. Created by the route layer when `prompt_log.capture_raw_upstream`
/// or raw trace capture is enabled; providers write at their send/receive
/// sites; readers snapshot it for the trace and drain the legacy buffers when
/// the prompt log entry is composed.
///
/// Semantics:
/// - one `RawAttempt` per upstream call (retries and fallbacks append);
/// - `request_body`: the JSON string exactly as serialized for that call
///   (byte-identical to what reqwest puts on the wire — same Value, same
///   serializer), empty when not recorded;
/// - `response_frames`: one entry per complete SSE frame for streaming
///   (frame text as received, `event:`/`data:` prefixes and comments kept,
///   frames that fail JSON parse included), or the whole body as a single
///   entry for non-streaming responses and upstream HTTP error bodies.
///   Callers join frames with `\n\n` to reconstruct the raw stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawAttempt {
    /// `Provider::name()` at the record site.
    pub provider: String,
    pub request_body: String,
    pub response_frames: Vec<String>,
}

#[derive(Debug, Default)]
pub struct RawCaptureChannel {
    attempts: Mutex<Vec<RawAttempt>>,
}

impl RawCaptureChannel {
    /// Start a new attempt holding `body` as its upstream request body.
    /// Retries and fallbacks append rather than overwrite the first body.
    pub fn record_request_body(&self, provider: &str, body: &str) {
        if let Ok(mut attempts) = self.attempts.lock() {
            attempts.push(RawAttempt {
                provider: provider.to_string(),
                request_body: body.to_string(),
                response_frames: Vec::new(),
            });
        }
    }

    /// Append a response frame to the attempt currently in flight, creating
    /// a default attempt when none was recorded yet.
    pub fn push_response_frame(&self, frame: String) {
        if let Ok(mut attempts) = self.attempts.lock() {
            if attempts.is_empty() {
                attempts.push(RawAttempt::default());
            }
            if let Some(attempt) = attempts.last_mut() {
                attempt.response_frames.push(frame);
            }
        }
    }

    /// Legacy single-slot view: the first non-empty request body. Every
    /// attempt is cleared, so a second call yields `None`.
    pub fn take_request_body(&self) -> Option<String> {
        let mut attempts = match self.attempts.lock() {
            Ok(attempts) => attempts,
            Err(_) => return None,
        };
        let mut first = None;
        for attempt in attempts.iter_mut() {
            let body = std::mem::take(&mut attempt.request_body);
            if first.is_none() && !body.is_empty() {
                first = Some(body);
            }
        }
        first
    }

    /// Drain every attempt's frames into a single raw string. Streaming
    /// frames are joined with `\n\n` (the SSE frame separator); a lone
    /// non-streaming body is returned verbatim.
    pub fn take_response_body(&self) -> Option<String> {
        let mut attempts = match self.attempts.lock() {
            Ok(attempts) => attempts,
            Err(_) => return None,
        };
        let mut frames = Vec::new();
        for attempt in attempts.iter_mut() {
            frames.append(&mut attempt.response_frames);
        }
        if frames.is_empty() {
            None
        } else {
            Some(frames.join("\n\n"))
        }
    }

    /// Non-draining clone of every attempt, in upstream call order. Trace
    /// readers call this before the legacy takes drain the buffers.
    pub fn snapshot_attempts(&self) -> Vec<RawAttempt> {
        match self.attempts.lock() {
            Ok(attempts) => attempts.clone(),
            Err(_) => Vec::new(),
        }
    }
}

pub type SharedRawCapture = Arc<RawCaptureChannel>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProtocol {
    OpenAiCompatible,
    Native,
}

/// Gateway context attached to a provider call after parent-request
/// authentication and quota admission have completed.
#[derive(Clone)]
pub struct ProviderCallContext {
    pub key_hash: String,
    pub key_alias: Option<String>,
    pub is_vip: bool,
    pub api_path: String,
    pub billing: ProviderBilling,
    pub prompt_trace: Option<SharedProviderPromptTrace>,
}

/// Provider trait — each LLM provider implements this.
///
/// The gateway routes a standardized ChatCompletionRequest to the chosen provider,
/// which handles format transformation and upstream communication internally.
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// Non-streaming chat completion.
    async fn chat(&self, request: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError>;

    /// Streaming chat completion. Returns an SSE byte stream from the upstream provider,
    /// already transformed into OpenAI-compatible chunks.
    async fn chat_stream(&self, request: ChatCompletionRequest) -> Result<ChatStream, GatewayError>;

    /// Context-aware entry point used by the gateway route. Ordinary providers
    /// use the direct implementation; virtual providers can override it to
    /// perform nested routing while retaining key-affinity and priority context.
    async fn chat_with_context(
        &self,
        request: ChatCompletionRequest,
        _context: ProviderCallContext,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        self.chat(request).await
    }

    /// Streaming counterpart of [`Provider::chat_with_context`].
    async fn chat_stream_with_context(
        &self,
        request: ChatCompletionRequest,
        _context: ProviderCallContext,
    ) -> Result<ChatStream, GatewayError> {
        self.chat_stream(request).await
    }

    /// Create provider-owned Prompt Log trace state for one parent request.
    fn create_prompt_trace(&self) -> Option<SharedProviderPromptTrace> {
        None
    }

    /// Provider identifier (e.g. "openai", "anthropic").
    fn name(&self) -> &str;

    /// Raw deployment provider type used for `gen_ai.provider.name`.
    fn provider_type(&self) -> &str {
        self.name()
    }

    /// Upstream API protocol implemented by this provider.
    fn protocol(&self) -> ProviderProtocol {
        ProviderProtocol::Native
    }

    /// List models supported by this provider deployment.
    fn models(&self) -> &[String];

    /// Optional deployment ID (from model_info.id), used to distinguish
    /// same-name deployments in logs and scheduling.
    fn deployment_id(&self) -> Option<&str> {
        None
    }

    /// Worker ID used to match KV-cache index entries.
    ///
    /// Derived from the upstream `api_base` host (pure IP/hostname, with
    /// scheme/port/path stripped) so it lines up with the `worker_id` vLLM
    /// publishes in its ZMQ topic (`kv@{worker_id}@{model}`). The KV-aware
    /// scheduler queries the trie by this id — it must NOT be sourced from
    /// `model_info.id`, which is an opaque deployment label, not a host.
    ///
    /// Returns None for providers that have no upstream host suitable for
    /// KV-cache matching (e.g. managed endpoints like Bedrock/Gemini).
    fn kv_worker_id(&self) -> Option<&str> {
        None
    }

    /// Context capability declaration for rollback tiering (tokens).
    /// `None` = not participating (default — fusion virtual providers,
    /// non-OpenAI-compatible providers, test fakes). Stored per deployment
    /// from `boom_model_deployment.max_context_tokens`.
    fn max_context_tokens(&self) -> Option<u64> {
        None
    }

    /// Whether to attach the `X-BooM-Client-Type` header to outgoing
    /// requests routed to this deployment. Driven by the per-deployment
    /// `client_type_header: bool` config flag; default false.
    fn client_type_header(&self) -> bool {
        false
    }

    /// Deployment-level custom headers (`litellm_params.headers`), already
    /// sanitized at provider creation: reserved config keys, spoof-blocked
    /// names ([`crate::is_hard_blocked_header`]) and transport-critical
    /// names are dropped. The route layer composes them into
    /// `ChatCompletionRequest::gateway_headers` between the client
    /// whitelist pass and the gateway-injected pass — merge priority:
    /// gateway-injected > deployment custom > client-whitelisted.
    fn custom_headers(&self) -> &[(String, String)] {
        &[]
    }
}

/// Rate limiter trait was removed during the limiter normalization refactor
/// (boom-quota deletion). The three-phase contract (peek_only → commit_counts
/// → settle_usage) lives directly on `boom_limiter::SlidingWindowLimiter` —
/// it cannot be expressed as a single `check_and_record` method because the
/// caller must drive commit/settle at distinct moments (post-accept and
/// post-stream-done).

/// Narrow trait for looking up key aliases by token hashes.
/// Separated from Authenticator so that consumers (e.g. Dashboard) don't
/// depend on the full authentication interface.
#[async_trait]
pub trait KeyAliasLookup: Send + Sync + 'static {
    /// Batch-lookup key aliases by token hashes.
    /// Returns a map of token_hash → key_alias (None if no alias set).
    async fn lookup_key_aliases(&self, _key_hashes: &[&str]) -> HashMap<String, Option<String>> {
        HashMap::new()
    }
}

/// Authenticator trait — validates API keys and returns identity info.
/// Extends KeyAliasLookup so that any Authenticator can also resolve key aliases.
#[async_trait]
pub trait Authenticator: KeyAliasLookup {
    /// Authenticate a raw API key string (e.g. from Authorization header).
    /// Returns the resolved identity or an error.
    async fn authenticate(&self, api_key: &str) -> Result<AuthIdentity, GatewayError>;

    /// Check if the identity can access the given model.
    fn check_model_access(&self, identity: &AuthIdentity, model: &str) -> Result<(), GatewayError>;

    /// Invalidate the cached token row for a key hash, so the next request
    /// re-reads the row from the database. Called after dashboard writes to
    /// the token table (update/block/unblock/delete). Default no-op — only
    /// caching implementations override.
    async fn invalidate_key(&self, _key_hash: &str) {}

    /// Invalidate every cached token row. Default no-op.
    async fn invalidate_all(&self) {}
}

/// Deployment — a single model deployment configuration.
/// Multiple deployments can share the same model_name (load balanced).
#[derive(Debug, Clone)]
pub struct Deployment {
    /// Public-facing model name (what the client requests).
    pub model_name: String,
    /// The provider instance that handles this deployment.
    pub provider: String,
    /// The actual model ID at the provider (may differ from model_name).
    pub model_id: String,
    /// RPM limit for this specific deployment.
    pub rpm_limit: Option<u64>,
    /// TPM limit for this specific deployment.
    pub tpm_limit: Option<u64>,
    /// Weight for weighted routing strategies.
    pub weight: u32,
    /// Priority for fallback routing (lower = higher priority).
    pub priority: u32,
}

/// Provider of per-deployment queue depth for scheduling decisions.
/// Implemented by flow control to expose total load (in-flight + queued).
pub trait DeploymentQueueInfo: Send + Sync + 'static {
    /// Total load for a deployment: in-flight requests + queued requests.
    /// Returns 0 if the deployment has no flow control configured.
    fn total_load(&self, deployment_id: &str) -> u64;

    /// Maximum concurrent capacity (max_inflight) for a deployment.
    /// Returns 0 if the deployment has no flow control configured (unlimited).
    fn max_capacity(&self, deployment_id: &str) -> u32;
}

#[cfg(test)]
mod raw_capture_tests {
    use super::*;

    #[test]
    fn request_body_first_writer_wins() {
        let cap = RawCaptureChannel::default();
        cap.record_request_body("openai", "first");
        cap.record_request_body("openai", "second");
        assert_eq!(cap.take_request_body().as_deref(), Some("first"));
        assert_eq!(cap.take_request_body(), None, "take drains the slot");
    }

    #[test]
    fn streaming_frames_join_with_blank_line_separator() {
        let cap = RawCaptureChannel::default();
        cap.push_response_frame("data: {\"a\":1}".to_string());
        cap.push_response_frame("data: [DONE]".to_string());
        assert_eq!(
            cap.take_response_body().as_deref(),
            Some("data: {\"a\":1}\n\ndata: [DONE]")
        );
        assert_eq!(cap.take_response_body(), None, "take drains the frames");
    }

    #[test]
    fn single_non_streaming_body_returned_verbatim() {
        let cap = RawCaptureChannel::default();
        cap.push_response_frame("{\"id\":\"x\"}".to_string());
        assert_eq!(cap.take_response_body().as_deref(), Some("{\"id\":\"x\"}"));
    }

    #[test]
    fn empty_channel_takes_return_none() {
        let cap = RawCaptureChannel::default();
        assert_eq!(cap.take_request_body(), None);
        assert_eq!(cap.take_response_body(), None);
    }

    #[test]
    fn second_record_starts_new_attempt() {
        let cap = RawCaptureChannel::default();
        cap.record_request_body("openai", "first");
        cap.record_request_body("openai", "second");
        let attempts = cap.snapshot_attempts();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[1].request_body, "second");
        assert_eq!(cap.take_request_body().as_deref(), Some("first"));
    }

    #[test]
    fn frames_route_to_current_attempt() {
        let cap = RawCaptureChannel::default();
        cap.record_request_body("openai", "req-a");
        cap.push_response_frame("err-body-a".into());   // attempt 1 fails
        cap.record_request_body("openai", "req-b");
        cap.push_response_frame("data: {\"a\":1}".into());
        let attempts = cap.snapshot_attempts();
        assert_eq!(attempts[0].response_frames, vec!["err-body-a".to_string()]);
        assert_eq!(attempts[1].response_frames, vec!["data: {\"a\":1}".to_string()]);
        assert_eq!(cap.take_response_body().as_deref(), Some("err-body-a\n\ndata: {\"a\":1}"));
    }

    #[test]
    fn snapshot_does_not_drain() {
        let cap = RawCaptureChannel::default();
        cap.record_request_body("openai", "x");
        assert_eq!(cap.snapshot_attempts().len(), 1);
        assert_eq!(cap.snapshot_attempts().len(), 1);
    }
}


#[cfg(test)]
mod provider_type_tests {
    use super::*;

    struct FakeProvider;

    #[async_trait]
    impl Provider for FakeProvider {
        async fn chat(&self, _request: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
            unimplemented!()
        }

        async fn chat_stream(&self, _request: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
            unimplemented!()
        }

        fn name(&self) -> &str {
            "fake"
        }

        fn models(&self) -> &[String] {
            &[]
        }
    }

    #[test]
    fn provider_type_defaults_to_name() {
        assert_eq!(FakeProvider.provider_type(), "fake");
    }
}