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
    /// Dedicated endpoint for vLLM `/tokenize` (kvc_aware routing). None → use
    /// `base_url`. Set when `api_base` points at a PD router that doesn't
    /// forward `/tokenize`; e.g. base_url=router:8100, tokenize_base=vLLM:7100.
    tokenize_base_url: Option<String>,
    model: String,
    deployment_id: Option<String>,
    kv_worker_id: Option<String>,
    client_type_header: bool,
}

impl OpenAIProvider {
    pub fn new(
        client: Client,
        api_key: Option<String>,
        api_base: Option<String>,
        model: &str,
        deployment_id: Option<String>,
        client_type_header: bool,
        tokenize_api_base: Option<String>,
    ) -> Self {
        let kv_worker_id = crate::kv_worker_id_from_api_base(api_base.as_deref());
        Self {
            client,
            api_key,
            base_url: api_base
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            tokenize_base_url: tokenize_api_base,
            model: model.to_string(),
            deployment_id,
            kv_worker_id,
            client_type_header,
        }
    }

    fn build_request(&self, mut req: ChatCompletionRequest) -> serde_json::Value {
        // Extract internal flags before serialization (they are skip_serializing).
        let kv_cache_report_full = req.kv_cache_report_full;

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
        let mut body = serde_json::to_value(&req).unwrap_or_default();

        // Inject vllm_xargs for KV cache full reporting when requested by gateway.
        if kv_cache_report_full {
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "vllm_xargs".to_string(),
                    serde_json::json!({ "kv_cache_report_mode": "full" }),
                );
            }
        }

        body
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn chat(&self, mut req: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
        // Take gateway-internal headers out before the request is serialized.
        let gateway_headers = std::mem::take(&mut req.gateway_headers);
        let body = self.build_request(req);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut builder = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            builder = builder.bearer_auth(key);
        }
        for (name, value) in &gateway_headers {
            builder = builder.header(name, value);
        }

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

        let raw_text = resp.text().await.map_err(|e| {
            tracing::error!("Failed to read OpenAI response body: {}", e);
            GatewayError::ProviderError("Failed to read upstream response".to_string())
        })?;

        let mut parsed: ChatCompletionResponse = serde_json::from_str(&raw_text).map_err(|e| {
            tracing::error!("Failed to parse OpenAI response: {}", e);
            GatewayError::ProviderError("Failed to process upstream response".to_string())
        })?;

        parsed.raw_response = Some(raw_text);
        Ok(parsed)
    }

    async fn chat_stream(&self, mut req: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
        // Take gateway-internal headers out before the request is serialized.
        let gateway_headers = std::mem::take(&mut req.gateway_headers);
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
        for (name, value) in &gateway_headers {
            builder = builder.header(name, value);
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
                                        Ok(mut chunk) => {
                                            chunk.raw_data = Some(data.to_string());
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

    fn kv_worker_id(&self) -> Option<&str> {
        self.kv_worker_id.as_deref()
    }

    fn client_type_header(&self) -> bool {
        self.client_type_header
    }

    /// Tokenize via the upstream's `/tokenize` endpoint (vLLM applies the chat
    /// template + content-format normalization identically to a real chat
    /// completion, so the returned ids are the exact prefill tokens). Used for
    /// KV-cache-aware routing so the gateway's prefix query matches the trie.
    async fn tokenize_prompt(
        &self,
        req: &ChatCompletionRequest,
    ) -> Result<Vec<u32>, GatewayError> {
        // Minimal body: vLLM's TokenizeChatRequest wants messages/tools +
        // add_generation_prompt. Use the deployment's upstream model name so
        // vLLM recognizes it (same as build_request forwarding). Include tools
        // when present: the chat template renders tool definitions into the
        // prompt, so /tokenize must include them to match the actual prefill.
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": req.messages,
            "add_generation_prompt": true,
        });
        if let Some(tools) = &req.tools {
            if !tools.is_empty() {
                if let Ok(v) = serde_json::to_value(tools) {
                    body["tools"] = v;
                }
            }
        }
        // vLLM mounts `/tokenize` at the server ROOT (not under `/v1`). If a
        // dedicated `tokenize_base_url` is configured (e.g. the vLLM direct
        // port when `api_base` is a PD router that doesn't forward /tokenize),
        // use it; otherwise derive from `base_url` by stripping `/v1`.
        let base = self
            .tokenize_base_url
            .as_deref()
            .unwrap_or(self.base_url.as_str());
        let root = base.trim_end_matches('/');
        let root = root.strip_suffix("/v1").unwrap_or(root);
        let url = format!("{root}/tokenize");
        let mut builder = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            builder = builder.bearer_auth(key);
        }
        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!("upstream /tokenize request failed: {e}");
                GatewayError::ProviderError("upstream /tokenize unavailable".to_string())
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(GatewayError::ProviderError(format!(
                "upstream /tokenize status {status}"
            )));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| {
            GatewayError::ProviderError(format!("upstream /tokenize bad body: {e}"))
        })?;
        let tokens: Vec<u32> = v
            .get("tokens")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_u64().map(|n| n as u32))
                    .collect()
            })
            .unwrap_or_default();
        if tokens.is_empty() {
            return Err(GatewayError::ProviderError(
                "upstream /tokenize returned no tokens".to_string(),
            ));
        }
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use wiremock::matchers::{method, path, header};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a minimal request, optionally carrying gateway-internal headers.
    fn request_with_headers(headers: &[(&str, &str)]) -> ChatCompletionRequest {
        let mut gateway_headers = HashMap::new();
        for (k, v) in headers {
            gateway_headers.insert(k.to_string(), v.to_string());
        }
        ChatCompletionRequest {
            model: "test-model".to_string(),
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
            gateway_headers,
            kv_cache_report_full: false,
        }
    }

    fn fake_completion_response() -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1700000000_u64,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "hi"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 1,
                "total_tokens": 6
            }
        })
    }

    fn provider_for(uri: String, api_key: Option<String>) -> OpenAIProvider {
        OpenAIProvider::new(Client::new(), api_key, Some(uri), "test-model", None, false, None)
    }

    #[tokio::test]
    async fn chat_injects_gateway_header_vip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("X-Gateway-Priority", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_response()))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let req = request_with_headers(&[("X-Gateway-Priority", "100")]);
        assert!(provider.chat(req).await.is_ok());
    }

    #[tokio::test]
    async fn chat_injects_gateway_header_normal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("X-Gateway-Priority", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_response()))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let req = request_with_headers(&[("X-Gateway-Priority", "0")]);
        assert!(provider.chat(req).await.is_ok());
    }

    #[tokio::test]
    async fn chat_injects_custom_priority_value() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("X-Gateway-Priority", "42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_response()))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let req = request_with_headers(&[("X-Gateway-Priority", "42")]);
        assert!(provider.chat(req).await.is_ok());
    }

    #[tokio::test]
    async fn chat_stream_injects_gateway_header() {
        let sse_body = "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("X-Gateway-Priority", "100"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let req = request_with_headers(&[("X-Gateway-Priority", "100")]);
        assert!(provider.chat_stream(req).await.is_ok());
    }

    #[tokio::test]
    async fn chat_includes_bearer_auth_alongside_gateway_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("Authorization", "Bearer sk-test-key"))
            .and(header("X-Gateway-Priority", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_response()))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), Some("sk-test-key".to_string()));
        let req = request_with_headers(&[("X-Gateway-Priority", "100")]);
        assert!(provider.chat(req).await.is_ok());
    }

    /// When gateway_headers is empty (e.g. priority injection disabled), the
    /// upstream request must NOT carry any X-Gateway-Priority header.
    #[tokio::test]
    async fn chat_without_gateway_headers_sends_no_priority_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_response()))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let req = request_with_headers(&[]);
        assert!(provider.chat(req).await.is_ok());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0].headers.contains_key("X-Gateway-Priority"),
            "no X-Gateway-Priority header should be sent when gateway_headers is empty"
        );
    }
}
