use boom_core::GatewayError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;

mod workflow;

pub use workflow::{
    DirectSynthesisRolesConfig, WorkflowDefinitionConfig, WorkflowModelInstanceConfig,
    WorkflowSettings,
};

/// Re-export of `boom_core::types::WindowLimit` — single source of truth.
///
/// Defined in boom-core (the leaf crate) so both boom-config (YAML parse) and
/// boom-limiter (plan model) reference the same type without boom-limiter
/// depending on boom-config. The serde helper also lives in boom-core for the
/// same reason — every module that owns a `Vec<WindowLimit>` field needs it.
pub use boom_core::types::WindowLimit;

/// Re-export of the serde helper — see `boom_core::types::deserialize_window_limit_vec`.
pub use boom_core::types::deserialize_window_limit_vec;

/// Field manifest — single source of truth for which config fields are
/// editable from the dashboard UI. See `manifest.rs` and CLAUDE.md §9.
pub mod manifest;

/// Top-level gateway configuration, loaded from YAML.
/// Compatible with litellm's `proxy_server_config.yaml` format.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub model_list: Vec<ModelEntry>,
    #[serde(default)]
    pub general_settings: GeneralSettings,
    #[serde(default)]
    pub router_settings: RouterSettings,
    #[serde(default)]
    pub workflow_settings: WorkflowSettings,
    #[serde(default)]
    pub server: ServerSettings,
    #[serde(default)]
    pub rate_limit: RateLimitSettings,
    #[serde(default)]
    pub plan_settings: PlanSettings,
    /// Reusable billing templates — referenced by `model_info.cost_template`.
    /// Lets ops define one rate set (e.g. deepseek-v3 pricing) and bind it to
    /// multiple deployments without repeating the cost fields per model.
    #[serde(default)]
    pub cost_templates: Vec<CostTemplate>,
    #[serde(default)]
    pub deployment_health_check: DeploymentHealthCheckSettings,
    /// Prompt log configuration (transparent pass-through to boom-promptlog).
    #[serde(default)]
    pub prompt_log: Option<serde_json::Value>,
    /// Gateway hook configuration. Optional — when absent or all entries
    /// disabled, the gateway runs as if the hook framework didn't exist.
    #[serde(default)]
    pub hooks: HooksConfig,
}

/// Top-level hook configuration. Each entry corresponds to one hook point
/// the gateway exposes. All entries default to disabled, so omitting the
/// whole `hooks` block is equivalent to "no hooks".
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct HooksConfig {
    #[serde(default)]
    pub pre_auth: PreAuthHookConfig,
}

/// Configuration for the `pre_auth` hook point.
///
/// ```yaml
/// hooks:
///   pre_auth:
///     enabled: true
///     path: /etc/gateway/hooks/libmyhook.so
///     failure_mode: allow          # "allow" (default) | "deny"
///     allowed_headers: [ucid]      # headers forwarded to the hook
///     config: |                     # free-form string, passed to hook_init
///       db_url: postgres://…
/// ```
///
/// When `enabled` is false (or `path` is empty), the gateway skips this hook
/// point entirely and authentication runs as if no hook existed.
///
/// `failure_mode`:
/// - `allow`: hook call failure (panic / error code / overflow) → fall back to
///   the original raw_key and run native authentication. Default. Keeps the
///   gateway fully functional when a misconfigured hook breaks.
/// - `deny`: hook call failure → reject the request with 500. Use when a
///   missing hook is a security-relevant event and silent fallback is worse.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct PreAuthHookConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub failure_mode: HookFailureMode,
    #[serde(default)]
    pub allowed_headers: Vec<String>,
    #[serde(default)]
    pub config: String,
}

/// Behaviour when a hook call itself fails (panic, error code, overflow).
///
/// `Allow` is the default because the hook framework is optional — a broken
/// hook must not bring down the gateway. `Deny` is opt-in for sites that
/// treat hook failure as a security event.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HookFailureMode {
    #[default]
    Allow,
    Deny,
}

impl HooksConfig {
    /// Returns true if no hook point is enabled. Lets the gateway skip all
    /// hook machinery with a single check in the hot path.
    pub fn is_empty(&self) -> bool {
        !self.pre_auth.enabled
    }
}

/// A reusable billing rate template.
///
/// All rates are **USD per million tokens** — write `0.27` for $0.27/1M tokens,
/// not `0.00000027`. The gateway converts to per-token internally.
///
/// ```yaml
/// cost_templates:
///   - name: deepseek-v3
///     input_cost_per_million_tokens: 0.27
///     cached_input_cost_per_million_tokens: 0.014   # 1/20 of input
///     output_cost_per_million_tokens: 1.10
/// ```
///
/// Bind a model to a template via `model_info.cost_template: deepseek-v3`.
/// Template fields override any inline cost fields on the model (avoids
/// ambiguity when both are set).
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct CostTemplate {
    pub name: String,
    #[serde(default)]
    pub input_cost_per_million_tokens: Option<f64>,
    /// USD per million cached input tokens (KV-cache hit). Usually 1/N of
    /// input price. When None / 0 → cache hits billed at the regular input rate.
    #[serde(default)]
    pub cached_input_cost_per_million_tokens: Option<f64>,
    #[serde(default)]
    pub output_cost_per_million_tokens: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
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

/// Re-export of `boom_core::types::PlanType` so config consumers don't need
/// to depend on boom-limiter. Defined in boom-core as the single source of truth.
pub use boom_core::types::PlanType;

/// A single plan definition in YAML config (plan name comes from the HashMap key).
///
/// A plan is a **generic template** — all limit fields apply uniformly to
/// whichever entity (key or team) the plan is assigned to. The `type` field
/// only acts as a guard against misassignment: `type=team` plans can only be
/// assigned to teams, `type=key` plans only to keys (typically team plans
/// carry larger quotas so mis-assigning them to a single key would be
/// dangerous).
///
/// Field names therefore carry **no** `key_` / `team_` prefix — there is only
/// one set of limits per plan.
///
/// `window_limits` accepts multi-dimensional entries — see [`WindowLimit`].
/// The legacy `rpm_limit` / `tpm_limit` fields are kept as 1-minute-window
/// convenience shorthand; they get merged into the effective window list as
/// a synthetic 60s entry at evaluation time, so configs can mix the
/// shorthand and the explicit `window_limits`.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct PlanConfig {
    #[serde(default)]
    pub r#type: PlanType,
    /// Only used when type=Team. Plan name applied to each member key.
    #[serde(default)]
    pub member_plan: Option<String>,

    #[serde(default)]
    pub concurrency_limit: Option<u32>,
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    #[serde(default)]
    pub tpm_limit: Option<u64>,
    #[serde(
        default,
        deserialize_with = "deserialize_window_limit_vec"
    )]
    pub window_limits: Vec<WindowLimit>,
    #[serde(default)]
    pub total_token_limit: Option<u64>,
    #[serde(default)]
    pub total_cost_limit: Option<rust_decimal::Decimal>,

    /// Optional time-based schedule overrides.
    #[serde(default)]
    pub schedule: Vec<ScheduleSlotConfig>,
}

/// A time-based schedule slot within a plan.
///
/// Slots override the base plan fields during their active time window.
/// Like the plan itself, slot fields are **generic** (no key_/team_ prefix)
/// — they apply to whichever entity the parent plan is assigned to.
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
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ScheduleSlotConfig {
    /// Time range, e.g. "9:00-21:00" or "21:00-9:00" (cross-midnight).
    pub hours: String,
    #[serde(default)]
    pub concurrency_limit: Option<u32>,
    #[serde(default)]
    pub rpm_limit: Option<u64>,
    #[serde(default)]
    pub tpm_limit: Option<u64>,
    #[serde(
        default,
        deserialize_with = "deserialize_window_limit_vec"
    )]
    pub window_limits: Vec<WindowLimit>,
}

/// Top-level plan settings section.
///
/// ```yaml
/// plan_settings:
///   default_plan: "basic"
///   default_team_plan: "team_basic"
///   plans:
///     basic:
///       concurrency_limit: 4
///       rpm_limit: 60
///       window_limits: [[100, 18000]]
/// ```
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct PlanSettings {
    /// Name of the plan to use for keys without an explicit assignment.
    pub default_plan: Option<String>,
    /// Name of the plan to use for teams without an explicit assignment.
    pub default_team_plan: Option<String>,
    /// Plan name → plan definition.
    #[serde(default)]
    pub plans: HashMap<String, PlanConfig>,
}

/// Model metadata — compatible with litellm's `model_info` field.
///
/// Pricing is configured exclusively via `cost_templates` (top-level YAML
/// section). Inline cost fields were removed to avoid ambiguity with
/// template rates — a single source of truth per model.
///
/// ```yaml
/// model_info:
///   cost_template: deepseek-v3      # reference into cost_templates
///   quota_count_ratio: 2            # each request counts as 2 against quota
/// ```
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct ModelInfo {
    pub id: Option<String>,
    /// Quota count multiplier for this model.
    /// Each request consumes `quota_count_ratio` units instead of 1.
    /// Defaults to 1 when not set.
    pub quota_count_ratio: Option<u64>,
    /// Reference to a `cost_templates` entry by name. Single source of
    /// pricing truth — the gateway looks up rates here, never inline.
    #[serde(default)]
    pub cost_template: Option<String>,
}

/// Per-deployment flow control configuration.
///
/// ```yaml
/// flow_control:
///   model_queue_limit: 50
///   model_context_limit: 5000000
/// ```
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct FlowControlEntry {
    /// Max concurrent in-flight requests. 0 or unset = no limit.
    #[serde(default)]
    pub model_queue_limit: Option<u32>,
    /// Max total input context chars across all in-flight requests. 0 or unset = no limit.
    #[serde(default)]
    pub model_context_limit: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
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
#[derive(Debug, Deserialize, Serialize, Clone)]
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

#[derive(Debug, Deserialize, Serialize, Clone)]
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
#[derive(Debug, Deserialize, Serialize, Clone)]
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

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
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
    /// Content-based auto router (optional dynamic model alias).
    #[serde(default)]
    pub auto_router: Option<AutoRouterConfig>,
    /// KV-cache aware routing settings.
    #[serde(default)]
    pub kvc_aware: KvcAwareSettings,
    /// When true, inject the `X-Gateway-Priority` header into upstream requests
    /// (consumed by the downstream load-aware scheduler). Default false: no extra
    /// header is injected, keeping normal flows clean. Enable this only when
    /// rolling out downstream so that downstream schedulers can read request priority.
    #[serde(default)]
    pub enable_priority_header: bool,
    /// Maximum time a parent or workflow child request may wait in a
    /// deployment flow-control queue. Omitted configs retain the historical
    /// 1200-second behavior.
    #[serde(default)]
    pub flow_control_queue_timeout_secs: Option<u64>,
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

impl RouterSettings {
    pub fn flow_control_queue_timeout_secs(&self) -> u64 {
        self.flow_control_queue_timeout_secs.unwrap_or(1200)
    }
}

/// Settings for KV-cache aware routing.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct KvcAwareSettings {
    /// Block size in BYTES for the gateway-side prefix serialization chunking.
    /// The request's serialized prefix (system+tools+messages) is sliced into
    /// blocks of this many bytes; each block is xxhash3-64'd into a trie edge.
    /// Default: 512 (≈ original 128-token granularity; 256-block record cap
    /// covers 128KB, enough for a typical system+tools prefix).
    #[serde(default = "default_block_size")]
    pub block_size: usize,
    /// Weight for cache hit score in combined scoring. Default: 0.7.
    #[serde(default = "default_cache_weight")]
    pub cache_weight: f64,
    /// Weight for load score in combined scoring. Default: 0.3.
    #[serde(default = "default_load_weight")]
    pub load_weight: f64,
    /// Maximum number of indexed blocks across all models/workers.
    /// When exceeded, the least recently stored blocks are evicted. Default: 500,000.
    #[serde(default = "default_max_blocks")]
    pub max_blocks: usize,
    /// Overload gate for kvc_aware routing: a candidate whose gateway-side
    /// inflight load ≥ this percentage of its capacity is HARD-EXCLUDED from
    /// selection (load_pct from inflight/capacity). 100 disables the gate.
    /// Default: 90.
    #[serde(default = "default_overload_threshold_pct")]
    pub overload_threshold_pct: u64,
    /// TTL in seconds for approximate-mode blocks. The trie is self-learned
    /// (gateway records routed prefixes); without a real evict signal from
    /// vLLM, blocks expire by wall-clock to bound over-approximation. A
    /// background sweep prunes blocks older than this TTL. Default: 1200.0.
    #[serde(default = "default_router_ttl_secs")]
    pub router_ttl_secs: f64,
}

impl Default for KvcAwareSettings {
    fn default() -> Self {
        Self {
            block_size: default_block_size(),
            cache_weight: default_cache_weight(),
            load_weight: default_load_weight(),
            max_blocks: default_max_blocks(),
            overload_threshold_pct: default_overload_threshold_pct(),
            router_ttl_secs: default_router_ttl_secs(),
        }
    }
}

impl KvcAwareSettings {
    /// Validate semantic constraints serde cannot enforce.
    pub fn validate(&self) -> Result<(), GatewayError> {
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
        // router_ttl_secs: 0 disables the TTL prune task (LRU-only); >0 is the TTL in seconds.
        // Must be finite and non-negative: NaN and ±∞ are rejected — ∞ would panic
        // Duration::from_secs_f64 in spawn_kv_prune_task.
        if !self.router_ttl_secs.is_finite() || self.router_ttl_secs < 0.0 {
            return Err(GatewayError::ConfigError(format!(
                "router_settings.kvc_aware.router_ttl_secs must be finite and >= 0 (0 disables TTL prune), got {}",
                self.router_ttl_secs
            )));
        }
        // block_size: 0 makes the trie silently inert (n_full=0, no blocks hashed).
        if self.block_size == 0 {
            return Err(GatewayError::ConfigError(
                "router_settings.kvc_aware.block_size must be > 0".to_string(),
            ));
        }
        // max_blocks: 0 triggers a silent fallback to 500_000 in LruCache::new; reject so
        // the configured value is honored (or the user learns it is invalid).
        if self.max_blocks == 0 {
            return Err(GatewayError::ConfigError(
                "router_settings.kvc_aware.max_blocks must be > 0".to_string(),
            ));
        }
        // Weights are mixing coefficients in [0,1]. range.contains() returns false for NaN,
        // so NaN is rejected here too.
        if !(0.0..=1.0).contains(&self.cache_weight) {
            return Err(GatewayError::ConfigError(format!(
                "router_settings.kvc_aware.cache_weight must be in 0.0..=1.0, got {}",
                self.cache_weight
            )));
        }
        if !(0.0..=1.0).contains(&self.load_weight) {
            return Err(GatewayError::ConfigError(format!(
                "router_settings.kvc_aware.load_weight must be in 0.0..=1.0, got {}",
                self.load_weight
            )));
        }
        // score = cache_weight·hit + load_weight·load_avail is selected by max, so the sum
        // isn't a mathematical requirement — but capping it at 1.0 keeps score normalized to
        // [0,1] and matches the original weighted design (cw+lw+tw=1.0 before tier removal).
        if self.cache_weight + self.load_weight > 1.0 {
            return Err(GatewayError::ConfigError(format!(
                "router_settings.kvc_aware.cache_weight + load_weight must be <= 1.0, got {}",
                self.cache_weight + self.load_weight
            )));
        }
        Ok(())
    }
}

fn default_block_size() -> usize {
    512
}

fn default_router_ttl_secs() -> f64 {
    1200.0
}

fn default_cache_weight() -> f64 {
    0.7
}

fn default_overload_threshold_pct() -> u64 {
    90
}

fn default_load_weight() -> f64 {
    0.3
}

fn default_max_blocks() -> usize {
    500_000
}

/// Configuration for the content-based auto router.
///
/// When enabled, requesting the virtual `model_name` triggers content
/// analysis which maps to a real model from `model_list`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AutoRouterConfig {
    /// Virtual model name that triggers classification (e.g. "auto").
    pub model_name: String,
    /// Classification strategy name. Default: "tier_classifier".
    #[serde(default = "default_auto_strategy")]
    pub strategy: String,
    /// Default tier when classification is uncertain.
    pub default_tier: String,
    /// Tier name → tier definition.
    #[serde(default)]
    pub tiers: HashMap<String, AutoRouterTier>,
    /// Settings for the `ml_service` strategy. Required when `strategy = "ml_service"`.
    #[serde(default)]
    pub ml_service: Option<MlServiceConfig>,
}

/// A single tier in the auto router configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AutoRouterTier {
    /// Target model_name in model_list to route to for this tier.
    pub target_model: String,
}

/// Configuration for the `ml_service` classification strategy.
///
/// The gateway POSTs `{messages, tools, default_tier}` to `{url}/classify`
/// and expects `{"tier": "..."}` back. On any failure (timeout, HTTP error,
/// malformed JSON, unknown tier), the strategy falls back to `TierClassifier`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MlServiceConfig {
    /// Base URL of the ML service, e.g. `http://127.0.0.1:2345`.
    /// The gateway appends `/classify`.
    pub url: String,
    /// Request timeout in milliseconds. Covers connect + write + read.
    /// Default 100ms — ML service must be fast and local.
    #[serde(default = "default_ml_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_ml_timeout_ms() -> u64 {
    100
}

fn default_auto_strategy() -> String {
    "tier_classifier".to_string()
}

fn default_schedule_policy() -> String {
    "round_robin".to_string()
}

fn default_rebalance_threshold() -> u8 {
    20
}

#[derive(Debug, Deserialize, Serialize, Clone)]
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
    /// Fallback switch for keys without explicit rate configuration.
    ///
    /// When true (default), `default_rpm` / `default_tpm` / `window_limits`
    /// are applied to any key that has no plan assignment AND no DB-set
    /// rpm/tpm_limit — i.e. the unconfigured-key safety net is active.
    ///
    /// When false, unconfigured keys are not rate-limited at all by the
    /// fallback path; only keys that carry their own limits (via plan or
    /// DB column) are constrained. Keys with explicit configuration are
    /// never affected by this switch.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-key fallback RPM. Applied only when a key has neither a plan
    /// assignment nor its own `rpm_limit` in the DB. Acts as the last-mile
    /// safety net so unconfigured keys still get a sane rate cap.
    #[serde(default = "default_rpm")]
    pub default_rpm: u64,
    /// Per-key fallback TPM. Same precedence as `default_rpm` — only kicks in
    /// for plan-less, DB-unset keys.
    #[serde(default)]
    pub default_tpm: Option<u64>,
    /// Custom window limits: [[count, window_seconds], ...].
    /// Example: [[100, 18000]] = 100 requests per 5 hours.
    /// Applied at the key scope in the legacy (plan-less) path.
    #[serde(default)]
    pub window_limits: Vec<Vec<u64>>,
}

impl Default for RateLimitSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            default_rpm: default_rpm(),
            default_tpm: None,
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
        if self
            .router_settings
            .flow_control_queue_timeout_secs
            .is_some_and(|seconds| seconds == 0)
        {
            return Err(GatewayError::ConfigError(
                "router_settings.flow_control_queue_timeout_secs must be greater than zero"
                    .to_string(),
            ));
        }
        self.workflow_settings.validate(self)?;
        Ok(())
    }

    /// Look up a [`CostTemplate`] by name. Used wherever a model's
    /// `model_info.cost_template` reference needs to be resolved to actual
    /// rates. Centralizing the lookup keeps the "cost_templates is the only
    /// source of pricing truth" rule in one place.
    pub fn lookup_cost_template(&self, name: &str) -> Option<&CostTemplate> {
        self.cost_templates.iter().find(|t| t.name == name)
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

// ─────────────────────────────────────────────────────────────────────
// Raw YAML I/O (used by web admin path)
// ─────────────────────────────────────────────────────────────────────

/// Read a YAML file as a raw `serde_yaml::Value` WITHOUT resolving env vars.
///
/// The typed `load_config()` resolves env vars at parse time, which would leak
/// secrets (`${OPENAI_API_KEY}` → `sk-...`) if serialized back to disk. The web
/// admin path must read raw, mutate only the edited section, and write back —
/// preserving env references in untouched sections.
pub fn read_raw_yaml(path: &str) -> Result<serde_yaml::Value, GatewayError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| GatewayError::ConfigError(format!("Failed to read config {}: {}", path, e)))?;
    serde_yaml::from_str(&content)
        .map_err(|e| GatewayError::ConfigError(format!("Failed to parse YAML: {}", e)))
}

/// Write a `serde_yaml::Value` to a file atomically.
///
/// Writes to `{path}.tmp`, fsyncs, then renames to the target. The atomic
/// rename guarantees the target file is never in a half-written state, so a
/// crash mid-write leaves the previous config intact.
pub fn write_yaml_atomic(path: &str, value: &serde_yaml::Value) -> Result<(), GatewayError> {
    let yaml_str = serde_yaml::to_string(value)
        .map_err(|e| GatewayError::ConfigError(format!("Failed to serialize YAML: {}", e)))?;

    let tmp_path = format!("{}.tmp", path);
    std::fs::write(&tmp_path, &yaml_str)
        .map_err(|e| GatewayError::ConfigError(format!("Failed to write tmp file {}: {}", tmp_path, e)))?;

    let file = std::fs::File::open(&tmp_path)
        .map_err(|e| GatewayError::ConfigError(format!("Failed to open tmp file for fsync: {}", e)))?;
    file.sync_all()
        .map_err(|e| GatewayError::ConfigError(format!("Failed to fsync tmp file: {}", e)))?;
    drop(file);

    std::fs::rename(&tmp_path, path)
        .map_err(|e| GatewayError::ConfigError(format!("Failed to rename tmp file to {}: {}", path, e)))?;

    Ok(())
}

/// Set a value at a nested YAML path (e.g., `["server", "host"]`).
///
/// Creates intermediate mappings as needed. Used by the web admin path to
/// apply surgical edits without disturbing the rest of the document.
pub fn set_yaml_path(
    root: &mut serde_yaml::Value,
    path: &[&str],
    new_value: serde_yaml::Value,
) -> Result<(), GatewayError> {
    if path.is_empty() {
        *root = new_value;
        return Ok(());
    }

    let mut current = root;
    for (i, segment) in path.iter().enumerate() {
        let is_last = i == path.len() - 1;
        if !current.is_mapping() {
            return Err(GatewayError::ConfigError(format!(
                "YAML path segment '{}' expects a mapping",
                segment
            )));
        }
        let mapping = current.as_mapping_mut().unwrap();
        let key = serde_yaml::Value::String((*segment).to_string());
        if is_last {
            mapping.insert(key, new_value);
            return Ok(());
        }
        if !mapping.contains_key(&key) {
            mapping.insert(key.clone(), serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        }
        current = mapping.get_mut(&key).unwrap();
    }
    Ok(())
}

/// Field names whose values must never be shown to the browser. Case-insensitive.
///
/// Covers litellm secret fields (master_key, database_url, api_key, aws
/// credentials) plus common HTTP header names that routinely carry secrets
/// (authorization, x-api-key, token, bearer, cookie, set-cookie,
/// proxy-authorization). The header set is needed because model deployments
/// accept a free-form `headers` map.
const SECRET_FIELD_NAMES: &[&str] = &[
    "master_key",
    "database_url",
    "api_key",
    "aws_access_key_id",
    "aws_secret_access_key",
    "authorization",
    "x-api-key",
    "api-key",
    "token",
    "bearer",
    "cookie",
    "set-cookie",
    "proxy-authorization",
];

/// True if a JSON/YAML key (case-insensitive) names a secret field.
pub fn is_secret_field(name: &str) -> bool {
    SECRET_FIELD_NAMES
        .iter()
        .any(|s| name.eq_ignore_ascii_case(s))
}

/// Recursively replace values of secret fields with `"****"`. Null values are
/// preserved so the UI can distinguish "configured" (shows ****) from "unset"
/// (shows empty). Used to scrub the GET /admin/config response.
pub fn mask_secrets_in_place(value: &mut serde_json::Value) {
    const MASK: &str = "****";
    if let Some(obj) = value.as_object_mut() {
        for (k, v) in obj.iter_mut() {
            if is_secret_field(k) {
                if !v.is_null() {
                    *v = serde_json::Value::String(MASK.to_string());
                }
            } else {
                mask_secrets_in_place(v);
            }
        }
    } else if let Some(arr) = value.as_array_mut() {
        for v in arr {
            mask_secrets_in_place(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_secrets_in_place_masks_known_fields() {
        let mut v = serde_json::json!({
            "master_key": "sk-live-123",
            "database_url": "postgres://user:pw@host/db",
            "model_list": [
                {
                    "litellm_params": {
                        "api_key": "sk-real",
                        "model": "gpt-4",
                        "headers": {
                            "Authorization": "Bearer secret",
                            "X-Custom": "keep-me"
                        }
                    }
                }
            ],
            "router_settings": {
                "schedule_policy": "round_robin"
            }
        });
        mask_secrets_in_place(&mut v);

        assert_eq!(v["master_key"], "****");
        assert_eq!(v["database_url"], "****");
        assert_eq!(v["model_list"][0]["litellm_params"]["api_key"], "****");
        assert_eq!(
            v["model_list"][0]["litellm_params"]["headers"]["Authorization"],
            "****"
        );
        // Non-secret fields preserved verbatim, including case-insensitive
        // matching for Authorization.
        assert_eq!(v["model_list"][0]["litellm_params"]["model"], "gpt-4");
        assert_eq!(
            v["model_list"][0]["litellm_params"]["headers"]["X-Custom"],
            "keep-me"
        );
        assert_eq!(v["router_settings"]["schedule_policy"], "round_robin");
    }

    #[test]
    fn test_mask_secrets_in_place_preserves_null() {
        // Null means "field unset" — UI must distinguish from "configured".
        let mut v = serde_json::json!({
            "master_key": null,
            "api_key": "sk-real"
        });
        mask_secrets_in_place(&mut v);
        assert!(v["master_key"].is_null());
        assert_eq!(v["api_key"], "****");
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
    fn test_validate_kvc_aware_params() {
        // Defaults (block_size=512, max_blocks=500_000, cw=0.5, lw=0.2) pass.
        let mut config: Config = serde_yaml::from_str("{}").unwrap();
        assert!(config.validate().is_ok(), "defaults should be valid");

        // router_ttl_secs: 0 = valid (disables TTL prune); negative / NaN / ±∞ → reject.
        config.router_settings.kvc_aware.router_ttl_secs = 0.0;
        assert!(config.validate().is_ok(), "router_ttl_secs=0 should be valid (disables TTL)");
        config.router_settings.kvc_aware.router_ttl_secs = -1.0;
        assert!(config.validate().is_err(), "negative ttl should be rejected");
        config.router_settings.kvc_aware.router_ttl_secs = f64::NAN;
        assert!(config.validate().is_err(), "NaN ttl should be rejected");
        config.router_settings.kvc_aware.router_ttl_secs = f64::INFINITY;
        assert!(config.validate().is_err(), "infinite ttl should be rejected");
        config.router_settings.kvc_aware.router_ttl_secs = 120.0;

        // block_size = 0 → reject (trie would be silently inert).
        config.router_settings.kvc_aware.block_size = 0;
        assert!(config.validate().is_err(), "block_size=0 should be rejected");
        config.router_settings.kvc_aware.block_size = 512;

        // max_blocks = 0 → reject (silent 500_000 fallback otherwise).
        config.router_settings.kvc_aware.max_blocks = 0;
        assert!(config.validate().is_err(), "max_blocks=0 should be rejected");
        config.router_settings.kvc_aware.max_blocks = 500_000;

        // cache_weight out of [0,1] → reject (NaN rejected too: contains() is false for NaN).
        config.router_settings.kvc_aware.cache_weight = 1.5;
        assert!(config.validate().is_err(), "cache_weight=1.5 should be rejected");
        config.router_settings.kvc_aware.cache_weight = -0.1;
        assert!(config.validate().is_err(), "negative cache_weight should be rejected");
        config.router_settings.kvc_aware.cache_weight = f64::NAN;
        assert!(config.validate().is_err(), "NaN cache_weight should be rejected");

        // Boundary: cache_weight=1.0, load_weight=0.0 (sum=1.0) is valid.
        config.router_settings.kvc_aware.cache_weight = 1.0;
        config.router_settings.kvc_aware.load_weight = 0.0;
        assert!(config.validate().is_ok(), "cw=1.0/lw=0.0 should be valid");

        // sum > 1.0 → reject (normalization).
        config.router_settings.kvc_aware.cache_weight = 0.5;
        config.router_settings.kvc_aware.load_weight = 0.6;
        assert!(config.validate().is_err(), "cw+lw>1.0 should be rejected");

        // sum == 1.0 → valid.
        config.router_settings.kvc_aware.load_weight = 0.5;
        assert!(config.validate().is_ok(), "cw+lw==1.0 should be valid");
    }

    #[test]
    fn test_window_limit_compact_array_syntax() {
        // Compact array form: [counts, tokens, costs, window_secs]
        let yaml = r#"
plan_settings:
  default_plan: basic
  plans:
    basic:
      window_limits:
        - [1000, null, null, 3600]
        - [null, 5000000, 5.0, 3600]
        - [null, null, 1.0, 60]
    team_plan:
      window_limits:
        - counts: 5000
          tokens: 50000000
          costs: 50.0
          window_secs: 86400
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let basic = &config.plan_settings.plans["basic"];
        assert_eq!(basic.window_limits.len(), 3);

        let w0 = &basic.window_limits[0];
        assert_eq!(w0.counts, Some(1000));
        assert_eq!(w0.tokens, None);
        assert_eq!(w0.costs, None);
        assert_eq!(w0.window_secs, 3600);

        let w1 = &basic.window_limits[1];
        assert_eq!(w1.counts, None);
        assert_eq!(w1.tokens, Some(5_000_000));
        assert!(w1.costs.is_some());
        assert_eq!(w1.window_secs, 3600);

        let w2 = &basic.window_limits[2];
        assert_eq!(w2.counts, None);
        assert_eq!(w2.tokens, None);
        assert!(w2.costs.is_some());
        assert_eq!(w2.window_secs, 60);

        // Verbose object form for team plan
        let team_plan = &config.plan_settings.plans["team_plan"];
        assert_eq!(team_plan.window_limits.len(), 1);
        let tw = &team_plan.window_limits[0];
        assert_eq!(tw.counts, Some(5000));
        assert_eq!(tw.tokens, Some(50_000_000));
        assert!(tw.costs.is_some());
        assert_eq!(tw.window_secs, 86400);
    }

    #[test]
    fn test_window_limit_legacy_pair_still_works_via_convenience_shorthand() {
        // The legacy convenience fields rpm_limit / tpm_limit are
        // independent of window_limits — they get merged in
        // boom-limiter::RateLimitPlan::effective_limits. Verify they still
        // parse and store as Option<u64>.
        let yaml = r#"
plan_settings:
  plans:
    legacy:
      rpm_limit: 60
      tpm_limit: 100000
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let legacy = &config.plan_settings.plans["legacy"];
        assert_eq!(legacy.rpm_limit, Some(60));
        assert_eq!(legacy.tpm_limit, Some(100_000));
        // No window_limits configured → empty vec.
        assert!(legacy.window_limits.is_empty());
    }

    #[test]
    fn test_set_yaml_path_overwrites_existing() {
        let yaml = "server:\n  host: 0.0.0.0\n  port: 4000\n";
        let mut value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        set_yaml_path(
            &mut value,
            &["server", "port"],
            serde_yaml::Value::Number(serde_yaml::Number::from(8080)),
        )
        .unwrap();
        let port = value
            .get("server")
            .unwrap()
            .get("port")
            .unwrap()
            .as_u64()
            .unwrap();
        assert_eq!(port, 8080);
        // Untouched field preserved.
        let host = value
            .get("server")
            .unwrap()
            .get("host")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(host, "0.0.0.0");
    }

    #[test]
    fn test_set_yaml_path_creates_missing_intermediate() {
        let mut value: serde_yaml::Value = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        set_yaml_path(
            &mut value,
            &["router_settings", "schedule_policy"],
            serde_yaml::Value::String("key_affinity".to_string()),
        )
        .unwrap();
        let policy = value
            .get("router_settings")
            .unwrap()
            .get("schedule_policy")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(policy, "key_affinity");
    }

    #[test]
    fn test_read_raw_yaml_preserves_env_refs() {
        // env var references like ${VAR} must survive the raw read; if they
        // were resolved, dumping the value back would leak real secrets.
        let dir = std::env::temp_dir();
        let path = dir.join("boom_config_test_raw.yaml");
        std::fs::write(
            &path,
            "model_list:\n  - model_name: gpt-4\n    litellm_params:\n      model: openai/gpt-4\n      api_key: ${OPENAI_API_KEY}\n",
        )
        .unwrap();

        let raw = read_raw_yaml(path.to_str().unwrap()).unwrap();
        let api_key = raw
            .get("model_list")
            .unwrap()
            .get(0)
            .unwrap()
            .get("litellm_params")
            .unwrap()
            .get("api_key")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(api_key, "${OPENAI_API_KEY}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_write_yaml_atomic_round_trip() {
        let dir = std::env::temp_dir();
        let path = dir.join("boom_config_test_atomic.yaml");
        let mut value: serde_yaml::Value = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        set_yaml_path(
            &mut value,
            &["server", "host"],
            serde_yaml::Value::String("127.0.0.1".to_string()),
        )
        .unwrap();
        write_yaml_atomic(path.to_str().unwrap(), &value).unwrap();

        // File should exist with expected content; no .tmp leftover.
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("127.0.0.1"));
        assert!(!std::path::Path::exists(path.with_extension("yaml.tmp").as_ref()));

        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod auto_router_config_tests {
    use super::*;

    #[test]
    fn ml_service_section_parses() {
        let yaml = r#"
model_name: auto
strategy: ml_service
default_tier: medium
tiers:
  small:  { target_model: small-cup }
  medium: { target_model: medium-cup }
  large:  { target_model: large-cup }
ml_service:
  url: http://127.0.0.1:2345
  timeout_ms: 50
"#;
        let cfg: AutoRouterConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.strategy, "ml_service");
        let ml = cfg.ml_service.expect("ml_service should be set");
        assert_eq!(ml.url, "http://127.0.0.1:2345");
        assert_eq!(ml.timeout_ms, 50);
    }
}
