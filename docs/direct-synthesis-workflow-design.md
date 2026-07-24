# Direct Synthesis Workflow 设计

## 1. 概述

Direct Synthesis 是 BooMGateway 内置的多模型编排能力。客户端仍然使用标准
OpenAI Chat Completions 协议，只需请求一个已配置的 workflow 模型，例如
`model: "fusion"`。Gateway 在进程内完成以下步骤：

1. 并行调用多个 panel 实例，收集独立分析或候选建议。
2. 过滤无效 panel 输出，并保持配置顺序。
3. 将原始对话和有效 panel 输出交给 aggregator。
4. 返回 aggregator 生成的最终 assistant response。

Workflow 对客户端表现为一个普通模型，但它不对应单一 deployment。它是一个受
配置、权限和父请求限额控制的逻辑模型。

本文档定义 Direct Synthesis 的架构边界、配置格式、请求变换、执行语义、失败
行为、用量统计和扩展方式。

## 2. 设计目标

- 在 Gateway 进程内提供可扩展的 Workflow 抽象。
- 首个实现为 `direct_synthesis`，但核心接口不绑定具体 workflow。
- 复用现有 deployment 路由、调度、流控、健康检查和 provider 归一化能力。
- 保持标准 OpenAI messages、tools、tool calls 和 usage 语义。
- 让 panel 只提供私有参考信息，由 aggregator 生成真正面向客户端的响应。
- 对 workflow 模型执行与普通模型一致的鉴权和套餐限制。
- 聚合真实子调用的 token usage，同时保持标准 OpenAI 响应 schema。
- 不为 workflow 自建客户端响应字段或独立 audit 体系。
- 配置错误在启动或 reload 阶段被拒绝，不在请求执行时静默回退。

## 3. 非目标

- 不允许客户端直接指定 workflow id、panel 或 aggregator。
- 不根据单次请求动态选择 workflow。
- 不允许 workflow 角色引用另一个 workflow 模型。
- 不通过 HTTP 回调 Gateway 自身来执行子模型请求。
- 不在首期提供 workflow 调试 HTTP 接口。
- 不在首期支持 `/v1/messages` 上的 workflow 模型。

## 4. 核心概念

### 4.1 Workflow 模型

Workflow 模型是客户端可见的逻辑模型名，例如 `fusion`。它只用于：

- 权限判断；
- 模型发现；
- 父请求限额和日志；
- 由 Router 选择对应的虚拟 Provider。

Workflow 模型会作为虚拟 Provider 注册到 `DeploymentStore`。父请求因此仍走普通的
`Router -> Provider` 链路；虚拟 Provider 独占该模型的候选集合，不会与运行期
动态添加的普通 deployment 混合。

### 4.2 Workflow 定义

Workflow 定义包含稳定的 workflow id、类型和角色绑定。例如：

```yaml
workflow_settings:
  models:
    fusion: direct_synthesis
  workflows:
    direct_synthesis:
      type: direct_synthesis
      panel_timeout_secs: 1200
      roles:
        panel:
          - model: glm-5.2
            temperature: 0.3
          - model: glm-5.2
            temperature: 0.5
        aggregator:
          model: glm-5.2
```

这里包含两层映射：

```text
客户端模型名 fusion
        |
        v
workflow id direct_synthesis
        |
        +-- panel[0] -> glm-5.2, temperature 0.3
        +-- panel[1] -> glm-5.2, temperature 0.5
        `-- aggregator -> glm-5.2
```

### 4.3 Panel

Panel 是只提供参考意见的模型实例。多个 panel 独立、并行执行，其输出不会直接
暴露给客户端。

### 4.4 Aggregator

Aggregator 接收完整原始对话和 panel 参考结果，负责生成当前对话真正的下一条
assistant response。最终 content、tool calls、finish reason 和响应模型名均来自
aggregator；仅在 aggregator 调用失败时使用降级结果。

## 5. 模块架构

### 5.1 依赖方向

```text
boom-core
    ^
    |
boom-config -----+
boom-fusion -----+--> boom-routing <----- boom-main
boom-flowcontrol-+         |
                           +-- FusionProvider
                           +-- RoutingModelInvoker
```

`boom-fusion` 只依赖基础类型，不依赖 `boom-routing` 或 `boom-main`。
`FusionProvider` 和 Gateway 侧 `ModelInvoker` 实现在 `boom-routing/src/fusion.rs`。
`boom-routing` 不依赖 `boom-main` 或 `boom-auth`；`boom-main` 只在启动和 reload
时注入 Router、FlowController、InFlightTracker、KV prefix index 和请求统计。

### 5.2 Workflow 抽象

核心接口包括：

- `Workflow`：统一异步执行接口。
- `WorkflowRegistry`：维护 workflow 实例以及公开模型名到 workflow id 的映射。
- `WorkflowContext`：包含原始请求和模型调用器。
- `WorkflowExecution`：包含最终标准响应。
- `ModelInvoker`：执行一次真实模型调用。
- `WorkflowRole`：标识调用属于 panel 或 aggregator。

`Workflow` 只负责请求编排和结果组合，不负责 deployment 选择、HTTP 调用和用户
配额管理。

### 5.3 FusionProvider 与 RoutingModelInvoker

`FusionProvider` 实现标准 `boom_core::Provider`。普通 Chat route 完成父请求
鉴权、模型权限和套餐 admission 后，通过 context-aware Provider 方法把 key hash、
key alias、VIP 和 API path 传给 Fusion。

`RoutingModelInvoker` 是 `ModelInvoker` 的 Gateway 实现。每次 workflow 子调用
都会重新进入同一个 Router，并复用：

- alias 和真实模型名解析；
- 与主路由一致的 tools + messages 字节前缀计算和 KV-cache 自学习；
- deployment 选择和 schedule policy；
- deployment flow control；
- inflight 统计；
- VIP priority header；
- client type header；
- provider 健康状态；
- deployment request rate；
- provider 请求与响应归一化。

子调用不会重复执行：

- 外部 API key 鉴权；
- 父请求的模型权限判断；
- 用户或团队套餐次数扣除；
- 父请求级数据库日志和 prompt log；
- workflow 模型解析。

## 6. 配置与校验

### 6.1 配置结构

`workflow_settings.models` 定义客户端模型名到 workflow id 的映射。

`workflow_settings.workflows` 定义具体 workflow。Direct Synthesis 需要：

- 至少两个 panel 实例；
- 恰好一个 aggregator 实例；
- 每个实例指定一个真实模型名或 alias；
- 每个实例可以独立配置 temperature。
- `panel_timeout_secs` 可选；配置后限制每个 panel 从进入 Router 到 Provider
  返回的总时间，未配置时继续使用 Provider 和 flow control 自身的超时。

### 6.2 启动与 reload 校验

配置必须满足：

1. Workflow 模型名非空。
2. Workflow id 非空。
3. Workflow 模型名不能与 deployment 或 alias 冲突。
4. 模型映射引用的 workflow id 必须存在。
5. Direct Synthesis 至少配置两个 panel。
6. Panel 和 aggregator 模型必须存在于模型或 alias 配置中。
7. Panel 和 aggregator 不能引用 workflow 模型。
8. Temperature 必须是有限浮点数。
9. `panel_timeout_secs` 配置后必须大于 0。
10. YAML alias 解析到 workflow 模型时拒绝配置。

任何校验失败都会拒绝启动或本次 reload。

### 6.3 虚拟 Provider 发布

启动时，`boom-routing` 根据已验证配置构造 `WorkflowRegistry`，随后为每个
`workflow_settings.models` 条目创建一个 `FusionProvider`，并注册到
`DeploymentStore`。

Reload 时真实 deployment store 会先按现有流程重建，然后重新注册 FusionProvider。
新请求通过同一个 Router 观察新 Provider；已经取得旧 Provider 的进行中请求继续
使用旧 workflow 和运行时快照。

启动和 reload 会同时检查 DB-only deployment 与 alias。Workflow 模型名与任何
已有资源冲突时直接拒绝，不覆盖或遮蔽真实资源。Reload 在清空旧 store 前完成 DB
命名空间预检。

## 7. 请求路由

正式支持的入口是：

```text
POST /v1/chat/completions
```

请求处理顺序为：

1. 执行正常的模型权限检查。
2. 执行父请求套餐和窗口限额检查。
3. Router 将 workflow 模型选择为唯一的 FusionProvider。
4. 父 route 调用 context-aware Provider 接口。
5. FusionProvider 创建 `RoutingModelInvoker` 并执行 workflow。
6. 每个真实子模型请求再次进入 Router 的 alias/hybrid、KVC 和 schedule policy。
7. Panel 始终并发执行非流式请求。
8. 非流请求通过 aggregator `chat()` 返回 JSON；流式请求通过 aggregator
   `chat_stream()` 返回 SSE。
9. 将成功子调用的 token 汇总到标准 `usage`。
10. 写入父请求日志和 prompt log。
11. 非流和流式响应均保持标准 OpenAI schema。

FusionProvider 注册到 DeploymentStore，因此 workflow 模型会自然出现在
`/v1/models` 和 `/v1/models/{model}` 中，并遵守 key 的模型白名单和
`public_models` 配置。

Legacy Completions 当前复用 Chat Completions 内部处理函数，但不作为 workflow
的稳定协议合同。`/v1/messages` 会明确拒绝 workflow 模型。

## 8. Direct Synthesis 执行流程

```text
原始 ChatCompletionRequest
        |
        +--------------------------+
        |                          |
        v                          v
   Panel request 0            Panel request 1
        |                          |
        +-------- 并行执行 --------+
                   |
                   v
          按配置顺序过滤结果
                   |
                   v
          构造 Aggregator request
                   |
                   v
             Aggregator 调用
                   |
                   v
          最终响应或降级响应
```

### 8.1 Panel 请求

每个 panel 请求从完整原始请求 clone 得到，然后执行以下变换：

1. `model` 替换为 panel 配置中的模型。
2. `temperature` 替换为 panel 实例配置值；未配置时该字段被省略，不继承客户端值。
3. `stream` 强制设为 `false`。
4. `tools` 强制设为空数组。
5. `tool_choice` 清除。
6. 保留原始 messages、max tokens 和 metadata。

如果原始请求包含非空 tools，还会在 messages 末尾追加
`reference_advisor.txt`，明确要求 panel：

- 只提供分析、建议、风险和行动策略；
- 不执行工具；
- 不向最终用户直接作答；
- 不输出无法访问工具或环境的免责声明。

原始请求不包含 tools 时，panel messages 保持不变，不追加 advisor 消息。

### 8.2 Panel 并发与顺序

所有 panel 使用 `join_all` 并发执行。

虽然不同请求的完成时间可能不同，`join_all` 的结果顺序与输入 future 顺序一致。
因此 aggregator 看到的 panel 答案顺序始终由配置决定，不由完成时间决定。

### 8.3 有效结果判断

Provider 调用失败不会产生 panel 结果。

对于成功响应，第一条 choice 满足以下任一条件时视为有效：

- 包含非空 tool calls；
- 包含非空文本。

Panel 正常情况下不应产生 tool calls，因为其 tools 已被清空。该判断仅作为防御性
响应处理。Provider 错误必须通过结构化 `Result::Err` 返回，不通过模型文本前缀推断，
避免正常的错误分析内容被误判为失败。

至少需要一个有效 panel 结果才能进入 aggregator。Aggregator 最多使用前 8 个有效
结果。超过 8 个的 panel 仍会被执行并计入顶层 token usage，但不会进入 aggregator
prompt。

### 8.4 Panel 答案渲染

有效结果按以下格式渲染：

```text
回答1：
{content}

回答2：
{content}
```

具体合同为：

- 编号从 1 开始；
- 使用全角冒号；
- 标题后一个换行；
- 回答块之间两个换行；
- 不包含 panel 模型名；
- 最多渲染 8 条。

### 8.5 Aggregator 请求

Aggregator 请求同样从完整原始请求 clone 得到，然后执行：

1. `model` 替换为 aggregator 配置模型。
2. `stream` 与客户端模式一致：非流请求设为 `false`，流式请求设为 `true`。
3. Aggregator 配置了 temperature 时使用配置值，否则继承客户端 temperature。
4. 保留客户端 max tokens 和 metadata。
5. 保留客户端原始 tools。
6. Tools 为空时清除 `tool_choice`，否则保留客户端原始值。
7. 在完整原始 messages 末尾追加一条合成 prompt。

原始问题取 messages 中最后一条 `user` 消息的文本内容。对于 multipart content，
只拼接文本和 reasoning 部分，忽略非文本部分。

合成 prompt 根据 tools 状态选择：

- 有 tools：使用 `direct_synthesis_reference_context.txt`。
- 无 tools：使用 `self_moa_aggregator.txt`。

模板填入原始问题和 panel 答案后，再追加 `output_contract.txt`。输出合同要求
aggregator：

- 生成当前对话的下一条 assistant response；
- 不暴露 panel 或内部编排过程；
- 需要继续执行动作时返回符合 schema 的 tool call；
- 遵守原始对话的任务目标和输出协议。

### 8.6 非流式最终响应

Aggregator 成功时，其完整 `ChatCompletionResponse` 作为基础响应，包括：

- 实际响应模型名；
- assistant content；
- tool calls；
- finish reason；
- system fingerprint 等 provider 字段。

随后 Gateway 将顶层 usage 替换为所有成功子调用的聚合 usage，并对 reasoning
content 做 OpenAI 协议归一化。

### 8.7 流式最终响应

流式请求仍先等待全部 panel 完成，随后仅对 aggregator 发起真实上游 stream。
Panel 输出不会作为 SSE 事件发送给客户端。

Aggregator 的 usage 可能集中在最后一个 chunk，也可能像 Anthropic 一样拆分在多个
chunk 中。Gateway 按字段保存 aggregator 的最新累计快照，再将 panel usage 加入每个
携带 usage 的 SSE chunk。中间计算使用无符号整数，写入 OpenAI `StreamUsage` 的
`i32` 字段时才做协议边界截断。

如果完整消费的 aggregator stream 没有返回任何 usage，Gateway 不额外构造 usage
chunk，因此客户端响应仍不虚构 usage。Provider accounting 会保留 panel 以及已经
观察到的 aggregator usage，供父请求日志和限额结算使用。

## 9. 失败与取消

### 9.1 失败矩阵

| 场景 | 行为 |
|---|---|
| 部分 panel 调用失败 | 使用剩余有效 panel 继续 |
| Panel 调用成功但输出无效 | 排除该结果，调用用量仍计入实际消耗 |
| 所有 panel 均失败或无效 | Workflow 返回 provider error，不调用 aggregator |
| 非流式 Aggregator 调用失败 | 返回第一条有效 panel 响应 |
| 流式 Aggregator 建流失败 | 将第一条有效 panel 响应转换为聚合 SSE |
| Aggregator stream 建立后中途失败 | 向客户端发送流错误并按已观察到的 usage 结算 |
| Workflow 模型用于 `/v1/messages` | 返回 HTTP 400 |

Aggregator 失败后的降级响应不增加任何非标准标记。非流式返回第一条有效 panel 的
标准响应；流式将该响应转换为标准 SSE chunk。降级响应内容可能无法继续原本依赖工具
的 agent 流程。

### 9.2 请求取消

Panel futures、flow control guard 和 inflight guard 均由 Rust 所有权管理。
客户端取消导致外层 future 被 drop 时，未完成调用会被取消，相关 guard 随之释放。
Aggregator stream 建立后，flow control guard 和 inflight guard 会一直持有到 stream
结束或被客户端取消。

Direct Synthesis 可以为每个 panel 配置独立超时。当前仍没有 workflow 总超时；
aggregator 超时由 provider 请求超时和 flow control 等待超时控制。

## 10. 请求协议

### 10.1 Metadata

`metadata` 由请求的 `extra` 字段保留，用于父请求跟踪和 prompt log。子请求进入
provider 前会显式移除 `metadata`。该清理不能只依赖 serde：
OpenAI 类 provider 会跳过 `extra`，但 Anthropic provider 会显式读取部分
provider-specific extra 字段。

## 11. 用量与父请求结算

### 11.1 父请求

外部 workflow 请求在 counts 维度只提交一次，不因 panel 和 aggregator 数量重复
扣除请求次数。

父 route 在 FusionProvider 成功返回响应或 stream 后提交一次 counts。如果父请求
最终失败，但已有成功 child 调用产生实际 usage 或成本，父 route 仍提交一次 counts，
并结算这些成功 child 的实际消耗。完全没有成功 child 时不提交 counts，也不结算
token 和成本。

### 11.2 Token 用量

顶层 `usage` 是所有 provider 调用成功的子调用 usage 之和，包括：

- prompt tokens；
- completion tokens；
- total tokens；
- cache creation input tokens；
- cache read input tokens；
- prompt token details 中的 cached tokens。

Provider 调用成功但 panel 内容被判定无效时，实际 token 仍会被统计，因为模型调用
已经发生。即使全部 panel 内容无效、父请求最终返回错误，成功 child 的 usage 仍会
通过父 Provider accounting 通道写入错误 audit 和限额结算。

流式响应中，panel usage 在 aggregator 建流前已经确定；aggregator usage 在消费 SSE
时按字段累计。标准 SSE 仍只在 aggregator 返回 usage chunk 时携带聚合 usage；父请求
日志和限额结算还会读取 Provider accounting，因此 stream 中途失败或缺少最终 usage
chunk 时，已经观察到的成功 child usage 不会丢失。

### 11.3 成本边界

Workflow 不建立独立的子调用 audit 记录，也不把 billing model 明细返回客户端。
每个成功 child 按 Router 解析后的真实模型费率计算成本，通过父 Provider accounting
汇总；父 route 将总成本写入现有限额和累计计费体系。父请求 audit 仍只有一条，不展开
为多条 child 请求记录。

## 12. 标准响应 Usage

非流式 Workflow 响应保持标准 OpenAI `ChatCompletionResponse`，不增加顶层扩展
字段。`usage` 是所有返回成功响应的子调用 usage 之和。

流式响应保持标准 OpenAI SSE chunk schema。Aggregator 提供 usage chunk 时，其中的
token 被替换为 panel 与 aggregator 的聚合值；没有 usage chunk 时不额外生成。

## 13. 可观测性

父请求保留一条正常请求日志，记录：

- 客户端请求的 workflow 模型名；
- 状态码和总耗时；
- 聚合后的输入、输出和缓存 token；
- 即使父请求失败，已经成功完成的 child token；
- 客户端 IP、key 和 team；
- prompt log 中的原始 metadata。

子调用通过 tracing 记录 workflow id、角色、配置模型和 deployment id，并复用真实
deployment 的：

- request rate；
- Router 健康状态过滤；
- flow control；
- inflight。

子调用不会各自写入父请求级数据库日志，避免一次 workflow 请求在用户日志中展开
为多条独立请求。

当前不捕获或返回每个子调用的完整 request/response，也不维护 workflow 专用 audit
结构。若后续需要子调用审计，应复用 Gateway 现有 audit 的存储、脱敏、保留周期和权限
模型，并作为独立功能设计。

## 14. 安全边界

- 客户端只能选择公开 workflow 模型名，不能覆盖 workflow id 或角色模型。
- Workflow 模型参与正常模型白名单和 public model 判断。
- 配置校验阻止 workflow 角色直接引用 workflow 模型；运行时还会拒绝任何解析到
  虚拟 Provider 的 child call，避免 alias 或动态配置造成递归。
- Router 保证虚拟 Provider 独占其公开模型的候选集合，运行期新增同名普通
  deployment 也不能绕过 Fusion。
- 启动、reload 和 Dashboard 管理入口拒绝与 workflow 模型同名的 deployment 或 alias。
- `gateway_headers` 是反序列化隔离的内部字段，客户端不能伪造 priority header。
- Metadata 不发送给上游。
- Panel tools 强制为空，避免参考模型执行动作或产生未经 aggregator 审核的工具调用。

## 15. Prompt 资产

Prompt 作为编译期静态资产存放在：

```text
boom-gateway/boom-fusion/src/prompts/
```

包含：

- `reference_advisor.txt`
- `direct_synthesis_reference_context.txt`
- `self_moa_aggregator.txt`
- `output_contract.txt`

这些文件是 workflow 行为合同的一部分。修改 prompt 时必须同步更新：

- Prompt SHA-256 测试；
- canonical request fixtures；
- tools 与无 tools 场景测试；
- panel 排序和渲染测试。

运行时不得从外部路径加载 prompt，避免部署环境和源码环境产生行为差异。

## 16. 测试策略

### 16.1 配置测试

- 正确解析 workflow 配置。
- 拒绝未知 workflow id。
- 拒绝少于两个 panel。
- 拒绝 workflow 角色引用 workflow 模型。
- 拒绝 workflow 角色通过 YAML alias 引用 workflow 模型。
- 拒绝不存在的真实模型或 alias。
- 拒绝非有限 temperature。
- 拒绝为 0 的 panel timeout。

### 16.2 请求变换测试

- 完整保留原始 messages 顺序。
- Tools 场景向 panel 追加 advisor prompt。
- Panel tools 为空且无 tool choice。
- Aggregator 保留原始 tools 和 tool choice。
- Panel temperature 使用角色配置。
- Aggregator temperature 正确继承或覆盖。
- Metadata 在 workflow 内保持不变。
- Provider 边界移除 metadata，但保留其他 provider-specific extra 字段。

### 16.3 执行语义测试

- Panel 并行但按配置顺序渲染。
- 部分 panel 失败时继续。
- 所有 panel 失败时不调用 aggregator。
- Aggregator 失败时返回第一条有效 panel。
- 顶层 usage 等于成功子调用 usage 之和。
- 流式请求只对 aggregator 建立 stream。
- Aggregator 分段 usage 按字段累计，不互相覆盖。
- Aggregator 未返回 usage 时不虚构用量。
- 全部 panel 内容无效时，成功 child usage 和成本仍进入父错误 accounting。
- Panel timeout 只排除超时 panel，并释放对应 guard。
- SSE usage 在 `i32` 协议边界做饱和转换。
- 非流和流式响应都不包含非标准 workflow audit 字段。

### 16.4 Provider 测试

- Metadata 不发送给 provider。
- Gateway 内部 header 正确注入。

### 16.5 端到端验收

正常的 tools 请求应满足：

1. 客户端只发起一个 workflow 请求。
2. Gateway 发起配置数量的无工具 panel 调用。
3. Gateway 发起一个携带客户端 tools 的 aggregator 调用。
4. 最终响应可以包含标准 OpenAI tool calls。
5. 后续带 tool result 的多轮请求仍保持完整消息顺序。
6. 顶层 usage 与全部成功子调用之和一致。
7. 全程不需要额外 sidecar 或 Gateway 自回调。

流式验收还应满足：

1. Panel 子调用全部为 `stream: false`。
2. Aggregator 子调用为 `stream: true`。
3. 客户端持续收到 aggregator SSE content。
4. Aggregator 返回 usage 时，最终 usage 等于 panel 与 aggregator usage 之和。
5. Aggregator 建流失败时返回第一条有效 panel 的聚合 SSE。
6. 请求日志记录 `is_stream=true`，并在收到 usage 时记录聚合 token。

## 17. 当前限制

- 仅实现 `direct_synthesis`。
- `/v1/messages` 不支持 workflow 模型。
- Aggregator 必须等待全部 panel 完成后才能开始。
- Panel 始终使用非流式调用，只有 aggregator 向客户端流式输出。
- 所有客户端响应均保持标准 OpenAI schema，不携带 workflow 专用 audit 字段。
- 最多只有前 8 个有效 panel 结果进入 aggregator。
- 没有 workflow 级总超时、aggregator 独立超时或独立重试策略。
- Aggregator 降级为 panel 响应时，可能无法继续需要 tool call 的任务。
- 成本按成功 child 的真实模型费率汇总，但不记录可查询的子调用 audit 明细。
- 普通请求和 workflow 子调用共享 Router、schedule policy 和真实 Provider；
  父请求的 HTTP/套餐生命周期仍由 route 管理，子调用生命周期由
  `boom-routing/src/fusion.rs` 管理。

## 18. 扩展新 Workflow

新增 workflow 类型时应遵循以下步骤：

1. 在 `boom-fusion` 中实现 `Workflow`。
2. 通过 `ModelInvoker` 执行真实模型，不依赖 `boom-main`。
3. 在 `boom-config` 中增加带类型标签的配置变体。
4. 在 `boom-routing/src/fusion.rs` 的 registry 构造逻辑中注册新实现。
5. 定义请求变换、失败、usage 和工具语义。
6. 增加配置、执行、失败和协议测试。
7. 保持 workflow 模型与真实 deployment 命名空间隔离。

新 workflow 不应绕过父请求权限和套餐检查，也不应在子调用中重复扣除用户配额。
