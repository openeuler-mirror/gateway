use crate::{
    AliasStore, DeploymentStore, InFlightGuard, InFlightTracker, ModelCostRate,
    RequestRateTracker, Router,
};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use boom_config::{WorkflowDefinitionConfig, WorkflowSettings};
use boom_core::kv_event::{KvIndexBackend, StorageTier};
use boom_core::provider::{
    Provider, ProviderBilling, ProviderCallContext, ProviderCost, ProviderPromptTrace,
    ProviderProtocol, SharedProviderPromptTrace,
};
use boom_core::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatStream, ChatStreamChunk, MessageContent,
    StreamUsage, Usage,
};
use boom_core::GatewayError;
use boom_flowcontrol::{FlowControlError, FlowControlGuard, FlowController};
use boom_fusion::{
    DirectSynthesisConfig, DirectSynthesisWorkflow, ModelInstance, ModelInvocation,
    ModelInvoker, ModelStreamInvocation, Workflow, WorkflowContext, WorkflowRegistry,
    WorkflowRole,
};
use futures::{Stream, TryStreamExt};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

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
    validate_fusion_child_providers(settings, deployment_store, alias_store)?;
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

fn validate_fusion_child_providers(
    settings: &WorkflowSettings,
    deployment_store: &DeploymentStore,
    alias_store: &AliasStore,
) -> Result<(), GatewayError> {
    for (workflow_id, definition) in &settings.workflows {
        let WorkflowDefinitionConfig::DirectSynthesis { roles, .. } = definition;
        for (role, instance) in roles
            .panel
            .iter()
            .map(|instance| ("panel", instance))
            .chain(std::iter::once(("aggregator", &roles.aggregator)))
        {
            let resolved_model = alias_store
                .resolve(&instance.model)
                .unwrap_or_else(|| instance.model.clone());
            let providers = deployment_store
                .get_providers(&resolved_model)
                .filter(|providers| !providers.is_empty())
                .ok_or_else(|| {
                    GatewayError::ConfigError(format!(
                        "workflow '{}' {} model '{}' resolves to model '{}', which has no active deployment",
                        workflow_id, role, instance.model, resolved_model
                    ))
                })?;
            for provider in providers {
                if provider.protocol() != ProviderProtocol::OpenAiCompatible {
                    return Err(GatewayError::ConfigError(format!(
                        "workflow '{}' {} model '{}' resolves to provider '{}'{}; Fusion child models must use an OpenAI-compatible provider",
                        workflow_id,
                        role,
                        instance.model,
                        provider.name(),
                        provider
                            .deployment_id()
                            .map(|id| format!(" (deployment_id='{id}')"))
                            .unwrap_or_default()
                    )));
                }
            }
        }
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

#[derive(Default)]
struct FusionPromptTrace {
    calls: Mutex<Vec<serde_json::Value>>,
    started: Mutex<Vec<Instant>>,
    streams: Mutex<HashMap<usize, Arc<Mutex<FusionStreamTraceState>>>>,
}

impl FusionPromptTrace {
    fn start_call(&self, mut call: serde_json::Value) -> Option<usize> {
        let mut calls = self.calls.lock().ok()?;
        let index = calls.len();
        if let Some(object) = call.as_object_mut() {
            object.insert("sequence".to_string(), serde_json::json!(index + 1));
            object.insert("status".to_string(), serde_json::json!("started"));
        }
        calls.push(call);
        if let Ok(mut started) = self.started.lock() {
            started.push(Instant::now());
        }
        Some(index)
    }

    fn update_call(&self, index: usize, update: serde_json::Value) {
        let Ok(mut calls) = self.calls.lock() else {
            return;
        };
        let Some(call) = calls
            .get_mut(index)
            .and_then(serde_json::Value::as_object_mut)
        else {
            return;
        };
        let serde_json::Value::Object(update) = update else {
            return;
        };
        for (key, value) in update {
            call.insert(key, value);
        }
    }

    fn register_stream(&self, index: usize) -> Arc<Mutex<FusionStreamTraceState>> {
        let state = Arc::new(Mutex::new(FusionStreamTraceState::default()));
        if let Ok(mut streams) = self.streams.lock() {
            streams.insert(index, state.clone());
        }
        state
    }

    fn stream_response(&self, index: usize) -> Option<serde_json::Value> {
        let state = self.streams.lock().ok()?.get(&index).cloned()?;
        state.lock().ok().map(|state| state.response())
    }
}

impl ProviderPromptTrace for FusionPromptTrace {
    fn finalize(&self) {
        // A route-level Prompt Log can be dropped before the background task
        // consuming a provider stream. Merge its latest compact state before
        // closing calls that never reached a terminal state.
        let Ok(mut calls) = self.calls.lock() else {
            return;
        };
        let Ok(started) = self.started.lock() else {
            return;
        };
        for (index, call) in calls.iter_mut().enumerate() {
            let Some(call) = call.as_object_mut() else {
                continue;
            };
            let status = call
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if matches!(status, "succeeded" | "failed" | "cancelled") {
                continue;
            }
            if let Some(response) = self.stream_response(index) {
                call.insert("response".to_string(), response);
            }
            call.insert("status".to_string(), serde_json::json!("cancelled"));
            if let Some(started) = started.get(index) {
                call.insert(
                    "duration_ms".to_string(),
                    serde_json::json!(elapsed_ms(*started)),
                );
            }
        }
    }

    fn snapshot(&self) -> Option<serde_json::Value> {
        let calls = self.calls.lock().ok()?;
        if calls.is_empty() {
            None
        } else {
            Some(serde_json::json!({ "calls": calls.clone() }))
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Default)]
struct FusionStreamTraceState {
    event_count: u64,
    last_chunk: Option<serde_json::Value>,
}

impl FusionStreamTraceState {
    fn response(&self) -> serde_json::Value {
        serde_json::json!({
            "stream": true,
            "event_count": self.event_count,
            "last_chunk": self.last_chunk,
        })
    }
}

fn fusion_prompt_trace(trace: &SharedProviderPromptTrace) -> Option<&FusionPromptTrace> {
    trace.as_any().downcast_ref::<FusionPromptTrace>()
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
        let workflow = self.workflow.clone();
        let invoker = Arc::new(RoutingModelInvoker::new(self.runtime.clone(), context));
        // Let the route establish SSE before panel and aggregator work begins.
        let pending = futures::stream::once(async move {
            workflow
                .execute_stream(WorkflowContext { request, invoker })
                .await
                .map(|execution| execution.stream)
                .map_err(|failure| failure.error)
        });

        Ok(Box::pin(pending.try_flatten()))
    }

    fn create_prompt_trace(&self) -> Option<SharedProviderPromptTrace> {
        Some(Arc::new(FusionPromptTrace::default()))
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

    fn start_prompt_call(
        &self,
        fusion: &str,
        role: WorkflowRole,
        requested_model: &str,
        stream: bool,
    ) -> Option<usize> {
        fusion_prompt_trace(self.context.prompt_trace.as_ref()?)?.start_call(
            serde_json::json!({
                "fusion": fusion,
                "role": role.as_str(),
                "model": requested_model,
                "stream": stream,
            }),
        )
    }

    fn route_prompt_call(
        &self,
        call_index: Option<usize>,
        provider: &dyn Provider,
        deployment_id: Option<&str>,
        request: &ChatCompletionRequest,
    ) {
        let (Some(prompt_trace), Some(call_index)) =
            (self.context.prompt_trace.as_ref(), call_index)
        else {
            return;
        };
        let Some(prompt_trace) = fusion_prompt_trace(prompt_trace) else {
            return;
        };
        prompt_trace.update_call(
            call_index,
            serde_json::json!({
                "status": "routed",
                "provider": provider.name(),
                "deployment_id": deployment_id,
                "request": serde_json::to_value(request).unwrap_or(serde_json::Value::Null),
            }),
        );
    }

    fn finish_prompt_call(
        &self,
        call_index: Option<usize>,
        started: Instant,
        status: &str,
        response: Option<serde_json::Value>,
        error: Option<&GatewayError>,
    ) {
        let (Some(prompt_trace), Some(call_index)) =
            (self.context.prompt_trace.as_ref(), call_index)
        else {
            return;
        };
        let Some(prompt_trace) = fusion_prompt_trace(prompt_trace) else {
            return;
        };
        let mut update = serde_json::json!({
            "status": status,
            "duration_ms": elapsed_ms(started),
        });
        if let Some(response) = response {
            update["response"] = response;
        }
        if let Some(error) = error {
            update["error"] = fusion_call_error(error);
        }
        prompt_trace.update_call(call_index, update);
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
        let requested_model = request.model.clone();
        let prompt_call = self.start_prompt_call(workflow_id, role, &requested_model, false);
        let started = Instant::now();
        let prepared = match self.prepare_call(request).await {
            Ok(prepared) => prepared,
            Err(error) => {
                self.finish_prompt_call(prompt_call, started, "failed", None, Some(&error));
                return Err(error);
            }
        };
        let PreparedModelCall {
            requested_model,
            request,
            provider,
            deployment_id,
            cost_rate,
            flow_guard: _flow_guard,
            inflight: _inflight,
        } = prepared;
        self.route_prompt_call(
            prompt_call,
            provider.as_ref(),
            deployment_id.as_deref(),
            &request,
        );
        tracing::info!(
            workflow_id,
            role = role.as_str(),
            model = %requested_model,
            deployment_id = deployment_id.as_deref(),
            "fusion child model call started"
        );
        let response = match provider.chat(request).await {
            Ok(response) => {
                self.finish_prompt_call(
                    prompt_call,
                    started,
                    "succeeded",
                    Some(serde_json::to_value(&response).unwrap_or(serde_json::Value::Null)),
                    None,
                );
                response
            }
            Err(error) => {
                self.finish_prompt_call(prompt_call, started, "failed", None, Some(&error));
                return Err(error);
            }
        };
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
        let requested_model = request.model.clone();
        let prompt_call = self.start_prompt_call(workflow_id, role, &requested_model, true);
        let started = Instant::now();
        let prepared = match self.prepare_call(request).await {
            Ok(prepared) => prepared,
            Err(error) => {
                self.finish_prompt_call(prompt_call, started, "failed", None, Some(&error));
                return Err(error);
            }
        };
        let PreparedModelCall {
            requested_model,
            request,
            provider,
            deployment_id,
            cost_rate,
            flow_guard,
            inflight,
        } = prepared;
        self.route_prompt_call(
            prompt_call,
            provider.as_ref(),
            deployment_id.as_deref(),
            &request,
        );
        tracing::info!(
            workflow_id,
            role = role.as_str(),
            model = %requested_model,
            deployment_id = deployment_id.as_deref(),
            stream = true,
            "fusion child model call started"
        );
        let stream = match provider.chat_stream(request).await {
            Ok(stream) => {
                if let (Some(prompt_trace), Some(call_index)) =
                    (self.context.prompt_trace.as_ref(), prompt_call)
                {
                    if let Some(prompt_trace) = fusion_prompt_trace(prompt_trace) {
                        prompt_trace.update_call(
                            call_index,
                            serde_json::json!({ "status": "streaming" }),
                        );
                    }
                }
                stream
            }
            Err(error) => {
                self.finish_prompt_call(prompt_call, started, "failed", None, Some(&error));
                return Err(error);
            }
        };
        self.record_success(deployment_id.as_deref());
        let prompt_trace = prompt_call.and_then(|call_index| {
            self.context
                .prompt_trace
                .clone()
                .map(|prompt_trace| FusionStreamPromptLog::new(prompt_trace, call_index, started))
        });

        Ok(ModelStreamInvocation {
            stream: Box::pin(GuardedFusionStream::new(
                stream,
                flow_guard,
                inflight,
                cost_rate,
                self.context.billing.clone(),
                prompt_trace,
            )),
        })
    }
}

struct FusionStreamPromptLog {
    prompt_trace: SharedProviderPromptTrace,
    call_index: usize,
    started: Instant,
    state: Arc<Mutex<FusionStreamTraceState>>,
    finished: bool,
}

impl FusionStreamPromptLog {
    fn new(
        prompt_trace: SharedProviderPromptTrace,
        call_index: usize,
        started: Instant,
    ) -> Self {
        let state = fusion_prompt_trace(&prompt_trace)
            .map(|trace| trace.register_stream(call_index))
            .unwrap_or_default();
        Self {
            prompt_trace,
            call_index,
            started,
            state,
            finished: false,
        }
    }

    fn push(&mut self, chunk: &ChatStreamChunk) {
        if let Ok(chunk) = serde_json::to_value(chunk) {
            if let Ok(mut state) = self.state.lock() {
                state.event_count = state.event_count.saturating_add(1);
                state.last_chunk = Some(chunk);
            }
        }
    }

    fn finish(&mut self, status: &str, error: Option<&GatewayError>) {
        if self.finished {
            return;
        }
        let response = self
            .state
            .lock()
            .ok()
            .map(|state| state.response())
            .unwrap_or_else(|| serde_json::json!({"stream": true}));
        let mut update = serde_json::json!({
            "status": status,
            "duration_ms": elapsed_ms(self.started),
            "response": response,
        });
        if let Some(error) = error {
            update["error"] = fusion_call_error(error);
        }
        if let Some(prompt_trace) = fusion_prompt_trace(&self.prompt_trace) {
            prompt_trace.update_call(self.call_index, update);
        }
        self.finished = true;
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
    prompt_log: Option<FusionStreamPromptLog>,
}

impl GuardedFusionStream {
    fn new(
        inner: ChatStream,
        flow_guard: Option<FlowControlGuard>,
        inflight: InFlightGuard,
        cost_rate: ModelCostRate,
        billing: ProviderBilling,
        prompt_log: Option<FusionStreamPromptLog>,
    ) -> Self {
        Self {
            inner,
            flow_guard,
            inflight: Some(inflight),
            cost_rate,
            usage: Usage::default(),
            reported_cost: ProviderCost::default(),
            billing,
            prompt_log,
        }
    }
}

impl Stream for GuardedFusionStream {
    type Item = Result<ChatStreamChunk, GatewayError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let result = this.inner.as_mut().poll_next(context);
        match &result {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(prompt_log) = &mut this.prompt_log {
                    prompt_log.push(chunk);
                }
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
            Poll::Ready(Some(Err(error))) => {
                if let Some(prompt_log) = &mut this.prompt_log {
                    prompt_log.finish("failed", Some(error));
                }
            }
            Poll::Ready(None) => {
                if let Some(prompt_log) = &mut this.prompt_log {
                    prompt_log.finish("succeeded", None);
                }
            }
            Poll::Pending => {}
        }
        if matches!(result, Poll::Ready(None)) {
            this.flow_guard.take();
            this.inflight.take();
        }
        result
    }
}

impl Drop for GuardedFusionStream {
    fn drop(&mut self) {
        // finish() is idempotent, so completed and failed streams keep their
        // terminal status while an abandoned stream becomes cancelled.
        if let Some(prompt_log) = &mut self.prompt_log {
            prompt_log.finish("cancelled", None);
        }
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn fusion_call_error(error: &GatewayError) -> serde_json::Value {
    let mut value = serde_json::json!({
        "type": error.error_type(),
        "code": error.status_code(),
        "message": error.to_string(),
    });
    if let GatewayError::UpstreamError { status, .. } = error {
        value["upstream_status"] = serde_json::json!(status);
    }
    value
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
        build_gateway_headers, register_fusion_providers, request_prefix_bytes,
        FusionPromptTrace, FusionRuntime, FusionStreamPromptLog,
    };
    use crate::{
        AliasStore, DeploymentStore, InFlightTracker, ModelCostRate, RequestRateTracker, Router,
        SchedulePolicy,
    };
    use arc_swap::ArcSwap;
    use async_trait::async_trait;
    use boom_config::Config;
    use boom_core::provider::{
        Provider, ProviderBilling, ProviderCallContext, ProviderPromptTrace, ProviderProtocol,
        SharedProviderPromptTrace,
    };
    use boom_core::types::{
        ChatCompletionRequest, ChatCompletionResponse, ChatStream, ChatStreamChunk, Choice, Message,
        MessageContent, MessageRole, StreamChoice, StreamDelta, StreamUsage, Usage,
    };
    use boom_core::GatewayError;
    use boom_flowcontrol::FlowController;
    use boom_fusion::{ModelInvoker, WorkflowRole};
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
        protocol: ProviderProtocol,
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

        fn protocol(&self) -> ProviderProtocol {
            self.protocol
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

    #[test]
    fn finalize_only_cancels_unfinished_prompt_calls() {
        let trace = FusionPromptTrace::default();
        let succeeded = trace.start_call(json!({"status": "started"})).unwrap();
        let failed = trace.start_call(json!({"status": "started"})).unwrap();
        let unfinished = trace.start_call(json!({"status": "started"})).unwrap();
        trace.update_call(succeeded, json!({"status": "succeeded"}));
        trace.update_call(failed, json!({"status": "failed"}));

        trace.finalize();

        let snapshot = trace.snapshot().unwrap();
        let calls = snapshot["calls"].as_array().unwrap();
        assert_eq!(calls[succeeded]["status"], "succeeded");
        assert_eq!(calls[failed]["status"], "failed");
        assert_eq!(calls[unfinished]["status"], "cancelled");
    }

    #[test]
    fn stream_prompt_log_updates_shared_trace_only_when_finished() {
        let trace = Arc::new(FusionPromptTrace::default());
        let call_index = trace
            .start_call(json!({"role": "aggregator", "stream": true}))
            .unwrap();
        let shared_trace: SharedProviderPromptTrace = trace.clone();
        let mut prompt_log =
            FusionStreamPromptLog::new(shared_trace, call_index, std::time::Instant::now());

        for index in 0..2048 {
            prompt_log.push(&ChatStreamChunk {
                id: "chatcmpl-buffered-trace".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1,
                model: "aggregator".to_string(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: StreamDelta {
                        role: None,
                        content: Some(index.to_string()),
                        tool_calls: None,
                        reasoning_content: None,
                    },
                    finish_reason: None,
                }],
                usage: None,
                raw_data: None,
            });
        }

        let before_finish = trace.snapshot().unwrap();
        assert!(before_finish["calls"][call_index].get("response").is_none());

        prompt_log.finish("succeeded", None);
        trace.finalize();

        let after_finish = trace.snapshot().unwrap();
        let call = &after_finish["calls"][call_index];
        assert_eq!(call["status"], "succeeded");
        assert_eq!(call["response"]["event_count"], 2048);
        assert_eq!(
            call["response"]["last_chunk"]["choices"][0]["delta"]["content"],
            "2047"
        );
    }

    #[test]
    fn fusion_registration_checks_alias_target_and_every_deployment_protocol() {
        let config: Config = serde_yaml::from_str(
            r#"
model_list:
  - model_name: panel-target
    litellm_params:
      model: openai/panel
  - model_name: aggregator
    litellm_params:
      model: openai/aggregator
router_settings:
  model_group_alias:
    panel-alias: panel-target
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: panel-alias
          - model: panel-alias
        aggregator:
          model: aggregator
"#,
        )
        .unwrap();
        config.validate().unwrap();

        let deployment_store = Arc::new(DeploymentStore::new());
        let compatible: Arc<dyn Provider> = Arc::new(FakeProvider {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_models: Arc::new(Mutex::new(HashSet::new())),
            invalid_models: Arc::new(Mutex::new(HashSet::new())),
            models: vec!["panel-target".to_string(), "aggregator".to_string()],
            protocol: ProviderProtocol::OpenAiCompatible,
        });
        let incompatible: Arc<dyn Provider> = Arc::new(FakeProvider {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_models: Arc::new(Mutex::new(HashSet::new())),
            invalid_models: Arc::new(Mutex::new(HashSet::new())),
            models: vec!["panel-target".to_string()],
            protocol: ProviderProtocol::Native,
        });
        deployment_store.add_deployment("panel-target", compatible.clone());
        deployment_store.add_deployment("panel-target", incompatible);
        deployment_store.add_deployment("aggregator", compatible);

        let alias_store = Arc::new(AliasStore::new());
        alias_store.set_alias(
            "panel-alias".to_string(),
            "panel-target".to_string(),
            false,
        );
        let router = Arc::new(Router::new(
            deployment_store.clone(),
            alias_store.clone(),
            Arc::new(RecordingPolicy {
                key_hashes: Arc::new(Mutex::new(Vec::new())),
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

        let error = register_fusion_providers(
            &config.workflow_settings,
            &deployment_store,
            &alias_store,
            runtime,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("panel model 'panel-alias'"));
        assert!(error.contains("provider 'fake'"));
        assert!(error.contains("OpenAI-compatible provider"));
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
            protocol: ProviderProtocol::OpenAiCompatible,
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
        let prompt_trace = fusion.create_prompt_trace().unwrap();
        let result = fusion
            .chat_with_context(
                request,
                ProviderCallContext {
                    key_hash: "parent-key".to_string(),
                    key_alias: Some("parent-alias".to_string()),
                    is_vip: true,
                    api_path: "/v1/chat/completions".to_string(),
                    billing: billing.clone(),
                    prompt_trace: Some(prompt_trace.clone()),
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
        let prompt_fusion = prompt_trace.snapshot().unwrap();
        let prompt_calls = prompt_fusion["calls"].as_array().unwrap();
        assert_eq!(prompt_calls.len(), 3);
        assert_eq!(prompt_calls[0]["role"], "panel");
        assert_eq!(prompt_calls[1]["role"], "panel");
        assert_eq!(prompt_calls[2]["role"], "aggregator");
        assert!(prompt_calls
            .iter()
            .all(|call| call["status"] == "succeeded"));
        assert!(prompt_calls
            .iter()
            .all(|call| call.get("request").is_some()));
        assert!(prompt_calls
            .iter()
            .all(|call| call.get("response").is_some()));

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
        let stream_prompt_trace = fusion.create_prompt_trace().unwrap();
        let calls_before_stream = calls.lock().unwrap().len();
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
                    prompt_trace: Some(stream_prompt_trace.clone()),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            calls.lock().unwrap().len(),
            calls_before_stream,
            "stream workflow must remain lazy until the response body is polled"
        );
        let chunks = stream.collect::<Vec<_>>().await;
        assert!(chunks.iter().all(Result::is_ok));
        assert_eq!(calls.lock().unwrap().len(), calls_before_stream + 3);
        assert_eq!(stream_billing.actual_usage().unwrap().total_tokens, 9);
        assert_eq!(stream_billing.actual_cost().unwrap().total(), 72.into());
        let stream_prompt_fusion = stream_prompt_trace.snapshot().unwrap();
        let stream_prompt_calls = stream_prompt_fusion["calls"].as_array().unwrap();
        assert_eq!(stream_prompt_calls.len(), 3);
        assert_eq!(stream_prompt_calls[2]["role"], "aggregator");
        assert_eq!(stream_prompt_calls[2]["status"], "succeeded");
        assert_eq!(stream_prompt_calls[2]["stream"], true);
        assert!(stream_prompt_calls[2]["response"]["event_count"]
            .as_u64()
            .is_some_and(|count| count > 0));

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
                    prompt_trace: None,
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
                    prompt_trace: None,
                },
            )
            .await;
        assert!(invalid_result.is_err());
        assert_eq!(invalid_billing.actual_usage().unwrap().total_tokens, 12);
        assert_eq!(invalid_billing.actual_cost().unwrap().total(), 72.into());

        alias_store.set_alias(
            "recursive-panel-alias".to_string(),
            "fusion".to_string(),
            false,
        );
        let recursive_billing = ProviderBilling::default();
        let recursive_prompt_trace = fusion.create_prompt_trace().unwrap();
        let recursive_invoker = super::RoutingModelInvoker::new(
            runtime,
            ProviderCallContext {
                key_hash: "parent-key".to_string(),
                key_alias: Some("parent-alias".to_string()),
                is_vip: true,
                api_path: "/v1/chat/completions".to_string(),
                billing: recursive_billing,
                prompt_trace: Some(recursive_prompt_trace.clone()),
            },
        );
        let recursive_request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "recursive-panel-alias",
            "messages": [{"role": "user", "content": "do not recurse"}]
        }))
        .unwrap();
        let error = match recursive_invoker
            .invoke("direct_synthesis", WorkflowRole::Panel, recursive_request)
            .await
        {
            Ok(_) => panic!("fusion child alias must not resolve to FusionProvider"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("resolves to virtual provider"));
        let recursive_fusion = recursive_prompt_trace.snapshot().unwrap();
        let recursive_calls = recursive_fusion["calls"].as_array().unwrap();
        assert_eq!(recursive_calls.len(), 1);
        assert_eq!(recursive_calls[0]["status"], "failed");
        assert_eq!(recursive_calls[0]["role"], "panel");
        assert!(recursive_calls[0].get("provider").is_none());
        assert!(recursive_calls[0].get("request").is_none());
        assert!(recursive_calls[0]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("resolves to virtual provider")));
    }
}
