use crate::span::truncate_at_byte_boundary;
use serde_json::{json, Value};

#[derive(Clone, Copy)]
enum ApiKind {
    Chat,
    Legacy,
    Anthropic,
    Responses,
}

fn api_kind(api_path: &str) -> Option<ApiKind> {
    match api_path.trim_end_matches('/') {
        "/v1/chat/completions" | "/chat/completions" => Some(ApiKind::Chat),
        "/v1/completions" | "/completions" => Some(ApiKind::Legacy),
        "/v1/messages" | "/messages" => Some(ApiKind::Anthropic),
        "/v1/responses" | "/responses" => Some(ApiKind::Responses),
        _ => None,
    }
}

pub fn convert_input_messages(
    api_path: &str,
    request: &Value,
    max_bytes: usize,
) -> Option<Value> {
    let mut messages = match api_kind(api_path)? {
        ApiKind::Chat | ApiKind::Anthropic => {
            convert_openai_messages(request.get("messages")?.as_array()?)
        }
        ApiKind::Legacy => {
            if let Some(messages) = request.get("messages").and_then(Value::as_array) {
                convert_openai_messages(messages)
            } else {
                let prompt = request.get("prompt")?.as_str()?;
                Value::Array(vec![message_from_parts(
                    "user",
                    vec![json!({"type": "text", "content": prompt})],
                )])
            }
        }
        ApiKind::Responses => {
            let input = request.get("input")?;
            match input {
                Value::String(prompt) => Value::Array(vec![message_from_parts(
                    "user",
                    vec![json!({"type": "text", "content": prompt})],
                )]),
                Value::Array(items) => Value::Array(
                    items.iter().filter_map(convert_response_item).collect(),
                ),
                _ => return None,
            }
        }
    };
    truncate_text_parts(&mut messages, max_bytes);
    enforce_total_message_budget(&mut messages, max_bytes);
    non_empty_array(messages)
}

pub fn convert_output_messages(
    api_path: &str,
    response: &Value,
    is_stream: bool,
    max_bytes: usize,
) -> Option<Value> {
    let mut messages = match api_kind(api_path)? {
        ApiKind::Chat | ApiKind::Legacy => convert_chat_choices(response)?,
        ApiKind::Anthropic if is_stream => convert_chat_choices(response)?,
        ApiKind::Anthropic => {
            let mut message = convert_message(response, Some("assistant"))?;
            insert_finish_reason(&mut message, response.get("stop_reason"));
            Value::Array(vec![message])
        }
        ApiKind::Responses if is_stream => convert_chat_choices(response)?,
        ApiKind::Responses => Value::Array(
            response
                .get("output")?
                .as_array()?
                .iter()
                .filter_map(convert_response_item)
                .collect(),
        ),
    };

    if has_error(response) {
        retain_actual_content(&mut messages);
    }
    truncate_text_parts(&mut messages, max_bytes);
    enforce_total_message_budget(&mut messages, max_bytes);
    non_empty_array(messages)
}

pub fn truncate_text_parts(messages: &mut Value, max_bytes: usize) {
    if max_bytes == 0 {
        return;
    }
    let Some(messages) = messages.as_array_mut() else {
        return;
    };
    for message in messages {
        let Some(parts) = message.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts {
            let is_text = matches!(
                part.get("type").and_then(Value::as_str),
                Some("text" | "reasoning")
            );
            if !is_text {
                continue;
            }
            let Some(content) = part.get("content").and_then(Value::as_str) else {
                continue;
            };
            let truncated = truncate_at_byte_boundary(content.to_string(), max_bytes);
            part["content"] = Value::String(truncated);
        }
    }
}

fn enforce_total_message_budget(messages: &mut Value, max_bytes: usize) {
    if max_bytes == 0 {
        return;
    }
    let Some(messages) = messages.as_array_mut() else {
        return;
    };
    let Ok(mut serialized_len) = serde_json::to_string(&messages).map(|value| value.len()) else {
        return;
    };
    if serialized_len <= max_bytes {
        return;
    }
    let mut remaining_text_parts = messages
        .iter()
        .filter_map(|message| message.get("parts").and_then(Value::as_array))
        .map(|parts| {
            parts
                .iter()
                .filter(|part| {
                    matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("text" | "reasoning")
                    )
                })
                .count()
        })
        .sum::<usize>();

    for message in messages.iter_mut().rev() {
        let Some(parts) = message.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        for index in (0..parts.len()).rev() {
            if serialized_len <= max_bytes {
                return;
            }
            if !matches!(
                parts[index].get("type").and_then(Value::as_str),
                Some("text" | "reasoning")
            ) {
                continue;
            }
            let Ok(old_part_len) = serde_json::to_string(&parts[index]).map(|value| value.len()) else {
                continue;
            };
            if remaining_text_parts == 1 && serialized_len.saturating_sub(old_part_len) > max_bytes {
                return;
            }
            remaining_text_parts = remaining_text_parts.saturating_sub(1);
            let Some(content) = parts[index].get("content").and_then(Value::as_str) else {
                continue;
            };
            let excess = serialized_len.saturating_sub(max_bytes);
            let target_len = content.len().saturating_sub(excess);
            let truncated = truncate_at_byte_boundary(content.to_string(), target_len);
            parts[index]["content"] = Value::String(truncated);
            let Ok(new_part_len) = serde_json::to_string(&parts[index]).map(|value| value.len()) else {
                continue;
            };
            serialized_len = serialized_len.saturating_sub(old_part_len.saturating_sub(new_part_len));
            if serialized_len <= max_bytes {
                return;
            }
            if parts[index]["content"].as_str() == Some("") {
                parts.remove(index);
                serialized_len = serialized_len.saturating_sub(new_part_len);
                if !parts.is_empty() {
                    serialized_len = serialized_len.saturating_sub(1);
                }
            }
        }
    }
}

fn convert_openai_messages(messages: &[Value]) -> Value {
    Value::Array(
        messages
            .iter()
            .filter_map(|message| convert_message(message, None))
            .collect(),
    )
}

fn convert_chat_choices(response: &Value) -> Option<Value> {
    let choices = response.get("choices")?.as_array()?;
    Some(Value::Array(
        choices
            .iter()
            .filter_map(|choice| {
                let mut message = if let Some(message) = choice.get("message") {
                    convert_message(message, Some("assistant"))?
                } else {
                    let text = choice.get("text")?.as_str()?;
                    message_from_parts(
                        "assistant",
                        vec![json!({"type": "text", "content": text})],
                    )
                };
                insert_finish_reason(&mut message, choice.get("finish_reason"));
                Some(message)
            })
            .collect(),
    ))
}

fn convert_message(message: &Value, default_role: Option<&str>) -> Option<Value> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .or(default_role)?;
    let mut parts = Vec::new();

    if role == "tool" {
        if let Some(part) = convert_tool_response(message) {
            parts.push(part);
        }
    } else {
        if let Some(reasoning) = message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .and_then(Value::as_str)
        {
            parts.push(json!({"type": "reasoning", "content": reasoning}));
        }
        if let Some(content) = message.get("content") {
            parts.extend(convert_content(content));
        }
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            parts.extend(tool_calls.iter().filter_map(convert_tool_call));
        }
    }

    Some(message_from_parts(role, parts))
}

fn convert_response_item(item: &Value) -> Option<Value> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => Some(message_from_parts(
            "assistant",
            vec![convert_tool_call(item)?],
        )),
        Some("function_call_output") => Some(message_from_parts(
            "tool",
            vec![convert_tool_response(item)?],
        )),
        Some("reasoning") => {
            let parts = item
                .get("summary")
                .map(convert_content)
                .unwrap_or_default();
            Some(message_from_parts("assistant", parts))
        }
        Some("message") => convert_message(item, Some("assistant")),
        _ if item.get("role").is_some() => convert_message(item, None),
        _ => None,
    }
}

fn convert_content(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => vec![json!({"type": "text", "content": text})],
        Value::Array(parts) => parts.iter().filter_map(convert_part).collect(),
        _ => Vec::new(),
    }
}

fn convert_part(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str)? {
        "text" | "input_text" | "output_text" => {
            let content = part
                .get("content")
                .or_else(|| part.get("text"))?
                .as_str()?;
            Some(json!({"type": "text", "content": content}))
        }
        "reasoning" | "thinking" | "summary_text" => {
            let content = part
                .get("content")
                .or_else(|| part.get("reasoning"))
                .or_else(|| part.get("thinking"))
                .or_else(|| part.get("text"))?
                .as_str()?;
            Some(json!({"type": "reasoning", "content": content}))
        }
        "tool_call" | "tool_use" => convert_tool_call(part),
        "tool_call_response" | "tool_result" => convert_tool_response(part),
        _ => None,
    }
}

fn convert_tool_call(call: &Value) -> Option<Value> {
    let function = call.get("function");
    let id = call
        .get("id")
        .or_else(|| call.get("call_id"))
        .and_then(Value::as_str)?;
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| function?.get("name").and_then(Value::as_str))?;
    let arguments = call
        .get("arguments")
        .or_else(|| call.get("input"))
        .or_else(|| function.and_then(|function| function.get("arguments")))?;
    let arguments = match arguments {
        Value::String(raw) => serde_json::from_str(raw).unwrap_or_else(|_| arguments.clone()),
        _ => arguments.clone(),
    };
    Some(json!({
        "type": "tool_call",
        "id": id,
        "name": name,
        "arguments": arguments,
    }))
}

fn convert_tool_response(response: &Value) -> Option<Value> {
    let id = response
        .get("id")
        .or_else(|| response.get("call_id"))
        .or_else(|| response.get("tool_call_id"))
        .or_else(|| response.get("tool_use_id"))
        .and_then(Value::as_str)?;
    let content = response
        .get("response")
        .or_else(|| response.get("content"))
        .or_else(|| response.get("output"))?;
    Some(json!({
        "type": "tool_call_response",
        "id": id,
        "response": content,
    }))
}

fn message_from_parts(role: &str, parts: Vec<Value>) -> Value {
    json!({"role": role, "parts": parts})
}

fn insert_finish_reason(message: &mut Value, reason: Option<&Value>) {
    if let Some(reason) = reason.and_then(Value::as_str) {
        message["finish_reason"] = Value::String(reason.to_string());
    }
}

fn has_error(response: &Value) -> bool {
    response.get("error").is_some_and(|error| !error.is_null())
        || matches!(
            response.get("status").and_then(Value::as_str),
            Some("failed" | "cancelled")
        )
}

fn retain_actual_content(messages: &mut Value) {
    let Some(messages) = messages.as_array_mut() else {
        return;
    };
    messages.retain(|message| {
        message
            .get("parts")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts.iter().any(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text" | "reasoning") => part
                        .get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|content| !content.is_empty()),
                    Some("tool_call" | "tool_call_response") => true,
                    _ => false,
                })
            })
    });
}

fn non_empty_array(messages: Value) -> Option<Value> {
    match &messages {
        Value::Array(items) if !items.is_empty() => Some(messages),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{convert_input_messages, convert_output_messages, truncate_text_parts};
    use serde_json::{json, Value};

    fn text_message(role: &str, content: &str) -> Value {
        json!({"role": role, "parts": [{"type": "text", "content": content}]})
    }

    #[test]
    fn genai_chat_input_normalizes_text_tool_call_and_tool_response() {
        let request = json!({"messages": [
            {"role": "user", "content": "Weather?"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call-1",
                "type": "function",
                "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call-1", "content": "sunny"}
        ]});

        assert_eq!(
            convert_input_messages("/v1/chat/completions", &request, 16_384),
            Some(json!([
                text_message("user", "Weather?"),
                {"role": "assistant", "parts": [{
                    "type": "tool_call",
                    "id": "call-1",
                    "name": "weather",
                    "arguments": {"city": "Paris"}
                }]},
                {"role": "tool", "parts": [{
                    "type": "tool_call_response", "id": "call-1", "response": "sunny"
                }]}
            ]))
        );
    }

    #[test]
    fn genai_chat_output_normalizes_reasoning_text_and_tool_call() {
        let response = json!({"choices": [{
            "message": {
                "role": "assistant",
                "content": "The weather is sunny.",
                "reasoning_content": "Check the tool result.",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]});

        assert_eq!(
            convert_output_messages("/v1/chat/completions", &response, false, 16_384),
            Some(json!([{
                "role": "assistant",
                "parts": [
                    {"type": "reasoning", "content": "Check the tool result."},
                    {"type": "text", "content": "The weather is sunny."},
                    {"type": "tool_call", "id": "call-1", "name": "weather", "arguments": {"city": "Paris"}}
                ],
                "finish_reason": "tool_calls"
            }]))
        );
    }

    #[test]
    fn genai_chat_stream_output_uses_assembled_choices() {
        let response = json!({"choices": [{
            "message": {"role": "assistant", "content": "streamed"},
            "finish_reason": "stop"
        }]});
        assert_eq!(
            convert_output_messages("/v1/chat/completions", &response, true, 16_384),
            Some(json!([{
                "role": "assistant",
                "parts": [{"type": "text", "content": "streamed"}],
                "finish_reason": "stop"
            }]))
        );
    }

    #[test]
    fn genai_legacy_completions_convert_prompt_and_text_choices() {
        let request = json!({"prompt": "Say hello"});
        let response = json!({"choices": [{"text": "hello", "finish_reason": "stop"}]});
        assert_eq!(
            convert_input_messages("/v1/completions", &request, 16_384),
            Some(json!([text_message("user", "Say hello")]))
        );
        assert_eq!(
            convert_output_messages("/v1/completions", &response, false, 16_384),
            Some(json!([{
                "role": "assistant",
                "parts": [{"type": "text", "content": "hello"}],
                "finish_reason": "stop"
            }]))
        );
    }

    #[test]
    fn genai_legacy_accepts_already_converted_chat_shape() {
        let request = json!({"messages": [{"role": "user", "content": "hello"}]});
        assert_eq!(
            convert_input_messages("/v1/completions", &request, 16_384),
            Some(json!([text_message("user", "hello")]))
        );
    }

    #[test]
    fn genai_anthropic_input_excludes_top_level_system() {
        let request = json!({
            "system": "not a separate field",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "你好"},
                {"type": "tool_result", "tool_use_id": "tool-1", "content": "sunny"}
            ]}]
        });
        assert_eq!(
            convert_input_messages("/v1/messages", &request, 16_384),
            Some(json!([{
                "role": "user",
                "parts": [
                    {"type": "text", "content": "你好"},
                    {"type": "tool_call_response", "id": "tool-1", "response": "sunny"}
                ]
            }]))
        );
    }

    #[test]
    fn genai_anthropic_nonstream_output_uses_native_content_blocks() {
        let response = json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "考虑"},
                {"type": "text", "text": "你好"},
                {"type": "tool_use", "id": "tool-1", "name": "weather", "input": {"city": "北京"}}
            ],
            "stop_reason": "tool_use"
        });
        assert_eq!(
            convert_output_messages("/v1/messages", &response, false, 16_384),
            Some(json!([{
                "role": "assistant",
                "parts": [
                    {"type": "reasoning", "content": "考虑"},
                    {"type": "text", "content": "你好"},
                    {"type": "tool_call", "id": "tool-1", "name": "weather", "arguments": {"city": "北京"}}
                ],
                "finish_reason": "tool_use"
            }]))
        );
    }

    #[test]
    fn genai_anthropic_stream_output_uses_assembled_openai_choices() {
        let response = json!({"choices": [{
            "message": {"role": "assistant", "content": "assembled"},
            "finish_reason": "stop"
        }]});
        assert_eq!(
            convert_output_messages("/v1/messages", &response, true, 16_384),
            Some(json!([{
                "role": "assistant",
                "parts": [{"type": "text", "content": "assembled"}],
                "finish_reason": "stop"
            }]))
        );
    }

    #[test]
    fn genai_responses_input_supports_string_and_native_items() {
        assert_eq!(
            convert_input_messages("/v1/responses", &json!({"input": "hello"}), 16_384),
            Some(json!([text_message("user", "hello")]))
        );
        let request = json!({
            "instructions": "not a separate field",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "weather"}]},
                {"type": "function_call", "call_id": "call-1", "name": "weather", "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call_output", "call_id": "call-1", "output": "sunny"}
            ]
        });
        assert_eq!(
            convert_input_messages("/v1/responses", &request, 16_384),
            Some(json!([
                text_message("user", "weather"),
                {"role": "assistant", "parts": [{
                    "type": "tool_call", "id": "call-1", "name": "weather", "arguments": {"city": "Paris"}
                }]},
                {"role": "tool", "parts": [{
                    "type": "tool_call_response", "id": "call-1", "response": "sunny"
                }]}
            ]))
        );
    }

    #[test]
    fn genai_responses_nonstream_output_uses_native_output_items() {
        let response = json!({"output": [
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "think"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]},
            {"type": "function_call", "call_id": "call-1", "name": "weather", "arguments": "{\"city\":\"Paris\"}"}
        ]});
        assert_eq!(
            convert_output_messages("/v1/responses", &response, false, 16_384),
            Some(json!([
                {"role": "assistant", "parts": [{"type": "reasoning", "content": "think"}]},
                {"role": "assistant", "parts": [{"type": "text", "content": "answer"}]},
                {"role": "assistant", "parts": [{
                    "type": "tool_call", "id": "call-1", "name": "weather", "arguments": {"city": "Paris"}
                }]}
            ]))
        );
    }

    #[test]
    fn genai_responses_stream_output_uses_assembled_openai_choices() {
        let response = json!({"choices": [{
            "message": {"role": "assistant", "content": "assembled"},
            "finish_reason": "stop"
        }]});
        assert_eq!(
            convert_output_messages("/v1/responses", &response, true, 16_384),
            Some(json!([{
                "role": "assistant",
                "parts": [{"type": "text", "content": "assembled"}],
                "finish_reason": "stop"
            }]))
        );
    }

    #[test]
    fn genai_truncation_preserves_cjk_json_and_zero_disables_it() {
        let mut messages = json!([{
            "role": "assistant",
            "parts": [
                {"type": "text", "content": "你好"},
                {"type": "reasoning", "content": "思考"},
                {"type": "tool_call", "id": "call-1", "name": "weather", "arguments": {"city": "巴黎"}}
            ]
        }]);
        truncate_text_parts(&mut messages, 4);
        assert_eq!(messages[0]["parts"][0]["content"], "你");
        assert_eq!(messages[0]["parts"][1]["content"], "思");
        assert_eq!(messages[0]["parts"][2]["arguments"]["city"], "巴黎");
        serde_json::to_string(&messages).expect("truncated messages remain valid JSON");

        let mut unlimited = json!([text_message("user", "你好")]);
        truncate_text_parts(&mut unlimited, 0);
        assert_eq!(unlimited[0]["parts"][0]["content"], "你好");
    }

    #[test]
    fn genai_multi_part_input_respects_total_byte_budget() {
        let request = json!({"messages": [
            {"role": "user", "content": "a".repeat(12_000)},
            {"role": "assistant", "content": [
                {"type": "text", "text": "b".repeat(12_000)},
                {"type": "reasoning", "reasoning": "c".repeat(12_000)}
            ]},
            {"role": "user", "content": "d".repeat(12_000)}
        ]});
        let messages = convert_input_messages("/v1/chat/completions", &request, 16_384)
            .expect("multi-part messages remain structured");
        let serialized = serde_json::to_string(&messages).expect("messages remain valid JSON");
        assert!(
            serialized.len() <= 16_384,
            "structured messages exceed total budget: {} bytes",
            serialized.len()
        );
        assert_eq!(messages[0]["parts"][0]["content"].as_str().unwrap().len(), 12_000);
        assert!(messages[0]["parts"][0]["content"].as_str().unwrap().starts_with('a'));
    }

    #[test]
    fn genai_error_without_content_is_none_but_partial_content_survives() {
        let empty_error = json!({
            "error": {"type": "upstream_error"},
            "choices": [{"message": {"role": "assistant", "content": ""}}]
        });
        assert_eq!(convert_output_messages("/v1/chat/completions", &empty_error, true, 16_384), None);

        let partial_error = json!({
            "error": {"type": "upstream_error"},
            "choices": [{"message": {"role": "assistant", "content": "partial"}}]
        });
        assert_eq!(
            convert_output_messages("/v1/chat/completions", &partial_error, true, 16_384),
            Some(json!([{"role": "assistant", "parts": [{"type": "text", "content": "partial"}]}]))
        );
    }

    #[test]
    fn genai_unknown_paths_and_malformed_shapes_return_none() {
        assert_eq!(convert_input_messages("/v1/unknown", &json!({}), 16_384), None);
        assert_eq!(convert_input_messages("/v1/chat/completions", &json!({"messages": {}}), 16_384), None);
        assert_eq!(convert_output_messages("/v1/responses", &json!({"output": "bad"}), false, 16_384), None);
    }

    #[test]
    fn genai_span_capture_converts_the_complete_arc_before_raw_truncation() {
        use crate::span::RequestSpan;
        use std::sync::Arc;

        let w3c = crate::context::parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01",
        )
        .unwrap();
        let mut span = RequestSpan::new(
            "request-1".to_string(),
            &w3c,
            "model-1".to_string(),
            "/v1/chat/completions".to_string(),
            false,
            1_000,
            true,
        );
        span.set_llm_request(
            Arc::new(json!({"messages": [{"role": "user", "content": "你好"}]})),
            4,
        );
        span.set_llm_response(
            Arc::new(json!({"choices": [{"message": {"role": "assistant", "content": "你好"}}]})),
            4,
        );

        for key in ["gen_ai.input.messages", "gen_ai.output.messages"] {
            let (_, value) = span.attributes.iter().find(|(name, _)| name == key).expect(key);
            let boom_core::trace::SpanAttributeValue::Json(messages) = value else {
                panic!("{key} must be structured JSON");
            };
            assert_eq!(messages[0]["parts"][0]["content"], "你");
        }
    }
}
