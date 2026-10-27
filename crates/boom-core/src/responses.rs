//! OpenAI Responses API edge layer (POST /v1/responses).
//!
//! Mirrors `anthropic.rs`: the internal canonical format stays Chat
//! Completions; this module only converts at the protocol edge.
//! - `responses_request_to_openai` — Responses request → ChatCompletionRequest
//! - `openai_chat_to_responses` — ChatCompletionResponse → Response object
//! - `ResponsesStreamTranscoder` — ChatStream chunks → Responses SSE events
//!
//! Wire contract (per OpenAI spec, see Response.md §6.5): sequence numbers
//! start at 0 and increase contiguously; terminal event is
//! response.completed / response.incomplete / response.failed with NO
//! `data: [DONE]` sentinel; finish_reason=length maps to the independent
//! `response.incomplete` event.

use crate::error::GatewayError;
use crate::types::*;
use std::collections::{BTreeMap, HashMap};

// ============================================================
// Request types
// ============================================================

/// `input` is either a plain string (sugar for one user message) or an
/// explicit item list. Items stay raw `Value`s so unsupported types can be
/// rejected with a precise error naming the offending `type`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),
    Items(Vec<serde_json::Value>),
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<serde_json::Value>,
}

/// A Responses API request. Continuation/storage fields (`store`,
/// `previous_response_id`, `conversation`, `background`, `include`,
/// `truncation`, `metadata`) are captured explicitly so they never leak into
/// `extra` (which forwards wholesale to the upstream body).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: ResponsesInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    /// Catch-all: unknown fields collect here (never rejected) and forward to
    /// OpenAI-compatible upstreams via `ChatCompletionRequest::extra`.
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ============================================================
// Response types
// ============================================================

/// Token usage in Responses naming.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResponsesUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<ResponsesCachedTokensDetail>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResponsesCachedTokensDetail {
    pub cached_tokens: u64,
}

/// The serialized `response` object returned for non-stream requests and
/// carried inside terminal SSE events.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResponseObject {
    pub id: String,
    pub object: &'static str,
    pub created_at: u64,
    pub status: String,
    pub model: String,
    /// Completed output items in `output_index` order (message /
    /// function_call / reasoning values).
    pub output: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponsesUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<ResponsesIncompleteDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResponsesIncompleteDetails {
    pub reason: String,
}

/// A single Responses SSE event (event type + JSON data), same shape as
/// `AnthropicSseEvent`.
pub struct ResponsesSseEvent {
    pub event: String,
    pub data: String,
}

// ============================================================
// Request conversion: Responses → OpenAI chat
// ============================================================

/// P0 protocol guards: reject continuation/storage parameters the stateless
/// path cannot honor. Silent-ignore would hand the client a wrong semantic
/// (see Response.md R8) — fail fast with 400 instead.
pub fn validate_supported_request(req: &ResponsesRequest) -> Result<(), GatewayError> {
    // NOTE: previous_response_id is store POLICY, not protocol validation —
    // the handler rejects it via previous_response_id_without_store() when
    // the response store is unavailable (P1 makes it conditional).
    if req.conversation.is_some() {
        return Err(GatewayError::UnsupportedMode(
            "the conversation parameter is not supported".to_string(),
        ));
    }
    if req.background == Some(true) {
        return Err(GatewayError::UnsupportedMode(
            "background mode is not supported".to_string(),
        ));
    }
    Ok(())
}

/// The 400 the handler returns for `previous_response_id` when the response
/// store is unavailable (no DB / `responses_store.enabled: false`).
pub fn previous_response_id_without_store() -> GatewayError {
    GatewayError::UnsupportedMode(
        "previous_response_id requires the response store, which is not enabled on this gateway"
            .to_string(),
    )
}

// ---------- input item parsing ----------

/// One classified `input` item. Items stay raw `Value` until here so
/// unsupported types can be rejected naming the offending `type`.
enum InputKind {
    Message { role: String, content: serde_json::Value },
    FunctionCall { call_id: String, name: String, arguments: String },
    FunctionCallOutput { call_id: String, output: serde_json::Value },
    /// Codex freeform custom call (P0.5): replayed as the WRAPPED function
    /// call the upstream originally saw — arguments `{"input": …}`.
    CustomToolCall { call_id: String, name: String, input: String },
    /// Accepted but dropped: the chat kernel has no reasoning replay channel
    /// (Response.md §6.3; native fidelity lands with the P2 passthrough).
    Reasoning,
}

fn parse_input_item(v: &serde_json::Value) -> Result<InputKind, GatewayError> {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("message");
    match ty {
        // Messages allow an omitted discriminator (EasyInputMessage).
        "message" => Ok(InputKind::Message {
            role: v.get("role").and_then(|r| r.as_str()).unwrap_or("user").to_string(),
            content: v.get("content").cloned().unwrap_or(serde_json::Value::Null),
        }),
        "function_call" => Ok(InputKind::FunctionCall {
            call_id: v
                .get("call_id")
                .and_then(|c| c.as_str())
                .or_else(|| v.get("id").and_then(|c| c.as_str()))
                .unwrap_or_default()
                .to_string(),
            name: v.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string(),
            arguments: v.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}").to_string(),
        }),
        "function_call_output" => Ok(InputKind::FunctionCallOutput {
            call_id: v.get("call_id").and_then(|c| c.as_str()).unwrap_or_default().to_string(),
            output: v.get("output").cloned().unwrap_or(serde_json::Value::Null),
        }),
        "custom_tool_call" => Ok(InputKind::CustomToolCall {
            call_id: v
                .get("call_id")
                .and_then(|c| c.as_str())
                .or_else(|| v.get("id").and_then(|c| c.as_str()))
                .unwrap_or_default()
                .to_string(),
            name: v.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string(),
            input: v.get("input").and_then(|i| i.as_str()).unwrap_or_default().to_string(),
        }),
        // Same downstream shape as function_call_output: a tool message.
        "custom_tool_call_output" => Ok(InputKind::FunctionCallOutput {
            call_id: v.get("call_id").and_then(|c| c.as_str()).unwrap_or_default().to_string(),
            output: v.get("output").cloned().unwrap_or(serde_json::Value::Null),
        }),
        "reasoning" => Ok(InputKind::Reasoning),
        other => Err(GatewayError::UnsupportedMode(format!(
            "unsupported input item type '{other}'"
        ))),
    }
}

/// Responses role → chat role. `developer` must be rewritten here: the kernel
/// `MessageRole` is a closed enum whose `#[serde(other)]` would degrade it to
/// `Unknown`/`"unknown"` on the wire (Response.md R11).
fn chat_role(role: &str) -> MessageRole {
    match role {
        "system" | "developer" => MessageRole::System,
        "assistant" => MessageRole::Assistant,
        // user (and anything else, lenient like the anthropic edge) → user
        _ => MessageRole::User,
    }
}

/// Message content: string → Text; array → ContentParts (`input_text` /
/// `output_text` / image / refusal); anything else → its JSON string form.
fn message_content_from(value: &serde_json::Value) -> MessageContent {
    match value {
        serde_json::Value::String(s) => MessageContent::Text(s.clone()),
        serde_json::Value::Array(parts) => {
            let mut out = Vec::new();
            for p in parts {
                let ty = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match ty {
                    "input_text" | "output_text" | "text" | "summary_text" => {
                        out.push(ContentPart::Text {
                            text: p.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                        });
                    }
                    "input_image" => {
                        // `image_url` is a string or an {url, detail} object.
                        let url = match p.get("image_url") {
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(obj) => obj
                                .get("url")
                                .and_then(|u| u.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            _ => String::new(),
                        };
                        let detail = p
                            .get("image_url")
                            .and_then(|i| i.get("detail"))
                            .and_then(|d| d.as_str())
                            .map(String::from);
                        out.push(ContentPart::ImageUrl { image_url: ImageUrl { url, detail } });
                    }
                    "refusal" => {
                        let r = p.get("refusal").and_then(|t| t.as_str()).unwrap_or("");
                        if !r.is_empty() {
                            out.push(ContentPart::Text { text: r.to_string() });
                        }
                    }
                    _ => {} // unknown part types are skipped, not fatal
                }
            }
            if out.is_empty() {
                MessageContent::Text(String::new())
            } else {
                MessageContent::Parts(out)
            }
        }
        serde_json::Value::Null => MessageContent::Text(String::new()),
        other => MessageContent::Text(other.to_string()),
    }
}

/// `function_call_output.output` → tool message text: string as-is, part
/// array joins text parts, object serializes to JSON.
fn output_to_text(output: &serde_json::Value) -> String {
    match output {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "output_text" | "text" => {
                    Some(p.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        other => other.to_string(),
    }
}

/// Only `type:"function"` tools are supported in P0; built-in tools
/// LENIENT on unknown types (R2 review): built-in tools the chat kernel
/// cannot express (web_search, mcp, …) are SKIPPED here — the converted
/// request only drives routing/limits, and a `protocol: responses`
/// deployment must be able to forward them natively. The strict rejection
/// for conversion deployments lives in [`validate_conversion_supported`],
/// called by the handler after routing decides the path.
fn convert_tools(tools: &[serde_json::Value]) -> Result<Option<Vec<Tool>>, GatewayError> {
    let mut out = Vec::new();
    for t in tools {
        let ty = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if ty != "function" && ty != "custom" {
            continue; // passthrough-only tool — dropped from the chat view
        }
        let name = t
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| GatewayError::UnsupportedMode("tool is missing 'name'".to_string()))?;
        match ty {
            "function" => out.push(Tool {
                tool_type: "function".to_string(),
                function: ToolFunction {
                    name: name.to_string(),
                    description: t.get("description").and_then(|x| x.as_str()).map(String::from),
                    parameters: t
                        .get("parameters")
                        .cloned()
                        .unwrap_or(serde_json::json!({ "type": "object" })),
                },
            }),
            // Freeform custom tool (Codex `apply_patch`): Chat Completions has
            // no custom-tool type — wrap as a function with a single `input`
            // string parameter; the exit side restores the custom contract
            // (Response.md §7 P0.5, vLLM #49724 is the coercion failure).
            _ => out.push(Tool {
                tool_type: "function".to_string(),
                function: ToolFunction {
                    name: name.to_string(),
                    description: t.get("description").and_then(|x| x.as_str()).map(String::from),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "The freeform text input for this tool",
                            }
                        },
                        "required": ["input"],
                        "additionalProperties": false,
                    }),
                },
            }),
        }
    }
    if out.is_empty() {
        Ok(None) // everything skipped or empty list — no tools upstream
    } else {
        Ok(Some(out))
    }
}

/// Strict pre-flight for CONVERSION deployments (chat-kernel expressibility):
/// the chat upstream can only honor `function`/`custom` tools and the known
/// input item types. Called by the handler AFTER routing — a passthrough
/// deployment never runs this, so native-only types reach their upstream
/// (Response.md R2 review finding).
pub fn validate_conversion_supported(req: &ResponsesRequest) -> Result<(), GatewayError> {
    if let Some(tools) = &req.tools {
        for t in tools {
            let ty = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if ty != "function" && ty != "custom" {
                return Err(GatewayError::UnsupportedMode(format!(
                    "unsupported tool type '{ty}' (only \"function\" and \"custom\" are supported)"
                )));
            }
            if t.get("name").and_then(|x| x.as_str()).is_none() {
                return Err(GatewayError::UnsupportedMode(
                    "tool is missing 'name'".to_string(),
                ));
            }
        }
    }
    if let ResponsesInput::Items(items) = &req.input {
        for item in items {
            let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("message");
            if !matches!(
                ty,
                "message"
                    | "function_call"
                    | "function_call_output"
                    | "custom_tool_call"
                    | "custom_tool_call_output"
                    | "reasoning"
            ) {
                return Err(GatewayError::UnsupportedMode(format!(
                    "unsupported input item type '{ty}'"
                )));
            }
        }
    }
    Ok(())
}

/// Responses tool_choice is flat, chat is nested (Response.md R1):
/// `"auto"/"none"/"required"` pass through, `"any"` → `"required"`,
/// `{"type":"function","name"}` → `{"type":"function","function":{"name"}}`.
fn convert_tool_choice(v: &serde_json::Value) -> Result<serde_json::Value, GatewayError> {
    match v {
        serde_json::Value::String(s) => Ok(if s == "any" {
            serde_json::json!("required")
        } else {
            v.clone()
        }),
        serde_json::Value::Object(o) => match o.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "function" => {
                let name = o.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                Ok(serde_json::json!({ "type": "function", "function": { "name": name } }))
            }
            other => Err(GatewayError::UnsupportedMode(format!(
                "unsupported tool_choice type '{other}'"
            ))),
        },
        _ => Err(GatewayError::UnsupportedMode(
            "tool_choice must be a string or an object".to_string(),
        )),
    }
}

/// `text.format` → chat `response_format` (Response.md R12): Responses is
/// flat, chat nests json_schema fields under a `json_schema` key.
fn convert_text_format(text: &serde_json::Value) -> Result<Option<serde_json::Value>, GatewayError> {
    let Some(format) = text.get("format") else {
        return Ok(None);
    };
    let ty = format.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "text" | "" => Ok(None),
        "json_object" => Ok(Some(serde_json::json!({ "type": "json_object" }))),
        "json_schema" => {
            let mut js = serde_json::Map::new();
            for key in ["name", "description", "schema", "strict"] {
                if let Some(v) = format.get(key) {
                    js.insert(key.to_string(), v.clone());
                }
            }
            Ok(Some(serde_json::json!({ "type": "json_schema", "json_schema": js })))
        }
        other => Err(GatewayError::UnsupportedMode(format!(
            "unsupported text.format type '{other}'"
        ))),
    }
}
/// Incrementally extract the value of a top-level string field from a
/// (possibly truncated) JSON object — the stream-side decoder for wrapped
/// custom-tool arguments `{"input":"<freeform>"}`.
///
/// Returns `(decoded_so_far, closed)`: escapes (`\"` `\\` `\n` `\t` `\r`
/// `\b` `\f` `\uXXXX`) are decoded as they complete; a stream cut mid-string
/// yields the decodable prefix with `closed == false`. Re-scanning the whole
/// buffer per fragment keeps the state machine trivial and never splits an
/// escape across emitted deltas.
fn extract_json_string_field(raw: &str, field: &str) -> Option<(String, bool)> {
    let needle = format!("\"{field}\"");
    let pos = raw.find(&needle)?;
    let mut chars = raw[pos + needle.len()..].chars();

    // ':' (whitespace tolerated around it)
    let mut saw_colon = false;
    for c in chars.by_ref() {
        if c == ':' {
            saw_colon = true;
            break;
        }
        if !c.is_whitespace() {
            return None;
        }
    }
    if !saw_colon {
        return None;
    }
    // opening quote of the string value
    for c in chars.by_ref() {
        if c == '"' {
            break;
        }
        if !c.is_whitespace() {
            return None;
        }
    }

    let mut out = String::new();
    let mut closed = false;
    let mut escape = false;
    let mut unicode: Option<String> = None;
    for c in chars {
        if let Some(hex) = unicode.as_mut() {
            hex.push(c);
            if hex.len() == 4 {
                let decoded_char =
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
                if let Some(ch) = decoded_char {
                    out.push(ch);
                }
                unicode = None;
            }
            continue;
        }
        if escape {
            escape = false;
            match c {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000C}'),
                'u' => unicode = Some(String::new()),
                other => out.push(other), // \" \\ \/ and pass-through unknowns
            }
            continue;
        }
        match c {
            '\\' => escape = true,
            '"' => {
                closed = true;
                break;
            }
            other => out.push(other),
        }
    }
    Some((out, closed))
}

fn new_item_id(prefix: &str) -> String {
    format!("{}_{}", prefix, uuid::Uuid::new_v4().simple())
}

/// Convert a Responses request into the internal ChatCompletion format.
/// Validation runs first (same errors as `validate_supported_request`).
pub fn responses_request_to_openai(
    req: &ResponsesRequest,
) -> Result<ChatCompletionRequest, GatewayError> {
    convert_request(req, &[])
}

/// P1 variant: rehydrated history items (from the response store) are
/// converted ahead of the current input — "next context = prior context +
/// prior output + new input". The caller owns tools inheritance (it rewrites
/// `req.tools` before calling when the client omitted them).
pub fn responses_request_to_openai_with_history(
    req: &ResponsesRequest,
    history: &[serde_json::Value],
) -> Result<ChatCompletionRequest, GatewayError> {
    convert_request(req, history)
}

/// The current input as storable Responses-shaped items: text sugar expands
/// to one user message, type-less messages get an explicit `"message"` tag so
/// rehydrated history round-trips uniformly.
pub fn normalize_input_items(req: &ResponsesRequest) -> Vec<serde_json::Value> {
    match &req.input {
        ResponsesInput::Text(s) => vec![serde_json::json!({
            "type": "message", "role": "user", "content": s,
        })],
        ResponsesInput::Items(items) => items
            .iter()
            .map(|item| {
                let mut v = item.clone();
                if v.get("type").is_none() {
                    v["type"] = serde_json::json!("message");
                }
                v
            })
            .collect(),
    }
}

fn convert_request(
    req: &ResponsesRequest,
    history: &[serde_json::Value],
) -> Result<ChatCompletionRequest, GatewayError> {
    validate_supported_request(req)?;

    let mut messages = Vec::new();

    // instructions → leading system message (never inherited across turns;
    // storage-side concern in P1).
    if let Some(instr) = req.instructions.as_ref().filter(|s| !s.is_empty()) {
        messages.push(Message {
            role: MessageRole::System,
            content: MessageContent::Text(instr.clone()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        });
    }

    // input: rehydrated history first, then the current input (string sugar ≡
    // one user message); items are classified one by one.
    let mut items: Vec<serde_json::Value> = history.to_vec();
    match &req.input {
        ResponsesInput::Text(s) => {
            items.push(serde_json::json!({ "role": "user", "content": s }))
        }
        ResponsesInput::Items(current) => items.extend(current.iter().cloned()),
    }
    for item in &items {
        // LENIENT (R2): unknown item types are skipped in the chat view —
        // strict rejection for conversion deployments happens post-routing
        // (validate_conversion_supported), so passthrough deployments can
        // forward native-only items.
        let Ok(kind) = parse_input_item(item) else {
            continue;
        };
        match kind {
            InputKind::Message { role, content } => messages.push(Message {
                role: chat_role(&role),
                content: message_content_from(&content),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            }),
            InputKind::FunctionCall { call_id, name, arguments } => {
                let call_id = if call_id.is_empty() { new_item_id("call") } else { call_id };
                messages.push(Message {
                    role: MessageRole::Assistant,
                    content: MessageContent::Text(String::new()),
                    name: None,
                    tool_calls: Some(vec![ToolCall {
                        id: call_id,
                        call_type: "function".to_string(),
                        function: FunctionCall { name, arguments },
                    }]),
                    tool_call_id: None,
                    reasoning_content: None,
                });
            }
            InputKind::FunctionCallOutput { call_id, output } => messages.push(Message {
                role: MessageRole::Tool,
                content: MessageContent::Text(output_to_text(&output)),
                name: None,
                tool_calls: None,
                tool_call_id: Some(call_id),
                reasoning_content: None,
            }),
            InputKind::CustomToolCall { call_id, name, input } => {
                // Replay as the wrapped function call the upstream saw.
                let call_id = if call_id.is_empty() { new_item_id("call") } else { call_id };
                messages.push(Message {
                    role: MessageRole::Assistant,
                    content: MessageContent::Text(String::new()),
                    name: None,
                    tool_calls: Some(vec![ToolCall {
                        id: call_id,
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name,
                            arguments: serde_json::json!({ "input": input }).to_string(),
                        },
                    }]),
                    tool_call_id: None,
                    reasoning_content: None,
                });
            }
            InputKind::Reasoning => {} // dropped by design
        }
    }

    let tools = match req.tools.as_deref() {
        Some(t) => convert_tools(t)?,
        None => None,
    };
    let tool_choice = match req.tool_choice.as_ref() {
        Some(v) => Some(convert_tool_choice(v)?),
        None => None,
    };
    let response_format = match req.text.as_ref() {
        Some(t) => convert_text_format(t)?,
        None => None,
    };

    // Unknown fields forward wholesale; gateway-side params live in explicit
    // fields above so they never leak here.
    let mut extra = req.extra.clone();
    if let Some(ptc) = req.parallel_tool_calls {
        extra.insert("parallel_tool_calls".to_string(), serde_json::json!(ptc));
    }
    if let Some(effort) = req.reasoning.as_ref().and_then(|r| r.effort.clone()) {
        extra.insert("reasoning_effort".to_string(), serde_json::json!(effort));
    }

    Ok(ChatCompletionRequest {
        model: req.model.clone(),
        messages,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: None,
        max_completion_tokens: req.max_output_tokens,
        stream: req.stream,
        stop: None,
        n: None,
        tools,
        tool_choice,
        response_format,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        user: None,
        logprobs: None,
        top_logprobs: None,
        logit_bias: None,
        extra,
        gateway_headers: HashMap::new(),
        kv_cache_report_full: false,
        from_anthropic_protocol: false,
        raw_capture: None,
    })
}

// ============================================================
// Response conversion: OpenAI chat → Responses
// ============================================================

fn message_output_item(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": new_item_id("msg"),
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
    })
}

fn reasoning_output_item(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": new_item_id("rs"),
        "type": "reasoning",
        "summary": [{ "type": "summary_text", "text": text }],
    })
}

/// Convert a non-stream ChatCompletionResponse into a Response object.
/// `response_id`/`created_at` are supplied by the handler so stream and
/// non-stream paths mint ids the same way.
pub fn openai_chat_to_responses(
    resp: &ChatCompletionResponse,
    model: &str,
    response_id: &str,
    created_at: u64,
) -> ResponseObject {
    openai_chat_to_responses_with_custom_tools(resp, model, response_id, created_at, &Default::default())
}

/// P0.5 variant: tool calls whose name is in `custom_tools` (the request's
/// `type:"custom"` declarations) come back as `custom_tool_call` items with
/// the freeform `input` unwrapped from the wrapped function arguments —
/// Codex depends on this contract (coercion to function_call breaks it,
/// vLLM #49724).
pub fn openai_chat_to_responses_with_custom_tools(
    resp: &ChatCompletionResponse,
    model: &str,
    response_id: &str,
    created_at: u64,
    custom_tools: &std::collections::HashSet<String>,
) -> ResponseObject {
    let mut output: Vec<serde_json::Value> = Vec::new();
    let mut status = "completed".to_string();
    let mut incomplete_details = None;

    if let Some(choice) = resp.choices.first() {
        // n>1 / multi-choice: Responses has no n — first choice only.
        if let Some(r) = choice.message.reasoning_content.as_ref().filter(|s| !s.is_empty()) {
            output.push(reasoning_output_item(r));
        }
        let text = match &choice.message.content {
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
        if !text.is_empty() {
            output.push(message_output_item(&text));
        }
        for tc in choice.message.tool_calls.as_deref().unwrap_or(&[]) {
            let call_id = if tc.id.is_empty() { new_item_id("call") } else { tc.id.clone() };
            if custom_tools.contains(tc.function.name.as_str()) {
                // Wrapped custom tool → restore the freeform contract.
                let input = extract_json_string_field(&tc.function.arguments, "input")
                    .map(|(text, _)| text)
                    .unwrap_or_else(|| tc.function.arguments.clone());
                output.push(serde_json::json!({
                    "id": new_item_id("ctc"),
                    "type": "custom_tool_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": tc.function.name,
                    "input": input,
                }));
            } else {
                output.push(serde_json::json!({
                    "id": new_item_id("fc"),
                    "type": "function_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": tc.function.name,
                    "arguments": tc.function.arguments,
                }));
            }
        }
        match choice.finish_reason.as_deref() {
            // length/content_filter surface as the response-level incomplete
            // status — Responses has no finish_reason field (Response.md §6.4).
            Some("length") => {
                status = "incomplete".to_string();
                incomplete_details =
                    Some(ResponsesIncompleteDetails { reason: "max_output_tokens".to_string() });
            }
            Some("content_filter") => {
                status = "incomplete".to_string();
                incomplete_details =
                    Some(ResponsesIncompleteDetails { reason: "content_filter".to_string() });
            }
            _ => {}
        }
    }

    let usage = resp.usage.as_ref().map(|u| ResponsesUsage {
        input_tokens: u.prompt_tokens as u64,
        output_tokens: u.completion_tokens as u64,
        total_tokens: u.total_tokens as u64,
        input_tokens_details: u
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .map(|c| ResponsesCachedTokensDetail { cached_tokens: c as u64 }),
    });

    ResponseObject {
        id: response_id.to_string(),
        object: "response",
        created_at,
        status,
        model: model.to_string(),
        output,
        usage,
        incomplete_details,
        error: None,
    }
}

// ============================================================
// Stream transcoder: OpenAI chunks → Responses SSE events
// ============================================================

/// Stateful transcoder converting OpenAI-shaped stream chunks into Responses
/// SSE events. Call order: `begin()` before the upstream connection is
/// attempted, `transcode()` per chunk, then exactly one of `drain()` (clean
/// or interrupted end) or `fail()` (upstream error after begin).
pub struct ResponsesStreamTranscoder {
    response_id: String,
    model: String,
    created_at: u64,
    seq: u64,
    next_output_index: u32,
    reasoning: Option<ReasoningItemState>,
    message: Option<MessageItemState>,
    tools: BTreeMap<u32, ToolItemState>,
    /// Completed items as (output_index, value); sorted at terminal time.
    completed: Vec<(u32, serde_json::Value)>,
    usage: Option<ResponsesUsage>,
    finish_reason: Option<String>,
    terminal_emitted: bool,
    began: bool,
    /// The terminal response object (set by drain/fail) — the exact payload
    /// the terminal event carried. Consumed by the P1 persist path.
    final_shell: Option<serde_json::Value>,
    /// P0.5: names declared as `type:"custom"` in the request — tool calls
    /// matching these restore the custom_tool_call contract.
    custom_tools: std::collections::HashSet<String>,
}

struct ReasoningItemState {
    output_index: u32,
    item_id: String,
    text: String,
}

struct MessageItemState {
    output_index: u32,
    item_id: String,
    text: String,
}

struct ToolItemState {
    output_index: u32,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    /// P0.5: name matches a `type:"custom"` declaration — stream decoded
    /// freeform input (`custom_tool_call_input.delta`) instead of raw JSON
    /// argument deltas, and close as a `custom_tool_call` item.
    custom: bool,
    input_text: String,
    emitted_input_len: usize,
}

impl ResponsesStreamTranscoder {
    pub fn new(model: &str, response_id: &str, created_at: u64) -> Self {
        Self {
            response_id: response_id.to_string(),
            model: model.to_string(),
            created_at,
            seq: 0,
            next_output_index: 0,
            reasoning: None,
            message: None,
            tools: BTreeMap::new(),
            completed: Vec::new(),
            usage: None,
            finish_reason: None,
            terminal_emitted: false,
            began: false,
            final_shell: None,
            custom_tools: std::collections::HashSet::new(),
        }
    }

    /// P0.5: register the request's `type:"custom"` tool names before the
    /// first chunk — tool calls for them restore `custom_tool_call` items.
    pub fn with_custom_tools(mut self, names: std::collections::HashSet<String>) -> Self {
        self.custom_tools = names;
        self
    }

    /// The terminal response object, if a terminal event was emitted. `None`
    /// while the stream is still open.
    pub fn final_response_object(&self) -> Option<&serde_json::Value> {
        self.final_shell.as_ref()
    }

    fn next_seq(&mut self) -> u64 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    /// Stamp `type` + `sequence_number` onto the payload and render the wire
    /// event. Contiguous-from-0 numbering holds because every event in the
    /// stream goes through here.
    fn event(&mut self, ty: &str, mut payload: serde_json::Value) -> ResponsesSseEvent {
        let seq = self.next_seq();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("type".to_string(), serde_json::json!(ty));
            obj.insert("sequence_number".to_string(), serde_json::json!(seq));
        }
        ResponsesSseEvent { event: ty.to_string(), data: payload.to_string() }
    }

    /// Base response object carried by lifecycle/terminal events. Completed
    /// items are sorted by output_index (open order), not close order.
    fn response_shell(
        &self,
        status: &str,
        incomplete_reason: Option<&str>,
        error: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let mut items = self.completed.clone();
        items.sort_by_key(|(index, _)| *index);
        serde_json::json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": items.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
            "usage": self.usage,
            "incomplete_details": incomplete_reason
                .map(|r| serde_json::json!({ "reason": r })),
            "error": error,
        })
    }

    /// Emit response.created (seq 0) + response.in_progress (seq 1) BEFORE
    /// connecting upstream — the OpenAI contract sends `created` as soon as
    /// the response object exists (output: [], usage: null).
    pub fn begin(&mut self) -> Vec<ResponsesSseEvent> {
        self.began = true;
        let shell = self.response_shell("in_progress", None, None);
        vec![
            self.event("response.created", serde_json::json!({ "response": shell })),
            self.event("response.in_progress", serde_json::json!({ "response": shell })),
        ]
    }

    /// Convert one chunk into zero or more events.
    pub fn transcode(&mut self, chunk: &ChatStreamChunk) -> Vec<ResponsesSseEvent> {
        let mut events = Vec::new();

        // Usage may ride on any chunk (final-chunk, separate-chunk, or
        // combined-with-finish); always take the latest.
        if let Some(u) = &chunk.usage {
            self.usage = Some(ResponsesUsage {
                input_tokens: u.prompt_tokens.unwrap_or(0).max(0) as u64,
                output_tokens: u.completion_tokens.unwrap_or(0).max(0) as u64,
                total_tokens: u.total_tokens.unwrap_or(0).max(0) as u64,
                input_tokens_details: u
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
                    .map(|c| ResponsesCachedTokensDetail { cached_tokens: c as u64 }),
            });
        }

        for choice in &chunk.choices {
            // Reasoning deltas (DeepSeek/GLM `reasoning_content`) → rs_ item.
            if let Some(r) = choice.delta.reasoning_content.as_ref().filter(|s| !s.is_empty()) {
                if self.reasoning.is_none() {
                    let item_id = new_item_id("rs");
                    let output_index = self.next_output_index;
                    self.next_output_index += 1;
                    events.push(self.event(
                        "response.output_item.added",
                        serde_json::json!({
                            "output_index": output_index,
                            "item": { "id": item_id, "type": "reasoning", "summary": [] },
                        }),
                    ));
                    self.reasoning =
                        Some(ReasoningItemState { output_index, item_id, text: String::new() });
                }
                let st = self.reasoning.as_mut().expect("reasoning state just opened");
                st.text.push_str(r);
                let (item_id, output_index) = (st.item_id.clone(), st.output_index);
                events.push(self.event(
                    "response.reasoning_text.delta",
                    serde_json::json!({
                        "item_id": item_id, "output_index": output_index, "delta": r,
                    }),
                ));
            }

            // Text deltas → msg_ item (+ its single content part).
            if let Some(text) = choice.delta.content.as_ref().filter(|s| !s.is_empty()) {
                if self.message.is_none() {
                    let item_id = new_item_id("msg");
                    let output_index = self.next_output_index;
                    self.next_output_index += 1;
                    events.push(self.event(
                        "response.output_item.added",
                        serde_json::json!({
                            "output_index": output_index,
                            "item": {
                                "id": item_id, "type": "message", "role": "assistant",
                                "status": "in_progress", "content": [],
                            },
                        }),
                    ));
                    events.push(self.event(
                        "response.content_part.added",
                        serde_json::json!({
                            "item_id": item_id, "output_index": output_index, "content_index": 0,
                            "part": { "type": "output_text", "text": "", "annotations": [] },
                        }),
                    ));
                    self.message =
                        Some(MessageItemState { output_index, item_id, text: String::new() });
                }
                let st = self.message.as_mut().expect("message state just opened");
                st.text.push_str(text);
                let (item_id, output_index) = (st.item_id.clone(), st.output_index);
                events.push(self.event(
                    "response.output_text.delta",
                    serde_json::json!({
                        "item_id": item_id, "output_index": output_index,
                        "content_index": 0, "delta": text,
                    }),
                ));
            }

            // Tool-call deltas, keyed by upstream ToolCallDelta.index (the
            // kernel leniently defaults a missing index to 0).
            if let Some(tool_calls) = choice.delta.tool_calls.as_ref() {
                for tc in tool_calls {
                    if !self.tools.contains_key(&tc.index) {
                        let call_id = tc
                            .id
                            .clone()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| new_item_id("call"));
                        let name = tc
                            .function
                            .as_ref()
                            .and_then(|f| f.name.clone())
                            .unwrap_or_default();
                        let custom = self.custom_tools.contains(&name);
                        let item_id = if custom { new_item_id("ctc") } else { new_item_id("fc") };
                        let output_index = self.next_output_index;
                        self.next_output_index += 1;
                        // Custom tools open as custom_tool_call with freeform
                        // `input` ("" until decoded) — Codex depends on the
                        // item type; function tools keep the usual shape.
                        let item = if custom {
                            serde_json::json!({
                                "id": item_id, "type": "custom_tool_call",
                                "status": "in_progress", "call_id": call_id,
                                "name": name, "input": "",
                            })
                        } else {
                            serde_json::json!({
                                "id": item_id, "type": "function_call",
                                "status": "in_progress", "call_id": call_id,
                                "name": name, "arguments": "",
                            })
                        };
                        events.push(self.event(
                            "response.output_item.added",
                            serde_json::json!({
                                "output_index": output_index,
                                "item": item,
                            }),
                        ));
                        self.tools.insert(
                            tc.index,
                            ToolItemState {
                                output_index,
                                item_id,
                                call_id,
                                name,
                                arguments: String::new(),
                                custom,
                                input_text: String::new(),
                                emitted_input_len: 0,
                            },
                        );
                    } else {
                        // Late-arriving id/name fragments still count for the
                        // done event even if they missed the added event.
                        let st = self.tools.get_mut(&tc.index).expect("checked above");
                        if st.call_id.is_empty() {
                            if let Some(id) = tc.id.clone().filter(|s| !s.is_empty()) {
                                st.call_id = id;
                            }
                        }
                        if st.name.is_empty() {
                            if let Some(n) = tc
                                .function
                                .as_ref()
                                .and_then(|f| f.name.clone())
                                .filter(|s| !s.is_empty())
                            {
                                st.name = n;
                            }
                        }
                    }
                    if let Some(args) = tc
                        .function
                        .as_ref()
                        .and_then(|f| f.arguments.as_ref())
                        .filter(|s| !s.is_empty())
                    {
                        let st = self.tools.get_mut(&tc.index).expect("opened or present");
                        st.arguments.push_str(args);
                        if st.custom {
                            // Re-scan decodes only what is unambiguously
                            // complete — escapes never split across deltas.
                            if let Some((decoded, _closed)) =
                                extract_json_string_field(&st.arguments, "input")
                            {
                                if decoded.len() > st.emitted_input_len {
                                    let chunk = decoded[st.emitted_input_len..].to_string();
                                    st.emitted_input_len = decoded.len();
                                    st.input_text = decoded;
                                    let (item_id, output_index) =
                                        (st.item_id.clone(), st.output_index);
                                    events.push(self.event(
                                        "response.custom_tool_call_input.delta",
                                        serde_json::json!({
                                            "item_id": item_id,
                                            "output_index": output_index,
                                            "delta": chunk,
                                        }),
                                    ));
                                }
                            }
                        } else {
                            let (item_id, output_index) = (st.item_id.clone(), st.output_index);
                            events.push(self.event(
                                "response.function_call_arguments.delta",
                                serde_json::json!({
                                    "item_id": item_id, "output_index": output_index, "delta": args,
                                }),
                            ));
                        }
                    }
                }
            }

            // finish_reason closes all open items; the terminal event waits
            // for drain() so a trailing usage chunk is still folded in.
            if choice.finish_reason.is_some() {
                self.finish_reason = choice.finish_reason.clone();
                self.close_open_items(&mut events);
            }
        }

        events
    }

    /// Close every open item, emitting done events and freezing the item
    /// values for the terminal response object.
    fn close_open_items(&mut self, events: &mut Vec<ResponsesSseEvent>) {
        if let Some(rs) = self.reasoning.take() {
            events.push(self.event(
                "response.reasoning_text.done",
                serde_json::json!({
                    "item_id": rs.item_id, "output_index": rs.output_index, "text": rs.text,
                }),
            ));
            let item = serde_json::json!({
                "id": rs.item_id, "type": "reasoning",
                "summary": [{ "type": "summary_text", "text": rs.text }],
            });
            events.push(self.event(
                "response.output_item.done",
                serde_json::json!({ "output_index": rs.output_index, "item": item }),
            ));
            self.completed.push((rs.output_index, item));
        }
        if let Some(msg) = self.message.take() {
            events.push(self.event(
                "response.output_text.done",
                serde_json::json!({
                    "item_id": msg.item_id, "output_index": msg.output_index,
                    "content_index": 0, "text": msg.text,
                }),
            ));
            let part = serde_json::json!({
                "type": "output_text", "text": msg.text, "annotations": [],
            });
            events.push(self.event(
                "response.content_part.done",
                serde_json::json!({
                    "item_id": msg.item_id, "output_index": msg.output_index,
                    "content_index": 0, "part": part,
                }),
            ));
            let item = serde_json::json!({
                "id": msg.item_id, "type": "message", "role": "assistant",
                "status": "completed", "content": [part],
            });
            events.push(self.event(
                "response.output_item.done",
                serde_json::json!({ "output_index": msg.output_index, "item": item }),
            ));
            self.completed.push((msg.output_index, item));
        }
        let indices: Vec<u32> = self.tools.keys().copied().collect();
        for index in indices {
            let tool = self.tools.remove(&index).expect("key came from the map");
            let item = if tool.custom {
                // Final input: prefer the decoder; malformed JSON falls back
                // to the raw argument text (never lose the model's output).
                let input = extract_json_string_field(&tool.arguments, "input")
                    .map(|(text, _)| text)
                    .unwrap_or_else(|| tool.arguments.clone());
                events.push(self.event(
                    "response.custom_tool_call_input.done",
                    serde_json::json!({
                        "item_id": tool.item_id, "output_index": tool.output_index,
                        "input": input,
                    }),
                ));
                serde_json::json!({
                    "id": tool.item_id, "type": "custom_tool_call", "status": "completed",
                    "call_id": tool.call_id, "name": tool.name, "input": input,
                })
            } else {
                events.push(self.event(
                    "response.function_call_arguments.done",
                    serde_json::json!({
                        "item_id": tool.item_id, "output_index": tool.output_index,
                        "call_id": tool.call_id, "name": tool.name, "arguments": tool.arguments,
                    }),
                ));
                serde_json::json!({
                    "id": tool.item_id, "type": "function_call", "status": "completed",
                    "call_id": tool.call_id, "name": tool.name, "arguments": tool.arguments,
                })
            };
            events.push(self.event(
                "response.output_item.done",
                serde_json::json!({ "output_index": tool.output_index, "item": item }),
            ));
            self.completed.push((tool.output_index, item));
        }
    }

    /// End of stream (clean or interrupted): close open items, emit the
    /// terminal event (completed / incomplete). Missing finish_reason maps to
    /// incomplete. No-op after a terminal event was already emitted.
    pub fn drain(&mut self) -> Vec<ResponsesSseEvent> {
        if self.terminal_emitted {
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.began {
            events.extend(self.begin());
        }
        self.close_open_items(&mut events);
        let (event_type, status, reason) = match self.finish_reason.as_deref() {
            Some("length") => ("response.incomplete", "incomplete", Some("max_output_tokens")),
            Some("content_filter") => {
                ("response.incomplete", "incomplete", Some("content_filter"))
            }
            Some(_) => ("response.completed", "completed", None),
            None => ("response.incomplete", "incomplete", None),
        };
        let shell = self.response_shell(status, reason, None);
        self.final_shell = Some(shell.clone());
        events.push(self.event(event_type, serde_json::json!({ "response": shell })));
        self.terminal_emitted = true;
        events
    }

    /// Upstream error after `begin()`: emit response.failed. No-op after a
    /// terminal event was already emitted.
    pub fn fail(&mut self, err: &GatewayError) -> Vec<ResponsesSseEvent> {
        if self.terminal_emitted {
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.began {
            events.extend(self.begin());
        }
        self.close_open_items(&mut events);
        let error = serde_json::json!({
            "code": err.error_type(),
            "message": err.to_string(),
        });
        let shell = self.response_shell("failed", None, Some(error));
        self.final_shell = Some(shell.clone());
        events.push(self.event("response.failed", serde_json::json!({ "response": shell })));
        self.terminal_emitted = true;
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------- helpers ----------

    fn parse_req(v: serde_json::Value) -> ResponsesRequest {
        serde_json::from_value(v).expect("request should deserialize")
    }

    fn base_req() -> serde_json::Value {
        json!({ "model": "gpt-test", "input": "hello" })
    }

    fn chat_resp(content: &str, finish: Option<&str>) -> ChatCompletionResponse {
        serde_json::from_value(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 0, "model": "m",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": finish
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        }))
        .expect("chat response should deserialize")
    }

    fn delta_chunk(delta: serde_json::Value, finish: Option<&str>) -> ChatStreamChunk {
        serde_json::from_value(json!({
            "id": "chatcmpl-s", "object": "chat.completion.chunk", "created": 0, "model": "m",
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
        }))
        .expect("chunk should deserialize")
    }

    fn usage_chunk(pt: i64, ct: i64, cached: Option<i64>) -> ChatStreamChunk {
        let mut usage = json!({ "prompt_tokens": pt, "completion_tokens": ct, "total_tokens": pt + ct });
        if let Some(c) = cached {
            usage["prompt_tokens_details"] = json!({ "cached_tokens": c });
        }
        serde_json::from_value(json!({
            "id": "chatcmpl-s", "object": "chat.completion.chunk", "created": 0, "model": "m",
            "choices": [], "usage": usage
        }))
        .expect("usage chunk should deserialize")
    }

    fn data_of(ev: &ResponsesSseEvent) -> serde_json::Value {
        serde_json::from_str(&ev.data).expect("event data is JSON")
    }

    /// Text content of a message (panics on non-text), since MessageContent
    /// does not derive PartialEq.
    fn text_of(m: &Message) -> String {
        match &m.content {
            MessageContent::Text(t) => t.clone(),
            other => panic!("expected Text content, got {other:?}"),
        }
    }

    /// Wire name of a message role ("system"/"user"/…).
    fn role_of(m: &Message) -> String {
        serde_json::to_value(&m.role)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    fn collect_events(t: &mut ResponsesStreamTranscoder, chunks: &[ChatStreamChunk]) -> Vec<ResponsesSseEvent> {
        let mut out = t.begin();
        for c in chunks {
            out.extend(t.transcode(c));
        }
        out.extend(t.drain());
        out
    }

    /// Assert the wire invariants from Response.md §8: sequence numbers
    /// contiguous from 0, every event carries `type` == its event name.
    fn assert_invariants(events: &[ResponsesSseEvent]) {
        for (i, ev) in events.iter().enumerate() {
            let data = data_of(ev);
            assert_eq!(data["sequence_number"], json!(i), "seq must be contiguous from 0");
            assert_eq!(data["type"].as_str(), Some(ev.event.as_str()));
        }
    }

    // ---------- P0-1: request conversion ----------

    #[test]
    fn instructions_become_leading_system_message() {
        let req = parse_req(json!({
            "model": "m", "input": "hi", "instructions": "be brief"
        }));
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(out.messages.len(), 2);
        assert!(matches!(out.messages[0].role, MessageRole::System));
        assert_eq!(text_of(&out.messages[0]), "be brief");
        assert!(matches!(out.messages[1].role, MessageRole::User));
    }

    #[test]
    fn input_text_sugar_becomes_single_user_message() {
        let req = parse_req(base_req());
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(out.messages.len(), 1);
        assert!(matches!(out.messages[0].role, MessageRole::User));
        assert_eq!(text_of(&out.messages[0]), "hello");
    }

    #[test]
    fn message_roles_map_and_developer_is_rewritten_to_system() {
        // MessageRole is a closed enum with #[serde(other)] — a `developer`
        // role forwarded verbatim would degrade to Unknown/`"unknown"`.
        let req = parse_req(json!({
            "model": "m",
            "input": [
                { "role": "system", "content": "sys" },
                { "role": "developer", "content": "dev rules" },
                { "role": "user", "content": "q" },
                { "role": "assistant", "content": "a" }
            ]
        }));
        let out = responses_request_to_openai(&req).unwrap();
        let roles: Vec<String> = out.messages.iter().map(role_of).collect();
        assert_eq!(roles, vec!["system", "system", "user", "assistant"]);
    }

    #[test]
    fn message_content_parts_map_text_and_image() {
        let req = parse_req(json!({
            "model": "m",
            "input": [{
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "look:" },
                    { "type": "input_image", "image_url": "https://x/img.png" },
                    { "type": "output_text", "text": "prior" }
                ]
            }]
        }));
        let out = responses_request_to_openai(&req).unwrap();
        match &out.messages[0].content {
            MessageContent::Parts(parts) => {
                assert!(matches!(&parts[0], ContentPart::Text { text } if text == "look:"));
                assert!(matches!(&parts[1], ContentPart::ImageUrl { image_url } if image_url.url == "https://x/img.png"));
                // output_text (assistant history replay) is still just text.
                assert!(matches!(&parts[2], ContentPart::Text { text } if text == "prior"));
            }
            other => panic!("expected parts, got {other:?}"),
        }
    }

    #[test]
    fn function_call_roundtrip_uses_call_id() {
        let req = parse_req(json!({
            "model": "m",
            "input": [
                { "role": "user", "content": "weather?" },
                { "type": "function_call", "call_id": "call_9", "name": "get_weather", "arguments": "{\"city\":\"BJ\"}" },
                { "type": "function_call_output", "call_id": "call_9", "output": "sunny" }
            ]
        }));
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(out.messages.len(), 3);
        let assistant = &out.messages[1];
        assert!(matches!(assistant.role, MessageRole::Assistant));
        let tc = &assistant.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.id, "call_9");
        assert_eq!(tc.function.name, "get_weather");
        assert_eq!(tc.function.arguments, "{\"city\":\"BJ\"}");
        let tool_msg = &out.messages[2];
        assert!(matches!(tool_msg.role, MessageRole::Tool));
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_9"));
        assert_eq!(text_of(tool_msg), "sunny");
    }

    #[test]
    fn function_call_output_accepts_part_array_and_object() {
        let req = parse_req(json!({
            "model": "m",
            "input": [
                { "type": "function_call_output", "call_id": "c1",
                  "output": [{ "type": "output_text", "text": "part1 " }, { "type": "output_text", "text": "part2" }] },
                { "type": "function_call_output", "call_id": "c2", "output": { "rows": 3 } }
            ]
        }));
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(text_of(&out.messages[0]), "part1 part2");
        match &out.messages[1].content {
            MessageContent::Text(t) => assert!(t.contains("\"rows\":3")),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_items_are_dropped() {
        let req = parse_req(json!({
            "model": "m",
            "input": [
                { "type": "reasoning", "id": "rs_1", "summary": [] },
                { "role": "user", "content": "hi" }
            ]
        }));
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(out.messages.len(), 1);
    }

    #[test]
    fn function_tools_convert_to_chat_tools() {
        let req = parse_req(json!({
            "model": "m", "input": "hi",
            "tools": [{
                "type": "function", "name": "get_weather", "description": "w",
                "parameters": { "type": "object", "properties": { "city": { "type": "string" } } },
                "strict": true
            }]
        }));
        let out = responses_request_to_openai(&req).unwrap();
        let tools = out.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "get_weather");
        assert_eq!(tools[0].function.description.as_deref(), Some("w"));
        assert_eq!(tools[0].function.parameters["type"], "object");
    }

    #[test]
    fn empty_tools_fold_to_none() {
        let req = parse_req(json!({ "model": "m", "input": "hi", "tools": [] }));
        let out = responses_request_to_openai(&req).unwrap();
        assert!(out.tools.is_none());
    }

    #[test]
    fn non_function_tools_are_rejected() {
        // Strict pre-flight for conversion deployments (post-routing).
        let req = parse_req(json!({
            "model": "m", "input": "hi",
            "tools": [{ "type": "web_search" }]
        }));
        let err = validate_conversion_supported(&req).unwrap_err();
        assert_eq!(err.status_code(), 400);
        assert!(err.to_string().contains("web_search"));
    }

    #[test]
    fn lenient_conversion_drops_unknown_tools_and_items() {
        // R2: the chat view (routing/limits only) never rejects native-only
        // types — a passthrough deployment must be able to forward them.
        let req = parse_req(json!({
            "model": "m",
            "input": [
                { "role": "user", "content": "hi" },
                { "type": "web_search_call", "id": "ws_1" }
            ],
            "tools": [
                { "type": "web_search" },
                { "type": "function", "name": "f" }
            ]
        }));
        let out = responses_request_to_openai(&req).unwrap();
        // Unknown tool dropped, valid one kept.
        let tools = out.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "f");
        // Unknown input item dropped, message kept.
        assert_eq!(out.messages.len(), 1);
        assert_eq!(text_of(&out.messages[0]), "hi");
        // But the strict check still names both offenders.
        let err = validate_conversion_supported(&req).unwrap_err();
        assert_eq!(err.status_code(), 400);
        assert!(err.to_string().contains("web_search"));
    }

    #[test]
    fn tool_choice_strings_pass_through_and_any_becomes_required() {
        let mk = |v: serde_json::Value| responses_request_to_openai(&parse_req(json!({
            "model": "m", "input": "hi", "tool_choice": v
        })))
        .unwrap();
        assert_eq!(mk(json!("auto")).tool_choice, Some(json!("auto")));
        assert_eq!(mk(json!("none")).tool_choice, Some(json!("none")));
        assert_eq!(mk(json!("required")).tool_choice, Some(json!("required")));
        assert_eq!(mk(json!("any")).tool_choice, Some(json!("required")));
    }

    #[test]
    fn tool_choice_object_is_rewritten_to_nested_shape() {
        // Responses is flat {"type":"function","name"}; chat is nested.
        let req = parse_req(json!({
            "model": "m", "input": "hi",
            "tools": [{ "type": "function", "name": "f" }],
            "tool_choice": { "type": "function", "name": "f" }
        }));
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(
            out.tool_choice,
            Some(json!({ "type": "function", "function": { "name": "f" } }))
        );
    }

    #[test]
    fn tool_choice_mcp_shape_is_rejected() {
        let req = parse_req(json!({
            "model": "m", "input": "hi",
            "tool_choice": { "type": "mcp", "server_label": "s", "name": "n" }
        }));
        let err = responses_request_to_openai(&req).unwrap_err();
        assert_eq!(err.status_code(), 400);
    }

    #[test]
    fn max_output_tokens_maps_to_max_completion_tokens() {
        let req = parse_req(json!({ "model": "m", "input": "hi", "max_output_tokens": 128 }));
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(out.max_completion_tokens, Some(128));
        assert_eq!(out.max_tokens, None);
    }

    #[test]
    fn reasoning_effort_lands_in_extra() {
        let req = parse_req(json!({
            "model": "m", "input": "hi", "reasoning": { "effort": "medium" }
        }));
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(out.extra.get("reasoning_effort"), Some(&json!("medium")));
    }

    #[test]
    fn text_format_json_schema_is_nested_into_response_format() {
        let req = parse_req(json!({
            "model": "m", "input": "hi",
            "text": { "format": {
                "type": "json_schema", "name": "cal", "strict": true,
                "schema": { "type": "object" }
            }}
        }));
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(
            out.response_format,
            Some(json!({
                "type": "json_schema",
                "json_schema": { "name": "cal", "strict": true, "schema": { "type": "object" } }
            }))
        );
    }

    #[test]
    fn text_format_text_is_ignored_and_json_object_maps() {
        let text = parse_req(json!({
            "model": "m", "input": "hi", "text": { "format": { "type": "text" } }
        }));
        assert_eq!(responses_request_to_openai(&text).unwrap().response_format, None);

        let obj = parse_req(json!({
            "model": "m", "input": "hi", "text": { "format": { "type": "json_object" } }
        }));
        assert_eq!(
            responses_request_to_openai(&obj).unwrap().response_format,
            Some(json!({ "type": "json_object" }))
        );
    }

    #[test]
    fn unknown_text_format_is_rejected() {
        let req = parse_req(json!({
            "model": "m", "input": "hi", "text": { "format": { "type": "grammar" } }
        }));
        assert_eq!(responses_request_to_openai(&req).unwrap_err().status_code(), 400);
    }

    #[test]
    fn gateway_side_fields_never_reach_upstream_extra() {
        let req = parse_req(json!({
            "model": "m", "input": "hi",
            "store": true, "metadata": { "k": "v" }, "truncation": "auto",
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "user-42"
        }));
        let out = responses_request_to_openai(&req).unwrap();
        for absent in ["store", "metadata", "truncation", "include", "input", "model"] {
            assert!(out.extra.get(absent).is_none(), "{absent} leaked into upstream extra");
        }
        // Unknown fields DO forward (prompt_cache_key on chat upstream).
        assert_eq!(out.extra.get("prompt_cache_key"), Some(&json!("user-42")));
    }

    #[test]
    fn parallel_tool_calls_lands_in_extra() {
        let req = parse_req(json!({
            "model": "m", "input": "hi", "parallel_tool_calls": false
        }));
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(out.extra.get("parallel_tool_calls"), Some(&json!(false)));
    }

    #[test]
    fn guards_reject_unsupported_continuation_params() {
        // previous_response_id is NOT here: it is store policy, decided by
        // the handler (see previous_response_id_without_store) — P1.
        let conv = parse_req(json!({ "model": "m", "input": "hi", "conversation": "conv_1" }));
        assert_eq!(validate_supported_request(&conv).unwrap_err().status_code(), 400);

        let bg = parse_req(json!({ "model": "m", "input": "hi", "background": true }));
        assert_eq!(validate_supported_request(&bg).unwrap_err().status_code(), 400);

        // background:false is the default state — accepted.
        let bg_off = parse_req(json!({ "model": "m", "input": "hi", "background": false }));
        assert!(validate_supported_request(&bg_off).is_ok());
    }

    #[test]
    fn guard_rejects_unsupported_input_item_type() {
        let req = parse_req(json!({
            "model": "m",
            "input": [{ "type": "web_search_call", "id": "ws_1" }]
        }));
        let err = validate_conversion_supported(&req).unwrap_err();
        assert_eq!(err.status_code(), 400);
        assert!(err.to_string().contains("web_search_call"));
    }

    #[test]
    fn request_without_input_fails_to_parse() {
        assert!(serde_json::from_value::<ResponsesRequest>(json!({ "model": "m" })).is_err());
    }

    // ---------- P0-2: non-stream response conversion ----------

    #[test]
    fn text_response_becomes_message_output_item() {
        let resp = chat_resp("hello world", Some("stop"));
        let obj = openai_chat_to_responses(&resp, "gpt-test", "resp_abc", 1000);
        let v = serde_json::to_value(&obj).unwrap();
        assert_eq!(v["id"], "resp_abc");
        assert_eq!(v["object"], "response");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["model"], "gpt-test");
        assert_eq!(v["created_at"], 1000);
        assert_eq!(v["output"].as_array().unwrap().len(), 1);
        let item = &v["output"][0];
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "assistant");
        assert_eq!(item["status"], "completed");
        assert!(item["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(item["content"][0]["type"], "output_text");
        assert_eq!(item["content"][0]["text"], "hello world");
    }

    #[test]
    fn tool_calls_become_function_call_items() {
        let resp: ChatCompletionResponse = serde_json::from_value(json!({
            "id": "c1", "object": "chat.completion", "created": 0, "model": "m",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant", "content": null,
                    "tool_calls": [{ "id": "call_7", "type": "function",
                                     "function": { "name": "f", "arguments": "{\"x\":1}" } }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        }))
        .unwrap();
        let obj = openai_chat_to_responses(&resp, "m", "resp_1", 0);
        let v = serde_json::to_value(&obj).unwrap();
        assert_eq!(v["status"], "completed");
        let fc = &v["output"][0];
        assert_eq!(fc["type"], "function_call");
        assert!(fc["id"].as_str().unwrap().starts_with("fc_"));
        assert_eq!(fc["call_id"], "call_7");
        assert_eq!(fc["name"], "f");
        assert_eq!(fc["arguments"], "{\"x\":1}");
    }

    #[test]
    fn reasoning_content_becomes_leading_reasoning_item() {
        let mut resp = chat_resp("answer", Some("stop"));
        resp.choices[0].message.reasoning_content = Some("thinking…".into());
        let obj = openai_chat_to_responses(&resp, "m", "resp_1", 0);
        let v = serde_json::to_value(&obj).unwrap();
        assert_eq!(v["output"].as_array().unwrap().len(), 2);
        assert_eq!(v["output"][0]["type"], "reasoning");
        assert!(v["output"][0]["id"].as_str().unwrap().starts_with("rs_"));
        assert_eq!(v["output"][1]["type"], "message");
    }

    #[test]
    fn finish_length_maps_to_incomplete_with_details() {
        let resp = chat_resp("partial", Some("length"));
        let obj = openai_chat_to_responses(&resp, "m", "resp_1", 0);
        let v = serde_json::to_value(&obj).unwrap();
        assert_eq!(v["status"], "incomplete");
        assert_eq!(v["incomplete_details"]["reason"], "max_output_tokens");
        assert!(v.get("error").is_none() || v["error"].is_null());
    }

    #[test]
    fn finish_content_filter_maps_to_incomplete() {
        let resp = chat_resp("x", Some("content_filter"));
        let obj = openai_chat_to_responses(&resp, "m", "resp_1", 0);
        let v = serde_json::to_value(&obj).unwrap();
        assert_eq!(v["status"], "incomplete");
        assert_eq!(v["incomplete_details"]["reason"], "content_filter");
    }

    #[test]
    fn usage_maps_to_responses_naming_with_cached_tokens() {
        let mut resp = chat_resp("hi", Some("stop"));
        resp.usage = Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: Some(4) }),
        });
        let obj = openai_chat_to_responses(&resp, "m", "resp_1", 0);
        let v = serde_json::to_value(&obj).unwrap();
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert_eq!(v["usage"]["output_tokens"], 5);
        assert_eq!(v["usage"]["total_tokens"], 15);
        assert_eq!(v["usage"]["input_tokens_details"]["cached_tokens"], 4);
    }

    #[test]
    fn missing_usage_serializes_null() {
        let mut resp = chat_resp("hi", Some("stop"));
        resp.usage = None;
        let obj = openai_chat_to_responses(&resp, "m", "resp_1", 0);
        let v = serde_json::to_value(&obj).unwrap();
        assert!(v.get("usage").is_none() || v["usage"].is_null());
    }

    // ---------- P0-3: stream transcoder ----------

    #[test]
    fn begin_emits_created_seq0_and_in_progress_seq1() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let events = t.begin();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "response.created");
        assert_eq!(data_of(&events[0])["sequence_number"], 0);
        assert_eq!(data_of(&events[0])["response"]["status"], "in_progress");
        assert_eq!(data_of(&events[0])["response"]["output"], json!([]));
        assert_eq!(events[1].event, "response.in_progress");
        assert_eq!(data_of(&events[1])["sequence_number"], 1);
        assert_eq!(data_of(&events[1])["response"]["id"], "resp_x");
    }

    #[test]
    fn text_stream_full_lifecycle_and_invariants() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let events = collect_events(&mut t, &[
            delta_chunk(json!({ "content": "Hel" }), None),
            delta_chunk(json!({ "content": "lo" }), None),
            delta_chunk(json!({}), Some("stop")),
            usage_chunk(3, 2, None),
        ]);
        assert_invariants(&events);

        let names: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        // text.done carries the full concatenation.
        let done = data_of(&events[6]);
        assert_eq!(done["text"], "Hello");
        // Terminal event carries the response object with usage + output.
        let terminal = data_of(&events[9]);
        assert_eq!(terminal["response"]["status"], "completed");
        assert_eq!(terminal["response"]["usage"]["input_tokens"], 3);
        assert_eq!(terminal["response"]["usage"]["output_tokens"], 2);
        assert_eq!(terminal["response"]["output"][0]["content"][0]["text"], "Hello");
        // No [DONE] sentinel anywhere.
        assert!(events.iter().all(|e| e.data != "[DONE]"));
    }

    #[test]
    fn finish_and_usage_in_same_chunk_still_reach_terminal() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let mut finish = delta_chunk(json!({ "content": "hi" }), Some("stop"));
        finish.usage = Some(StreamUsage {
            prompt_tokens: Some(8),
            completion_tokens: Some(4),
            total_tokens: Some(12),
            prompt_tokens_details: None,
        });
        let events = collect_events(&mut t, &[finish]);
        assert_invariants(&events);
        let terminal = data_of(events.last().unwrap());
        assert_eq!(terminal["response"]["usage"]["input_tokens"], 8);
        assert_eq!(events.last().unwrap().event, "response.completed");
    }

    #[test]
    fn tool_stream_lifecycle() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let events = collect_events(&mut t, &[
            delta_chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function",
                "function": { "name": "f", "arguments": "" } }] }), None),
            delta_chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "{\"x\":" } }] }), None),
            delta_chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "1}" } }] }), None),
            delta_chunk(json!({}), Some("tool_calls")),
        ]);
        assert_invariants(&events);
        let names: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let added = data_of(&events[2]);
        assert_eq!(added["item"]["type"], "function_call");
        assert_eq!(added["item"]["call_id"], "call_1");
        assert_eq!(added["item"]["name"], "f");
        let args_done = data_of(&events[5]);
        assert_eq!(args_done["arguments"], "{\"x\":1}");
        let item_done = data_of(&events[6]);
        assert_eq!(item_done["item"]["arguments"], "{\"x\":1}");
        assert_eq!(item_done["item"]["status"], "completed");
    }

    #[test]
    fn text_then_tool_assigns_output_indices_in_open_order() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let events = collect_events(&mut t, &[
            delta_chunk(json!({ "content": "let me check" }), None),
            delta_chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function",
                "function": { "name": "f", "arguments": "{}" } }] }), None),
            delta_chunk(json!({}), Some("tool_calls")),
        ]);
        assert_invariants(&events);
        let msg_added = events.iter().find(|e| e.event == "response.output_item.added").unwrap();
        assert_eq!(data_of(msg_added)["output_index"], 0);
        let fc_added = events.iter().filter(|e| e.event == "response.output_item.added").nth(1).unwrap();
        assert_eq!(data_of(fc_added)["output_index"], 1);
        // Terminal output array respects open order: message first.
        let terminal = data_of(events.last().unwrap());
        assert_eq!(terminal["response"]["output"][0]["type"], "message");
        assert_eq!(terminal["response"]["output"][1]["type"], "function_call");
    }

    #[test]
    fn finish_length_emits_independent_incomplete_terminal() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let events = collect_events(&mut t, &[
            delta_chunk(json!({ "content": "partial" }), None),
            delta_chunk(json!({}), Some("length")),
        ]);
        assert_eq!(events.last().unwrap().event, "response.incomplete");
        let terminal = data_of(events.last().unwrap());
        assert_eq!(terminal["response"]["status"], "incomplete");
        assert_eq!(terminal["response"]["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn drain_without_finish_reason_emits_incomplete() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let events = collect_events(&mut t, &[
            delta_chunk(json!({ "content": "cut off" }), None),
        ]);
        assert_eq!(events.last().unwrap().event, "response.incomplete");
        // Open items were still closed properly.
        assert_eq!(events.iter().filter(|e| e.event == "response.output_item.done").count(), 1);
    }

    #[test]
    fn fail_emits_response_failed_terminal() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let mut events = t.begin();
        events.extend(t.fail(&GatewayError::ProviderError("upstream boom".into())));
        assert_invariants(&events);
        assert_eq!(events.last().unwrap().event, "response.failed");
        let terminal = data_of(events.last().unwrap());
        assert_eq!(terminal["response"]["status"], "failed");
        assert!(terminal["response"]["error"]["message"].as_str().unwrap().contains("upstream boom"));
    }

    #[test]
    fn drain_after_fail_is_noop() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let _ = t.begin();
        let _ = t.fail(&GatewayError::UpstreamTimeout);
        assert!(t.drain().is_empty());
    }

    #[test]
    fn reasoning_deltas_map_to_rs_item_events() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let events = collect_events(&mut t, &[
            delta_chunk(json!({ "reasoning_content": "step1 " }), None),
            delta_chunk(json!({ "reasoning_content": "step2" }), None),
            delta_chunk(json!({ "content": "answer" }), None),
            delta_chunk(json!({}), Some("stop")),
        ]);
        assert_invariants(&events);
        let names: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
        assert!(names.contains(&"response.reasoning_text.delta"));
        let rs_done = events.iter().find(|e| e.event == "response.reasoning_text.done").unwrap();
        assert_eq!(data_of(rs_done)["text"], "step1 step2");
        let terminal = data_of(events.last().unwrap());
        assert_eq!(terminal["response"]["output"][0]["type"], "reasoning");
        assert_eq!(terminal["response"]["output"][1]["type"], "message");
    }

    #[test]
    fn empty_stream_drains_to_incompleted_terminal() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let events = collect_events(&mut t, &[]);
        assert_eq!(events.len(), 3); // created + in_progress + incomplete
        assert_eq!(events.last().unwrap().event, "response.incomplete");
        assert_eq!(data_of(events.last().unwrap())["response"]["output"], json!([]));
    }

    #[test]
    fn usage_cached_tokens_reach_terminal_event() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        let events = collect_events(&mut t, &[
            delta_chunk(json!({ "content": "x" }), Some("stop")),
            usage_chunk(9, 6, Some(5)),
        ]);
        let terminal = data_of(events.last().unwrap());
        assert_eq!(terminal["response"]["usage"]["input_tokens_details"]["cached_tokens"], 5);
    }

    // ---------- P1: stateful continuation ----------

    #[test]
    fn previous_response_id_is_store_policy_not_core_validation() {
        // Whether previous_response_id is honored is the handler's call
        // (store enabled vs disabled); the conversion layer must accept it.
        let req = parse_req(json!({
            "model": "m", "input": "hi", "previous_response_id": "resp_1"
        }));
        assert!(validate_supported_request(&req).is_ok());
        assert!(responses_request_to_openai(&req).is_ok());
    }

    #[test]
    fn without_store_error_is_400_naming_the_param() {
        let e = previous_response_id_without_store();
        assert_eq!(e.status_code(), 400);
        assert!(e.to_string().contains("previous_response_id"));
    }

    #[test]
    fn history_items_precede_current_input() {
        let req = parse_req(json!({
            "model": "m",
            "input": [{ "role": "user", "content": "second question" }],
            "previous_response_id": "resp_1",
        }));
        let history = vec![
            json!({ "type": "message", "role": "user", "content": "first question" }),
            json!({ "type": "message", "role": "assistant", "content": "first answer" }),
        ];
        let out = responses_request_to_openai_with_history(&req, &history).unwrap();
        assert_eq!(out.messages.len(), 3);
        let texts: Vec<String> = out.messages.iter().map(text_of).collect();
        assert_eq!(texts, vec!["first question", "first answer", "second question"]);
    }

    #[test]
    fn instructions_stay_ahead_of_rehydrated_history() {
        let req = parse_req(json!({
            "model": "m", "input": "q2", "instructions": "sys",
            "previous_response_id": "resp_1",
        }));
        let history = vec![json!({ "type": "message", "role": "user", "content": "q1" })];
        let out = responses_request_to_openai_with_history(&req, &history).unwrap();
        assert_eq!(out.messages.len(), 3);
        assert!(matches!(out.messages[0].role, MessageRole::System));
        assert_eq!(text_of(&out.messages[0]), "sys");
        assert_eq!(text_of(&out.messages[1]), "q1");
    }

    #[test]
    fn normalize_input_items_expands_text_sugar_and_tags_messages() {
        let sugar = parse_req(json!({ "model": "m", "input": "plain" }));
        let items = normalize_input_items(&sugar);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"], "plain");

        // Items keep their shape; type-less messages get the explicit tag so
        // stored history round-trips uniformly through rehydrate.
        let explicit = parse_req(json!({
            "model": "m",
            "input": [
                { "role": "user", "content": "a" },
                { "type": "function_call", "call_id": "c", "name": "f", "arguments": "{}" },
                { "type": "function_call_output", "call_id": "c", "output": "r" }
            ]
        }));
        let items = normalize_input_items(&explicit);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[2]["type"], "function_call_output");
    }

    #[test]
    fn transcoder_exposes_final_response_object_only_after_terminal() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7);
        assert!(t.final_response_object().is_none());
        let events =
            collect_events(&mut t, &[delta_chunk(json!({ "content": "hi" }), Some("stop"))]);
        let final_obj = t.final_response_object().expect("terminal emitted above");
        assert_eq!(final_obj["id"], "resp_x");
        assert_eq!(final_obj["status"], "completed");
        assert_eq!(final_obj["output"][0]["content"][0]["text"], "hi");
        // Identical to the terminal event's `response` payload.
        let terminal = data_of(events.last().unwrap());
        assert_eq!(terminal["response"], *final_obj);
    }

    // ---------- P0.5: freeform custom tools (Codex contract) ----------

    fn custom_tool_names(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn custom_tool_declaration_is_wrapped_as_function() {
        // Chat Completions has no custom-tool type: wrap as a function with a
        // single `input` string parameter (Response.md §7 P0.5).
        let req = parse_req(json!({
            "model": "m", "input": "patch it",
            "tools": [{ "type": "custom", "name": "apply_patch", "description": "Apply a patch" }]
        }));
        let out = responses_request_to_openai(&req).unwrap();
        let tools = out.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "apply_patch");
        let params = &tools[0].function.parameters;
        assert_eq!(params["properties"]["input"]["type"], "string");
        assert_eq!(params["required"][0], "input");
    }

    #[test]
    fn mixed_function_and_custom_tools_both_convert() {
        let req = parse_req(json!({
            "model": "m", "input": "hi",
            "tools": [
                { "type": "function", "name": "f", "parameters": { "type": "object" } },
                { "type": "custom", "name": "apply_patch" }
            ]
        }));
        let out = responses_request_to_openai(&req).unwrap();
        let tools = out.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].function.parameters["type"], "object"); // untouched
        assert_eq!(tools[1].function.parameters["properties"]["input"]["type"], "string");
    }

    #[test]
    fn custom_tool_call_item_replays_as_wrapped_function_call() {
        // History replay of a custom call must look EXACTLY like what the
        // upstream saw originally: a function call with {"input": …} args.
        let req = parse_req(json!({
            "model": "m",
            "input": [
                { "role": "user", "content": "patch" },
                { "type": "custom_tool_call", "call_id": "call_c1", "name": "apply_patch",
                  "input": "*** Begin Patch\n+hi" },
                { "type": "custom_tool_call_output", "call_id": "call_c1", "output": "done" }
            ]
        }));
        let out = responses_request_to_openai(&req).unwrap();
        assert_eq!(out.messages.len(), 3);
        let assistant = &out.messages[1];
        let tc = &assistant.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.id, "call_c1");
        assert_eq!(tc.function.name, "apply_patch");
        let args: serde_json::Value =
            serde_json::from_str(&tc.function.arguments).expect("arguments are JSON");
        assert_eq!(args["input"], "*** Begin Patch\n+hi");
        let tool_msg = &out.messages[2];
        assert!(matches!(tool_msg.role, MessageRole::Tool));
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_c1"));
        assert_eq!(text_of(tool_msg), "done");
    }

    #[test]
    fn wrapped_custom_tool_call_restores_custom_item() {
        // Exit side: a function call whose name is in the request's custom
        // set must come back as custom_tool_call (Codex breaks on coercion,
        // vLLM #49724) — ctc_ id, freeform `input`, NOT a function_call.
        let mut resp = chat_resp("", Some("tool_calls"));
        resp.choices[0].message.content = MessageContent::Null;
        resp.choices[0].message.tool_calls = Some(vec![ToolCall {
            id: "call_c9".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "apply_patch".to_string(),
                arguments: r#"{"input":"*** Begin Patch\n+hi"}"#.to_string(),
            },
        }]);
        let obj = openai_chat_to_responses_with_custom_tools(
            &resp, "m", "resp_1", 0,
            &custom_tool_names(&["apply_patch"]),
        );
        let v = serde_json::to_value(&obj).unwrap();
        let item = &v["output"][0];
        assert_eq!(item["type"], "custom_tool_call");
        assert!(item["id"].as_str().unwrap().starts_with("ctc_"));
        assert_eq!(item["call_id"], "call_c9");
        assert_eq!(item["name"], "apply_patch");
        assert_eq!(item["input"], "*** Begin Patch\n+hi");
        assert!(v["output"].as_array().unwrap().iter().all(|i| i["type"] != "function_call"));
    }

    #[test]
    fn custom_extraction_falls_back_to_raw_arguments() {
        // Model emitted malformed JSON — degrade to the raw argument text.
        let mut resp = chat_resp("", Some("tool_calls"));
        resp.choices[0].message.content = MessageContent::Null;
        resp.choices[0].message.tool_calls = Some(vec![ToolCall {
            id: "c".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "apply_patch".into(),
                arguments: "not json at all".into(),
            },
        }]);
        let obj = openai_chat_to_responses_with_custom_tools(
            &resp, "m", "resp_1", 0, &custom_tool_names(&["apply_patch"]),
        );
        let v = serde_json::to_value(&obj).unwrap();
        assert_eq!(v["output"][0]["input"], "not json at all");
    }

    #[test]
    fn json_string_field_decoder_handles_escapes_and_partials() {
        use std::collections::HashMap;
        let _unused: HashMap<(), ()> = HashMap::new();
        // Full value with escapes.
        let (val, closed) =
            extract_json_string_field(r#"{"input":"a\nb\"c\\d中"}"#, "input").unwrap();
        assert_eq!(val, "a\nb\"c\\d\u{4e2d}");
        assert!(closed);
        // Partial (stream cut mid-string): decodes what's available.
        let (val, closed) = extract_json_string_field(r#"{"input":"hel"#, "input").unwrap();
        assert_eq!(val, "hel");
        assert!(!closed);
        // Partial escape contributes nothing yet.
        let (val, closed) = extract_json_string_field(r#"{"input":"x\u4e"#, "input").unwrap();
        assert_eq!(val, "x");
        assert!(!closed);
        // Missing key / not a string value.
        assert!(extract_json_string_field(r#"{"other":"x"}"#, "input").is_none());
        assert!(extract_json_string_field(r#"{"input":42}"#, "input").is_none());
        // Key found after whitespace and other fields.
        let (val, _) = extract_json_string_field(r#"{"z":1, "input" : "v"}"#, "input").unwrap();
        assert_eq!(val, "v");
    }

    #[test]
    fn custom_tool_stream_emits_ctc_events_not_function_events() {
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7)
            .with_custom_tools(custom_tool_names(&["apply_patch"]));
        let events = collect_events(&mut t, &[
            delta_chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_c1", "type": "function",
                "function": { "name": "apply_patch", "arguments": "{\"input\":\"*** Be" } }] }), None),
            delta_chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "gin Patch\\n+hi\"}" } }] }), None),
            delta_chunk(json!({}), Some("tool_calls")),
        ]);
        assert_invariants(&events);
        let names: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.custom_tool_call_input.delta",
                "response.custom_tool_call_input.delta",
                "response.custom_tool_call_input.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let added = data_of(&events[2]);
        assert_eq!(added["item"]["type"], "custom_tool_call");
        assert!(added["item"]["id"].as_str().unwrap().starts_with("ctc_"));
        assert_eq!(added["item"]["input"], "");
        // Deltas decode the freeform text incrementally (escape \n not split).
        let d1 = data_of(&events[3])["delta"].as_str().unwrap().to_string();
        let d2 = data_of(&events[4])["delta"].as_str().unwrap().to_string();
        let done = data_of(&events[5])["input"].as_str().unwrap().to_string();
        assert_eq!(format!("{d1}{d2}"), done);
        assert_eq!(done, "*** Begin Patch\n+hi");
        // Terminal output carries the restored custom item.
        let terminal = data_of(events.last().unwrap());
        assert_eq!(terminal["response"]["output"][0]["type"], "custom_tool_call");
        assert_eq!(terminal["response"]["output"][0]["input"], "*** Begin Patch\n+hi");
    }

    #[test]
    fn function_tool_stream_unaffected_by_custom_set() {
        // A tool NOT in the custom set keeps the function_call lifecycle.
        let mut t = ResponsesStreamTranscoder::new("m", "resp_x", 7)
            .with_custom_tools(custom_tool_names(&["apply_patch"]));
        let events = collect_events(&mut t, &[
            delta_chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function",
                "function": { "name": "get_weather", "arguments": "{}" } }] }), None),
            delta_chunk(json!({}), Some("tool_calls")),
        ]);
        let names: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
        assert!(names.contains(&"response.function_call_arguments.delta"));
        let terminal = data_of(events.last().unwrap());
        assert_eq!(terminal["response"]["output"][0]["type"], "function_call");
    }

    // ---------- P2: native passthrough ----------

    #[test]
    fn passthrough_body_strips_gateway_fields_and_inlines_history() {
        let req = parse_req(json!({
            "model": "m",
            "input": [{ "role": "user", "content": "q2" }],
            "previous_response_id": "resp_prev",
            "store": true,
            "conversation": "conv_1",
            "background": false,
            "instructions": "sys",
            "tools": [{ "type": "custom", "name": "apply_patch" }],
            "reasoning": { "effort": "high" },
            "prompt_cache_key": "u1",
        }));
        let history = vec![json!({ "type": "message", "role": "user", "content": "q1" })];
        let body = build_responses_passthrough_body(&req, &history, true);

        // Gateway-side fields never reach the upstream (the gateway owns
        // storage/continuation — same stripping as the conversion path).
        for absent in ["store", "previous_response_id", "conversation", "background"] {
            assert!(body.get(absent).is_none(), "{absent} leaked into passthrough body");
        }
        // History inlined ahead of the current input; stream flag explicit.
        assert_eq!(
            body["input"],
            json!([
                { "type": "message", "role": "user", "content": "q1" },
                { "type": "message", "role": "user", "content": "q2" },
            ])
        );
        assert_eq!(body["stream"], true);
        // Native fields survive verbatim — this is the fidelity point.
        assert_eq!(body["tools"][0]["type"], "custom");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["prompt_cache_key"], "u1");
    }

    #[test]
    fn passthrough_body_non_stream_and_empty_history() {
        let req = parse_req(json!({ "model": "m", "input": "hi" }));
        let body = build_responses_passthrough_body(&req, &[], false);
        assert_eq!(body["stream"], false);
        assert_eq!(
            body["input"],
            json!([{ "type": "message", "role": "user", "content": "hi" }])
        );
    }

    #[test]
    fn passthrough_id_rewrite_replaces_upstream_response_id_only() {
        let mut event = json!({
            "type": "response.completed",
            "sequence_number": 9,
            "response": { "id": "resp_UPSTREAM", "status": "completed",
                          "output": [{ "id": "msg_native", "type": "message" }] },
        });
        rewrite_passthrough_response_id(&mut event, "resp_gateway");
        assert_eq!(event["response"]["id"], "resp_gateway");
        // Item ids stay native — they round-trip to the upstream verbatim.
        assert_eq!(event["response"]["output"][0]["id"], "msg_native");
        assert_eq!(event["sequence_number"], 9);

        // Events without a response object pass through untouched.
        let mut delta = json!({ "type": "response.output_text.delta", "delta": "x" });
        rewrite_passthrough_response_id(&mut delta, "resp_gateway");
        assert_eq!(delta["delta"], "x");
    }

    #[test]
    fn passthrough_id_rewrite_handles_bare_response_object() {
        // Non-stream upstream reply: the Response object IS the payload (no
        // event wrapper) — its top-level id must be rewritten too.
        let mut bare = json!({
            "id": "resp_upstream_9", "object": "response", "status": "completed",
            "output": [{ "id": "rs_native_1", "type": "reasoning" }],
        });
        rewrite_passthrough_response_id(&mut bare, "resp_gateway");
        assert_eq!(bare["id"], "resp_gateway");
        assert_eq!(bare["output"][0]["id"], "rs_native_1");
    }

    #[test]
    fn passthrough_id_rewrite_injects_missing_id() {
        // Gateway-synthesized response.failed: the response object never had
        // an upstream id — the rewrite must inject the gateway's (clients
        // id-address every response object they see).
        let mut synthetic = json!({
            "type": "response.failed",
            "response": { "status": "failed", "error": { "message": "boom" } },
        });
        rewrite_passthrough_response_id(&mut synthetic, "resp_gateway");
        assert_eq!(synthetic["response"]["id"], "resp_gateway");
    }
}

