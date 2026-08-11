use crate::{
    AliasStore, DeploymentStore, InFlightGuard, InFlightTracker, ModelCostRate,
    RequestRateTracker, Router,
};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use boom_config::{WorkflowDefinitionConfig, WorkflowSettings};
use boom_core::kv_event::{KvIndexBackend, StorageTier};
use boom_core::provider::{Provider, ProviderBilling, ProviderCallContext, ProviderCost};
use boom_core::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatStream, ChatStreamChunk, MessageContent,
    StreamUsage, Usage,
};
use boom_core::GatewayError;
use boom_flowcontrol::{FlowControlError, FlowControlGuard, FlowController};
use boom_fusion::{
    DirectSynthesisConfig, DirectSynthesisWorkflow, ModelInstance, ModelInvocation, ModelInvoker,
    ModelStreamInvocation, Workflow, WorkflowContext, WorkflowRegistry, WorkflowRole,
};
use futures::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

#[derive(Clone)]
pub struct FusionRuntime {
    router: Weak<Router>,
    deployment_store: Arc<DeploymentStore>,
    flow_controller: Arc<FlowController>,
    inflight: Arc<InFlightTracker>,
    request_rate: Arc<RequestRateTracker>,
    kv_index: Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
    enable_priority_header: bool,
    flow_control_queue_timeout: Duration,
}

impl FusionRuntime {
    pub fn new(
        router: Weak<Router>,
        deployment_store: Arc<DeploymentStore>,
        flow_controller: Arc<FlowController>,
        inflight: Arc<InFlightTracker>,
        request_rate: Arc<RequestRateTracker>,
        kv_index: Arc<ArcSwap<Option<Arc<dyn KvIndexBackend>>>>,
        enable_priority_header: bool,
        flow_control_queue_timeout_secs: u64,
    ) -> Self {
        Self {
            router,
            deployment_store,
            flow_controller,
            inflight,
            request_rate,
            kv_index,
            enable_priority_header,
            flow_control_queue_timeout: Duration::from_secs(flow_control_queue_timeout_secs),
        }
    }

    fn router(&self) -> Result<Arc<Router>, GatewayError> {
        self.router.upgrade().ok_or_else(|| {
            GatewayError::ProviderError("fusion routing runtime is unavailable".to_string())
        })
    }
}

/// Register configured workflow models as ordinary Provider deployments.
///
/// Workflow models own an exclusive candidate set. Registration fails rather
/// than replacing a YAML, DB-only, or dynamically-created resource.
pub fn register_fusion_providers(
    settings: &WorkflowSettings,
    deployment_store: &Arc<DeploymentStore>,
    alias_store: &Arc<AliasStore>,
    runtime: FusionRuntime,
) -> Result<(), GatewayError> {
    let registry = build_registry(settings)?;
    for model in settings.models.keys() {
        if deployment_store.contains(model) {
            return Err(GatewayError::ConfigError(format!(
                "workflow model '{}' conflicts with an existing deployment",
                model
            )));
        }
        if alias_store.resolve(model).is_some() {
            return Err(GatewayError::ConfigError(format!(
                "workflow model '{}' conflicts with an existing alias",
                model
            )));
        }
    }
    for model in settings.models.keys() {
        let workflow = registry.workflow_for_model(model).ok_or_else(|| {
            GatewayError::ConfigError(format!(
                "workflow model '{}' has no registered workflow",
                model
            ))
        })?;
        let provider: Arc<dyn Provider> = Arc::new(FusionProvider::new(
            model.clone(),
            workflow,
            runtime.clone(),
        ));
        deployment_store.set_exclusive_deployment(model.clone(), provider)?;
    }
    Ok(())
}

fn build_registry(settings: &WorkflowSettings) -> Result<WorkflowRegistry, GatewayError> {
    let mut workflows = HashMap::<String, Arc<dyn Workflow>>::new();

    for (workflow_id, definition) in &settings.workflows {
        let workflow: Arc<dyn Workflow> = match definition {
            WorkflowDefinitionConfig::DirectSynthesis {
                roles,
                panel_timeout_secs,
            } => {
                let panel = roles
                    .panel
                    .iter()
                    .map(|instance| ModelInstance {
                        model: instance.model.clone(),
                        temperature: instance.temperature,
                    })
                    .collect();
                let aggregator = ModelInstance {
                    model: roles.aggregator.model.clone(),
                    temperature: roles.aggregator.temperature,
                };
                Arc::new(
                    DirectSynthesisWorkflow::new(
                        workflow_id.clone(),
                        DirectSynthesisConfig {
                            panel,
                            aggregator,
                            panel_timeout: panel_timeout_secs.map(Duration::from_secs),
                        },
                    )
                    .map_err(GatewayError::ConfigError)?,
                )
            }
        };
        workflows.insert(workflow_id.clone(), workflow);
    }

    WorkflowRegistry::new(workflows, settings.models.clone()).map_err(GatewayError::ConfigError)
}

struct FusionProvider {
    model: String,
    models: Vec<String>,
    workflow: Arc<dyn Workflow>,
    runtime: FusionRuntime,
}

impl FusionProvider {
    fn new(model: String, workflow: Arc<dyn Workflow>, runtime: FusionRuntime) -> Self {
        Self {
            models: vec![model.clone()],
            model,
            workflow,
            runtime,
        }
    }

    fn context_required(&self) -> GatewayError {
        GatewayError::UnsupportedMode(format!(
            "fusion model '{}' is only supported by /v1/chat/completions",
            self.model
        ))
    }
}

#[async_trait]
impl Provider for FusionProvider {
    async fn chat(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        Err(self.context_required())
    }

    async fn chat_stream(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<ChatStream, GatewayError> {
        Err(self.context_required())
    }

    async fn chat_with_context(
        &self,
        request: ChatCompletionRequest,
        context: ProviderCallContext,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let execution = self
            .workflow
            .execute(WorkflowContext {
                request,
                invoker: Arc::new(RoutingModelInvoker::new(self.runtime.clone(), context)),
            })
            .await
            .map_err(|failure| failure.error)?;

        Ok(execution.response)
    }

    async fn chat_stream_with_context(
        &self,
        request: ChatCompletionRequest,
        context: ProviderCallContext,
    ) -> Result<ChatStream, GatewayError> {
        let execution = self
            .workflow
            .execute_stream(WorkflowContext {
                request,
                invoker: Arc::new(RoutingModelInvoker::new(self.runtime.clone(), context)),
            })
            .await
            .map_err(|failure| failure.error)?;

        Ok(execution.stream)
    }

    fn name(&self) -> &str {
        "fusion"
    }

    fn models(&self) -> &[String] {
        &self.models
    }
}

struct RoutingModelInvoker {
    runtime: FusionRuntime,
    context: ProviderCallContext,
}

impl RoutingModelInvoker {
    fn new(runtime: FusionRuntime, context: ProviderCallContext) -> Self {
        Self { runtime, context }
    }

    async fn prepare_call(
        &self,
        mut request: ChatCompletionRequest,
    ) -> Result<PreparedModelCall, GatewayError> {
        let requested_model = request.model.clone();
        let input_chars = request_input_chars(&request) as u64;
        let router = self.runtime.router()?;
        let resolved_model =
            router.resolve_request_model(&requested_model, &request.messages, &request.tools)
                .await;
        let kv_index = (**self.runtime.kv_index.load()).clone();
        let prefix_bytes = if kv_index.is_some() {
            request_prefix_bytes(&request)
        } else {
            Vec::new()
        };
        let selection = router
            .select_provider_with_prefix(
                &resolved_model,
                Some(&self.context.key_hash),
                input_chars,
                &prefix_bytes,
            )
            .ok_or_else(|| GatewayError::ModelNotFound(resolved_model.clone()))?;
        let record_prefix = selection.kv_match_attempted;
        let provider = selection.provider;
        if provider.name() == "fusion" {
            return Err(GatewayError::ConfigError(format!(
                "fusion child model '{}' resolves to virtual provider '{}'",
                requested_model,
                provider.name()
            )));
        }

        let deployment_id = provider.deployment_id().map(str::to_string);
        let billing_model = router.resolve_model_name(&resolved_model);
        let cost_rate = self
            .runtime
            .deployment_store
            .get_cost_rate(&billing_model);
        if record_prefix {
            if let (Some(index), Some(worker_id)) = (kv_index, provider.kv_worker_id()) {
                index.record_request_prefix(
                    &resolved_model,
                    worker_id,
                    &prefix_bytes,
                    StorageTier::Gpu,
                );
            }
        }
        request.gateway_headers = build_gateway_headers(
            self.context.is_vip,
            self.runtime.enable_priority_header,
            &self.context.api_path,
            provider.client_type_header(),
        );
        request.extra.remove("metadata");

        let flow_guard = if let Some(deployment_id) = deployment_id.as_deref() {
            match self
                .runtime
                .flow_controller
                .acquire(
                    deployment_id,
                    input_chars,
                    self.runtime.flow_control_queue_timeout,
                    self.context.is_vip,
                    self.context.key_alias.clone(),
                    Some(self.context.key_hash.clone()),
                    Some(billing_model.clone()),
                )
                .await
            {
                Ok(guard) => Some(guard),
                Err(FlowControlError::NoSlot) => None,
                Err(FlowControlError::Timeout { waiters, .. }) => {
                    return Err(GatewayError::FlowControlQueueTimeout {
                        deployment_id: deployment_id.to_string(),
                        waiters,
                        message: format!(
                            "Deployment '{}' fusion child call queue timeout",
                            deployment_id
                        ),
                    });
                }
                Err(FlowControlError::ContextExceeded {
                    context_chars,
                    max_context,
                    ..
                }) => {
                    return Err(GatewayError::RateLimitExceeded {
                        retry_after_secs: None,
                        message: format!(
                            "Fusion child context ({} chars) exceeds deployment max_context ({})",
                            context_chars, max_context
                        ),
                        limit_type: "flow_control_context",
                        scope: None,
                        scope_id: None,
                        plan_name: None,
                    });
                }
            }
        } else {
            None
        };
        let inflight = if let Some(deployment_id) = deployment_id.as_deref() {
            InFlightGuard::new_for_deployment(
                self.runtime.inflight.clone(),
                &billing_model,
                deployment_id,
                input_chars,
            )
        } else {
            InFlightGuard::new(self.runtime.inflight.clone(), &billing_model, input_chars)
        };

        Ok(PreparedModelCall {
            requested_model,
            request,
            provider,
            deployment_id,
            cost_rate,
            flow_guard,
            inflight,
        })
    }

    fn record_success(&self, deployment_id: Option<&str>) {
        if let Some(deployment_id) = deployment_id {
            self.runtime.request_rate.record(deployment_id);
        }
    }
}

struct PreparedModelCall {
    requested_model: String,
    request: ChatCompletionRequest,
    provider: Arc<dyn Provider>,
    deployment_id: Option<String>,
    cost_rate: ModelCostRate,
    flow_guard: Option<FlowControlGuard>,
    inflight: InFlightGuard,
}

#[async_trait]
impl ModelInvoker for RoutingModelInvoker {
    async fn invoke(
        &self,
        workflow_id: &str,
        role: WorkflowRole,
        request: ChatCompletionRequest,
    ) -> Result<ModelInvocation, GatewayError> {
        let PreparedModelCall {
            requested_model,
            request,
            provider,
            deployment_id,
            cost_rate,
            flow_guard: _flow_guard,
            inflight: _inflight,
        } = self.prepare_call(request).await?;
        tracing::info!(
            workflow_id,
            role = role.as_str(),
            model = %requested_model,
            deployment_id = deployment_id.as_deref(),
            "fusion child model call started"
        );
        let response = provider.chat(request).await?;
        self.record_success(deployment_id.as_deref());
        self.context.billing.add_actual_usage(&response.usage);
        let cost = response_cost(&cost_rate, &response.usage);
        self.context.billing.add_actual_cost(&cost);

        Ok(ModelInvocation { response })
    }

    async fn invoke_stream(
        &self,
        workflow_id: &str,
        role: WorkflowRole,
        request: ChatCompletionRequest,
    ) -> Result<ModelStreamInvocation, GatewayError> {
        let PreparedModelCall {
            requested_model,
            request,
            provider,
            deployment_id,
            cost_rate,
            flow_guard,
            inflight,
        } = self.prepare_call(request).await?;
        tracing::info!(
            workflow_id,
            role = role.as_str(),
            model = %requested_model,
            deployment_id = deployment_id.as_deref(),
            stream = true,
            "fusion child model call started"
        );
        let stream = provider.chat_stream(request).await?;
        self.record_success(deployment_id.as_deref());

        Ok(ModelStreamInvocation {
            stream: Box::pin(GuardedFusionStream::new(
                stream,
                flow_guard,
                inflight,
                cost_rate,
                self.context.billing.clone(),
            )),
        })
    }
}

struct GuardedFusionStream {
    inner: ChatStream,
    flow_guard: Option<FlowControlGuard>,
    inflight: Option<InFlightGuard>,
    cost_rate: ModelCostRate,
    usage: Usage,
    reported_cost: ProviderCost,
    billing: ProviderBilling,
}

impl GuardedFusionStream {
    fn new(
        inner: ChatStream,
        flow_guard: Option<FlowControlGuard>,
        inflight: InFlightGuard,
        cost_rate: ModelCostRate,
        billing: ProviderBilling,
    ) -> Self {
        Self {
            inner,
            flow_guard,
            inflight: Some(inflight),
            cost_rate,
            usage: Usage::default(),
            reported_cost: ProviderCost::default(),
            billing,
        }
    }
}

impl Stream for GuardedFusionStream {
    type Item = Result<ChatStreamChunk, GatewayError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let result = this.inner.as_mut().poll_next(context);
        if let Poll::Ready(Some(Ok(chunk))) = &result {
            if let Some(usage) = &chunk.usage {
                let previous_usage = this.usage.clone();
                update_usage_snapshot(&mut this.usage, usage);
                this.billing
                    .add_actual_usage(&usage_delta(&this.usage, &previous_usage));
                let cost = response_cost(&this.cost_rate, &this.usage);
                let delta = ProviderCost {
                    regular_input: cost.regular_input - this.reported_cost.regular_input,
                    cached_input: cost.cached_input - this.reported_cost.cached_input,
                    output: cost.output - this.reported_cost.output,
                };
                this.billing.add_actual_cost(&delta);
                this.reported_cost = cost;
            }
        }
        if matches!(result, Poll::Ready(None)) {
            this.flow_guard.take();
            this.inflight.take();
        }
        result
    }
}

fn response_cost(rate: &ModelCostRate, usage: &Usage) -> ProviderCost {
    let cached_tokens = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .unwrap_or(0);
    let (regular_input, cached_input, output) = rate.compute_cost_breakdown(
        u64::from(usage.prompt_tokens),
        u64::from(cached_tokens),
        u64::from(usage.completion_tokens),
    );
    ProviderCost {
        regular_input,
        cached_input,
        output,
    }
}

fn update_usage_snapshot(target: &mut Usage, usage: &StreamUsage) {
    if let Some(prompt_tokens) = usage.prompt_tokens {
        target.prompt_tokens = prompt_tokens.max(0) as u32;
    }
    if let Some(completion_tokens) = usage.completion_tokens {
        target.completion_tokens = completion_tokens.max(0) as u32;
    }
    target.total_tokens = usage.total_tokens.map_or_else(
        || {
            target
                .prompt_tokens
                .saturating_add(target.completion_tokens)
        },
        |total_tokens| total_tokens.max(0) as u32,
    );
    if let Some(cached_tokens) = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
    {
        target
            .prompt_tokens_details
            .get_or_insert_default()
            .cached_tokens = Some(cached_tokens);
    }
}

fn usage_delta(current: &Usage, previous: &Usage) -> Usage {
    Usage {
        prompt_tokens: current
            .prompt_tokens
            .saturating_sub(previous.prompt_tokens),
        completion_tokens: current
            .completion_tokens
            .saturating_sub(previous.completion_tokens),
        total_tokens: current.total_tokens.saturating_sub(previous.total_tokens),
        prompt_tokens_details: current
            .prompt_tokens_details
            .as_ref()
            .and_then(|current_details| {
                current_details.cached_tokens.map(|cached_tokens| {
                    let previous_cached = previous
                        .prompt_tokens_details
                        .as_ref()
                        .and_then(|details| details.cached_tokens)
                        .unwrap_or(0);
                    boom_core::types::PromptTokensDetails {
                        cached_tokens: Some(cached_tokens.saturating_sub(previous_cached)),
                    }
                })
            }),
        cache_creation_input_tokens: current.cache_creation_input_tokens.map(|value| {
            value.saturating_sub(previous.cache_creation_input_tokens.unwrap_or(0))
        }),
        cache_read_input_tokens: current.cache_read_input_tokens.map(|value| {
            value.saturating_sub(previous.cache_read_input_tokens.unwrap_or(0))
        }),
    }
}

fn request_prefix_bytes(request: &ChatCompletionRequest) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(tools) = &request.tools {
        if !tools.is_empty() {
            if let Ok(value) = serde_json::to_vec(tools) {
                bytes.extend_from_slice(&value);
            }
        }
    }
    if let Ok(value) = serde_json::to_vec(&request.messages) {
        bytes.extend_from_slice(&value);
    }
    bytes
}

fn request_input_chars(request: &ChatCompletionRequest) -> usize {
    request
        .messages
        .iter()
        .map(|message| match &message.content {
            MessageContent::Text(text) => text.len(),
            MessageContent::Parts(parts) => parts
                .iter()
                .map(|part| match part {
                    boom_core::types::ContentPart::Text { text } => text.len(),
                    _ => 0,
                })
                .sum(),
            MessageContent::Null => 0,
        })
        .sum()
}

pub fn build_gateway_headers(
    is_vip: bool,
    enable_priority_header: bool,
    api_path: &str,
    client_type_enabled: bool,
) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    if enable_priority_header {
        headers.insert(
            "X-Gateway-Priority".to_string(),
            if is_vip { "100" } else { "0" }.to_string(),
        );
    }
    if client_type_enabled {
        let kind = boom_ctxaware::classify(api_path);
        headers.insert(
            boom_ctxaware::CLIENT_TYPE_HEADER.to_string(),
            kind.wire_label().to_string(),
        );
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::{
        build_gateway_headers, register_fusion_providers, request_prefix_bytes, FusionRuntime,
    };
    use crate::{
        AliasStore, DeploymentStore, InFlightTracker, ModelCostRate, RequestRateTracker, Router,
        SchedulePolicy,
    };
    use arc_swap::ArcSwap;
    use async_trait::async_trait;
    use boom_config::Config;
    use boom_core::provider::{Provider, ProviderBilling, ProviderCallContext};
    use boom_core::types::{
        ChatCompletionRequest, ChatCompletionResponse, ChatStream, ChatStreamChunk, Choice, Message,
        MessageContent, MessageRole, StreamChoice, StreamDelta, StreamUsage, Usage,
    };
    use boom_core::GatewayError;
    use boom_flowcontrol::FlowController;
    use futures::StreamExt;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    struct RecordingPolicy {
        key_hashes: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl SchedulePolicy for RecordingPolicy {
        fn select(
            &self,
            _model: &str,
            candidates: &[Arc<dyn Provider>],
            key_hash: Option<&str>,
            _input_chars: u64,
        ) -> Option<Arc<dyn Provider>> {
            self.key_hashes
                .lock()
                .unwrap()
                .push(key_hash.map(str::to_string));
            candidates.first().cloned()
        }

        fn name(&self) -> &str {
            "recording"
        }
    }

    struct FakeProvider {
        calls: Arc<Mutex<Vec<ChatCompletionRequest>>>,
        fail_models: Arc<Mutex<HashSet<String>>>,
        invalid_models: Arc<Mutex<HashSet<String>>>,
        models: Vec<String>,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn chat(
            &self,
            request: ChatCompletionRequest,
        ) -> Result<ChatCompletionResponse, GatewayError> {
            self.calls.lock().unwrap().push(request.clone());
            if self.fail_models.lock().unwrap().contains(&request.model) {
                return Err(GatewayError::ProviderError(format!(
                    "{} unavailable",
                    request.model
                )));
            }
            let content = if self.invalid_models.lock().unwrap().contains(&request.model) {
                String::new()
            } else {
                format!("answer from {}", request.model)
            };
            Ok(ChatCompletionResponse {
                id: format!("chatcmpl-{}", request.model),
                object: "chat.completion".to_string(),
                created: 1,
                model: request.model.clone(),
                choices: vec![Choice {
                    index: 0,
                    message: Message {
                        role: MessageRole::Assistant,
                        content: MessageContent::Text(content),
                        name: None,
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                    },
                    finish_reason: Some("stop".to_string()),
                    logprobs: None,
                }],
                usage: Usage {
                    prompt_tokens: 2,
                    completion_tokens: 1,
                    total_tokens: 3,
                    ..Usage::default()
                },
                system_fingerprint: None,
                raw_response: None,
            })
        }

        async fn chat_stream(
            &self,
            request: ChatCompletionRequest,
        ) -> Result<ChatStream, GatewayError> {
            self.calls.lock().unwrap().push(request.clone());
            Ok(Box::pin(futures::stream::iter([Ok(ChatStreamChunk {
                id: format!("chatcmpl-stream-{}", request.model),
                object: "chat.completion.chunk".to_string(),
                created: 1,
                model: request.model,
                choices: vec![StreamChoice {
                    index: 0,
                    delta: StreamDelta {
                        role: Some(MessageRole::Assistant),
                        content: Some("streamed answer".to_string()),
                        tool_calls: None,
                        reasoning_content: None,
                    },
                    finish_reason: Some("stop".to_string()),
                }],
                usage: Some(StreamUsage {
                    prompt_tokens: Some(2),
                    completion_tokens: Some(1),
                    total_tokens: Some(3),
                    prompt_tokens_details: None,
                }),
                raw_data: None,
            })])))
        }

        fn name(&self) -> &str {
            "fake"
        }

        fn models(&self) -> &[String] {
            &self.models
        }

        fn deployment_id(&self) -> Option<&str> {
            Some("fake-deployment")
        }

        fn client_type_header(&self) -> bool {
            true
        }
    }

    #[test]
    fn gateway_headers_preserve_priority_and_client_type_rules() {
        let headers = build_gateway_headers(true, true, "/v1/chat/completions", true);
        assert_eq!(
            headers.get("X-Gateway-Priority").map(String::as_str),
            Some("100")
        );
        assert_eq!(
            headers
                .get(boom_ctxaware::CLIENT_TYPE_HEADER)
                .map(String::as_str),
            Some("anonymous")
        );
    }

    #[test]
    fn kvc_prefix_places_tools_before_messages() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "panel-a",
            "messages": [{"role": "user", "content": "solve it"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "bash",
                    "parameters": {"type": "object"}
                }
            }]
        }))
        .unwrap();
        let mut expected = serde_json::to_vec(request.tools.as_ref().unwrap()).unwrap();
        expected.extend_from_slice(&serde_json::to_vec(&request.messages).unwrap());

        assert_eq!(request_prefix_bytes(&request), expected);
    }

    #[tokio::test]
    async fn fusion_children_reenter_router_with_parent_context() {
        let config: Config = serde_yaml::from_str(
            r#"
model_list:
  - model_name: panel-a
    litellm_params:
      model: openai/panel-a
  - model_name: panel-b
    litellm_params:
      model: openai/panel-b
  - model_name: aggregator
    litellm_params:
      model: openai/aggregator
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: panel-a
            temperature: 0.3
          - model: panel-b
            temperature: 0.3
        aggregator:
          model: aggregator
          temperature: 0
"#,
        )
        .unwrap();
        config.validate().unwrap();

        let deployment_store = Arc::new(DeploymentStore::new());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fail_models = Arc::new(Mutex::new(HashSet::new()));
        let invalid_models = Arc::new(Mutex::new(HashSet::new()));
        let fake: Arc<dyn Provider> = Arc::new(FakeProvider {
            calls: calls.clone(),
            fail_models: fail_models.clone(),
            invalid_models: invalid_models.clone(),
            models: vec![
                "panel-a".to_string(),
                "panel-b".to_string(),
                "aggregator".to_string(),
            ],
        });
        for model in ["panel-a", "panel-b", "aggregator"] {
            deployment_store.add_deployment(model, fake.clone());
        }
        deployment_store.set_cost_rate(
            "panel-a",
            ModelCostRate::new(1.into(), 10.into()),
        );
        deployment_store.set_cost_rate(
            "panel-b",
            ModelCostRate::new(2.into(), 20.into()),
        );
        deployment_store.set_cost_rate(
            "aggregator",
            ModelCostRate::new(3.into(), 30.into()),
        );

        let key_hashes = Arc::new(Mutex::new(Vec::new()));
        let alias_store = Arc::new(AliasStore::new());
        let router = Arc::new(Router::new(
            deployment_store.clone(),
            alias_store.clone(),
            Arc::new(RecordingPolicy {
                key_hashes: key_hashes.clone(),
            }),
        ));
        let runtime = FusionRuntime::new(
            Arc::downgrade(&router),
            deployment_store.clone(),
            Arc::new(FlowController::new()),
            Arc::new(InFlightTracker::new()),
            Arc::new(RequestRateTracker::new()),
            Arc::new(ArcSwap::from_pointee(None)),
            true,
            1200,
        );
        register_fusion_providers(
            &config.workflow_settings,
            &deployment_store,
            &alias_store,
            runtime.clone(),
        )
        .unwrap();
        assert!(!deployment_store.add_deployment("fusion", fake.clone()));

        let fusion = router
            .select_provider_with_prefix("fusion", Some("parent-key"), 4, &[])
            .unwrap()
            .provider;
        assert_eq!(fusion.name(), "fusion");
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "fusion",
            "messages": [{"role": "user", "content": "solve it"}],
            "metadata": {"run_id": "routing-test"}
        }))
        .unwrap();
        let billing = ProviderBilling::default();
        let result = fusion
            .chat_with_context(
                request,
                ProviderCallContext {
                    key_hash: "parent-key".to_string(),
                    key_alias: Some("parent-alias".to_string()),
                    is_vip: true,
                    api_path: "/v1/chat/completions".to_string(),
                    billing: billing.clone(),
                },
            )
            .await
            .unwrap();

        assert_eq!(result.usage.total_tokens, 9);
        assert_eq!(billing.actual_usage().unwrap().total_tokens, 9);
        let actual_cost = billing.actual_cost().unwrap();
        assert_eq!(actual_cost.regular_input, 12.into());
        assert_eq!(actual_cost.cached_input, 0.into());
        assert_eq!(actual_cost.output, 60.into());
        assert_eq!(actual_cost.total(), 72.into());

        let routed_keys = key_hashes.lock().unwrap();
        assert_eq!(routed_keys.len(), 4);
        assert_eq!(routed_keys[0], Some("parent-key".to_string()));
        assert!(routed_keys[1..]
            .iter()
            .all(|key| key.as_deref() == Some("parent-key")));

        let child_calls = calls.lock().unwrap();
        assert_eq!(child_calls.len(), 3);
        assert!(child_calls
            .iter()
            .all(|call| !call.extra.contains_key("metadata")));
        assert!(child_calls.iter().all(|call| {
            call.gateway_headers
                .get("X-Gateway-Priority")
                .is_some_and(|value| value == "100")
        }));
        assert!(child_calls.iter().all(|call| {
            call.gateway_headers
                .get(boom_ctxaware::CLIENT_TYPE_HEADER)
                .is_some_and(|value| value == "anonymous")
        }));
        drop(child_calls);
        drop(routed_keys);

        let stream_billing = ProviderBilling::default();
        let stream = fusion
            .chat_stream_with_context(
                serde_json::from_value(json!({
                    "model": "fusion",
                    "messages": [{"role": "user", "content": "stream it"}],
                    "stream": true
                }))
                .unwrap(),
                ProviderCallContext {
                    key_hash: "parent-key".to_string(),
                    key_alias: Some("parent-alias".to_string()),
                    is_vip: true,
                    api_path: "/v1/chat/completions".to_string(),
                    billing: stream_billing.clone(),
                },
            )
            .await
            .unwrap();
        let chunks = stream.collect::<Vec<_>>().await;
        assert!(chunks.iter().all(Result::is_ok));
        assert_eq!(stream_billing.actual_usage().unwrap().total_tokens, 9);
        assert_eq!(stream_billing.actual_cost().unwrap().total(), 72.into());

        fail_models
            .lock()
            .unwrap()
            .insert("aggregator".to_string());
        let fallback_billing = ProviderBilling::default();
        let fallback = fusion
            .chat_with_context(
                serde_json::from_value(json!({
                    "model": "fusion",
                    "messages": [{"role": "user", "content": "fall back"}]
                }))
                .unwrap(),
                ProviderCallContext {
                    key_hash: "parent-key".to_string(),
                    key_alias: Some("parent-alias".to_string()),
                    is_vip: true,
                    api_path: "/v1/chat/completions".to_string(),
                    billing: fallback_billing.clone(),
                },
            )
            .await
            .unwrap();
        assert_eq!(fallback.usage.total_tokens, 6);
        assert_eq!(fallback_billing.actual_usage().unwrap().total_tokens, 6);
        assert_eq!(fallback_billing.actual_cost().unwrap().total(), 36.into());

        fail_models.lock().unwrap().clear();
        invalid_models
            .lock()
            .unwrap()
            .extend(["panel-a".to_string(), "panel-b".to_string()]);
        let invalid_billing = ProviderBilling::default();
        let invalid_result = fusion
            .chat_with_context(
                serde_json::from_value(json!({
                    "model": "fusion",
                    "messages": [{"role": "user", "content": "invalid panels"}]
                }))
                .unwrap(),
                ProviderCallContext {
                    key_hash: "parent-key".to_string(),
                    key_alias: Some("parent-alias".to_string()),
                    is_vip: true,
                    api_path: "/v1/chat/completions".to_string(),
                    billing: invalid_billing.clone(),
                },
            )
            .await;
        assert!(invalid_result.is_err());
        assert_eq!(invalid_billing.actual_usage().unwrap().total_tokens, 6);
        assert_eq!(invalid_billing.actual_cost().unwrap().total(), 36.into());

        alias_store.set_alias(
            "recursive-panel-alias".to_string(),
            "fusion".to_string(),
            false,
        );
        let recursive_billing = ProviderBilling::default();
        let recursive_invoker = super::RoutingModelInvoker::new(
            runtime,
            ProviderCallContext {
                key_hash: "parent-key".to_string(),
                key_alias: Some("parent-alias".to_string()),
                is_vip: true,
                api_path: "/v1/chat/completions".to_string(),
                billing: recursive_billing,
            },
        );
        let recursive_request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "recursive-panel-alias",
            "messages": [{"role": "user", "content": "do not recurse"}]
        }))
        .unwrap();
        let error = match recursive_invoker.prepare_call(recursive_request).await {
            Ok(_) => panic!("fusion child alias must not resolve to FusionProvider"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("resolves to virtual provider"));
    }
}
