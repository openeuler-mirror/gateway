# 02 模型调用（Invocation）

覆盖：调用协议（OpenAI / Anthropic）、流式行为、模型列表、别名、兜底路由、异常路径。

| ID | 标题 | 角色 | 深度 |
|----|------|------|------|
| INV-01 | 非流式调用返回完整响应与 usage | user | smoke+trial |
| INV-02 | 流式调用返回 SSE 且首 token 及时 | user | smoke+trial |
| INV-03 | Anthropic 原生协议直连 | user | trial |
| INV-04 | 未知模型名走兜底路由 | user | trial |
| INV-05 | 无兜底时未知模型返回 404 | user | trial |
| INV-06 | 模型别名调用 | user | trial |
| INV-07 | 多轮会话上下文保持 | user | trial |
| INV-08 | 长上下文请求正常处理 | user | trial |
| INV-09 | 上游错误信息可读 | user | trial |
| INV-10 | 同名多 deployment 负载均衡 | admin | trial |
| INV-11 | 并发调用均正常返回 | user | trial |
| INV-12 | 客户端提前断连不影响网关 | user | trial |

---

## INV-01 非流式调用返回完整响应与 usage

- **角色**：user　**深度**：smoke+trial
- **前置**：可用 key + 已授权模型（同 ACC-01）。
- **步骤**：发起非流式 chat 请求（`"stream": false` 或缺省）。
- **预期**：HTTP 200；`choices[0].message.content` 非空；`usage` 含 `prompt_tokens` / `completion_tokens` / `total_tokens`，三者满足加和关系。

## INV-02 流式调用返回 SSE 且首 token 及时

- **角色**：user　**深度**：smoke+trial
- **前置**：同 INV-01。
- **步骤**：
  ```bash
  curl -N http://<gateway-host>:4000/v1/chat/completions \
    -H "Authorization: Bearer sk-<key>" \
    -H "Content-Type: application/json" \
    -d '{"model": "<model>", "stream": true, "messages": [{"role": "user", "content": "写一首短诗"}]}'
  ```
- **预期**：
  1. 响应为 `text/event-stream`，逐块收到 `data: {...}` 分片
  2. 首个内容分片体感在秒级到达（对照直连上游的延迟，无数量级劣化）
  3. 流以 `data: [DONE]` 结束
  4. 最后一个内容分片（或紧邻 [DONE] 前的分片）携带 `usage` 字段

## INV-03 Anthropic 原生协议直连

- **角色**：user　**深度**：trial
- **前置**：key 已授权某模型；该模型允许经 `/v1/messages` 调用。
- **步骤**：
  1. 用 Anthropic 客户端（或裸 curl）向 `/v1/messages` 发起请求，头用 `x-api-key: sk-...`，body 为 Anthropic 格式（`max_tokens` / `messages`）：
     ```bash
     curl http://<gateway-host>:4000/v1/messages \
       -H "x-api-key: sk-<key>" \
       -H "anthropic-version: 2023-06-01" \
       -H "Content-Type: application/json" \
       -d '{"model": "<model>", "max_tokens": 256, "messages": [{"role": "user", "content": "Hello"}]}'
     ```
  2. 若试用场景包含 Claude Code / opencode：将客户端 `ANTHROPIC_BASE_URL` 指向网关，正常发起一次对话
- **预期**：
  1. 响应为标准 Anthropic 格式（`content` blocks、`stop_reason`、`usage.input_tokens/output_tokens`）
  2. Claude Code / opencode 无需改代码即可工作
  3. 流式（`"stream": true`）时收到标准 Anthropic SSE 事件序列（`message_start` → `content_block_delta` → … → `message_stop`）

## INV-04 未知模型名走兜底路由

- **角色**：user　**深度**：trial
- **前置**：网关配置了 `"*"` 兜底 deployment；key 的模型权限为空或含 `all-proxy-models`（即不限模型）。
- **步骤**：用一个配置中不存在的模型名（如 `totally-unknown-model`）发起调用。
- **预期**：请求成功（200），实际由兜底 deployment 响应；日志中该请求记录了真实服务的 deployment。**注意**：`"*"` 在本网关中是"兜底路由"目标，不是全权限标记——权限判定依据是 key 的 models 列表（见 ACC-10）。

## INV-05 无兜底时未知模型返回 404

- **角色**：user　**深度**：trial
- **前置**：网关**未**配置 `"*"` 兜底。
- **步骤**：同 INV-04，用不存在的模型名调用。
- **预期**：HTTP 404，`error.type` 为 `model_not_found`，错误信息包含请求的模型名。

## INV-06 模型别名调用

- **角色**：user　**深度**：trial
- **前置**：管理员已配置别名 `fast-model` → 真实模型 `model-a`，且 key 对 `model-a` 有权限。
- **步骤**：
  1. 用 `fast-model` 发起调用
  2. 请求 `GET /v1/models`
- **预期**：
  1. 调用成功，行为与直接调 `model-a` 一致；日志中记录的是真实模型 `model-a`
  2. 别名是否出现在模型列表取决于其 hidden 配置（按管理员告知的配置验证）

## INV-07 多轮会话上下文保持

- **角色**：user　**深度**：trial
- **前置**：同 INV-01。
- **步骤**：连续发送三轮对话：第一轮告知"我的代号是 42"；第二、三轮分别追问"我的代号是什么"。
- **预期**：第二、三轮回答能正确说出 42，说明 messages 数组完整送达上游；第三轮的 `usage.prompt_tokens` 明显大于第一轮（上下文累积）。

## INV-08 长上下文请求正常处理

- **角色**：user　**深度**：trial
- **前置**：同 INV-01；模型支持较大上下文。
- **步骤**：构造约 50K 字符的输入（如粘贴长文），发起非流式请求。
- **预期**：正常返回（200），`usage.prompt_tokens` 与输入长度数量级相符；响应延迟相对短输入合理增长，无中断或网关层超时。

## INV-09 上游错误信息可读

- **角色**：user　**深度**：trial
- **前置**：管理员配合将某模型的 mock 上游切换为返回 500（或暂停上游进程）。
- **步骤**：试用者用该模型发起调用。
- **预期**：HTTP 502（上游故障类），错误信息能区分"网关问题"与"上游问题"（如指明上游返回错误），不暴露内部堆栈；上游恢复后再调用即恢复 200。

## INV-10 同名多 deployment 负载均衡

- **角色**：admin　**深度**：trial
- **前置**：同一 `model_name` 配置两个 deployment（指向两个不同 mock 上游，响应内容可区分，如分别带标记 `node-a` / `node-b`）。
- **步骤**：连续发起 ≥10 次非流式调用，记录每次响应的标记。
- **预期**：请求分布到两个 deployment（轮询策略下大致交替），无全部压到单节点的情况；面板 Stats 中两个 deployment 均有计数。

## INV-11 并发调用均正常返回

- **角色**：user　**深度**：trial
- **前置**：同 INV-01；key 并发额度 ≥ 5。
- **步骤**：用脚本（或多个终端）同时发起 5 个请求。
- **预期**：5 个请求全部 200，无超时或 429（若触发 429 说明并发额度配置与预期不符，记录实际额度）；每个响应内容独立完整。

## INV-12 客户端提前断连不影响网关

- **角色**：user　**深度**：trial
- **前置**：同 INV-02。
- **步骤**：发起流式调用，收到首个分片后立即 `Ctrl+C` 中断客户端。随后立刻再发起一次正常调用。
- **预期**：后续调用正常 200；面板 In-Flight 视图中在途数回落（断连的请求最终计为完成，不永久挂起）；日志中该请求有记录，duration 合理。
