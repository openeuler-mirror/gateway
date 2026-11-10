# WebSocket Responses 传输层实现设计（基于 vLLM agentic-api 参考实现）

> 状态：设计稿（未实现）
> 日期：2026-11-10
> 参考实现：[vllm-project/agentic-api](https://github.com/vllm-project/agentic-api) `crates/agentic-server/src/handler/websocket/`（Apache-2.0）
> 本文档取代 `websocket-agent-link-design.md`（2026-11-04 自定义协议稿）——参考实现证明了更优路线：**不发明新协议，直接把 Responses API 事件流搬上 WS + `stream_id` 多路复用**，Codex 原生可用。

## 0. 摘要

在 BooMGateway 增加 `GET /v1/responses` 的 **WebSocket 升级端点**，协议与 vLLM agentic-api 完全对齐：

```
Codex / 智能体 ──WS(HTTP/1.1 Upgrade)──> LB(透传) ──> BooMGateway
                                                        ├─ WS 终结 + stream_id 多路复用
                                                        └─ 帧转出后照常走现有 responses
                                                           业务内核(store/rehydrate/转换/SSE 消费)
                                                                                        ──HTTP/SSE──> vLLM
```

核心决策（均来自参考实现的已验证做法）：

1. **协议 = Responses SSE 事件原样上 WS 帧**，每个出站事件附 `stream_id` 路由成员——不造新帧类型，Codex 配 `supports_websockets = true` 即直连
2. **客户端帧只有 `response.create`（+ P2 的 `response.inject` / `response.steer`）**，`type` 是传输层判别符，剥掉后就是普通 Responses 请求体
3. **有界准入 + 车道（lane）FIFO**：并发安全靠预算制（请求数/字节数/车道数/checkpoint 内存全部有上限），不靠信任客户端

## 1. 参考实现解剖（agentic-api 源码要点）

源码树（行数为抓取时快照）：

```
handler/websocket/
├── mod.rs            # 出口：responses_ws / responses_ws_with_auth
├── error.rs          # WsError → {type:"error", status, error:{...}} 帧；部分错误连接级致命
├── responses.rs      # 734 行：升级握手 + 客户端消息分发 + 单请求执行
└── responses/
    ├── connection.rs   # 154 行核心循环：split socket、独立 writer 任务、优雅 draining
    ├── control.rs      # steering/inject 控制（MAX_CONTROLS=32、字节预算、60s 保留）
    ├── event.rs        # WsOutboundEvent：入队前序列化+限长；StreamId(1-256 chars)
    ├── local.rs        # generate:false 的无推理本地完成
    ├── multiplexer.rs  # 394 行：车道 FIFO、有界准入、字节预算、JoinSet
    └── request.rs      # response.create 解析（先判别 type，剥 type 再解析 body）
```

### 1.1 协议（从源码提炼）

**客户端 → 服务端**（JSON 文本帧；二进制帧拒绝：`invalid_request_error`）：

```jsonc
{
  "type": "response.create",      // 传输层判别符（解析前剥离）
  "stream_id": "lane-1",          // 可选，1-256 chars；缺省 = 默认车道
  ...ResponsesCreateRequest       // 普通 POST /v1/responses body 原样展开
}
```

关键处理（`request.rs`）：
- `payload.stream` **强制 true**（WS 上非流式无意义）
- `store` 语义保留——store:true 的轮次照常持久化、可 `previous_response_id` 续接
- `generate: false` → 不调推理，本地合成 `response.created` + `response.completed` 两事件并持久化（`local.rs`）
- P2 扩展消息：`response.inject`（生成中途注入上下文）、`response.steer`（中途改向）

**服务端 → 客户端**：**Responses SSE 事件原样作为 WS 文本帧**（`response.created`、`response.output_text.delta`、`response.completed`…），统一附 `"stream_id"` 成员；错误为：

```json
{ "type": "error", "status": 429,
  "error": { "message": "...", "type": "rate_limit_error", "code": "rate_limit_exceeded" } }
```

错误帧带 HTTP status + OpenAI error 形状——WS 上保持与 HTTP 相同的错误词表（rate_limited 连接保持打开；serialize/send/task 类内部错误为连接级致命，不发帧直接关）。

### 1.2 关键机制（全部有界，这是该实现的生产级内核）

| 机制 | 参考实现取值/做法 | 出处 |
|---|---|---|
| 出站缓冲 | `WS_OUTBOUND_BUFFER=64` 帧；每帧入队**前**序列化并按 `max_stream_event_bytes` 限长（超限拒绝，不进队列） | `event.rs` |
| 帧限长双保险 | executor 侧也按 `limit - stream_id 开销(256B slack)` 验证**终态帧**——"发不出去的 response.completed 不能留下已持久化的 response" | `event.rs executor_limit` |
| 在途请求 | `WS_MAX_OUTSTANDING_REQUESTS=64` | `multiplexer.rs` |
| 在途字节 | `WS_MAX_OUTSTANDING_BYTES=12MiB`（按输入帧字节数预留/释放） | `WsByteBudget` |
| 车道 | `stream_id`（含 None 默认车道）→ 每车道 **FIFO 串行**执行（同车道前一请求完成才调度下一个）；车道数上限 128 | `multiplexer.rs lanes` |
| 会话内存 | checkpoint items 32k / bytes 16MiB / retained 32MiB（`ResponseSessionGroup` 有界组） | `multiplexer.rs` |
| 控制消息 | steering/inject 合计 32 条、12MiB、完成态保留 60s | `control.rs` |
| 限流次序 | **先计费限流、后占容量**——被限流的 create 不占任何容量（"a limited create holds nothing"） | `responses.rs` |
| writer 任务 | 独立 `spawn_writer`：单写者从 mpsc 消费；单帧发送超时 10s、close 超时 5s——写不动的连接不拖垮会话 | `connection.rs` |
| 优雅停机 | shutdown_token 取消 → 丢弃排队、取消在途控制、draining 中新到文本帧丢弃，等全部任务空再退出；客户端断开则 await 所有 lease idle | `connection.rs` |
| 升级前防护 | `max_message_size`/`max_frame_size` 在 **upgrade 前设置**——超限帧由传输层拒绝，不进 JSON 解析 | `responses.rs` |

### 1.3 连接主循环形态（`connection.rs`，select 多路）

```
loop {
  若 shutdown 已取消且未 draining → begin_draining(丢弃队列/取消控制)
  若 draining 且 request_tasks 与 controls 全空 → 退出
  select! {
    shutdown 取消        → begin_draining
    writer 任务结束       → 客户端已断，标记退出
    1s 到期              → controls.expire()（过期清理保留的 steering 完成态）
    控制注册消息          → controls.register
    控制任务完成          → controls.finish
    请求任务完成          → multiplexer.finish（释放字节预算→调度同车道下一个）
    客户端消息            → handle_ws_client_message（限流→容量→解析→入车道）
  }
}
finish: 取消 disposal、等 writer（15s 超时后 abort）、日志收尾
```

## 2. BooMGateway 落地设计

### 2.1 模块布局（遵守 CLAUDE.md 依赖原则）

```
boom-main/src/handler/websocket/          ← 新增，镜像参考实现目录
├── mod.rs          responses_ws 入口（路由注册于 main.rs：GET /v1/responses 双协议：
│                   无 Upgrade 头 → 既有 POST 语义不受影响；有 Upgrade 头 → WS）
├── connection.rs   会话主循环 + writer 任务（WS 依赖只在此层：axum::extract::ws）
├── event.rs        WsOutboundEvent / StreamId / 限长（纯逻辑 → 可上移 boom-core）
├── error.rs        WsError → 错误帧（复用 boom-core GatewayError → HTTP status/error_code 映射）
├── request.rs      response.create 解析（剥离 type/stream_id → 既有 ResponsesRequest）
├── multiplexer.rs  车道 FIFO + 有界准入 + 字节预算
└── local.rs        generate:false 本地完成（P1）
```

纯协议类型（`event.rs`/`error.rs` 的类型层）放 **boom-core**（叶子，零新依赖）；axum/ws 依赖只在 boom-main——与 §1 模块依赖原则一致。**不新建 boom-ws crate**：协议类型并入 boom-core::responses，组装层在 boom-main，避免为两个文件开 crate。

### 2.2 与现有 responses 内核的对接点（复用而非重建）

WS 收到 `response.create` 后，剥壳得到 `ResponsesRequest`，**走与 HTTP 同一条业务路径**：

```
WS 帧 → request.rs 剥壳 → 既有 responses() handler 内核
         （认证上下文来自升级握手，一次认证连接期有效）
         ├─ store rehydrate（previous_response_id）→ boom-responses 原样
         ├─ validate_conversion_supported / namespace 拍平 / loud-drop → 原样
         ├─ charge_and_route（限流按请求计，先计费后占容量）→ 原样
         ├─ Provider::chat_stream → SSE chunk 流 → transcoder 事件
         └─ 事件不进 SSE body，改为 WsOutboundEvent{附 stream_id} → 出站通道
```

需要的内核抽取（一次，防 HTTP/WS 行为分叉）：
- `responses()` 中「认证后 → 事件流」段提取为 `run_responses_stream(req, identity, ...) -> impl Stream<Item=ResponsesSseEvent>`，HTTP 层包 SSE body，WS 层转帧——两传输共用一个业务函数
- 认证：升级握手时执行一次现有认证逻辑（Bearer 头），`AuthIdentity` 存入会话；P0 不做连接中途换钥

各中间面映射：

| 中间面 | 映射 |
|---|---|
| 限流/plan | **按请求**照常走 charge_and_route（先计费后占 WS 容量，对齐参考实现次序） |
| 并发 | 现有 per-key 并发照常 + WS 连接级 `max_outstanding_requests`(64) |
| kvc 前缀亲和 | 零改动（车道串行 → 同会话前缀稳定，亲和天然受益） |
| prompt log / 审计 | 每请求照常；连接 open/close 各记一条会话审计 |
| response store | 原样（store:true 持久化；previous_response_id 跨连接续接不受影响） |

### 2.3 配置项（general_settings.responses_ws，进程级 YAML，§7 权威拆分）

| 键 | 默认 | 说明 |
|---|---|---|
| enabled | false | 端点开关（灰度） |
| max_outstanding_requests | 64 | 连接内在途上限 |
| max_outstanding_bytes | 12582912 | 在途输入字节预算 |
| max_session_lanes | 128 | stream_id 车道数上限 |
| max_stream_event_bytes | 262144 | 单出站帧上限（入队前校验） |
| outbound_buffer | 64 | 出站通道容量（帧） |
| send_timeout_ms | 10000 | 单帧发送超时（写不动即关连接） |
| max_connections_per_key | 4 | 每密钥并发 WS 连接数 |

manifest 原则：5 处同步（boom-config 结构体 / manifest 字段 / Dashboard 不涉及 / boom-main 消费 / 无 migration）。

### 2.4 车道语义（对齐参考实现）

- `stream_id` 缺省 → 默认车道（None key）；同一车道 **FIFO 串行**：第 2 个请求排队，前一个 `done/error` 后调度——**车道内顺序 = HTTP 逐轮语义**，`previous_response_id` 链不会并发竞争
- 不同 `stream_id` 并行（受在途上限约束）——Codex 的多 agent 并行即开多车道
- 车道完成后保留其最新 checkpoint（内存），后续同车道请求免 DB rehydrate（P1 优化；P0 每请求照常走 store）

### 2.5 与 misc/LB 的配合（已实测前提）

- HTTP/1.1 Upgrade 全程透传 ✓；`upstream_read_timeout` 调大（WS 路由 600s）+ 应用层心跳
- 不支持 h2 extended CONNECT（pingora 0.8）——客户端 HTTP/1.1 握手
- 连接钉扎：LB 把 WS 钉在一个网关实例；网关无跨实例会话态（store 在 DB），滚动重启由客户端重连 + `previous_response_id` 续接兜底

## 3. 差异与裁剪（相对参考实现）

| 参考实现有 | 本设计 | 理由 |
|---|---|---|
| response.inject / steer（mid-turn 控制） | **P2 再做** | 依赖生成中途改写上游请求，vLLM 上游无原生支持，实现重；P0/P1 不带 |
| ResponseSessionGroup 内存 checkpoint 组 | P0 每请求走 DB store；P1 引入有界内存车道缓存 | 复用既有 boom-responses，避免新状态层 |
| OIDC principal 过期检测（is_expired 逐帧检查） | P0 连接期密钥状态不再复查；P1 加逐帧过期检查 | boom-auth 为 API key 体系，无 token 过期语义；key 拉黑靠 auth cache 失效，P1 评估 |
| telemetry 模块（per-request span） | 复用现有 trace 体系 | 已有 boom-trace |
| websocket_tracker（全局连接计数） | 用现有 inflight/agent_stats 扩展一维 | 已有设施 |

## 4. 实施切分

| 阶段 | 内容 | 验收 |
|---|---|---|
| **P0** | 端点 + response.create 多路复用 + 车道 FIFO + 有界准入 + 错误帧 + 优雅停机；内核抽取 `run_responses_stream` | misc/Agent 新增 `ws_responses_loop` 检查：单连接双车道并发、同车道顺序、限流帧、超限拒绝、断连重连续接（mock 全绿） |
| **P0.5** | **Codex e2e**：`supports_websockets = true` 指向网关，跑通一次会话（含工具循环、store 续接） | Codex 会话完整可用 |
| **P1** | generate:false 本地完成、车道内存 checkpoint、key 拉黑逐帧生效、连接级配置项 | 长会话压测（40K ctx × 20 轮 WS vs SSE 对比） |
| **P2** | response.inject / steer（需上游支持评估） | 参考实现 control 语义对齐测试 |

依赖：`axum` 的 `ws` feature（tokio-tungstenite 不需要——axum 内置够用且已是既有框架依赖）。

## 5. 关键不变量（实现时的 checklist）

1. 被限流的 create **不占**任何容量（计费在容量预留之前）
2. 超限出站帧**不入队**；终态帧超限在**持久化之前**拒绝（不留孤儿 store 行）
3. 同车道严格 FIFO；车道内看到的历史顺序与 HTTP 逐轮一致
4. `type`/`stream_id` 是传输层成员，剥离后才进 Responses 解析（不污染 provider body）
5. 写超时（10s）是连接级死刑——写不动的客户端必须断开，不能冻结整个会话
6. 优雅停机先丢排队、再等在途；客户端断开则等待全部 lease idle 后才释放连接资源
7. `payload.stream` 强制 true；`store` 语义不变

## 6. 与前版设计（11-04）的差异

| 项 | 前版（自定义协议） | 本版（agentic-api 对齐） |
|---|---|---|
| 帧协议 | 自造 request/delta/done 五类帧 | **Responses SSE 事件原样 + stream_id**——零新协议，SSE 事件契约检验逻辑复用 |
| Codex 兼容 | 无 | **原生**（Codex `supports_websockets` 即此协议） |
| 多路复用 | id 多路、无顺序约束 | stream_id 车道 FIFO——续接链安全 |
| 准入控制 | 只有 max_streams | 请求数/字节/车道/checkpoint 四维预算（生产级） |
| 中途控制 | 无 | P2：inject/steer（参考实现已验证形态） |

## 参考资料

- 源码快照：本机 `/tmp/agentic-src/`（responses.rs 734 行、multiplexer.rs 394 行、connection.rs、event.rs、error.rs、request.rs、local.rs、control.rs）
- [agentic-api README](https://github.com/vllm-project/agentic-api)（`GET /v1/responses` = WebSocket transport；Codex `supports_websockets = true` 配置）
- [Codex WS 续接 issue #72](https://github.com/vllm-project/agentic-api/issues/72)、[stream_id 多路复用 #239](https://github.com/vllm-project/agentic-api/issues/239)、[mid-turn steering #333](https://github.com/vllm-project/agentic-api/issues/333)
- 本仓库：`websocket-agent-link-design.md`（前版，被取代）、misc/LB WebSocket 透传实测记录
