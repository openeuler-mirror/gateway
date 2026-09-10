use boom_core::provider::{Provider, ProviderProtocol};
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
    kv_worker_id: Option<String>,
    client_type_header: bool,
}

impl AzureProvider {
    pub fn new(
        client: Client,
        api_key: Option<String>,
        api_base: Option<String>,
        deployment: &str,
        api_version: &str,
        deployment_id: Option<String>,
        client_type_header: bool,
    ) -> Self {
        let base = api_base.unwrap_or_default();
        let kv_worker_id = crate::kv_worker_id_from_api_base(Some(base.as_str()));
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
            kv_worker_id,
            client_type_header,
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
        let gateway_headers = std::mem::take(&mut req.gateway_headers);
        let body = serde_json::to_value(&req)
            .map_err(|e| GatewayError::InternalError(format!("Serialize error: {}", e)))?;

        let mut builder = self.client.post(self.azure_url());
        if let Some(ref key) = self.api_key {
            builder = builder.header("api-key", key);
        }
        for (name, value) in &gateway_headers {
            builder = builder.header(name, value);
        }
        // Non-streaming: upstream sends no data until the entire response is ready.
        // Uses the reqwest Client timeout from deployment config (`create_provider`), not a separate 600s cap.

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
        let gateway_headers = std::mem::take(&mut req.gateway_headers);
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
        for (name, value) in &gateway_headers {
            builder = builder.header(name, value);
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

            loop {
                let chunk_result = tokio::select! {
                    _ = tx.closed() => return,
                    chunk_result = stream.next() => chunk_result,
                };
                let Some(chunk_result) = chunk_result else {
                    return;
                };
                match chunk_result {
                    Ok(bytes) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(pos) = buffer.find("\n\n") {
                            let event_text = buffer[..pos].to_string();
                            buffer = buffer[pos + 2..].to_string();

                            for line in event_text.lines() {
                                // SSE spec allows "data:payload" without a space;
                                // some upstreams always emit that form.
                                if let Some(data) = line.strip_prefix("data:") {
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

    fn protocol(&self) -> ProviderProtocol {
        ProviderProtocol::OpenAiCompatible
    }

    fn models(&self) -> &[String] {
        std::slice::from_ref(&self.deployment)
    }

    fn deployment_id(&self) -> Option<&str> {
        self.deployment_id.as_deref()
    }

    fn kv_worker_id(&self) -> Option<&str> {
        self.kv_worker_id.as_deref()
    }

    fn client_type_header(&self) -> bool {
        self.client_type_header
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn minimal_request() -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "test-dep".to_string(),
            messages: vec![Message {
                role: MessageRole::User,
                content: MessageContent::Text("hello".to_string()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            }],
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            max_tokens: None,
            max_completion_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            user: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            seed: None,
            logprobs: None,
            top_logprobs: None,
            logit_bias: None,
            extra: Default::default(),
            gateway_headers: HashMap::new(),
            kv_cache_report_full: false,
            raw_capture: None,
        }
    }

    /// Regression: the SSE spec allows `data:{...}` without a space after the
    /// colon; the streaming parser used to require `"data: "` and silently
    /// dropped every chunk from such upstreams.
    #[tokio::test]
    async fn chat_stream_data_prefix_without_space_parses() {
        let sse = concat!(
            "data:{\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data:[DONE]\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/test-dep/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = AzureProvider::new(
            Client::new(),
            None,
            Some(server.uri()),
            "test-dep",
            "",
            None,
            false,
        );
        let stream = provider
            .chat_stream(minimal_request())
            .await
            .expect("stream must start");
        let chunks: Vec<_> = stream
            .filter_map(|c| async move { c.ok() })
            .collect()
            .await;
        assert_eq!(chunks.len(), 1, "no-space data: chunk must not be dropped");
        assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("hi"));
    }
}
