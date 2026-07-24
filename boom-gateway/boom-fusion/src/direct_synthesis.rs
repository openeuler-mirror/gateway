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
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

const SELF_MOA_AGGREGATOR_PROMPT: &str = include_str!("prompts/self_moa_aggregator.txt");
const DIRECT_SYNTHESIS_REFERENCE_CONTEXT_PROMPT: &str =
    include_str!("prompts/direct_synthesis_reference_context.txt");
const REFERENCE_ADVISOR_PROMPT: &str = include_str!("prompts/reference_advisor.txt");
const DEFAULT_OUTPUT_CONTRACT: &str = include_str!("prompts/output_contract.txt");

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
        if config.panel.len() < 2 {
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

    fn panel_request(
        &self,
        original: &ChatCompletionRequest,
        instance: &ModelInstance,
    ) -> ChatCompletionRequest {
        let mut request = original.clone();
        request.model = instance.model.clone();
        request.temperature = instance.temperature;
        request.stream = Some(false);

        let has_tools = original
            .tools
            .as_ref()
            .map_or(false, |tools| !tools.is_empty());
        if has_tools {
            request.messages.push(text_message(
                MessageRole::User,
                prompt_text(REFERENCE_ADVISOR_PROMPT).to_string(),
            ));
        }
        request.tools = Some(Vec::new());
        request.tool_choice = None;
        request
    }

    fn aggregator_request(
        &self,
        original: &ChatCompletionRequest,
        panel_results: &[ModelInvocation],
        stream: bool,
    ) -> ChatCompletionRequest {
        let question = last_user_question(&original.messages);
        let answers_text = answers_text(panel_results);
        let has_tools = original
            .tools
            .as_ref()
            .map_or(false, |tools| !tools.is_empty());
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
        let tools = original.tools.clone().unwrap_or_default();
        let tools_empty = tools.is_empty();
        request.tools = Some(tools);
        if tools_empty {
            request.tool_choice = None;
        }
        request
    }

    async fn prepare(&self, context: WorkflowContext) -> Result<PreparedSynthesis, WorkflowFailure> {
        let WorkflowContext { request, invoker } = context;
        let panel_futures = self
            .config
            .panel
            .iter()
            .map(|instance| {
                let call = invoker.invoke(
                    &self.id,
                    WorkflowRole::Panel,
                    self.panel_request(&request, instance),
                );
                async move {
                    match self.config.panel_timeout {
                        Some(timeout) => tokio::time::timeout(timeout, call).await.map_err(|_| {
                            GatewayError::ProviderError(format!(
                                "direct_synthesis panel call timed out after {} seconds",
                                timeout.as_secs()
                            ))
                        })?,
                        None => call.await,
                    }
                }
            });
        let panel_results = join_all(panel_futures).await;
        let mut valid_panels = Vec::with_capacity(self.config.panel.len());
        let mut usage = Usage::default();

        for result in panel_results {
            if let Ok(invocation) = result {
                add_usage(&mut usage, &invocation.response.usage);
                if valid_panel(&invocation.response) {
                    valid_panels.push(invocation);
                }
            }
        }

        if valid_panels.is_empty() {
            return Err(WorkflowFailure {
                error: GatewayError::ProviderError(
                    "direct_synthesis produced no panel answers: ALL_PANELS_FAILED".to_string(),
                ),
            });
        }

        Ok(PreparedSynthesis {
            request,
            invoker,
            valid_panels,
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
            valid_panels,
            mut usage,
        } = self.prepare(context).await?;
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
    ) -> Result<crate::WorkflowStreamExecution, WorkflowFailure> {
        self.execute_stream_inner(context).await
    }
}

struct PreparedSynthesis {
    request: ChatCompletionRequest,
    invoker: Arc<dyn ModelInvoker>,
    valid_panels: Vec<ModelInvocation>,
    usage: Usage,
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

fn valid_panel(response: &ChatCompletionResponse) -> bool {
    let Some(choice) = response.choices.first() else {
        return false;
    };
    if choice
        .message
        .tool_calls
        .as_ref()
        .map_or(false, |calls| !calls.is_empty())
    {
        return true;
    }
    let content = message_text(&choice.message.content);
    !content.trim().is_empty()
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

impl DirectSynthesisWorkflow {
    async fn execute_stream_inner(
        &self,
        context: WorkflowContext,
    ) -> Result<WorkflowStreamExecution, WorkflowFailure> {
        let PreparedSynthesis {
            request,
            invoker,
            valid_panels,
            usage: panel_usage,
        } = self.prepare(context).await?;
        let request = self.aggregator_request(&request, &valid_panels, true);
        match invoker
            .invoke_stream(&self.id, WorkflowRole::Aggregator, request)
            .await
        {
            Ok(invocation) => {
                let stream = AggregateUsageStream {
                    inner: invocation.stream,
                    panel_usage,
                    aggregator_usage: Usage::default(),
                };
                Ok(WorkflowStreamExecution {
                    stream: Box::pin(stream),
                })
            }
            Err(_) => Ok(WorkflowStreamExecution {
                stream: response_stream(&valid_panels[0].response, &panel_usage),
            }),
        }
    }
}

struct AggregateUsageStream {
    inner: ChatStream,
    panel_usage: Usage,
    aggregator_usage: Usage,
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
    use boom_core::types::{
        ChatStreamChunk, Choice, StreamChoice, StreamDelta, StreamUsage, Tool, ToolCall,
        ToolFunction,
    };
    use futures::StreamExt;
    use serde_json::{json, Map, Value};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeInvoker {
        calls: Mutex<Vec<(WorkflowRole, ChatCompletionRequest)>>,
    }

    struct ScenarioInvoker {
        calls: Mutex<Vec<(WorkflowRole, ChatCompletionRequest)>>,
        fail_all_panels: bool,
        fail_first_panel: bool,
        fail_aggregator: bool,
        delay_first_panel: bool,
    }

    #[async_trait]
    impl ModelInvoker for FakeInvoker {
        async fn invoke(
            &self,
            _workflow_id: &str,
            role: WorkflowRole,
            request: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            self.calls.lock().unwrap().push((role, request.clone()));
            let index = self.calls.lock().unwrap().len();
            let (content, tool_calls, finish_reason) = if role == WorkflowRole::Aggregator {
                (
                    String::new(),
                    Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        call_type: "function".to_string(),
                        function: boom_core::types::FunctionCall {
                            name: "bash".to_string(),
                            arguments: "{\"command\":\"pwd\"}".to_string(),
                        },
                    }]),
                    Some("tool_calls".to_string()),
                )
            } else {
                (format!("panel {}", index), None, Some("stop".to_string()))
            };
            Ok(ModelInvocation {
                response: response(&request.model, content, tool_calls, finish_reason),
            })
        }
    }

    #[async_trait]
    impl ModelInvoker for ScenarioInvoker {
        async fn invoke(
            &self,
            _workflow_id: &str,
            role: WorkflowRole,
            request: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            self.calls.lock().unwrap().push((role, request.clone()));
            if role == WorkflowRole::Aggregator && self.fail_aggregator {
                return Err(GatewayError::ProviderError(
                    "aggregator unavailable".to_string(),
                ));
            }
            if role == WorkflowRole::Panel {
                let is_first = request.temperature == Some(0.3);
                if self.fail_all_panels || (self.fail_first_panel && is_first) {
                    return Err(GatewayError::ProviderError("panel unavailable".to_string()));
                }
                if self.delay_first_panel && is_first {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                let content = if is_first {
                    "configured first"
                } else {
                    "configured second"
                };
                return Ok(invocation(content));
            }
            Ok(invocation("aggregated"))
        }

        async fn invoke_stream(
            &self,
            _workflow_id: &str,
            role: WorkflowRole,
            request: ChatCompletionRequest,
        ) -> Result<ModelStreamInvocation, GatewayError> {
            self.calls.lock().unwrap().push((role, request.clone()));
            if self.fail_aggregator {
                return Err(GatewayError::ProviderError(
                    "aggregator unavailable".to_string(),
                ));
            }
            Ok(ModelStreamInvocation {
                stream: Box::pin(futures::stream::iter([Ok(ChatStreamChunk {
                    id: "chatcmpl-stream".to_string(),
                    object: "chat.completion.chunk".to_string(),
                    created: 1,
                    model: request.model.clone(),
                    choices: vec![StreamChoice {
                        index: 0,
                        delta: StreamDelta {
                            role: Some(MessageRole::Assistant),
                            content: Some("aggregated".to_string()),
                            tool_calls: None,
                            reasoning_content: None,
                        },
                        finish_reason: Some("stop".to_string()),
                    }],
                    usage: Some(StreamUsage {
                        prompt_tokens: Some(5),
                        completion_tokens: Some(2),
                        total_tokens: Some(7),
                        prompt_tokens_details: None,
                    }),
                    raw_data: None,
                })])),
            })
        }
    }

    fn request() -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "fusion".to_string(),
            messages: vec![text_message(MessageRole::User, "fix it".to_string())],
            max_tokens: Some(128),
            max_completion_tokens: None,
            tools: Some(vec![Tool {
                tool_type: "function".to_string(),
                function: ToolFunction {
                    name: "bash".to_string(),
                    description: None,
                    parameters: json!({"type": "object"}),
                },
            }]),
            tool_choice: Some(Value::String("required".to_string())),
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
            extra: Map::from_iter([(
                "metadata".to_string(),
                json!({
                    "benchmark": "swebench",
                    "instance_id": "astropy__astropy-13236",
                }),
            )]),
            gateway_headers: HashMap::new(),
            kv_cache_report_full: false,
        }
    }

    fn workflow() -> DirectSynthesisWorkflow {
        DirectSynthesisWorkflow::new(
            "direct_synthesis",
            DirectSynthesisConfig {
                panel: vec![
                    ModelInstance {
                        model: "glm-5.2".to_string(),
                        temperature: Some(0.3),
                    },
                    ModelInstance {
                        model: "glm-5.2".to_string(),
                        temperature: Some(0.5),
                    },
                ],
                aggregator: ModelInstance {
                    model: "glm-5.2".to_string(),
                    temperature: None,
                },
                panel_timeout: None,
            },
        )
        .unwrap()
    }

    fn fixture(value: &str) -> ChatCompletionRequest {
        serde_json::from_str(value).unwrap()
    }

    fn invocation(content: &str) -> ModelInvocation {
        ModelInvocation {
            response: response(
                "glm-5.2",
                content.to_string(),
                None,
                Some("stop".to_string()),
            ),
        }
    }

    fn sha256(value: &str) -> String {
        hex::encode(Sha256::digest(value.as_bytes()))
    }

    fn response(
        model: &str,
        content: String,
        tool_calls: Option<Vec<ToolCall>>,
        finish_reason: Option<String>,
    ) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: format!("chatcmpl-{}", model),
            object: "chat.completion".to_string(),
            created: 1,
            model: model.to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: MessageRole::Assistant,
                    content: MessageContent::Text(content),
                    name: None,
                    tool_calls,
                    tool_call_id: None,
                    reasoning_content: None,
                },
                finish_reason,
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

    #[tokio::test]
    async fn direct_synthesis_preserves_tool_contract_and_order() {
        let invoker = Arc::new(FakeInvoker {
            calls: Mutex::new(Vec::new()),
        });
        let workflow = workflow();

        let result = workflow
            .execute(WorkflowContext {
                request: request(),
                invoker: invoker.clone(),
            })
            .await
            .unwrap();

        let calls = invoker.calls.lock().unwrap();
        let panel_calls: Vec<_> = calls
            .iter()
            .filter(|(role, _)| *role == WorkflowRole::Panel)
            .collect();
        assert_eq!(panel_calls.len(), 2);
        assert_eq!(panel_calls[0].1.temperature, Some(0.3));
        assert_eq!(panel_calls[1].1.temperature, Some(0.5));
        assert!(panel_calls[0].1.tools.as_ref().is_some_and(Vec::is_empty));
        assert!(panel_calls[0].1.tool_choice.is_none());

        let aggregator = calls
            .iter()
            .find(|(role, _)| *role == WorkflowRole::Aggregator)
            .unwrap();
        assert_eq!(aggregator.1.temperature, Some(0.0));
        assert_eq!(
            aggregator.1.tool_choice,
            Some(Value::String("required".to_string()))
        );
        let prompt = message_text(&aggregator.1.messages.last().unwrap().content);
        assert!(prompt.contains("回答1：\npanel 1\n\n回答2：\npanel 2"));
        assert!(prompt.contains("当前对话的下一条 assistant response"));
        assert_eq!(result.response.usage.total_tokens, 9);
        assert_eq!(
            result.response.choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
    }

    #[test]
    fn prompt_assets_match_migration_baseline() {
        assert_eq!(
            sha256(DEFAULT_OUTPUT_CONTRACT),
            "d37eada5aeff79e70dce467e1489b2521462e712175b83fce4eddff914b26503"
        );
        assert_eq!(
            sha256(prompt_text(DIRECT_SYNTHESIS_REFERENCE_CONTEXT_PROMPT)),
            "59fb526ab0826688bad49aebb87a1fe6952900f27948ed4555ba4fe77a0b89a4"
        );
        assert_eq!(
            sha256(prompt_text(DEFAULT_OUTPUT_CONTRACT).trim()),
            "729d94fa14b730931863ec0d90afa03e2f435fbe5d732d4d39cd10aedb0d0ee4"
        );
        assert_eq!(
            sha256(prompt_text(REFERENCE_ADVISOR_PROMPT)),
            "fe831153a436a2d73188ed76db19598a23fdedce515dabc364bce32418647586"
        );
        assert_eq!(
            sha256(prompt_text(SELF_MOA_AGGREGATOR_PROMPT)),
            "513a249e312902e2ce8a22247806cdf28491fb1f2db7e66c78b1172bfa2b93b3"
        );
        assert_eq!(
            prompt_text(REFERENCE_ADVISOR_PROMPT),
            "You are a reference advisor in a Mixture of Agents (MoA) process. You are NOT the acting agent and you do NOT execute anything: you cannot call tools, run commands, browse, or access files, repositories, or URLs, and you should not try to or apologize for being unable to. A separate aggregator/orchestrator model holds those capabilities and will take the actual actions.\n\nThe conversation above is the current state of a task handled by that acting agent. Your job is to give your most intelligent analysis of that state: understand the goal, reason about the problem, and advise on what to do next. Surface the best approach, concrete next steps and tool-use strategy, likely pitfalls and risks, and anything the acting agent may have missed or gotten wrong. Assume any referenced files, URLs, or systems exist and reason about them from the context given rather than asking for access.\n\nRespond with your advice directly — no preamble, no disclaimers about tools or access. Your response is private guidance handed to the aggregator, not an answer shown to the user.\n\nGive your judgement: what is going on, what should happen next, what risks or mistakes you see, and how the acting agent should proceed."
        );
        assert!(prompt_text(DEFAULT_OUTPUT_CONTRACT)
            .starts_with("你的输出必须是当前对话的下一条 assistant response"));
    }

    #[test]
    fn canonical_swe_tool_requests_match_direct_synthesis_contract() {
        for raw in [
            include_str!("../tests/fixtures/swe_tool_first_turn_request.json"),
            include_str!("../tests/fixtures/swe_tool_result_request.json"),
        ] {
            let original = fixture(raw);
            let workflow = workflow();
            let panel = workflow.panel_request(&original, &workflow.config.panel[0]);

            assert_eq!(
                serde_json::to_value(&panel.messages[..original.messages.len()]).unwrap(),
                serde_json::to_value(&original.messages).unwrap()
            );
            assert_eq!(
                message_text(&panel.messages.last().unwrap().content),
                prompt_text(REFERENCE_ADVISOR_PROMPT)
            );
            assert!(panel.tools.as_ref().is_some_and(Vec::is_empty));
            assert!(panel.tool_choice.is_none());
            assert_eq!(panel.temperature, Some(0.3));
            assert_eq!(panel.extra.get("metadata"), original.extra.get("metadata"));

            let aggregator = workflow.aggregator_request(
                &original,
                &[invocation("panel A"), invocation("panel B")],
                false,
            );
            assert_eq!(
                serde_json::to_value(&aggregator.messages[..original.messages.len()]).unwrap(),
                serde_json::to_value(&original.messages).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&aggregator.tools).unwrap(),
                serde_json::to_value(&original.tools).unwrap()
            );
            assert_eq!(aggregator.tool_choice, original.tool_choice);
            assert_eq!(
                aggregator.extra.get("metadata"),
                original.extra.get("metadata")
            );
            let prompt = message_text(&aggregator.messages.last().unwrap().content);
            assert!(
                prompt.contains("原始问题：Fix astropy__astropy-13236 and run the relevant tests.")
            );
            assert!(prompt.contains("回答1：\npanel A\n\n回答2：\npanel B"));
            assert!(prompt.ends_with(prompt_text(DEFAULT_OUTPUT_CONTRACT).trim()));
        }
    }

    #[test]
    fn canonical_no_tools_request_keeps_panel_messages_unchanged() {
        let original = fixture(include_str!("../tests/fixtures/no_tools_request.json"));
        let workflow = workflow();
        let panel = workflow.panel_request(&original, &workflow.config.panel[1]);
        assert_eq!(
            serde_json::to_value(&panel.messages).unwrap(),
            serde_json::to_value(&original.messages).unwrap()
        );
        assert!(panel.tools.as_ref().is_some_and(Vec::is_empty));
        assert_eq!(panel.temperature, Some(0.5));

        let aggregator = workflow.aggregator_request(
            &original,
            &[invocation("first"), invocation("second")],
            false,
        );
        let prompt = message_text(&aggregator.messages.last().unwrap().content);
        assert!(prompt.starts_with("你是一个回答合成专家。"));
        assert!(prompt.contains("回答1：\nfirst\n\n回答2：\nsecond"));
        assert!(aggregator.tools.as_ref().is_some_and(Vec::is_empty));
        assert!(aggregator.tool_choice.is_none());
    }

    #[tokio::test]
    async fn partial_panel_failure_continues_and_counts_only_successful_usage() {
        let invoker = Arc::new(ScenarioInvoker {
            calls: Mutex::new(Vec::new()),
            fail_all_panels: false,
            fail_first_panel: true,
            fail_aggregator: false,
            delay_first_panel: false,
        });
        let result = workflow()
            .execute(WorkflowContext {
                request: request(),
                invoker,
            })
            .await
            .unwrap();

        assert_eq!(result.response.usage.total_tokens, 6);
    }

    #[tokio::test]
    async fn panel_timeout_excludes_only_the_slow_panel() {
        let invoker = Arc::new(ScenarioInvoker {
            calls: Mutex::new(Vec::new()),
            fail_all_panels: false,
            fail_first_panel: false,
            fail_aggregator: false,
            delay_first_panel: true,
        });
        let mut workflow = workflow();
        workflow.config.panel_timeout = Some(Duration::from_millis(1));
        let result = workflow
            .execute(WorkflowContext {
                request: request(),
                invoker,
            })
            .await
            .unwrap();

        assert_eq!(result.response.usage.total_tokens, 6);
    }

    #[test]
    fn panel_content_is_not_rejected_by_error_like_prefixes() {
        for content in [
            "ERROR: this is a diagnosis, not a provider error",
            "API_ERROR: explain how to recover",
            "ALL_PANELS_FAILED",
        ] {
            assert!(valid_panel(&invocation(content).response));
        }
    }

    #[tokio::test]
    async fn aggregator_failure_falls_back_to_first_panel() {
        let invoker = Arc::new(ScenarioInvoker {
            calls: Mutex::new(Vec::new()),
            fail_all_panels: false,
            fail_first_panel: false,
            fail_aggregator: true,
            delay_first_panel: false,
        });
        let result = workflow()
            .execute(WorkflowContext {
                request: request(),
                invoker,
            })
            .await
            .unwrap();

        assert_eq!(
            message_text(&result.response.choices[0].message.content),
            "configured first"
        );
        assert_eq!(result.response.usage.total_tokens, 6);
    }

    #[tokio::test]
    async fn all_panel_failures_stop_before_aggregator() {
        let invoker = Arc::new(ScenarioInvoker {
            calls: Mutex::new(Vec::new()),
            fail_all_panels: true,
            fail_first_panel: false,
            fail_aggregator: false,
            delay_first_panel: false,
        });
        let result = workflow()
            .execute(WorkflowContext {
                request: request(),
                invoker: invoker.clone(),
            })
            .await;
        let failure = match result {
            Ok(_) => panic!("all panel failures must fail the workflow"),
            Err(failure) => failure,
        };

        assert!(failure.to_string().contains("ALL_PANELS_FAILED"));
        assert_eq!(invoker.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn panel_answers_follow_config_order_not_completion_order() {
        let invoker = Arc::new(ScenarioInvoker {
            calls: Mutex::new(Vec::new()),
            fail_all_panels: false,
            fail_first_panel: false,
            fail_aggregator: false,
            delay_first_panel: true,
        });
        workflow()
            .execute(WorkflowContext {
                request: request(),
                invoker: invoker.clone(),
            })
            .await
            .unwrap();

        let calls = invoker.calls.lock().unwrap();
        let aggregator = calls
            .iter()
            .find(|(role, _)| *role == WorkflowRole::Aggregator)
            .unwrap();
        let prompt = message_text(&aggregator.1.messages.last().unwrap().content);
        assert!(prompt.contains("回答1：\nconfigured first\n\n回答2：\nconfigured second"));
    }

    #[tokio::test]
    async fn streams_only_the_aggregator_and_adds_panel_usage() {
        let invoker = Arc::new(ScenarioInvoker {
            calls: Mutex::new(Vec::new()),
            fail_all_panels: false,
            fail_first_panel: false,
            fail_aggregator: false,
            delay_first_panel: false,
        });
        let execution = workflow()
            .execute_stream(WorkflowContext {
                request: request(),
                invoker: invoker.clone(),
            })
            .await
            .unwrap();

        let chunk = execution
            .stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.choices[0].delta.content.as_deref(),
            Some("aggregated")
        );
        assert_eq!(chunk.usage.as_ref().unwrap().total_tokens, Some(13));

        let calls = invoker.calls.lock().unwrap();
        assert!(calls
            .iter()
            .filter(|(role, _)| *role == WorkflowRole::Panel)
            .all(|(_, request)| request.stream == Some(false)));
        assert_eq!(
            calls
                .iter()
                .find(|(role, _)| *role == WorkflowRole::Aggregator)
                .unwrap()
                .1
                .stream,
            Some(true)
        );
    }

    #[tokio::test]
    async fn streaming_usage_accumulates_partial_provider_snapshots() {
        let chunk = |usage| ChatStreamChunk {
            id: "chatcmpl-stream".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1,
            model: "glm-5.2".to_string(),
            choices: Vec::new(),
            usage: Some(usage),
            raw_data: None,
        };
        let inner = futures::stream::iter([
            Ok(chunk(StreamUsage {
                prompt_tokens: Some(5),
                completion_tokens: Some(0),
                total_tokens: None,
                prompt_tokens_details: Some(PromptTokensDetails {
                    cached_tokens: Some(2),
                }),
            })),
            Ok(chunk(StreamUsage {
                prompt_tokens: None,
                completion_tokens: Some(2),
                total_tokens: None,
                prompt_tokens_details: None,
            })),
        ]);
        let stream = AggregateUsageStream {
            inner: Box::pin(inner),
            panel_usage: Usage {
                prompt_tokens: 4,
                completion_tokens: 2,
                total_tokens: 6,
                prompt_tokens_details: Some(PromptTokensDetails {
                    cached_tokens: Some(1),
                }),
                ..Usage::default()
            },
            aggregator_usage: Usage::default(),
        };

        let chunks = stream.collect::<Vec<_>>().await;
        let first = chunks[0].as_ref().unwrap().usage.as_ref().unwrap();
        assert_eq!(first.prompt_tokens, Some(9));
        assert_eq!(first.completion_tokens, Some(2));
        assert_eq!(first.total_tokens, Some(11));

        let second = chunks[1].as_ref().unwrap().usage.as_ref().unwrap();
        assert_eq!(second.prompt_tokens, Some(9));
        assert_eq!(second.completion_tokens, Some(4));
        assert_eq!(second.total_tokens, Some(13));
        assert_eq!(
            second
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens),
            Some(3)
        );

    }

    #[tokio::test]
    async fn streaming_without_provider_usage_does_not_invent_aggregator_usage() {
        let inner = futures::stream::iter([Ok(ChatStreamChunk {
            id: "chatcmpl-stream".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1,
            model: "glm-5.2".to_string(),
            choices: Vec::new(),
            usage: None,
            raw_data: None,
        })]);
        let stream = AggregateUsageStream {
            inner: Box::pin(inner),
            panel_usage: Usage {
                prompt_tokens: 4,
                completion_tokens: 2,
                total_tokens: 6,
                ..Usage::default()
            },
            aggregator_usage: Usage::default(),
        };

        let chunks = stream.collect::<Vec<_>>().await;
        assert!(chunks[0].as_ref().unwrap().usage.is_none());
    }

    #[test]
    fn stream_usage_clamps_only_at_the_i32_protocol_boundary() {
        let usage = combined_stream_usage(
            &Usage {
                prompt_tokens: u32::MAX,
                completion_tokens: u32::MAX,
                total_tokens: u32::MAX,
                ..Usage::default()
            },
            &Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 1,
                ..Usage::default()
            },
        );

        assert_eq!(usage.prompt_tokens, Some(i32::MAX));
        assert_eq!(usage.completion_tokens, Some(i32::MAX));
        assert_eq!(usage.total_tokens, Some(i32::MAX));
    }

    #[tokio::test]
    async fn streaming_aggregator_failure_falls_back_to_panel_stream() {
        let invoker = Arc::new(ScenarioInvoker {
            calls: Mutex::new(Vec::new()),
            fail_all_panels: false,
            fail_first_panel: false,
            fail_aggregator: true,
            delay_first_panel: false,
        });
        let execution = workflow()
            .execute_stream(WorkflowContext {
                request: request(),
                invoker,
            })
            .await
            .unwrap();

        let chunks = execution.stream.collect::<Vec<_>>().await;
        assert_eq!(
            chunks[0].as_ref().unwrap().choices[0]
                .delta
                .content
                .as_deref(),
            Some("configured first")
        );
        assert_eq!(
            chunks[1]
                .as_ref()
                .unwrap()
                .usage
                .as_ref()
                .unwrap()
                .total_tokens,
            Some(6)
        );
    }
}
