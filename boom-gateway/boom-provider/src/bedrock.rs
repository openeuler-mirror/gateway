use boom_core::provider::Provider;
use boom_core::types::*;
use boom_core::GatewayError;
use async_trait::async_trait;
use reqwest::Client;

/// AWS Bedrock provider.
///
/// Bedrock requires AWS Signature V4 for authentication.
/// This skeleton provides the structure; AWS signing will be added in a follow-up.
#[allow(dead_code)]
pub struct BedrockProvider {
    client: Client,
    model: String,
    region: String,
    deployment_id: Option<String>,
}

impl BedrockProvider {
    pub fn new(client: Client, model: &str, region: &str, deployment_id: Option<String>) -> Self {
        Self {
            client,
            model: model.to_string(),
            region: region.to_string(),
            deployment_id,
        }
    }

    #[allow(dead_code)]
    fn invoke_url(&self) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/invoke",
            self.region, self.model
        )
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    async fn chat(&self, _req: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
        // TODO: Implement AWS SigV4 request signing.
        // This requires either the aws-sdk-bedrockruntime crate or a manual SigV4 implementation.
        Err(GatewayError::ProviderError(
            "Bedrock provider is not yet implemented. AWS SigV4 signing is required.".to_string(),
        ))
    }

    async fn chat_stream(&self, _req: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
        // TODO: Implement with streaming invoke endpoint.
        Err(GatewayError::ProviderError(
            "Bedrock streaming is not yet implemented.".to_string(),
        ))
    }

    fn name(&self) -> &str {
        "bedrock"
    }

    fn models(&self) -> &[String] {
        std::slice::from_ref(&self.model)
    }

    fn deployment_id(&self) -> Option<&str> {
        self.deployment_id.as_deref()
    }
}
