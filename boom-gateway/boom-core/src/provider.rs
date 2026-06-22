use crate::types::{
    AuthIdentity, ChatCompletionRequest, ChatCompletionResponse, ChatStream, RateLimitKey,
    RateLimitDecision,
};
use crate::GatewayError;
use async_trait::async_trait;
use std::collections::HashMap;

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

    /// Provider identifier (e.g. "openai", "anthropic").
    fn name(&self) -> &str;

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
}

/// Rate limiter trait — supports per-key and per-model sliding window limits.
#[async_trait]
pub trait RateLimiter: Send + Sync + 'static {
    /// Check if the request is within rate limits and record a counter.
    /// Returns a decision with remaining quota info.
    /// `weight` is the quota consumption multiplier (default 1).
    async fn check_and_record(
        &self,
        key: &RateLimitKey,
        rpm_limit: Option<u64>,
        window_limits: &[(u64, u64)], // (limit, window_secs) pairs
        weight: u64,
    ) -> Result<RateLimitDecision, GatewayError>;
}

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
