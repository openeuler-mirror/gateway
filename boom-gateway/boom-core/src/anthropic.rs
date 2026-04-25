use crate::normalize;
use crate::types::*;
use std::collections::HashMap;

// ============================================================
// Request Conversion: Anthropic → OpenAI
// ============================================================

/// Convert an Anthropic Messages API request to OpenAI ChatCompletion format.
pub fn anthropic_request_to_openai(req: &AnthropicMessagesRequest) -> ChatCompletionRequest {
    let mut messages = Vec::new();

    // 1. System prompt → System role message.
    if let Some(ref system) = req.system {
        let text = extract_system_text(system);
        if !text.is_empty() {
            messages.push(Message {
                role: MessageRole::System,
                content: MessageContent::Text(text),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
    }

    // 2. Convert messages.
    for msg in &req.messages {
        match msg.role.as_str() {
            "user" => convert_user_message(&msg.content, &mut messages),
            "assistant" => convert_assistant_message(&msg.content, &mut messages),
            _ => {
                // Fallback: treat as user.
                let text = content_to_string(&msg.content);
                messages.push(Message {
                    role: MessageRole::User,
                    content: MessageContent::Text(text),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                reasoning_content: None,
                });
            }
        }
    }

    // 3. Tools: Anthropic input_schema → OpenAI parameters.
    let tools = req.tools.as_ref().map(|ts| {
        ts.iter()
            .map(|t| Tool {
                tool_type: "function".to_string(),
                function: ToolFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect()
    });

    // 4. Stop sequences.
    let stop = req.stop_sequences.as_ref().map(|seqs| {
        if seqs.len() == 1 {
            StopSequence::Single(seqs[0].clone())
        } else {
            StopSequence::Multiple(seqs.clone())
        }
    });

    // 5. Extra fields (thinking, metadata, etc.).
    let mut extra = serde_json::Map::new();
    if let Some(ref thinking) = req.thinking {
        extra.insert("thinking".to_string(), thinking.clone());
    }
    if let Some(ref metadata) = req.metadata {
        extra.insert("metadata".to_string(), metadata.clone());
    }
    for (k, v) in &req.extra {
        extra.insert(k.clone(), v.clone());
    }

    ChatCompletionRequest {
        model: req.model.clone(),
        messages,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: req.max_tokens,
        max_completion_tokens: None,
        stream: req.stream,
        stop,
        n: None,
        tools,
        tool_choice: req.tool_choice.clone(),
        response_format: None,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        user: None,
        logprobs: None,
        top_logprobs: None,
        logit_bias: None,
        extra,
    }
}

// ============================================================
// Response Conversion: OpenAI → Anthropic
// ============================================================

/// Convert an OpenAI ChatCompletion response to Anthropic Messages format.
pub fn openai_response_to_anthropic(resp: &ChatCompletionResponse) -> AnthropicMessagesResponse {
    let mut content_blocks = Vec::new();

    if let Some(choice) = resp.choices.first() {
        // Text and reasoning content.
        match &choice.message.content {
            MessageContent::Text(t) => {
                if !t.is_empty() {
                    content_blocks.push(AnthropicResponseContentBlock::Text { text: t.clone() });
                }
            }
            MessageContent::Parts(parts) => {
                for p in parts {
                    match p {
                        ContentPart::Text { text } => {
                            if !text.is_empty() {
                                content_blocks
                                    .push(AnthropicResponseContentBlock::Text { text: text.clone() });
                            }
                        }
                        ContentPart::Reasoning { reasoning } => {
                            content_blocks.push(AnthropicResponseContentBlock::Thinking {
                                thinking: reasoning.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            MessageContent::Null => {}
        }

        // Tool calls.
        if let Some(ref tool_calls) = choice.message.tool_calls {
            for tc in tool_calls {
                let input: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);
                content_blocks.push(AnthropicResponseContentBlock::ToolUse {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    input,
                });
            }
        }
    }

    if content_blocks.is_empty() {
        content_blocks.push(AnthropicResponseContentBlock::Text {
            text: String::new(),
        });
    }

    let stop_reason = resp
        .choices
        .first()
        .and_then(|c| finish_reason_to_stop_reason(&c.finish_reason));

    AnthropicMessagesResponse {
        id: generate_anthropic_message_id(),
        response_type: "message".to_string(),
        role: "assistant".to_string(),
        content: content_blocks,
        model: resp.model.clone(),
        stop_reason,
        stop_sequence: None,
        usage: AnthropicUsage {
            input_tokens: resp.usage.prompt_tokens,
            output_tokens: resp.usage.completion_tokens,
            cache_creation_input_tokens: resp.usage.cache_creation_input_tokens,
            cache_read_input_tokens: resp.usage.cache_read_input_tokens,
        },
    }
}

// ============================================================
// Stream Transcoder: OpenAI chunks → Anthropic SSE events
// ============================================================

/// A single Anthropic SSE event (event type + JSON data).
pub struct AnthropicSseEvent {
    pub event: String,
    pub data: String,
}

/// Stateful transcoder that converts OpenAI stream chunks into Anthropic SSE events.
pub struct AnthropicStreamTranscoder {
    response_id: String,
    model: String,
    message_started: bool,
    content_block_index: u32,
    text_block_open: bool,
    /// Whether a thinking block is currently open.
    thinking_block_open: bool,
    /// Maps OpenAI tool_call index → Anthropic content block index.
    tool_block_map: HashMap<u32, u32>,
    /// Buffers argument fragments per OpenAI tool_call index.
    tool_arg_buf: HashMap<u32, String>,
    input_tokens: u32,
    output_tokens: u32,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    /// Held stop_reason, waiting for the usage chunk to arrive before emitting
    /// message_delta + message_stop. Events are reconstructed at release time
    /// so that the latest usage values are always used.
    pending_stop_reason: Option<String>,
}

impl AnthropicStreamTranscoder {
    pub fn new(model: String) -> Self {
        Self {
            response_id: generate_anthropic_message_id(),
            model,
            message_started: false,
            content_block_index: 0,
            text_block_open: false,
            thinking_block_open: false,
            tool_block_map: HashMap::new(),
            tool_arg_buf: HashMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            pending_stop_reason: None,
        }
    }

    /// Close the currently open content block (text or thinking) if any.
    fn close_open_block(&mut self, events: &mut Vec<AnthropicSseEvent>) {
        if self.thinking_block_open {
            self.thinking_block_open = false;
            let idx = self.content_block_index;
            self.content_block_index += 1;
            events.push(AnthropicSseEvent {
                event: "content_block_stop".to_string(),
                data: serde_json::json!({ "type": "content_block_stop", "index": idx })
                    .to_string(),
            });
        } else if self.text_block_open {
            self.text_block_open = false;
            let idx = self.content_block_index;
            self.content_block_index += 1;
            events.push(AnthropicSseEvent {
                event: "content_block_stop".to_string(),
                data: serde_json::json!({ "type": "content_block_stop", "index": idx })
                    .to_string(),
            });
        }
    }

    /// Convert one OpenAI stream chunk into zero or more Anthropic SSE events.
    pub fn transcode(&mut self, chunk: &ChatStreamChunk) -> Vec<AnthropicSseEvent> {
        let mut events = Vec::new();

        // Extract usage data when available.
        // Use real values directly (overriding char-based estimates) — upstream
        // usage is authoritative for actual token counts.
        if let Some(ref usage) = chunk.usage {
            if let Some(pt) = usage.prompt_tokens {
                self.input_tokens = pt as u32;
            }
            if let Some(ct) = usage.completion_tokens {
                self.output_tokens = ct as u32;
            }
        }

        // If we have a held stop_reason, the previous chunk had finish_reason.
        // The current chunk likely carries the usage data (vLLM pattern) or is
        // just the next chunk. Either way, emit message_delta + message_stop now
        // with the LATEST usage values (extracted above).
        if let Some(stop_reason) = self.pending_stop_reason.take() {
            self.emit_finish_events(&mut events, &stop_reason);
            return events;
        }

        for choice in &chunk.choices {
            // First chunk: emit message_start.
            if !self.message_started {
                self.message_started = true;
                events.push(AnthropicSseEvent {
                    event: "message_start".to_string(),
                    data: serde_json::json!({
                        "type": "message_start",
                        "message": {
                            "id": self.response_id,
                            "type": "message",
                            "role": "assistant",
                            "content": [],
                            "model": self.model,
                            "stop_reason": null,
                            "stop_sequence": null,
                            "usage": {
                                "input_tokens": self.input_tokens,
                                "output_tokens": self.output_tokens,
                                "cache_creation_input_tokens": self.cache_creation_input_tokens,
                                "cache_read_input_tokens": self.cache_read_input_tokens,
                            }
                        }
                    })
                    .to_string(),
                });
            }

            // Reasoning content delta → thinking block.
            if let Some(ref reasoning) = choice.delta.reasoning_content {
                if !reasoning.is_empty() {
                    // Close text block if open (thinking comes before text).
                    if self.text_block_open {
                        self.text_block_open = false;
                        let idx = self.content_block_index;
                        self.content_block_index += 1;
                        events.push(AnthropicSseEvent {
                            event: "content_block_stop".to_string(),
                            data: serde_json::json!({ "type": "content_block_stop", "index": idx })
                                .to_string(),
                        });
                    }
                    if !self.thinking_block_open {
                        self.thinking_block_open = true;
                        let idx = self.content_block_index;
                        events.push(AnthropicSseEvent {
                            event: "content_block_start".to_string(),
                            data: serde_json::json!({
                                "type": "content_block_start",
                                "index": idx,
                                "content_block": { "type": "thinking", "thinking": "" }
                            })
                            .to_string(),
                        });
                    }
                    events.push(AnthropicSseEvent {
                        event: "content_block_delta".to_string(),
                        data: serde_json::json!({
                            "type": "content_block_delta",
                            "index": self.content_block_index,
                            "delta": { "type": "thinking_delta", "thinking": reasoning }
                        })
                        .to_string(),
                    });
                }
            }

            // Text content delta — only open a text block when there's actual text.
            if let Some(ref text) = choice.delta.content {
                if !text.is_empty() {
                    // Close thinking block before starting text.
                    if self.thinking_block_open {
                        self.thinking_block_open = false;
                        let idx = self.content_block_index;
                        self.content_block_index += 1;
                        events.push(AnthropicSseEvent {
                            event: "content_block_stop".to_string(),
                            data: serde_json::json!({ "type": "content_block_stop", "index": idx })
                                .to_string(),
                        });
                    }
                    if !self.text_block_open {
                        self.text_block_open = true;
                        let idx = self.content_block_index;
                        events.push(AnthropicSseEvent {
                            event: "content_block_start".to_string(),
                            data: serde_json::json!({
                                "type": "content_block_start",
                                "index": idx,
                                "content_block": { "type": "text", "text": "" }
                            })
                            .to_string(),
                        });
                    }
                    self.output_tokens += 1;
                    events.push(AnthropicSseEvent {
                        event: "content_block_delta".to_string(),
                        data: serde_json::json!({
                            "type": "content_block_delta",
                            "index": self.content_block_index,
                            "delta": { "type": "text_delta", "text": text }
                        })
                        .to_string(),
                    });
                }
            }

            // Tool call deltas.
            if let Some(ref tool_calls) = choice.delta.tool_calls {
                // Close any open block before starting tool blocks.
                self.close_open_block(&mut events);

                for tc in tool_calls {
                    // New tool call: has id.
                    if let Some(ref id) = tc.id {
                        let name = tc
                            .function
                            .as_ref()
                            .and_then(|f| f.name.clone())
                            .unwrap_or_default();
                        let content_idx = self.content_block_index;
                        self.tool_block_map.insert(tc.index, content_idx);
                        self.content_block_index += 1;
                        events.push(AnthropicSseEvent {
                            event: "content_block_start".to_string(),
                            data: serde_json::json!({
                                "type": "content_block_start",
                                "index": content_idx,
                                "content_block": {
                                    "type": "tool_use",
                                    "id": id,
                                    "name": name,
                                    "input": {}
                                }
                            })
                            .to_string(),
                        });
                    }
                    // Argument delta — buffer instead of emitting immediately.
                    if let Some(ref func) = tc.function {
                        if let Some(ref args) = func.arguments {
                            if !args.is_empty() {
                                // vLLM quirk: the finish chunk may contain the COMPLETE
                                // JSON arguments after already sending (possibly incomplete)
                                // fragments. When we detect a complete JSON object AND the
                                // buffer already has content, REPLACE the buffer with the
                                // canonical complete version rather than concatenating.
                                let is_vllm_complete = args.starts_with('{')
                                    && args.ends_with('}')
                                    && serde_json::from_str::<serde_json::Value>(args).is_ok();
                                let has_existing = self
                                    .tool_arg_buf
                                    .get(&tc.index)
                                    .map_or(false, |e| !e.is_empty());
                                if is_vllm_complete && has_existing {
                                    self.tool_arg_buf.insert(tc.index, args.clone());
                                } else {
                                    self.tool_arg_buf
                                        .entry(tc.index)
                                        .or_default()
                                        .push_str(args);
                                }
                            }
                        }
                    }
                }
            }

            // Finish.
            if choice.finish_reason.is_some() {
                // Close any open block (text or thinking).
                self.close_open_block(&mut events);

                // Close tool blocks — flush buffered args as a single partial_json first.
                let mut tc_indices: Vec<u32> = self.tool_block_map.keys().copied().collect();
                tc_indices.sort();
                for tc_idx in &tc_indices {
                    if let Some(block_idx) = self.tool_block_map.get(tc_idx) {
                        // Flush buffered arguments as one input_json_delta.
                        if let Some(buf) = self.tool_arg_buf.remove(tc_idx) {
                            if !buf.is_empty() {
                                events.push(AnthropicSseEvent {
                                    event: "content_block_delta".to_string(),
                                    data: serde_json::json!({
                                        "type": "content_block_delta",
                                        "index": block_idx,
                                        "delta": {
                                            "type": "input_json_delta",
                                            "partial_json": buf
                                        }
                                    })
                                    .to_string(),
                                });
                            }
                        }
                        events.push(AnthropicSseEvent {
                            event: "content_block_stop".to_string(),
                            data: serde_json::json!({ "type": "content_block_stop", "index": block_idx })
                                .to_string(),
                        });
                    }
                }
                self.tool_block_map.clear();

                let stop_reason = finish_reason_to_stop_reason(&choice.finish_reason);

                // Hold the stop_reason — don't emit message_delta yet.
                // vLLM sends usage in a separate chunk right after finish_reason.
                // When the next chunk arrives, transcode() will extract its usage
                // and then emit message_delta + message_stop with correct values.
                self.pending_stop_reason = stop_reason;
            }
        }

        events
    }

    /// Flush any held finish events (call after stream ends).
    /// If the usage chunk never arrived, emit with whatever we have.
    pub fn drain(&mut self) -> Vec<AnthropicSseEvent> {
        let mut events = Vec::new();
        if let Some(stop_reason) = self.pending_stop_reason.take() {
            self.emit_finish_events(&mut events, &stop_reason);
        }
        events
    }

    /// Build and emit message_delta + message_stop with current usage values.
    fn emit_finish_events(&self, events: &mut Vec<AnthropicSseEvent>, stop_reason: &str) {
        events.push(AnthropicSseEvent {
            event: "message_delta".to_string(),
            data: serde_json::json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": self.output_tokens.max(1)
                }
            })
            .to_string(),
        });
        events.push(AnthropicSseEvent {
            event: "message_stop".to_string(),
            data: "{\"type\":\"message_stop\"}".to_string(),
        });
    }
}

// ============================================================
// Internal Helpers
// ============================================================

fn extract_system_text(system: &AnthropicSystemContent) -> String {
    match system {
        AnthropicSystemContent::Text(t) => t.clone(),
        AnthropicSystemContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| {
                if b.block_type == "text" {
                    Some(b.text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn content_to_string(content: &AnthropicContent) -> String {
    match content {
        AnthropicContent::Text(t) => t.clone(),
        AnthropicContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                AnthropicContentBlock::Text { text, .. } => Some(text.as_str()),
                AnthropicContentBlock::Thinking { thinking } => Some(thinking.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// Convert user message (may contain tool_result blocks → multiple OpenAI messages).
fn convert_user_message(content: &AnthropicContent, messages: &mut Vec<Message>) {
    match content {
        AnthropicContent::Text(t) => {
            messages.push(Message {
                role: MessageRole::User,
                content: MessageContent::Text(t.clone()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
        AnthropicContent::Blocks(blocks) => {
            let mut text_parts = Vec::new();
            let mut tool_results = Vec::new();

            for block in blocks {
                match block {
                    AnthropicContentBlock::Text { text, .. } => {
                        text_parts.push(ContentPart::Text { text: text.clone() });
                    }
                    AnthropicContentBlock::Image { source } => {
                        let url = normalize::convert_image_source(source);
                        text_parts.push(ContentPart::ImageUrl {
                            image_url: ImageUrl { url, detail: None },
                        });
                    }
                    AnthropicContentBlock::ToolResult {
                        tool_use_id,
                        content: result_content,
                        is_error,
                        ..
                    } => {
                        let mut text = match result_content {
                            Some(AnthropicContent::Text(t)) => t.clone(),
                            Some(AnthropicContent::Blocks(bs)) => bs
                                .iter()
                                .filter_map(|b| match b {
                                    AnthropicContentBlock::Text { text, .. } => Some(text.clone()),
                                    AnthropicContentBlock::Image { source } => {
                                        // OpenAI Tool role doesn't support images —
                                        // emit a placeholder so the content isn't silently lost.
                                        let url = normalize::convert_image_source(source);
                                        let display = if url.len() > 80 { &url[..80] } else { &url };
                                        Some(format!("[image: {}]", display))
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join(""),
                            None => String::new(),
                        };
                        // Forward is_error flag to OpenAI consumers.
                        if *is_error == Some(true) {
                            text = format!("[ERROR] {}", text);
                        }
                        tool_results.push((tool_use_id.clone(), text));
                    }
                    AnthropicContentBlock::Document {
                        source,
                        title,
                        ..
                    } => {
                        // Extract text from document source if possible.
                        let mut doc_text = String::new();
                        if let Some(t) = title {
                            if !t.is_empty() {
                                doc_text = format!("{}\n", t);
                            }
                        }
                        // For text-type sources, extract the content.
                        if let Some(data) = source.get("data").and_then(|d| d.as_str()) {
                            doc_text.push_str(data);
                        }
                        if !doc_text.is_empty() {
                            text_parts.push(ContentPart::Text { text: doc_text });
                        }
                    }
                    _ => {}
                }
            }

            // Text/image parts → User message.
            if !text_parts.is_empty() {
                messages.push(Message {
                    role: MessageRole::User,
                    content: MessageContent::Parts(text_parts),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                reasoning_content: None,
                });
            }

            // Each tool_result → separate Tool role message.
            for (tool_use_id, text) in tool_results {
                messages.push(Message {
                    role: MessageRole::Tool,
                    content: MessageContent::Text(text),
                    name: None,
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id),
                    reasoning_content: None,
                });
            }
        }
    }
}

/// Convert assistant message (may contain tool_use blocks → tool_calls).
fn convert_assistant_message(content: &AnthropicContent, messages: &mut Vec<Message>) {
    match content {
        AnthropicContent::Text(t) => {
            messages.push(Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text(t.clone()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
        AnthropicContent::Blocks(blocks) => {
            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_calls = Vec::new();

            for block in blocks {
                match block {
                    AnthropicContentBlock::Text { text, .. } => {
                        text_parts.push(text.clone());
                    }
                    AnthropicContentBlock::ToolUse { id, name, input, .. } => {
                        tool_calls.push(ToolCall {
                            id: id.clone(),
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name: name.clone(),
                                arguments: serde_json::to_string(input).unwrap_or_default(),
                            },
                        });
                    }
                    AnthropicContentBlock::Thinking { thinking } => {
                        // Carry thinking content through the pipeline.
                        text_parts.push(thinking.clone());
                    }
                    AnthropicContentBlock::RedactedThinking { .. } => {
                        // Intentionally opaque — no meaningful content to forward.
                    }
                    AnthropicContentBlock::Document { source, title, .. } => {
                        let mut doc_text = String::new();
                        if let Some(t) = title {
                            if !t.is_empty() {
                                doc_text = format!("{}\n", t);
                            }
                        }
                        if let Some(data) = source.get("data").and_then(|d| d.as_str()) {
                            doc_text.push_str(data);
                        }
                        if !doc_text.is_empty() {
                            text_parts.push(doc_text);
                        }
                    }
                    _ => {}
                }
            }

            // If we have thinking blocks, use Parts-based content so that
            // Reasoning parts can be preserved for Anthropic round-tripping.
            let has_thinking = blocks.iter().any(|b| {
                        matches!(b, AnthropicContentBlock::Thinking { .. })
                    });
            let text = text_parts.join("");

            if has_thinking {
                let mut parts = Vec::new();
                for block in blocks {
                    match block {
                        AnthropicContentBlock::Thinking { thinking } => {
                            parts.push(ContentPart::Reasoning { reasoning: thinking.clone() });
                        }
                        AnthropicContentBlock::Text { text: t, .. } => {
                            if !t.is_empty() {
                                parts.push(ContentPart::Text { text: t.clone() });
                            }
                        }
                        AnthropicContentBlock::Document { source, title, .. } => {
                            let mut doc_text = String::new();
                            if let Some(t) = title {
                                if !t.is_empty() {
                                    doc_text = format!("{}\n", t);
                                }
                            }
                            if let Some(data) = source.get("data").and_then(|d| d.as_str()) {
                                doc_text.push_str(data);
                            }
                            if !doc_text.is_empty() {
                                parts.push(ContentPart::Text { text: doc_text });
                            }
                        }
                        _ => {}
                    }
                }
                if parts.is_empty() {
                    parts.push(ContentPart::Text { text: String::new() });
                }
                messages.push(Message {
                    role: MessageRole::Assistant,
                    content: MessageContent::Parts(parts),
                    name: None,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                reasoning_content: None,
                });
            } else {
                messages.push(Message {
                    role: MessageRole::Assistant,
                    content: MessageContent::Text(text),
                    name: None,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                reasoning_content: None,
                });
            }
        }
    }
}

/// Map OpenAI finish_reason to Anthropic stop_reason.
fn finish_reason_to_stop_reason(reason: &Option<String>) -> Option<String> {
    reason.as_ref().map(|r| match r.as_str() {
        "stop" => "end_turn",
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        other => other,
    }.to_string())
}

/// Generate a `msg_` prefixed random ID.
fn generate_anthropic_message_id() -> String {
    let id = uuid::Uuid::new_v4();
    format!("msg_{}", id.simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_user_message_image_url() {
        let content = AnthropicContent::Blocks(vec![AnthropicContentBlock::Image {
            source: serde_json::json!({
                "type": "url",
                "url": "https://example.com/img.png"
            }),
        }]);
        let mut messages = Vec::new();
        convert_user_message(&content, &mut messages);
        assert_eq!(messages.len(), 1);
        match &messages[0].content {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 1);
                match &parts[0] {
                    ContentPart::ImageUrl { image_url } => {
                        assert_eq!(image_url.url, "https://example.com/img.png");
                    }
                    _ => panic!("Expected ImageUrl"),
                }
            }
            _ => panic!("Expected Parts"),
        }
    }

    #[test]
    fn test_convert_user_message_image_base64() {
        let content = AnthropicContent::Blocks(vec![AnthropicContentBlock::Image {
            source: serde_json::json!({
                "type": "base64",
                "media_type": "image/jpeg",
                "data": "SGVsbG8="
            }),
        }]);
        let mut messages = Vec::new();
        convert_user_message(&content, &mut messages);
        match &messages[0].content {
            MessageContent::Parts(parts) => match &parts[0] {
                ContentPart::ImageUrl { image_url } => {
                    assert!(image_url.url.starts_with("data:image/jpeg;base64,"));
                }
                _ => panic!("Expected ImageUrl"),
            },
            _ => panic!("Expected Parts"),
        }
    }

    #[test]
    fn test_convert_user_message_tool_result_error() {
        let content = AnthropicContent::Blocks(vec![AnthropicContentBlock::ToolResult {
            tool_use_id: "tool_123".to_string(),
            content: Some(AnthropicContent::Text("something failed".to_string())),
            is_error: Some(true),
        }]);
        let mut messages = Vec::new();
        convert_user_message(&content, &mut messages);
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0].role, MessageRole::Tool));
        match &messages[0].content {
            MessageContent::Text(t) => {
                assert!(t.starts_with("[ERROR]"));
            }
            _ => panic!("Expected Text"),
        }
    }

    #[test]
    fn test_convert_assistant_thinking() {
        let content = AnthropicContent::Blocks(vec![
            AnthropicContentBlock::Thinking {
                thinking: "Let me think...".to_string(),
            },
            AnthropicContentBlock::Text {
                text: "Here is the answer.".to_string(),
                cache_control: None,
            },
        ]);
        let mut messages = Vec::new();
        convert_assistant_message(&content, &mut messages);
        assert_eq!(messages.len(), 1);
        match &messages[0].content {
            MessageContent::Parts(parts) => {
                // Should have Reasoning + Text
                assert!(parts.iter().any(|p| matches!(p, ContentPart::Reasoning { .. })));
                assert!(parts.iter().any(|p| matches!(p, ContentPart::Text { .. })));
            }
            _ => panic!("Expected Parts for thinking content"),
        }
    }

    #[test]
    fn test_response_thinking_roundtrip() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-123".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "test".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: MessageRole::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Reasoning {
                            reasoning: "hmm".to_string(),
                        },
                        ContentPart::Text {
                            text: "answer".to_string(),
                        },
                    ]),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                reasoning_content: None,
                },
                finish_reason: Some("stop".to_string()),
                logprobs: None,
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cache_creation_input_tokens: Some(3),
                cache_read_input_tokens: Some(7),
            },
            system_fingerprint: None,
        };
        let anthro = openai_response_to_anthropic(&resp);
        assert_eq!(anthro.content.len(), 2);
        // First block should be thinking
        match &anthro.content[0] {
            AnthropicResponseContentBlock::Thinking { thinking } => {
                assert_eq!(thinking, "hmm");
            }
            _ => panic!("Expected Thinking block"),
        }
        // Second block should be text
        match &anthro.content[1] {
            AnthropicResponseContentBlock::Text { text } => {
                assert_eq!(text, "answer");
            }
            _ => panic!("Expected Text block"),
        }
        assert_eq!(anthro.usage.cache_creation_input_tokens, Some(3));
        assert_eq!(anthro.usage.cache_read_input_tokens, Some(7));
    }
}
