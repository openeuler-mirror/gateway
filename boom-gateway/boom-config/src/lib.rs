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
    /// Attach `X-BooM-Client-Type: <label>` to every outgoing request routed
    /// to this deployment. Label is `anthropic` for `/v1/messages`, `anonymous`
    /// otherwise. Default: false.
    #[serde(default)]
    pub client_type_header: bool,
    /// Per-deployment kvc_aware endpoint ports. Lets ZMQ report + tokenize
    /// endpoints live next to api_base (host reused, only ports specified)
    /// instead of repeating full URLs under `kvc_aware.zmq_endpoints` /
    /// `vllm_tokenize_endpoints`. load_config derives the full endpoint lists
    /// from these + api_base.
    #[serde(default)]
    pub kvc: Option<KvcEndpointEntry>,
}

/// Per-deployment kvc_aware endpoint config, placed on a `ModelEntry`.
///
/// The master host is taken from `litellm_params.api_base`, so only ports
/// need to be specified here:
///   - `zmq_port`        → `tcp://<api_base_host>:<zmq_port>` (KV-event report)
///   - `tokenize_port`   → `http://<api_base_host>:<tokenize_port>` (vLLM /tokenize)
///
/// `peers` are PD peers (full ZMQ URLs, different hosts) that report KV events
/// for this cluster but don't receive requests (e.g. a 2P1D peer). They are
/// appended to the master's ZMQ group so the gateway knows they share the
/// cluster's cache.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct KvcEndpointEntry {
    /// ZMQ report port for this deployment's master.
    #[serde(default)]
    pub zmq_port: Option<u16>,
    /// vLLM /tokenize port for this deployment's master (optional — only
    /// deployments that can tokenize set this).
    #[serde(default)]
    pub tokenize_port: Option<u16>,
    /// PD peers (full `tcp://host:port` URLs) reporting KV for this cluster.
    #[serde(default)]
    pub peers: Vec<String>,
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
    /// Dedicated endpoint for KV-tokenization (vLLM `/tokenize`), used by
    /// kvc_aware routing to obtain the exact prompt tokens. Needed when
    /// `api_base` points at a PD router / frontend that doesn't forward
    /// `/tokenize` (e.g. `api_base`=router:8100, `tokenize_api_base`=vLLM:7100).
    /// If unset, the provider's `api_base` is used (with `/v1` stripped).
    pub tokenize_api_base: Option<String>,
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
    /// Shared rebalance threshold for key_affinity AND kvc_aware: if the
    /// preferred/winning provider's utilization exceeds the least-loaded
    /// candidate's by more than this percentage, hand off to the least-loaded
    /// (so it can build cache / spread load by capacity). Utilization =
    /// (in-flight + queued) * 100 / max_inflight per deployment.
    /// key_affinity applies it per-key (migration); kvc_aware applies it
    /// per-request at the scoring stage. Default: 20.
    /// (serde alias keeps the old `key_affinity_rebalance_threshold` name
    /// working for existing configs.)
    #[serde(default = "default_rebalance_threshold", alias = "key_affinity_rebalance_threshold")]
    pub rebalance_threshold: u8,
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
    /// Strip Claude Code's `x-anthropic-billing-header` attribution block from
    /// `/v1/messages` request bodies before forwarding upstream.
    ///
    /// Why: Claude Code injects a standalone text block whose content begins
    /// with `x-anthropic-billing-header: cc_version=...; cch=<random>; ...`
    /// at the head of the system prompt. The per-request `cch=` field
    /// invalidates byte-exact KV-cache prefix matching on every request,
    /// causing 100% prefix-cache miss on non-Anthropic backends. Enabling
    /// this drops the entire block (matching vLLM PR #36829), restoring
    /// cache hits.
    ///
    /// Default `false`. Only enable when routing Claude Code to non-Anthropic
    /// backends (vLLM, Bedrock, OpenAI-compatible); stripping may trip
    /// Anthropic's anti-piracy defenses when forwarding to the official API.
    ///
    /// Only affects `/v1/messages`. OpenAI `/v1/chat/completions` requests
    /// are untouched (Claude Code does not inject this header on the OpenAI
    /// protocol).
    #[serde(default)]
    pub strip_claude_code_attribution: bool,
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
    /// ZMQ endpoints for KV event subscription, grouped by KV-cache sharing.
    ///
    /// Each element is one "KV-sharing group" — workers whose caches are
    /// shared (e.g. 2 prefill workers behind a shared KV pool / RDMA fabric).
    /// When routing to a deployment in a group, prefix matching queries the
    /// whole group's cache, so a peer's cached prefix counts as a hit.
    ///
    /// Two element forms are accepted (can be mixed):
    ///   - bare string  `"tcp://h:5557"`         → single-worker group (no peers)
    ///   - string list  `["tcp://h1:5557", ...]` → shared-KV group
    /// A flat list of strings (legacy config) is still accepted: each endpoint
    /// becomes its own single-worker group (no sharing) — fully backward compatible.
    #[serde(default, deserialize_with = "deserialize_zmq_endpoint_groups")]
    pub zmq_endpoints: Vec<Vec<String>>,
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
    /// Hit-ratio threshold below which kvc_aware stops differentiating
    /// candidates by the (near-zero) cache hit and degrades to key_affinity
    /// (key-sticky + load rebalance) for the request. A very low hit means the
    /// matched prefix is too shallow to be a meaningful affinity signal (only a
    /// few shared blocks, e.g. a common system prompt), and picking the winner
    /// by tiny hit differences (0.02 vs 0.019) causes traffic to drift between
    /// workers instead of sticking to one for vLLM-local cache reuse. Set to 0
    /// to disable (only degrade on exactly hit==0). Default: 0.2.
    #[serde(default = "default_degrade_hit_threshold")]
    pub degrade_hit_threshold: f64,
    /// Per-model pool of vLLM HTTP endpoints that can serve `/tokenize` for
    /// KV-aware routing. Used when `api_base` points at a PD router / frontend
    /// that doesn't forward `/tokenize`. The gateway picks the least-loaded
    /// endpoint (by reported queue depth) and fails over to the next on error
    /// / 404. Each entry is a bare base URL (host:port); `/tokenize` is
    /// appended at the server root.
    /// Example: `{ "MiniMax-M2.7": ["http://7.150.7.202:7100", "http://7.150.2.142:7100"] }`
    #[serde(default)]
    pub vllm_tokenize_endpoints: HashMap<String, Vec<String>>,
    /// Overload gate for kvc_aware routing: a candidate whose gateway-side
    /// inflight load ≥ this percentage of its capacity is HARD-EXCLUDED from
    /// selection (load_pct from inflight/capacity). 100 disables the gate.
    /// Default: 90.
    #[serde(default = "default_overload_threshold_pct")]
    pub overload_threshold_pct: u64,
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
            degrade_hit_threshold: default_degrade_hit_threshold(),
            vllm_tokenize_endpoints: HashMap::new(),
            overload_threshold_pct: default_overload_threshold_pct(),
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
        // degrade_hit_threshold must be in [0.0, 1.0): 0 disables the low-hit
        // degradation (only degrade on hit==0); >= 1 would degrade always,
        // making kvc_aware behave as pure key_affinity; NaN breaks comparisons.
        let degrade = self.degrade_hit_threshold;
        if !(degrade >= 0.0 && degrade < 1.0) {
            return Err(GatewayError::ConfigError(format!(
                "router_settings.kvc_aware.degrade_hit_threshold must be in [0.0, 1.0), got {degrade}"
            )));
        }
        // overload_threshold_pct: 1..=100. 100 disables the overload gate;
        // values >100 are silently a no-op (load_pct is capped at 100, so
        // load_pct >= 150 is never true) — reject to avoid a misleading
        // "configured but ineffective" state. 0 would hard-exclude every
        // candidate (load_pct >= 0 always true), also rejected.
        let otp = self.overload_threshold_pct;
        if !(1..=100).contains(&otp) {
            return Err(GatewayError::ConfigError(format!(
                "router_settings.kvc_aware.overload_threshold_pct must be in 1..=100 (100 disables), got {otp}"
            )));
        }
        Ok(())
    }
}

fn default_block_size() -> usize {
    16
}

/// Deserialize `zmq_endpoints` accepting both grouped and flat forms.
///
/// Each YAML element may be either:
///   - a bare scalar string (single-worker group), or
///   - a sequence of strings (shared-KV group).
/// Produces `Vec<Vec<String>>` — one inner vec per KV-sharing group. A bare
/// string becomes a single-element group. Untagged dispatch tries the sequence
/// form first so a single-element array `["a"]` stays a group of one.
fn deserialize_zmq_endpoint_groups<'de, D>(deserializer: D) -> Result<Vec<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Group(Vec<String>),
        Single(String),
    }

    let entries = Vec::<Entry>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .map(|e| match e {
            Entry::Group(v) => v,
            Entry::Single(s) => vec![s],
        })
        .collect())
}

fn default_cache_weight() -> f64 {
    0.5
}

fn default_overload_threshold_pct() -> u64 {
    90
}

fn default_full_report_hit_threshold() -> f64 {
    0.8
}

fn default_degrade_hit_threshold() -> f64 {
    0.2
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

/// Delegate to boom_core::normalize::host_of_api_base (single, IPv6-correct
/// implementation shared across crates). Kept as a thin local alias so call
/// sites in this file read naturally.
fn host_of_api_base(api_base: &str) -> Option<String> {
    boom_core::normalize::host_of_api_base(api_base)
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
        p.tokenize_api_base = p.tokenize_api_base.take().map(|v| resolve_env_value(&v));
        p.aws_access_key_id = p.aws_access_key_id.take().map(|v| resolve_env_value(&v));
        p.aws_secret_access_key = p.aws_secret_access_key.take().map(|v| resolve_env_value(&v));
        for v in p.headers.values_mut() {
            *v = resolve_env_value(v);
        }
    }

    // Derive kvc_aware zmq_endpoints / vllm_tokenize_endpoints from model_list
    // entries that carry a `kvc:` block. The master host comes from each
    // entry's api_base; only ports (and PD peers) are specified per-deployment.
    // Derived endpoints are appended to any explicitly-configured ones, so the
    // two forms can coexist during migration.
    for entry in &config.model_list {
        let Some(kvc) = &entry.kvc else { continue };
        let Some(host) = entry
            .litellm_params
            .api_base
            .as_deref()
            .and_then(host_of_api_base)
        else {
            continue;
        };
        if kvc.zmq_port.is_some() || !kvc.peers.is_empty() {
            // Subscription group: master ZMQ URL (only when it reports) + peers.
            // When the master itself doesn't report (zmq_port absent) but peers
            // do, the group is peers-only — the master is still linked into the
            // KV-sharing group for routing via the model_list derivation in
            // state.rs (so routing to the master considers the peers' cache).
            let mut group: Vec<String> = Vec::new();
            if let Some(zport) = kvc.zmq_port {
                group.push(format!("tcp://{host}:{zport}"));
            }
            group.extend(kvc.peers.iter().cloned());
            config.router_settings.kvc_aware.zmq_endpoints.push(group);
        }
        if let Some(tport) = kvc.tokenize_port {
            config
                .router_settings
                .kvc_aware
                .vllm_tokenize_endpoints
                .entry(entry.model_name.clone())
                .or_default()
                .push(format!("http://{host}:{tport}"));
        }
    }

    // Validate the shared rebalance threshold when a policy that uses it is
    // active (key_affinity applies it per-key; kvc_aware at scoring stage).
    if matches!(config.router_settings.schedule_policy.as_str(), "key_affinity" | "kvc_aware") {
        let t = config.router_settings.rebalance_threshold;
        if t == 0 || t > 100 {
            return Err(GatewayError::ConfigError(format!(
                "rebalance_threshold must be 1..=100 (percentage), got {}",
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
    fn zmq_endpoints_accepts_grouped_flat_and_mixed() {
        // Deserialize through KvcAwareSettings so the field's custom
        // deserialize_with (string | string-list per element) is exercised.

        // Fully backward-compatible flat list: each endpoint → its own group.
        let flat: KvcAwareSettings = serde_yaml::from_str(
            r#"
zmq_endpoints:
  - "tcp://10.0.0.1:5557"
  - "tcp://10.0.0.2:5557"
"#,
        )
        .unwrap();
        assert_eq!(flat.zmq_endpoints, vec![
            vec!["tcp://10.0.0.1:5557".to_string()],
            vec!["tcp://10.0.0.2:5557".to_string()],
        ]);

        // Grouped (shared KV): a multi-element list is one group.
        let grouped: KvcAwareSettings = serde_yaml::from_str(
            r#"
zmq_endpoints:
  - ["tcp://10.0.0.1:5557", "tcp://10.0.0.2:5557"]
  - ["tcp://10.0.0.3:5557"]
"#,
        )
        .unwrap();
        assert_eq!(grouped.zmq_endpoints, vec![
            vec!["tcp://10.0.0.1:5557".to_string(), "tcp://10.0.0.2:5557".to_string()],
            vec!["tcp://10.0.0.3:5557".to_string()],
        ]);

        // Mixed: bare strings and arrays together.
        let mixed: KvcAwareSettings = serde_yaml::from_str(
            r#"
zmq_endpoints:
  - "tcp://10.0.0.9:5557"
  - ["tcp://10.0.0.1:5557", "tcp://10.0.0.2:5557"]
"#,
        )
        .unwrap();
        assert_eq!(mixed.zmq_endpoints, vec![
            vec!["tcp://10.0.0.9:5557".to_string()],
            vec!["tcp://10.0.0.1:5557".to_string(), "tcp://10.0.0.2:5557".to_string()],
        ]);
    }

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
            tokenize_api_base: None,
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
            tokenize_api_base: None,
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
