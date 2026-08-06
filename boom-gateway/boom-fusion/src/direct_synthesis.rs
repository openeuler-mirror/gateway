use crate::{
    ModelInvocation, ModelInvoker, Workflow, WorkflowContext, WorkflowExecution, WorkflowFailure,
    WorkflowRole, WorkflowStreamExecution,
};
use async_trait::async_trait;
use boom_core::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatStream, ChatStreamChunk, FunctionCallDelta,
    Message, MessageContent, MessageRole, PromptTokensDetails, StreamChoice, StreamDelta,
    StreamUsage, ToolCallDelta, Usage,
};
use boom_core::GatewayError;
use futures::{future::join_all, Stream};
use std::fmt::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

const SELF_MOA_AGGREGATOR_PROMPT: &str = include_str!("prompts/self_moa_aggregator.txt");
const DIRECT_SYNTHESIS_REFERENCE_CONTEXT_PROMPT: &str =
    include_str!("prompts/direct_synthesis_reference_context.txt");
const DEFAULT_OUTPUT_CONTRACT: &str = include_str!("prompts/output_contract.txt");
const PANEL_AGGREGATION_THRESHOLD: usize = 2;

#[derive(Debug, Clone)]
pub struct ModelInstance {
    pub model: String,
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct DirectSynthesisConfig {
    pub panel: Vec<ModelInstance>,
    pub aggregator: ModelInstance,
    pub panel_timeout: Option<Duration>,
}

pub struct DirectSynthesisWorkflow {
    id: String,
    config: DirectSynthesisConfig,
}

impl DirectSynthesisWorkflow {
    pub fn new(id: impl Into<String>, config: DirectSynthesisConfig) -> Result<Self, String> {
        let id = id.into();
        if id.is_empty() {
            return Err("workflow id must not be empty".to_string());
        }
        if config.panel.len() < PANEL_AGGREGATION_THRESHOLD {
            return Err("direct_synthesis requires at least two panel instances".to_string());
        }
        if config
            .panel
            .iter()
            .any(|instance| instance.model.is_empty())
        {
            return Err("direct_synthesis panel model must not be empty".to_string());
        }
        if config.aggregator.model.is_empty() {
            return Err("direct_synthesis aggregator model must not be empty".to_string());
        }
        Ok(Self { id, config })
    }

    fn validate_request(&self, request: &ChatCompletionRequest) -> Result<(), WorkflowFailure> {
        if request.n.unwrap_or(1) != 1 {
            return Err(WorkflowFailure {
                error: GatewayError::UnsupportedMode(
                    "fusion direct_synthesis supports only n=1".to_string(),
                ),
            });
        }
        Ok(())
    }

    fn panel_request(
        &self,
        original: &ChatCompletionRequest,
        instance: &ModelInstance,
    ) -> ChatCompletionRequest {
        let mut request = original.clone();
        request.model = instance.model.clone();
        request.temperature = instance.temperature;
        request.stream = Some(false);
        request.tools = non_empty_tools(original);
        if request.tools.is_none() {
            request.tool_choice = None;
        }
        request
    }

    fn aggregator_request(
        &self,
        original: &ChatCompletionRequest,
        panel_results: &[ModelInvocation],
        stream: bool,
    ) -> ChatCompletionRequest {
        let question = last_user_question(&original.messages);
        let has_tools = request_has_tools(original);
        let answers_text = if has_tools {
            answers_text_with_tool_calls(panel_results)
        } else {
            answers_text(panel_results)
        };
        let template = if has_tools {
            prompt_text(DIRECT_SYNTHESIS_REFERENCE_CONTEXT_PROMPT)
        } else {
            prompt_text(SELF_MOA_AGGREGATOR_PROMPT)
        };
        let prompt = template
            .replace("{question}", &question)
            .replace("{answers_text}", &answers_text);
        let prompt = format!(
            "{}\n{}",
            prompt.trim_end(),
            prompt_text(DEFAULT_OUTPUT_CONTRACT).trim()
        );

        let mut request = original.clone();
        request.model = self.config.aggregator.model.clone();
        if let Some(temperature) = self.config.aggregator.temperature {
            request.temperature = Some(temperature);
        }
        request.stream = Some(stream);
        request
            .messages
            .push(text_message(MessageRole::User, prompt));
        request.tools = non_empty_tools(original);
        if request.tools.is_none() {
            request.tool_choice = None;
        }
        request
    }

    async fn run_panel_round(
        &self,
        request: &ChatCompletionRequest,
        invoker: &Arc<dyn ModelInvoker>,
        attempt: u8,
    ) -> PanelRound {
        let panel_futures = self
            .config
            .panel
            .iter()
            .enumerate()
            .map(|(index, instance)| {
                let model = instance.model.clone();
                let call = invoker.invoke(
                    &self.id,
                    WorkflowRole::Panel,
                    self.panel_request(request, instance),
                );
                async move {
                    let result = match self.config.panel_timeout {
                        Some(timeout) => match tokio::time::timeout(timeout, call).await {
                            Ok(result) => result,
                            Err(_) => Err(GatewayError::ProviderError(format!(
                                "panel call timed out after {} seconds",
                                timeout.as_secs()
                            ))),
                        },
                        None => call.await,
                    };
                    (index, model, result)
                }
            });

        let mut round = PanelRound::default();
        for (index, model, result) in join_all(panel_futures).await {
            match result {
                Ok(invocation) => {
                    add_usage(&mut round.usage, &invocation.response.usage);
                    if valid_panel(&invocation.response) {
                        round.valid.push(PanelSuccess { index, invocation });
                    } else {
                        round.failures.push(FusionFailure::invalid_panel(
                            index,
                            model,
                            attempt,
                            invalid_panel_reason(&invocation.response),
                        ));
                    }
                }
                Err(error) => round.failures.push(FusionFailure::from_error(
                    WorkflowRole::Panel,
                    Some(index),
                    model,
                    Some(attempt),
                    &error,
                )),
            }
        }
        round
    }

    async fn prepare(
        &self,
        context: WorkflowContext,
    ) -> Result<PreparedSynthesis, WorkflowFailure> {
        let WorkflowContext { request, invoker } = context;
        self.validate_request(&request)?;

        let first_round = self.run_panel_round(&request, &invoker, 1).await;
        let PanelRound {
            valid: first_valid,
            mut failures,
            mut usage,
        } = first_round;

        let (mut valid_panels, attempts) = if first_valid.is_empty() {
            let retry_round = self.run_panel_round(&request, &invoker, 2).await;
            failures.extend(retry_round.failures);
            add_usage(&mut usage, &retry_round.usage);
            (retry_round.valid, 2)
        } else {
            (first_valid, 1)
        };

        if valid_panels.is_empty() {
            return Err(workflow_failure(
                &self.id,
                "panel",
                attempts,
                "no valid panel answers after retry",
                &failures,
            ));
        }

        valid_panels.sort_by_key(|panel| panel.index);
        let mut valid_panels = valid_panels
            .into_iter()
            .map(|panel| panel.invocation)
            .collect::<Vec<_>>();
        let panel_outcome = if valid_panels.len() >= PANEL_AGGREGATION_THRESHOLD {
            PanelOutcome::Aggregate(valid_panels)
        } else if request_has_tools(&request) {
            PanelOutcome::Single(valid_panels.pop().expect("one valid panel"))
        } else {
            return Err(workflow_failure(
                &self.id,
                "panel",
                attempts,
                "only 1 valid panel answer; at least 2 are required when the request has no tools",
                &failures,
            ));
        };

        Ok(PreparedSynthesis {
            request,
            invoker,
            panel_outcome,
            usage,
        })
    }

    async fn execute_inner(
        &self,
        context: WorkflowContext,
    ) -> Result<WorkflowExecution, WorkflowFailure> {
        let PreparedSynthesis {
            request,
            invoker,
            panel_outcome,
            mut usage,
        } = self.prepare(context).await?;
        let valid_panels = match panel_outcome {
            PanelOutcome::Single(invocation) => {
                let mut response = invocation.response;
                response.usage = usage;
                return Ok(WorkflowExecution { response });
            }
            PanelOutcome::Aggregate(valid_panels) => valid_panels,
        };

        let aggregator_request = self.aggregator_request(&request, &valid_panels, false);
        let mut response = match invoker
            .invoke(&self.id, WorkflowRole::Aggregator, aggregator_request)
            .await
        {
            Ok(invocation) => {
                add_usage(&mut usage, &invocation.response.usage);
                invocation.response
            }
            Err(_) => valid_panels[0].response.clone(),
        };
        response.usage = usage;
        Ok(WorkflowExecution { response })
    }

    async fn execute_stream_inner(
        &self,
        context: WorkflowContext,
    ) -> Result<WorkflowStreamExecution, WorkflowFailure> {
        let PreparedSynthesis {
            request,
            invoker,
            panel_outcome,
            usage: panel_usage,
        } = self.prepare(context).await?;
        let valid_panels = match panel_outcome {
            PanelOutcome::Single(invocation) => {
                return Ok(WorkflowStreamExecution {
                    stream: response_stream(&invocation.response, &panel_usage),
                });
            }
            PanelOutcome::Aggregate(valid_panels) => valid_panels,
        };

        let request = self.aggregator_request(&request, &valid_panels, true);
        match invoker
            .invoke_stream(&self.id, WorkflowRole::Aggregator, request)
            .await
        {
            Ok(invocation) => Ok(WorkflowStreamExecution {
                stream: Box::pin(AggregateUsageStream {
                    inner: invocation.stream,
                    panel_usage,
                    aggregator_usage: Usage::default(),
                    workflow_id: self.id.clone(),
                    aggregator_model: self.config.aggregator.model.clone(),
                }),
            }),
            Err(_) => Ok(WorkflowStreamExecution {
                stream: response_stream(&valid_panels[0].response, &panel_usage),
            }),
        }
    }
}

#[async_trait]
impl Workflow for DirectSynthesisWorkflow {
    fn id(&self) -> &str {
        &self.id
    }

    async fn execute(
        &self,
        context: WorkflowContext,
    ) -> Result<WorkflowExecution, WorkflowFailure> {
        self.execute_inner(context).await
    }

    async fn execute_stream(
        &self,
        context: WorkflowContext,
    ) -> Result<WorkflowStreamExecution, WorkflowFailure> {
        self.execute_stream_inner(context).await
    }
}

struct PreparedSynthesis {
    request: ChatCompletionRequest,
    invoker: Arc<dyn ModelInvoker>,
    panel_outcome: PanelOutcome,
    usage: Usage,
}

#[derive(Default)]
struct PanelRound {
    valid: Vec<PanelSuccess>,
    failures: Vec<FusionFailure>,
    usage: Usage,
}

struct PanelSuccess {
    index: usize,
    invocation: ModelInvocation,
}

enum PanelOutcome {
    Aggregate(Vec<ModelInvocation>),
    Single(ModelInvocation),
}

#[derive(Debug)]
struct FusionFailure {
    role: WorkflowRole,
    panel_index: Option<usize>,
    model: String,
    attempt: Option<u8>,
    error_type: String,
    upstream_status: Option<u16>,
    message: String,
}

impl FusionFailure {
    fn invalid_panel(index: usize, model: String, attempt: u8, message: String) -> Self {
        Self {
            role: WorkflowRole::Panel,
            panel_index: Some(index),
            model,
            attempt: Some(attempt),
            error_type: "invalid_response".to_string(),
            upstream_status: None,
            message,
        }
    }

    fn from_error(
        role: WorkflowRole,
        panel_index: Option<usize>,
        model: String,
        attempt: Option<u8>,
        error: &GatewayError,
    ) -> Self {
        let upstream_status = match error {
            GatewayError::UpstreamError { status, .. } => Some(*status),
            _ => None,
        };
        Self {
            role,
            panel_index,
            model,
            attempt,
            error_type: error.error_type().to_string(),
            upstream_status,
            message: error.to_string(),
        }
    }
}

fn workflow_failure(
    workflow_id: &str,
    stage: &str,
    attempts: u8,
    summary: &str,
    failures: &[FusionFailure],
) -> WorkflowFailure {
    let mut message = format!(
        "fusion '{}' direct_synthesis failed at {} stage after {} attempt(s): {}",
        workflow_id, stage, attempts, summary
    );
    if !failures.is_empty() {
        message.push_str("; failures: ");
        for (index, failure) in failures.iter().enumerate() {
            if index > 0 {
                message.push_str(" | ");
            }
            let _ = write!(
                message,
                "role={}{} model='{}'{} type={}",
                failure.role.as_str(),
                failure
                    .panel_index
                    .map(|index| format!("[{index}]"))
                    .unwrap_or_default(),
                failure.model,
                failure
                    .attempt
                    .map(|attempt| format!(" attempt={attempt}"))
                    .unwrap_or_default(),
                failure.error_type,
            );
            if let Some(status) = failure.upstream_status {
                let _ = write!(message, " upstream_status={status}");
            }
            let _ = write!(message, " message={}", failure.message);
        }
    }
    WorkflowFailure {
        error: GatewayError::ProviderError(message),
    }
}

fn prompt_text(value: &'static str) -> &'static str {
    value.strip_suffix('\n').unwrap_or(value)
}

fn text_message(role: MessageRole, content: String) -> Message {
    Message {
        role,
        content: MessageContent::Text(content),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    }
}

fn non_empty_tools(request: &ChatCompletionRequest) -> Option<Vec<boom_core::types::Tool>> {
    request.tools.clone().filter(|tools| !tools.is_empty())
}

fn request_has_tools(request: &ChatCompletionRequest) -> bool {
    request
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
}

fn last_user_question(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| matches!(message.role, MessageRole::User))
        .map(|message| message_text(&message.content))
        .unwrap_or_default()
}

fn answers_text(panel_results: &[ModelInvocation]) -> String {
    panel_results
        .iter()
        .take(8)
        .enumerate()
        .map(|(index, result)| {
            let content = result
                .response
                .choices
                .first()
                .map(|choice| message_text(&choice.message.content))
                .unwrap_or_default();
            format!("回答{}：\n{}", index + 1, content)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn answers_text_with_tool_calls(panel_results: &[ModelInvocation]) -> String {
    panel_results
        .iter()
        .take(8)
        .enumerate()
        .map(|(index, result)| {
            let mut parts = Vec::new();
            if let Some(choice) = result.response.choices.first() {
                let content = message_text(&choice.message.content);
                if !content.trim().is_empty() {
                    parts.push(content.trim().to_string());
                }
                if let Some(tool_calls) = &choice.message.tool_calls {
                    parts.extend(tool_calls.iter().map(|call| {
                        format!(
                            "候选 tool_call: {}({})",
                            call.function.name, call.function.arguments
                        )
                    }));
                }
            }
            format!("回答{}：\n{}", index + 1, parts.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn valid_panel(response: &ChatCompletionResponse) -> bool {
    let Some(choice) = response.choices.first() else {
        return false;
    };
    if choice
        .message
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty())
    {
        return true;
    }
    !message_text(&choice.message.content).trim().is_empty()
}

fn invalid_panel_reason(response: &ChatCompletionResponse) -> String {
    match response.choices.first() {
        None => "panel response has no choices".to_string(),
        Some(choice)
            if choice.message.tool_calls.as_ref().is_none_or(Vec::is_empty)
                && message_text(&choice.message.content).trim().is_empty() =>
        {
            "panel response has neither content nor tool calls".to_string()
        }
        Some(_) => "panel response is invalid".to_string(),
    }
}

fn message_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                boom_core::types::ContentPart::Text { text } => Some(text.as_str()),
                boom_core::types::ContentPart::Reasoning { reasoning } => Some(reasoning.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        MessageContent::Null => String::new(),
    }
}

fn add_usage(target: &mut Usage, usage: &Usage) {
    target.prompt_tokens = target.prompt_tokens.saturating_add(usage.prompt_tokens);
    target.completion_tokens = target
        .completion_tokens
        .saturating_add(usage.completion_tokens);
    target.total_tokens = target.total_tokens.saturating_add(usage.total_tokens);
    add_optional(
        &mut target.cache_creation_input_tokens,
        usage.cache_creation_input_tokens,
    );
    add_optional(
        &mut target.cache_read_input_tokens,
        usage.cache_read_input_tokens,
    );
    if let Some(details) = &usage.prompt_tokens_details {
        let target_details = target
            .prompt_tokens_details
            .get_or_insert_with(PromptTokensDetails::default);
        add_optional(&mut target_details.cached_tokens, details.cached_tokens);
    }
}

fn add_optional(target: &mut Option<u32>, value: Option<u32>) {
    if let Some(value) = value {
        *target = Some(target.unwrap_or(0).saturating_add(value));
    }
}

struct AggregateUsageStream {
    inner: ChatStream,
    panel_usage: Usage,
    aggregator_usage: Usage,
    workflow_id: String,
    aggregator_model: String,
}

impl Stream for AggregateUsageStream {
    type Item = Result<ChatStreamChunk, GatewayError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(Ok(mut chunk))) => {
                if let Some(usage) = &mut chunk.usage {
                    update_usage_snapshot(&mut this.aggregator_usage, usage);
                    *usage = combined_stream_usage(&this.aggregator_usage, &this.panel_usage);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                Poll::Ready(Some(Err(GatewayError::ProviderError(format!(
                    "fusion '{}' aggregator model '{}' stream failed: {}",
                    this.workflow_id, this.aggregator_model, error
                )))))
            }
            result => result,
        }
    }
}

fn update_usage_snapshot(target: &mut Usage, usage: &StreamUsage) {
    if let Some(prompt_tokens) = usage.prompt_tokens {
        target.prompt_tokens = non_negative_value(prompt_tokens);
    }
    if let Some(completion_tokens) = usage.completion_tokens {
        target.completion_tokens = non_negative_value(completion_tokens);
    }
    target.total_tokens = usage.total_tokens.map_or_else(
        || {
            target
                .prompt_tokens
                .saturating_add(target.completion_tokens)
        },
        non_negative_value,
    );
    if let Some(cached_tokens) = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
    {
        target
            .prompt_tokens_details
            .get_or_insert_with(PromptTokensDetails::default)
            .cached_tokens = Some(cached_tokens);
    }
}

fn combined_stream_usage(aggregator: &Usage, panel: &Usage) -> StreamUsage {
    let prompt_tokens = u64::from(aggregator.prompt_tokens) + u64::from(panel.prompt_tokens);
    let completion_tokens =
        u64::from(aggregator.completion_tokens) + u64::from(panel.completion_tokens);
    let total_tokens = u64::from(aggregator.total_tokens) + u64::from(panel.total_tokens);
    let aggregator_cached = aggregator
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens);
    let panel_cached = panel
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens);
    let cached_tokens = match (aggregator_cached, panel_cached) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    };

    StreamUsage {
        prompt_tokens: Some(stream_token(prompt_tokens)),
        completion_tokens: Some(stream_token(completion_tokens)),
        total_tokens: Some(stream_token(total_tokens)),
        prompt_tokens_details: cached_tokens.map(|cached_tokens| PromptTokensDetails {
            cached_tokens: Some(cached_tokens),
        }),
    }
}

fn non_negative_value(value: i32) -> u32 {
    value.max(0) as u32
}

fn stream_token(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn response_stream(response: &ChatCompletionResponse, usage: &Usage) -> ChatStream {
    let content_chunk = ChatStreamChunk {
        id: response.id.clone(),
        object: "chat.completion.chunk".to_string(),
        created: response.created,
        model: response.model.clone(),
        choices: response
            .choices
            .iter()
            .map(|choice| StreamChoice {
                index: choice.index,
                delta: StreamDelta {
                    role: Some(MessageRole::Assistant),
                    content: Some(message_text(&choice.message.content)),
                    tool_calls: choice.message.tool_calls.as_ref().map(|calls| {
                        calls
                            .iter()
                            .enumerate()
                            .map(|(index, call)| ToolCallDelta {
                                index: index as u32,
                                id: Some(call.id.clone()),
                                call_type: Some(call.call_type.clone()),
                                function: Some(FunctionCallDelta {
                                    name: Some(call.function.name.clone()),
                                    arguments: Some(call.function.arguments.clone()),
                                }),
                            })
                            .collect()
                    }),
                    reasoning_content: choice.message.reasoning_content.clone(),
                },
                finish_reason: None,
            })
            .collect(),
        usage: None,
        raw_data: None,
    };
    let finish_chunk = ChatStreamChunk {
        id: response.id.clone(),
        object: "chat.completion.chunk".to_string(),
        created: response.created,
        model: response.model.clone(),
        choices: response
            .choices
            .iter()
            .map(|choice| StreamChoice {
                index: choice.index,
                delta: StreamDelta {
                    role: None,
                    content: None,
                    tool_calls: None,
                    reasoning_content: None,
                },
                finish_reason: choice.finish_reason.clone(),
            })
            .collect(),
        usage: Some(combined_stream_usage(&Usage::default(), usage)),
        raw_data: None,
    };
    Box::pin(futures::stream::iter([Ok(content_chunk), Ok(finish_chunk)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelStreamInvocation;
    use boom_core::types::{Choice, FunctionCall, Tool, ToolCall, ToolFunction};
    use futures::StreamExt;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Clone, Copy)]
    enum PanelBehavior {
        Valid,
        Invalid,
        Error,
        ValidOnRetry,
    }

    struct TestInvoker {
        calls: Mutex<Vec<(WorkflowRole, ChatCompletionRequest)>>,
        panels: Vec<PanelBehavior>,
        aggregator_error: bool,
        aggregator_stream_error: bool,
        aggregator_empty: bool,
    }

    impl TestInvoker {
        fn new(panels: Vec<PanelBehavior>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                panels,
                aggregator_error: false,
                aggregator_stream_error: false,
                aggregator_empty: false,
            }
        }

        fn panel_attempt(&self, model: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(role, request)| *role == WorkflowRole::Panel && request.model == model)
                .count()
        }
    }

    #[async_trait]
    impl ModelInvoker for TestInvoker {
        async fn invoke(
            &self,
            _workflow_id: &str,
            role: WorkflowRole,
            request: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            self.calls.lock().unwrap().push((role, request.clone()));
            if role == WorkflowRole::Aggregator {
                if self.aggregator_error {
                    return Err(GatewayError::ProviderError(
                        "aggregator unavailable".to_string(),
                    ));
                }
                return Ok(ModelInvocation {
                    response: response(
                        &request.model,
                        if self.aggregator_empty {
                            ""
                        } else {
                            "aggregated"
                        },
                        None,
                    ),
                });
            }

            let index = request
                .model
                .strip_prefix("panel-")
                .unwrap()
                .parse::<usize>()
                .unwrap();
            let attempt = self.panel_attempt(&request.model);
            match self.panels[index] {
                PanelBehavior::Error => Err(GatewayError::UpstreamError {
                    status: 503,
                    message: format!("{} unavailable", request.model),
                }),
                PanelBehavior::Invalid => Ok(ModelInvocation {
                    response: response(&request.model, "", None),
                }),
                PanelBehavior::ValidOnRetry if attempt == 1 => Err(GatewayError::ProviderError(
                    "temporary panel failure".to_string(),
                )),
                PanelBehavior::Valid | PanelBehavior::ValidOnRetry => Ok(ModelInvocation {
                    response: response(&request.model, "panel answer", None),
                }),
            }
        }

        async fn invoke_stream(
            &self,
            _workflow_id: &str,
            role: WorkflowRole,
            request: ChatCompletionRequest,
        ) -> Result<ModelStreamInvocation, GatewayError> {
            self.calls.lock().unwrap().push((role, request.clone()));
            if self.aggregator_error {
                return Err(GatewayError::ProviderError(
                    "aggregator unavailable".to_string(),
                ));
            }
            let item = if self.aggregator_stream_error {
                Err(GatewayError::UpstreamError {
                    status: 502,
                    message: "stream broke".to_string(),
                })
            } else {
                Ok(stream_chunk(&request.model, "aggregated"))
            };
            Ok(ModelStreamInvocation {
                stream: Box::pin(futures::stream::iter([item])),
            })
        }
    }

    fn workflow() -> DirectSynthesisWorkflow {
        DirectSynthesisWorkflow::new(
            "fusion-test",
            DirectSynthesisConfig {
                panel: vec![
                    ModelInstance {
                        model: "panel-0".to_string(),
                        temperature: Some(0.3),
                    },
                    ModelInstance {
                        model: "panel-1".to_string(),
                        temperature: Some(0.5),
                    },
                ],
                aggregator: ModelInstance {
                    model: "aggregator".to_string(),
                    temperature: Some(0.0),
                },
                panel_timeout: None,
            },
        )
        .unwrap()
    }

    fn request(with_tools: bool) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "fusion".to_string(),
            messages: vec![text_message(MessageRole::User, "fix it".to_string())],
            max_tokens: Some(128),
            max_completion_tokens: None,
            tools: with_tools.then(|| {
                vec![Tool {
                    tool_type: "function".to_string(),
                    function: ToolFunction {
                        name: "bash".to_string(),
                        description: None,
                        parameters: json!({"type": "object"}),
                    },
                }]
            }),
            tool_choice: with_tools.then(|| Value::String("auto".to_string())),
            response_format: None,
            temperature: Some(0.0),
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: None,
            n: None,
            stream: Some(false),
            logprobs: None,
            top_logprobs: None,
            logit_bias: None,
            user: None,
            extra: Default::default(),
            gateway_headers: HashMap::new(),
            kv_cache_report_full: false,
        }
    }

    fn response(
        model: &str,
        content: &str,
        tool_calls: Option<Vec<ToolCall>>,
    ) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: format!("chatcmpl-{model}"),
            object: "chat.completion".to_string(),
            created: 1,
            model: model.to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: MessageRole::Assistant,
                    content: MessageContent::Text(content.to_string()),
                    name: None,
                    tool_calls,
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
        }
    }

    fn stream_chunk(model: &str, content: &str) -> ChatStreamChunk {
        ChatStreamChunk {
            id: format!("chatcmpl-{model}"),
            object: "chat.completion.chunk".to_string(),
            created: 1,
            model: model.to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: StreamDelta {
                    role: Some(MessageRole::Assistant),
                    content: Some(content.to_string()),
                    tool_calls: None,
                    reasoning_content: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
            raw_data: None,
        }
    }

    #[tokio::test]
    async fn panels_and_aggregator_receive_non_empty_tools() {
        let invoker = Arc::new(TestInvoker::new(vec![
            PanelBehavior::Valid,
            PanelBehavior::Valid,
        ]));
        workflow()
            .execute(WorkflowContext {
                request: request(true),
                invoker: invoker.clone(),
            })
            .await
            .unwrap();

        let calls = invoker.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls
            .iter()
            .all(|(_, request)| { request.tools.as_ref().is_some_and(|tools| tools.len() == 1) }));
    }

    #[tokio::test]
    async fn empty_tools_are_omitted_from_child_requests() {
        let invoker = Arc::new(TestInvoker::new(vec![
            PanelBehavior::Valid,
            PanelBehavior::Valid,
        ]));
        let mut input = request(false);
        input.tools = Some(Vec::new());
        input.tool_choice = Some(Value::String("required".to_string()));
        workflow()
            .execute(WorkflowContext {
                request: input,
                invoker: invoker.clone(),
            })
            .await
            .unwrap();

        assert!(invoker
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, request)| { request.tools.is_none() && request.tool_choice.is_none() }));
    }

    #[tokio::test]
    async fn all_panels_retry_once_and_report_detailed_failures() {
        let invoker = Arc::new(TestInvoker::new(vec![
            PanelBehavior::Error,
            PanelBehavior::Invalid,
        ]));
        let result = workflow()
            .execute(WorkflowContext {
                request: request(false),
                invoker: invoker.clone(),
            })
            .await;
        let error = match result {
            Err(error) => error.to_string(),
            Ok(_) => panic!("all failed panels unexpectedly succeeded"),
        };

        assert_eq!(invoker.calls.lock().unwrap().len(), 4);
        assert!(error.contains("after 2 attempt(s)"));
        assert!(error.contains("panel[0]"));
        assert!(error.contains("upstream_status=503"));
        assert!(error.contains("panel[1]"));
        assert!(error.contains("invalid_response"));
    }

    #[tokio::test]
    async fn one_initial_panel_does_not_retry_and_requires_tools() {
        let invoker = Arc::new(TestInvoker::new(vec![
            PanelBehavior::Valid,
            PanelBehavior::Error,
        ]));
        let result = workflow()
            .execute(WorkflowContext {
                request: request(false),
                invoker: invoker.clone(),
            })
            .await;
        let error = match result {
            Err(error) => error.to_string(),
            Ok(_) => panic!("single panel without tools unexpectedly succeeded"),
        };

        assert_eq!(invoker.calls.lock().unwrap().len(), 2);
        assert!(error.contains("only 1 valid panel answer"));
    }

    #[tokio::test]
    async fn one_panel_with_tools_returns_without_requiring_a_tool_call() {
        let invoker = Arc::new(TestInvoker::new(vec![
            PanelBehavior::Valid,
            PanelBehavior::Error,
        ]));
        let result = workflow()
            .execute(WorkflowContext {
                request: request(true),
                invoker: invoker.clone(),
            })
            .await
            .unwrap();

        assert_eq!(result.response.model, "panel-0");
        assert_eq!(invoker.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn aggregator_failure_falls_back_to_first_panel() {
        let invoker = Arc::new(TestInvoker {
            aggregator_error: true,
            ..TestInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Valid])
        });
        let result = workflow()
            .execute(WorkflowContext {
                request: request(false),
                invoker,
            })
            .await
            .unwrap();

        assert_eq!(result.response.model, "panel-0");
    }

    #[tokio::test]
    async fn successful_aggregator_response_is_not_revalidated() {
        let invoker = Arc::new(TestInvoker {
            aggregator_empty: true,
            ..TestInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Valid])
        });
        let result = workflow()
            .execute(WorkflowContext {
                request: request(false),
                invoker,
            })
            .await
            .unwrap();

        assert_eq!(result.response.model, "aggregator");
        assert_eq!(
            message_text(&result.response.choices[0].message.content),
            ""
        );
    }

    #[tokio::test]
    async fn established_aggregator_stream_error_is_not_replaced_by_panel() {
        let invoker = Arc::new(TestInvoker {
            aggregator_stream_error: true,
            ..TestInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Valid])
        });
        let execution = workflow()
            .execute_stream(WorkflowContext {
                request: request(false),
                invoker,
            })
            .await
            .unwrap();
        let error = execution
            .stream
            .collect::<Vec<_>>()
            .await
            .pop()
            .unwrap()
            .unwrap_err();

        assert!(matches!(
            error,
            GatewayError::ProviderError(message)
                if message.contains("fusion-test")
                    && message.contains("aggregator")
                    && message.contains("stream broke")
        ));
    }

    #[tokio::test]
    async fn fusion_rejects_multiple_choices_before_child_calls() {
        let invoker = Arc::new(TestInvoker::new(vec![
            PanelBehavior::ValidOnRetry,
            PanelBehavior::ValidOnRetry,
        ]));
        let mut input = request(false);
        input.n = Some(2);
        let result = workflow()
            .execute(WorkflowContext {
                request: input,
                invoker: invoker.clone(),
            })
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("n=2 Fusion request unexpectedly succeeded"),
        };

        assert!(matches!(error.error, GatewayError::UnsupportedMode(_)));
        assert!(invoker.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn tool_calls_are_included_in_aggregator_reference_text() {
        let invocation = ModelInvocation {
            response: response(
                "panel-0",
                "",
                Some(vec![ToolCall {
                    id: "call-1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "bash".to_string(),
                        arguments: "{\"command\":\"pwd\"}".to_string(),
                    },
                }]),
            ),
        };

        let text = answers_text_with_tool_calls(&[invocation]);
        assert!(text.contains("bash"));
        assert!(text.contains("\"command\":\"pwd\""));
    }
}
