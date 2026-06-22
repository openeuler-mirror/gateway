use boom_core::GatewayError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;

/// Top-level gateway configuration, loaded from YAML.
/// Compatible with litellm's `proxy_server_config.yaml` format.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub model_list: Vec<ModelEntry>,
    #[serde(default)]
    pub general_settings: GeneralSettings,
    #[serde(default)]
    pub router_settings: RouterSettings,
    #[serde(default)]
    pub server: ServerSettings,
    #[serde(default)]
    pub rate_limit: RateLimitSettings,
    #[serde(default)]
    pub plan_settings: PlanSettings,
    #[serde(default)]
    pub deployment_health_check: DeploymentHealthCheckSettings,
    /// Prompt log configuration (transparent pass-through to boom-promptlog).
    #[serde(default)]
    pub prompt_log: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DeploymentHealthCheckSettings {
    #[serde(default)]
    pub auto_offline_enabled: bool,
    #[serde(default)]
    pub auto_recovery_enabled: bool,
    #[serde(default = "default_health_check_path")]
    pub path: String,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    #[serde(default = "default_recovery_threshold")]
    pub recovery_threshold: u32,
    #[serde(default = "default_offline_check_interval_secs")]
    pub offline_check_interval_secs: u64,
    #[serde(default = "default_recovery_check_interval_secs")]
    pub recovery_check_interval_secs: u64,
    #[serde(default)]
    pub request_failure_auto_offline_enabled: bool,
    #[serde(default = "default_request_failure_threshold")]
    pub request_failure_threshold: u32,
}

impl Default for DeploymentHealthCheckSettings {
    fn default() -> Self {
        Self {
            auto_offline_enabled: false,
            auto_recovery_enabled: false,
            path: default_health_check_path(),
            failure_threshold: default_failure_threshold(),
            recovery_threshold: default_recovery_threshold(),
            offline_check_interval_secs: default_offline_check_interval_secs(),
            recovery_check_interval_secs: default_recovery_check_interval_secs(),
            request_failure_auto_offline_enabled: false,
            request_failure_threshold: default_request_failure_threshold(),
        }
    }
}

fn default_health_check_path() -> String { "/metric".to_string() }
fn default_failure_threshold() -> u32 { 3 }
fn default_recovery_threshold() -> u32 { 2 }
fn default_offline_check_interval_secs() -> u64 { 30 }
fn default_recovery_check_interval_secs() -> u64 { 60 }
fn default_request_failure_threshold() -> u32 { 3 }

/// A single plan definition in YAML config (plan name comes from the HashMap key).
#[derive(Debug, Deserialize, Clone)]
pub struct PlanConfig {
    #[serde(default)]
    pub concurrency_limit: Option<u32>,
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    #[serde(default)]
    pub window_limits: Vec<Vec<u64>>,
    /// Optional time-based schedule overrides.
    #[serde(default)]
    pub schedule: Vec<ScheduleSlotConfig>,
}

/// A time-based schedule slot within a plan.
///
/// ```yaml
/// schedule:
///   - hours: "9:00-21:00"
///     concurrency_limit: 4
///     rpm_limit: 60
///   - hours: "21:00-9:00"
///     concurrency_limit: 8
///     rpm_limit: 120
/// ```
#[derive(Debug, Deserialize, Clone)]
pub struct ScheduleSlotConfig {
    /// Time range, e.g. "9:00-21:00" or "21:00-9:00" (cross-midnight).
    pub hours: String,
    #[serde(default)]
    pub concurrency_limit: Option<u32>,
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    #[serde(default)]
    pub window_limits: Vec<Vec<u64>>,
}

/// Top-level plan settings section.
///
/// ```yaml
/// plan_settings:
///   default_plan: "basic"
///   plans:
///     basic:
///       concurrency_limit: 4
///       rpm_limit: 60
///       window_limits: [[100, 18000]]
/// ```
#[derive(Debug, Deserialize, Clone, Default)]
pub struct PlanSettings {
    /// Name of the plan to use for keys without an explicit assignment.
    pub default_plan: Option<String>,
    /// Plan name → plan definition.
    #[serde(default)]
    pub plans: HashMap<String, PlanConfig>,
}

/// Model metadata — compatible with litellm's `model_info` field.
///
/// ```yaml
/// model_info:
///   id: node-a
///   input_cost_per_token: 0.000005
///   output_cost_per_token: 0.000015
/// ```
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ModelInfo {
    pub id: Option<String>,
    pub input_cost_per_token: Option<f64>,
    pub output_cost_per_token: Option<f64>,
    /// Quota count multiplier for this model.
    /// Each request consumes `quota_count_ratio` units instead of 1.
    /// Defaults to 1 when not set.
    pub quota_count_ratio: Option<u64>,
}

/// Per-deployment flow control configuration.
///
/// ```yaml
/// flow_control:
///   model_queue_limit: 50
///   model_context_limit: 5000000
/// ```
#[derive(Debug, Deserialize, Clone, Default)]
pub struct FlowControlEntry {
    /// Max concurrent in-flight requests. 0 or unset = no limit.
    #[serde(default)]
    pub model_queue_limit: Option<u32>,
    /// Max total input context chars across all in-flight requests. 0 or unset = no limit.
    #[serde(default)]
    pub model_context_limit: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ModelEntry {
    pub model_name: String,
    pub litellm_params: ProviderParams,
    #[serde(default)]
    pub model_info: Option<ModelInfo>,
    /// Per-deployment flow control (queue + context limits).
    #[serde(default)]
    pub flow_control: Option<FlowControlEntry>,
    /// When true, this deployment also serves as a catch-all for unmatched model names.
    /// The same deployment is registered under both its real name and "*".
    #[serde(default)]
    pub serve_not_match: bool,
    /// When false, the deployment is written to DB (visible in dashboard) but
    /// excluded from the in-memory routing table. Default: true.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Provider params — compatible with litellm's `litellm_params` format.
///
/// The `model` field follows litellm's `provider/model-id` convention:
///   - `openai/gpt-4` → OpenAI provider, model `gpt-4`
///   - `anthropic/claude-sonnet-4-20250514` → Anthropic
///   - `azure/my-deployment` → Azure OpenAI
///   - `gemini/gemini-2.0-flash` → Google Gemini
///   - `bedrock/anthropic.claude-3-sonnet` → AWS Bedrock
///   - `gpt-4` → auto-detected as OpenAI
///   - `claude-3-opus` → auto-detected as Anthropic
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderParams {
    /// Model string in litellm format: `[provider/]model-id`.
    pub model: String,
    /// API key (supports ${ENV_VAR} and os.environ/VAR syntax).
    pub api_key: Option<String>,
    /// API base URL override.
    pub api_base: Option<String>,
    /// Azure OpenAI API version.
    pub api_version: Option<String>,
    /// AWS region for Bedrock.
    pub aws_region_name: Option<String>,
    /// AWS access key ID.
    pub aws_access_key_id: Option<String>,
    /// AWS secret access key.
    pub aws_secret_access_key: Option<String>,
    /// RPM limit for this deployment.
    pub rpm: Option<u64>,
    /// TPM limit for this deployment.
    pub tpm: Option<u64>,
    /// Request timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// Custom headers to send with every request.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Temperature override.
    pub temperature: Option<f64>,
    /// Max tokens override.
    pub max_tokens: Option<u32>,
}

impl ProviderParams {
    /// Parse the `model` field to determine provider type and actual model ID.
    ///
    /// Returns `(provider_type, actual_model_id)`.
    pub fn resolve_provider_and_model(&self) -> (String, String) {
        if let Some((provider, model)) = self.model.split_once('/') {
            // Explicit prefix: `openai/gpt-4`, `anthropic/claude-3`, etc.
            (provider.to_string(), model.to_string())
        } else {
            // No prefix — auto-detect from model name.
            let provider = auto_detect_provider(&self.model);
            (provider, self.model.clone())
        }
    }
}

fn default_timeout() -> u64 {
    1200
}

/// Auto-detect provider from model name when no explicit prefix is given.
fn auto_detect_provider(model: &str) -> String {
    let lower = model.to_lowercase();

    // OpenAI models
    if lower.starts_with("gpt-")
        || lower.starts_with("o1-")
        || lower.starts_with("o3-")
        || lower.starts_with("o4-")
        || lower.starts_with("text-")
        || lower.starts_with("dall-e-")
        || lower.starts_with("chatgpt-")
        || lower.starts_with("ft:gpt-")
    {
        return "openai".to_string();
    }

    // Anthropic models
    if lower.starts_with("claude-") {
        return "anthropic".to_string();
    }

    // Google models
    if lower.starts_with("gemini-") || lower.starts_with("gemma-") {
        return "gemini".to_string();
    }

    // Bedrock models (common prefixes)
    if lower.starts_with("anthropic.")
        || lower.starts_with("amazon.")
        || lower.starts_with("meta.")
        || lower.starts_with("cohere.")
        || lower.starts_with("ai21.")
        || lower.starts_with("mistral.")
    {
        return "bedrock".to_string();
    }

    // Azure — typically uses explicit prefix, but if someone names their deployment
    // differently, they should use `azure/deployment-name`.
    // Default fallback to openai.
    tracing::warn!(
        "Could not auto-detect provider for model '{}', defaulting to openai",
        model
    );
    "openai".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct GeneralSettings {
    /// Master key for admin access.
    pub master_key: Option<String>,
    /// PostgreSQL database URL (compatible with litellm schema).
    pub database_url: Option<String>,
    /// When true, DB is the authority for model deployments, aliases, and plans.
    /// YAML is only used to seed on first run. When false (default), YAML is
    /// the authority and DB only persists rate-limit state / key assignments.
    #[serde(default)]
    pub store_model_in_db: bool,
    /// Models accessible to ALL keys regardless of per-key model whitelist.
    /// Add new universally-available models here instead of updating every key.
    #[serde(default)]
    pub public_models: Vec<String>,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            master_key: None,
            database_url: None,
            store_model_in_db: false,
            public_models: Vec::new(),
        }
    }
}

/// Model alias configuration — supports both simple string and extended format with `hidden`.
///
/// Examples in YAML:
///   Simple:    `"gpt-4": "gpt-4o"`
///   Extended:  `"GPT-4": { model: "gpt-4o", hidden: true }`
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum ModelGroupAlias {
    Simple(String),
    Extended {
        model: String,
        #[serde(default)]
        hidden: bool,
    },
}

impl ModelGroupAlias {
    pub fn target_model(&self) -> &str {
        match self {
            ModelGroupAlias::Simple(s) => s,
            ModelGroupAlias::Extended { model, .. } => model,
        }
    }

    pub fn is_hidden(&self) -> bool {
        match self {
            ModelGroupAlias::Simple(_) => false,
            ModelGroupAlias::Extended { hidden, .. } => *hidden,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct RouterSettings {
    /// Scheduling policy: round_robin (default) or key_affinity.
    #[serde(default = "default_schedule_policy", alias = "routing_strategy")]
    pub schedule_policy: String,
    /// Model group aliases: alias_name → target_model_name.
    #[serde(default)]
    pub model_group_alias: HashMap<String, ModelGroupAlias>,
    /// Key-affinity: context threshold (total input chars) below which
    /// the policy always picks lowest-load (warm-up phase).
    /// 0 means always use affinity (no warm-up). Default: 0.
    #[serde(default)]
    pub key_affinity_context_threshold: u64,
    /// Key-affinity: rebalance threshold (percentage, 1..=100).
    /// If the preferred provider's utilization exceeds the least-loaded
    /// by more than this percentage, reassign to the least-loaded provider.
    /// Utilization = (in-flight + queued) * 100 / max_inflight per deployment.
    /// Default: 20 (20% utilization difference triggers rebalance).
    #[serde(default = "default_rebalance_threshold")]
    pub key_affinity_rebalance_threshold: u8,
    /// Content-based hybrid router (optional dynamic model alias).
    #[serde(default)]
    pub hybrid_router: Option<HybridRouterConfig>,
    /// KV-cache aware routing settings.
    #[serde(default)]
    pub kvc_aware: KvcAwareSettings,
    /// When true, inject the `X-Gateway-Priority` header into upstream requests
    /// (consumed by the downstream load-aware scheduler). Default false: no extra
    /// header is injected, keeping normal flows clean. Enable this only when
    /// rolling out downstream so that downstream schedulers can read request priority.
    #[serde(default)]
    pub enable_priority_header: bool,
}

/// Settings for KV-cache aware routing.
#[derive(Debug, Deserialize, Clone)]
pub struct KvcAwareSettings {
    /// Block size for token chunking (tokens per block). Default: 16.
    #[serde(default = "default_block_size")]
    pub block_size: usize,
    /// Weight for cache hit score in combined scoring. Default: 0.5.
    #[serde(default = "default_cache_weight")]
    pub cache_weight: f64,
    /// Weight for load score in combined scoring. Default: 0.2.
    #[serde(default = "default_load_weight")]
    pub load_weight: f64,
    /// Weight for storage tier score in combined scoring. Default: 0.3.
    #[serde(default = "default_tier_weight")]
    pub tier_weight: f64,
    /// Directory containing tokenizer.json files ({tokenizer_dir}/{model}/tokenizer.json).
    #[serde(default)]
    pub tokenizer_dir: Option<String>,
    /// ZMQ endpoints for KV event subscription (e.g., `["tcp://worker-0:5557"]`).
    #[serde(default)]
    pub zmq_endpoints: Vec<String>,
    /// ZMQ topic prefix for subscription filtering. Default: `"kv@"`.
    #[serde(default = "default_zmq_topic_prefix")]
    pub zmq_topic_prefix: String,
    /// Maximum number of indexed blocks across all models/workers.
    /// When exceeded, the least recently stored blocks are evicted. Default: 500,000.
    #[serde(default = "default_max_blocks")]
    pub max_blocks: usize,
    /// KV-cache prefix hit ratio at/above which incremental reporting is used
    /// instead of full. When a request's prefix hit ratio is below this
    /// threshold the gateway asks vLLM for a full report to backfill the trie
    /// (the trie is missing part of the reused prefix); otherwise incremental
    /// suffices because the trie already covers the prefix vLLM will reuse.
    /// Default: 0.8.
    #[serde(default = "default_full_report_hit_threshold")]
    pub full_report_hit_threshold: f64,
}

impl Default for KvcAwareSettings {
    fn default() -> Self {
        Self {
            block_size: default_block_size(),
            cache_weight: default_cache_weight(),
            load_weight: default_load_weight(),
            tier_weight: default_tier_weight(),
            tokenizer_dir: None,
            zmq_endpoints: Vec::new(),
            zmq_topic_prefix: default_zmq_topic_prefix(),
            max_blocks: default_max_blocks(),
            full_report_hit_threshold: default_full_report_hit_threshold(),
        }
    }
}

impl KvcAwareSettings {
    /// Validate semantic constraints serde cannot enforce.
    pub fn validate(&self) -> Result<(), GatewayError> {
        // full_report_hit_threshold must be a valid ratio in (0.0, 1.0]:
        //   - <= 0 would never trigger a full report, so the trie never
        //     backfills and KVC matches degrade to zero (silent failure).
        //   - > 1 would always trigger a full report (equivalent to disabling
        //     the optimization); NaN is rejected too since all comparisons
        //     against NaN are false.
        let threshold = self.full_report_hit_threshold;
        if !(threshold > 0.0 && threshold <= 1.0) {
            return Err(GatewayError::ConfigError(format!(
                "router_settings.kvc_aware.full_report_hit_threshold must be in (0.0, 1.0], got {threshold}"
            )));
        }
        Ok(())
    }
}

fn default_block_size() -> usize {
    16
}

fn default_cache_weight() -> f64 {
    0.5
}

fn default_full_report_hit_threshold() -> f64 {
    0.8
}

fn default_load_weight() -> f64 {
    0.2
}

fn default_tier_weight() -> f64 {
    0.3
}

fn default_zmq_topic_prefix() -> String {
    "kv@".to_string()
}

fn default_max_blocks() -> usize {
    500_000
}

/// Configuration for the content-based hybrid router.
///
/// When enabled, requesting the virtual `model_name` triggers content
/// analysis which maps to a real model from `model_list`.
#[derive(Debug, Deserialize, Clone)]
pub struct HybridRouterConfig {
    /// Virtual model name that triggers classification (e.g. "auto").
    pub model_name: String,
    /// Classification strategy name. Default: "tier_classifier".
    #[serde(default = "default_hybrid_strategy")]
    pub strategy: String,
    /// Default tier when classification is uncertain.
    pub default_tier: String,
    /// Tier name → tier definition.
    #[serde(default)]
    pub tiers: HashMap<String, HybridRouterTier>,
}

/// A single tier in the hybrid router configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct HybridRouterTier {
    /// Target model_name in model_list to route to for this tier.
    pub target_model: String,
}

fn default_hybrid_strategy() -> String {
    "tier_classifier".to_string()
}

fn default_schedule_policy() -> String {
    "round_robin".to_string()
}

fn default_rebalance_threshold() -> u8 {
    20
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerSettings {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_workers")]
    pub workers: usize,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            workers: default_workers(),
        }
    }
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    4000
}
fn default_workers() -> usize {
    4
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RateLimitSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Default RPM per key if not set in the database.
    #[serde(default = "default_rpm")]
    pub default_rpm: u64,
    /// Custom window limits: [[count, window_seconds], ...].
    /// Example: [[100, 18000]] = 100 requests per 5 hours.
    #[serde(default)]
    pub window_limits: Vec<Vec<u64>>,
}

impl Default for RateLimitSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            default_rpm: default_rpm(),
            window_limits: vec![],
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_rpm() -> u64 {
    60
}

/// Resolve environment variable references in a string.
/// Supports both `${VAR_NAME}` and `os.environ/VAR_NAME` syntax.
pub fn resolve_env_value(value: &str) -> String {
    let trimmed = value.trim();
    if let Some(var_name) = trimmed.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        env::var(var_name).unwrap_or_else(|_| value.to_string())
    } else if let Some(var_name) = trimmed.strip_prefix("os.environ/") {
        env::var(var_name).unwrap_or_else(|_| value.to_string())
    } else {
        value.to_string()
    }
}

impl Config {
    /// Validate semantic constraints that serde cannot enforce (e.g. numeric
    /// ranges). Returns the first violation as a `ConfigError`.
    ///
    /// Called by [`load_config`] so both startup and hot-reload reject bad
    /// values instead of silently misbehaving.
    pub fn validate(&self) -> Result<(), GatewayError> {
        // Compose per-section validation. Each settings struct owns its own
        // semantic checks; Config::validate just orchestrates them so new
        // sections plug in without growing this method.
        self.router_settings.kvc_aware.validate()?;
        Ok(())
    }
}

/// Load config from a YAML file with env var resolution.
/// Compatible with litellm's `proxy_server_config.yaml`.
pub fn load_config(path: &str) -> Result<Config, GatewayError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| GatewayError::ConfigError(format!("Failed to read config {}: {}", path, e)))?;

    // Resolve env vars in raw YAML before parsing.
    let resolved = resolve_env_vars_in_text(&content);

    let mut config: Config = serde_yaml::from_str(&resolved)
        .map_err(|e| GatewayError::ConfigError(format!("Failed to parse config: {}", e)))?;

    // Validate semantic constraints (numeric ranges, etc.) that serde cannot.
    config.validate()?;

    // Resolve env vars in string fields after parsing.
    config.general_settings.master_key = config
        .general_settings
        .master_key
        .take()
        .map(|v| resolve_env_value(&v));
    config.general_settings.database_url = config
        .general_settings
        .database_url
        .take()
        .map(|v| resolve_env_value(&v));

    for entry in &mut config.model_list {
        let p = &mut entry.litellm_params;
        p.api_key = p.api_key.take().map(|v| resolve_env_value(&v));
        p.api_base = p.api_base.take().map(|v| resolve_env_value(&v));
        p.aws_access_key_id = p.aws_access_key_id.take().map(|v| resolve_env_value(&v));
        p.aws_secret_access_key = p.aws_secret_access_key.take().map(|v| resolve_env_value(&v));
        for v in p.headers.values_mut() {
            *v = resolve_env_value(v);
        }
    }

    // Validate key-affinity rebalance threshold when policy is key_affinity.
    if config.router_settings.schedule_policy == "key_affinity" {
        let t = config.router_settings.key_affinity_rebalance_threshold;
        if t == 0 || t > 100 {
            return Err(GatewayError::ConfigError(format!(
                "key_affinity_rebalance_threshold must be 1..=100 (percentage), got {}",
                t
            )));
        }
    }

    tracing::info!(
        "Config loaded: {} model(s), policy={}",
        config.model_list.len(),
        config.router_settings.schedule_policy,
    );

    Ok(config)
}

/// Replace ${VAR} and os.environ/VAR patterns in raw text.
fn resolve_env_vars_in_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    let text_bytes = text.as_bytes();

    while let Some((i, ch)) = chars.next() {
        if ch == '$' && i + 1 < text.len() && text_bytes[i + 1] == b'{' {
            if let Some(end) = find_closing_brace(text, i + 2) {
                let var_name = &text[i + 2..end];
                if is_valid_env_var(var_name) {
                    result.push_str(
                        &env::var(var_name).unwrap_or_else(|_| format!("${{{}}}", var_name)),
                    );
                    while let Some((j, _)) = chars.next() {
                        if j >= end {
                            break;
                        }
                    }
                    continue;
                }
            }
        }
        result.push(ch);
    }

    // Handle os.environ/VAR patterns
    result = result
        .split("os.environ/")
        .enumerate()
        .map(|(i, part)| {
            if i == 0 {
                part.to_string()
            } else {
                let end = part
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .unwrap_or(part.len());
                let var_name = &part[..end];
                let rest = &part[end..];
                let resolved =
                    env::var(var_name).unwrap_or_else(|_| format!("os.environ/{}", var_name));
                format!("{}{}", resolved, rest)
            }
        })
        .collect();

    result
}

fn find_closing_brace(text: &str, start: usize) -> Option<usize> {
    for (i, ch) in text[start..].char_indices() {
        if ch == '}' {
            return Some(start + i);
        }
        if !ch.is_alphanumeric() && ch != '_' {
            return None;
        }
    }
    None
}

fn is_valid_env_var(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_explicit_provider_prefix() {
        let params = ProviderParams {
            model: "openai/gpt-4-turbo".to_string(),
            api_key: None,
            api_base: None,
            api_version: None,
            aws_region_name: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            rpm: None,
            tpm: None,
            timeout: 120,
            headers: HashMap::new(),
            temperature: None,
            max_tokens: None,
        };
        let (provider, model) = params.resolve_provider_and_model();
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4-turbo");
    }

    #[test]
    fn test_auto_detect_openai() {
        let params = ProviderParams {
            model: "gpt-4o".to_string(),
            api_key: None,
            api_base: None,
            api_version: None,
            aws_region_name: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            rpm: None,
            tpm: None,
            timeout: 120,
            headers: HashMap::new(),
            temperature: None,
            max_tokens: None,
        };
        let (provider, model) = params.resolve_provider_and_model();
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn test_auto_detect_anthropic() {
        assert_eq!(auto_detect_provider("claude-3-opus"), "anthropic");
    }

    #[test]
    fn test_auto_detect_gemini() {
        assert_eq!(auto_detect_provider("gemini-2.0-flash"), "gemini");
    }

    #[test]
    fn test_validate_full_report_hit_threshold() {
        // All defaults (threshold = 0.8) should pass.
        let mut config: Config = serde_yaml::from_str("{}").unwrap();
        assert!(config.validate().is_ok(), "default threshold should be valid");

        // Boundary: 1.0 is the maximum valid value.
        config.router_settings.kvc_aware.full_report_hit_threshold = 1.0;
        assert!(config.validate().is_ok(), "1.0 should be valid");

        // Small positive value is fine.
        config.router_settings.kvc_aware.full_report_hit_threshold = 0.01;
        assert!(config.validate().is_ok(), "0.01 should be valid");

        // Zero → reject (would never trigger a full report).
        config.router_settings.kvc_aware.full_report_hit_threshold = 0.0;
        assert!(config.validate().is_err(), "0.0 should be rejected");

        // Above 1.0 → reject.
        config.router_settings.kvc_aware.full_report_hit_threshold = 1.1;
        assert!(config.validate().is_err(), "1.1 should be rejected");

        // Negative → reject.
        config.router_settings.kvc_aware.full_report_hit_threshold = -0.5;
        assert!(config.validate().is_err(), "negative should be rejected");

        // NaN → reject (all comparisons are false).
        config.router_settings.kvc_aware.full_report_hit_threshold = f64::NAN;
        assert!(config.validate().is_err(), "NaN should be rejected");
    }
}
