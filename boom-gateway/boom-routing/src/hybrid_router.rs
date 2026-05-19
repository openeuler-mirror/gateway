use boom_core::types::{Message, MessageContent, Tool};
use std::collections::HashMap;
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════
// Strategy trait + registry
// ═══════════════════════════════════════════════════════════

/// Input for classification strategies.
pub struct ClassifyRequest<'a> {
    pub messages: &'a [Message],
    pub tools: &'a Option<Vec<Tool>>,
    pub default_tier: &'a str,
}

/// Extensible classification strategy interface.
/// Implement this trait and register via `StrategyRegistry` to add
/// new content-based routing strategies — no other code changes needed.
pub trait ClassificationStrategy: Send + Sync {
    /// Strategy name (matches the `strategy` config field).
    fn name(&self) -> &str;
    /// Classify the request and return a tier name.
    fn classify(&self, req: &ClassifyRequest) -> String;
}

/// Registry of named classification strategies.
pub struct StrategyRegistry {
    strategies: HashMap<String, Arc<dyn ClassificationStrategy>>,
}

impl StrategyRegistry {
    pub fn new() -> Self {
        Self {
            strategies: HashMap::new(),
        }
    }

    /// Register a strategy. One-line call at startup.
    pub fn register(&mut self, strategy: Arc<dyn ClassificationStrategy>) {
        self.strategies.insert(strategy.name().to_string(), strategy);
    }

    /// Look up a strategy by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn ClassificationStrategy>> {
        self.strategies.get(name)
    }
}

impl Default for StrategyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════
// Built-in: TierClassifier (keyword + heuristic scoring)
// ═══════════════════════════════════════════════════════════

/// Keyword-based tier classification strategy.
/// Scores requests by text length, code blocks, reasoning keywords,
/// tool usage, and conversation depth, then maps to a tier.
pub struct TierClassifier;

impl ClassificationStrategy for TierClassifier {
    fn name(&self) -> &str {
        "tier_classifier"
    }

    fn classify(&self, req: &ClassifyRequest) -> String {
        let mut score: f64 = 0.0;

        let mut user_text = String::new();
        let mut has_system = false;
        let mut system_len = 0usize;
        let mut has_code_blocks = false;

        for msg in req.messages {
            let text = match &msg.content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .map(|p| match p {
                        boom_core::types::ContentPart::Text { text } => text.as_str(),
                        _ => "",
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                MessageContent::Null => continue,
            };

            match msg.role {
                boom_core::types::MessageRole::User => {
                    user_text.push_str(&text);
                    user_text.push(' ');
                }
                boom_core::types::MessageRole::System => {
                    has_system = true;
                    system_len += text.len();
                }
                _ => {}
            }

            if text.contains("```") {
                has_code_blocks = true;
            }
        }

        let user_text_lower = user_text.to_lowercase();
        let user_len = user_text.trim().len();

        // Text length contribution.
        if user_len > 8000 {
            score += 1.0;
        } else if user_len > 2000 {
            score += 0.4;
        } else if user_len > 500 {
            score += 0.1;
        }

        // Code blocks.
        if has_code_blocks {
            score += 0.8;
        }

        // Debug/system keywords.
        let debug_keywords = ["debug", "调试", "traceback", "error log", "stack trace", "异常"];
        let debug_count = debug_keywords.iter().filter(|k| user_text_lower.contains(*k)).count();
        if debug_count > 0 {
            score += 0.5 * debug_count.min(2) as f64;
        }

        // Reasoning keywords.
        let reasoning_keywords = [
            "prove", "证明", "推导", "theorem", "公式", "formula", "derive",
            "mathematical", "数学", "calculus", "logic", "逻辑推理",
        ];
        let reasoning_count = reasoning_keywords
            .iter()
            .filter(|k| user_text_lower.contains(*k))
            .count();
        if reasoning_count > 0 {
            score += 0.8 * reasoning_count.min(3) as f64;
        }

        // Tool calling.
        if req.tools.is_some() {
            score += 0.6;
        }

        // Complex system prompt.
        if has_system && system_len > 500 {
            score += 0.3;
        }

        // Conversation depth.
        let user_msg_count = req
            .messages
            .iter()
            .filter(|m| matches!(m.role, boom_core::types::MessageRole::User))
            .count();
        if user_msg_count > 6 {
            score += 0.3;
        } else if user_msg_count > 3 {
            score += 0.1;
        }

        if score < 0.5 {
            "small".to_string()
        } else if score < 1.5 {
            "medium".to_string()
        } else {
            "large".to_string()
        }
    }
}

// ═══════════════════════════════════════════════════════════
// HybridRouter — the dynamic alias resolver
// ═══════════════════════════════════════════════════════════

/// Content-based model router. Acts as a "dynamic alias":
/// matches a virtual model name → classifies request content →
/// returns a real model name for the normal routing pipeline.
pub struct HybridRouter {
    model_name: String,
    strategy: Arc<dyn ClassificationStrategy>,
    default_tier: String,
    tiers: HashMap<String, String>,
}

impl HybridRouter {
    pub fn new(
        model_name: String,
        strategy: Arc<dyn ClassificationStrategy>,
        default_tier: String,
        tiers: HashMap<String, String>,
    ) -> Self {
        Self {
            model_name,
            strategy,
            default_tier,
            tiers,
        }
    }

    /// The virtual model name this router responds to.
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// Attempt to classify a request.
    /// Returns `Some(target_model)` if the model matches the virtual name.
    /// Returns `None` otherwise (pass-through to normal routing).
    pub fn classify(
        &self,
        model: &str,
        messages: &[Message],
        tools: &Option<Vec<Tool>>,
    ) -> Option<String> {
        if model != self.model_name {
            return None;
        }

        let req = ClassifyRequest {
            messages,
            tools,
            default_tier: &self.default_tier,
        };

        let tier = self.strategy.classify(&req);

        let target = self.tiers.get(&tier).cloned().unwrap_or_else(|| {
            tracing::warn!(
                tier = %tier,
                default_tier = %self.default_tier,
                "Classification returned unknown tier, falling back to default"
            );
            self.tiers
                .get(&self.default_tier)
                .cloned()
                .unwrap_or_else(|| self.model_name.clone())
        });

        tracing::info!(
            model = %model,
            tier = %tier,
            target_model = %target,
            "Hybrid router classified request"
        );

        Some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boom_core::types::{ContentPart, MessageRole};

    fn make_messages(texts: Vec<(&str, &str)>) -> Vec<Message> {
        texts
            .into_iter()
            .map(|(role, text)| Message {
                role: match role {
                    "user" => MessageRole::User,
                    "system" => MessageRole::System,
                    "assistant" => MessageRole::Assistant,
                    _ => MessageRole::User,
                },
                content: MessageContent::Text(text.to_string()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            })
            .collect()
    }

    fn make_router() -> HybridRouter {
        let mut tiers = HashMap::new();
        tiers.insert("small".to_string(), "small-cup".to_string());
        tiers.insert("medium".to_string(), "medium-cup".to_string());
        tiers.insert("large".to_string(), "large-cup".to_string());
        HybridRouter::new(
            "auto".to_string(),
            Arc::new(TierClassifier),
            "medium".to_string(),
            tiers,
        )
    }

    #[test]
    fn non_matching_model_returns_none() {
        let router = make_router();
        let msgs = make_messages(vec![("user", "hello")]);
        assert!(router.classify("gpt-4o", &msgs, &None).is_none());
    }

    #[test]
    fn short_greeting_routes_to_small() {
        let router = make_router();
        let msgs = make_messages(vec![("user", "hi")]);
        let result = router.classify("auto", &msgs, &None);
        assert_eq!(result, Some("small-cup".to_string()));
    }

    #[test]
    fn code_request_routes_to_large() {
        let router = make_router();
        let msgs = make_messages(vec![("user", "debug this code:\n```python\nprint(1)\n```")]);
        let result = router.classify("auto", &msgs, &None);
        assert_eq!(result, Some("large-cup".to_string()));
    }

    #[test]
    fn tool_request_routes_to_medium_or_higher() {
        let router = make_router();
        let msgs = make_messages(vec![("user", "what is the weather?")]);
        let tools = vec![Tool {
            tool_type: "function".to_string(),
            function: boom_core::types::ToolFunction {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                parameters: serde_json::json!({}),
            },
        }];
        let result = router.classify("auto", &msgs, &Some(tools));
        assert!(result == Some("medium-cup".to_string()) || result == Some("large-cup".to_string()));
    }

    #[test]
    fn registry_register_and_lookup() {
        let mut registry = StrategyRegistry::new();
        registry.register(Arc::new(TierClassifier));
        assert!(registry.get("tier_classifier").is_some());
        assert!(registry.get("nonexistent").is_none());
    }
}
