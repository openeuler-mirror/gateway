use boom_core::provider::Provider;
use boom_core::types::*;
use boom_core::GatewayError;
use async_trait::async_trait;
use futures::stream::StreamExt;
use reqwest::Client;
use tokio_stream::wrappers::ReceiverStream;

/// OpenAI provider — also serves as the base for Azure (same API format).
pub struct OpenAIProvider {
    client: Client,
    api_key: Option<String>,
    base_url: String,
    model: String,
    deployment_id: Option<String>,
}

impl OpenAIProvider {
    pub fn new(
        client: Client,
        api_key: Option<String>,
        api_base: Option<String>,
        model: &str,
        deployment_id: Option<String>,
    ) -> Self {
        Self {
            client,
            api_key,
            base_url: api_base
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            model: model.to_string(),
            deployment_id,
        }
    }

    fn build_request(&self, mut req: ChatCompletionRequest) -> serde_json::Value {
        // Replace model name with the actual provider model ID.
        req.model = self.model.clone();
        // Convert internal ContentPart::Reasoning to ContentPart::Text so that
        // upstream OpenAI-compatible APIs don't reject the unknown "reasoning" type.
        for msg in &mut req.messages {
            if let MessageContent::Parts(parts) = &mut msg.content {
                for part in parts.iter_mut() {
                    if let ContentPart::Reasoning { reasoning } = part {
                        *part = ContentPart::Text { text: std::mem::take(reasoning) };
                    }
                }
            }
        }
        // Serialize — skip_serializing on `extra` ensures non-standard fields
        // (service_tier, store, etc.) are NOT forwarded to upstream providers.
        serde_json::to_value(&req).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn chat(&self, req: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
        let body = self.build_request(req);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut builder = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            builder = builder.bearer_auth(key);
        }
        // Non-streaming: upstream sends no data until the entire response is ready.
        // Override timeout to 10 minutes — the client-level timeout (typically 30-60s)
        // would abort the request during model generation.
        builder = builder.timeout(std::time::Duration::from_secs(600));

        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("OpenAI request failed: {}", e);
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
                tracing::error!("Failed to parse OpenAI response: {}", e);
                GatewayError::ProviderError("Failed to process upstream response".to_string())
            })
    }

    async fn chat_stream(&self, req: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
        let mut body = self.build_request(req);
        // Ensure stream is enabled and request usage in the final chunk.
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".to_string(), serde_json::Value::Bool(true));
            obj.insert(
                "stream_options".to_string(),
                serde_json::json!({ "include_usage": true }),
            );
        }

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut builder = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            builder = builder.bearer_auth(key);
        }

        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("OpenAI stream request failed: {}", e);
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

        // Parse SSE byte stream into ChatStreamChunk items.
        let (tx, rx) = tokio::sync::mpsc::channel(32);

        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();

            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(bytes) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        // Process complete SSE lines.
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
                        tracing::error!("OpenAI stream read error: {}", e);
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

        let stream = ReceiverStream::new(rx).filter_map(|result| async move {
            match result {
                Ok(Some(chunk)) => Some(Ok(chunk)),
                Ok(None) => None, // [DONE]
                Err(e) => Some(Err(e)),
            }
        });

        Ok(Box::pin(stream))
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn models(&self) -> &[String] {
        std::slice::from_ref(&self.model)
    }

    fn deployment_id(&self) -> Option<&str> {
        self.deployment_id.as_deref()
    }
}
