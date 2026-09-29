//! DT 用例 — boom-core::anthropic：Anthropic Messages ↔ OpenAI Chat 格式互转。
//!
//! 全部纯函数（含状态流式 transcoder），无网络无 DB。覆盖：
//! - anthropic_request_to_openai：system(Text/Blocks/空) / user(Text/Blocks) /
//!   assistant(Text/Blocks) / tools / stop_sequences(Single/Multiple) / extra(thinking+metadata)
//! - openai_response_to_anthropic：reasoning_content→thinking / content 各形态 /
//!   tool_calls / 空内容兜底 / stop_reason 映射 / usage 透传
//! - AnthropicStreamTranscoder：message_start / thinking 块 / text 块 / tool_call 块 /
//!   finish 持有+下块释放 / drain / close_open_block
//! - extract_system_text：Text / Blocks(过滤非 text，\n join)

use boom_core::anthropic::{
    anthropic_request_to_openai, extract_system_text, openai_response_to_anthropic,
    AnthropicStreamTranscoder,
};
use boom_core::types::{
    AnthropicContent, AnthropicContentBlock, AnthropicMessage, AnthropicMessagesRequest,
    AnthropicSystemBlock, AnthropicSystemContent, AnthropicTool, ChatCompletionResponse,
    ChatStreamChunk, ContentPart, MessageContent, MessageRole, Usage,
};

// ───────────────────────── helpers ─────────────────────────

fn req_with_messages(messages: Vec<AnthropicMessage>) -> AnthropicMessagesRequest {
    AnthropicMessagesRequest {
        model: "claude-3".to_string(),
        system: None,
        messages,
        max_tokens: Some(1024),
        tools: None,
        tool_choice: None,
        thinking: None,
        temperature: None,
        top_p: None,
        stop_sequences: None,
        stream: None,
        metadata: None,
        extra: serde_json::Map::new(),
    }
}

fn chunk_with(delta: serde_json::Value, finish_reason: Option<&str>) -> ChatStreamChunk {
    serde_json::from_value(serde_json::json!({
        "id": "chatcmpl-x",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "m",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
    }))
    .expect("chunk deserializes")
}

fn event_names(events: &[boom_core::anthropic::AnthropicSseEvent]) -> Vec<&str> {
    events.iter().map(|e| e.event.as_str()).collect()
}

// ═════════════════════════════════════════════════════════════
// extract_system_text
// ═════════════════════════════════════════════════════════════

/// DT-ANT-01：Text 形式直接返回；Blocks 形式过滤非 text 块后用 \n join。
#[test]
fn extract_system_text_forms() {
    assert_eq!(extract_system_text(&AnthropicSystemContent::Text("hi".into())), "hi");

    let blocks = AnthropicSystemContent::Blocks(vec![
        AnthropicSystemBlock {
            block_type: "text".into(),
            text: "part1".into(),
            cache_control: None,
        },
        AnthropicSystemBlock {
            block_type: "text".into(),
            text: "part2".into(),
            cache_control: None,
        },
        AnthropicSystemBlock {
            block_type: "other".into(), // 非 text → 过滤
            text: "ignored".into(),
            cache_control: None,
        },
    ]);
    assert_eq!(extract_system_text(&blocks), "part1\npart2");
}

// ═════════════════════════════════════════════════════════════
// anthropic_request_to_openai — system
// ═════════════════════════════════════════════════════════════

/// DT-ANT-02：system 为 Text → 转成 System role 消息；空 system 不产生消息。
#[test]
fn anthropic_request_system_text_to_openai() {
    let mut r = req_with_messages(vec![]);
    r.system = Some(AnthropicSystemContent::Text("you are helpful".into()));
    let o = anthropic_request_to_openai(&r);
    assert_eq!(o.messages.len(), 1);
    assert!(matches!(o.messages[0].role, MessageRole::System));
    assert!(matches!(&o.messages[0].content, MessageContent::Text(t) if t == "you are helpful"));

    // 空 system → 不产生 System 消息
    let mut r2 = req_with_messages(vec![]);
    r2.system = Some(AnthropicSystemContent::Text(String::new()));
    let o2 = anthropic_request_to_openai(&r2);
    assert!(o2.messages.is_empty());
}

/// DT-ANT-03：system 为 Blocks → 提取 text 块 join。
#[test]
fn anthropic_request_system_blocks_to_openai() {
    let mut r = req_with_messages(vec![]);
    r.system = Some(AnthropicSystemContent::Blocks(vec![
        AnthropicSystemBlock { block_type: "text".into(), text: "a".into(), cache_control: None },
        AnthropicSystemBlock { block_type: "text".into(), text: "b".into(), cache_control: None },
    ]));
    let o = anthropic_request_to_openai(&r);
    assert!(matches!(&o.messages[0].content, MessageContent::Text(t) if t == "a\nb"));
}

// ═════════════════════════════════════════════════════════════
// anthropic_request_to_openai — user/assistant messages
// ═════════════════════════════════════════════════════════════

/// DT-ANT-04：user Text → User role；assistant Text → Assistant role；未知 role → 兜底 User。
#[test]
fn anthropic_request_user_assistant_text() {
    let r = req_with_messages(vec![
        AnthropicMessage { role: "user".into(), content: AnthropicContent::Text("hello".into()) },
        AnthropicMessage { role: "assistant".into(), content: AnthropicContent::Text("hi".into()) },
        AnthropicMessage { role: "system".into(), content: AnthropicContent::Text("weird".into()) },
    ]);
    let o = anthropic_request_to_openai(&r);
    assert_eq!(o.messages.len(), 3);
    assert!(matches!(o.messages[0].role, MessageRole::User));
    assert!(matches!(o.messages[1].role, MessageRole::Assistant));
    // 未知 role → 兜底 User
    assert!(matches!(o.messages[2].role, MessageRole::User));
}

/// DT-ANT-05：user Blocks 含 Image(url) → OpenAI ContentPart::ImageUrl。
#[test]
fn anthropic_request_user_image_url_block() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "user".into(),
        content: AnthropicContent::Blocks(vec![AnthropicContentBlock::Image {
            source: serde_json::json!({"type":"url","url":"https://x/i.png"}),
        }]),
    }]);
    let o = anthropic_request_to_openai(&r);
    assert_eq!(o.messages.len(), 1);
    match &o.messages[0].content {
        MessageContent::Parts(parts) => {
            assert!(matches!(&parts[0], ContentPart::ImageUrl { image_url } if image_url.url == "https://x/i.png"));
        }
        _ => panic!("expected Parts"),
    }
}

/// DT-ANT-06：user Blocks 含 Image(base64) → 构造 data URI。
#[test]
fn anthropic_request_user_image_base64_block() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "user".into(),
        content: AnthropicContent::Blocks(vec![AnthropicContentBlock::Image {
            source: serde_json::json!({"type":"base64","media_type":"image/png","data":"SGk="}),
        }]),
    }]);
    let o = anthropic_request_to_openai(&r);
    match &o.messages[0].content {
        MessageContent::Parts(parts) => {
            assert!(matches!(&parts[0], ContentPart::ImageUrl { image_url } if image_url.url.starts_with("data:image/png;base64,")));
        }
        _ => panic!("expected Parts"),
    }
}

/// DT-ANT-07：user Blocks 含 ToolResult(Text) → 单独 Tool role 消息，带 tool_call_id。
#[test]
fn anthropic_request_user_tool_result_text() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "user".into(),
        content: AnthropicContent::Blocks(vec![AnthropicContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: Some(AnthropicContent::Text("result".into())),
            is_error: None,
        }]),
    }]);
    let o = anthropic_request_to_openai(&r);
    assert_eq!(o.messages.len(), 1);
    assert!(matches!(o.messages[0].role, MessageRole::Tool));
    assert_eq!(o.messages[0].tool_call_id.as_deref(), Some("tu_1"));
    assert!(matches!(&o.messages[0].content, MessageContent::Text(t) if t == "result"));
}

/// DT-ANT-08：user Blocks 含 ToolResult(is_error=true) → 内容加 [ERROR] 前缀。
#[test]
fn anthropic_request_user_tool_result_error_prefix() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "user".into(),
        content: AnthropicContent::Blocks(vec![AnthropicContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: Some(AnthropicContent::Text("failed".into())),
            is_error: Some(true),
        }]),
    }]);
    let o = anthropic_request_to_openai(&r);
    assert!(matches!(&o.messages[0].content, MessageContent::Text(t) if t.starts_with("[ERROR] failed")));
}

/// DT-ANT-09：user Blocks 含 ToolResult(Blocks-with-Image) → 图片转 [image: url] 占位。
#[test]
fn anthropic_request_user_tool_result_image_placeholder() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "user".into(),
        content: AnthropicContent::Blocks(vec![AnthropicContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: Some(AnthropicContent::Blocks(vec![AnthropicContentBlock::Image {
                source: serde_json::json!({"type":"url","url":"https://x/i.png"}),
            }])),
            is_error: None,
        }]),
    }]);
    let o = anthropic_request_to_openai(&r);
    assert!(matches!(&o.messages[0].content, MessageContent::Text(t) if t.starts_with("[image: ")));
}

/// DT-ANT-10：user Blocks 含 ToolResult(None content) → 空文本 Tool 消息。
#[test]
fn anthropic_request_user_tool_result_none_content() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "user".into(),
        content: AnthropicContent::Blocks(vec![AnthropicContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: None,
            is_error: None,
        }]),
    }]);
    let o = anthropic_request_to_openai(&r);
    assert!(matches!(&o.messages[0].content, MessageContent::Text(t) if t.is_empty()));
}

/// DT-ANT-11：user Blocks 含 Document(title + data) → 提取为 Text part。
#[test]
fn anthropic_request_user_document_block() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "user".into(),
        content: AnthropicContent::Blocks(vec![AnthropicContentBlock::Document {
            source: serde_json::json!({"type":"text","data":"doc body"}),
            title: Some("Title".into()),
            context: None,
            citations: None,
        }]),
    }]);
    let o = anthropic_request_to_openai(&r);
    match &o.messages[0].content {
        MessageContent::Parts(parts) => {
            assert!(matches!(&parts[0], ContentPart::Text { text } if text == "Title\ndoc body"));
        }
        _ => panic!("expected Parts"),
    }
}

/// DT-ANT-12：user Blocks 同时含 Text 与 ToolResult → 产生 User(Parts) + Tool 两条消息。
#[test]
fn anthropic_request_user_mixed_text_and_tool_result() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "user".into(),
        content: AnthropicContent::Blocks(vec![
            AnthropicContentBlock::Text { text: "prompt".into(), cache_control: None },
            AnthropicContentBlock::ToolResult {
                tool_use_id: "tu_1".into(),
                content: Some(AnthropicContent::Text("r".into())),
                is_error: None,
            },
        ]),
    }]);
    let o = anthropic_request_to_openai(&r);
    assert_eq!(o.messages.len(), 2);
    assert!(matches!(o.messages[0].role, MessageRole::User));
    assert!(matches!(o.messages[1].role, MessageRole::Tool));
}

// ═════════════════════════════════════════════════════════════
// anthropic_request_to_openai — assistant blocks / tools / stop / extra
// ═════════════════════════════════════════════════════════════

/// DT-ANT-13：assistant Blocks 含 ToolUse → assistant 消息带 tool_calls（arguments 序列化）。
#[test]
fn anthropic_request_assistant_tool_use() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "assistant".into(),
        content: AnthropicContent::Blocks(vec![
            AnthropicContentBlock::Text { text: "ok".into(), cache_control: None },
            AnthropicContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "get_weather".into(),
                input: serde_json::json!({"city": "BJ"}),
                cache_control: None,
            },
        ]),
    }]);
    let o = anthropic_request_to_openai(&r);
    assert_eq!(o.messages.len(), 1);
    let tc = o.messages[0].tool_calls.as_ref().expect("tool_calls");
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0].id, "tu_1");
    assert_eq!(tc[0].function.name, "get_weather");
    assert!(tc[0].function.arguments.contains("\"city\""));
}

/// DT-ANT-14：assistant Blocks 含 Thinking → 走 Parts 路径保留 Reasoning。
#[test]
fn anthropic_request_assistant_thinking_parts() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "assistant".into(),
        content: AnthropicContent::Blocks(vec![
            AnthropicContentBlock::Thinking { thinking: "hmm".into() },
            AnthropicContentBlock::Text { text: "ans".into(), cache_control: None },
        ]),
    }]);
    let o = anthropic_request_to_openai(&r);
    match &o.messages[0].content {
        MessageContent::Parts(parts) => {
            assert!(parts.iter().any(|p| matches!(p, ContentPart::Reasoning { reasoning } if reasoning == "hmm")));
            assert!(parts.iter().any(|p| matches!(p, ContentPart::Text { text } if text == "ans")));
        }
        _ => panic!("expected Parts for thinking"),
    }
}

/// DT-ANT-15：assistant Blocks 含 RedactedThinking → 跳过（无内容前传）。
#[test]
fn anthropic_request_assistant_redacted_thinking_skipped() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "assistant".into(),
        content: AnthropicContent::Blocks(vec![
            AnthropicContentBlock::RedactedThinking { data: "redacted".into() },
            AnthropicContentBlock::Text { text: "ans".into(), cache_control: None },
        ]),
    }]);
    let o = anthropic_request_to_openai(&r);
    // 无 thinking → 走 Text(join) 路径
    assert!(matches!(&o.messages[0].content, MessageContent::Text(t) if t == "ans"));
}

/// DT-ANT-16：assistant Blocks 含 Document → 提取 title+data 拼入文本。
#[test]
fn anthropic_request_assistant_document_block() {
    let r = req_with_messages(vec![AnthropicMessage {
        role: "assistant".into(),
        content: AnthropicContent::Blocks(vec![AnthropicContentBlock::Document {
            source: serde_json::json!({"type":"text","data":"body"}),
            title: Some("T".into()),
            context: None,
            citations: None,
        }]),
    }]);
    let o = anthropic_request_to_openai(&r);
    assert!(matches!(&o.messages[0].content, MessageContent::Text(t) if t == "T\nbody"));
}

/// DT-ANT-17：tools 字段 → OpenAI Tool 列表（input_schema → parameters）。
#[test]
fn anthropic_request_tools_conversion() {
    let mut r = req_with_messages(vec![]);
    r.tools = Some(vec![AnthropicTool {
        name: "get_weather".into(),
        description: Some("weather".into()),
        input_schema: serde_json::json!({"type":"object"}),
    }]);
    let o = anthropic_request_to_openai(&r);
    let tools = o.tools.expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].tool_type, "function");
    assert_eq!(tools[0].function.name, "get_weather");
    assert_eq!(tools[0].function.description.as_deref(), Some("weather"));
    assert_eq!(tools[0].function.parameters, serde_json::json!({"type":"object"}));
}

/// DT-ANT-18：stop_sequences 单元素 → Single；多元素 → Multiple。
#[test]
fn anthropic_request_stop_sequences() {
    let mut r1 = req_with_messages(vec![]);
    r1.stop_sequences = Some(vec!["END".into()]);
    let o1 = anthropic_request_to_openai(&r1);
    assert!(matches!(&o1.stop, Some(boom_core::types::StopSequence::Single(s)) if s == "END"));

    let mut r2 = req_with_messages(vec![]);
    r2.stop_sequences = Some(vec!["A".into(), "B".into()]);
    let o2 = anthropic_request_to_openai(&r2);
    assert!(matches!(&o2.stop, Some(boom_core::types::StopSequence::Multiple(v)) if v.len() == 2));
}

/// DT-ANT-19：thinking / metadata / extra 字段透传到 OpenAI extra map。
#[test]
fn anthropic_request_extra_fields_passthrough() {
    let mut r = req_with_messages(vec![]);
    r.thinking = Some(serde_json::json!({"type":"enabled","budget_tokens":1024}));
    r.metadata = Some(serde_json::json!({"user_id":"u1"}));
    r.extra.insert("custom".into(), serde_json::json!(42));
    let o = anthropic_request_to_openai(&r);
    assert_eq!(o.extra.get("thinking").and_then(|v| v.get("type")), Some(&serde_json::json!("enabled")));
    assert_eq!(o.extra.get("metadata").and_then(|v| v.get("user_id")), Some(&serde_json::json!("u1")));
    assert_eq!(o.extra.get("custom"), Some(&serde_json::json!(42)));
}

/// DT-ANT-20：temperature/top_p/max_tokens/stream/tool_choice 透传。
#[test]
fn anthropic_request_sampling_passthrough() {
    let mut r = req_with_messages(vec![]);
    r.temperature = Some(0.7);
    r.top_p = Some(0.9);
    r.stream = Some(true);
    r.tool_choice = Some(serde_json::json!({"type":"auto"}));
    let o = anthropic_request_to_openai(&r);
    assert_eq!(o.temperature, Some(0.7));
    assert_eq!(o.top_p, Some(0.9));
    assert_eq!(o.stream, Some(true));
    assert_eq!(o.tool_choice, Some(serde_json::json!({"type":"auto"})));
    assert_eq!(o.max_tokens, Some(1024));
}

// ═════════════════════════════════════════════════════════════
// openai_response_to_anthropic
// ═════════════════════════════════════════════════════════════

use boom_core::types::{Choice, Message};

fn resp_with(content: MessageContent, reasoning: Option<String>, tool_calls: Option<Vec<boom_core::types::ToolCall>>, finish: Option<&str>) -> ChatCompletionResponse {
    ChatCompletionResponse {
        id: "r1".into(),
        object: "chat.completion".into(),
        created: 0,
        model: "m".into(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: MessageRole::Assistant,
                content,
                name: None,
                tool_calls,
                tool_call_id: None,
                reasoning_content: reasoning,
            },
            finish_reason: finish.map(|s| s.to_string()),
            logprobs: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_creation_input_tokens: Some(3),
            cache_read_input_tokens: Some(7),
            prompt_tokens_details: None,
        }),
        system_fingerprint: None,
        raw_response: None,
    }
}

/// DT-ANT-21：content 为 Text → 单个 Text 块。
#[test]
fn response_text_to_anthropic() {
    let r = resp_with(MessageContent::Text("hello".into()), None, None, Some("stop"));
    let a = openai_response_to_anthropic(&r);
    assert_eq!(a.content.len(), 1);
    assert!(matches!(&a.content[0], boom_core::types::AnthropicResponseContentBlock::Text { text } if text == "hello"));
    assert_eq!(a.stop_reason.as_deref(), Some("end_turn"));
}

/// DT-ANT-22：top-level reasoning_content → 首块 Thinking（在 Text 之前）。
#[test]
fn response_top_level_reasoning_to_thinking() {
    let r = resp_with(MessageContent::Text("ans".into()), Some("think".into()), None, Some("stop"));
    let a = openai_response_to_anthropic(&r);
    assert_eq!(a.content.len(), 2);
    assert!(matches!(&a.content[0], boom_core::types::AnthropicResponseContentBlock::Thinking { thinking } if thinking == "think"));
    assert!(matches!(&a.content[1], boom_core::types::AnthropicResponseContentBlock::Text { text } if text == "ans"));
}

/// DT-ANT-23：content Parts 含 Reasoning + Text → Thinking + Text 块。
#[test]
fn response_parts_reasoning_to_thinking() {
    let r = resp_with(
        MessageContent::Parts(vec![
            ContentPart::Reasoning { reasoning: "hmm".into() },
            ContentPart::Text { text: "out".into() },
            ContentPart::ImageUrl { image_url: boom_core::types::ImageUrl { url: "u".into(), detail: None } }, // 响应侧跳过
        ]),
        None, None, Some("stop"),
    );
    let a = openai_response_to_anthropic(&r);
    assert_eq!(a.content.len(), 2);
    assert!(matches!(&a.content[0], boom_core::types::AnthropicResponseContentBlock::Thinking { thinking } if thinking == "hmm"));
    assert!(matches!(&a.content[1], boom_core::types::AnthropicResponseContentBlock::Text { text } if text == "out"));
}

/// DT-ANT-24：content Null → 空内容兜底单个空 Text 块。
#[test]
fn response_null_content_fallback_empty_text() {
    let r = resp_with(MessageContent::Null, None, None, Some("stop"));
    let a = openai_response_to_anthropic(&r);
    assert_eq!(a.content.len(), 1);
    assert!(matches!(&a.content[0], boom_core::types::AnthropicResponseContentBlock::Text { text } if text.is_empty()));
}

/// DT-ANT-25：空 choices → 兜底单个空 Text 块，stop_reason=None。
#[test]
fn response_empty_choices_fallback() {
    let r = ChatCompletionResponse {
        id: "r".into(), object: "chat.completion".into(), created: 0, model: "m".into(),
        choices: vec![], usage: None, system_fingerprint: None, raw_response: None,
    };
    let a = openai_response_to_anthropic(&r);
    assert_eq!(a.content.len(), 1);
    assert!(matches!(&a.content[0], boom_core::types::AnthropicResponseContentBlock::Text { text } if text.is_empty()));
    assert!(a.stop_reason.is_none());
    // usage 缺失 → 全 0
    assert_eq!(a.usage.input_tokens, 0);
    assert_eq!(a.usage.output_tokens, 0);
}

/// DT-ANT-26：tool_calls → ToolUse 块（arguments JSON 解析；非法 → Null）。
#[test]
fn response_tool_calls_to_tool_use() {
    let tc = vec![boom_core::types::ToolCall {
        id: "call_1".into(),
        call_type: "function".into(),
        function: boom_core::types::FunctionCall {
            name: "get_weather".into(),
            arguments: r#"{"city":"BJ"}"#.into(),
        },
    }];
    let r = resp_with(MessageContent::Text(String::new()), None, Some(tc), Some("tool_calls"));
    let a = openai_response_to_anthropic(&r);
    assert_eq!(a.stop_reason.as_deref(), Some("tool_use"));
    assert!(matches!(&a.content[0], boom_core::types::AnthropicResponseContentBlock::ToolUse { id, name, input } if id == "call_1" && name == "get_weather" && input["city"] == "BJ"));
}

/// DT-ANT-27：tool_calls 非法 arguments JSON → input 为 Null。
#[test]
fn response_tool_calls_invalid_json_to_null() {
    let tc = vec![boom_core::types::ToolCall {
        id: "call_1".into(),
        call_type: "function".into(),
        function: boom_core::types::FunctionCall {
            name: "f".into(),
            arguments: "not-json".into(),
        },
    }];
    let r = resp_with(MessageContent::Text(String::new()), None, Some(tc), Some("tool_calls"));
    let a = openai_response_to_anthropic(&r);
    assert!(matches!(&a.content[0], boom_core::types::AnthropicResponseContentBlock::ToolUse { input, .. } if input.is_null()));
}

/// DT-ANT-28：finish_reason 映射 stop→end_turn / tool_calls→tool_use / length→max_tokens / other 透传。
#[test]
fn response_finish_reason_mapping() {
    for (fr, sr) in [("stop", "end_turn"), ("tool_calls", "tool_use"), ("length", "max_tokens"), ("content_filter", "content_filter")] {
        let r = resp_with(MessageContent::Text("x".into()), None, None, Some(fr));
        let a = openai_response_to_anthropic(&r);
        assert_eq!(a.stop_reason.as_deref(), Some(sr), "finish {fr} -> {sr}");
    }
    // None finish_reason
    let r = resp_with(MessageContent::Text("x".into()), None, None, None);
    assert!(openai_response_to_anthropic(&r).stop_reason.is_none());
}

/// DT-ANT-29：usage 字段透传到 AnthropicUsage。
#[test]
fn response_usage_passthrough() {
    let r = resp_with(MessageContent::Text("x".into()), None, None, Some("stop"));
    let a = openai_response_to_anthropic(&r);
    assert_eq!(a.usage.input_tokens, 10);
    assert_eq!(a.usage.output_tokens, 5);
    assert_eq!(a.usage.cache_creation_input_tokens, Some(3));
    assert_eq!(a.usage.cache_read_input_tokens, Some(7));
}

/// DT-ANT-30：生成的 message id 以 msg_ 前缀。
#[test]
fn response_generates_msg_id() {
    let r = resp_with(MessageContent::Text("x".into()), None, None, Some("stop"));
    let a = openai_response_to_anthropic(&r);
    assert!(a.id.starts_with("msg_"), "id was: {}", a.id);
    assert_eq!(a.response_type, "message");
    assert_eq!(a.role, "assistant");
    assert_eq!(a.model, "m");
}

// ═════════════════════════════════════════════════════════════
// AnthropicStreamTranscoder
// ═════════════════════════════════════════════════════════════

/// DT-ANT-31：首个 text delta → message_start + content_block_start(text) + text_delta。
#[test]
fn transcoder_text_stream() {
    let mut t = AnthropicStreamTranscoder::new("m".into());
    let events = t.transcode(&chunk_with(serde_json::json!({"content":"hi"}), None));
    assert_eq!(event_names(&events), vec!["message_start", "content_block_start", "content_block_delta"]);
    assert_eq!(events[1].event, "content_block_start");
}

/// DT-ANT-32：reasoning delta → thinking 块（content_block_start thinking + thinking_delta）。
#[test]
fn transcoder_thinking_block() {
    let mut t = AnthropicStreamTranscoder::new("m".into());
    let events = t.transcode(&chunk_with(serde_json::json!({"reasoning_content":"think"}), None));
    let names = event_names(&events);
    assert!(names.contains(&"content_block_start"));
    assert!(names.contains(&"content_block_delta"));
    // thinking 块在 text 之前：start data 的 content_block.type == thinking
    let start_data: serde_json::Value = events.iter()
        .find(|e| e.event == "content_block_start")
        .map(|e| serde_json::from_str(&e.data).unwrap())
        .unwrap();
    assert_eq!(start_data["content_block"]["type"], "thinking");
}

/// DT-ANT-33：先 thinking 后 text → 关闭 thinking 块再开 text 块。
#[test]
fn transcoder_thinking_then_text() {
    let mut t = AnthropicStreamTranscoder::new("m".into());
    let _ = t.transcode(&chunk_with(serde_json::json!({"reasoning_content":"t"}), None));
    let events = t.transcode(&chunk_with(serde_json::json!({"content":"ans"}), None));
    // 应含 content_block_stop(thinking) + content_block_start(text) + content_block_delta
    let names = event_names(&events);
    assert!(names.contains(&"content_block_stop"));
    assert!(names.contains(&"content_block_start"));
    let start_data: serde_json::Value = events.iter()
        .find(|e| e.event == "content_block_start")
        .map(|e| serde_json::from_str(&e.data).unwrap())
        .unwrap();
    assert_eq!(start_data["content_block"]["type"], "text");
}

/// DT-ANT-34：text 后 reasoning → 关闭 text 块再开 thinking 块。
#[test]
fn transcoder_text_then_thinking() {
    let mut t = AnthropicStreamTranscoder::new("m".into());
    let _ = t.transcode(&chunk_with(serde_json::json!({"content":"a"}), None));
    let events = t.transcode(&chunk_with(serde_json::json!({"reasoning_content":"r"}), None));
    let names = event_names(&events);
    assert!(names.contains(&"content_block_stop"));
}

/// DT-ANT-35：tool_call 新 id → content_block_start(tool_use)；args delta → input_json_delta。
#[test]
fn transcoder_tool_call_blocks() {
    let mut t = AnthropicStreamTranscoder::new("m".into());
    let events = t.transcode(&chunk_with(
        serde_json::json!({"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"ci"}}]}),
        None,
    ));
    let names = event_names(&events);
    assert!(names.contains(&"content_block_start"));
    let delta = events.iter().rev().find(|e| e.event == "content_block_delta").map(|e| serde_json::from_str::<serde_json::Value>(&e.data).unwrap()).unwrap();
    assert_eq!(delta["delta"]["type"], "input_json_delta");
    assert_eq!(delta["delta"]["partial_json"], "{\"ci");
}

/// DT-ANT-36：finish_reason 持有 stop_reason；下一块携带 usage 时释放 message_delta + message_stop。
#[test]
fn transcoder_finish_holds_then_releases_on_next_chunk() {
    let mut t = AnthropicStreamTranscoder::new("m".into());
    // text chunk then finish chunk
    let _ = t.transcode(&chunk_with(serde_json::json!({"content":"hi"}), None));
    let finish_events = t.transcode(&chunk_with(serde_json::json!({}), Some("stop")));
    // finish 关闭 text 块，持有 stop_reason（不立即发 message_delta）
    let names = event_names(&finish_events);
    assert!(names.contains(&"content_block_stop"));
    assert!(!names.contains(&"message_delta"), "held, not emitted yet");

    // 下一块带 usage → 释放 message_delta + message_stop（chunk_with 不带 usage，单独构造）
    let usage_chunk: ChatStreamChunk = serde_json::from_value(serde_json::json!({
        "id":"x","object":"chat.completion.chunk","created":0,"model":"m",
        "choices":[],"usage":{"prompt_tokens":11,"completion_tokens":6}
    })).unwrap();
    let usage_events = t.transcode(&usage_chunk);
    let names = event_names(&usage_events);
    assert!(names.contains(&"message_delta"), "message_delta emitted on usage chunk");
    assert!(names.contains(&"message_stop"));
}

/// DT-ANT-37：stream 结束但 usage chunk 未到 → drain 释放持有的 finish 事件。
#[test]
fn transcoder_drain_releases_held_finish() {
    let mut t = AnthropicStreamTranscoder::new("m".into());
    let _ = t.transcode(&chunk_with(serde_json::json!({"content":"hi"}), None));
    let _ = t.transcode(&chunk_with(serde_json::json!({}), Some("stop")));
    let drained = t.drain();
    let names = event_names(&drained);
    assert!(names.contains(&"message_delta"));
    assert!(names.contains(&"message_stop"));
    // 二次 drain 为空
    assert!(t.drain().is_empty());
}

/// DT-ANT-38：transcoder usage 累计（usage chunk 的 prompt/completion tokens 被采用；
/// 注意每个 text delta 会使 output_tokens +=1，故此处不发 text delta 以保持值确定）。
#[test]
fn transcoder_extracts_usage() {
    let mut t = AnthropicStreamTranscoder::new("m".into());
    let usage_chunk: ChatStreamChunk = serde_json::from_value(serde_json::json!({
        "id":"x","object":"chat.completion.chunk","created":0,"model":"m",
        "choices":[],"usage":{"prompt_tokens":42,"completion_tokens":7}
    })).unwrap();
    let _ = t.transcode(&usage_chunk);
    // 不发 text delta（避免 output_tokens +=1）；finish chunk 带 finish_reason 触发 message_start + 持有
    let _ = t.transcode(&chunk_with(serde_json::json!({}), Some("stop")));
    let drained = t.drain();
    let delta = drained.iter().find(|e| e.event == "message_delta").map(|e| serde_json::from_str::<serde_json::Value>(&e.data).unwrap()).unwrap();
    assert_eq!(delta["usage"]["input_tokens"], 42);
    assert_eq!(delta["usage"]["output_tokens"], 7);
}

/// DT-ANT-39：空 delta 的 chunk（无 content/reasoning/tool_calls）只发 message_start（首块）。
#[test]
fn transcoder_empty_delta_first_chunk_emits_message_start() {
    let mut t = AnthropicStreamTranscoder::new("m".into());
    let events = t.transcode(&chunk_with(serde_json::json!({}), None));
    assert_eq!(event_names(&events), vec!["message_start"]);
    // 第二个空 delta → 无新事件（message_started 已 true）
    let events2 = t.transcode(&chunk_with(serde_json::json!({}), None));
    assert!(events2.is_empty());
}

/// DT-ANT-40：finish_reason=tool_calls 关闭 tool 块并持有 stop_reason(tool_use)。
#[test]
fn transcoder_finish_tool_calls() {
    let mut t = AnthropicStreamTranscoder::new("m".into());
    let _ = t.transcode(&chunk_with(
        serde_json::json!({"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"f","arguments":""}}]}),
        None,
    ));
    let events = t.transcode(&chunk_with(serde_json::json!({}), Some("tool_calls")));
    let names = event_names(&events);
    assert!(names.contains(&"content_block_stop"), "tool block closed on finish");
    // 持有 stop_reason=tool_use，drain 释放
    let drained = t.drain();
    let delta = drained.iter().find(|e| e.event == "message_delta").map(|e| serde_json::from_str::<serde_json::Value>(&e.data).unwrap()).unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
}
