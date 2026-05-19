use boom_core::provider::Provider;
use boom_core::types::*;
use boom_core::GatewayError;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tracing;

/// Tier level for auto routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    Small,
    Medium,
    Large,
}

impl Tier {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "small" => Some(Tier::Small),
            "medium" => Some(Tier::Medium),
            "large" => Some(Tier::Large),
            _ => None,
        }
    }

    /// Minimum tier — upgrade self to at least `minimum`.
    #[allow(dead_code)]
    fn at_least(self, minimum: Tier) -> Tier {
        if (self as usize) < (minimum as usize) {
            minimum
        } else {
            self
        }
    }
}

/// Heuristic classifier output.
struct Classification {
    tier: Tier,
    reason: String,
}

/// Virtual model provider that classifies requests by content and delegates
/// to tiered backend providers (small/medium/large cup).
///
/// Implements the `Provider` trait so it plugs into the existing routing
/// pipeline as a regular deployment — `Router` treats "hybrid" like any other model.
pub struct HybridRouterProvider {
    /// Virtual model name (e.g. "hybrid").
    model_name: String,
    /// Fallback tier when classification is uncertain.
    #[allow(dead_code)]
    default_tier: Tier,
    /// Tier → backend provider.
    providers: HashMap<Tier, Arc<dyn Provider>>,
    /// Tier → actual model name (used to set request.model before delegating).
    tier_models: HashMap<Tier, String>,
}

impl HybridRouterProvider {
    /// Build a new HybridRouterProvider.
    ///
    /// * `model_name` — virtual model name (e.g. "hybrid").
    /// * `default_tier` — fallback tier string ("small"/"medium"/"large").
    /// * `tier_map` — mapping from tier string to (target_model_name, Arc<dyn Provider>).
    pub fn new(
        model_name: String,
        default_tier: &str,
        tier_map: HashMap<String, (String, Arc<dyn Provider>)>,
    ) -> Result<Self, GatewayError> {
        let default = Tier::from_str(default_tier).ok_or_else(|| {
            GatewayError::ConfigError(format!(
                "hybrid_router: invalid default_tier '{}', expected small/medium/large",
                default_tier
            ))
        })?;

        let mut providers = HashMap::new();
        let mut tier_models = HashMap::new();

        for (tier_str, (target_model, provider)) in tier_map {
            let tier = Tier::from_str(&tier_str).ok_or_else(|| {
                GatewayError::ConfigError(format!(
                    "hybrid_router: invalid tier '{}', expected small/medium/large",
                    tier_str
                ))
            })?;
            providers.insert(tier, provider);
            tier_models.insert(tier, target_model);
        }

        // Validate that all three tiers are configured.
        for t in &[Tier::Small, Tier::Medium, Tier::Large] {
            if !providers.contains_key(t) {
                return Err(GatewayError::ConfigError(format!(
                    "hybrid_router: missing tier '{}' configuration",
                    match t {
                        Tier::Small => "small",
                        Tier::Medium => "medium",
                        Tier::Large => "large",
                    }
                )));
            }
        }

        Ok(Self {
            model_name,
            default_tier: default,
            providers,
            tier_models,
        })
    }

    /// Classify a request based on message content using heuristic rules.
    ///
    /// Returns the chosen tier and a human-readable reason for logging.
    fn classify(&self, messages: &[Message], tools: &Option<Vec<Tool>>) -> Classification {
        let mut score: f64 = 0.0; // 0.0 = small, 1.0 = medium, 2.0 = large

        // Accumulate all user text for analysis.
        let mut user_text = String::new();
        let mut has_system = false;
        let mut system_len = 0usize;
        let mut _total_text_len = 0usize;
        let mut has_code_blocks = false;

        for msg in messages {
            let text = match &msg.content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => text.as_str(),
                        _ => "",
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                MessageContent::Null => continue,
            };

            _total_text_len += text.len();

            match msg.role {
                MessageRole::User => {
                    user_text.push_str(&text);
                    user_text.push(' ');
                }
                MessageRole::System => {
                    has_system = true;
                    system_len += text.len();
                }
                _ => {}
            }

            // Code block detection (``` markers).
            if text.contains("```") {
                has_code_blocks = true;
            }
        }

        let user_text_lower = user_text.to_lowercase();
        let user_len = user_text.trim().len();

        // === Feature: text length ===
        // Short inputs tend to be simple, long ones complex.
        if user_len < 200 {
            // score unchanged — lean towards small
        } else if user_len < 1000 {
            score += 0.4; // lean towards medium
        } else if user_len < 2000 {
            score += 0.8; // lean towards medium-large
        } else {
            score += 1.2; // lean towards large
        }

        // === Feature: code blocks ===
        if has_code_blocks {
            score += 0.6; // code questions need at least medium
        }

        // === Feature: debug/error keywords ===
        let debug_keywords = [
            "error", "bug", "traceback", "报错", "修复", "debug", "crash",
            "exception", "fail", "fault", "panic", "segfault", "异常",
            "不工作", "不生效", "hang", "timeout", "broken",
        ];
        let debug_count = debug_keywords
            .iter()
            .filter(|k| user_text_lower.contains(*k))
            .count();
        if debug_count > 0 {
            score += 1.0 * debug_count.min(2) as f64; // cap at 2 matches
        }

        // === Feature: complexity keywords ===
        let complex_keywords = [
            "architect", "design", "compare", "analyze", "review", "refactor",
            "架构", "设计", "对比", "分析", "重构", "优化", "性能",
            "optimize", "performance", "benchmark", "trade-off", "tradeoff",
            "evaluate", "explain why", "pros and cons", "优劣",
        ];
        let complex_count = complex_keywords
            .iter()
            .filter(|k| user_text_lower.contains(*k))
            .count();
        if complex_count > 0 {
            score += 0.7 * complex_count.min(3) as f64;
        }

        // === Feature: math/reasoning keywords ===
        let reasoning_keywords = [
            "prove", "证明", "推导", "theorem", "lemma", "公式",
            "formula", "derive", "mathematical", "数学", "calculus",
            "logic", "逻辑推理", "deduce", "induction",
        ];
        let reasoning_count = reasoning_keywords
            .iter()
            .filter(|k| user_text_lower.contains(*k))
            .count();
        if reasoning_count > 0 {
            score += 0.8 * reasoning_count.min(3) as f64;
        }

        // === Feature: tool calling ===
        if tools.is_some() {
            score += 0.6; // tool use needs at least medium
        }

        // === Feature: system prompt complexity ===
        if has_system && system_len > 500 {
            score += 0.3;
        }

        // === Feature: conversation depth ===
        let user_msg_count = messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::User))
            .count();
        if user_msg_count > 6 {
            score += 0.3;
        } else if user_msg_count > 3 {
            score += 0.1;
        }

        // === Map score to tier ===
        let tier = if score < 0.5 {
            Tier::Small
        } else if score < 1.5 {
            Tier::Medium
        } else {
            Tier::Large
        };

        let reason = format!(
            "score={:.2} user_len={} code={} debug_hits={} complex_hits={} reasoning_hits={} depth={} has_tools={}",
            score,
            user_len,
            has_code_blocks,
            debug_count,
            complex_count,
            reasoning_count,
            user_msg_count,
            tools.is_some(),
        );

        Classification { tier, reason }
    }
}

#[async_trait]
impl Provider for HybridRouterProvider {
    async fn chat(&self, mut request: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
        let classification = self.classify(&request.messages, &request.tools);
        tracing::info!(
            model = %self.model_name,
            target_tier = ?classification.tier,
            reason = %classification.reason,
            "Auto router classification"
        );

        let provider = self.providers.get(&classification.tier).ok_or_else(|| {
            GatewayError::InternalError(format!(
                "hybrid_router: no provider for tier {:?}",
                classification.tier
            ))
        })?;

        // Set the actual model name for the backend provider.
        if let Some(model) = self.tier_models.get(&classification.tier) {
            request.model = model.clone();
        }

        provider.chat(request).await
    }

    async fn chat_stream(&self, mut request: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
        let classification = self.classify(&request.messages, &request.tools);
        tracing::info!(
            model = %self.model_name,
            target_tier = ?classification.tier,
            reason = %classification.reason,
            "Auto router classification"
        );

        let provider = self.providers.get(&classification.tier).ok_or_else(|| {
            GatewayError::InternalError(format!(
                "hybrid_router: no provider for tier {:?}",
                classification.tier
            ))
        })?;

        // Set the actual model name for the backend provider.
        if let Some(model) = self.tier_models.get(&classification.tier) {
            request.model = model.clone();
        }

        provider.chat_stream(request).await
    }

    fn name(&self) -> &str {
        "hybrid-router"
    }

    fn models(&self) -> &[String] {
        // Return empty — the virtual model name is registered separately.
        static EMPTY: &[String] = &[];
        EMPTY
    }

    fn deployment_id(&self) -> Option<&str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Standalone classify function for unit testing (mirrors HybridRouterProvider::classify logic).
    fn classify_messages(messages: &[Message], tools: &Option<Vec<Tool>>) -> Tier {
        let mut score: f64 = 0.0;

        let mut user_text = String::new();
        let mut has_system = false;
        let mut system_len = 0usize;
        let mut _total_text_len = 0usize;
        let mut has_code_blocks = false;

        for msg in messages {
            let text = match &msg.content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => text.as_str(),
                        _ => "",
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                MessageContent::Null => continue,
            };

            _total_text_len += text.len();

            match msg.role {
                MessageRole::User => {
                    user_text.push_str(&text);
                    user_text.push(' ');
                }
                MessageRole::System => {
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

        // Text length
        if user_len < 200 {
            // unchanged
        } else if user_len < 1000 {
            score += 0.4;
        } else if user_len < 2000 {
            score += 0.8;
        } else {
            score += 1.2;
        }

        // Code blocks
        if has_code_blocks {
            score += 0.6;
        }

        // Debug keywords
        let debug_keywords = [
            "error", "bug", "traceback", "报错", "修复", "debug", "crash",
            "exception", "fail", "fault", "panic", "segfault", "异常",
            "不工作", "不生效", "hang", "timeout", "broken",
        ];
        let debug_count = debug_keywords
            .iter()
            .filter(|k| user_text_lower.contains(*k))
            .count();
        if debug_count > 0 {
            score += 1.0 * debug_count.min(2) as f64;
        }

        // Complexity keywords
        let complex_keywords = [
            "architect", "design", "compare", "analyze", "review", "refactor",
            "架构", "设计", "对比", "分析", "重构", "优化", "性能",
            "optimize", "performance", "benchmark", "trade-off", "tradeoff",
            "evaluate", "explain why", "pros and cons", "优劣",
        ];
        let complex_count = complex_keywords
            .iter()
            .filter(|k| user_text_lower.contains(*k))
            .count();
        if complex_count > 0 {
            score += 0.7 * complex_count.min(3) as f64;
        }

        // Reasoning keywords
        let reasoning_keywords = [
            "prove", "证明", "推导", "theorem", "lemma", "公式",
            "formula", "derive", "mathematical", "数学", "calculus",
            "logic", "逻辑推理", "deduce", "induction",
        ];
        let reasoning_count = reasoning_keywords
            .iter()
            .filter(|k| user_text_lower.contains(*k))
            .count();
        if reasoning_count > 0 {
            score += 0.8 * reasoning_count.min(3) as f64;
        }

        // Tool calling
        if tools.is_some() {
            score += 0.6;
        }

        // System prompt complexity
        if has_system && system_len > 500 {
            score += 0.3;
        }

        // Conversation depth
        let user_msg_count = messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::User))
            .count();
        if user_msg_count > 6 {
            score += 0.3;
        } else if user_msg_count > 3 {
            score += 0.1;
        }

        if score < 0.5 {
            Tier::Small
        } else if score < 1.5 {
            Tier::Medium
        } else {
            Tier::Large
        }
    }

    #[test]
    fn test_simple_greeting_routes_small() {
        let msgs = make_messages(vec![("user", "Hello, how are you?")]);
        assert_eq!(classify_messages(&msgs, &None), Tier::Small);
    }

    #[test]
    fn test_short_question_routes_small() {
        let msgs = make_messages(vec![("user", "What is the capital of France?")]);
        assert_eq!(classify_messages(&msgs, &None), Tier::Small);
    }

    #[test]
    fn test_code_question_routes_medium_or_large() {
        let msgs = make_messages(vec![("user", "How do I write a for loop in Python? Here is my code:\n```python\nfor i in range(10):\n    print(i)\n```")]);
        let tier = classify_messages(&msgs, &None);
        assert!(tier as usize >= Tier::Medium as usize, "Code questions should route to at least medium, got {:?}", tier);
    }

    #[test]
    fn test_debug_error_routes_large() {
        let msgs = make_messages(vec![("user", "I'm getting this error when running my code:\n```\nTraceback (most recent call last):\n  File 'main.py', line 42\n    result = divide(a, b)\nZeroDivisionError: division by zero\n```\nCan you help me debug this?")]);
        let tier = classify_messages(&msgs, &None);
        assert_eq!(tier, Tier::Large, "Debug/error questions should route to large");
    }

    #[test]
    fn test_complex_analysis_routes_large() {
        let msgs = make_messages(vec![("user", "Please compare and analyze the architectural differences between microservices and monolithic design patterns. What are the pros and cons of each approach?")]);
        let tier = classify_messages(&msgs, &None);
        assert_eq!(tier, Tier::Large, "Complex analysis should route to large");
    }

    #[test]
    fn test_tool_calling_routes_medium_plus() {
        let msgs = make_messages(vec![("user", "Search for the weather today")]);
        let tools = Some(vec![Tool {
            tool_type: "function".to_string(),
            function: ToolFunction {
                name: "search".to_string(),
                description: Some("Search the web".to_string()),
                parameters: serde_json::json!({"type": "object"}),
            },
        }]);
        let tier = classify_messages(&msgs, &tools);
        assert!(tier as usize >= Tier::Medium as usize, "Tool-calling requests should route to at least medium");
    }

    #[test]
    fn test_chinese_debug_routes_large() {
        let msgs = make_messages(vec![("user", "我的程序报错了，报错信息是 NullPointerException，请帮我修复一下")]);
        let tier = classify_messages(&msgs, &None);
        assert_eq!(tier, Tier::Large, "Chinese debug keywords should route to large");
    }

    #[test]
    fn test_math_proof_routes_large() {
        let msgs = make_messages(vec![("user", "Please prove that the square root of 2 is irrational using mathematical induction and derive the formula.")]);
        let tier = classify_messages(&msgs, &None);
        assert_eq!(tier, Tier::Large, "Math/proof questions should route to large");
    }
}
