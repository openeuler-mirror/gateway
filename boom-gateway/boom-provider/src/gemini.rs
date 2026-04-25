use crate::{generate_response_id, now_timestamp};
use boom_core::provider::Provider;
use boom_core::types::*;
use boom_core::GatewayError;
use async_trait::async_trait;
use futures::stream::StreamExt;
use reqwest::Client;
use tokio_stream::wrappers::ReceiverStream;

/// Google Gemini provider.
///
/// Gemini uses a very different API format from OpenAI.
/// The key is passed as a query parameter, not a header.
pub struct GeminiProvider {
    client: Client,
    api_key: Option<String>,
    model: String,
    deployment_id: Option<String>,
}

impl GeminiProvider {
    pub fn new(client: Client, api_key: Option<String>, model: &str, deployment_id: Option<String>) -> Self {
        Self {
            client,
            api_key,
            model: model.to_string(),
            deployment_id,
        }
    }

    /// Convert to Gemini's generateContent format.
    fn to_gemini_request(&self, req: &ChatCompletionRequest) -> serde_json::Value {
        let mut system_instruction = None;
        let mut contents = Vec::new();

        for msg in &req.messages {
            match msg.role {
                MessageRole::System => {
                    let parts = Self::content_to_gemini_parts(&msg.content);
                    if !parts.is_empty() {
                        system_instruction = Some(serde_json::json!({ "parts": parts }));
                    }
                }
                MessageRole::User => {
                    let parts = Self::content_to_gemini_parts(&msg.content);
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": parts,
                    }));
                }
                MessageRole::Assistant => {
                    let mut parts = Self::content_to_gemini_parts(&msg.content);
                    // Tool calls → functionCall parts.
                    if let Some(ref tool_calls) = msg.tool_calls {
                        for tc in tool_calls {
                            let args: serde_json::Value =
                                serde_json::from_str(&tc.function.arguments)
                                    .unwrap_or(serde_json::Value::Object(Default::default()));
                            parts.push(serde_json::json!({
                                "functionCall": {
                                    "name": tc.function.name,
                                    "args": args,
                                }
                            }));
                        }
                    }
                    if !parts.is_empty() {
                        contents.push(serde_json::json!({
                            "role": "model",
                            "parts": parts,
                        }));
                    }
                }
                MessageRole::Tool => {
                    // Tool result → functionResponse part in a user message.
                    let text = match &msg.content {
                        MessageContent::Text(t) => t.clone(),
                        MessageContent::Parts(parts) => parts
                            .iter()
                            .filter_map(|p| match p {
                                ContentPart::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                        MessageContent::Null => String::new(),
                    };
                    // Gemini expects functionResponse with the tool name.
                    // Since we only have tool_call_id, we pass the response as-is.
                    let tool_name = msg.tool_call_id.clone().unwrap_or_default();
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": [{
                            "functionResponse": {
                                "name": tool_name,
                                "response": { "result": text }
                            }
                        }]
                    }));
                }
            }
        }

        let mut body = serde_json::json!({
            "contents": contents,
        });

        if let Some(si) = system_instruction {
            body["systemInstruction"] = si;
        }

        // Build generationConfig.
        let mut config = serde_json::Map::new();
        if let Some(temp) = req.temperature {
            config.insert("temperature".to_string(), serde_json::json!(temp));
        }
        if let Some(top_p) = req.top_p {
            config.insert("topP".to_string(), serde_json::json!(top_p));
        }
        if let Some(max_tokens) = req.max_completion_tokens.or(req.max_tokens) {
            config.insert("maxOutputTokens".to_string(), serde_json::json!(max_tokens));
        }
        if let Some(n) = req.n {
            config.insert("candidateCount".to_string(), serde_json::json!(n));
        }
        if let Some(stop) = &req.stop {
            let stops: Vec<String> = match stop {
                StopSequence::Single(s) => vec![s.clone()],
                StopSequence::Multiple(v) => v.clone(),
            };
            config.insert("stopSequences".to_string(), serde_json::json!(stops));
        }

        if !config.is_empty() {
            body["generationConfig"] = serde_json::Value::Object(config);
        }

        // Tools → functionDeclarations.
        if let Some(ref tools) = req.tools {
            let func_decls: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    let mut decl = serde_json::json!({
                        "name": t.function.name,
                        "parameters": t.function.parameters,
                    });
                    if let Some(ref desc) = t.function.description {
                        decl["description"] = serde_json::json!(desc);
                    }
                    decl
                })
                .collect();
            body["tools"] = serde_json::json!([{ "functionDeclarations": func_decls }]);
        }

        body
    }

    /// Convert MessageContent to Gemini parts array.
    fn content_to_gemini_parts(content: &MessageContent) -> Vec<serde_json::Value> {
        match content {
            MessageContent::Text(t) if !t.is_empty() => {
                vec![serde_json::json!({"text": t})]
            }
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => {
                        if text.is_empty() { None } else { Some(serde_json::json!({"text": text})) }
                    }
                    ContentPart::ImageUrl { image_url } => {
                        let url = &image_url.url;
                        if url.starts_with("data:") {
                            // Data URI → inline_data.
                            if let Some(rest) = url.strip_prefix("data:") {
                                if let Some((mime, data)) = rest.split_once(";base64,") {
                                    return Some(serde_json::json!({
                                        "inlineData": {
                                            "mimeType": mime,
                                            "data": data,
                                        }
                                    }));
                                }
                            }
                        }
                        // Regular URL → file_data.
                        Some(serde_json::json!({
                            "fileData": { "fileUri": url }
                        }))
                    }
                    ContentPart::Reasoning { reasoning } => {
                        // Gemini has no reasoning block — emit as text.
                        if reasoning.is_empty() { None } else { Some(serde_json::json!({"text": reasoning})) }
                    }
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn from_gemini_response(
        &self,
        resp: serde_json::Value,
        requested_model: &str,
    ) -> ChatCompletionResponse {
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        if let Some(parts) = resp
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(text.to_string());
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    let args = fc.get("args").cloned().unwrap_or(serde_json::json!({}));
                    let arguments = serde_json::to_string(&args).unwrap_or_default();
                    let id = format!("call_{}", uuid::Uuid::new_v4().simple());
                    tool_calls.push(ToolCall {
                        id,
                        call_type: "function".to_string(),
                        function: FunctionCall { name, arguments },
                    });
                }
            }
        }

        let finish_reason = resp
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finishReason"))
            .and_then(|r| r.as_str())
            .map(|r| match r {
                "STOP" => "stop",
                "MAX_TOKENS" => "length",
                "SAFETY" => "content_filter",
                "RECITATION" => "content_filter",
                other => other,
            })
            .map(String::from)
            .or_else(|| Some("stop".to_string()));

        let usage_meta = resp.get("usageMetadata");
        let prompt_tokens = usage_meta
            .and_then(|u| u.get("promptTokenCount"))
            .and_then(|t| t.as_u64())
            .unwrap_or(0) as u32;
        let completion_tokens = usage_meta
            .and_then(|u| u.get("candidatesTokenCount"))
            .and_then(|t| t.as_u64())
            .unwrap_or(0) as u32;

        let finish_for_choice = if !tool_calls.is_empty() {
            Some("tool_calls".to_string())
        } else {
            finish_reason
        };

        ChatCompletionResponse {
            id: generate_response_id(),
            object: "chat.completion".to_string(),
            created: now_timestamp(),
            model: requested_model.to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: MessageRole::Assistant,
                    content: MessageContent::Text(text_parts.join("")),
                    name: None,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                    reasoning_content: None,
                },
                finish_reason: finish_for_choice,
                logprobs: None,
            }],
            usage: Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
            system_fingerprint: None,
        }
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn chat(&self, req: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
        let requested_model = req.model.clone();
        let body = self.to_gemini_request(&req);
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );

        let mut builder = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            builder = builder.query(&[("key", key)]);
        }
        builder = builder.timeout(std::time::Duration::from_secs(600));

        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Gemini request failed: {}", e);
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

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| {
                tracing::error!("Failed to parse Gemini response: {}", e);
                GatewayError::ProviderError("Failed to process upstream response".to_string())
            })?;

        Ok(self.from_gemini_response(body, &requested_model))
    }

    async fn chat_stream(&self, req: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
        let requested_model = req.model.clone();
        let body = self.to_gemini_request(&req);
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.model
        );

        let mut builder = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            builder = builder.query(&[("key", key)]);
        }

        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Gemini stream request failed: {}", e);
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
        let model = requested_model;

        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();
            let response_id = generate_response_id();
            let created = now_timestamp();
            let mut tool_call_index: u32 = 0;

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
                                    if let Ok(gemini_resp) =
                                        serde_json::from_str::<serde_json::Value>(data)
                                    {
                                        // Check for usage metadata (typically in the last chunk).
                                        let usage_meta = gemini_resp.get("usageMetadata");
                                        let stream_usage = usage_meta.map(|u| {
                                            let pt = u.get("promptTokenCount").and_then(|t| t.as_u64()).unwrap_or(0) as i32;
                                            let ct = u.get("candidatesTokenCount").and_then(|t| t.as_u64()).unwrap_or(0) as i32;
                                            StreamUsage {
                                                prompt_tokens: Some(pt),
                                                completion_tokens: Some(ct),
                                                total_tokens: None,
                                            }
                                        });

                                        // Check for finish reason.
                                        let finish_reason = gemini_resp
                                            .get("candidates")
                                            .and_then(|c| c.get(0))
                                            .and_then(|c| c.get("finishReason"))
                                            .and_then(|r| r.as_str())
                                            .map(|r| match r {
                                                "STOP" => "stop".to_string(),
                                                "MAX_TOKENS" => "length".to_string(),
                                                _ => r.to_string(),
                                            });

                                        // Extract parts.
                                        let parts = gemini_resp
                                            .get("candidates")
                                            .and_then(|c| c.get(0))
                                            .and_then(|c| c.get("content"))
                                            .and_then(|c| c.get("parts"))
                                            .and_then(|p| p.as_array());

                                        let mut has_content = false;

                                        if let Some(parts) = parts {
                                            for part in parts {
                                                // Text content.
                                                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                                    if !text.is_empty() {
                                                        has_content = true;
                                                        let chunk = ChatStreamChunk {
                                                            id: response_id.clone(),
                                                            object: "chat.completion.chunk".to_string(),
                                                            created,
                                                            model: model.clone(),
                                                            choices: vec![StreamChoice {
                                                                index: 0,
                                                                delta: StreamDelta {
                                                                    role: None,
                                                                    content: Some(text.to_string()),
                                                                    tool_calls: None,
                                                                    reasoning_content: None,
                                                                },
                                                                finish_reason: None,
                                                            }],
                                                            usage: None,
                                                        };
                                                        if tx.send(Ok(Some(chunk))).await.is_err() {
                                                            return;
                                                        }
                                                    }
                                                }
                                                // Function call in streaming.
                                                if let Some(fc) = part.get("functionCall") {
                                                    has_content = true;
                                                    let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                                    let args = fc.get("args").cloned().unwrap_or(serde_json::json!({}));
                                                    let arguments = serde_json::to_string(&args).unwrap_or_default();
                                                    let id = format!("call_{}", uuid::Uuid::new_v4().simple());
                                                    let idx = tool_call_index;
                                                    tool_call_index += 1;
                                                    let chunk = ChatStreamChunk {
                                                        id: response_id.clone(),
                                                        object: "chat.completion.chunk".to_string(),
                                                        created,
                                                        model: model.clone(),
                                                        choices: vec![StreamChoice {
                                                            index: 0,
                                                            delta: StreamDelta {
                                                                role: None,
                                                                content: None,
                                                                tool_calls: Some(vec![ToolCallDelta {
                                                                    index: idx,
                                                                    id: Some(id),
                                                                    call_type: Some("function".to_string()),
                                                                    function: Some(FunctionCallDelta {
                                                                        name: Some(name),
                                                                        arguments: Some(arguments),
                                                                    }),
                                                                }]),
                                                                reasoning_content: None,
                                                            },
                                                            finish_reason: None,
                                                        }],
                                                        usage: None,
                                                    };
                                                    if tx.send(Ok(Some(chunk))).await.is_err() {
                                                        return;
                                                    }
                                                }
                                            }
                                        }

                                        // Emit finish chunk if we have a finish reason or usage.
                                        if finish_reason.is_some() || stream_usage.is_some() {
                                            let finish = finish_reason.unwrap_or_else(|| "stop".to_string());
                                            let chunk = ChatStreamChunk {
                                                id: response_id.clone(),
                                                object: "chat.completion.chunk".to_string(),
                                                created,
                                                model: model.clone(),
                                                choices: vec![StreamChoice {
                                                    index: 0,
                                                    delta: StreamDelta {
                                                        role: None,
                                                        content: None,
                                                        tool_calls: None,
                                                        reasoning_content: None,
                                                    },
                                                    finish_reason: Some(finish),
                                                }],
                                                usage: stream_usage,
                                            };
                                            if tx.send(Ok(Some(chunk))).await.is_err() {
                                                return;
                                            }
                                        }

                                        let _ = has_content;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Gemini stream read error: {}", e);
                        let _ = tx
                            .send(Err(GatewayError::ProviderError(
                                "Upstream stream error".to_string(),
                            )))
                            .await;
                        return;
                    }
                }
            }
            let _ = tx.send(Ok(None)).await;
        });

        let stream = ReceiverStream::new(rx).filter_map(|result| async move {
            match result {
                Ok(Some(chunk)) => Some(Ok(chunk)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            }
        });

        Ok(Box::pin(stream))
    }

    fn name(&self) -> &str {
        "gemini"
    }

    fn models(&self) -> &[String] {
        std::slice::from_ref(&self.model)
    }

    fn deployment_id(&self) -> Option<&str> {
        self.deployment_id.as_deref()
    }
}
