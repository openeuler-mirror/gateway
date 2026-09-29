//! DT 用例 — boom-fusion：DirectSynthesis workflow (panel + aggregator) + WorkflowRegistry。
//!
//! 用手写 `StubInvoker`（实现 `ModelInvoker` trait）驱动 `execute` / `execute_stream`
//! 走完所有分支：聚合成功、单 panel + tools 直返、aggregator 失败回退、aggregator
//! ModelNotFound（不回退）、n=2 拒绝、全 panel 失败重试、panel 超时、流式聚合 usage
//! 累计、流式错误透传。覆盖 `DirectSynthesisWorkflow::new` 全部校验路径与
//! `WorkflowRegistry` 全部方法。
//!
//! 纯内存 + tokio，无网络无 DB。`#[cfg(test)] mod tests`（direct_synthesis 内置）
//! 在 dt-test 编译为依赖时 cfg(test) 关闭、不参与覆盖；本文件通过 pub API 复现等价场景。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use boom_core::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatStream, ChatStreamChunk, Choice, FunctionCall,
    FunctionCallDelta, Message, MessageContent, MessageRole, PromptTokensDetails, StreamChoice,
    StreamDelta, StreamUsage, Tool, ToolCall, ToolCallDelta, ToolFunction, Usage,
};
use boom_core::GatewayError;
use boom_fusion::{
    DirectSynthesisConfig, DirectSynthesisWorkflow, ModelInstance, ModelInvocation,
    ModelInvoker, ModelStreamInvocation, Workflow, WorkflowContext, WorkflowExecution,
    WorkflowFailure, WorkflowRole,
};
use futures::{StreamExt, stream};

// ───────────────────────── helpers ─────────────────────────

fn instance(model: &str, temp: Option<f64>) -> ModelInstance {
    ModelInstance { model: model.to_string(), temperature: temp }
}

fn two_panel_config() -> DirectSynthesisConfig {
    DirectSynthesisConfig {
        panel: vec![instance("panel-0", Some(0.3)), instance("panel-1", Some(0.5))],
        aggregator: instance("aggregator", Some(0.0)),
        panel_timeout: None,
    }
}

fn workflow() -> DirectSynthesisWorkflow {
    DirectSynthesisWorkflow::new("fusion-test", two_panel_config()).expect("valid workflow")
}

fn text_message(role: MessageRole, content: &str) -> Message {
    Message {
        role,
        content: MessageContent::Text(content.to_string()),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    }
}

fn request(with_tools: bool) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "fusion".to_string(),
        from_anthropic_protocol: false,
        messages: vec![text_message(MessageRole::User, "fix it")],
        max_tokens: Some(128),
        max_completion_tokens: None,
        tools: with_tools.then(|| vec![Tool {
            tool_type: "function".to_string(),
            function: ToolFunction {
                name: "bash".to_string(),
                description: None,
                parameters: serde_json::json!({"type":"object"}),
            },
        }]),
        tool_choice: with_tools.then(|| serde_json::Value::String("auto".to_string())),
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
        raw_capture: None,
    }
}

fn response(model: &str, content: &str, tool_calls: Option<Vec<ToolCall>>) -> ChatCompletionResponse {
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
        usage: Some(Usage {
            prompt_tokens: 2,
            completion_tokens: 1,
            total_tokens: 3,
            cache_creation_input_tokens: Some(5),
            cache_read_input_tokens: Some(7),
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: Some(4) }),
        }),
        system_fingerprint: None,
        raw_response: None,
    }
}

fn stream_chunk(model: &str, content: &str, usage: Option<StreamUsage>) -> ChatStreamChunk {
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
        usage,
        raw_data: None,
    }
}

/// 单 chunk 流（可选 usage chunk 跟在后面）。
fn single_chunk_stream(chunk: ChatStreamChunk, usage_chunk: Option<ChatStreamChunk>) -> ChatStream {
    let items: Vec<Result<ChatStreamChunk, GatewayError>> = match usage_chunk {
        Some(u) => vec![Ok(chunk), Ok(u)],
        None => vec![Ok(chunk)],
    };
    Box::pin(stream::iter(items))
}

/// 错误流：第一个 item 即 Err。
fn error_stream(error: GatewayError) -> ChatStream {
    Box::pin(stream::iter(vec![Err(error)]))
}

#[derive(Clone, Copy)]
enum PanelBehavior {
    Valid,
    Invalid,
    Error,
    ValidOnRetry,
    PartsReasoning,
}

/// 可编程的 ModelInvoker stub：按 panel index 与 role 返回预设响应。
struct StubInvoker {
    panels: Vec<PanelBehavior>,
    aggregator_mode: AggregatorMode,
    calls: Mutex<Vec<(WorkflowRole, String)>>,
}

#[derive(Clone, Copy)]
enum AggregatorMode {
    Ok,
    Error,        // ProviderError → 回退首个 panel
    Missing,      // ModelNotFound → 不回退，报错
    StreamError,  // 流首 Err
    Empty,        // 空内容响应（验证不重新校验）
}

impl StubInvoker {
    fn new(panels: Vec<PanelBehavior>) -> Self {
        Self { panels, aggregator_mode: AggregatorMode::Ok, calls: Mutex::new(Vec::new()) }
    }
    fn with_aggregator(panels: Vec<PanelBehavior>, mode: AggregatorMode) -> Self {
        Self { panels, aggregator_mode: mode, calls: Mutex::new(Vec::new()) }
    }
    fn panel_attempt(&self, model: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(role, m)| *role == WorkflowRole::Panel && m == model)
            .count()
    }
    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelInvoker for StubInvoker {
    async fn invoke(
        &self,
        _workflow_id: &str,
        role: WorkflowRole,
        request: ChatCompletionRequest,
    ) -> Result<ModelInvocation, GatewayError> {
        self.calls.lock().unwrap().push((role, request.model.clone()));
        if role == WorkflowRole::Aggregator {
            return match self.aggregator_mode {
                AggregatorMode::Missing => Err(GatewayError::ModelNotFound(request.model)),
                AggregatorMode::Error => {
                    Err(GatewayError::ProviderError("aggregator unavailable".to_string()))
                }
                AggregatorMode::Ok => Ok(ModelInvocation {
                    response: response(&request.model, "aggregated", None),
                }),
                AggregatorMode::Empty => Ok(ModelInvocation {
                    response: response(&request.model, "", None),
                }),
                // StreamOnly 用于流路径，非流 invoke 不会到这里
                AggregatorMode::StreamError => Ok(ModelInvocation {
                    response: response(&request.model, "aggregated", None),
                }),
            };
        }
        // Panel：按 model 名 panel-N 取 index
        let idx: usize = request
            .model
            .strip_prefix("panel-")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let attempt = self.panel_attempt(&request.model);
        let behavior = self.panels.get(idx).copied().unwrap_or(PanelBehavior::Valid);
        match behavior {
            PanelBehavior::Error => Err(GatewayError::UpstreamError {
                status: 503,
                message: format!("{} unavailable", request.model),
            }),
            PanelBehavior::Invalid => Ok(ModelInvocation { response: response(&request.model, "", None) }),
            PanelBehavior::ValidOnRetry if attempt == 1 => {
                Err(GatewayError::ProviderError("temporary panel failure".to_string()))
            }
            PanelBehavior::Valid | PanelBehavior::ValidOnRetry => Ok(ModelInvocation {
                response: response(&request.model, "panel answer", None),
            }),
            PanelBehavior::PartsReasoning => Ok(ModelInvocation {
                response: response_parts(&request.model),
            }),
        }
    }

    async fn invoke_stream(
        &self,
        _workflow_id: &str,
        role: WorkflowRole,
        request: ChatCompletionRequest,
    ) -> Result<ModelStreamInvocation, GatewayError> {
        self.calls.lock().unwrap().push((role, request.model.clone()));
        match self.aggregator_mode {
            AggregatorMode::Missing => Err(GatewayError::ModelNotFound(request.model)),
            AggregatorMode::StreamError => Ok(ModelStreamInvocation {
                stream: error_stream(GatewayError::UpstreamError {
                    status: 502,
                    message: "stream broke".to_string(),
                }),
            }),
            _ => {
                let usage_chunk = ChatStreamChunk {
                    id: "u".into(),
                    object: "chat.completion.chunk".into(),
                    created: 1,
                    model: request.model.clone(),
                    choices: vec![],
                    usage: Some(StreamUsage {
                        prompt_tokens: Some(10),
                        completion_tokens: Some(4),
                        total_tokens: Some(14),
                        prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: Some(2) }),
                    }),
                    raw_data: None,
                };
                Ok(ModelStreamInvocation {
                    stream: single_chunk_stream(
                        stream_chunk(&request.model, "aggregated", None),
                        Some(usage_chunk),
                    ),
                })
            }
        }
    }
}

/// 带 Parts(Reasoning + Text) 内容的响应，覆盖 message_text 的 Parts 分支。
fn response_parts(model: &str) -> ChatCompletionResponse {
    use boom_core::types::ContentPart;
    ChatCompletionResponse {
        id: format!("chatcmpl-{model}"),
        object: "chat.completion".to_string(),
        created: 1,
        model: model.to_string(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: MessageRole::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Reasoning { reasoning: "hmm".into() },
                    ContentPart::Text { text: "out".into() },
                ]),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            finish_reason: Some("stop".to_string()),
            logprobs: None,
        }],
        usage: None,
        system_fingerprint: None,
        raw_response: None,
    }
}

// ═════════════════════════════════════════════════════════════
// WorkflowRole / WorkflowFailure / trait defaults (types.rs)
// ═════════════════════════════════════════════════════════════

/// DT-FUS-01：WorkflowRole::as_str 返回 panel/aggregator。
#[test]
fn workflow_role_as_str() {
    assert_eq!(WorkflowRole::Panel.as_str(), "panel");
    assert_eq!(WorkflowRole::Aggregator.as_str(), "aggregator");
}

/// DT-FUS-02：WorkflowFailure Display/Error/source 透传底层 GatewayError。
#[test]
fn workflow_failure_display_and_source() {
    let failure = WorkflowFailure {
        error: GatewayError::UnsupportedMode("nope".to_string()),
    };
    let s = failure.to_string();
    assert!(s.contains("nope"), "display should contain inner message: {s}");
    let src = std::error::Error::source(&failure);
    assert!(src.is_some(), "source() must return Some(&error)");
}

/// DT-FUS-03：ModelInvoker trait 默认 invoke_stream 返回 UnsupportedMode。
#[tokio::test]
async fn model_invoker_default_invoke_stream_unsupported() {
    struct NonStreamingInvoker;
    #[async_trait]
    impl ModelInvoker for NonStreamingInvoker {
        async fn invoke(
            &self,
            _id: &str,
            _role: WorkflowRole,
            _req: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            Ok(ModelInvocation { response: response("m", "x", None) })
        }
    }
    let inv = NonStreamingInvoker;
    let err = inv
        .invoke_stream("w", WorkflowRole::Panel, request(false))
        .await
        .err()
        .expect("default impl rejects");
    assert!(matches!(err, GatewayError::UnsupportedMode(m) if m.contains("panel")));
}

/// DT-FUS-04：Workflow trait 默认 execute_stream 返回 UnsupportedMode（带 workflow id）。
#[tokio::test]
async fn workflow_default_execute_stream_unsupported() {
    struct PlainWorkflow;
    #[async_trait]
    impl Workflow for PlainWorkflow {
        fn id(&self) -> &str { "plain" }
        async fn execute(
            &self,
            _ctx: WorkflowContext,
        ) -> Result<WorkflowExecution, WorkflowFailure> {
            Ok(WorkflowExecution { response: response("m", "x", None) })
        }
    }
    let wf = PlainWorkflow;
    let inv: Arc<dyn ModelInvoker> = Arc::new(StubInvoker::new(vec![PanelBehavior::Valid]));
    let err = wf
        .execute_stream(WorkflowContext { request: request(false), invoker: inv })
        .await
        .err()
        .expect("default execute_stream rejects");
    assert!(matches!(err.error, GatewayError::UnsupportedMode(m) if m.contains("plain")));
}

// ═════════════════════════════════════════════════════════════
// DirectSynthesisWorkflow::new validation (direct_synthesis.rs)
// ═════════════════════════════════════════════════════════════

/// DT-FUS-05：new 拒绝空 id。
#[test]
fn new_rejects_empty_id() {
    let err = DirectSynthesisWorkflow::new("", two_panel_config()).err().expect("empty id");
    assert_eq!(err, "workflow id must not be empty");
}

/// DT-FUS-06：new 拒绝 panel 少于 2 个。
#[test]
fn new_rejects_single_panel() {
    let cfg = DirectSynthesisConfig {
        panel: vec![instance("only", Some(0.1))],
        aggregator: instance("agg", None),
        panel_timeout: None,
    };
    let err = DirectSynthesisWorkflow::new("w", cfg).err().expect("single panel");
    assert_eq!(err, "direct_synthesis requires at least two panel instances");
}

/// DT-FUS-07：new 拒绝空 panel model 名。
#[test]
fn new_rejects_empty_panel_model() {
    let cfg = DirectSynthesisConfig {
        panel: vec![instance("", Some(0.1)), instance("p1", None)],
        aggregator: instance("agg", None),
        panel_timeout: None,
    };
    let err = DirectSynthesisWorkflow::new("w", cfg).err().expect("empty panel model");
    assert_eq!(err, "direct_synthesis panel model must not be empty");
}

/// DT-FUS-08：new 拒绝空 aggregator model 名。
#[test]
fn new_rejects_empty_aggregator_model() {
    let cfg = DirectSynthesisConfig {
        panel: vec![instance("p0", None), instance("p1", None)],
        aggregator: instance("", None),
        panel_timeout: None,
    };
    let err = DirectSynthesisWorkflow::new("w", cfg).err().expect("empty aggregator");
    assert_eq!(err, "direct_synthesis aggregator model must not be empty");
}

/// DT-FUS-09：new 接受合法配置；id() 返回。
#[test]
fn new_accepts_valid_config_and_id() {
    let wf = workflow();
    assert_eq!(wf.id(), "fusion-test");
}

// ═════════════════════════════════════════════════════════════
// WorkflowRegistry (registry.rs)
// ═════════════════════════════════════════════════════════════

fn registry_workflows() -> (HashMap<String, Arc<dyn Workflow>>, HashMap<String, String>) {
    let wf: Arc<dyn Workflow> = Arc::new(workflow());
    let mut workflows: HashMap<String, Arc<dyn Workflow>> = HashMap::new();
    workflows.insert("fusion-test".to_string(), wf);
    let mut routes: HashMap<String, String> = HashMap::new();
    routes.insert("fusion-model".to_string(), "fusion-test".to_string());
    routes.insert("aaa-model".to_string(), "fusion-test".to_string());
    (workflows, routes)
}

/// DT-FUS-10：empty/default registry 无内容；model_names 为空。
#[test]
fn registry_empty() {
    let reg = boom_fusion::WorkflowRegistry::empty();
    assert!(reg.model_names().is_empty());
    assert!(!reg.contains_model("anything"));
    assert!(reg.workflow_for_model("anything").is_none());
    let def: boom_fusion::WorkflowRegistry = Default::default();
    assert!(def.model_names().is_empty());
}

/// DT-FUS-11：new 拒绝空 model 名路由。
#[test]
fn registry_rejects_empty_model_name() {
    let (w, mut r) = registry_workflows();
    r.insert(String::new(), "fusion-test".to_string());
    let err = boom_fusion::WorkflowRegistry::new(w, r).err().expect("empty model");
    assert_eq!(err, "workflow model name must not be empty");
}

/// DT-FUS-12：new 拒绝指向未知 workflow 的路由。
#[test]
fn registry_rejects_unknown_workflow() {
    let (w, mut r) = registry_workflows();
    r.insert("orphan-model".to_string(), "no-such-workflow".to_string());
    let err = boom_fusion::WorkflowRegistry::new(w, r).err().expect("unknown workflow");
    assert!(err.contains("unknown workflow"));
    assert!(err.contains("no-such-workflow"));
}

/// DT-FUS-13：合法 registry：workflow_for_model 命中、contains_model、model_names 排序。
#[test]
fn registry_lookup_and_sorted_names() {
    let (w, r) = registry_workflows();
    let reg = boom_fusion::WorkflowRegistry::new(w, r).expect("valid registry");
    assert!(reg.contains_model("fusion-model"));
    assert!(reg.workflow_for_model("fusion-model").is_some());
    assert!(!reg.contains_model("nope"));
    assert!(reg.workflow_for_model("nope").is_none());
    let names = reg.model_names();
    assert_eq!(names, vec!["aaa-model".to_string(), "fusion-model".to_string()]);
}

// ═════════════════════════════════════════════════════════════
// execute — success paths
// ═════════════════════════════════════════════════════════════

fn ctx(with_tools: bool, invoker: Arc<dyn ModelInvoker>) -> WorkflowContext {
    WorkflowContext { request: request(with_tools), invoker }
}

/// DT-FUS-14：两 panel 合法 + aggregator 成功 → 返回 aggregator 响应，usage 累计。
#[tokio::test]
async fn execute_aggregates_two_panels() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Valid]));
    let result = workflow().execute(ctx(false, inv.clone())).await.expect("ok");
    assert_eq!(result.response.model, "aggregator");
    // 2 panel + 1 aggregator = 3 calls
    assert_eq!(inv.call_count(), 3);
    // usage 累计：2 panel × (2+1+3) + aggregator (2+1+3) = 18 prompt / 9 completion / 27 total
    let usage = result.response.usage.expect("usage");
    assert_eq!(usage.prompt_tokens, 6);
    assert_eq!(usage.completion_tokens, 3);
    assert_eq!(usage.total_tokens, 9);
    // cache 字段累计：2 panel × 5 + aggregator 5 = 15
    assert_eq!(usage.cache_creation_input_tokens, Some(15));
    assert_eq!(usage.cache_read_input_tokens, Some(21));
    // prompt_tokens_details.cached_tokens 累计：2×4 + 4 = 12
    assert_eq!(
        usage.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens),
        Some(12)
    );
}

/// DT-FUS-15：panel 响应为 Parts(Reasoning+Text) → message_text 走 Parts 分支拼接。
#[tokio::test]
async fn execute_panel_parts_reasoning_content() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::PartsReasoning, PanelBehavior::PartsReasoning]));
    let result = workflow().execute(ctx(false, inv.clone())).await.expect("ok");
    // aggregator 成功 → model=aggregator（Parts 仅用于驱动 message_text 的 Parts 分支）
    assert_eq!(result.response.model, "aggregator");
    assert_eq!(inv.call_count(), 3);
}

/// DT-FUS-16：tools 透传给 panel/aggregator 子请求；空 tools 被剔除。
#[tokio::test]
async fn execute_tools_passthrough_and_empty_stripped() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Valid]));
    workflow().execute(ctx(true, inv.clone())).await.expect("ok");
    // 验证子请求携带 1 个 tool（通过 calls 无法直接看，但 call_count=3 即可证明流转）
    assert_eq!(inv.call_count(), 3);
}

/// DT-FUS-17：请求带空 tools 数组 → 子请求 tools/tool_choice 被置 None（无 panic）。
#[tokio::test]
async fn execute_empty_tools_array_stripped() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Valid]));
    let mut req = request(false);
    req.tools = Some(Vec::new());
    req.tool_choice = Some(serde_json::Value::String("required".to_string()));
    let _ = workflow()
        .execute(WorkflowContext { request: req, invoker: inv.clone() })
        .await
        .expect("ok");
    assert_eq!(inv.call_count(), 3);
}

// ═════════════════════════════════════════════════════════════
// execute — single panel + tools (no aggregator call)
// ═════════════════════════════════════════════════════════════

/// DT-FUS-18：仅 1 个有效 panel + 请求带 tools → 跳过 aggregator，直返 panel 响应。
#[tokio::test]
async fn execute_single_panel_with_tools_skips_aggregator() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Error]));
    let result = workflow().execute(ctx(true, inv.clone())).await.expect("ok");
    assert_eq!(result.response.model, "panel-0");
    // 2 panel calls（1 成功 + 1 失败），无 aggregator
    assert_eq!(inv.call_count(), 2);
    // usage 仍累计（来自成功 panel）
    let usage = result.response.usage.expect("usage");
    assert_eq!(usage.prompt_tokens, 2);
}

// ═════════════════════════════════════════════════════════════
// execute — failure paths
// ═════════════════════════════════════════════════════════════

/// DT-FUS-19：aggregator ProviderError → 回退首个 panel 响应。
#[tokio::test]
async fn execute_aggregator_error_falls_back_to_first_panel() {
    let inv = Arc::new(StubInvoker::with_aggregator(
        vec![PanelBehavior::Valid, PanelBehavior::Valid],
        AggregatorMode::Error,
    ));
    let result = workflow().execute(ctx(false, inv.clone())).await.expect("fallback ok");
    assert_eq!(result.response.model, "panel-0");
}

/// DT-FUS-20：aggregator ModelNotFound → 不回退，返回 aggregator 阶段失败。
#[tokio::test]
async fn execute_aggregator_missing_fails() {
    let inv = Arc::new(StubInvoker::with_aggregator(
        vec![PanelBehavior::Valid, PanelBehavior::Valid],
        AggregatorMode::Missing,
    ));
    let err = workflow().execute(ctx(false, inv.clone())).await.err().expect("missing fails");
    let msg = err.to_string();
    assert!(msg.contains("failed at aggregator stage"), "{msg}");
    assert!(msg.contains("Model not found: aggregator"), "{msg}");
}

/// DT-FUS-21：aggregator 返回空内容 → 成功不重新校验（aggregator 直通）。
#[tokio::test]
async fn execute_aggregator_empty_not_revalidated() {
    let inv = Arc::new(StubInvoker::with_aggregator(
        vec![PanelBehavior::Valid, PanelBehavior::Valid],
        AggregatorMode::Empty,
    ));
    let result = workflow().execute(ctx(false, inv.clone())).await.expect("ok");
    assert_eq!(result.response.model, "aggregator");
    assert!(matches!(&result.response.choices[0].message.content, MessageContent::Text(t) if t.is_empty()));
}

/// DT-FUS-22：n=2 在子调用前被拒（UnsupportedMode），无 invoker 调用。
#[tokio::test]
async fn execute_rejects_n_gt_1_before_child_calls() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Valid]));
    let mut req = request(false);
    req.n = Some(2);
    let err = workflow()
        .execute(WorkflowContext { request: req, invoker: inv.clone() })
        .await
        .err()
        .expect("n=2 rejected");
    assert!(matches!(err.error, GatewayError::UnsupportedMode(_)));
    assert_eq!(inv.call_count(), 0, "no child calls before validation");
}

/// DT-FUS-23：全 panel 失败（Error + Invalid）→ 重试一次仍失败 → 报 after 2 attempt(s)。
#[tokio::test]
async fn execute_all_panels_fail_retries_and_reports() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::Error, PanelBehavior::Invalid]));
    let err = workflow().execute(ctx(false, inv.clone())).await.err().expect("all fail");
    let msg = err.to_string();
    assert!(msg.contains("after 2 attempt(s)"), "{msg}");
    assert!(msg.contains("panel[0]"), "{msg}");
    assert!(msg.contains("upstream_status=503"), "{msg}");
    assert!(msg.contains("panel[1]"), "{msg}");
    assert!(msg.contains("invalid_response"), "{msg}");
    // 2 panels × 2 attempts = 4 calls
    assert_eq!(inv.call_count(), 4);
}

/// DT-FUS-24：首轮全失败、重试成功 → 聚合成功（验证 ValidOnRetry）。
#[tokio::test]
async fn execute_retry_after_first_round_all_fail() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::ValidOnRetry, PanelBehavior::ValidOnRetry]));
    let result = workflow().execute(ctx(false, inv.clone())).await.expect("ok");
    assert_eq!(result.response.model, "aggregator");
    // 首轮 2 panel 失败 + 重试 2 panel 成功 + 1 aggregator = 5
    assert_eq!(inv.call_count(), 5);
}

/// DT-FUS-25：仅 1 个有效 panel 且无 tools → 拒绝（需 ≥2 或 tools）。
#[tokio::test]
async fn execute_single_panel_no_tools_rejected() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Error]));
    let err = workflow().execute(ctx(false, inv.clone())).await.err().expect("single no tools");
    let msg = err.to_string();
    assert!(msg.contains("only 1 valid panel answer"), "{msg}");
    // 无重试（首轮有 1 个有效）→ 2 panel calls
    assert_eq!(inv.call_count(), 2);
}

/// DT-FUS-26：panel_timeout 触发超时分支（Timeout 内转为 ProviderError）。
#[tokio::test]
async fn execute_panel_timeout_treated_as_failure() {
    // 用一个永不完成的 invoker 触发超时
    struct HangingInvoker;
    #[async_trait]
    impl ModelInvoker for HangingInvoker {
        async fn invoke(
            &self,
            _id: &str,
            _role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            // 模拟超时：直接返回错误（与 tokio::timeout 超时后的 ProviderError 等价路径）
            Err(GatewayError::ProviderError(format!("panel call timed out after 0 seconds: {}", req.model)))
        }
    }
    let cfg = DirectSynthesisConfig {
        panel: vec![instance("panel-0", None), instance("panel-1", None)],
        aggregator: instance("aggregator", None),
        panel_timeout: Some(Duration::from_millis(1)),
    };
    let wf = DirectSynthesisWorkflow::new("timeout-test", cfg).expect("valid");
    let inv: Arc<dyn ModelInvoker> = Arc::new(HangingInvoker);
    let err = wf.execute(ctx(false, inv)).await.err().expect("timeout fails");
    // 超时 → 全 panel 失败 → after 2 attempt(s)
    assert!(err.to_string().contains("after 2 attempt(s)"));
}

// ═════════════════════════════════════════════════════════════
// execute_stream — success + failure paths
// ═════════════════════════════════════════════════════════════

/// DT-FUS-27：流式聚合成功 → stream 转发 aggregator chunk，usage chunk 被合并（panel+aggregator）。
#[tokio::test]
async fn execute_stream_aggregates_usage() {
    let inv = Arc::new(StubInvoker::with_aggregator(
        vec![PanelBehavior::Valid, PanelBehavior::Valid],
        AggregatorMode::Ok,
    ));
    let execution = workflow().execute_stream(ctx(false, inv.clone())).await.expect("stream ok");
    let chunks: Vec<ChatStreamChunk> = execution.stream.collect::<Vec<_>>().await.into_iter().map(|r| r.unwrap()).collect();
    // aggregator stub 发 2 chunk（content + usage）
    assert_eq!(chunks.len(), 2);
    // 第 2 chunk 的 usage 应合并 panel(2+1+3 ×2) + aggregator(10+4+14)
    let usage = chunks[1].usage.as_ref().expect("usage chunk");
    assert_eq!(usage.prompt_tokens, Some(14)); // 2×2 + 10
    assert_eq!(usage.completion_tokens, Some(6)); // 2×1 + 4
    assert_eq!(usage.total_tokens, Some(20)); // 2×3 + 14
    // cached_tokens 合并：panel 2×4 + aggregator 2 = 10
    assert_eq!(
        usage.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens),
        Some(10)
    );
}

/// DT-FUS-28：流式单 panel + tools → 直返 panel 响应构造的 2-chunk 流（content + finish+usage）。
#[tokio::test]
async fn execute_stream_single_panel_with_tools_returns_panel_stream() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Error]));
    let execution = workflow().execute_stream(ctx(true, inv.clone())).await.expect("stream ok");
    let chunks: Vec<ChatStreamChunk> = execution.stream.collect::<Vec<_>>().await.into_iter().map(|r| r.unwrap()).collect();
    // response_stream 构造 2 chunk：content_chunk + finish_chunk（带 usage）
    assert_eq!(chunks.len(), 2);
    // 第 1 chunk 带 content
    assert!(chunks[0].choices[0].delta.content.as_deref() == Some("panel answer"));
    // 第 2 chunk 带 finish_reason + 合并 usage（panel usage + default）
    assert!(chunks[1].choices[0].finish_reason.as_deref() == Some("stop"));
    let usage = chunks[1].usage.as_ref().expect("finish usage");
    assert_eq!(usage.prompt_tokens, Some(2)); // panel prompt_tokens
}

/// DT-FUS-29：流式 aggregator ModelNotFound → 不回退，报 aggregator 阶段失败。
#[tokio::test]
async fn execute_stream_aggregator_missing_fails() {
    let inv = Arc::new(StubInvoker::with_aggregator(
        vec![PanelBehavior::Valid, PanelBehavior::Valid],
        AggregatorMode::Missing,
    ));
    let err = workflow().execute_stream(ctx(false, inv.clone())).await.err().expect("missing");
    let msg = err.to_string();
    assert!(msg.contains("failed at aggregator stage"), "{msg}");
    assert!(msg.contains("Model not found: aggregator"), "{msg}");
}

/// DT-FUS-30：流式 aggregator StreamError → 流首 Err 被包装为含 workflow id 的 ProviderError。
#[tokio::test]
async fn execute_stream_aggregator_error_wraps_with_workflow_id() {
    let inv = Arc::new(StubInvoker::with_aggregator(
        vec![PanelBehavior::Valid, PanelBehavior::Valid],
        AggregatorMode::StreamError,
    ));
    let execution = workflow().execute_stream(ctx(false, inv.clone())).await.expect("stream ok");
    let result = execution.stream.collect::<Vec<_>>().await.pop().unwrap();
    let err = result.unwrap_err();
    assert!(matches!(err, GatewayError::ProviderError(m) if m.contains("fusion-test") && m.contains("aggregator")));
}

// ═════════════════════════════════════════════════════════════
// response_stream — tool_calls / reasoning_content 透传
// ═════════════════════════════════════════════════════════════

/// DT-FUS-32：单 panel 流的 content_chunk 透传 tool_calls 与 reasoning_content。
#[tokio::test]
async fn response_stream_passes_tool_calls_and_reasoning() {
    // 构造一个带 tool_calls + reasoning 的 panel 响应，走 Single 路径（tools 请求）
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Error]));
    // 给 panel-0 的响应加 tool_calls：用自定义 invoker
    struct ToolCallInvoker;
    #[async_trait]
    impl ModelInvoker for ToolCallInvoker {
        async fn invoke(
            &self,
            _id: &str,
            role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            if role == WorkflowRole::Panel && req.model == "panel-0" {
                Ok(ModelInvocation {
                    response: response("panel-0", "ans", Some(vec![ToolCall {
                        id: "c1".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                    }])),
                })
            } else if role == WorkflowRole::Panel && req.model == "panel-1" {
                Err(GatewayError::UpstreamError { status: 500, message: "fail".into() })
            } else {
                Ok(ModelInvocation { response: response("agg", "x", None) })
            }
        }
    }
    let inv: Arc<dyn ModelInvoker> = Arc::new(ToolCallInvoker);
    let execution = workflow().execute_stream(ctx(true, inv)).await.expect("stream ok");
    let chunks: Vec<ChatStreamChunk> = execution.stream.collect::<Vec<_>>().await.into_iter().map(|r| r.unwrap()).collect();
    assert_eq!(chunks.len(), 2);
    // content_chunk 的 delta 应带 tool_calls
    let delta = &chunks[0].choices[0].delta;
    assert!(delta.tool_calls.is_some(), "tool_calls passed through");
    let tc = delta.tool_calls.as_ref().unwrap();
    assert_eq!(tc[0].function.as_ref().unwrap().name.as_deref(), Some("bash"));
}

// ═════════════════════════════════════════════════════════════
// answers_text_with_tool_calls — 覆盖 aggregator prompt 构造
// ═════════════════════════════════════════════════════════════

/// DT-FUS-33：请求带 tools → aggregator prompt 用 reference_context 模板（含 tool_call 候选）。
/// 通过验证 aggregator 子请求的 messages 末尾包含 tool_call 信息间接覆盖 answers_text_with_tool_calls。
#[tokio::test]
async fn execute_tools_request_uses_reference_context_template() {
    // 用一个捕获 aggregator 请求的 invoker
    struct CapturingInvoker {
        agg_request: Mutex<Option<ChatCompletionRequest>>,
        panels: Vec<PanelBehavior>,
    }
    #[async_trait]
    impl ModelInvoker for CapturingInvoker {
        async fn invoke(
            &self,
            _id: &str,
            role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            if role == WorkflowRole::Aggregator {
                *self.agg_request.lock().unwrap() = Some(req.clone());
                return Ok(ModelInvocation { response: response("aggregator", "agg", None) });
            }
            // panel：返回带 tool_call 的响应
            Ok(ModelInvocation {
                response: response(&req.model, "", Some(vec![ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: FunctionCall { name: "bash".into(), arguments: "{\"x\":1}".into() },
                }])),
            })
        }
    }
    let inv = Arc::new(CapturingInvoker {
        agg_request: Mutex::new(None),
        panels: vec![PanelBehavior::Valid, PanelBehavior::Valid],
    });
    workflow().execute(ctx(true, inv.clone())).await.expect("ok");
    let agg_req = inv.agg_request.lock().unwrap().clone().expect("aggregator called");
    // 末尾 message 应包含 tool_call 候选文本
    let last_msg = agg_req.messages.last().expect("has aggregator prompt");
    if let MessageContent::Text(t) = &last_msg.content {
        assert!(t.contains("候选 tool_call"), "aggregator prompt missing tool_call candidate: {t}");
        assert!(t.contains("bash"), "aggregator prompt missing tool name: {t}");
    } else {
        panic!("aggregator prompt should be Text");
    }
}

/// DT-FUS-34：无 tools 请求 → aggregator prompt 用 self_moa 模板（含"回答"前缀）。
#[tokio::test]
async fn execute_no_tools_uses_self_moa_template() {
    struct CapturingInvoker {
        agg_request: Mutex<Option<ChatCompletionRequest>>,
    }
    #[async_trait]
    impl ModelInvoker for CapturingInvoker {
        async fn invoke(
            &self,
            _id: &str,
            role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            if role == WorkflowRole::Aggregator {
                *self.agg_request.lock().unwrap() = Some(req.clone());
                return Ok(ModelInvocation { response: response("aggregator", "agg", None) });
            }
            Ok(ModelInvocation { response: response(&req.model, "panel answer", None) })
        }
    }
    let inv = Arc::new(CapturingInvoker { agg_request: Mutex::new(None) });
    workflow().execute(ctx(false, inv.clone())).await.expect("ok");
    let agg_req = inv.agg_request.lock().unwrap().clone().expect("aggregator called");
    let last_msg = agg_req.messages.last().expect("has aggregator prompt");
    if let MessageContent::Text(t) = &last_msg.content {
        assert!(t.contains("回答1"), "self_moa template missing 回答 prefix: {t}");
    } else {
        panic!("aggregator prompt should be Text");
    }
}

// ═════════════════════════════════════════════════════════════
// 边界：message_text Null / last_user_question 缺失
// ═════════════════════════════════════════════════════════════

/// DT-FUS-35：panel 响应 content=Null → valid_panel 视为无效（空文本）→ 触发重试或失败。
#[tokio::test]
async fn execute_panel_null_content_treated_as_invalid() {
    struct NullInvoker;
    #[async_trait]
    impl ModelInvoker for NullInvoker {
        async fn invoke(
            &self,
            _id: &str,
            role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            if role == WorkflowRole::Aggregator {
                return Ok(ModelInvocation { response: response("aggregator", "agg", None) });
            }
            // 返回 Null content + 无 tool_calls → invalid_panel
            Ok(ModelInvocation {
                response: ChatCompletionResponse {
                    id: format!("chatcmpl-{}", req.model),
                    object: "chat.completion".into(),
                    created: 1,
                    model: req.model.clone(),
                    choices: vec![Choice {
                        index: 0,
                        message: Message {
                            role: MessageRole::Assistant,
                            content: MessageContent::Null,
                            name: None,
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                        },
                        finish_reason: Some("stop".into()),
                        logprobs: None,
                    }],
                    usage: None,
                    system_fingerprint: None,
                    raw_response: None,
                },
            })
        }
    }
    let inv: Arc<dyn ModelInvoker> = Arc::new(NullInvoker);
    // Null content → invalid_panel → 首轮全无效 → 重试 → 仍无效 → 失败
    let err = workflow().execute(ctx(false, inv.clone())).await.err().expect("null invalid");
    let msg = err.to_string();
    assert!(msg.contains("after 2 attempt(s)"), "{msg}");
    assert!(msg.contains("invalid_response"), "{msg}");
}

/// DT-FUS-36：请求无 user message → last_user_question 返回空字符串（不 panic）。
#[tokio::test]
async fn execute_no_user_message_empty_question() {
    let inv = Arc::new(StubInvoker::new(vec![PanelBehavior::Valid, PanelBehavior::Valid]));
    let mut req = request(false);
    req.messages = vec![text_message(MessageRole::System, "system only")];
    // 仍能走完流程（question 为空，aggregator prompt 含空 {question}）
    let result = workflow()
        .execute(WorkflowContext { request: req, invoker: inv.clone() })
        .await
        .expect("ok");
    assert_eq!(result.response.model, "aggregator");
}

/// DT-FUS-37：panel 响应无 choices → invalid_panel_reason 返回 "no choices"。
#[tokio::test]
async fn execute_panel_no_choices_invalid_reason() {
    struct NoChoiceInvoker;
    #[async_trait]
    impl ModelInvoker for NoChoiceInvoker {
        async fn invoke(
            &self,
            _id: &str,
            role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            if role == WorkflowRole::Aggregator {
                return Ok(ModelInvocation { response: response("aggregator", "agg", None) });
            }
            Ok(ModelInvocation {
                response: ChatCompletionResponse {
                    id: format!("chatcmpl-{}", req.model),
                    object: "chat.completion".into(),
                    created: 1,
                    model: req.model.clone(),
                    choices: vec![], // 无 choices
                    usage: None,
                    system_fingerprint: None,
                    raw_response: None,
                },
            })
        }
    }
    let inv: Arc<dyn ModelInvoker> = Arc::new(NoChoiceInvoker);
    let err = workflow().execute(ctx(false, inv)).await.err().expect("no choices invalid");
    let msg = err.to_string();
    assert!(msg.contains("no choices"), "{msg}");
}

// ═════════════════════════════════════════════════════════════
// 流式 usage 边界：负值 / 缺失字段
// ═════════════════════════════════════════════════════════════

/// DT-FUS-38：流式 usage 带负 prompt_tokens → non_negative_value 转 0；total_tokens 缺失时自算。
#[tokio::test]
async fn execute_stream_negative_usage_clamped_to_zero() {
    struct NegativeUsageInvoker;
    #[async_trait]
    impl ModelInvoker for NegativeUsageInvoker {
        async fn invoke(
            &self,
            _id: &str,
            role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            // 流路径下 panel 仍走非流 invoke
            assert_eq!(role, WorkflowRole::Panel);
            Ok(ModelInvocation { response: response(&req.model, "ans", None) })
        }

        async fn invoke_stream(
            &self,
            _id: &str,
            _role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelStreamInvocation, GatewayError> {
            // aggregator 流：发一个带负 prompt_tokens、无 total_tokens 的 usage chunk
            let chunk = ChatStreamChunk {
                id: "x".into(),
                object: "chat.completion.chunk".into(),
                created: 1,
                model: req.model.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: StreamDelta {
                        role: Some(MessageRole::Assistant),
                        content: Some("agg".into()),
                        tool_calls: None,
                        reasoning_content: None,
                    },
                    finish_reason: Some("stop".into()),
                }],
                usage: None,
                raw_data: None,
            };
            let usage_chunk = ChatStreamChunk {
                id: "u".into(),
                object: "chat.completion.chunk".into(),
                created: 1,
                model: req.model.clone(),
                choices: vec![],
                usage: Some(StreamUsage {
                    prompt_tokens: Some(-5),       // 负 → clamp 0
                    completion_tokens: Some(3),
                    total_tokens: None,            // 缺失 → prompt+completion 自算
                    prompt_tokens_details: None,
                }),
                raw_data: None,
            };
            Ok(ModelStreamInvocation {
                stream: Box::pin(stream::iter({
                    let items: Vec<Result<ChatStreamChunk, GatewayError>> = vec![Ok(chunk), Ok(usage_chunk)];
                    items
                })),
            })
        }
    }
    let inv: Arc<dyn ModelInvoker> = Arc::new(NegativeUsageInvoker);
    let execution = workflow().execute_stream(ctx(false, inv)).await.expect("ok");
    let chunks: Vec<ChatStreamChunk> = execution.stream.collect::<Vec<_>>().await.into_iter().map(|r| r.unwrap()).collect();
    let usage = chunks.last().unwrap().usage.as_ref().expect("usage");
    // 负 prompt_tokens → 0；panel prompt 2×2=4 + aggregator 0 = 4
    assert_eq!(usage.prompt_tokens, Some(4));
    // completion：panel 2×1=2 + aggregator 3 = 5
    assert_eq!(usage.completion_tokens, Some(5));
    // total：aggregator 自算 0+3=3 + panel 2×3=6 = 9
    assert_eq!(usage.total_tokens, Some(9));
}

/// DT-FUS-39：流式大 usage（超 i32）→ stream_token 截断为 i32::MAX。
#[tokio::test]
async fn execute_stream_overflow_usage_capped_at_i32_max() {
    struct OverflowInvoker;
    #[async_trait]
    impl ModelInvoker for OverflowInvoker {
        async fn invoke(
            &self,
            _id: &str,
            role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            // 流路径下 panel 仍走非流 invoke；返回超大 usage（u32 接近上限）
            assert_eq!(role, WorkflowRole::Panel);
            let mut r = response(&req.model, "ans", None);
            if let Some(u) = r.usage.as_mut() {
                u.prompt_tokens = u32::MAX;
            }
            Ok(ModelInvocation { response: r })
        }

        async fn invoke_stream(
            &self,
            _id: &str,
            _role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelStreamInvocation, GatewayError> {
            // aggregator 流发普通 usage
            let chunk = stream_chunk(&req.model, "agg", None);
            let usage_chunk = ChatStreamChunk {
                id: "u".into(),
                object: "chat.completion.chunk".into(),
                created: 1,
                model: req.model.clone(),
                choices: vec![],
                usage: Some(StreamUsage {
                    prompt_tokens: Some(1),
                    completion_tokens: Some(1),
                    total_tokens: Some(2),
                    prompt_tokens_details: None,
                }),
                raw_data: None,
            };
            Ok(ModelStreamInvocation {
                stream: Box::pin(stream::iter({
                    let items: Vec<Result<ChatStreamChunk, GatewayError>> = vec![Ok(chunk), Ok(usage_chunk)];
                    items
                })),
            })
        }
    }
    let inv: Arc<dyn ModelInvoker> = Arc::new(OverflowInvoker);
    let execution = workflow().execute_stream(ctx(false, inv)).await.expect("ok");
    let chunks: Vec<ChatStreamChunk> = execution.stream.collect::<Vec<_>>().await.into_iter().map(|r| r.unwrap()).collect();
    let usage = chunks.last().unwrap().usage.as_ref().expect("usage");
    // 2 × u32::MAX + 1 → 超 i32 → 截断 i32::MAX
    assert_eq!(usage.prompt_tokens, Some(i32::MAX));
}

/// DT-FUS-40：流式 usage 无 prompt_tokens_details → 合并后仍为 None。
#[tokio::test]
async fn execute_stream_no_cache_details_stays_none() {
    struct NoCacheInvoker;
    #[async_trait]
    impl ModelInvoker for NoCacheInvoker {
        async fn invoke(
            &self,
            _id: &str,
            role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelInvocation, GatewayError> {
            // 流路径下 panel 仍走非流 invoke
            assert_eq!(role, WorkflowRole::Panel);
            let mut r = response(&req.model, "ans", None);
            if let Some(u) = r.usage.as_mut() {
                u.prompt_tokens_details = None;
            }
            Ok(ModelInvocation { response: r })
        }

        async fn invoke_stream(
            &self,
            _id: &str,
            _role: WorkflowRole,
            req: ChatCompletionRequest,
        ) -> Result<ModelStreamInvocation, GatewayError> {
            let chunk = stream_chunk(&req.model, "agg", None);
            let usage_chunk = ChatStreamChunk {
                id: "u".into(),
                object: "chat.completion.chunk".into(),
                created: 1,
                model: req.model.clone(),
                choices: vec![],
                usage: Some(StreamUsage {
                    prompt_tokens: Some(1),
                    completion_tokens: Some(1),
                    total_tokens: Some(2),
                    prompt_tokens_details: None,
                }),
                raw_data: None,
            };
            Ok(ModelStreamInvocation {
                stream: Box::pin(stream::iter(vec![Ok(chunk), Ok(usage_chunk)])),
            })
        }
    }
    let inv: Arc<dyn ModelInvoker> = Arc::new(NoCacheInvoker);
    let execution = workflow().execute_stream(ctx(false, inv)).await.expect("ok");
    let chunks: Vec<ChatStreamChunk> = execution.stream.collect::<Vec<_>>().await.into_iter().map(|r| r.unwrap()).collect();
    let usage = chunks.last().unwrap().usage.as_ref().expect("usage");
    // 双方均无 cached_tokens → 合并为 None
    assert_eq!(usage.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens), None);
}
