pub mod anthropic;
pub mod azure;
pub mod bedrock;
pub mod gemini;
pub mod openai;

use boom_core::provider::Provider;
use boom_core::GatewayError;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;

/// Create a provider instance from litellm-style config params.
///
/// `model` follows litellm's `provider/model-id` convention:
///   - `openai/gpt-4` → provider=openai, model=gpt-4
///   - `gpt-4` → auto-detected as openai, model=gpt-4
///   - `anthropic/claude-sonnet-4-20250514` → provider=anthropic
///   - `hosted_vllm/my-model` → OpenAI-compatible provider
pub fn create_provider(
    model: &str,
    api_key: Option<String>,
    api_base: Option<String>,
    timeout: u64,
    extra: &HashMap<String, String>,
    deployment_id: Option<String>,
) -> Result<Arc<dyn Provider>, GatewayError> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .build()
        .map_err(|e| GatewayError::ConfigError(format!("Failed to create HTTP client: {}", e)))?;

    let (provider_type, actual_model) = parse_model_provider(model);

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
            Ok(Arc::new(openai::OpenAIProvider::new(
                client,
                key,
                api_base,
                &actual_model,
                deployment_id,
            )))
        }
        "anthropic" => {
            let mut provider = anthropic::AnthropicProvider::new(
                client,
                api_key,
                api_base,
                &actual_model,
                deployment_id,
            );
            if let Some(version) = extra.get("anthropic_version") {
                provider = provider.with_api_version(version.clone());
            }
            Ok(Arc::new(provider))
        }
        "azure" => {
            let api_version = extra.get("api_version").cloned().unwrap_or_default();
            Ok(Arc::new(azure::AzureProvider::new(
                client,
                api_key,
                api_base,
                &actual_model,
                &api_version,
                deployment_id,
            )))
        }
        "gemini" => Ok(Arc::new(gemini::GeminiProvider::new(
            client,
            api_key,
            &actual_model,
            deployment_id,
        ))),
        "bedrock" => {
            let region = extra
                .get("aws_region_name")
                .cloned()
                .unwrap_or_else(|| "us-east-1".to_string());
            Ok(Arc::new(bedrock::BedrockProvider::new(
                client,
                &actual_model,
                &region,
                deployment_id,
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
