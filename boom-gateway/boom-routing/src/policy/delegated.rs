use boom_core::provider::Provider;
use std::sync::Arc;

use super::SchedulePolicy;

/// Delegated scheduling: the gateway does not apply `round_robin` or `key_affinity`
/// across multiple upstream candidates. Replica placement and load-aware routing are
/// delegated to the downstream load balancer behind the shared `api_base`; the gateway
/// forwards the HTTP request unchanged (including body and `model`).
///
/// Each logical `model_name` must resolve to exactly one `Provider`. More than one
/// candidate yields `None` and a warning — fix `model_list` / DB to one row per model.
#[derive(Debug, Default, Clone, Copy)]
pub struct DelegatedPolicy;

impl DelegatedPolicy {
    pub fn new() -> Self {
        Self
    }
}

impl SchedulePolicy for DelegatedPolicy {
    fn select(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        _key_hash: Option<&str>,
        _input_chars: u64,
    ) -> Option<Arc<dyn Provider>> {
        match candidates.len() {
            0 => None,
            1 => Some(candidates[0].clone()),
            n => {
                let ids: Vec<&str> = candidates
                    .iter()
                    .map(|p| p.deployment_id().unwrap_or("(no id)"))
                    .collect();
                tracing::warn!(
                    model = %model,
                    candidate_count = n,
                    deployment_ids = ?ids,
                    concat!(
                        "delegated policy: expected exactly one deployment per model_name; ",
                        "routing strategy is delegated to the downstream load balancer — ",
                        "fix configuration",
                    ),
                );
                None
            }
        }
    }

    fn name(&self) -> &str {
        "delegated"
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use async_trait::async_trait;
    use boom_core::types::{
        ChatCompletionRequest, ChatCompletionResponse, ChatStream,
    };
    use boom_core::GatewayError;

    /// Stub `Provider`; only `deployment_id` is read by `DelegatedPolicy::select`.
    struct MockProvider {
        deployment: &'static str,
        models: Vec<String>,
    }

    impl MockProvider {
        fn new(deployment: &'static str) -> Self {
            Self {
                deployment,
                models: Vec::new(),
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<ChatCompletionResponse, GatewayError> {
            unreachable!("delegated policy tests do not call chat")
        }

        async fn chat_stream(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<ChatStream, GatewayError> {
            unreachable!("delegated policy tests do not call chat_stream")
        }

        fn name(&self) -> &str {
            "mock"
        }

        fn models(&self) -> &[String] {
            &self.models
        }

        fn deployment_id(&self) -> Option<&str> {
            Some(self.deployment)
        }
    }

    /// `model_name: gemini-pro` (config.example.yaml) but zero candidates after resolve —
    /// no provider to forward to.
    #[test]
    fn select_empty_returns_none() {
        let policy = DelegatedPolicy::new();
        let got = policy.select("gemini-pro", &[], None, 0);
        assert!(got.is_none());
    }

    /// One row: `model_name: claude-sonnet` with `model_info.id: node-a` —
    /// expect that deployment chosen.
    #[test]
    fn select_single_claude_sonnet_returns_node_a() {
        let policy = DelegatedPolicy::new();
        let p: Arc<dyn Provider> = Arc::new(MockProvider::new("node-a"));
        let candidates = vec![p.clone()];
        let got = policy.select("claude-sonnet", &candidates, None, 0);
        assert!(
            got.is_some(),
            "expected one provider to be selected",
        );
        assert_eq!(got.unwrap().deployment_id(), Some("node-a"));
    }

    /// Two rows for the same logical model (`gpt-4o` appears twice in config.example.yaml)
    /// — delegated must not pick one implicitly.
    #[test]
    fn select_two_gpt4o_candidates_returns_none() {
        let policy = DelegatedPolicy::new();
        // Simulates two `- model_name: gpt-4o` entries (default row + row with `api_base`);
        // deployment ids are illustrative.
        let primary: Arc<dyn Provider> = Arc::new(MockProvider::new("gpt-4o-default"));
        let backup: Arc<dyn Provider> = Arc::new(MockProvider::new("gpt-4o-api-base-row"));
        let candidates = vec![primary, backup];
        let got = policy.select("gpt-4o", &candidates, None, 0);
        assert!(got.is_none());
    }

    #[test]
    fn name_is_delegated() {
        assert_eq!(DelegatedPolicy::new().name(), "delegated");
    }
}
