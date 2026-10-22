pub mod anthropic;
pub mod azure;
pub mod bedrock;
pub mod gemini;
pub mod openai;
pub mod sse;

use boom_core::provider::Provider;
use boom_core::GatewayError;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;

/// Keys inside `litellm_params.headers` that historically doubled as provider
/// config knobs (the headers map used to be merged into the `extra` param
/// namespace). They keep their config meaning and are never sent as headers.
const RESERVED_PARAM_KEYS: &[&str] = &["api_version", "aws_region_name", "anthropic_version"];

/// Transport/framing headers a deployment must not override — customizing
/// these breaks request serialization or response parsing (JSON body,
/// chunked SSE).
const TRANSPORT_CRITICAL_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "host",
    "connection",
    "transfer-encoding",
    "accept",
    "accept-encoding",
    "te",
    "upgrade",
    "expect",
];

/// Filter a deployment's `litellm_params.headers` down to the entries that
/// may actually be attached to upstream requests. Drops (with a warn, once
/// at provider creation — zero per-request cost):
/// - reserved config keys (`api_version`, …) — config semantics, not headers
/// - spoof/auth hard-blocked names — see `boom_core::is_hard_blocked_header`
/// - transport-critical names (content-type, …)
/// - syntactically invalid header names/values
fn sanitize_custom_headers(
    provider_type: &str,
    headers: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, value) in headers {
        let lower = name.to_lowercase();
        if RESERVED_PARAM_KEYS.contains(&name.as_str()) {
            continue;
        }
        if boom_core::is_hard_blocked_header(&lower) {
            tracing::warn!(
                provider = provider_type,
                header = %name,
                "dropping deployment custom header: name is hard-blocked by gateway policy"
            );
            continue;
        }
        if TRANSPORT_CRITICAL_HEADERS.contains(&lower.as_str()) {
            tracing::warn!(
                provider = provider_type,
                header = %name,
                "dropping deployment custom header: transport-critical name"
            );
            continue;
        }
        let name_ok = reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_ok();
        let value_ok = reqwest::header::HeaderValue::from_str(value).is_ok();
        if !name_ok || !value_ok {
            tracing::warn!(
                provider = provider_type,
                header = %name,
                "dropping deployment custom header: invalid name or value"
            );
            continue;
        }
        out.push((name.clone(), value.clone()));
    }
    out
}

/// Apply the merged upstream-header side channel (`gateway_headers`) in one
/// shot. `RequestBuilder::headers` replaces same-named entries the provider
/// already set, so a deployment custom header can deliberately override a
/// provider default (e.g. `anthropic-beta`) without producing duplicate
/// values — the per-entry `RequestBuilder::header` appends and would.
pub(crate) fn apply_gateway_headers(
    builder: reqwest::RequestBuilder,
    headers: &HashMap<String, String>,
) -> reqwest::RequestBuilder {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        match (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            (Ok(n), Ok(v)) => {
                map.insert(n, v);
            }
            _ => tracing::warn!(header = %name, "dropping invalid upstream header entry"),
        }
    }
    builder.headers(map)
}

/// Create a provider instance from litellm-style config params.
///
/// `model` follows litellm's `provider/model-id` convention:
///   - `openai/gpt-4` → provider=openai, model=gpt-4
///   - `gpt-4` → auto-detected as openai, model=gpt-4
///   - `anthropic/claude-sonnet-4-20250514` → provider=anthropic
///   - `hosted_vllm/my-model` → OpenAI-compatible provider
///
/// `extra` carries provider config params (`api_version`, `aws_region_name`);
/// `custom_headers` carries the deployment's `litellm_params.headers` map,
/// sanitized here into headers actually attached to every upstream request.
pub fn create_provider(
    model: &str,
    api_key: Option<String>,
    api_base: Option<String>,
    timeout: u64,
    extra: &HashMap<String, String>,
    custom_headers: &HashMap<String, String>,
    deployment_id: Option<String>,
    client_type_header: bool,
) -> Result<Arc<dyn Provider>, GatewayError> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(timeout.max(1)))
        .build()
        .map_err(|e| GatewayError::ConfigError(format!("Failed to create HTTP client: {}", e)))?;

    let (provider_type, actual_model) = parse_model_provider(model);

    // Backward compatibility: reserved keys configured inside the headers
    // map used to reach `extra` through namespace merging and act as
    // provider config; keep honoring them, with typed fields (already in
    // `extra`) taking precedence.
    let mut merged_extra = extra.clone();
    for (k, v) in custom_headers {
        if RESERVED_PARAM_KEYS.contains(&k.as_str()) {
            merged_extra.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    let attached = sanitize_custom_headers(provider_type, custom_headers);

    match provider_type {
        // All OpenAI-compatible providers share the same API format.
        // They just use different api_base / api_key.
        "openai"
        | "hosted_vllm"
        | "vllm"
        | "ollama"
        | "ollama_chat"
        | "deepseek"
        | "groq"
        | "together_ai"
        | "fireworks_ai"
        | "perplexity"
        | "anyscale"
        | "deepinfra"
        | "lm_studio"
        | "llamafile"
        | "xinference"
        | "sambanova"
        | "cerebras"
        | "nvidia_nim"
        | "codestral"
        | "volcengine"
        | "dashscope"
        | "moonshot"
        | "xai"
        | "ai21"
        | "ai21_chat" => {
            // hosted_vllm/ollama etc. may not require an API key.
            // If none provided, use a placeholder so the provider still works.
            let key = api_key.or_else(|| {
                if provider_type != "openai" {
                    Some("fake-api-key".to_string())
                } else {
                    None
                }
            });
            Ok(Arc::new(
                openai::OpenAIProvider::new(
                    client,
                    key,
                    api_base,
                    &actual_model,
                    deployment_id,
                    client_type_header,
                )
                .with_custom_headers(attached),
            ))
        }
        "anthropic" => {
            let mut provider = anthropic::AnthropicProvider::new(
                client,
                api_key,
                api_base,
                &actual_model,
                deployment_id,
                client_type_header,
            )
            .with_custom_headers(attached);
            if let Some(version) = merged_extra.get("anthropic_version") {
                provider = provider.with_api_version(version.clone());
            }
            Ok(Arc::new(provider))
        }
        "azure" => {
            let api_version = merged_extra.get("api_version").cloned().unwrap_or_default();
            Ok(Arc::new(azure::AzureProvider::new(
                client,
                api_key,
                api_base,
                &actual_model,
                &api_version,
                deployment_id,
                client_type_header,
            )
            .with_custom_headers(attached)))
        }
        "gemini" => Ok(Arc::new(gemini::GeminiProvider::new(
            client,
            api_key,
            &actual_model,
            deployment_id,
            client_type_header,
        )
        .with_custom_headers(attached))),
        "bedrock" => {
            let region = merged_extra
                .get("aws_region_name")
                .cloned()
                .unwrap_or_else(|| "us-east-1".to_string());
            Ok(Arc::new(bedrock::BedrockProvider::new(
                client,
                &actual_model,
                &region,
                deployment_id,
                client_type_header,
            )))
        }
        _ => Err(GatewayError::ConfigError(format!(
            "Unknown provider: '{}'. Supported: openai, anthropic, azure, gemini, bedrock, hosted_vllm, vllm, ollama, deepseek, groq, etc.",
            provider_type
        ))),
    }
}

/// Parse `provider/model-id` format. Returns (provider, actual_model).
fn parse_model_provider(model: &str) -> (&str, String) {
    if let Some((provider, rest)) = model.split_once('/') {
        (provider, rest.to_string())
    } else {
        let provider = auto_detect_provider(model);
        (provider, model.to_string())
    }
}

/// Auto-detect provider from model name when no explicit prefix is given.
fn auto_detect_provider(model: &str) -> &'static str {
    let lower = model.to_lowercase();
    if lower.starts_with("gpt-")
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
        tracing::warn!(
            "Cannot auto-detect provider for '{}', defaulting to openai",
            model
        );
        "openai"
    }
}

/// Derive the KV worker identity for a deployment: the deployment id
/// (`model_info.id`), or nothing — there is deliberately NO api_base-derived
/// fallback.
///
/// Tier 1 — participation gate: no usable `api_base` → `None`. Hosted
/// endpoints without an api_base (Bedrock/Gemini form) never join kvc, so
/// the participant set stays identical to the old api_base-only derivation.
/// Tier 2 — deployment id: a non-empty (trimmed) id is used verbatim as the
/// worker identity, keying the trie in the same domain as load / flow
/// control / logging (`deployment_id`), and distinguishing deployments that
/// share an api_base host (same IP different port, or path-routed inference
/// gateways). **Id uniqueness is assumed, not enforced** — duplicate-id
/// validation is future work.
///
/// No id → `None` (not a kvc participant). Every DB insert path (seed /
/// create / import) auto-generates a UUID for an empty deployment_id, so a
/// gateway running against the DB never sees one; an entry that still has
/// none simply degrades to non-participation (load-based routing) rather
/// than a wrong identity — a host-derived id would silently merge distinct
/// instances behind the same host.
///
/// Examples:
///   (`dep-1`, `http://10.0.0.5:8000/v1`) → `dep-1`
///   (None,    `http://10.0.0.5:8000/v1`) → `None` (no id → not a participant)
///   (`dep-1`, None)                      → `None` (gate: no api_base)
pub fn kv_worker_id_for(deployment_id: Option<&str>, api_base: Option<&str>) -> Option<String> {
    if api_base.map(str::trim).filter(|s| !s.is_empty()).is_none() {
        return None;
    }
    deployment_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Helper: build a default OpenAI-compatible response ID.
pub(crate) fn generate_response_id() -> String {
    format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
}

/// Helper: get current unix timestamp.
pub(crate) fn now_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::create_provider;
    use boom_core::provider::{Provider, ProviderProtocol};
    use std::collections::HashMap;

    #[test]
    fn provider_protocol_comes_from_the_created_provider() {
        let create = |model: &str| {
            create_provider(
                model,
                Some("test-key".to_string()),
                Some("http://127.0.0.1:1/v1".to_string()),
                1,
                &HashMap::new(),
                &HashMap::new(),
                None,
                false,
            )
            .unwrap()
        };

        for model in [
            "openai/test-model",
            "hosted_vllm/test-model",
            "azure/test-deployment",
        ] {
            assert_eq!(create(model).protocol(), ProviderProtocol::OpenAiCompatible);
        }
        for model in [
            "anthropic/test-model",
            "gemini/test-model",
            "bedrock/test-model",
        ] {
            assert_eq!(create(model).protocol(), ProviderProtocol::Native);
        }
    }

    #[test]
    fn custom_headers_survive_sanitization_and_attach_to_provider() {
        let mut headers = HashMap::new();
        headers.insert("X-Request-Id".to_string(), "deploy-1".to_string());
        // Reserved config key → config semantics, never sent as header.
        headers.insert("api_version".to_string(), "2024-02-01".to_string());
        // Hard-blocked → dropped.
        headers.insert("x-gateway-priority".to_string(), "spoof".to_string());
        headers.insert("Authorization".to_string(), "Bearer leak".to_string());
        // Transport-critical → dropped.
        headers.insert("content-type".to_string(), "text/plain".to_string());
        // Invalid header name (space) → dropped.
        headers.insert("bad name".to_string(), "x".to_string());

        let provider = create_provider(
            "openai/test-model",
            Some("test-key".to_string()),
            Some("http://127.0.0.1:1/v1".to_string()),
            1,
            &HashMap::new(),
            &headers,
            None,
            false,
        )
        .unwrap();
        let attached = provider.custom_headers();
        assert_eq!(attached.len(), 1, "only the ordinary header survives");
        assert_eq!(attached[0].0, "X-Request-Id");
        assert_eq!(attached[0].1, "deploy-1");
    }

    #[test]
    fn reserved_key_in_headers_map_still_configures_provider() {
        // Backward compat: anthropic_version inside the headers map keeps
        // its historical config meaning (never sent as a header).
        let mut headers = HashMap::new();
        headers.insert("anthropic_version".to_string(), "2023-01-01".to_string());

        let provider = create_provider(
            "anthropic/test-model",
            None,
            Some("http://127.0.0.1:1".to_string()),
            1,
            &HashMap::new(),
            &headers,
            None,
            false,
        )
        .unwrap();
        assert!(
            provider.custom_headers().is_empty(),
            "reserved key must not become an upstream header"
        );
    }
}
