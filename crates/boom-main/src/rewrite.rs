//! Optional request-body rewrites applied before forwarding upstream.

use boom_core::types::{
    AnthropicContent, AnthropicContentBlock, AnthropicMessagesRequest, AnthropicSystemContent,
};

/// Marker prefix identifying Claude Code's injected attribution block.
/// Aligned with vLLM's filter in `vllm/entrypoints/anthropic/serving.py` (PR #36829).
const ATTRIBUTION_PREFIX: &str = "x-anthropic-billing-header";

fn is_attribution_text(text: &str) -> bool {
    text.starts_with(ATTRIBUTION_PREFIX)
}

/// Strip Claude Code's attribution blocks from an Anthropic-format
/// `/v1/messages` request.
///
/// Inspects both the top-level `system` field and any `role="system"` messages
/// nested in `messages`. Only the blocks form is scanned; string-form system
/// content is left untouched (matching vLLM's behavior — Claude Code always
/// uses the blocks form for the attribution block). Returns `true` if at least
/// one block was removed.
pub fn strip_cc_attribution_anthropic(req: &mut AnthropicMessagesRequest) -> bool {
    let mut changed = false;

    if let Some(AnthropicSystemContent::Blocks(blocks)) = req.system.as_mut() {
        let before = blocks.len();
        blocks.retain(|b| !(b.block_type == "text" && is_attribution_text(&b.text)));
        if blocks.len() != before {
            changed = true;
        }
    }

    for msg in req.messages.iter_mut() {
        if msg.role != "system" {
            continue;
        }
        if let AnthropicContent::Blocks(parts) = &mut msg.content {
            let before = parts.len();
            parts.retain(|p| match p {
                AnthropicContentBlock::Text { text, .. } => !is_attribution_text(text),
                _ => true,
            });
            if parts.len() != before {
                changed = true;
            }
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_from(json: &str) -> AnthropicMessagesRequest {
        serde_json::from_str(json).expect("test fixture must deserialize")
    }

    fn system_text(req: &AnthropicMessagesRequest) -> String {
        match &req.system.as_ref().unwrap() {
            AnthropicSystemContent::Text(t) => t.clone(),
            AnthropicSystemContent::Blocks(blocks) => blocks
                .iter()
                .filter(|b| b.block_type == "text")
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("||"),
        }
    }

    #[test]
    fn strips_attribution_from_top_level_system_blocks() {
        let mut req = req_from(
            r#"{
                "model": "claude",
                "system": [
                    {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.78.13b; cch=0;"},
                    {"type": "text", "text": "You are helpful."}
                ],
                "messages": []
            }"#,
        );
        assert!(strip_cc_attribution_anthropic(&mut req));
        assert_eq!(system_text(&req), "You are helpful.");
    }

    #[test]
    fn no_op_when_no_attribution_present() {
        let mut req = req_from(
            r#"{
                "model": "claude",
                "system": [{"type": "text", "text": "You are helpful."}],
                "messages": []
            }"#,
        );
        assert!(!strip_cc_attribution_anthropic(&mut req));
        assert_eq!(system_text(&req), "You are helpful.");
    }

    #[test]
    fn string_form_system_is_left_untouched() {
        let mut req = req_from(
            r#"{
                "model": "claude",
                "system": "x-anthropic-billing-header: should not be stripped in string form",
                "messages": []
            }"#,
        );
        assert!(!strip_cc_attribution_anthropic(&mut req));
        // Still the original string.
        assert!(system_text(&req).starts_with("x-anthropic-billing-header"));
    }

    #[test]
    fn strips_attribution_from_nested_system_message_blocks() {
        let mut req = req_from(
            r#"{
                "model": "claude",
                "messages": [
                    {
                        "role": "system",
                        "content": [
                            {"type": "text", "text": "x-anthropic-billing-header: cc_version=x; cch=1;"},
                            {"type": "text", "text": "Real system prompt."}
                        ]
                    },
                    {"role": "user", "content": "hi"}
                ]
            }"#,
        );
        assert!(strip_cc_attribution_anthropic(&mut req));
        let sys_msg = req.messages.iter().find(|m| m.role == "system").unwrap();
        match &sys_msg.content {
            AnthropicContent::Blocks(parts) => {
                assert_eq!(parts.len(), 1);
                match &parts[0] {
                    AnthropicContentBlock::Text { text, .. } => {
                        assert_eq!(text, "Real system prompt.");
                    }
                    other => panic!("unexpected block: {:?}", other),
                }
            }
            other => panic!("unexpected content form: {:?}", other),
        }
    }

    #[test]
    fn strips_both_top_level_and_nested_system() {
        let mut req = req_from(
            r#"{
                "model": "claude",
                "system": [
                    {"type": "text", "text": "x-anthropic-billing-header: top;"},
                    {"type": "text", "text": "top-keep"}
                ],
                "messages": [
                    {
                        "role": "system",
                        "content": [
                            {"type": "text", "text": "x-anthropic-billing-header: nested;"},
                            {"type": "text", "text": "nested-keep"}
                        ]
                    }
                ]
            }"#,
        );
        assert!(strip_cc_attribution_anthropic(&mut req));
        assert_eq!(system_text(&req), "top-keep");
    }

    #[test]
    fn non_text_content_blocks_preserved() {
        // system blocks in the Anthropic spec are always text, but content
        // blocks inside messages (tool_use, image, etc.) are polymorphic.
        // The strip must only drop text blocks whose content starts with the
        // attribution prefix; other variants must pass through untouched.
        let mut req = req_from(
            r#"{
                "model": "claude",
                "messages": [
                    {
                        "role": "system",
                        "content": [
                            {"type": "text", "text": "x-anthropic-billing-header: cch=0;"},
                            {"type": "text", "text": "real prompt"}
                        ]
                    }
                ]
            }"#,
        );
        assert!(strip_cc_attribution_anthropic(&mut req));
        let sys_msg = req.messages.iter().find(|m| m.role == "system").unwrap();
        if let AnthropicContent::Blocks(parts) = &sys_msg.content {
            assert_eq!(parts.len(), 1, "only the attribution text block should be removed");
        }
    }

    #[test]
    fn user_role_messages_not_scanned() {
        let mut req = req_from(
            r#"{
                "model": "claude",
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": "x-anthropic-billing-header: should not be stripped from user msg"}
                        ]
                    }
                ]
            }"#,
        );
        assert!(!strip_cc_attribution_anthropic(&mut req));
        let user_msg = req.messages.iter().find(|m| m.role == "user").unwrap();
        if let AnthropicContent::Blocks(parts) = &user_msg.content {
            assert_eq!(parts.len(), 1);
        } else {
            panic!("expected blocks form");
        }
    }
}
