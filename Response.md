# Boom Gateway 支持 OpenAI Responses API（`/v1/responses`）设计方案

> 状态：设计稿（待评审）
> 日期：2026-10-20
> 参考：[vllm-project/agentic-api](https://github.com/vllm-project/agentic-api)（commit 浅克隆 2026-10）、本仓库 CLAUDE.md 架构原则

---

## 1. 背景与目标

OpenAI Responses API（`POST /v1/responses`）正在成为 agentic 客户端的事实入口：Codex CLI、OpenAI SDK 新版、以及大量 agent 框架默认走 Responses 协议。当前 Boom Gateway 只暴露 Chat Completions（`/v1/chat/completions`）、Completions、Anthropic Messages（`/v1/messages`）三类入口，走 Responses 协议的客户端无法接入。

**目标**：

1. 网关新增 `POST /v1/responses` 端点（流式 + 非流式），对客户端呈现 OpenAI Responses 协议语义。
2. 复用现有全部网关能力：litellm 兼容认证、Plan 限流/计费、模型路由（alias → deployment）、KV prefix 亲和、请求审计、trace、Dashboard。
3. 支持 Responses API 的有状态续写（`previous_response_id` + `store`），存储归网关所有。
4. 所有上游 Provider（OpenAI 兼容 / Anthropic / Gemini / Bedrock / Azure）立即可用，不要求上游支持 Responses 协议。

**Non-goals**（明确排除，见 §10）。

---

## 2. agentic-api 的处理方式拆解（分析结论）

agentic-api 是 vLLM 官方的有状态 agentic 网关（Rust），其 Responses 处理方式是本方案最重要的参照。核心结论：

### 2.1 定位与上游接口选择

- agentic-api 是**无状态推理引擎前面的有状态层**：状态、工具执行、协议规范化在网关，tokenization / chat template 留在上游（ADR-01 D2）。
- 上游接口选了 **vLLM 原生 `/v1/responses`**（ADR-01 D1），明确否决了"转 Chat Completions 再发上游"——因为它的上游只有 vLLM，vLLM 内部 Responses 本就直达 `generate()`。
- **这一前提与 Boom Gateway 不同**：我们的上游是 30+ 种异构 provider，内部规范格式是 Chat Completions（`Provider` trait 唯一入口是 `chat*/ChatCompletionRequest`）。因此"入口转换、内核复用"对我们是正确路线，agentic-api 的"原生 Responses 上游"只适用于其中一类 deployment（本方案 P2 处理，见 §4.3）。

### 2.2 执行 pipeline（五步，均为独立可组合的公共函数）

```
prepare（解析+工具归属分类）
  → rehydrate（previous_response_id / conversation → 完整历史）
  → upstream_request（构造上游 body：历史内联、continuation 字段剥离）
  → inference（POST 上游 /v1/responses，SSE 行流）
  → events::normalize（上游 SSE → typed EventFrame）
  → accumulator/translate（累积完整 Response、工具名翻译回公共命名空间）
  → [工具循环：gateway-owned 工具在网关执行 → 结果 append → 再打上游]
  → relay（客户端投递：公共 ID、output_index 重基、sequence_number 连续化）
  → persist（完成的 Response + items 写回 store）
```

### 2.3 事件模型（`events/types.rs` + `normalize.rs`）

- 上游每条 SSE `data:` 行规范化为 `EventFrame { event_type: SSEEventType, payload: EventPayload, wire: WireEvent }`——**typed payload（enum）+ 原始 wire JSON 并存**：typed 驱动状态累积，wire 保证未知字段不丢。
- `EventPayload` 覆盖：`Response`（created/completed/failed/incomplete 共用）、`OutputItemAdded/Done`、`TextDelta/Done`、`FunctionCallArgsDelta/Done`、`ReasoningTextDelta/Done`、`ReasoningSummaryTextDelta/Done`、`ContentPartDone` 等。
- 兼容性修正示例：vLLM 发 `response.completed` 但 `response.status == "incomplete"` 时，规范化阶段改判为 `response.incomplete`（上游 quirk 在入口统一抹平）。

### 2.4 存储模型（ADR-02，三表 + checkpoint）

```
conversations(id, item_ids[], metadata, ...)
responses(id, conversation_id?, previous_response_id?, history_item_ids[], ...)
items(id, conversation_id?, data JSONB, seq, ...)
```

- **热路径 rehydrate 不走链**：`history_item_ids` 是有序 item-ID checkpoint，一次读 response 行 + 一次 bulk fetch items + 应用层恢复顺序，避免 O(N²) 历史复制（每轮全量嵌套）和链式回溯（N 次 DB 往返）。
- `store: false` → 完全不落盘，id 既不可 `GET` 检索也不可续写。
- `instructions` **不自动继承**（每轮需重传）；`tools` 在本轮省略时可继承上轮；compaction 生成新 checkpoint 而不改历史行。
- SQLite 默认 / PostgreSQL 多实例。

### 2.5 客户端事件契约

- typed、**sequenced**（`sequence_number` 全流连续递增）、**stable item IDs**（`resp_` / `msg_` / `fc_` / `rs_` 前缀）。
- item 生命周期：`response.output_item.added` →（delta 若干）→ `*.done` → `response.output_item.done` → `response.completed`（终态事件携带完整 response 对象 + usage）。

### 2.6 值得移植的设计，与不移植的部分

| 移植 | 理由 |
|---|---|
| 输入/输出 item 的 typed enum 建模（`ResponsesInput::{Text,Items}`、`InputItem`/`OutputItem` tagged enum） | 这是协议的核心数据模型，直接决定转换层的类型设计 |
| `previous_response_id` 的 checkpoint 式存储 + bulk fetch rehydrate | 避免 O(N²) 与链式回溯，是被 ADR 论证过的中间路线 |
| SSE 事件序列契约（sequence_number 连续、item 生命周期、`[DONE]` 结尾） | 客户端（Codex 等）依赖此契约 |
| `store`/instructions/tools 的继承语义 | 与 OpenAI 实测行为对齐 |
| 上游 quirk 在规范化入口抹平 | 减少状态机分支污染 |
| **不移植**：工具执行循环（gateway-owned tools / MCP / code interpreter） | Boom Gateway 是路由/限流/计费网关，不是 agent 执行器（Non-goal） |
| **不移植**：Conversation API、multi-agent、WebSocket、compaction | 同上，P0/P1 均不需要 |

---

## 3. Boom Gateway 现状（协议处理链路）

内部事实标准：**Chat Completions 是内核规范格式**。所有协议差异在两个"边缘"消化：

```
入口边缘（协议 → 内核）                     出口边缘（内核 → 协议）
/v1/messages:  anthropic_request_to_openai   openai_response_to_anthropic
               (boom-core/src/anthropic.rs)  AnthropicStreamTranscoder
/v1/responses: 【本方案新增，同款模式】       【本方案新增】
```

关键现状（引用为设计依据）：

- `Provider` trait（`boom-core/src/provider.rs:193`）唯一请求入口是 `ChatCompletionRequest`；流式返回 `ChatStream`（`Pin<Box<dyn Stream<Item = Result<ChatStreamChunk>>> + Send>>`，`types.rs:565`），**所有 provider 的流都已被规范化为 OpenAI chunk 形状**。这意味着 Responses 出口转换器只需面对一种输入形状。
- `/v1/messages` handler（`boom-main/src/routes.rs:3057`）已验证"边缘转换"模式完整可行：请求转换 → 复用 `chat_completions_inner` 骨架（认证 → 模型权限 → 内容感知解析 → Plan 限流 → 路由选 deployment → flow control → provider 调用）→ 出口转换（非流式 `openai_response_to_anthropic`，流式 `AnthropicStreamTranscoder::transcode(&mut self, &ChatStreamChunk) -> Vec<SseEvent>` + `drain()`）。
- `ChatCompletionRequest.extra` 是 flatten catch-all（`types.rs:298`），未知字段原样透传 OpenAI 兼容上游 —— Responses 的 `reasoning.effort` 等可借道透传。
- 审计/计费在流包装链完成：`sse_stream_from_chat_stream`（routes.rs:3004）提取 usage 进 `UsageTracker`，`LoggedStream` Drop 时写 `RequestLog` + `PlanCharge::settle`。**转换层若保持"内部仍是 ChatStream、出口才转 Responses 事件"，这条链零改动。**
- `CachedJson` body 上限 2MB（extractor.rs:327）——Responses 请求带全量 history 时需关注（§6.6）。
- 全仓库无任何 `previous_response_id` / responses 存储设施，全新增量。

---

## 4. 方案决策

### 4.1 候选方案

| 方案 | 描述 | 评估 |
|---|---|---|
| **A. 边缘转换（推荐，P0/P1）** | `/v1/responses` 入口转 `ChatCompletionRequest`，走现有内核链路，出口转回 Responses 对象/SSE。照抄 `/v1/messages` 模式 | ✅ 全部 30+ provider 立即可用；限流/计费/审计/KV 亲和/trace 零改动；符合模块依赖原则。代价：Responses 专有概念（reasoning item 回放、stateless 直通字段）有映射损失 |
| B. 原生透传 | `ProviderProtocol` 加 `Responses` 变体，`OpenAIProvider` 直通 `/v1/responses` | ❌ 作为唯一方案不可行：Anthropic/Gemini/Bedrock 上游无 Responses 端点，`Provider` trait 需开第二入口，审计/usage 提取全部重做。✅ 作为 **P2 对 vLLM 类 deployment 的保真直通**（§7 P2） |
| C. 外挂 agentic-api 服务 | 网关把 `/v1/responses` 反代到独立部署的 agentic-api | ❌ 引入第二套认证/限流/计费体系，与网关自有 PlanStore/审计冲突；多一跳运维成本。agentic-api 的工具执行/Conversation API 我们不需要 |

### 4.2 决策

**方案 A 为 P0/P1 主体**：新增 `boom-core/src/responses.rs` 转换层（与 `anthropic.rs` 对称）+ `boom-main` handler + （P1）新 leaf crate `boom-responses` 做 response store。**方案 B 作为 P2 可选增强**，仅对显式声明 Responses 协议的 deployment 启用。

### 4.3 决策依据对照

- CLAUDE.md §1：转换层在 boom-core（leaf）✓；handler 在 boom-main（组装层）✓；无新跨模块依赖。
- CLAUDE.md §2/§3：`boom_response*` 表归新 crate `boom-responses` 所有，DDL 不越界。
- CLAUDE.md §6："handler 只做提取→调用→组装"，业务逻辑在转换层函数里，可单测。

---

## 5. 总体架构

### 5.1 数据流（P0 无状态）

```
客户端 (Codex/SDK)
  │ POST /v1/responses  {model, input, instructions, tools, stream, ...}
  ▼
boom-main routes.rs :: responses()                     【入口边缘】
  ├─ CachedJson<ResponsesRequest>                       （2MB 上限）
  ├─ RequiredAuth（litellm 密钥认证，与 chat 同源）
  ├─ [P1] boom_responses::rehydrate(previous_response_id, key_hash)
  │        → history: Vec<InOutItem>   （DB checkpoint → bulk fetch）
  ├─ boom_core::responses::responses_request_to_openai(req, history)
  │        → ChatCompletionRequest                     ── 进入内核规范格式
  ├─ ── 以下与 /v1/chat/completions 完全同构，直接复用 ──
  │    check_model_access → resolve_request_model → check_plan_limits
  │    → select_provider（KV prefix 亲和）→ flow_controller.acquire
  │    → provider.chat_stream_with_context(ChatCompletionRequest)
  │         → ChatStream（OpenAI chunk 形状，含 usage 尾包）
  │    流包装链照旧：sse_stream_from_chat_stream（UsageTracker）
  │                 → InFlight/FlowControlled/Guarded/LoggedStream（审计+settle）
  ▼
boom_core::responses::ResponsesStreamTranscoder          【出口边缘】
  │  ChatStreamChunk 逐个转 Responses SSE 事件：
  │  response.created → output_item.added → output_text.delta*
  │  → output_item.done → 终态事件 completed/incomplete/failed(usage)（无 [DONE] 尾帧）
  ▼
客户端 SSE / JSON

非流式：provider.chat_with_context → ChatCompletionResponse
        → openai_chat_to_responses() → Response JSON
```

### 5.2 模块归属

| 模块 | 新增内容 | 依赖 | CLAUDE.md 依据 |
|---|---|---|---|
| `boom-core` | `src/responses.rs`：Responses 协议类型 + 三个转换函数 + 流转码器（`pub use` 于 lib.rs） | 无新依赖 | §1 leaf；对称 `anthropic.rs` |
| `boom-responses`（P1 新 crate） | ResponseStore：`boom_response` / `boom_response_item` 两表的读写 + rehydrate + TTL 查询 | 仅 boom-core（+sqlx 经由现有公共封装） | §2/§3/§8：DB 表归本 crate；leaf |
| `boom-main` | `routes.rs::responses` handler（+ `/responses` 无前缀别名）；AppState 加 `response_store: Arc<ResponseStore>`（顶层，跨 reload）；TTL 清理后台任务 | 上述两者 | §4 顶层 Arc 跨 reload；§6 handler 无业务逻辑 |
| `boom-config` | `general_settings.responses_store: {enabled: bool, ttl_secs: u64, max_history_items: usize}` | — | §9 结构体登记 |
| Dashboard | 不改（P0/P1 无管理面需求） | — | — |

**P0 不新增任何 crate、任何表、任何配置**——只有 boom-core 一个新文件 + boom-main 路由/handler，风险面最小。

---

## 6. 详细设计

### 6.1 API 面

| 端点 | 方法 | 阶段 | 说明 |
|---|---|---|---|
| `/v1/responses`（+ `/responses` 别名，与 chat 前缀策略一致） | POST | P0 | 创建 response，`stream: true` 走 SSE |
| `/v1/responses/{id}` | GET | P1 | 检索已存储 response（`store:false` 的 id → 404） |
| `/v1/responses/{id}` | DELETE | P2 | 主动删除（TTL 之外的管理动作） |

### 6.2 类型定义（`boom-core/src/responses.rs`）

对齐 agentic-api `types/io/input.rs`/`output.rs` 的建模，裁剪到本网关支持的范围（不支持项在解析层显式报错，见 §6.8）：

```rust
// ── 请求 ─────────────────────────────────────────────
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),                       // 语法糖 = 单条 user message
    Items(Vec<ResponsesInputItem>),     // 显式 item 列表（含回放的 history）
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesInputItem {
    /// type 省略时的普通消息（untagged 兜底：只接受 role+content）
    Message(EasyInputMessage),
    FunctionCall(FunctionCallItem),        // 上轮 assistant 工具调用（id/call_id/name/arguments）
    FunctionCallOutput(FunctionCallOutputItem), // call_id + output
    Reasoning(ReasoningItem),              // 接受但 P0 丢弃（见 6.3 映射表）
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: ResponsesInput,
    pub instructions: Option<String>,
    pub previous_response_id: Option<String>,   // P0 未启用存储时显式 400
    pub tools: Option<Vec<ResponsesTool>>,      // P0 仅 type=function
    pub tool_choice: Option<serde_json::Value>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_output_tokens: Option<u32>,
    pub stream: Option<bool>,
    pub store: Option<bool>,                    // 默认 true；P0 接受并忽略
    pub reasoning: Option<ReasoningConfig>,     // {effort, ...}
    pub text: Option<serde_json::Value>,        // {format: {type:"text"|"json_object"|"json_schema",...}}
    pub include: Option<Vec<String>>,           // 接受；P0 不产出任何 include 附件
    pub truncation: Option<String>,             // 接受并忽略（"auto"）
    pub metadata: Option<serde_json::Value>,
    pub parallel_tool_calls: Option<bool>,
    // 以下两个显式捕获（不落 extra）：静默忽略会让客户端拿到错误语义，必须显式 400
    pub conversation: Option<serde_json::Value>, // conversation 会话前置参数（§6.7 拒绝）
    pub background: Option<bool>,                // background:true → 400（agentic-api 同款拒绝）
    // 与 ChatCompletionRequest.extra 同哲学：未知字段收集不拒绝
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ── 响应 ─────────────────────────────────────────────
#[derive(Debug, Clone, Serialize)]
pub struct ResponseObject {
    pub id: String,                       // "resp_" + uuidv7 简拼
    pub object: &'static str,             // "response"
    pub created_at: u64,
    pub status: &'static str,             // completed | incomplete | failed
    pub model: String,
    pub output: Vec<ResponsesOutputItem>,
    pub usage: Option<ResponseUsage>,
    pub incomplete_details: Option<IncompleteDetails>,  // {reason:"max_output_tokens"}
    pub error: Option<serde_json::Value>,
    // …并行输出 instructions/metadata/tools 等 echo 字段
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesOutputItem {
    Message { id: String, role: &'static str, status: &'static str,
              content: Vec<OutputContentPart> },   // {type:"output_text", text, annotations:[]}
    FunctionCall { id: String, call_id: String, name: String,
                  arguments: String, status: &'static str },
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseUsage {                  // Responses 命名
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub input_tokens_details: Option<CachedTokensDetail>,   // {cached_tokens}
    pub output_tokens_details: Option<ReasoningTokensDetail>,
}
```

### 6.3 请求转换：`responses_request_to_openai`

```rust
pub fn responses_request_to_openai(
    req: &ResponsesRequest,
    history: &[serde_json::Value],   // P1 由 rehydrate 注入，P0 恒为空
) -> Result<ChatCompletionRequest, ResponsesConvertError>
```

字段映射表：

| Responses | Chat Completions 内核 | 备注 |
|---|---|---|
| `model` | `model` | 先于 handler 的 alias/deployment 解析，链路与 chat 完全一致 |
| `instructions` | `messages` 头部插入 `{role:"system"}` | 与 agentic-api 一致；OpenAI 语义也是拼成 system |
| `input: "text"` | `messages += [{role:"user", content: text}]` | untagged 语法糖 |
| `input[].message(user/assistant/system/developer)` | 同名 role message；**`developer` 必须在转换层改写为 `system`** | 内核 `MessageRole` 是封闭枚举（types.rs:57，`#[serde(other)]` 兜底，无 Developer 变体）——直接透传会被降级为 `Unknown`（序列化成 `"unknown"`），上游收到非法 role。content parts（`input_text`/`input_image`）→ `ContentPart`（`ImageUrl` 变体已存在）映射；`output_text` 视为 text |
| `input[].function_call` | `{role:"assistant", tool_calls:[{id: call_id, type:"function", function:{name, arguments}}]}` | 关键：`call_id` 进 `ToolCall.id`，保证与下一条 tool message 对得上 |
| `input[].function_call_output` | `{role:"tool", tool_call_id: call_id, content: output}` | output 为对象时序列化为 JSON 字符串 |
| `input[].reasoning` | **丢弃** | 内核无对应物。P2 直通模式下保留。文档明示：跨轮 reasoning 上下文在转换路径上不回放 |
| `tools[].type="function"` | `tools[]`（`{type:"function", function:{name, description, parameters}}`，一一对应） | |
| `tool_choice` | **形状重写**：字符串 `"auto"/"none"/"required"` 直传；对象 `{"type":"function","name"}` → chat 嵌套形 `{"type":"function","function":{"name"}}`；`{"type":"mcp",...}` → 400 | 两协议对象形状不同（Responses 扁平 / Chat 嵌套，openai-openapi `ToolChoiceFunction` schema），是桥接路径的经典踩坑点；LiteLLM 专设 `_transform_tool_choice` 做同款转换 |
| `temperature` / `top_p` / `parallel_tool_calls` | 同名透传 | |
| `max_output_tokens` | `max_completion_tokens`（None 则不设） | 不用已废弃的 `max_tokens` |
| `reasoning.effort` | `extra["reasoning_effort"]` | OpenAI 兼容上游直接生效（`extra` flatten 透传）。**注意：Anthropic 上游现状会静默丢弃它**——`anthropic.rs` 只认 `extra["thinking"]`（`has_thinking`，anthropic.rs:64），不认 `reasoning_effort`（`openai.rs:227` 只有 thinking→effort 的反向补齐）。P0 必须补一个 effort→thinking budget 的正向小转换（low/medium/high → budget_tokens 阶梯），否则转换路径对 Claude 系模型丢 reasoning |
| `stream` | `stream: true` 时走流式分支 | |
| `previous_response_id` / `store` / `metadata` / `include` / `truncation` | **不进上游 body** | 网关侧消费（P1 存储 / P0 忽略），与 agentic-api `upstream_request` 剥离 continuation 字段同构 |
| `text.format` | `response_format`（**嵌套重写**） | Responses 扁平 `{type:"json_schema", name, schema, strict}` → chat 嵌套 `{"type":"json_schema","json_schema":{name, schema, strict}}`（与 tool_choice 同类的形状差异）；`{type:"json_object"}` → `{"type":"json_object"}`；`{type:"text"}` → 忽略；其余 400。LiteLLM `_transform_text_format_to_response_format` 同款 |
| `conversation` / `background:true` | **网关侧显式拒绝（400），见 §6.7** | `conversation` 官方语义是"会话 items 前置到 input"，静默忽略 = 客户端拿到错误语义（vLLM 静默忽略 `previous_response_id` 已被用户投诉为陷阱，issue #37697）；LiteLLM 对两者均未实现。显式 400 与 agentic-api 拒绝 background 同款 |
| 未知字段（`ResponsesRequest.extra`） | **整体并入** `ChatCompletionRequest.extra` | `prompt_cache_key`、`service_tier` 等自动获得 OpenAI 兼容上游透传（内核 extra 即 catch-all，types.rs:298）；continuation/存储类字段（store/conversation/background 等）已是显式捕获字段，不会泄入 extra |

**多轮 history 与新 input 的拼接顺序**（P1）：`rehydrate 出的旧 items（含上轮 output） ‖ 本轮 input items`，时间序即数组序；随后才做上表逐项转换。这正是 agentic-api "next context = prior context + prior output + new input" 的落点。

### 6.4 非流式响应转换：`openai_chat_to_responses`

```rust
pub fn openai_chat_to_responses(
    resp: &ChatCompletionResponse,
    model: String,          // 客户端请求里的 model 名（echo 语义）
    response_id: String,    // handler 生成，与流式同源
) -> ResponseObject
```

| Chat | Responses |
|---|---|
| `choices[0].message.content`（有值） | `output += Message{id:"msg_*", content:[{type:"output_text", text, annotations:[]}]}` |
| `choices[0].message.tool_calls[]` | 每个调用 `output += FunctionCall{id:"fc_*", call_id, name, arguments}`（`call_id` 取 `ToolCall.id`，缺失则生成 `call_*`） |
| `choices[0].message.reasoning_content`（非空时） | output 头部插入 `{type:"reasoning", id:"rs_*", summary:[]}` item | 内核已携带该字段（`Message.reasoning_content`，types.rs:186；GLM/Anthropic thinking 经此通道），与流式规则 6 对称 |
| `finish_reason == "stop"` | `status: "completed"` |
| `finish_reason == "length"` | `status: "incomplete"` + `incomplete_details: {reason: "max_output_tokens"}`（OpenAI 在 response 对象层表达，不发独立错误） |
| `finish_reason == "content_filter"` | `status: "incomplete"` + `incomplete_details: {reason: "content_filter"}` |
| `usage.prompt_tokens` | `usage.input_tokens` |
| `usage.prompt_tokens_details.cached_tokens` | `usage.input_tokens_details.cached_tokens` |
| `usage.completion_tokens` | `usage.output_tokens` |
| `usage.completion_tokens_details.reasoning_tokens` | `usage.output_tokens_details.reasoning_tokens` | **仅非流式可得**：`StreamUsage` 无 `completion_tokens_details`（types.rs:593），流式终态事件的该字段置空 |
| `n > 1` / 多 choice | 只取 `choices[0]`（Responses 无多 choice 概念，请求侧也不接受 `n`） |

### 6.5 流式转换：`ResponsesStreamTranscoder`（状态机）

接口与 `AnthropicStreamTranscoder`（`anthropic.rs:247`，`new(model)` / `transcode(&mut self, &ChatStreamChunk) -> Vec<ResponsesSseEvent>` / `drain()`）严格对称：

```rust
pub struct ResponsesStreamTranscoder {
    response_id: String,          // handler 预生成
    model: String,
    seq: u64,                     // sequence_number 计数器
    output_index: u32,
    // 当前 message item 状态
    cur_msg: Option<MsgItemState>,      // {item_id, text 累积}
    // 工具调用状态：index → {item_id, call_id, name, args 累积}
    cur_tools: BTreeMap<u32, ToolItemState>,
    usage: Option<ResponseUsage>,       // 尾包 usage 缓存
    finish_reason: Option<String>,
    created_at: u64,
}

impl ResponsesStreamTranscoder {
    pub fn new(model: &str, response_id: &str) -> Self;
    /// 连接上游**之前**调用：立即发 response.created(seq 0) + response.in_progress(seq 1)。
    /// 与 OpenAI 官方一致（response 对象创建即发，output:[]、usage:null），客户端在
    /// TTFT 之前就有首帧可渲染；上游连接失败转入 response.failed（错误走流内终态）。
    pub fn begin(&mut self) -> Vec<ResponsesSseEvent>;
    /// 逐 chunk 转换 delta / item 生命周期事件
    pub fn transcode(&mut self, chunk: &ChatStreamChunk) -> Vec<ResponsesSseEvent>;
    /// 流终止（正常结束/Err/Drop 后调用）：补 output_item.done + 终态事件
    /// （response.completed / response.incomplete / response.failed 三选一）
    pub fn drain(&mut self) -> Vec<ResponsesSseEvent>;
}
```

**事件序列契约**（对照 OpenAI 规范 + agentic-api `EventPayload` 全集裁剪）：

```
# handler 连接上游之前（begin()）——OpenAI 语义：response 对象创建即发，output:[]、usage:null
{type:"response.created",      sequence_number:0, response:{id, status:"in_progress", output:[], usage:null, ...}}
{type:"response.in_progress",  sequence_number:1, response:{...}}

# ── 文本输出（chunk.choices[0].delta.content 有值时）──
# 首个文本 delta 前一次性发出：
{type:"response.output_item.added",   ..., item:{id:"msg_*", type:"message", role:"assistant",
                                                status:"in_progress", content:[]}}
{type:"response.content_part.added",  ..., item_id:"msg_*", content_index:0,
                                        part:{type:"output_text", text:"", annotations:[]}}
# 每个文本 delta：
{type:"response.output_text.delta",   ..., item_id:"msg_*", delta:"..."}
# 上游文本结束（finish_reason 到达或 drain）：
{type:"response.output_text.done",    ..., item_id:"msg_*", text:"<全量累积>"}
{type:"response.content_part.done",   ..., part:{...全量}}
{type:"response.output_item.done",    ..., item:{...status:"completed"}}

# ── 工具调用（delta.tool_calls[i]）──
# 每个新 index 首次出现：
{type:"response.output_item.added",   ..., item:{id:"fc_*", type:"function_call", status:"in_progress",
                                                call_id:"call_*", name:"...", arguments:""}}
# 参数增量：
{type:"response.function_call_arguments.delta", ..., item_id:"fc_*", delta:"..."}
# 结束：
{type:"response.function_call_arguments.done",  ..., item_id:"fc_*", arguments:"<全量>"}
{type:"response.output_item.done",    ..., item:{...status:"completed"}}

# ── 终态（drain）——三选一，之后流自然结束（官方协议无 data:[DONE] 尾帧）──
{type:"response.completed",   sequence_number:N, response:{id, status:"completed",
                                output:[...全部 output items...], usage:{...}}}
{type:"response.incomplete",  sequence_number:N, response:{id, status:"incomplete",
                                incomplete_details:{reason:"max_output_tokens"|"content_filter"}, ...}}
                              # finish_reason=="length"/"content_filter" 走此独立终态事件类型，
                              # 不是 completed 里带 incomplete status（官方 response.incomplete 语义）
{type:"response.failed",      sequence_number:N, response:{id, status:"failed", error:{...}}}
                              # created 已发出后的任何错误中断（含上游连接失败）
```

状态机规则：

1. `sequence_number` 从 **0** 起连续递增。官方规范只承诺单调可排序（官方示例存在跳号；配合 `GET ?stream=true&starting_after=<seq>` 可断流续传——续传列为 P2）；我们自签发全部事件，自然满足严格 +1。
2. `response.created`/`in_progress` 由 `begin()` 在连接上游**之前**发出——与官方一致（response 对象创建即发），客户端 TTFT 之前就有首帧。**错误路径二分**：认证/限流/参数校验等前置失败不进流，仍是普通 HTTP 错误（§6.7）；created 已发出后的失败（含上游连接失败）必须走流内 `response.failed` 终态，不能中途改抛 HTTP 错误。
3. 文本与工具调用可交错；`output_index` 按 **item 开启顺序** 分配（msg 先开为 0，fc 后开为 1……），不按上游 chunk 顺序回退。以 `ToolCallDelta.index` 为 key 区分并发工具调用——内核已 lenient 处理缺省 index（默认 0，types.rs:629），上游省略 index 不会丢调用。
4. 上游若不发 `finish_reason`（异常断流）：`drain()` 补 `output_item.done` + `response.incomplete`（对齐官方断流语义）。
5. 上游 usage 尾包（`chunk.usage`）缓存进终态事件的 `response.usage`；没有则置 null（`UsageTracker`/计费不受影响，它们直接看 ChatStream）。
6. 上游发 `delta.reasoning_content`（DeepSeek R 系等 provider 已有此扩展）：转 `response.reasoning_text.delta` 事件，item_id `rs_*` —— 内核 chunk 已带该字段，转码器顺手映射，零额外成本。
7. **不发送 `data: [DONE]` 尾帧**：官方 Responses 流终止于终态事件（`[DONE]` 是 Chat Completions 行为；LiteLLM 桥接路径同样吞掉上游 `[DONE]`、以终态事件收尾）。agentic-api 会补发 `[DONE]`，如实测确有客户端依赖再加配置项（默认关）。

### 6.6 Handler 设计（`boom-main/src/routes.rs`）

结构与 `messages`（routes.rs:3057）对称，全部为"提取→转换→复用→组装"：

```rust
async fn responses(State(state), RequiredAuth(identity), headers, ConnectInfo(addr),
                   CachedJson(req): CachedJson<ResponsesRequest>) -> Response {
    // 1. P0 守卫：previous_response_id / conversation / background:true → 400（P1 起第一个改为 rehydrate）
    //    tools 含非 function 类型 → 400；input 含不支持 item 类型 → 400
    // 2. response_id = format!("resp_{}", uuid)；解析 model（pre_auth hook 改写同 chat）
    // 3. let chat_req = responses_request_to_openai(&req, &history)?;
    // 4. 复用 chat 链路骨架（与 chat_completions_inner 相同的调用序列）：
    //    check_model_access / resolve_request_model / check_plan_limits /
    //    candidates_for + kvc_orchestrator.route / acquire_fc_guard /
    //    compose_gateway_headers / ProviderCallContext
    // 5a. 流式：begin() 先发 created/in_progress（连接上游之前）
    //     → provider.chat_stream_with_context
    //     → 现有包装链不动（sse_stream_from_chat_stream → LoggedStream…）
    //     → 最外层 map chunk: ResponsesStreamTranscoder::transcode；终结时 drain()
    //     → Sse::new(sse_event_to_wire)   // 每帧含 event: <type> 行 + data: {json}；
    //                                     // 终态事件（completed/incomplete/failed）后流自然结束，无 [DONE] 尾帧
    //     → axum SSE Body（content-type: text/event-stream）
    // 5b. 非流式：provider.chat_with_context
    //     → openai_chat_to_responses → log_request(api_path="/v1/responses") → Json
}
```

接入点核对清单：

- **审计**：`RequestLog.api_path = "/v1/responses"`；token/duration/TTFT 由 `LoggedStream` Drop 照常写入（转换在 LoggedStream 之外层，不影响其观测的 ChatStream）。
- **计费**：`PlanCharge::settle` 消费 `UsageTracker`+`ProviderBilling`，两者都在内核侧，协议无关，零改动。
- **限流/配额**：`check_plan_limits` 以转换后的 `ChatCompletionRequest` 估 token，与 chat 同源。
- **中间件/统计**：`/v1/` 前缀过滤天然覆盖；无前缀别名 `/responses` 照 `/chat/completions` 模式注册（main.rs:186 同款）。**ctxaware 已核实无需改动**：`classify()` 为二分（anthropic/other，classifier.rs:40），`/v1/responses` 自动归 `Other`；`agent_stats.record(api_path)` 按 path 透传。P0 仅需验证，无代码改动。
- **prompt log**：`PromptLogStream` 位于包装链内层、看的是 ChatStream 的 raw data，保持不动。
- **body 大小**：沿用 2MB `CachedJson` 上限；P1 rehydrate 后 history 在内存拼接，不放大客户端 body 配额。超限行为与 chat 一致（413）。

### 6.7 错误模型

Responses 协议的错误载体（HTTP 非 200 或 SSE `response.failed`）：

```json
{ "error": { "code": "...", "message": "...", "param": null, "type": "..." } }
```

| 场景 | HTTP | error.code |
|---|---|---|
| `previous_response_id` 且存储未启用（P0）/ id 不存在 / 跨 key 归属 | 400 / 404 / 404 | `unsupported_parameter` / `response_not_found` / `response_not_found` |
| tools 含 built-in 类型（web_search 等） | 400 | `unsupported_tool_type` |
| input item 类型不支持 | 400 | `unsupported_input_item` |
| `conversation` 参数非空 / `background:true` / `tool_choice` 为 `{"type":"mcp",...}` | 400 | `unsupported_parameter` / `unsupported_tool_type` |
| created 已发出后的流内错误（上游连接失败、上游中断） | SSE `response.failed` 终态事件 | 见 §6.5 规则 2：前置失败（认证/限流/400）仍是普通 HTTP 错误，不进流 |
| 模型无权限 / 限流 / 上游错误（前置） | 沿用 `GatewayError` 既有映射 | 语义不变，仅包装为上述 error JSON 形状 |

跨 key 防枚举：`previous_response_id` 的 `boom_response.key_hash` 与当前请求不匹配时按 404 处理（不泄露存在性）。

### 6.8 P1：Response Store（`boom-responses` 新 crate）

**DDL**（migration 文件按惯例落 `boom-main/migrations/`，代码所有权归 `boom-responses`）：

```sql
CREATE TABLE boom_response (
    id                    TEXT PRIMARY KEY,        -- resp_*
    previous_response_id  TEXT,                    -- 自引用血缘（仅审计用，rehydrate 不走链）
    key_hash              TEXT NOT NULL,           -- 归属校验 + 防跨 key 枚举
    model                 TEXT NOT NULL,           -- 请求原始 model 名
    instructions          TEXT,                    -- 存但不自动继承（OpenAI 语义）
    tools_json            JSONB,                   -- 上轮 tools，本轮省略时继承
    history_item_ids      JSONB NOT NULL,          -- 有序 checkpoint（ADR-02 模式）
    status                TEXT NOT NULL,           -- completed / incomplete
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at            TIMESTAMPTZ NOT NULL     -- TTL 索引列
);
CREATE INDEX idx_boom_response_expires ON boom_response (expires_at);

CREATE TABLE boom_response_item (
    id           TEXT PRIMARY KEY,                 -- msg_* / fc_* / rs_*（与流式事件同 id）
    response_id  TEXT NOT NULL REFERENCES boom_response(id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,                 -- 顺序恢复依据
    data         JSONB NOT NULL,                   -- InputItem/OutputItem 原生 JSON
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_boom_response_item_resp ON boom_response_item (response_id, seq);
```

**为什么两表而非 agentic-api 三表**：我们无 Conversation API、无 per-conversation 旁路读写，`conversations` 表没有消费者；`items.conversation_id` 同理。保留的是被论证过的核心——item 只存一份、checkpoint 存有序 id、rehydrate 两次查询。

**Store API**：

```rust
pub struct ResponseStore { pool: Option<PgPool>, config: ResponsesStoreConfig }

impl ResponseStore {
    pub fn disabled() -> Self;                                       // enabled=false / 无 DB
    /// rehydrate：GET boom_response(id) WHERE key_hash=? AND expires_at>now()
    ///   → bulk GET boom_response_item WHERE id IN (...) ORDER BY seq
    ///   → Vec<Value>（应用层按 history_item_ids 恢复顺序）
    pub async fn rehydrate(&self, id: &str, key_hash: &str) -> StoreResult<RehydratedTurn>;
    /// persist：status ∈ {completed, incomplete} 且 store!=false 时写
    ///   response 行 + history items（上一轮 history_item_ids ‖ 本轮 input items ‖ 本轮 output items）
    pub async fn persist(&self, turn: &CompletedTurn) -> StoreResult<()>;
    pub async fn get(&self, id: &str, key_hash: &str) -> StoreResult<Option<ResponseObject>>;  // GET /v1/responses/{id}
    /// TTL 清理（boom-main 后台任务周期调用，DELETE ... WHERE expires_at < now()）
    pub async fn prune_expired(&self) -> StoreResult<u64>;
}
```

**语义（与 agentic-api / OpenAI 对齐）**：

- `store` 默认 **true**；`store:false` 完全不落盘（id 不可检索不可续）。
- 持久化时机：由**持有 transcoder 的最外层流适配器**在流终结（正常/异常/Drop）时触发——与 `LoggedStream` 同款 Drop 模式，此时终态事件已生成、output items 与 usage 都在 transcoder 状态里，就地组装 `CompletedTurn` 写库（drop 后无人再持有这些数据，不能靠"handler 尾部任务"补写）；非流式在响应组装后。写失败仅 `warn`，不影响已发出的响应（与 YAML best-effort 同哲学）。
- **instructions 不自动继承**；**tools 省略时继承上轮**（rehydrate 返回 `tools_json`，转换层在 `req.tools.is_none()` 时采用）。
- 新 checkpoint 的 history = `上轮 history_item_ids ‖ 本轮 input items ‖ 本轮 output items`（output 复用流式已分配的 `msg_/fc_` id，客户端续写回放的 id 与库中一致）。
- AppState 挂载：`response_store: Arc<ResponseStore>` **顶层字段**（CLAUDE.md §4：跨 reload 存活，不进 `AppStateInner`）。
- **续写路由亲和（免费获得，优于 LiteLLM 的显式钉扎）**：rehydrate 发生在 `select_provider` **之前**，重建出的完整历史作为前缀参与 KV prefix 亲和打分，续写请求天然以最大前缀命中上一轮的 worker/deployment——LiteLLM 需要专门的 `responses_api_deployment_check` 预检才能做到的事，本方案由"先重建历史再路由"的顺序自然满足。
- **response id 签发权（借 LiteLLM 的安全思路）**：`resp_` id 一律由网关签发；P2 直通路径下上游返回的自带 id 必须替换为网关 id 再返回客户端，GET/续写只认网关签发的 id（LiteLLM 对非本代理签发的 id 一律 403，防伪造与跨实例枚举）。
- 多实例：表在共享 PostgreSQL（与 `boom_request_log` 同部署形态）；SQLite 不支持（本网关 DB 即 PG，无 SQLite 形态）。

**配置（`boom-config` `general_settings` 新增，走 §9 manifest 流程的 1、2、5 三处）**：

```yaml
general_settings:
  responses_store:
    enabled: true          # 默认 true；false 时 /v1/responses 仍可用但恒为无状态
    ttl_secs: 2592000      # 默认 30 天（对齐 OpenAI 默认 retention）
    max_history_items: 1000
```

后台 prune 任务与现有 `boom-main` 后台任务同框架（周期 `min(ttl_secs/10, 1h)`）。

### 6.9 P2：vLLM 类 deployment 的原生直通（可选增强）

对显式声明的 deployment 走 Responses 原生上游（agentic-api 的上游路线），获得转换路径没有的保真能力（reasoning item 跨轮回放、`stateless` 直通字段）：

- `ProviderParams` 新增 `protocol: Option<String>`（`"responses"`）→ `ProviderProtocol::Responses` 变体。
- `OpenAIProvider` 直通模式：POST `{base}/v1/responses`，上游 body = 客户端 body 剥离 `store`/`previous_response_id`（rehydrate 由网关做，agentic-api `upstream_request` 同构）；SSE 逐事件透传，usage 从 `response.completed` 事件提取进 `ProviderBilling`。
- 出口侧：直通 deployment 的 `/v1/responses` 不经 transcoder（handler 按 `Selection.provider.protocol()` 分叉）；`/v1/chat/completions` 打到该 deployment 时仍走 chat 端点（vLLM 两者都支持）。
- **涉及 §9 manifest 5 处同步**（新字段 `protocol`）：`boom-config/lib.rs`、`manifest.rs`、`deployment_store.rs` 的 `DEPLOYMENT_CORE_COLUMNS`+SQL×5、前端 `app.js`+`i18n.js`、migration。这条链路的编译期 forcing functions 已存在，照走即可。

---

## 7. 分阶段落地计划

| 阶段 | 内容 | 交付判据 |
|---|---|---|
| **P0 无状态**（一个 PR 量级） | `boom-core/src/responses.rs`（类型+3 个转换函数+transcoder，含单测）；`boom-main` 路由注册 + handler；ctxaware 归类验证（无代码改动）；`previous_response_id`/`conversation`/`background`/built-in tools 显式 400；`reasoning.effort`→Anthropic thinking budget 正向转换 | openai-sdk（`client.responses.create`，stream/non-stream）指向网关可用；单测/集成测全绿。**Codex CLI 依赖 P0.5** |
| **P0.5 Codex 兼容** | freeform custom tools：`type:"custom"` 工具声明 + `custom_tool_call`/`custom_tool_call_output` item + `response.custom_tool_call_input.delta/.done` 事件。上游侧把 custom 包装为 function tool（单 string 参数 schema），出口侧还原 custom 契约（input 为自由文本） | Codex CLI 端到端可用（其 `apply_patch` 走 custom tool）；vLLM #49724 那种"强转 function 导致 Codex 报错"不复发 |
| **P1 有状态** | `boom-responses` crate（两表 store + rehydrate + prune）；配置项 + migration；handler 接 rehydrate/persist；`GET /v1/responses/{id}` | `previous_response_id` 多轮续写正确（含工具调用轮回放）；`store:false` 不落盘；跨 key 404；TTL 清理生效 |
| **P2 直通**（独立评审） | `protocol: responses` deployment + 直通模式 + manifest 5 处同步 | vLLM deployment 上 reasoning 回放保真；转换路径回归不受影响 |

**实施状态（2026-10-21）**：P0 ✅（`1b6fc11`）、P0.5 ✅（`a2fdccc`）、P1 ✅（`640d154`）、**P2 ✅ 已落地**（附录 B 预案实施：B.1 manifest 5 处同步 / B.2 side-channel / B.3 raw_data 透传保审计链 / store 复合主键修复上游 item id 冲突；misc/Agent 7 项检查在直通与转换两类 deployment 上均全绿，reasoning 保真续写闭环 `NATIVE_ROUND2 items=4` 验证通过）。全部阶段完成。

每阶段独立可合并、可回滚；P0 不引入任何持久化与配置面变更。

---

## 8. 测试计划

**单元（boom-core/responses.rs 内嵌 tests，参照 anthropic.rs 测试形态）**
- 请求映射表逐行用例：instructions、input text 糖、四种 role message、function_call↔tool_calls（`call_id` 往返）、function_call_output、reasoning 丢弃、tools、**tool_choice 对象形状重写**（`{"type":"function","name"}` → 嵌套形；`mcp` 形状 400）、max_output_tokens→max_completion_tokens、reasoning.effort→extra（含 Anthropic thinking 阶梯）。
- 响应映射：文本、多工具调用、finish_reason×3 → status/incomplete_details、usage 五字段映射、usage 缺失。
- Transcoder 状态机：纯文本流、纯工具流、文本+工具交错（output_index 分配）、参数增量跨 chunk、上游无 finish_reason（drain 补 incomplete）、usage 尾包、`delta.reasoning_content` → `rs_` 事件、错误中断 → `response.failed`。
- **不变量断言**：sequence_number 连续且从 0 起；每个 `output_item.added` 恰好配对一个 `output_item.done`；`output_text.done.text` == 全量 delta 拼接；终态事件后**无** `data: [DONE]` 尾帧；`finish_reason=length` 的终态事件是 `response.incomplete`（独立事件类型，非 completed 带 incomplete status）；created/in_progress 在任何 delta 之前发出。

**集成（boom-main 测试 + wiremock 上游）**
- wiremock 回放 OpenAI 形状 chat SSE → 网关 `/v1/responses` → 断言客户端侧事件序列、`[DONE]`、header。
- 非流式 JSON 全字段对比（golden file）。
- 限流/审计联动：断言 `boom_request_log` 行（api_path、token 数与 chat 路径同源）。

**兼容性校准**
- 用 OpenAI 官方 `/v1/responses` 录制真实流（文本/工具两种），逐事件 diff 网关输出（忽略 id/时间戳），校准字段名、事件顺序与终态行为（官方基准：`response.created` seq=0、终态事件收尾、无 `[DONE]`、length→`response.incomplete`）。Codex CLI 端到端冒烟（P0.5 之后）。

**P1 存储**
- rehydrate 顺序恢复（乱序 bulk fetch）、tools 继承/instructions 不继承、store:false、跨 key 404、expires_at 过滤、prune 幂等。

---

## 9. 架构合规自查（对照 CLAUDE.md）

| 原则 | 本方案 | ✓ |
|---|---|---|
| §1 单向依赖 | 转换层 boom-core（leaf）；store 新 crate 仅依赖 boom-core；handler boom-main。无环、无新跨模块依赖 | ✓ |
| §2 职责边界 | boom-core 只加协议类型与纯函数，无实现状态；boom-responses 只管两张自有表；boom-main handler 无业务逻辑 | ✓ |
| §3 表所有权 | `boom_response`/`boom_response_item` 归 boom-responses，前缀合规，DDL 不越界；不读写他模块表 | ✓ |
| §4 状态生命周期 | `response_store` 放 AppState 顶层 Arc，跨 reload；不进 AppStateInner | ✓ |
| §5 AdminCommand | P0/P1 无 Dashboard 写操作，不涉及 | ✓ |
| §6 路由原则 | handler 仅提取→转换→复用→组装；流式 Drop 审计链不变（transcode 在 LoggedStream 外层） | ✓ |
| §7 热加载 | store 配置热加载仅影响"是否持久化"，不清运行态；limiter/concurrency 不受影响 | ✓ |
| §8 新增模块清单 | boom-responses：依赖仅 boom-core ✓、无环 ✓、表前缀 ✓、`pub use` 导出 ✓、boom-main Cargo.toml+state.rs 更新 ✓ | ✓ |
| §9 manifest 单一真相源 | P0/P1 新配置仅 general_settings 三字段（config+manifest+migration 三处，无 deployment 字段）；P2 的 `protocol` 字段按 5 处清单全走 | ✓ |
| §10 CRUD 写路径 | 无 CRUD handler；persist 是内部写路径，失败 warn 不阻断（best-effort 与 YAML 同哲学） | ✓ |

---

## 10. Non-goals

1. **工具执行循环**（gateway-owned tools、MCP、code interpreter、web search）——Boom Gateway 是路由/限流/计费网关，agent 执行器是 agentic-api 的定位；两者可串联部署（客户端 → boom → agentic-api → vLLM）。
2. Conversation API（`/v1/conversations*`）、multi-agent、WebSocket、compaction、background mode。
3. `stateless` 直通字段 / reasoning `encrypted_content` 保真（转换路径丢弃；P2 直通解决）。
4. Batch API、文件上传、vector store。
5. Responses 协议的 Dashboard 管理界面（P1 之后按需另立项）。
6. `conversation` 会话参数、`background` 后台模式、Conversations API（LiteLLM 亦未实现 conversation/background；本网关显式 400 而非静默忽略）。

---

## 附录 A：与其他 Responses API 实现的对照（review 记录，2026-10-20）

定稿前与 **LiteLLM**（本网关对标代理，`litellm/responses/` 源码）、**OpenAI 官方协议**（openai/openai-openapi spec + streaming-events 参考）、**vLLM 原生实现**、**agentic-api** 逐点对照。review 发现的问题及已应用的修正：

| # | 原稿问题 | 严重度 | 证据 | 修正 |
|---|---|---|---|---|
| R1 | `tool_choice` 声称"两协议形状相同，原样透传" | 🔴 协议错误 | Responses 扁平 `{"type":"function","name"}`，Chat 嵌套 `{"type":"function","function":{}}`（openai-openapi `ToolChoiceFunction`）；LiteLLM 专设 `_transform_tool_choice` | §6.3 显式形状重写，`mcp` 形状 400 |
| R2 | `finish_reason=length` 映射为 `response.completed` 事件携带 incomplete status | 🔴 协议错误 | 官方有独立终态事件 `response.incomplete`（官方示例即 `max_output_tokens`） | §6.5 终态三事件分叉 |
| R3 | `sequence_number` 从 2 起 | 🔴 协议错误 | 官方示例 `response.created` seq=0；规范仅承诺单调可排序（配 `starting_after` 续传） | §6.5 改 0 起 |
| R4 | 流结尾发 `data: [DONE]` | 🟠 偏离官方 | 官方 Responses 流终止于终态事件、无 `[DONE]`（那是 Chat 行为）；LiteLLM 桥接路径吞掉上游 `[DONE]` 以终态事件收尾 | §6.5 去掉尾帧（可配置保留，默认关） |
| R5 | `response.created` 惰性发出（首 chunk 才开流） | 🟠 语义偏离 | 官方在 response 对象创建后立即发出（`output:[]`、`usage:null`）；惰性推迟客户端首帧、且把本应流内的上游连接失败变成 HTTP 错误 | §6.5 增 `begin()`；错误路径二分（前置失败=HTTP，created 后=流内 failed） |
| R6 | P0 验收含 Codex CLI，但 P0 拒绝 freeform custom tools | 🟠 目标自相矛盾 | Codex 的 `apply_patch` 是 custom tool（`custom_tool_call` item + `custom_tool_call_input.delta` 事件），严格依赖 custom 契约——vLLM #49724 强转 function 即挂 | 新增 P0.5：custom 双向转换（上游包装 function、出口还原契约） |
| R7 | 声称 Anthropic 上游能消化 `reasoning.effort` | 🟠 声明失实 | `anthropic.rs:64` 只认 `extra["thinking"]`；`openai.rs:227` 仅 thinking→effort 反向；effort 进 Anthropic 会被静默丢 | §6.3 列为 P0 工作项（effort→budget 阶梯） |
| R8 | `conversation`/`background` 落入 extra 被静默忽略 | 🟡 语义陷阱 | `conversation` 官方语义是会话 items 前置；静默忽略=客户端拿到错误结果（vLLM 静默忽略 `previous_response_id` 已被投诉，issue #37697） | §6.2 显式字段捕获 + §6.7 显式 400 |
| R9 | P1 持久化触发点描述含糊（"LoggedStream::drop 之后由 handler 尾部任务写"） | 🟡 实现风险 | drop 后无人持有 output items（在 transcoder 状态里） | §6.8 改为持有 transcoder 的最外层适配器在流终结时写 |
| R10 | `ResponsesRequest` 缺 `text`/`include`/`truncation` 字段（映射表引用了它们，实际会落 extra） | 🟡 内部不一致 | 结构体与映射表对照 | §6.2 补齐显式字段 |

第二轮 review（对源码逐条验证 + 上轮修补一致性复查）新增：

| # | 问题 | 严重度 | 证据 | 修正 |
|---|---|---|---|---|
| R11 | `developer` role 原稿当"兼容性选择"透传——实际会被内核吞掉 | 🔴 | `MessageRole` 封闭枚举 + `#[serde(other)]`（types.rs:57-69）：无 Developer 变体，透传即降级 `Unknown`（序列化 `"unknown"`），上游收到非法 role | §6.3 升级为转换层硬性改写 developer→system |
| R12 | `text.format` json_schema 未写嵌套重写（与 tool_choice 同类的形状差异） | 🟠 | Responses 扁平 vs chat 嵌套 `json_schema:{...}`；LiteLLM 专设 `_transform_text_format_to_response_format` | §6.3 补形状重写 |
| R13 | 非流式 `message.reasoning_content` 未映射 | 🟠 | 内核 `Message.reasoning_content` 已存在（types.rs:186，GLM/Anthropic thinking 通道），流式侧已映射、非流式侧漏了 | §6.4 补 `rs_` reasoning output item |
| R14 | 原稿声称需改 ctxaware path 分类（"唯一既有代码改动点"）—— 复核不成立 | 🟡 降风险 | `classify()` 为二分 anthropic/other（classifier.rs:40），`/v1/responses` 自动归 `Other` | §6.6/§7 改为"验证即可，无代码改动" |
| R15 | 流式终态事件声称映射 `output_tokens_details.reasoning_tokens` | 🟡 | `StreamUsage` 无 `completion_tokens_details`（types.rs:593），流式拿不到 | §6.4 注明仅非流式映射，流式置空 |
| R16 | `ResponsesRequest.extra` 转发策略未声明 | 🟡 | `prompt_cache_key`/`service_tier` 等依赖透传才能对 OpenAI 上游生效 | §6.3 补"整体并入内核 extra" |
| R17 | 上轮修正残留：§5.1 数据流图仍画着 `[DONE]` 尾帧 | 🟡 一致性 | 与修正后的 §6.5 规则 7 矛盾 | 已同步修正 |

第二轮同时**验证为成立**的既有声明（无需改）：`ContentPart::ImageUrl` 支撑 input_image 映射（types.rs:87）；`ToolCall.id` 支撑 `call_id` 往返（types.rs:217）；`ToolCallDelta` lenient 缺省 index（types.rs:629）保证以 index 为 key 的工具状态机可靠；无前缀别名路由模式已存在（main.rs:186）；`anthropic.rs:262` 用 `max_completion_tokens.or(max_tokens).unwrap_or(4096)` 保证 Anthropic 必填 max_tokens 不缺。

对照中**被验证为正确/优于对照物**的设计（保留并补据）：

- 边缘转换架构与 LiteLLM 桥接路径（`litellm_completion_transformation/`）同构；其"原生 Responses provider 透传、其余桥接"的分层 == 本方案 P2 的方向， mutually印证。
- `instructions` → system message 置于消息列表首位（LiteLLM `transform_instructions_to_system_message` 同款）。
- 桥接路径自造 SSE 事件、不转发上游 `[DONE]`（LiteLLM `streaming_iterator` 同款行为）。
- `previous_response_id` 无存储时**显式报错**优于 vLLM 的接受并忽略（LiteLLM 亦显式报错 "Using previous_response_id without a session database"；vLLM 静默忽略已被用户投诉）。
- 专用两表存储优于 LiteLLM 复用 SpendLogs+S3 的方案（其前提 `store_prompts_in_spend_logs` 默认关闭、大 payload 依赖冷存储组件；专用表无前置条件）。
- 新增借鉴：response id 网关签发权（LiteLLM 对非自签 id 403）；rehydrate 前置于路由，使 KV prefix 亲和天然实现续写钉扎（LiteLLM 需专门配置 `responses_api_deployment_check`）。

---

## 附录 B：P2 实施预案（✅ 已于 2026-10-21 按此实施）

> 实施记录：B.1/B.2/B.3/B.4/B.5 全部按预案落地（B.2/B.3 采用推荐方案 1）。
> 实施中发现并修复两个真问题：① 非流式直通的裸 Response 对象缺 id 重写
> （rewrite 扩展为兼容事件包裹形与裸对象形）；② `boom_response_item` 全局
> 单列主键在上游 item id 跨 response 重复时炸 persist → 改复合主键
> `(response_id, id)`。

### B.1 协议标记链路 —— §9 manifest 5 处同步（编译器强制）

| # | 文件 | 改动 |
|---|---|---|
| 1 | `boom-core/src/provider.rs` | `ProviderProtocol` 加 `Responses` 变体（现仅 `OpenAiCompatible`/`Native`） |
| 2 | `boom-config/src/lib.rs` | `ProviderParams` 加 `protocol: Option<String>`——forcing test 编译失败直至 #3 完成 |
| 3 | `boom-config/src/manifest.rs` | `model_deployment_fields()` 注册 `protocol` FieldMeta（select） |
| 4 | `boom-routing/src/deployment_store.rs` | `DEPLOYMENT_CORE_COLUMNS` + `DeploymentInput` + 5 条 SQL（三条编译期测试强制） |
| 5 | `boom-dashboard` `app.js` + `i18n.js` | 表单字段 + label/tip 翻译 |
| + | `boom-routing/src/migrations.rs` | `ALTER TABLE boom_model_deployment ADD COLUMN protocol TEXT`（仓库无 boom-main/migrations/ 目录，DDL 按现有模式归各 crate 的 migrations.rs） |

### B.2 决策点 1：原始 Responses body 怎么进 Provider —— 推荐 side-channel

`Provider` trait 只有 chat 形状入口，直通必须让原始 Responses body 原样到上游（转 chat 再转回即失去 reasoning 保真意义）。

- **✅ 方案 1（采用）**：`ChatCompletionRequest` 加 `#[serde(skip)] responses_passthrough: Option<serde_json::Value>`——与既有 side-channel 先例（`gateway_headers`、`raw_capture`）同款。handler 把剥离 `store`/`previous_response_id`/`conversation`、**rehydrate 历史已内联**的原始 body 放入；`OpenAIProvider` 检测到即 POST `{base}/v1/responses`
- 方案 2（弃）：`Provider` trait 加 `responses_stream()` 默认 `NotSupported`——波及所有 provider 实现与测试

### B.3 决策点 2：直通流怎么过审计/计费链 —— 推荐 raw_data 透传

- **✅ 方案 1（采用）**：上游每个 Responses 事件装进 `ChatStreamChunk.raw_data`（字段已存在），handler 直通模式原样转发、跳过 transcoder——`LoggedStream`/`InFlight`/`FlowControlled`/`GuardedStream` 包装链与 RequestLog 审计**全部原样保留**。usage 从透传的 `response.completed` 事件提取进 `UsageTracker`（否则 PlanCharge settle 0 token）；`sse_responses_extractor` 按事件 type 解析，直通事件同名直接复用
- 方案 2（弃）：handler 按 `protocol()` 绕开 chat 链路另起流管线——审计/限流/计费接线重做，双路径长期维护

### B.4 P1 store 与 handler 适配

- 直通轮同样 rehydrate → 历史前插（只是不做 chat 转换）
- persist 用上游终态事件的 response 对象与 output items（含 `rs_` reasoning、`encrypted_content`、原生 id）直接入库——`GET` 与下一轮回放原样送回上游，**reasoning 保真闭环是 P2 的核心价值**
- `responses` handler 加直通分叉；`/v1/chat/completions` 打到该 deployment 仍走 chat 端点（vLLM 两端点都支持）
- 直通返回的自带 resp id 必须替换为网关 id（§6.8 id 签发权规则）

### B.5 测试

provider 直通单测（wiremock vLLM 形状 `/v1/responses` SSE）；handler 分叉集成；mock-upstream 增加原生 `/v1/responses` 端点 + Agent 直通检查（验证 `rs_`/`encrypted_content` 回放保真、转换路径回归不受影响）。
