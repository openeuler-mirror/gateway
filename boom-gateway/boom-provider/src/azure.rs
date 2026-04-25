use boom_core::provider::Provider;
use boom_core::types::*;
use boom_core::GatewayError;
use async_trait::async_trait;
use futures::stream::StreamExt;
use reqwest::Client;

/// Azure OpenAI provider.
///
/// Same API format as OpenAI, but different URL pattern and auth header.
pub struct AzureProvider {
    client: Client,
    api_key: Option<String>,
    deployment: String,
    api_version: String,
    base_url: String,
    deployment_id: Option<String>,
}

impl AzureProvider {
    pub fn new(
        client: Client,
        api_key: Option<String>,
        api_base: Option<String>,
        deployment: &str,
        api_version: &str,
        deployment_id: Option<String>,
    ) -> Self {
        let base = api_base.unwrap_or_default();
        Self {
            client,
            api_key,
            deployment: deployment.to_string(),
            api_version: if api_version.is_empty() {
                "2024-02-01".to_string()
            } else {
                api_version.to_string()
            },
            base_url: base,
            deployment_id,
        }
    }

    fn azure_url(&self) -> String {
        format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.base_url.trim_end_matches('/'),
            self.deployment,
            self.api_version,
        )
    }
}

#[async_trait]
impl Provider for AzureProvider {
    async fn chat(&self, mut req: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
        req.model = self.deployment.clone();
        let body = serde_json::to_value(&req)
            .map_err(|e| GatewayError::InternalError(format!("Serialize error: {}", e)))?;

        let mut builder = self.client.post(self.azure_url());
        if let Some(ref key) = self.api_key {
            builder = builder.header("api-key", key);
        }
        builder = builder.timeout(std::time::Duration::from_secs(600));

        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Azure request failed: {}", e);
                GatewayError::ProviderError("Upstream provider unavailable".to_string())
            })?;

        let status = resp.status();
        if !status.is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            return Err(GatewayError::UpstreamError {
                status: status.as_u16(),
                message: error_body,
            });
        }

        resp.json::<ChatCompletionResponse>()
            .await
            .map_err(|e| {
                tracing::error!("Failed to parse Azure response: {}", e);
                GatewayError::ProviderError("Failed to process upstream response".to_string())
            })
    }

    async fn chat_stream(&self, mut req: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
        req.model = self.deployment.clone();
        let mut body = serde_json::to_value(&req)
            .map_err(|e| GatewayError::InternalError(format!("Serialize error: {}", e)))?;

        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".to_string(), serde_json::Value::Bool(true));
            obj.insert(
                "stream_options".to_string(),
                serde_json::json!({ "include_usage": true }),
            );
        }

        let mut builder = self.client.post(self.azure_url());
        if let Some(ref key) = self.api_key {
            builder = builder.header("api-key", key);
        }

        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Azure stream request failed: {}", e);
                GatewayError::ProviderError("Upstream provider unavailable".to_string())
            })?;

        let status = resp.status();
        if !status.is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            return Err(GatewayError::UpstreamError {
                status: status.as_u16(),
                message: error_body,
            });
        }

        let (tx, rx) = tokio::sync::mpsc::channel(32);

        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();

            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(bytes) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(pos) = buffer.find("\n\n") {
                            let event_text = buffer[..pos].to_string();
                            buffer = buffer[pos + 2..].to_string();

                            for line in event_text.lines() {
                                if let Some(data) = line.strip_prefix("data: ") {
                                    let data = data.trim();
                                    if data == "[DONE]" {
                                        let _ = tx.send(Ok(None)).await;
                                        return;
                                    }
                                    match serde_json::from_str::<ChatStreamChunk>(data) {
                                        Ok(chunk) => {
                                            if tx.send(Ok(Some(chunk))).await.is_err() {
                                                return;
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("Failed to parse SSE chunk: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Azure stream read error: {}", e);
                        let _ = tx
                            .send(Err(GatewayError::ProviderError(
                                "Upstream stream error".to_string(),
                            )))
                            .await;
                        return;
                    }
                }
            }
        });

        let stream = tokio_stream::wrappers::ReceiverStream::new(rx).filter_map(|result| async move {
            match result {
                Ok(Some(chunk)) => Some(Ok(chunk)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            }
        });

        Ok(Box::pin(stream))
    }

    fn name(&self) -> &str {
        "azure"
    }

    fn models(&self) -> &[String] {
        std::slice::from_ref(&self.deployment)
    }

    fn deployment_id(&self) -> Option<&str> {
        self.deployment_id.as_deref()
    }
}
