use boom_core::provider::{Provider, ProviderProtocol};
use boom_core::types::*;
use boom_core::GatewayError;
use async_trait::async_trait;
use futures::stream::StreamExt;
use reqwest::Client;
use std::collections::BTreeMap;
use tokio_stream::wrappers::ReceiverStream;

/// Assemble a complete ChatCompletionResponse from an SSE body. Used when a
/// non-standard upstream answers a non-streaming request with
/// `text/event-stream` chunks anyway: `data:` payloads are ChatStreamChunks
/// whose deltas are concatenated per choice, and the final usage-only chunk
/// (if any) becomes the response usage.
fn assemble_sse_response(text: &str) -> Result<ChatCompletionResponse, String> {
    #[derive(Default)]
    struct AssembledToolCall {
        id: String,
        call_type: String,
        name: String,
        arguments: String,
    }
    #[derive(Default)]
    struct AssembledChoice {
        role: Option<MessageRole>,
        content: String,
        reasoning: String,
        finish_reason: Option<String>,
        tool_calls: BTreeMap<u32, AssembledToolCall>,
    }

    let mut id = String::new();
    let mut created = 0u64;
    let mut model = String::new();
    let mut choices: BTreeMap<u32, AssembledChoice> = BTreeMap::new();
    let mut usage: Option<Usage> = None;
    let mut saw_chunk = false;

    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let chunk: ChatStreamChunk = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(e) => {
                tracing::warn!("Skipping unparseable SSE chunk in non-stream response: {}", e);
                continue;
            }
        };
        saw_chunk = true;
        if id.is_empty() && !chunk.id.is_empty() {
            id = chunk.id;
        }
        if created == 0 {
            created = chunk.created;
        }
        if model.is_empty() && !chunk.model.is_empty() {
            model = chunk.model;
        }
        for choice in chunk.choices {
            let entry = choices.entry(choice.index).or_default();
            if let Some(role) = choice.delta.role {
                entry.role.get_or_insert(role);
            }
            if let Some(content) = choice.delta.content {
                entry.content.push_str(&content);
            }
            if let Some(reasoning) = choice.delta.reasoning_content {
                entry.reasoning.push_str(&reasoning);
            }
            if let Some(calls) = choice.delta.tool_calls {
                for call in calls {
                    let tc = entry.tool_calls.entry(call.index).or_default();
                    if let Some(v) = call.id {
                        if tc.id.is_empty() {
                            tc.id = v;
                        }
                    }
                    if let Some(v) = call.call_type {
                        if tc.call_type.is_empty() {
                            tc.call_type = v;
                        }
                    }
                    if let Some(func) = call.function {
                        if let Some(v) = func.name {
                            if tc.name.is_empty() {
                                tc.name = v;
                            }
                        }
                        if let Some(v) = func.arguments {
                            tc.arguments.push_str(&v);
                        }
                    }
                }
            }
            if choice.finish_reason.is_some() {
                entry.finish_reason = choice.finish_reason;
            }
        }
        if let Some(stream_usage) = chunk.usage {
            usage = Some(Usage {
                prompt_tokens: stream_usage.prompt_tokens.unwrap_or(0).max(0) as u32,
                completion_tokens: stream_usage.completion_tokens.unwrap_or(0).max(0) as u32,
                total_tokens: stream_usage.total_tokens.unwrap_or(0).max(0) as u32,
                prompt_tokens_details: stream_usage.prompt_tokens_details,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            });
        }
    }

    if !saw_chunk {
        return Err("SSE response contained no parseable data chunks".to_string());
    }

    let choices = choices
        .into_iter()
        .map(|(index, c)| Choice {
            index,
            message: Message {
                role: c.role.unwrap_or(MessageRole::Assistant),
                content: MessageContent::Text(c.content),
                name: None,
                tool_calls: if c.tool_calls.is_empty() {
                    None
                } else {
                    Some(
                        c.tool_calls
                            .into_values()
                            .map(|tc| ToolCall {
                                id: tc.id,
                                call_type: if tc.call_type.is_empty() {
                                    "function".to_string()
                                } else {
                                    tc.call_type
                                },
                                function: FunctionCall {
                                    name: tc.name,
                                    arguments: tc.arguments,
                                },
                            })
                            .collect(),
                    )
                },
                tool_call_id: None,
                reasoning_content: (!c.reasoning.is_empty()).then_some(c.reasoning),
            },
            finish_reason: c.finish_reason,
            logprobs: None,
        })
        .collect();

    Ok(ChatCompletionResponse {
        id,
        object: "chat.completion".to_string(),
        created,
        model,
        choices,
        usage,
        system_fingerprint: None,
        raw_response: None,
    })
}

/// OpenAI provider — also serves as the base for Azure (same API format).
pub struct OpenAIProvider {
    client: Client,
    api_key: Option<String>,
    base_url: String,
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
    ) -> Self {
        let kv_worker_id = crate::kv_worker_id_from_api_base(api_base.as_deref());
        Self {
            client,
            api_key,
            base_url: api_base
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
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
        let raw_capture = std::mem::take(&mut req.raw_capture);
        let body = self.build_request(req);
        // Record the exact serialized bytes — reqwest's .json() serializes
        // the same Value with the same serializer, so this is byte-identical
        // to what goes on the wire.
        if let Some(ref cap) = raw_capture {
            cap.record_request_body(&body.to_string());
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
                tracing::error!("OpenAI request failed: {}", e);
                GatewayError::ProviderError("Upstream provider unavailable".to_string())
            })?;

        let status = resp.status();
        if !status.is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            if let Some(ref cap) = raw_capture {
                cap.push_response_frame(error_body.clone());
            }
            return Err(GatewayError::UpstreamError {
                status: status.as_u16(),
                message: error_body,
            });
        }

        let raw_text = resp.text().await.map_err(|e| {
            tracing::error!("Failed to read OpenAI response body: {}", e);
            GatewayError::ProviderError("Failed to read upstream response".to_string())
        })?;
        if let Some(ref cap) = raw_capture {
            cap.push_response_frame(raw_text.clone());
        }

        // Tolerate a UTF-8 BOM: some upstreams prepend one invisibly (curl
        // output looks fine) and serde_json fails with "expected value at
        // line 1 column 1". Stripped for parsing only — raw_response and the
        // prompt-log raw body keep the bytes as sent.
        let parse_input = raw_text.strip_prefix('\u{feff}').unwrap_or(&raw_text);
        let mut parsed: ChatCompletionResponse = if parse_input.trim_start().starts_with("data:") {
            // Non-standard upstream answered a non-streaming request with an
            // SSE stream — assemble the chunks into a complete response.
            match assemble_sse_response(parse_input) {
                Ok(assembled) => assembled,
                Err(e) => {
                    tracing::error!("Failed to assemble SSE response for non-stream request: {}", e);
                    return Err(GatewayError::UpstreamParseError {
                        parse_error: e,
                        raw_body: raw_text,
                    });
                }
            }
        } else {
            match serde_json::from_str(parse_input) {
                Ok(parsed) => parsed,
                Err(e) => {
                    tracing::error!("Failed to parse OpenAI response: {}", e);
                    return Err(GatewayError::UpstreamParseError {
                        parse_error: e.to_string(),
                        raw_body: raw_text,
                    });
                }
            }
        };

        parsed.raw_response = Some(raw_text);
        Ok(parsed)
    }

    async fn chat_stream(&self, mut req: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
        // Take gateway-internal headers out before the request is serialized.
        let gateway_headers = std::mem::take(&mut req.gateway_headers);
        let raw_capture = std::mem::take(&mut req.raw_capture);
        let mut body = self.build_request(req);
        if let Some(ref cap) = raw_capture {
            cap.record_request_body(&body.to_string());
        }
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
            if let Some(ref cap) = raw_capture {
                cap.push_response_frame(error_body.clone());
            }
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

            loop {
                let chunk_result = tokio::select! {
                    _ = tx.closed() => return,
                    chunk_result = stream.next() => chunk_result,
                };
                let Some(chunk_result) = chunk_result else {
                    // Upstream ended. Flush any partial (unterminated) frame
                    // so the raw capture shows exactly where truncation hit.
                    if let Some(ref cap) = raw_capture {
                        if !buffer.is_empty() {
                            cap.push_response_frame(std::mem::take(&mut buffer));
                        }
                    }
                    return;
                };
                match chunk_result {
                    Ok(bytes) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        // Process complete SSE lines.
                        while let Some(pos) = buffer.find("\n\n") {
                            let event_text = buffer[..pos].to_string();
                            buffer = buffer[pos + 2..].to_string();
                            // Capture the raw frame before any parsing —
                            // keeps event:/data: prefixes and frames that
                            // fail JSON parse.
                            if let Some(ref cap) = raw_capture {
                                cap.push_response_frame(event_text.clone());
                            }

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

    fn protocol(&self) -> ProviderProtocol {
        ProviderProtocol::OpenAiCompatible
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use wiremock::matchers::{body_partial_json, method, path, header};
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
            raw_capture: None,
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
        OpenAIProvider::new(Client::new(), api_key, Some(uri), "test-model", None, false)
    }

    /// Extra fields clients send (`reasoning_effort`, `service_tier`, etc.)
    /// must reach the upstream OpenAI-compatible body as-is — the gateway does
    /// not whitelist the OpenAI protocol field set.
    #[tokio::test]
    async fn extra_fields_are_forwarded_to_upstream_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(serde_json::json!({
                "reasoning_effort": "high",
                "service_tier": "auto",
                "prompt_cache_key": "pc-1",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(fake_completion_response()))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let mut req = request_with_headers(&[]);
        req.extra.insert(
            "reasoning_effort".to_string(),
            serde_json::json!("high"),
        );
        req.extra.insert(
            "service_tier".to_string(),
            serde_json::json!("auto"),
        );
        req.extra.insert(
            "prompt_cache_key".to_string(),
            serde_json::json!("pc-1"),
        );
        let _ = provider.chat(req).await.unwrap();
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

    /// Regression: non-standard upstreams serving only /chat/completions may
    /// omit `usage`. The response must still parse (usage = None, token
    /// accounting skipped) instead of failing the whole request.
    #[tokio::test]
    async fn chat_response_without_usage_parses() {
        let server = MockServer::start().await;
        let mut body = fake_completion_response();
        body.as_object_mut().unwrap().remove("usage");
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let resp = provider.chat(request_with_headers(&[])).await.expect("missing usage must not fail parsing");
        assert!(resp.usage.is_none());
    }

    /// Parse failures must carry the raw body and the serde detail up to the
    /// route layer so the prompt log records exactly what the upstream sent.
    #[tokio::test]
    async fn chat_unparseable_response_carries_raw_body() {
        let server = MockServer::start().await;
        let body = "{\"not\":\"a valid completion\"}";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let err = provider.chat(request_with_headers(&[])).await.unwrap_err();
        match err {
            GatewayError::UpstreamParseError { parse_error, raw_body } => {
                assert!(parse_error.contains("missing field"), "unexpected parse error: {parse_error}");
                assert_eq!(raw_body, body);
            }
            other => panic!("expected UpstreamParseError, got: {other:?}"),
        }
    }

    /// A UTF-8 BOM before the JSON (invisible in terminal curl output, but
    /// serde_json fails at line 1 column 1) must be tolerated.
    #[tokio::test]
    async fn chat_response_with_utf8_bom_parses() {
        let server = MockServer::start().await;
        let body = format!("\u{feff}{}", fake_completion_response());
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let resp = provider.chat(request_with_headers(&[])).await.expect("BOM-prefixed JSON must parse");
        assert!(resp.usage.is_some());
    }

    /// Non-standard upstreams may answer a non-streaming request with an SSE
    /// stream — the chunks must be assembled into a complete response.
    #[tokio::test]
    async fn chat_sse_answer_to_non_stream_request_is_assembled() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"test-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let resp = provider.chat(request_with_headers(&[])).await.expect("SSE answer must be assembled");
        assert_eq!(resp.id, "chatcmpl-1");
        assert_eq!(resp.object, "chat.completion");
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert!(matches!(
            &resp.choices[0].message.content,
            MessageContent::Text(text) if text == "Hello"
        ));
        let usage = resp.usage.expect("usage from final chunk");
        assert_eq!(usage.prompt_tokens, 5);
        assert_eq!(usage.total_tokens, 7);
    }

    /// Passthrough identity fields (id/object/created) are not load-bearing;
    /// a backend returning bare choices must still parse.
    #[tokio::test]
    async fn chat_response_without_passthrough_fields_parses() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop"
            }]
        });
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let resp = provider.chat(request_with_headers(&[])).await.expect("missing passthrough fields must not fail parsing");
        assert!(matches!(
            &resp.choices[0].message.content,
            MessageContent::Text(text) if text == "hi"
        ));
    }

    /// Stream chunks missing non-semantic fields (object) and the usage-only
    /// final chunk missing `choices` must not be silently dropped.
    #[tokio::test]
    async fn stream_chunks_with_missing_fields_are_not_dropped() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"1\",\"created\":0,\"model\":\"m\",\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider_for(server.uri(), None);
        let stream = provider.chat_stream(request_with_headers(&[])).await.expect("stream must start");
        let chunks: Vec<_> = stream
            .filter_map(|c| async move { c.ok() })
            .collect()
            .await;
        assert_eq!(chunks.len(), 2, "both chunks must survive parsing");
        assert_eq!(
            chunks[0].choices[0].delta.content.as_deref(),
            Some("hi")
        );
        assert_eq!(chunks[1].usage.as_ref().unwrap().prompt_tokens, Some(5));
    }
}
