//! DT 用例 — boom-routing::fusion：Fusion 虚拟 Provider 编排全链路。
//!
//! 覆盖（全部走公开 API，boom-routing 作为依赖编译时其 #[cfg(test)] 不参与）：
//! - register_fusion_providers：正常注册 / 与已有 deployment 冲突 / 与 alias 冲突 /
//!   子模型 provider 协议非 OpenAI 兼容拒绝 / 子 deployment 缺失放行（运行期才报）/
//!   独占候选集（add_deployment 失败）
//! - FusionProvider：无 context 的 chat/chat_stream → UnsupportedMode；
//!   name/models/protocol/create_prompt_trace 访问器
//! - chat_with_context 全链路：panel×2 + aggregator、usage/成本归账（billing）、
//!   FusionPromptTrace 快照（role/status/request/response）、子调用剥离 metadata、
//!   子调用注入 X-Gateway-Priority + X-BooM-Client-Type、父 key 亲和再入路由
//! - chat_stream_with_context：lazy（不 poll 不发起）→ collect 后三段调用、
//!   流式 trace（stream=true / event_count）、aggregator 流启动失败 → 回退 panel 流、
//!   流中错误 chunk 透传（GuardedFusionStream poll Err）
//! - aggregator 失败 → panel0 内容回退；panel 全部无效 → 报错但计费照记
//! - 子模型缺失：通配 "*" 不接管 Fusion 子调用（运行期 502）
//! - 递归别名：子模型经 alias 解析到 FusionProvider → ConfigError
//! - flow control：队列超时（FlowControlQueueTimeout）、上下文超限
//!   （RateLimitExceeded flow_control_context）、未配置 slot（NoSlot → 无 guard 直过）
//! - Router 弱引用失效 → "fusion routing runtime is unavailable"
//! - kv_index：KVC 感知选择 + 请求前缀自学习写入 trie
//! - build_gateway_headers：priority 开关 / vip 值 / client_type 分类
//! - 非 FusionPromptTrace 的外来 trace → 各 helper 静默跳过

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
use boom_flowcontrol::{FlowControlConfig, FlowController};
use boom_kvindex::TokenPrefixIndex;
use boom_routing::fusion::{build_gateway_headers, register_fusion_providers, FusionRuntime};
use boom_routing::{
    AliasStore, DeploymentStore, InFlightTracker, ModelCostRate, RequestRateTracker, Router,
    SchedulePolicy,
};
use futures::StreamExt;
use serde_json::json;

// ───────────────────────── 测试替身 ─────────────────────────

/// 记录 key_hash 的调度策略（默认取首候选）。
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
        self.key_hashes.lock().unwrap().push(key_hash.map(str::to_string));
        candidates.first().cloned()
    }

    fn name(&self) -> &str {
        "recording"
    }
}

/// 可配置的假 provider：失败模型 / 无效模型 / 流启动失败 / 流中错误 chunk。
struct FakeProvider {
    calls: Arc<Mutex<Vec<ChatCompletionRequest>>>,
    fail_models: Arc<Mutex<HashSet<String>>>,
    invalid_models: Arc<Mutex<HashSet<String>>>,
    stream_error_models: Arc<Mutex<HashSet<String>>>,
    error_chunk_models: Arc<Mutex<HashSet<String>>>,
    models: Vec<String>,
    protocol: ProviderProtocol,
    kv: Option<String>,
    deployment: String,
}

impl FakeProvider {
    #[allow(clippy::too_many_arguments)]
    fn new(
        calls: Arc<Mutex<Vec<ChatCompletionRequest>>>,
        fail_models: Arc<Mutex<HashSet<String>>>,
        invalid_models: Arc<Mutex<HashSet<String>>>,
        stream_error_models: Arc<Mutex<HashSet<String>>>,
        error_chunk_models: Arc<Mutex<HashSet<String>>>,
        models: &[&str],
    ) -> Self {
        Self {
            calls,
            fail_models,
            invalid_models,
            stream_error_models,
            error_chunk_models,
            models: models.iter().map(|m| m.to_string()).collect(),
            protocol: ProviderProtocol::OpenAiCompatible,
            kv: None,
            deployment: "fake-deployment".to_string(),
        }
    }

    fn with_kv(mut self, kv: &str) -> Self {
        self.kv = Some(kv.to_string());
        self
    }

    fn with_native_protocol(mut self) -> Self {
        self.protocol = ProviderProtocol::Native;
        self
    }

    fn with_deployment(mut self, dep: &str) -> Self {
        self.deployment = dep.to_string();
        self
    }
}

fn text_response(model: &str, content: String) -> ChatCompletionResponse {
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
                tool_calls: None,
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
            ..Usage::default()
        }),
        system_fingerprint: None,
        raw_response: None,
    }
}

fn text_chunk(model: &str, content: &str, with_usage: bool) -> ChatStreamChunk {
    ChatStreamChunk {
        id: format!("chatcmpl-stream-{}", model),
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
        usage: if with_usage {
            Some(StreamUsage {
                prompt_tokens: Some(2),
                completion_tokens: Some(1),
                total_tokens: Some(3),
                prompt_tokens_details: None,
            })
        } else {
            None
        },
        raw_data: None,
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn chat(&self, request: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
        self.calls.lock().unwrap().push(request.clone());
        if self.fail_models.lock().unwrap().contains(&request.model) {
            return Err(GatewayError::ProviderError(format!("{} unavailable", request.model)));
        }
        let content = if self.invalid_models.lock().unwrap().contains(&request.model) {
            String::new()
        } else {
            format!("answer from {}", request.model)
        };
        Ok(text_response(&request.model, content))
    }

    async fn chat_stream(&self, request: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
        self.calls.lock().unwrap().push(request.clone());
        if self.stream_error_models.lock().unwrap().contains(&request.model) {
            return Err(GatewayError::UpstreamError {
                status: 502,
                message: "stream start failed".to_string(),
            });
        }
        if self.error_chunk_models.lock().unwrap().contains(&request.model) {
            return Ok(Box::pin(futures::stream::iter([
                Ok(text_chunk(&request.model, "partial", false)),
                Err(GatewayError::ProviderError("mid-stream failure".to_string())),
            ])));
        }
        Ok(Box::pin(futures::stream::iter([Ok(text_chunk(
            &request.model,
            "streamed answer",
            true,
        ))])))
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
        Some(&self.deployment)
    }

    fn kv_worker_id(&self) -> Option<&str> {
        self.kv.as_deref()
    }

    fn client_type_header(&self) -> bool {
        true
    }
}

/// 仅供 KVC 验证选择用的静态探测 provider（chat 不会被调用）。
struct KvProbe {
    deployment: String,
    kv: String,
}

#[async_trait]
impl Provider for KvProbe {
    async fn chat(&self, _r: ChatCompletionRequest) -> Result<ChatCompletionResponse, GatewayError> {
        Err(GatewayError::ProviderError("probe".into()))
    }
    async fn chat_stream(&self, _r: ChatCompletionRequest) -> Result<ChatStream, GatewayError> {
        Err(GatewayError::ProviderError("probe".into()))
    }
    fn name(&self) -> &str {
        "probe"
    }
    fn models(&self) -> &[String] {
        &[]
    }
    fn deployment_id(&self) -> Option<&str> {
        Some(&self.deployment)
    }
    fn kv_worker_id(&self) -> Option<&str> {
        Some(&self.kv)
    }
}

/// 外来 prompt trace（非 FusionPromptTrace）→ downcast 失败各 helper 静默跳过。
struct ForeignTrace;
impl ProviderPromptTrace for ForeignTrace {
    fn snapshot(&self) -> Option<serde_json::Value> {
        Some(json!({"kind": "foreign"}))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ───────────────────────── fixture ─────────────────────────

struct Fixture {
    router: Arc<Router>,
    deployment_store: Arc<DeploymentStore>,
    alias_store: Arc<AliasStore>,
    flow: Arc<FlowController>,
    runtime: FusionRuntime,
    calls: Arc<Mutex<Vec<ChatCompletionRequest>>>,
    fail_models: Arc<Mutex<HashSet<String>>>,
    invalid_models: Arc<Mutex<HashSet<String>>>,
    stream_error_models: Arc<Mutex<HashSet<String>>>,
    error_chunk_models: Arc<Mutex<HashSet<String>>>,
    key_hashes: Arc<Mutex<Vec<Option<String>>>>,
}

const STANDARD_YAML: &str = r#"
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
"#;

fn parse_config(yaml: &str) -> Config {
    let config: Config = serde_yaml::from_str(yaml).expect("yaml parses");
    config.validate().expect("config validates");
    config
}

fn fixture() -> Fixture {
    fixture_with(STANDARD_YAML, None, None, true, 1200)
}

#[allow(clippy::too_many_arguments)]
fn fixture_with(
    yaml: &str,
    policy: Option<Arc<dyn SchedulePolicy>>,
    kv_index: Option<Arc<TokenPrefixIndex>>,
    enable_priority_header: bool,
    queue_timeout_secs: u64,
) -> Fixture {
    let config = parse_config(yaml);
    let deployment_store = Arc::new(DeploymentStore::new());
    let calls: Arc<Mutex<Vec<ChatCompletionRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let fail_models: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let invalid_models: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let stream_error_models: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let error_chunk_models: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let fake: Arc<dyn Provider> = Arc::new(
        FakeProvider::new(
            calls.clone(),
            fail_models.clone(),
            invalid_models.clone(),
            stream_error_models.clone(),
            error_chunk_models.clone(),
            &["panel-a", "panel-b", "aggregator"],
        )
        .with_kv("fake-deployment"),
    );
    for model in ["panel-a", "panel-b", "aggregator"] {
        deployment_store.add_deployment(model, fake.clone());
    }
    deployment_store.set_cost_rate("panel-a", ModelCostRate::new(1.into(), 10.into()));
    deployment_store.set_cost_rate("panel-b", ModelCostRate::new(2.into(), 20.into()));
    deployment_store.set_cost_rate("aggregator", ModelCostRate::new(3.into(), 30.into()));

    let key_hashes: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let alias_store = Arc::new(AliasStore::new());
    let router = Arc::new(Router::new(
        deployment_store.clone(),
        alias_store.clone(),
        policy.unwrap_or_else(|| {
            Arc::new(RecordingPolicy {
                key_hashes: key_hashes.clone(),
            })
        }),
    ));
    let flow = Arc::new(FlowController::new());
    let kv_swap: Arc<ArcSwap<Option<Arc<dyn boom_core::kv_event::KvIndexBackend>>>> =
        Arc::new(ArcSwap::from_pointee(None));
    if let Some(index) = kv_index {
        kv_swap.store(Arc::new(Some(index as Arc<dyn boom_core::kv_event::KvIndexBackend>)));
    }
    let runtime = FusionRuntime::new(
        Arc::downgrade(&router),
        deployment_store.clone(),
        flow.clone(),
        Arc::new(InFlightTracker::new()),
        Arc::new(RequestRateTracker::new()),
        kv_swap,
        enable_priority_header,
        queue_timeout_secs,
    );
    register_fusion_providers(&config.workflow_settings, &deployment_store, &alias_store, runtime.clone())
        .expect("register fusion providers");

    Fixture {
        router,
        deployment_store,
        alias_store,
        flow,
        runtime,
        calls,
        fail_models,
        invalid_models,
        stream_error_models,
        error_chunk_models,
        key_hashes,
    }
}

fn fusion_provider(f: &Fixture) -> Arc<dyn Provider> {
    f.router
        .select_provider_with_prefix("fusion", Some("parent-key"), 4, &[])
        .expect("fusion provider registered")
        .provider
}

fn request(model: &str, user_text: &str) -> ChatCompletionRequest {
    serde_json::from_value(json!({
        "model": model,
        "messages": [{"role": "user", "content": user_text}]
    }))
    .expect("request parses")
}

fn ctx(billing: ProviderBilling, prompt_trace: Option<SharedProviderPromptTrace>) -> ProviderCallContext {
    ProviderCallContext {
        key_hash: "parent-key".to_string(),
        key_alias: Some("parent-alias".to_string()),
        is_vip: true,
        api_path: "/v1/chat/completions".to_string(),
        billing,
        prompt_trace,
    }
}

fn vip_ctx() -> ProviderCallContext {
    ctx(ProviderBilling::default(), None)
}

// ═════════════════════════════════════════════════════════════════
// Provider 表面 / 注册约束
// ═════════════════════════════════════════════════════════════════

/// DT-RF-01：FusionProvider 无 context 的 chat/chat_stream → UnsupportedMode；
/// 访问器 name/models/protocol/deployment_id/create_prompt_trace。
#[tokio::test]
async fn fusion_provider_requires_context() {
    let f = fixture();
    let fusion = fusion_provider(&f);
    assert_eq!(fusion.name(), "fusion");
    assert_eq!(fusion.models(), &["fusion".to_string()]);
    assert_eq!(fusion.protocol(), ProviderProtocol::Native);
    assert_eq!(fusion.deployment_id(), None);
    assert!(fusion.create_prompt_trace().is_some());
    assert_eq!(fusion.custom_headers().len(), 0);

    let err = fusion.chat(request("fusion", "hi")).await.err().expect("context required");
    assert!(matches!(&err, GatewayError::UnsupportedMode(m) if m.contains("/v1/chat/completions")), "{err:?}");
    let err = fusion.chat_stream(request("fusion", "hi")).await.err().expect("context required");
    assert!(matches!(&err, GatewayError::UnsupportedMode(m) if m.contains("/v1/chat/completions")), "{err:?}");
}

/// DT-RF-02：注册后独占候选集——普通 add_deployment 失败，contains 为真。
#[test]
fn fusion_registration_is_exclusive() {
    let f = fixture();
    assert!(f.deployment_store.contains("fusion"));
    let probe: Arc<dyn Provider> = Arc::new(KvProbe {
        deployment: "probe".into(),
        kv: "probe".into(),
    });
    assert!(!f.deployment_store.add_deployment("fusion", probe));
}

/// DT-RF-03：workflow model 名与运行时已有 deployment 冲突 → ConfigError。
/// （"fusion" 已由 fixture 注册为 fusion deployment；重复注册同名被拒。
/// 注意 config.validate 只查 config 内部的 model_list/alias 冲突，查不到运行时 store。）
#[test]
fn registration_rejects_deployment_conflict() {
    let f = fixture();
    let config = parse_config(STANDARD_YAML);
    let err = register_fusion_providers(&config.workflow_settings, &f.deployment_store, &f.alias_store, f.runtime.clone())
        .err()
        .expect("conflict rejected");
    assert!(matches!(err, GatewayError::ConfigError(_)));
    assert!(err.to_string().contains("conflicts with an existing deployment"), "{}", err);
}

/// DT-RF-04：workflow model 名与已有 alias 冲突 → ConfigError。
#[test]
fn registration_rejects_alias_conflict() {
    let f = fixture();
    // 新 workflow model 名 "fusion2"：不在 store、config 内部合法；
    // 预先设置 alias "fusion2" → 撞上注册检查
    f.alias_store.set_alias("fusion2".to_string(), "panel-a".to_string(), false);
    let config = parse_config(
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
    fusion2: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - model: panel-a
          - model: panel-b
        aggregator:
          model: aggregator
"#,
    );
    let err = register_fusion_providers(&config.workflow_settings, &f.deployment_store, &f.alias_store, f.runtime.clone())
        .err()
        .expect("alias conflict rejected");
    assert!(err.to_string().contains("conflicts with an existing alias"), "{}", err);
}

/// DT-RF-05：子模型 provider 协议非 OpenAI 兼容 → 注册期拒绝（含 alias 解析路径）。
#[test]
fn registration_rejects_native_protocol_children() {
    let deployment_store = Arc::new(DeploymentStore::new());
    let calls: Arc<Mutex<Vec<ChatCompletionRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let empty_sets = || Arc::new(Mutex::new(HashSet::<String>::new()));
    let compatible: Arc<dyn Provider> = Arc::new(
        FakeProvider::new(calls.clone(), empty_sets(), empty_sets(), empty_sets(), empty_sets(), &["aggregator"]),
    );
    let incompatible: Arc<dyn Provider> = Arc::new(
        FakeProvider::new(calls, empty_sets(), empty_sets(), empty_sets(), empty_sets(), &["panel-target"])
            .with_native_protocol(),
    );
    deployment_store.add_deployment("panel-target", compatible.clone());
    deployment_store.add_deployment("panel-target", incompatible);
    deployment_store.add_deployment("aggregator", compatible);

    let alias_store = Arc::new(AliasStore::new());
    alias_store.set_alias("panel-alias".to_string(), "panel-target".to_string(), false);
    let key_hashes: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let router = Arc::new(Router::new(
        deployment_store.clone(),
        alias_store.clone(),
        Arc::new(RecordingPolicy { key_hashes }),
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
    let config = parse_config(
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
    );
    let err = register_fusion_providers(&config.workflow_settings, &deployment_store, &alias_store, runtime)
        .err()
        .expect("native protocol rejected");
    let msg = err.to_string();
    assert!(msg.contains("panel model 'panel-alias'"), "{msg}");
    assert!(msg.contains("provider 'fake'"), "{msg}");
    assert!(msg.contains("OpenAI-compatible provider"), "{msg}");
}

// ═════════════════════════════════════════════════════════════════
// chat_with_context 全链路
// ═════════════════════════════════════════════════════════════════

/// DT-RF-06：非流式全链路——usage/成本归账、trace 快照、子调用头与 metadata 剥离、
/// 父 key 亲和再入路由。
#[tokio::test]
async fn fusion_chat_full_pipeline() {
    let f = fixture();
    let fusion = fusion_provider(&f);
    let billing = ProviderBilling::default();
    let prompt_trace = fusion.create_prompt_trace().unwrap();
    let mut req = request("fusion", "solve it");
    req.extra.insert("metadata".to_string(), json!({"run_id": "dt"}));

    let result = fusion
        .chat_with_context(req, ctx(billing.clone(), Some(prompt_trace.clone())))
        .await
        .expect("fusion chat ok");

    assert_eq!(result.usage.clone().unwrap().total_tokens, 9);
    assert_eq!(billing.actual_usage().unwrap().total_tokens, 9);
    let cost = billing.actual_cost().unwrap();
    assert_eq!(cost.regular_input, 12.into());
    assert_eq!(cost.cached_input, 0.into());
    assert_eq!(cost.output, 60.into());
    assert_eq!(cost.total(), 72.into());

    let snapshot = prompt_trace.snapshot().unwrap();
    let calls = snapshot["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["role"], "panel");
    assert_eq!(calls[1]["role"], "panel");
    assert_eq!(calls[2]["role"], "aggregator");
    assert!(calls.iter().all(|c| c["status"] == "succeeded"));
    assert!(calls.iter().all(|c| c.get("request").is_some() && c.get("response").is_some()));

    // 父请求 1 次 + 子调用 3 次都带父 key
    let routed = f.key_hashes.lock().unwrap().clone();
    assert_eq!(routed.len(), 4);
    assert!(routed.iter().all(|k| k.as_deref() == Some("parent-key")));

    let child_calls = f.calls.lock().unwrap();
    assert_eq!(child_calls.len(), 3);
    assert!(child_calls.iter().all(|c| !c.extra.contains_key("metadata")));
    assert!(child_calls.iter().all(|c| {
        c.gateway_headers.get("X-Gateway-Priority").is_some_and(|v| v == "100")
    }));
    assert!(child_calls.iter().all(|c| {
        c.gateway_headers
            .get(boom_ctxaware::CLIENT_TYPE_HEADER)
            .is_some_and(|v| v == "anonymous")
    }));
}

/// DT-RF-07：非 VIP + priority 开 → X-Gateway-Priority 值为 "0"（降级不缺席）。
#[tokio::test]
async fn fusion_chat_non_vip_gets_zero_priority() {
    let f = fixture();
    let fusion = fusion_provider(&f);
    let mut c = vip_ctx();
    c.is_vip = false;
    let result = fusion.chat_with_context(request("fusion", "solve it"), c).await.expect("ok");
    assert_eq!(result.usage.unwrap().total_tokens, 9);
    let child_calls = f.calls.lock().unwrap();
    assert!(child_calls.iter().all(|c| {
        c.gateway_headers.get("X-Gateway-Priority").is_some_and(|v| v == "0")
    }));
    assert!(child_calls.iter().all(|c| {
        c.gateway_headers.contains_key(boom_ctxaware::CLIENT_TYPE_HEADER)
    }));
}

/// DT-RF-08：外来 prompt trace（非 FusionPromptTrace）→ 调用链静默跳过 trace 记录。
#[tokio::test]
async fn fusion_chat_with_foreign_prompt_trace() {
    let f = fixture();
    let fusion = fusion_provider(&f);
    let mut c = vip_ctx();
    c.prompt_trace = Some(Arc::new(ForeignTrace));
    let result = fusion.chat_with_context(request("fusion", "solve it"), c).await.expect("ok");
    assert_eq!(result.usage.unwrap().total_tokens, 9);
}

// ═════════════════════════════════════════════════════════════════
// 流式路径
// ═════════════════════════════════════════════════════════════════

/// DT-RF-09：流式——SSE 建立前不发起子调用（lazy），poll 后三段调用 + 归账 + trace。
#[tokio::test]
async fn fusion_stream_lazy_then_full_pipeline() {
    let f = fixture();
    let fusion = fusion_provider(&f);
    let billing = ProviderBilling::default();
    let prompt_trace = fusion.create_prompt_trace().unwrap();
    let before = f.calls.lock().unwrap().len();
    let stream = fusion
        .chat_stream_with_context(request("fusion", "stream it"), ctx(billing.clone(), Some(prompt_trace.clone())))
        .await
        .expect("stream starts");
    assert_eq!(f.calls.lock().unwrap().len(), before, "workflow must stay lazy before first poll");

    let chunks: Vec<_> = stream.collect().await;
    assert!(chunks.iter().all(Result::is_ok), "{chunks:?}");
    assert!(chunks.iter().any(|c| {
        c.as_ref().map(|c| c.choices.iter().any(|s| s.delta.content.as_deref() == Some("streamed answer")))
            .unwrap_or(false)
    }));
    assert_eq!(f.calls.lock().unwrap().len(), before + 3);
    assert_eq!(billing.actual_usage().unwrap().total_tokens, 9);
    assert_eq!(billing.actual_cost().unwrap().total(), 72.into());

    let snapshot = prompt_trace.snapshot().unwrap();
    let calls = snapshot["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2]["role"], "aggregator");
    assert_eq!(calls[2]["status"], "succeeded");
    assert_eq!(calls[2]["stream"], true);
    assert!(calls[2]["response"]["event_count"].as_u64().is_some_and(|n| n > 0));
}

/// DT-RF-10：aggregator 流启动失败（非 ModelNotFound）→ 回退为 panel0 的响应流。
#[tokio::test]
async fn fusion_stream_aggregator_start_error_falls_back_to_panel() {
    let f = fixture();
    f.stream_error_models.lock().unwrap().insert("aggregator".to_string());
    let fusion = fusion_provider(&f);
    let billing = ProviderBilling::default();
    let stream = fusion
        .chat_stream_with_context(request("fusion", "fallback"), ctx(billing.clone(), None))
        .await
        .expect("stream starts");
    let chunks: Vec<_> = stream.collect().await;
    assert!(chunks.iter().all(Result::is_ok));
    // 回退内容来自 panel0
    assert!(chunks.iter().any(|c| {
        c.as_ref()
            .map(|c| c.choices.iter().any(|s| s.delta.content.as_deref() == Some("answer from panel-a")))
            .unwrap_or(false)
    }));
}

/// DT-RF-11：流中错误 chunk → GuardedFusionStream 透传 Err（trace 记 failed）。
#[tokio::test]
async fn fusion_stream_mid_stream_error_propagates() {
    let f = fixture();
    f.error_chunk_models.lock().unwrap().insert("aggregator".to_string());
    let fusion = fusion_provider(&f);
    let billing = ProviderBilling::default();
    let prompt_trace = fusion.create_prompt_trace().unwrap();
    let stream = fusion
        .chat_stream_with_context(request("fusion", "err"), ctx(billing.clone(), Some(prompt_trace.clone())))
        .await
        .expect("stream starts");
    let chunks: Vec<_> = stream.collect().await;
    assert!(chunks.iter().any(Result::is_err), "mid-stream error must surface");

    let snapshot = prompt_trace.snapshot().unwrap();
    let calls = snapshot["calls"].as_array().unwrap();
    let aggregator = calls.iter().find(|c| c["role"] == "aggregator").expect("aggregator call");
    assert_eq!(aggregator["status"], "failed");
    assert!(aggregator.get("error").is_some());
}

/// DT-RF-12：流中途 Drop（未消费完）→ prompt trace 终结为 cancelled/未完成。
#[tokio::test]
async fn fusion_stream_dropped_midway_marks_unfinished() {
    let f = fixture();
    f.error_chunk_models.lock().unwrap().insert("aggregator".to_string());
    let fusion = fusion_provider(&f);
    let prompt_trace = fusion.create_prompt_trace().unwrap();
    let mut stream = fusion
        .chat_stream_with_context(request("fusion", "drop"), ctx(ProviderBilling::default(), Some(prompt_trace.clone())))
        .await
        .expect("stream starts");
    // 只 poll 第一个 chunk 然后 Drop GuardedFusionStream
    let _first = stream.next().await;
    drop(stream);
    prompt_trace.finalize();
    let snapshot = prompt_trace.snapshot().unwrap();
    let calls = snapshot["calls"].as_array().unwrap();
    let aggregator = calls.iter().find(|c| c["role"] == "aggregator").expect("aggregator call");
    assert_eq!(aggregator["status"], "cancelled");
}

// ═════════════════════════════════════════════════════════════════
// 失败语义
// ═════════════════════════════════════════════════════════════════

/// DT-RF-13：aggregator 失败 → 回退 panel0 内容，usage=panel 之和，计费照记。
#[tokio::test]
async fn fusion_aggregator_failure_falls_back_to_panel() {
    let f = fixture();
    f.fail_models.lock().unwrap().insert("aggregator".to_string());
    let fusion = fusion_provider(&f);
    let billing = ProviderBilling::default();
    let result = fusion
        .chat_with_context(request("fusion", "fall back"), ctx(billing.clone(), None))
        .await
        .expect("panel fallback ok");
    let content = match &result.choices[0].message.content {
        MessageContent::Text(t) => t.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(content, "answer from panel-a");
    assert_eq!(result.usage.unwrap().total_tokens, 6);
    assert_eq!(billing.actual_usage().unwrap().total_tokens, 6);
    assert_eq!(billing.actual_cost().unwrap().total(), 36.into());
}

/// DT-RF-14：panel 全部无效（空响应）→ 报错；已产生调用照常归账。
#[tokio::test]
async fn fusion_invalid_panels_error_but_bill_usage() {
    let f = fixture();
    let mut invalid = f.invalid_models.lock().unwrap();
    invalid.insert("panel-a".to_string());
    invalid.insert("panel-b".to_string());
    drop(invalid);
    let fusion = fusion_provider(&f);
    let billing = ProviderBilling::default();
    let result = fusion
        .chat_with_context(request("fusion", "invalid panels"), ctx(billing.clone(), None))
        .await;
    assert!(result.is_err());
    assert_eq!(billing.actual_usage().unwrap().total_tokens, 12);
    assert_eq!(billing.actual_cost().unwrap().total(), 72.into());
}

/// DT-RF-15：子模型无 deployment（aggregator 下线）→ 运行期 502；通配 "*" 不接管。
#[tokio::test]
async fn fusion_missing_child_fails_at_runtime_wildcard_does_not_take_over() {
    // aggregator 不注册 deployment；通配 "*" 指向 panel provider
    let config = parse_config(
        r#"
model_list:
  - model_name: panel
    litellm_params:
      model: openai/panel
  - model_name: aggregator
    enabled: false
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
          - model: panel
          - model: panel
        aggregator:
          model: aggregator
"#,
    );
    let deployment_store = Arc::new(DeploymentStore::new());
    let calls: Arc<Mutex<Vec<ChatCompletionRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let empty_sets = || Arc::new(Mutex::new(HashSet::<String>::new()));
    let panel: Arc<dyn Provider> =
        Arc::new(FakeProvider::new(calls.clone(), empty_sets(), empty_sets(), empty_sets(), empty_sets(), &["panel"]));
    deployment_store.add_deployment("panel", panel.clone());
    deployment_store.add_deployment("*", panel);
    let alias_store = Arc::new(AliasStore::new());
    let key_hashes: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let router = Arc::new(Router::new(
        deployment_store.clone(),
        alias_store.clone(),
        Arc::new(RecordingPolicy { key_hashes }),
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
    register_fusion_providers(&config.workflow_settings, &deployment_store, &alias_store, runtime)
        .expect("registration must not require online children");

    let fusion = router
        .select_provider_with_prefix("fusion", Some("parent-key"), 4, &[])
        .expect("fusion provider present")
        .provider;
    let err = fusion.chat_with_context(request("fusion", "q"), vip_ctx()).await.err().expect("must fail");
    assert_eq!(err.status_code(), 502);
    let msg = err.to_string();
    assert!(msg.contains("failed at aggregator stage"), "{msg}");
    assert!(msg.contains("Model not found: aggregator"), "{msg}");
    assert!(calls.lock().unwrap().iter().all(|r| r.model == "panel"), "wildcard must not serve fusion children");
}

/// DT-RF-16：子模型经 alias 解析到 FusionProvider（递归）→ ConfigError。
#[tokio::test]
async fn fusion_child_alias_resolving_to_fusion_rejected() {
    // panel 模型 "pa"/"pb" 不注册 deployment；注册后再把 "pa" 别名到 "fusion"
    let config = parse_config(
        r#"
model_list:
  - model_name: pa
    litellm_params:
      model: openai/pa
  - model_name: pb
    litellm_params:
      model: openai/pb
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
          - model: pa
          - model: pb
        aggregator:
          model: aggregator
"#,
    );
    let deployment_store = Arc::new(DeploymentStore::new());
    let calls: Arc<Mutex<Vec<ChatCompletionRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let empty_sets = || Arc::new(Mutex::new(HashSet::<String>::new()));
    let aggregator: Arc<dyn Provider> =
        Arc::new(FakeProvider::new(calls, empty_sets(), empty_sets(), empty_sets(), empty_sets(), &["aggregator"]));
    deployment_store.add_deployment("aggregator", aggregator);
    let alias_store = Arc::new(AliasStore::new());
    let key_hashes: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let router = Arc::new(Router::new(
        deployment_store.clone(),
        alias_store.clone(),
        Arc::new(RecordingPolicy { key_hashes }),
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
    register_fusion_providers(&config.workflow_settings, &deployment_store, &alias_store, runtime)
        .expect("registration tolerates offline panels");

    alias_store.set_alias("pa".to_string(), "fusion".to_string(), false);
    let fusion = router
        .select_provider_with_prefix("fusion", Some("parent-key"), 4, &[])
        .expect("fusion provider present")
        .provider;
    let err = fusion.chat_with_context(request("fusion", "q"), vip_ctx()).await.err().expect("must fail");
    let msg = err.to_string();
    assert!(msg.contains("resolves to virtual provider"), "{msg}");
}

// ═════════════════════════════════════════════════════════════════
// flow control / router 生命周期
// ═════════════════════════════════════════════════════════════════

/// DT-RF-17：队列超时——占满 inflight + queue_timeout=0 → FlowControlQueueTimeout。
#[tokio::test]
async fn fusion_child_flow_control_queue_timeout() {
    let f = fixture_with(STANDARD_YAML, None, None, true, 0);
    f.flow.ensure_slot(
        "fake-deployment",
        &FlowControlConfig { max_inflight: 1, max_context: 0 },
    );
    // 占住唯一 inflight 名额并保持到断言结束
    let _held = f
        .flow
        .acquire("fake-deployment", 10, Duration::from_secs(30), false, None, None, None)
        .await
        .expect("occupy slot");

    let fusion = fusion_provider(&f);
    let err = fusion.chat_with_context(request("fusion", "q"), vip_ctx()).await.err().expect("must fail");
    // panel 阶段把子调用错误包装成 workflow 失败，type 标记 flow_control_timeout
    let msg = err.to_string();
    assert!(msg.contains("flow_control_timeout"), "{msg}");
    assert!(msg.contains("fusion child call queue timeout"), "{msg}");
}

/// DT-RF-18：上下文超限——max_context=100，输入超长 → RateLimitExceeded(flow_control_context)。
#[tokio::test]
async fn fusion_child_context_exceeded() {
    let f = fixture();
    f.flow.ensure_slot(
        "fake-deployment",
        &FlowControlConfig { max_inflight: 100, max_context: 100 },
    );
    let fusion = fusion_provider(&f);
    let long_input = "x".repeat(300);
    let err = fusion
        .chat_with_context(request("fusion", &long_input), vip_ctx())
        .await
        .err()
        .expect("must fail");
    // panel 阶段把子调用错误包装成 workflow 失败，type 标记 flow_control_context
    let msg = err.to_string();
    assert!(msg.contains("flow_control_context"), "{msg}");
    assert!(msg.contains("exceeds deployment max_context"), "{msg}");
}

/// DT-RF-19：Router 强引用全部释放 → 弱引用升级失败 → "unavailable"。
#[tokio::test]
async fn fusion_router_dropped_reports_unavailable() {
    let f = fixture();
    let fusion = fusion_provider(&f);
    let Fixture { router, .. } = f;
    drop(router);
    let err = fusion.chat_with_context(request("fusion", "q"), vip_ctx()).await.err().expect("must fail");
    let msg = err.to_string();
    assert!(msg.contains("fusion routing runtime is unavailable"), "{msg}");
}

// ═════════════════════════════════════════════════════════════════
// kv_index 自学习 / build_gateway_headers
// ═════════════════════════════════════════════════════════════════

/// DT-RF-20：kv_index 挂载 + KVC 感知策略 → 子调用前缀写入 trie，后续同前缀命中。
#[tokio::test]
async fn fusion_records_request_prefix_into_kv_index() {
    // 块大小 8B：消息序列化远超一块，保证 record/查询都产生完整块
    let index = Arc::new(TokenPrefixIndex::new(8, 500_000));
    let tracker = Arc::new(InFlightTracker::new());
    let policy: Arc<dyn SchedulePolicy> = Arc::new(boom_routing::KvcAwarePolicy::new(index.clone(), tracker, None));
    let f = fixture_with(STANDARD_YAML, Some(policy), Some(index.clone()), true, 1200);
    // KVC 策略对单候选跳过查询（kv_match_attempted=false → 不记录），
    // 给 panel-a 加第二个同 kv worker 的候选使记录路径生效
    let panel_extra: Arc<dyn Provider> = Arc::new(
        FakeProvider::new(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(HashSet::new())),
            &["panel-a"],
        )
        .with_kv("fake-deployment")
        .with_deployment("fake-deployment"),
    );
    f.deployment_store.add_deployment("panel-a", panel_extra);
    let fusion = fusion_provider(&f);
    let result = fusion.chat_with_context(request("fusion", "solve it"), vip_ctx()).await.expect("ok");
    assert_eq!(result.usage.unwrap().total_tokens, 9);

    // 子调用（panel-a，消息同父请求）的前缀已被记录到选中 worker（fake-deployment）下；
    // 用同前缀查询 → 命中记录过的 worker 而非另一个。
    let messages = vec![Message {
        role: MessageRole::User,
        content: MessageContent::Text("solve it".to_string()),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    }];
    let prefix = serde_json::to_vec(&messages).expect("prefix bytes");
    let probe_hit: Arc<dyn Provider> = Arc::new(KvProbe { deployment: "d1".into(), kv: "fake-deployment".into() });
    let probe_miss: Arc<dyn Provider> = Arc::new(KvProbe { deployment: "d2".into(), kv: "other-worker".into() });
    let cands = vec![probe_miss, probe_hit];
    let pol = boom_routing::KvcAwarePolicy::new(index, Arc::new(InFlightTracker::new()), None);
    let sel = pol.select_with_context("panel-a", &cands, None, 0, &prefix).expect("selection");
    assert!(sel.kv_match_attempted);
    assert!(sel.kv_hit_ratio > 0.0, "learned prefix must hit the recorded worker");
    assert_eq!(sel.provider.deployment_id(), Some("d1"));
}

/// DT-RF-21：build_gateway_headers——priority 开关与 VIP 值、client_type 分类、全关为空。
#[test]
fn gateway_headers_combinations() {
    let vip_on = build_gateway_headers(true, true, "/v1/chat/completions", true);
    assert_eq!(vip_on.get("X-Gateway-Priority").map(String::as_str), Some("100"));
    assert_eq!(
        vip_on.get(boom_ctxaware::CLIENT_TYPE_HEADER).map(String::as_str),
        Some("anonymous")
    );

    let non_vip = build_gateway_headers(false, true, "/v1/chat/completions", false);
    assert_eq!(non_vip.get("X-Gateway-Priority").map(String::as_str), Some("0"));
    assert!(!non_vip.contains_key(boom_ctxaware::CLIENT_TYPE_HEADER));

    let priority_off = build_gateway_headers(true, false, "/v1/chat/completions", true);
    assert!(!priority_off.contains_key("X-Gateway-Priority"));
    assert!(priority_off.contains_key(boom_ctxaware::CLIENT_TYPE_HEADER));

    // Anthropic 原生路径 → client_type 分类为 anthropic
    let anthropic = build_gateway_headers(false, false, "/v1/messages", true);
    assert_eq!(
        anthropic.get(boom_ctxaware::CLIENT_TYPE_HEADER).map(String::as_str),
        Some("anthropic")
    );

    assert!(build_gateway_headers(false, false, "/v1/x", false).is_empty());
}
