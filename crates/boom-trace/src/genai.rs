use crate::span::truncate_at_byte_boundary;
use serde_json::{json, Value};

#[derive(Clone, Copy)]
enum ApiKind {
    Chat,
    Legacy,
    Anthropic,
    Responses,
}

pub struct ConvertedMessages {
    pub messages: Value,
    pub original_bytes: usize,
    pub truncated: bool,
    pub omitted_bytes: usize,
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
) -> Option<ConvertedMessages> {
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
    retain_actual_content(&mut messages);
    let original_messages = messages.clone();
    enforce_input_budget(&mut messages, max_bytes);
    retain_actual_content(&mut messages);
    into_converted_messages(messages, &original_messages)
}

pub fn convert_output_messages(
    api_path: &str,
    response: &Value,
    is_stream: bool,
    max_bytes: usize,
) -> Option<ConvertedMessages> {
    let mut messages = match api_kind(api_path)? {
        ApiKind::Chat | ApiKind::Legacy => convert_chat_choices(response)?,
        ApiKind::Anthropic if is_stream => convert_chat_choices(response)?,
        ApiKind::Anthropic => {
            let message = convert_message(response, Some("assistant"))?;
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

    let original_messages = messages.clone();
    enforce_output_budget(&mut messages, max_bytes);
    retain_actual_content(&mut messages);
    into_converted_messages(messages, &original_messages)
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

/// Official gen-ai-tool-definitions.json shape: [{type, name, description?, parameters?}].
/// include_details=false -> only {type, name}. Malformed entries are skipped with
/// tracing::debug! and never fail the whole conversion. Returns None when the request
/// carries no tools/functions or nothing valid remains.
pub fn convert_tool_definitions(request: &Value, include_details: bool) -> Option<Value> {
    let mut definitions = Vec::new();
    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        definitions.extend(
            tools
                .iter()
                .filter_map(|tool| convert_tool_definition(tool, include_details)),
        );
    }
    if let Some(functions) = request.get("functions").and_then(Value::as_array) {
        definitions.extend(functions.iter().filter_map(|function| {
            definition_value("function", function, include_details)
        }));
    }
    non_empty_array(Value::Array(definitions))
}

fn convert_tool_definition(tool: &Value, include_details: bool) -> Option<Value> {
    let Some(tool_type) = tool.get("type").and_then(Value::as_str) else {
        tracing::debug!("skipping tool definition without a string type");
        return None;
    };
    let source = if tool_type == "function" {
        let Some(function) = tool.get("function") else {
            tracing::debug!("skipping function tool without a function object");
            return None;
        };
        function
    } else {
        tool
    };
    definition_value(tool_type, source, include_details)
}

fn definition_value(tool_type: &str, source: &Value, include_details: bool) -> Option<Value> {
    let Some(name) = source.get("name").and_then(Value::as_str) else {
        tracing::debug!(tool_type, "skipping tool definition without a string name");
        return None;
    };
    let mut definition = json!({"type": tool_type, "name": name});
    if !include_details {
        return Some(definition);
    }
    if let Some(description) = source.get("description").and_then(Value::as_str) {
        definition["description"] = Value::String(description.to_string());
    }
    if let Some(parameters) = source.get("parameters") {
        if !parameters.is_object() {
            tracing::debug!(tool = name, "skipping tool definition with non-object parameters");
            return None;
        }
        definition["parameters"] = parameters.clone();
    }
    Some(definition)
}

#[derive(Clone, Copy)]
enum InputBudgetPhase {
    History,
    LatestUserText,
    LatestUserTools,
    SystemText,
}

fn serialized_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_string(value).unwrap_or_default().len()
}

fn message_role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

fn latest_user_index(messages: &[Value]) -> Option<usize> {
    messages
        .iter()
        .rposition(|message| message_role(message) == Some("user"))
}

fn oldest_removable_range(messages: &[Value]) -> Option<(usize, usize)> {
    let latest_user = latest_user_index(messages);
    for (index, message) in messages.iter().enumerate() {
        if Some(index) == latest_user || message_role(message) == Some("system") {
            continue;
        }
        let mut end = index + 1;
        let has_tool_call = message
            .get("parts")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("tool_call")
                })
            });
        if has_tool_call {
            while end < messages.len() && message_role(&messages[end]) == Some("tool") {
                end += 1;
            }
        }
        return Some((index, end));
    }
    None
}

fn text_part_count(messages: &[Value]) -> usize {
    messages
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
        .sum()
}

fn has_tool_parts(message: &Value) -> bool {
    message
        .get("parts")
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts.iter().any(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("tool_call" | "tool_call_response")
                )
            })
        })
}

fn remove_part(parts: &mut Vec<Value>, index: usize) -> usize {
    let part_bytes = serialized_len(&parts[index]);
    let comma_bytes = if parts.len() > 1 { 1 } else { 0 };
    parts.remove(index);
    part_bytes + comma_bytes
}

fn apply_next_part_action(
    message: &mut Value,
    text: bool,
    reasoning: bool,
    tools: bool,
    preserve_last_text: bool,
    defer_last_text_for_tools: bool,
    current_bytes: usize,
    max_bytes: usize,
) -> Option<usize> {
    let Some(parts) = message.get_mut("parts").and_then(Value::as_array_mut) else {
        return None;
    };
    for index in 0..parts.len() {
        let part_type = parts[index].get("type").and_then(Value::as_str);
        if (text && part_type == Some("text")) || (reasoning && part_type == Some("reasoning")) {
            let old_part_len = serialized_len(&parts[index]);
            let Some(content) = parts[index].get("content").and_then(Value::as_str) else {
                continue;
            };
            if preserve_last_text && current_bytes.saturating_sub(old_part_len) > max_bytes {
                if defer_last_text_for_tools {
                    continue;
                }
                let first_char_len = content.chars().next().map(char::len_utf8).unwrap_or_default();
                if first_char_len == 0 || first_char_len == content.len() {
                    continue;
                }
                parts[index]["content"] = Value::String(truncate_at_byte_boundary(
                    content.to_string(),
                    first_char_len,
                ));
                let new_part_len = serialized_len(&parts[index]);
                return Some(old_part_len.saturating_sub(new_part_len));
            }
            if content.is_empty() {
                return Some(remove_part(parts, index));
            }
            let excess = current_bytes.saturating_sub(max_bytes);
            let target_len = content.len().saturating_sub(excess);
            let truncated = truncate_at_byte_boundary(content.to_string(), target_len);
            if truncated.len() == content.len() {
                continue;
            }
            parts[index]["content"] = Value::String(truncated);
            let new_part_len = serialized_len(&parts[index]);
            return Some(old_part_len.saturating_sub(new_part_len));
        }
        if tools && matches!(part_type, Some("tool_call" | "tool_call_response")) {
            return Some(remove_part(parts, index));
        }
    }
    None
}

fn enforce_input_budget(messages: &mut Value, max_bytes: usize) {
    if max_bytes == 0 {
        return;
    }
    let Some(messages) = messages.as_array_mut() else {
        return;
    };
    let mut current_bytes = serialized_len(messages);
    if current_bytes <= max_bytes {
        return;
    }

    while current_bytes > max_bytes {
        let Some((start, end)) = oldest_removable_range(messages) else {
            break;
        };
        let removed_bytes = serialized_len(&messages[start..end]).saturating_sub(2);
        let drained_all = end - start == messages.len();
        messages.drain(start..end);
        if drained_all {
            current_bytes = 0;
        } else {
            current_bytes = current_bytes.saturating_sub(removed_bytes.saturating_add(1));
        }
    }

    for phase in [
        InputBudgetPhase::History,
        InputBudgetPhase::LatestUserText,
        InputBudgetPhase::LatestUserTools,
        InputBudgetPhase::LatestUserText,
        InputBudgetPhase::SystemText,
    ] {
        while current_bytes > max_bytes {
            let latest_user = latest_user_index(messages);
            let preserve_last_text = text_part_count(messages) == 1;
            let mut changed = false;
            for (index, message) in messages.iter_mut().enumerate() {
                let is_latest_user = Some(index) == latest_user;
                let is_system = message_role(message) == Some("system");
                let selected = match phase {
                    InputBudgetPhase::History => !is_latest_user && !is_system,
                    InputBudgetPhase::LatestUserText | InputBudgetPhase::LatestUserTools => {
                        is_latest_user
                    }
                    InputBudgetPhase::SystemText => is_system,
                };
                if !selected {
                    continue;
                }
                let (text, reasoning, tools) = match phase {
                    InputBudgetPhase::History => (true, true, true),
                    InputBudgetPhase::LatestUserText => (true, true, false),
                    InputBudgetPhase::LatestUserTools => (false, false, true),
                    InputBudgetPhase::SystemText => (true, true, false),
                };
                let defer_last_text_for_tools = has_tool_parts(message);
                if let Some(saved_bytes) = apply_next_part_action(
                    message,
                    text,
                    reasoning,
                    tools,
                    preserve_last_text,
                    defer_last_text_for_tools,
                    current_bytes,
                    max_bytes,
                ) {
                    current_bytes = current_bytes.saturating_sub(saved_bytes);
                    changed = true;
                    break;
                }
            }
            if !changed {
                break;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum OutputBudgetPhase {
    Reasoning,
    Text,
    Tools,
}

fn enforce_output_budget(messages: &mut Value, max_bytes: usize) {
    if max_bytes == 0 {
        return;
    }
    let Some(messages) = messages.as_array_mut() else {
        return;
    };
    let mut current_bytes = serialized_len(messages);
    if current_bytes <= max_bytes {
        return;
    }

    for phase in [
        OutputBudgetPhase::Reasoning,
        OutputBudgetPhase::Text,
        OutputBudgetPhase::Tools,
        OutputBudgetPhase::Reasoning,
        OutputBudgetPhase::Text,
    ] {
        while current_bytes > max_bytes {
            let preserve_last_text = text_part_count(messages) == 1;
            let mut changed = false;
            let defer_last_text_for_tools = messages.iter().any(has_tool_parts);
            for message in messages.iter_mut() {
                let (text, reasoning, tools) = match phase {
                    OutputBudgetPhase::Reasoning => (false, true, false),
                    OutputBudgetPhase::Text => (true, false, false),
                    OutputBudgetPhase::Tools => (false, false, true),
                };
                if let Some(saved_bytes) = apply_next_part_action(
                    message,
                    text,
                    reasoning,
                    tools,
                    preserve_last_text,
                    defer_last_text_for_tools,
                    current_bytes,
                    max_bytes,
                ) {
                    current_bytes = current_bytes.saturating_sub(saved_bytes);
                    changed = true;
                    break;
                }
            }
            if !changed {
                break;
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
                let message = if let Some(message) = choice.get("message") {
                    convert_message(message, Some("assistant"))?
                } else {
                    let text = choice.get("text")?.as_str()?;
                    message_from_parts(
                        "assistant",
                        vec![json!({"type": "text", "content": text})],
                    )
                };
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

fn into_converted_messages(messages: Value, original_messages: &Value) -> Option<ConvertedMessages> {
    let messages = non_empty_array(messages)?;
    let original_bytes = serde_json::to_string(original_messages).unwrap_or_default().len();
    let final_bytes = serde_json::to_string(&messages).unwrap_or_default().len();
    let truncated = messages != *original_messages;
    Some(ConvertedMessages {
        messages,
        original_bytes,
        truncated,
        omitted_bytes: original_bytes.saturating_sub(final_bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        convert_input_messages, convert_output_messages, convert_tool_definitions,
        truncate_text_parts,
    };
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
            convert_input_messages("/v1/chat/completions", &request, 16_384)
                .map(|converted| converted.messages),
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
            convert_output_messages("/v1/chat/completions", &response, false, 16_384)
                .map(|converted| converted.messages),
            Some(json!([{
                "role": "assistant",
                "parts": [
                    {"type": "reasoning", "content": "Check the tool result."},
                    {"type": "text", "content": "The weather is sunny."},
                    {"type": "tool_call", "id": "call-1", "name": "weather", "arguments": {"city": "Paris"}}
                ]
            }]))
        );

        let converted =
            convert_output_messages("/v1/chat/completions", &response, false, 16_384).unwrap();
        assert!(
            serde_json::to_string(&converted.messages).unwrap().find("finish_reason").is_none(),
            "output messages must not embed finish_reason (deprecated; use gen_ai.response.finish_reasons)"
        );
    }

    #[test]
    fn genai_chat_stream_output_uses_assembled_choices() {
        let response = json!({"choices": [{
            "message": {"role": "assistant", "content": "streamed"},
            "finish_reason": "stop"
        }]});
        assert_eq!(
            convert_output_messages("/v1/chat/completions", &response, true, 16_384)
                .map(|converted| converted.messages),
            Some(json!([{
                "role": "assistant",
                "parts": [{"type": "text", "content": "streamed"}]
            }]))
        );
    }

    #[test]
    fn genai_legacy_completions_convert_prompt_and_text_choices() {
        let request = json!({"prompt": "Say hello"});
        let response = json!({"choices": [{"text": "hello", "finish_reason": "stop"}]});
        assert_eq!(
            convert_input_messages("/v1/completions", &request, 16_384)
                .map(|converted| converted.messages),
            Some(json!([text_message("user", "Say hello")]))
        );
        assert_eq!(
            convert_output_messages("/v1/completions", &response, false, 16_384)
                .map(|converted| converted.messages),
            Some(json!([{
                "role": "assistant",
                "parts": [{"type": "text", "content": "hello"}]
            }]))
        );
    }

    #[test]
    fn genai_legacy_accepts_already_converted_chat_shape() {
        let request = json!({"messages": [{"role": "user", "content": "hello"}]});
        assert_eq!(
            convert_input_messages("/v1/completions", &request, 16_384)
                .map(|converted| converted.messages),
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
            convert_input_messages("/v1/messages", &request, 16_384)
                .map(|converted| converted.messages),
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
            convert_output_messages("/v1/messages", &response, false, 16_384)
                .map(|converted| converted.messages),
            Some(json!([{
                "role": "assistant",
                "parts": [
                    {"type": "reasoning", "content": "考虑"},
                    {"type": "text", "content": "你好"},
                    {"type": "tool_call", "id": "tool-1", "name": "weather", "arguments": {"city": "北京"}}
                ]
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
            convert_output_messages("/v1/messages", &response, true, 16_384)
                .map(|converted| converted.messages),
            Some(json!([{
                "role": "assistant",
                "parts": [{"type": "text", "content": "assembled"}]
            }]))
        );
    }

    #[test]
    fn genai_responses_input_supports_string_and_native_items() {
        assert_eq!(
            convert_input_messages("/v1/responses", &json!({"input": "hello"}), 16_384)
                .map(|converted| converted.messages),
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
            convert_input_messages("/v1/responses", &request, 16_384)
                .map(|converted| converted.messages),
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
            convert_output_messages("/v1/responses", &response, false, 16_384)
                .map(|converted| converted.messages),
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
            convert_output_messages("/v1/responses", &response, true, 16_384)
                .map(|converted| converted.messages),
            Some(json!([{
                "role": "assistant",
                "parts": [{"type": "text", "content": "assembled"}]
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
        let converted = convert_input_messages("/v1/chat/completions", &request, 16_384)
            .expect("multi-part messages remain structured");
        let serialized = serde_json::to_string(&converted.messages).expect("messages remain valid JSON");
        assert!(
            serialized.len() <= 16_384,
            "structured messages exceed total budget: {} bytes",
            serialized.len()
        );
        let latest = converted.messages.as_array().unwrap().last().unwrap();
        assert_eq!(latest["parts"][0]["content"].as_str().unwrap().len(), 12_000);
        assert!(latest["parts"][0]["content"].as_str().unwrap().starts_with('d'));
        assert!(converted.truncated);
    }

    #[test]
    fn budget_removes_oldest_history_and_preserves_system_and_latest_user() {
        let request = json!({"messages": [
            {"role": "system", "content": "SYS"},
            {"role": "user", "content": "old1-".to_string() + &"a".repeat(2000)},
            {"role": "assistant", "content": "old2-".to_string() + &"b".repeat(2000)},
            {"role": "user", "content": "LATEST"}
        ]});
        let c = convert_input_messages("/v1/chat/completions", &request, 2000).unwrap();
        let roles: Vec<&str> = c.messages.as_array().unwrap().iter()
            .map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["system", "user"]);
        assert_eq!(c.messages[1]["parts"][0]["content"], "LATEST");
        assert!(c.truncated);
        let final_bytes = serde_json::to_string(&c.messages).unwrap().len();
        assert_eq!(c.omitted_bytes, c.original_bytes - final_bytes);
        assert!(final_bytes <= 2000);
    }

    #[test]
    fn budget_groups_tool_chain_removal() {
        let request = json!({"messages": [
            {"role": "system", "content": "SYS"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call-1", "type": "function",
                "function": {"name": "weather", "arguments": {"data": "x".repeat(3000)}}
            }]},
            {"role": "tool", "tool_call_id": "call-1", "content": "small"},
            {"role": "user", "content": "LATEST"}
        ]});
        let c = convert_input_messages("/v1/chat/completions", &request, 500).unwrap();
        let roles: Vec<&str> = c.messages.as_array().unwrap().iter()
            .map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["system", "user"]);
        assert!(serde_json::to_string(&c.messages).unwrap().find("tool_call_response").is_none());
        assert!(c.truncated);
    }

    #[test]
    fn budget_drops_whole_tool_parts_instead_of_slicing_arguments() {
        let request = json!({"messages": [
            {"role": "user", "content": "LATEST", "tool_calls": [{
                "id": "call-1", "type": "function",
                "function": {"name": "weather", "arguments": {"data": "x".repeat(3000)}}
            }]}
        ]});
        let c = convert_input_messages("/v1/chat/completions", &request, 300).unwrap();
        for message in c.messages.as_array().unwrap() {
            for part in message["parts"].as_array().unwrap() {
                if part["type"] == "tool_call" {
                    assert_eq!(part["arguments"], json!({"data": "x".repeat(3000)}));
                }
            }
        }
        assert!(c.truncated);
        serde_json::to_string(&c.messages).expect("budgeted messages remain valid JSON");
    }

    #[test]
    fn budget_last_resort_truncates_latest_user_at_char_boundary() {
        let request = json!({"messages": [
            {"role": "system", "content": "系统"},
            {"role": "user", "content": "你好世界"}
        ]});
        let c = convert_input_messages("/v1/chat/completions", &request, 122).unwrap();
        assert_eq!(c.messages[0]["parts"][0]["content"], "系统");
        assert_eq!(c.messages[1]["parts"][0]["content"], "你");
        assert!(c.truncated);
        let serialized = serde_json::to_string(&c.messages).expect("valid JSON");
        assert!(serialized.len() <= 122);
        assert_eq!(c.omitted_bytes, c.original_bytes - serialized.len());
    }

    #[test]
    fn budget_revisits_latest_user_text_after_dropping_tools() {
        let request = json!({"messages": [{
            "role": "user", "content": "T".repeat(100), "tool_calls": [{
                "id": "call-1", "type": "function",
                "function": {"name": "weather", "arguments": {"data": "x".repeat(3000)}}
            }]
        }]});
        let c = convert_input_messages("/v1/chat/completions", &request, 150).unwrap();
        let serialized = serde_json::to_string(&c.messages).expect("valid JSON");
        assert!(serialized.len() <= 150, "budget exceeded: {} bytes", serialized.len());
        let content = c.messages[0]["parts"][0]["content"].as_str().unwrap();
        assert!(!content.is_empty());
        assert!(content.len() < 100);
        assert!(serialized.find("tool_call").is_none());
    }

    #[test]
    fn budget_drain_byte_accounting_excludes_slice_brackets() {
        let request = json!({"messages": [
            {"role": "system", "content": "SYS"},
            {"role": "user", "content": "old1-".to_string() + &"a".repeat(2000)},
            {"role": "assistant", "content": "old2-".to_string() + &"b".repeat(2000)},
            {"role": "user", "content": "LATEST"}
        ]});
        let c = convert_input_messages("/v1/chat/completions", &request, 121).unwrap();
        let serialized = serde_json::to_string(&c.messages).expect("valid JSON");
        assert!(serialized.len() <= 121, "budget exceeded: {} bytes", serialized.len());
        let roles: Vec<&str> = c.messages.as_array().unwrap().iter()
            .map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["system", "user"]);
        assert!(c.truncated);
    }

    #[test]
    fn output_budget_truncates_reasoning_before_text() {
        let response = json!({"choices": [{"message": {
            "role": "assistant",
            "content": "t".repeat(8000),
            "reasoning_content": "r".repeat(8000)
        }}]});
        let c = convert_output_messages("/v1/chat/completions", &response, false, 10_000).unwrap();
        let parts = c.messages[0]["parts"].as_array().unwrap();
        let reasoning = parts.iter().find(|part| part["type"] == "reasoning").unwrap();
        let text = parts.iter().find(|part| part["type"] == "text").unwrap();
        assert!(reasoning["content"].as_str().unwrap().len() < 8000);
        assert_eq!(text["content"].as_str().unwrap().len(), 8000);
        let serialized = serde_json::to_string(&c.messages).expect("valid JSON");
        assert!(serialized.len() <= 10_000);
        assert!(c.truncated);
        assert_eq!(c.omitted_bytes, c.original_bytes - serialized.len());
    }

    #[test]
    fn output_budget_drops_cross_message_tools_before_truncating_final_text() {
        let response = json!({"output": [
            {"type": "function_call", "call_id": "call-1", "name": "weather",
             "arguments": {"data": "x".repeat(15_000)}},
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "t".repeat(8000)}]}
        ]});
        let c = convert_output_messages("/v1/responses", &response, false, 10_000).unwrap();
        let serialized = serde_json::to_string(&c.messages).expect("valid JSON");
        assert!(serialized.len() <= 10_000);
        let final_message = c.messages.as_array().unwrap().last().unwrap();
        assert_eq!(final_message["parts"][0]["content"].as_str().unwrap().len(), 8000);
        assert!(serialized.find("tool_call").is_none());
        assert!(c.truncated);
    }

    #[test]
    fn output_budget_revisits_text_after_dropping_tool_parts() {
        let response = json!({"choices": [{"message": {
            "role": "assistant", "content": "t".repeat(100), "tool_calls": [{
                "id": "call-1", "type": "function",
                "function": {"name": "weather", "arguments": {"data": "x".repeat(3000)}}
            }]
        }}]});
        let c = convert_output_messages("/v1/chat/completions", &response, false, 150).unwrap();
        let serialized = serde_json::to_string(&c.messages).expect("valid JSON");
        assert!(serialized.len() <= 150);
        let content = c.messages[0]["parts"][0]["content"].as_str().unwrap();
        assert!(!content.is_empty());
        assert!(content.len() < 100);
        assert!(serialized.find("tool_call").is_none());
    }

    #[test]
    fn genai_error_without_content_is_none_but_partial_content_survives() {
        let empty_error = json!({
            "error": {"type": "upstream_error"},
            "choices": [{"message": {"role": "assistant", "content": ""}}]
        });
        assert_eq!(convert_output_messages("/v1/chat/completions", &empty_error, true, 16_384).map(|converted| converted.messages), None);

        let partial_error = json!({
            "error": {"type": "upstream_error"},
            "choices": [{"message": {"role": "assistant", "content": "partial"}}]
        });
        assert_eq!(
            convert_output_messages("/v1/chat/completions", &partial_error, true, 16_384)
                .map(|converted| converted.messages),
            Some(json!([{"role": "assistant", "parts": [{"type": "text", "content": "partial"}]}]))
        );
    }

    #[test]
    fn genai_unknown_paths_and_malformed_shapes_return_none() {
        assert_eq!(convert_input_messages("/v1/unknown", &json!({}), 16_384).map(|converted| converted.messages), None);
        assert_eq!(convert_input_messages("/v1/chat/completions", &json!({"messages": {}}), 16_384).map(|converted| converted.messages), None);
        assert_eq!(convert_output_messages("/v1/responses", &json!({"output": "bad"}), false, 16_384).map(|converted| converted.messages), None);
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
            false,
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

    #[test]
    fn tool_definitions_default_to_type_and_name_only() {
        let request = json!({"tools": [
            {"type": "function", "function": {"name": "weather",
                "description": "get weather", "parameters": {"type": "object"}}},
            {"type": "web_search", "name": "search"}
        ]});
        assert_eq!(convert_tool_definitions(&request, false), Some(json!([
            {"type": "function", "name": "weather"},
            {"type": "web_search", "name": "search"}
        ])));
        assert_eq!(convert_tool_definitions(&request, true), Some(json!([
            {"type": "function", "name": "weather", "description": "get weather",
             "parameters": {"type": "object"}},
            {"type": "web_search", "name": "search"}
        ])));
    }

    #[test]
    fn tool_definitions_convert_legacy_functions() {
        let request = json!({"functions": [{"name": "f", "description": "d"}]});
        assert_eq!(convert_tool_definitions(&request, true),
            Some(json!([{"type": "function", "name": "f", "description": "d"}])));
    }

    #[test]
    fn tool_definitions_skip_malformed_entries() {
        let request = json!({"tools": [
            {"type": "function"},                                   // no name -> skip
            {"type": "function", "function": {"name": "ok", "parameters": "not-an-object"}}, // bad parameters -> skip
            {"type": "function", "function": {"name": "good"}}
        ]});
        assert_eq!(convert_tool_definitions(&request, true),
            Some(json!([{"type": "function", "name": "good"}])));
    }

    #[test]
    fn tool_definitions_absent_when_no_tools() {
        assert_eq!(convert_tool_definitions(&json!({"messages": []}), true), None);
        assert_eq!(convert_tool_definitions(&json!({"tools": []}), true), None);
    }
}
