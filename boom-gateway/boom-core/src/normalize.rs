use crate::types::{Message, MessageContent, MessageRole};

/// Ensure messages alternate between user/assistant roles by inserting empty
/// user messages between consecutive same-role messages. Anthropic requires
/// strict alternation; this normalisation is applied before sending to an
/// Anthropic provider.
///
/// System messages are left at their current position — the caller is
/// responsible for extracting them before building the Anthropic request.
pub fn ensure_role_alternation(messages: &mut Vec<Message>) {
    let mut i = 1;
    while i < messages.len() {
        let prev_role = &messages[i - 1].role;
        let curr_role = &messages[i].role;

        // Only insert separators between user/assistant pairs that would violate
        // the alternation rule. System and Tool roles have special handling.
        let needs_separator = match (prev_role, curr_role) {
            (MessageRole::User, MessageRole::User)
            | (MessageRole::Assistant, MessageRole::Assistant)
            | (MessageRole::User, MessageRole::Tool)
            | (MessageRole::Assistant, MessageRole::User)
            | (MessageRole::Tool, MessageRole::Tool)
            | (MessageRole::Tool, MessageRole::User) => true,
            _ => false,
        };

        if needs_separator {
            messages.insert(
                i,
                Message {
                    role: MessageRole::User,
                    content: MessageContent::Text(String::new()),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                },
            );
            // Skip past the inserted message and the one that triggered it.
            i += 2;
        } else {
            i += 1;
        }
    }
}

/// Convert OpenAI `tool_choice` + `parallel_tool_calls` to Anthropic format.
///
/// Returns `(anthropic_tool_choice, should_strip_tools)`:
/// - `anthropic_tool_choice`: the value to set as `tool_choice` in the Anthropic request.
/// - `should_strip_tools`: when `true`, the caller should omit `tools` and `tool_choice`
///   entirely from the request body (OpenAI `tool_choice: "none"`).
pub fn convert_tool_choice_for_anthropic(
    tc: &Option<serde_json::Value>,
    parallel_tool_calls: Option<bool>,
) -> (Option<serde_json::Value>, bool) {
    let Some(tc) = tc else {
        // No tool_choice specified. If parallel_tool_calls is explicitly false,
        // wrap it as Anthropic's auto + disable_parallel.
        if parallel_tool_calls == Some(false) {
            return (
                Some(serde_json::json!({
                    "type": "auto",
                    "disable_parallel_tool_use": true
                })),
                false,
            );
        }
        return (None, false);
    };

    let tc_type = tc.get("type").and_then(|t| t.as_str()).unwrap_or("auto");

    match tc_type {
        "none" => (None, true),
        "auto" => {
            let mut val = serde_json::json!({"type": "auto"});
            if parallel_tool_calls == Some(false) {
                val["disable_parallel_tool_use"] = serde_json::json!(true);
            }
            (Some(val), false)
        }
        "required" => {
            // OpenAI doesn't have "required" but some clients send it.
            // Map to Anthropic "any".
            let mut val = serde_json::json!({"type": "any"});
            if parallel_tool_calls == Some(false) {
                val["disable_parallel_tool_use"] = serde_json::json!(true);
            }
            (Some(val), false)
        }
        "function" => {
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let mut val = serde_json::json!({
                "type": "tool",
                "name": name
            });
            if parallel_tool_calls == Some(false) {
                val["disable_parallel_tool_use"] = serde_json::json!(true);
            }
            (Some(val), false)
        }
        _ => {
            // Pass through unknown types (e.g. already-Anthropic format).
            (Some(tc.clone()), false)
        }
    }
}

/// Convert an Anthropic image `source` block to a URL string suitable for
/// OpenAI's `image_url.url` field.
///
/// - `source.type == "url"` → use the `url` field directly.
/// - `source.type == "base64"` → construct a data URI.
pub fn convert_image_source(source: &serde_json::Value) -> String {
    let source_type = source.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match source_type {
        "url" => source
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string(),
        "base64" => {
            let media_type = source
                .get("media_type")
                .and_then(|m| m.as_str())
                .unwrap_or("image/png");
            let data = source
                .get("data")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            format!("data:{};base64,{}", media_type, data)
        }
        _ => source.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_alternation_consecutive_assistant() {
        let mut messages = vec![
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("hello".into()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text("hi".into()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text("there".into()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
        ];
        ensure_role_alternation(&mut messages);
        assert_eq!(messages.len(), 4);
        assert!(matches!(messages[2].role, MessageRole::User));
        assert!(matches!(messages[3].role, MessageRole::Assistant));
    }

    #[test]
    fn test_role_alternation_already_alternating() {
        let mut messages = vec![
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("hello".into()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text("hi".into()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
        ];
        ensure_role_alternation(&mut messages);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_convert_tool_choice_none() {
        let (tc, strip) = convert_tool_choice_for_anthropic(
            &Some(serde_json::json!({"type": "none"})),
            None,
        );
        assert!(tc.is_none());
        assert!(strip);
    }

    #[test]
    fn test_convert_tool_choice_auto() {
        let (tc, strip) = convert_tool_choice_for_anthropic(
            &Some(serde_json::json!({"type": "auto"})),
            None,
        );
        assert_eq!(tc.unwrap()["type"], "auto");
        assert!(!strip);
    }

    #[test]
    fn test_convert_tool_choice_function() {
        let (tc, strip) = convert_tool_choice_for_anthropic(
            &Some(serde_json::json!({
                "type": "function",
                "function": {"name": "get_weather"}
            })),
            None,
        );
        let val = tc.unwrap();
        assert_eq!(val["type"], "tool");
        assert_eq!(val["name"], "get_weather");
        assert!(!strip);
    }

    #[test]
    fn test_convert_tool_choice_parallel_false() {
        let (tc, _) = convert_tool_choice_for_anthropic(
            &Some(serde_json::json!({"type": "auto"})),
            Some(false),
        );
        let val = tc.unwrap();
        assert_eq!(val["disable_parallel_tool_use"], true);
    }

    #[test]
    fn test_convert_image_source_url() {
        let source = serde_json::json!({
            "type": "url",
            "url": "https://example.com/image.png"
        });
        assert_eq!(
            convert_image_source(&source),
            "https://example.com/image.png"
        );
    }

    #[test]
    fn test_convert_image_source_base64() {
        let source = serde_json::json!({
            "type": "base64",
            "media_type": "image/jpeg",
            "data": "SGVsbG8="
        });
        let result = convert_image_source(&source);
        assert!(result.starts_with("data:image/jpeg;base64,"));
        assert!(result.ends_with("SGVsbG8="));
    }
}

/// Extract the bare host (IPv4, hostname, or IPv6 literal WITHOUT brackets)
/// from an API-base URL, stripping scheme, port, and path. Correctly handles
/// IPv6 literals of the form `http://[2001:db8::1]:8000/v1` — returns
/// `2001:db8::1` (no brackets), so it matches the bare-host worker_id vLLM
/// publishes in ZMQ topics. Returns None for empty input.
///
/// Examples:
///   `http://7.150.7.202:8000/v1`   → `7.150.7.202`
///   `http://[2001:db8::1]:8000/v1` → `2001:db8::1`
///   `http://host:8000`             → `host`
///   `7.150.7.202`                  → `7.150.7.202`
pub fn host_of_api_base(api_base: &str) -> Option<String> {
    let raw = api_base.trim();
    if raw.is_empty() {
        return None;
    }
    // Strip scheme.
    let after_scheme = match raw.split_once("://") {
        Some((_, rest)) => rest,
        None => raw,
    };
    // Keep only the authority (strip path).
    let authority = after_scheme.split('/').next().unwrap_or("");
    // IPv6 literal in brackets: `[2001:db8::1]` or `[2001:db8::1]:port`.
    if let Some(rest) = authority.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = rest[..end].trim();
            return if host.is_empty() { None } else { Some(host.to_string()) };
        }
    }
    // IPv4 / hostname: strip a trailing `:port` (the last colon). A bare
    // IPv4/hostname has no colon, so rsplit_once returns None and we keep it.
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority)
        .trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

#[cfg(test)]
mod host_tests {
    use super::host_of_api_base;

    #[test]
    fn ipv4_with_port_and_path() {
        assert_eq!(host_of_api_base("http://7.150.7.202:8000/v1"), Some("7.150.7.202".into()));
    }
    #[test]
    fn ipv6_literal_with_port() {
        assert_eq!(host_of_api_base("http://[2001:db8::1]:8000/v1"), Some("2001:db8::1".into()));
    }
    #[test]
    fn ipv6_literal_no_port() {
        assert_eq!(host_of_api_base("http://[2001:db8::1]/v1"), Some("2001:db8::1".into()));
    }
    #[test]
    fn hostname_no_port() {
        assert_eq!(host_of_api_base("http://worker-0/v1"), Some("worker-0".into()));
    }
    #[test]
    fn bare_host() {
        assert_eq!(host_of_api_base("7.150.7.202"), Some("7.150.7.202".into()));
    }
    #[test]
    fn empty_returns_none() {
        assert_eq!(host_of_api_base(""), None);
        assert_eq!(host_of_api_base("   "), None);
    }
}
