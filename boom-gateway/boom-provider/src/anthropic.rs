use crate::{generate_response_id, now_timestamp};
use boom_core::normalize;
use boom_core::provider::Provider;
use boom_core::types::*;
use boom_core::GatewayError;
use async_trait::async_trait;
use futures::stream::StreamExt;
use reqwest::Client;
use tokio_stream::wrappers::ReceiverStream;

/// Anthropic provider.
///
/// Anthropic's Messages API differs from OpenAI:
/// - System prompt is a top-level field, not a message.
/// - Content uses content blocks (array of {type: "text", text: "..."}).
/// - Streaming uses different SSE event types.
pub struct AnthropicProvider {
    client: Client,
    api_key: Option<String>,
    base_url: String,
    model: String,
    deployment_id: Option<String>,
    api_version: String,
}

impl AnthropicProvider {
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
                .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string()),
            model: model.to_string(),
            deployment_id,
            api_version: "2023-06-01".to_string(),
        }
    }

    pub fn with_api_version(mut self, version: String) -> Self {
        self.api_version = version;
        self
    }

    /// Whether the request includes extended thinking parameters.
    fn has_thinking(&self, req: &ChatCompletionRequest) -> bool {
        req.extra.contains_key("thinking")
    }

    /// Build the set of beta headers needed for the request.
    fn beta_headers(&self, req: &ChatCompletionRequest) -> Vec<&'static str> {
        let mut betas = Vec::new();
        if self.has_thinking(req) {
            betas.push("extended-thinking-2025-04-11");
        }
        // Prompt caching beta header — needed when cache_control is used.
        betas.push("prompt-caching-2024-07-31");
        betas
    }

    /// Convert our ChatCompletionRequest to Anthropic's format.
    fn to_anthropic_request(&self, req: &ChatCompletionRequest) -> serde_json::Value {
        let mut system_blocks: Vec<serde_json::Value> = Vec::new();
        let mut messages: Vec<serde_json::Value> = Vec::new();

        // Build a mutable copy of messages for role-alternation normalization.
        let mut req_messages = req.messages.clone();

        // Apply role alternation to ensure Anthropic-compatible alternating
        // user/assistant roles (after system messages are extracted).
        normalize::ensure_role_alternation(&mut req_messages);

        for msg in &req_messages {
            match msg.role {
                MessageRole::System => {
                    // Anthropic puts system as a top-level field — always use
                    // the array form to support cache_control.
                    match &msg.content {
                        MessageContent::Text(t) if !t.is_empty() => {
                            let mut block = serde_json::json!({
                                "type": "text",
                                "text": t,
                            });
                            // Forward cache_control if present in extra.
                            if let Some(cc) = req.extra.get("system_cache_control") {
                                block["cache_control"] = cc.clone();
                            }
                            system_blocks.push(block);
                        }
                        MessageContent::Parts(parts) => {
                            for p in parts {
                                match p {
                                    ContentPart::Text { text } => {
                                        let mut block = serde_json::json!({
                                            "type": "text",
                                            "text": text,
                                        });
                                        if let Some(cc) = req.extra.get("system_cache_control") {
                                            block["cache_control"] = cc.clone();
                                        }
                                        system_blocks.push(block);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                MessageRole::Assistant => {
                    let mut content_blocks: Vec<serde_json::Value> = Vec::new();

                    // Text and reasoning content.
                    match &msg.content {
                        MessageContent::Text(t) if !t.is_empty() => {
                            content_blocks
                                .push(serde_json::json!({"type": "text", "text": t}));
                        }
                        MessageContent::Parts(parts) => {
                            for p in parts {
                                match p {
                                    ContentPart::Text { text } => {
                                        content_blocks.push(
                                            serde_json::json!({"type": "text", "text": text}),
                                        );
                                    }
                                    ContentPart::Reasoning { reasoning } => {
                                        content_blocks.push(serde_json::json!({
                                            "type": "thinking",
                                            "thinking": reasoning,
                                        }));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }

                    // Tool calls → tool_use content blocks.
                    if let Some(ref tool_calls) = msg.tool_calls {
                        for tc in tool_calls {
                            let input: serde_json::Value =
                                serde_json::from_str(&tc.function.arguments)
                                    .unwrap_or(serde_json::Value::Object(Default::default()));
                            content_blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.function.name,
                                "input": input,
                            }));
                        }
                    }

                    // Ensure at least one content block (Anthropic requires non-empty content).
                    if content_blocks.is_empty() {
                        content_blocks.push(serde_json::json!({"type": "text", "text": ""}));
                    }

                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": content_blocks,
                    }));
                }
                MessageRole::Tool => {
                    // Tool result → user message with tool_result content block.
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
                    // Check if content starts with [ERROR] marker from our conversion.
                    let (content_text, is_error) = if text.starts_with("[ERROR] ") {
                        (text.trim_start_matches("[ERROR] ").to_string(), true)
                    } else {
                        (text, false)
                    };
                    let tool_call_id = msg.tool_call_id.clone().unwrap_or_default();
                    let mut result = serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": content_text,
                    });
                    if is_error {
                        result["is_error"] = serde_json::json!(true);
                    }
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": [result],
                    }));
                }
                _ => {
                    let content = match &msg.content {
                        MessageContent::Text(t) => {
                            serde_json::json!([{"type": "text", "text": t}])
                        }
                        MessageContent::Parts(parts) => {
                            let blocks: Vec<serde_json::Value> = parts
                                .iter()
                                .map(|p| match p {
                                    ContentPart::Text { text } => {
                                        serde_json::json!({"type": "text", "text": text})
                                    }
                                    ContentPart::ImageUrl { image_url } => {
                                        serde_json::json!({"type": "text", "text": image_url.url})
                                    }
                                    ContentPart::Reasoning { reasoning } => {
                                        serde_json::json!({"type": "text", "text": reasoning})
                                    }
                                })
                                .collect();
                            serde_json::json!(blocks)
                        }
                        MessageContent::Null => serde_json::json!([]),
                    };
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            }
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": req.max_completion_tokens.or(req.max_tokens).unwrap_or(4096),
        });

        // System prompt — always use array form.
        if !system_blocks.is_empty() {
            body["system"] = serde_json::json!(system_blocks);
        }
        if let Some(temp) = req.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(top_p) = req.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }

        // Stop sequences.
        if let Some(ref stop) = req.stop {
            let seqs: Vec<String> = match stop {
                StopSequence::Single(s) => vec![s.clone()],
                StopSequence::Multiple(v) => v.clone(),
            };
            body["stop_sequences"] = serde_json::json!(seqs);
        }

        // Tools: OpenAI format → Anthropic format.
        // Check parallel_tool_calls from extra fields.
        let parallel_tool_calls: Option<bool> = req
            .extra
            .get("parallel_tool_calls")
            .and_then(|v| v.as_bool());

        // Tool choice conversion.
        let (tc_value, strip_tools) =
            normalize::convert_tool_choice_for_anthropic(&req.tool_choice, parallel_tool_calls);

        if !strip_tools {
            let mut anthropic_tools: Vec<serde_json::Value> = Vec::new();

            // User-provided tools.
            if let Some(ref tools) = req.tools {
                for t in tools {
                    let mut tool = serde_json::json!({
                        "name": t.function.name,
                        "input_schema": t.function.parameters,
                    });
                    if let Some(ref desc) = t.function.description {
                        tool["description"] = serde_json::json!(desc);
                    }
                    anthropic_tools.push(tool);
                }
            }

            // response_format → synthetic tool (matches litellm behavior).
            // litellm creates a tool named "output_json" and forces tool_choice.
            if let Some(ref rf) = req.response_format {
                let rf_type = rf.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if rf_type != "text" {
                    // Build the JSON schema for the synthetic tool.
                    let schema = if rf_type == "json_schema" {
                        rf.get("json_schema")
                            .and_then(|js| js.get("schema"))
                            .cloned()
                            .unwrap_or(serde_json::json!({"type": "object"}))
                    } else {
                        // json_object — no specific schema.
                        serde_json::json!({"type": "object"})
                    };
                    anthropic_tools.push(serde_json::json!({
                        "name": "output_json",
                        "description": "Respond with a JSON object matching the provided schema.",
                        "input_schema": schema,
                    }));
                    // Force the model to use this tool.
                    body["tool_choice"] = serde_json::json!({"type": "tool", "name": "output_json"});
                }
            }

            if !anthropic_tools.is_empty() {
                body["tools"] = serde_json::json!(anthropic_tools);
            }
            // Only set tool_choice if response_format didn't already set it.
            if !body.get("tool_choice").is_some() {
                if let Some(tc) = tc_value {
                    body["tool_choice"] = tc;
                }
            }
        }
        // If strip_tools is true, we simply don't include tools or tool_choice.

        // Forward extra fields: thinking, metadata, etc.
        if let Some(thinking) = req.extra.get("thinking") {
            body["thinking"] = thinking.clone();
        }
        if let Some(metadata) = req.extra.get("metadata") {
            body["metadata"] = metadata.clone();
        }
        // Forward Anthropic-specific params from extra.
        if let Some(top_k) = req.extra.get("top_k") {
            body["top_k"] = top_k.clone();
        }
        // Forward user (litellm does this too).
        if let Some(ref user) = req.user {
            body["metadata"] = body.get("metadata").cloned().unwrap_or(serde_json::json!({}));
            if let Some(meta) = body.get_mut("metadata").and_then(|m| m.as_object_mut()) {
                meta.insert("user_id".to_string(), serde_json::json!(user));
            }
        }

        body
    }

    /// Convert Anthropic's response to OpenAI format.
    fn from_anthropic_response(
        &self,
        resp: serde_json::Value,
        requested_model: &str,
    ) -> ChatCompletionResponse {
        let mut content_parts: Vec<ContentPart> = Vec::new();
        let mut tool_calls = Vec::new();

        if let Some(blocks) = resp.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match block_type {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            content_parts.push(ContentPart::Text { text: text.to_string() });
                        }
                    }
                    "thinking" => {
                        if let Some(thinking) = block.get("thinking").and_then(|t| t.as_str()) {
                            content_parts.push(ContentPart::Reasoning {
                                reasoning: thinking.to_string(),
                            });
                        }
                    }
                    "redacted_thinking" => {
                        // Intentionally opaque — skip.
                    }
                    "tool_use" => {
                        let name = block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = block.get("input").cloned().unwrap_or(serde_json::json!({}));
                        // If this is the synthetic output_json tool from response_format,
                        // convert the input to text content instead of a tool_call.
                        if name == "output_json" {
                            let json_text = serde_json::to_string(&input).unwrap_or_default();
                            if !json_text.is_empty() {
                                content_parts.push(ContentPart::Text { text: json_text });
                            }
                        } else {
                            let id = block
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or("")
                                .to_string();
                            let arguments = serde_json::to_string(&input).unwrap_or_default();
                            tool_calls.push(ToolCall {
                                id,
                                call_type: "function".to_string(),
                                function: FunctionCall { name, arguments },
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        let stop_reason = resp
            .get("stop_reason")
            .and_then(|s| s.as_str())
            .map(String::from);

        let usage = resp.get("usage");
        let input_tokens = usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(|t| t.as_u64())
            .unwrap_or(0) as u32;
        let output_tokens = usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(|t| t.as_u64())
            .unwrap_or(0) as u32;
        let cache_creation = usage
            .and_then(|u| u.get("cache_creation_input_tokens"))
            .and_then(|t| t.as_u64())
            .map(|v| v as u32);
        let cache_read = usage
            .and_then(|u| u.get("cache_read_input_tokens"))
            .and_then(|t| t.as_u64())
            .map(|v| v as u32);

        let finish_reason = stop_reason
            .map(|r| match r.as_str() {
                "tool_use" => "tool_calls".to_string(),
                "end_turn" | "stop" => "stop".to_string(),
                "max_tokens" => "length".to_string(),
                other => other.to_string(),
            })
            .or_else(|| Some("stop".to_string()));

        // Use Parts-based content when we have reasoning blocks,
        // otherwise use simple Text for backward compatibility.
        let message_content = if content_parts.iter().any(|p| matches!(p, ContentPart::Reasoning { .. })) {
            if content_parts.is_empty() {
                MessageContent::Text(String::new())
            } else {
                // Filter out empty text parts when reasoning is present
                let filtered: Vec<ContentPart> = content_parts
                    .into_iter()
                    .filter(|p| match p {
                        ContentPart::Text { text } => !text.is_empty(),
                        _ => true,
                    })
                    .collect();
                if filtered.is_empty() {
                    MessageContent::Text(String::new())
                } else {
                    MessageContent::Parts(filtered)
                }
            }
        } else {
            let text: String = content_parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            MessageContent::Text(text)
        };

        ChatCompletionResponse {
            id: resp
                .get("id")
                .and_then(|i| i.as_str())
                .map(String::from)
                .unwrap_or_else(generate_response_id),
            object: "chat.completion".to_string(),
            created: now_timestamp(),
            model: requested_model.to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: MessageRole::Assistant,
                    content: message_content,
                    name: None,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                    reasoning_content: None,
                },
                finish_reason,
                logprobs: None,
            }],
            usage: Usage {
                prompt_tokens: input_tokens,
                completion_tokens: output_tokens,
                total_tokens: input_tokens + output_tokens,
                cache_creation_input_tokens: cache_creation,
                cache_read_input_tokens: cache_read,
            },
            system_fingerprint: None,
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn chat(&self, req: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
        let requested_model = req.model.clone();
        let body = self.to_anthropic_request(&req);
        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));
        let betas = self.beta_headers(&req);

        let mut builder = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            builder = builder.header("x-api-key", key);
        }
        builder = builder
            .header("anthropic-version", &self.api_version)
            .header("content-type", "application/json");
        if !betas.is_empty() {
            builder = builder.header("anthropic-beta", betas.join(","));
        }

        // Non-streaming: upstream sends no data until the entire response is ready.
        let resp = builder
            .timeout(std::time::Duration::from_secs(600))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Anthropic request failed: {}", e);
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
                tracing::error!("Failed to parse Anthropic response: {}", e);
                GatewayError::ProviderError("Failed to process upstream response".to_string())
            })?;

        Ok(self.from_anthropic_response(body, &requested_model))
    }

    async fn chat_stream(&self, req: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
        let requested_model = req.model.clone();
        let mut body = self.to_anthropic_request(&req);
        body["stream"] = serde_json::json!(true);
        let betas = self.beta_headers(&req);

        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));

        let mut builder = self.client.post(&url);
        if let Some(ref key) = self.api_key {
            builder = builder.header("x-api-key", key);
        }
        builder = builder
            .header("anthropic-version", &self.api_version)
            .header("content-type", "application/json");
        if !betas.is_empty() {
            builder = builder.header("anthropic-beta", betas.join(","));
        }

        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Anthropic stream request failed: {}", e);
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

            // Track open content blocks by index for proper tool_use handling.
            // Maps Anthropic content_block index → OpenAI tool_call index (0-based).
            let mut tool_index_map: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
            let mut next_tool_index: u32 = 0;

            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(bytes) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));

                        while let Some(pos) = buffer.find("\n\n") {
                            let event_text = buffer[..pos].to_string();
                            buffer = buffer[pos + 2..].to_string();

                            let mut event_type = String::new();
                            let mut data_str = String::new();

                            for line in event_text.lines() {
                                if let Some(val) = line.strip_prefix("event: ") {
                                    event_type = val.trim().to_string();
                                } else if let Some(val) = line.strip_prefix("data: ") {
                                    data_str = val.trim().to_string();
                                }
                            }

                            if data_str.is_empty() {
                                continue;
                            }

                            let data: serde_json::Value = match serde_json::from_str(&data_str) {
                                Ok(d) => d,
                                Err(_) => continue,
                            };

                            match event_type.as_str() {
                                "content_block_start" => {
                                    let idx = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                                    let block = data.get("content_block").unwrap_or(&serde_json::Value::Null);
                                    let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("text").to_string();

                                    match btype.as_str() {
                                        "tool_use" => {
                                            let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                            let openai_idx = next_tool_index;
                                            tool_index_map.insert(idx, openai_idx);
                                            next_tool_index += 1;

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
                                                            index: openai_idx,
                                                            id: Some(id),
                                                            call_type: Some("function".to_string()),
                                                            function: Some(FunctionCallDelta {
                                                                name: Some(name),
                                                                arguments: Some(String::new()),
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
                                        "thinking" => {
                                            // Emit reasoning_content delta for thinking blocks.
                                            // No initial content — thinking deltas come via content_block_delta.
                                        }
                                        _ => {
                                            // Text blocks: nothing to emit on start.
                                        }
                                    }
                                }
                                "content_block_delta" => {
                                    let idx = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                                    let delta = data.get("delta").unwrap_or(&serde_json::Value::Null);
                                    let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");

                                    match delta_type {
                                        "text_delta" => {
                                            let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                            if !text.is_empty() {
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
                                        "input_json_delta" => {
                                            // Tool argument fragment.
                                            let partial = delta.get("partial_json").and_then(|p| p.as_str()).unwrap_or("");
                                            let openai_idx = tool_index_map.get(&idx).copied().unwrap_or(idx);
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
                                                            index: openai_idx,
                                                            id: None,
                                                            call_type: None,
                                                            function: Some(FunctionCallDelta {
                                                                name: None,
                                                                arguments: Some(partial.to_string()),
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
                                        "thinking_delta" => {
                                            // Extended thinking content.
                                            let thinking = delta.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                                            if !thinking.is_empty() {
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
                                                            reasoning_content: Some(thinking.to_string()),
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
                                        _ => {}
                                    }
                                }
                                "message_delta" => {
                                    // Final event: stop_reason + usage.
                                    let stop_reason = data
                                        .get("delta")
                                        .and_then(|d| d.get("stop_reason"))
                                        .and_then(|s| s.as_str());
                                    let finish_reason = stop_reason.map(|r| match r {
                                        "tool_use" => "tool_calls".to_string(),
                                        "end_turn" | "stop" => "stop".to_string(),
                                        "max_tokens" => "length".to_string(),
                                        other => other.to_string(),
                                    });

                                    let usage_data = data.get("usage");
                                    let output_tokens = usage_data
                                        .and_then(|u| u.get("output_tokens"))
                                        .and_then(|t| t.as_u64())
                                        .unwrap_or(0) as i32;

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
                                            finish_reason,
                                        }],
                                        usage: Some(StreamUsage {
                                            prompt_tokens: None,
                                            completion_tokens: Some(output_tokens),
                                            total_tokens: None,
                                        }),
                                    };
                                    if tx.send(Ok(Some(chunk))).await.is_err() {
                                        return;
                                    }
                                }
                                "message_start" => {
                                    // Extract input_tokens from message_start event.
                                    let input_tokens = data
                                        .get("message")
                                        .and_then(|m| m.get("usage"))
                                        .and_then(|u| u.get("input_tokens"))
                                        .and_then(|t| t.as_u64())
                                        .map(|t| t as i32);

                                    // Emit a role chunk so OpenAI clients see `role: "assistant"`
                                    // in the first SSE chunk (matching litellm behavior).
                                    let role_chunk = ChatStreamChunk {
                                        id: response_id.clone(),
                                        object: "chat.completion.chunk".to_string(),
                                        created,
                                        model: model.clone(),
                                        choices: vec![StreamChoice {
                                            index: 0,
                                            delta: StreamDelta {
                                                role: Some(MessageRole::Assistant),
                                                content: None,
                                                tool_calls: None,
                                                reasoning_content: None,
                                            },
                                            finish_reason: None,
                                        }],
                                        usage: input_tokens.map(|it| StreamUsage {
                                            prompt_tokens: Some(it),
                                            completion_tokens: Some(0),
                                            total_tokens: None,
                                        }),
                                    };
                                    if tx.send(Ok(Some(role_chunk))).await.is_err() {
                                        return;
                                    }
                                }
                                "message_stop" => {
                                    let _ = tx.send(Ok(None)).await;
                                    return;
                                }
                                "content_block_stop" | "ping" => {
                                    // No action needed.
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Anthropic stream read error: {}", e);
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
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            }
        });

        Ok(Box::pin(stream))
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn models(&self) -> &[String] {
        std::slice::from_ref(&self.model)
    }

    fn deployment_id(&self) -> Option<&str> {
        self.deployment_id.as_deref()
    }
}
