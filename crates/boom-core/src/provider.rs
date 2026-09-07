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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProtocol {
    OpenAiCompatible,
    Native,
}

/// Gateway context attached to a provider call after parent-request
/// authentication and quota admission have completed.
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

    /// Whether to attach the `X-BooM-Client-Type` header to outgoing
    /// requests routed to this deployment. Driven by the per-deployment
    /// `client_type_header: bool` config flag; default false.
    fn client_type_header(&self) -> bool {
        false
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
