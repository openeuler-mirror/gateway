# BooMGateway 架构设计文档

> **版本**：v2 · 2026-07
> **状态**：生产验证中（L2 PromptPrefix 🟡 / L0/L1/L3 ✅ / L4 AgentAffinity 🔄 roadmap）
> **配套文档**：
> - [用户感知模块](./user-facing-modules.md) — 面向业务方的功能描述
> - [内部请求流](./internal-request-flow.md) — 面向开发者的逐行代码追踪
> - [KV-Cache 设计](./kvc-aware-design.md) — L3 KvcAware 完整设计
> - [架构图集](../boom-gateway/ppt/boom-arch-2p/architecture-diagram.md) — PPT 配套字符画

---

## 1. 项目定位

**Intelligence Boom Gateway** 是一个高性能 LLM API 网关，用 Rust workspace 实现，Docker 容器化部署。

核心目标：

- **协议兼容**：同时兼容 OpenAI `/v1/chat/completions` 与 Anthropic `/v1/messages` 协议，Claude Code 等 Anthropic 客户端可直接接入
- **密钥兼容**：复用 litellm 的 `LiteLLM_VerificationToken` / `LiteLLM_TeamTable` 表，平滑迁移已有 litellm 部署
- **精细调度**：自建多层调度策略（L0 RR / L1 KeyAffinity / L2 PromptPrefix / L3 KvcAware / L4 AgentAffinity），针对 vLLM 后端的 KV cache 特性做亲和优化
- **零停机运维**：SIGHUP / HTTP 触发热加载，配置变更不影响 in-flight 请求
- **完整可观测**：审计日志（DB）、prompt log（JSONL/Kafka 规划）、Dashboard 管理 UI 全套自建

非目标：

- ❌ 不替换 litellm 的密钥管理 UI（密钥仍由 litellm 创建，本网关只读 + 兼容校验）
- ❌ 不直接对接训练框架（仅作为推理 API 网关）
- ❌ 不实现自有模型协议（仅做 OpenAI/Anthropic 协议转码）

---

## 2. 架构设计哲学

下列 8 条原则是本项目迭代开发的硬约束。任何新功能或重构必须先通过这 8 条的检验。

### 2.1 单向无环依赖

```
boom-core (叶子) ← 所有 boom-* 模块
boom-main (根)   → 依赖所有 boom-* 模块
其他模块互相之间不直接依赖
```

- `boom-core` 是唯一的叶子依赖，定义核心 trait（`Provider`、`Authenticator`、`RateLimiter`、`KvIndexBackend`）和公共类型
- `boom-main` 是唯一的根，负责组装所有模块、管理生命周期
- 中间层模块（auth/limiter/routing/...）只依赖 `boom-core`，不互相依赖
- **禁止**：在中间层模块之间引入直接依赖（会形成循环）

### 2.2 DB 表所有权

每个模块拥有且只操作自己的表。跨模块直接操作他人的表是禁止的。

| 模块 | 拥有的表 | 权限 |
|------|----------|------|
| boom-auth | `LiteLLM_VerificationToken`, `LiteLLM_TeamTable` | 只读（litellm 兼容） |
| boom-audit | `boom_request_log` | 读写 |
| boom-routing | `boom_model_deployment`, `boom_model_alias` | 读写 |
| boom-limiter | `boom_rate_limit_state`, `boom_key_plan_assignment`, `boom_rate_limit_plan` | 读写 |
| boom-dashboard | `boom_config` | 读写 |

跨模块写表的需求 → 通过 `AdminCommand` mpsc channel 异步通知 `boom-main` 代办。

### 2.3 三种状态生命周期

AppState 顶层字段按 reload 行为分为三类，**新增字段时必须明确归入哪一类**：

```
AppState (Clone, 整生命存活)
  ├─ inner: Arc<ArcSwap<AppStateInner>>    ← ① 热替换
  │    ├─ config
  │    ├─ auth
  │    └─ health
  │
  ├─ db_pool                               ← ② 跨 reload 内容存活
  ├─ deployment_store (DashMap)
  ├─ alias_store (DashMap)
  ├─ plan_store (DashMap)
  ├─ limiter (SlidingWindowLimiter)
  ├─ flow_controller
  ├─ inflight (InFlightTracker)
  ├─ rebalance_move_tracker
  ├─ request_rate
  ├─ agent_stats
  │
  └─ kv_index: Arc<ArcSwap<Option<…>>>     ← ③ 重建即清缓存
       └─ kvc_aware 配置一变 → 旧 trie drop → 新 subscriber 从空开始
```

**规则**：
- 可变配置（会被热加载替换）→ 放入 `AppStateInner`，ArcSwap 原子交换
- 持久化状态（counter、assignment、deployment）→ AppState 顶层 Arc
- 第三种生命周期（kv_index）：kvc_aware 配置变化即重建空 trie，瞬态降级到 lowest-load 是有意的"清缓存"语义

### 2.4 mpsc 异步解耦

`boom-dashboard` 需要执行写操作（创建模型、修改配置）时，**不能**直接调用 `boom-provider` 或 `boom-config`。设计模式：

```
Dashboard API handler
    ↓ AdminCommand::CreateModel { ... }
mpsc channel
    ↓
boom-main::admin_command_handler
    ↓ 拥有所有模块的完整访问权
操作对应模块的 store
```

- `AdminCommand` enum 在 `boom-dashboard` 中定义
- 处理逻辑在 `boom-main::admin_command_handler` 中实现
- 这确保 `boom-dashboard` 不需要依赖 `boom-provider` 和 `boom-config`

### 2.5 RAII Drop 链精确计费

流式响应的 duration / token usage / 审计日志不能在请求开始时记录（流式可能持续数分钟）。所有"请求结束才能确定"的副作用通过 RAII 包装器在 Drop 时回写：

```
provider.chat_stream(req)
    ↓
UsageTracker           ← 计数 prompt/completion tokens
    ↓
InFlightStream         ← Drop 时 inflight counter -= 1
    ↓
FlowControlledStream   ← Drop 时释放 FC queue slot
    ↓
GuardedStream          ← Drop 时 plan_charge.commit() 或 settle()
    ↓
LoggedStream           ← Drop 时写 boom_request_log (boom-audit)
    ↓
PromptLogStream        ← Drop 时写 JSONL (boom-promptlog)
    ↓
Sse<...>               ← axum 响应流
```

每层包装器只关心自己的 Drop 副作用，组合起来形成完整的"请求收尾"链。

### 2.6 PlanCharge 状态机

`boom-limiter` 的限流检查返回 `PlanCharge`，它是一个显式状态机：

```
PlanCharge::new()      ← check_plan_limits 创建，占用 concurrency 槽
    ↓
    ├─ 请求成功 → commit()    ← 记录 token usage，扣 total_token/total_cost
    ├─ 请求失败 → settle()    ← 释放 concurrency 槽，不扣 total
    └─ 请求未到 provider → settle()  ← 未服务不计费
```

**未服务不计费**是核心契约：如果请求在 `boom-flowcontrol` 或 `boom-provider` 阶段被拒绝（队列满、后端不可达），PlanCharge 必须 settle 而非 commit，用户的配额不能扣。

### 2.7 ArcSwap 原子热加载

热加载通过 `Arc<ArcSwap<T>>` 实现 lock-free 原子交换：

- 新请求立即看到新配置（load 新 inner）
- in-flight 请求继续用旧配置（持有旧 Arc）
- 旧 Arc 在最后一个引用释放时自动 drop
- 零停机、无锁、无竞态

**热加载规则**：
- SIGHUP / `POST /admin/config/reload` 触发
- `store_model_in_db=true`：只重新 seed `source='yaml'` 的 DB 行
- `store_model_in_db=false`：从 YAML 重建所有 store
- 不得清除运行时计数器（limiter、concurrency、assignment 不受影响）
- kv_index 是例外（见 §2.3 第三种生命周期）

### 2.8 反馈式自愈

后端慢 / 故障时不依赖人工介入，网关自动调整流量分配：

```
boom-flowcontrol.queue 满
    ↓ load_norm 信号 (VIP/普通队列长度归一化)
boom-routing::KvcAwarePolicy
    ↓ 折中 KV 命中率与负载
RebalanceMoveTracker
    ↓ 累计 rebalance 流量
KeyAffinityPolicy / KvcAwarePolicy
    ↓ 慢后端旁路（自动减少分发）
```

慢后端的"恢复"也由同样的信号驱动：队列恢复正常后，load_norm 下降，policy 重新把流量分配回来。

---

## 3. 模块矩阵（13 个 crate）

| Crate | 职责 | 依赖（boom-*） | DB 表 |
|-------|------|----------------|-------|
| **boom-core** | 核心 trait + 公共类型 + Anthropic↔OpenAI 转码实现 | （无） | （无） |
| **boom-auth** | 密钥认证：SHA-256 + moka 缓存 + master_key | core | LiteLLM_* (只读) |
| **boom-config** | YAML + env 解析 | core | （无） |
| **boom-provider** | 创建 Provider 实例（OpenAI/Anthropic/Bedrock 工厂） | core | （无） |
| **boom-limiter** | PlanStore + SlidingWindowLimiter + ConcurrencyGuard + PlanCharge | core | boom_rate_limit_* |
| **boom-routing** | DeploymentStore + AliasStore + Router + Phase A/B 策略 | core | boom_model_* |
| **boom-flowcontrol** | FlowController + VIP/普通双队列 + load_norm | core | （无） |
| **boom-audit** | boom_request_log 读写 | core | boom_request_log |
| **boom-promptlog** | 完整 req/resp JSONL 写入（Kafka 🔄 规划） | **（无，完全独立）** | （无） |
| **boom-kvindex** | ZMQ SUB + TokenPrefixIndex (Trie) + TokenizerPool | core | （无） |
| **boom-ctxaware** | AgentStatsTracker + ClientKind 分类（L4 基础） | core | （无） |
| **boom-dashboard** | Web UI + REST API + JWT 认证 + AdminCommand | core, flowcontrol, limiter, routing, ctxaware, audit, promptlog | boom_config |
| **boom-main** | axum 路由 + AppState 组装 + 后台任务 + 热加载 | **全部** | （聚合层） |

**关键观察**：
- `boom-promptlog` 是唯一不依赖 `boom-core` 的功能 crate（设计上是纯 IO 写入，不需要 trait 抽象）
- `boom-dashboard` 依赖 7 个 boom-* 模块，但**不**依赖 `boom-provider` 和 `boom-config`（解耦硬约束）
- `boom-main` 是唯一依赖所有模块的根

---

## 4. 模块依赖 DAG

```
                       ┌─────────────────┐
                       │   boom-main      │ ← 根（组装层）
                       │  routes+state   │
                       └─┬──┬──┬──┬──┬──┬┘
                         │  │  │  │  │  │
        ┌────────────────┘  │  │  │  │  └──────────────┐
        ▼          ▼        ▼  ▼  ▼                   ▼
    boom-auth  boom-cfg  boom-routing  boom-flowcontrol boom-promptlog
        │          │        │  │  │     │               (独立)
        │          │        │  │  │     │
        │          │     ┌──┘  │  └──┐  │
        │          │     ▼     ▼      ▼  │
        │          │  boom-   boom-   boom-
        │          │  limiter kvindex ctxaware
        │          │     │     │        │
        │          │     ▼     ▼        ▼
        │          │  ┌──────────────────┐
        │          │  │ boom-dashboard    │ ← 依赖 7 模块（不含 provider/config）
        │          │  └──────────────────┘
        │          │
        ▼          ▼
       ┌──────────────────┐
       │  boom-audit       │
       └────────┬──────────┘
                │
                ▼
            ┌────────┐
            │  boom-  │
            │ core    │ ← 叶子（traits + types + 转码）
            └────────┘

    boom-provider  → 依赖 boom-core（叶子）
```

---

## 5. 数据流（6 条主要流）

### 5.1 请求流（主流程）

```
LLM Client (HTTPS)
    ↓ /v1/chat, /v1/messages
boom-main::routes
    ↓ ① 认证
boom-auth (SHA-256 + moka + master_key + ↔ LiteLLM_*)
    ↓ ② 权限
boom-routing::check_model_access (白名单 + alias + "*" 兜底)
    ↓ ③ Phase A 模型层解析（boom-sched）
boom-routing::resolve_request_model (HybridRouter + TierClassifier)
    ↓ ④ 配额
boom-limiter::check_plan_limits (返回 PlanCharge)
    ↓ ⑤ tokenize
boom-kvindex::TokenizerPool (tokenize_openai → prefix hash)
    ↓ ⑥ Phase B 后端选择（boom-sched）
boom-routing::select_provider_with_prefix (L0/L1/L2/L3 Policy)
    ↓ ⑦ KV 反馈
boom-kvindex::need_full_kv_report (命中率低 → 全量上报)
    ↓ ⑧ 流控
boom-flowcontrol::acquire_fc_guard (VIP/普通队列 + max_inflight)
    ↓ ⑨ 协议调用
boom-provider::chat_stream (HTTPS + Anthropic↔OpenAI 转码)
    ↓
vLLM worker (HTTPS /v1/chat/completions)
    ↓
RAII 流包装链（Drop 时回写）
    ↓
SSE 响应 → Client
```

详见 [internal-request-flow.md](./internal-request-flow.md)。

### 5.2 KV 事件反馈流

```
vLLM worker-1..N
    ↓ ZMQ PUB (topic: kv@, msgpack 3-frame)
        Frame 1: token_ids (Vec<u32>)
        Frame 2: block_hashes (Vec<u64>)
        Frame 3: block_meta (序列化)
boom-kvindex::spawn_kv_subscriber
    ↓ ZMQ SUB (config: zmq_endpoints, zmq_topic_prefix)
TokenPrefixIndex (Trie 插入)
    ↓
查询时：KvcAwarePolicy.select(...)
    ↓ 查询 Trie，得到每个 candidate deployment 的命中率
    ↓ load_norm (boom-flowcontrol 信号) + cache_hit_rate 加权
选择最高得分的 deployment
    ↓
低命中率 → need_full_kv_report=true
    ↓
下一轮 vLLM 上报更多 block_hashes → Trie 重建
```

### 5.3 热加载流

```
SIGHUP / POST /admin/config/reload
    ↓
AppState::reload() (60s timeout + catch_unwind 包装)
    ↓
reload_inner():
    1. 重读 config.yaml (boom-config::load_config)
    2. 检测 database_url 是否变更
    3. 重建 stores（YAML 优先 + DB-only 叠加）
       - deployment_store.clear() + 重建
       - alias_store.clear() + 重建
       - plan_store.clear_plans() + 重建
       - flow_controller.retain_slots()
    4. kvc_aware 配置签名对比
       - 未变 → 保留 trie + subscriber
       - 变更 → stop_kv_subscriber + 重建空 trie + spawn 新 subscriber
    5. router.set_policy(new_policy) (重建 Phase B 策略)
    6. router.set_classifier(...) (重建 Phase A)
    7. plan_store.cleanup_assignments() (清理孤儿分配)
    8. prompt_log_writer.update_config(...)
    9. build_inner(new_config, ...) → AppStateInner
    10. self.inner.store(Arc::new(new_inner))  ← 原子 swap
```

**关键安全保证**：
- 整体 60s timeout + catch_unwind，确保 SIGHUP 永远不会 hang 进程
- 早期 Err 不 swap，旧 inner 继续路由流量
- 慢 DB 步骤用 `with_db_timeout(15s)` 包装

### 5.4 Dashboard 写流

```
管理员浏览器
    ↓ HTTPS (JWT 认证)
boom-dashboard::routes
    ↓ 验证 + 提取参数
AdminCommand::CreateModel { name, litellm_params, ... }
    ↓
tokio::sync::mpsc channel
    ↓
boom-main::admin_command_handler (后台任务)
    ↓ 拥有 AppState 完整访问权
根据 AdminCommand 分支：
    - CreateModel        → deployment_store.add + DB upsert
    - UpdateAlias        → alias_store.set_alias + DB upsert
    - CreatePlan         → plan_store.upsert_plan + DB upsert
    - AssignPlan         → plan_store.assign (key/team)
    - UpdateConfig       → boom_config + reload 触发
    - ... (具体变体见 AdminCommand enum)
    ↓
返回操作结果 → Dashboard 渲染
```

### 5.5 RAII 计费流（Drop 时回写）

```
provider.chat_stream(req) 返回 Stream
    ↓
UsageTracker::new(stream, usage_state)
    ↓
InFlightStream::new(stream, inflight_guard)
    ↓ Drop 时：inflight -= 1（boom-routing::InFlightTracker）
FlowControlledStream::new(stream, fc_guard)
    ↓ Drop 时：释放 queue slot（boom-flowcontrol）
GuardedStream::new(stream, plan_charge_take_guard)
    ↓ Drop 时：plan_charge.commit() 或 settle()（boom-limiter）
LoggedStream::new(stream, ctx_for_log)
    ↓ Drop 时：INSERT boom_request_log（boom-audit）
PromptLogStream::new(stream, ctx_for_promptlog)
    ↓ Drop 时：写 JSONL（boom-promptlog）
    ↓
axum::response::Sse<...>
    ↓
客户端
```

**为什么必须 Drop 链**：流式请求从首个 chunk 到结束可能持续几分钟。如果计费/日志在请求开始时记录，duration 不准；如果在外层 handler 末尾记录，已经 return 给客户端的 stream 无法等到结束。Drop 是唯一可靠的"流真正结束"时机。

### 5.6 反馈式背压流

```
某 deployment 队列堆积（boom-flowcontrol.queue_len 接近上限）
    ↓
load_norm = queue_len / max_inflight (归一化到 [0, 1])
    ↓
boom-routing::KvcAwarePolicy.select(...)
    ↓ 加权计算：
       score = cache_weight * hit_rate
             - load_weight * load_norm      ← 这一项让负载高的后端得分下降
             + tier_weight * tier_match
    ↓
慢后端 score 低 → 不会被选中
    ↓
KeyAffinityPolicy 触发 rebalance：
    RebalanceMoveTracker.record_move(from, to)
    ↓
慢后端逐渐旁路（流量自动迁移到快后端）
    ↓
慢后端恢复 → queue_len 下降 → load_norm 下降 → score 回升 → 流量回流
```

**注意**：整个自愈闭环不依赖外部健康检查。`boom-main` 的 `health_monitor` 后台任务负责 hard failover（连续失败 N 次标记下线），而软负载均衡完全由 in-flight 的 load_norm 信号驱动。

---

## 6. 调度层 boom-sched（核心品牌能力）

boom-sched 是抽象概念，物理上由 `boom-routing` 的两次调用组成：

### 6.1 Phase A · 模型层解析

```rust
// boom-routing::Router::resolve_request_model
fn resolve_request_model(&self, requested: &str, messages: &[Message], tools: &[Tool])
    -> ResolvedModel
```

职责：把客户端请求的 `model` 字段解析为最终的 `model_name`。

- **HybridRouter**：基于 messages/tools 内容做分类（用 `TierClassifier`），决定走哪个 tier 的 target_model
- **AliasStore**：alias → target_model 一对一映射
- **"*" catch-all**：未匹配任何 model_name 时，路由到 `"*"` 对应的 deployment

⚠️ 注意：`"*"` 在本网关中是真实的 model_name（兜底路由），**不是** litellm 的"全权限"通配符。判断"全权限"的唯一依据是 `models` 数组为空或包含 `"all-team-models"`。

### 6.2 Phase B · 后端选择

```rust
// boom-routing::Router::select_provider_with_prefix
fn select_provider_with_prefix(&self, model: &str, prefix: &[u8], ...)
    -> Selection
```

职责：在 `model_name` 对应的多个 deployment 中选一个。

**抽屉式策略**（`StrategyRegistry`，按 `schedule_policy` 配置选择）：

| 策略 | 状态 | 决策依据 |
|------|------|----------|
| L0 RoundRobin | ✅ | 简单轮询 |
| L1 KeyAffinity | ✅ | API key 哈希 + RebalanceMoveTracker 反馈 |
| L2 PromptPrefix | 🟡 生产验证 | prompt 前 N token 哈希亲和 |
| L3 KvcAware | ✅ | TokenPrefixIndex Trie 命中率 + load_norm |
| L4 AgentAffinity | 🔄 Roadmap | boom-ctxaware 的 AgentStats 信号 |

详见 [kvc-aware-design.md](./kvc-aware-design.md)。

### 6.3 为什么 Phase A 和 Phase B 之间夹着其他模块

代码顺序：access check → Phase A → **boom-limiter** → **boom-kvindex tokenize** → Phase B → ...

这并非设计缺陷，而是 boom-sched 是抽象分类，不是连续代码块：
- Phase A 决定 `model_name`（语义层）
- Phase B 决定 `deployment`（实例层）
- 中间的 limiter / tokenize 依赖 Phase A 的输出（model_name）才能工作，且为 Phase B 提供输入（token prefix hash）

---

## 7. 关键机制汇总

### 7.1 双层配额（key + team）

```
请求 (key_id)
    ↓
key 属于某个 team
    ↓
配额检查：
    ├─ team 维度：team_plan.team_concurrency / team_rpm / ...
    │  （外箱：team 整体上限）
    └─ key 维度：key_plan.concurrency / rpm / ...
       （内箱：单 key 上限）

3 级回退：
    1. key 显式分配的 plan
    2. team 显式分配的 plan
    3. default_plan / default_team_plan

PlanCharge 双层同时持有：commit 时双扣，settle 时双退
```

### 7.2 Anthropic ↔ OpenAI 转码

转码实现位于 `boom-core`，被 `boom-main::routes` 在入口和出口处调用：

```
OpenAI client → /v1/chat → 内部 canonical form → provider
                                              ↓
                                              OpenAI 后端：直接发
                                              Anthropic 后端：转 Anthropic schema

Anthropic client → /v1/messages → 内部 canonical form → provider
                                                       ↓
                                                       Anthropic 后端：直接发
                                                       OpenAI 后端：转 OpenAI schema

响应路径反向转码 + SSE 事件流重建
```

**Billing 清洗**：Anthropic 在响应 header 中带有 billing hash，转码到 OpenAI 协议时会被剔除，避免上游计费系统重复计算。

### 7.3 ZMQ KV 事件协议

```
vLLM worker → ZMQ PUB
  topic: kv@<deployment_id>
  payload: msgpack 3-frame
    Frame 1: token_ids    : Vec<u32>
    Frame 2: block_hashes : Vec<u64>
    Frame 3: block_meta   : 序列化结构

boom-kvindex subscriber → ZMQ SUB (topic prefix: kv@)
  → 解码 msgpack
  → 插入 TokenPrefixIndex Trie
  → 服务后续 KvcAwarePolicy 查询
```

Trie 的查询是 O(prefix_length)，可以快速给出每个 candidate deployment 的命中率。

### 7.4 后台任务（AppState 顶层持有）

`boom-main` 启动时 spawn 多个后台任务：

- **health_monitor**：周期性检查 deployment 健康（基于 `boom-flowcontrol` 错误率 + 显式 ping）
- **admin_command_handler**：消费 mpsc channel 中的 AdminCommand
- **request_rate_logger**：周期性打印 request_count 摘要
- **prompt_log_flusher**：刷盘 prompt log JSONL
- **kv_subscriber**（可选）：ZMQ SUB 接收 vLLM KV 事件
- **dashboard_stats_aggregator**：周期性聚合统计数据供 Dashboard 查询

这些任务通过 `AppState` 共享状态，通过 `mpsc::channel` / `broadcast::channel` 通信，不互相阻塞。

---

## 8. 扩展点

### 8.1 新增 Provider

1. 在 `boom-provider/src/providers/` 下新增模块
2. 实现 `boom_core::provider::Provider` trait
3. 在 `boom_provider::create_provider` 的 match 分支中添加新 model prefix
4. 配置 `model_list[].litellm_params.model` 使用新 prefix

不需要修改 `boom-main` / `boom-routing` / `boom-limiter` 任何代码。

### 8.2 新增调度策略

1. 在 `boom-routing/src/policy/` 下新增模块
2. 实现 `SchedulePolicy` trait
3. 在 `StrategyRegistry` 中注册
4. 在 `boom-main::state::create_policy` 的 match 中添加分支
5. 配置 `router_settings.schedule_policy` 使用新策略

### 8.3 新增业务模块

新增 `boom-{name}/` crate 时：

- [ ] `Cargo.toml` 依赖只包含 `boom-core`（或明确确认的单向依赖）
- [ ] 不引入对其他 boom-* 模块的循环依赖
- [ ] DB 表（如有）以 `boom_` 前缀命名，DDL 只在本 crate 内
- [ ] 公共 API 通过 `pub use` 在 `lib.rs` 明确导出
- [ ] `boom-main` 的 `Cargo.toml` 和 `state.rs` 已更新以引入新模块
- [ ] 如需 Dashboard 写操作 → 在 `AdminCommand` enum 加变体 + `admin_command_handler` 加分支
- [ ] 如需 KV/计数器跨 reload 存活 → 加到 `AppState` 顶层 Arc

### 8.4 新增 Dashboard 写操作

1. 在 `boom-dashboard::AdminCommand` enum 中添加变体
2. 在 `boom-main::admin_command_handler` 中添加处理分支（boom-main 拥有所有模块访问权）
3. **不要**在 `boom-dashboard` 中引入对 `boom-provider` 或 `boom-config` 的依赖

---

## 9. 部署形态

### 9.1 单进程部署

```
┌─────────────────────────────────┐
│        boom-main (单进程)         │
│                                 │
│  axum HTTP server (port 4000)   │
│  Dashboard server (port 4001)   │
│                                 │
│  AppState (共享内存)             │
│  ├─ deployment_store            │
│  ├─ plan_store                  │
│  ├─ limiter                     │
│  ├─ flow_controller             │
│  └─ kv_index                    │
│                                 │
│  PostgreSQL (外部)               │
└─────────────────────────────────┘
```

适合中小规模部署（单机 <10k QPS）。

### 9.2 LB + 多实例部署

```
                ┌──────────────┐
                │  Pingora LB  │ ← misc/LB/
                └──────┬───────┘
                       │
       ┌───────────────┼───────────────┐
       ▼               ▼               ▼
   ┌────────┐     ┌────────┐     ┌────────┐
   │ boom-  │     │ boom-  │     │ boom-  │
   │ main-1 │     │ main-2 │     │ main-N │
   └───┬────┘     └───┬────┘     └───┬────┘
       │              │              │
       └──────────────┼──────────────┘
                      ▼
              ┌──────────────┐
              │ PostgreSQL   │ ← 共享 DB
              └──────────────┘
```

⚠️ **多实例限制**：
- `boom_rate_limit_state` 通过 DB 共享（粗粒度限流可用）
- `inflight` / `kv_index` 等内存状态**不共享**（细粒度限流会有误差）
- `kv_subscriber` 每个 boom-main 实例独立订阅（都会建自己的 Trie）
- Dashboard 配置变更通过 DB 传播（需配合 SIGHUP / `/admin/config/reload`）

### 9.3 容器化

`Dockerfile`（标准 multi-stage build）：

```dockerfile
# Build stage
FROM rust:1.78 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --bin boom-main

# Runtime stage
FROM debian:bookworm-slim
COPY --from=builder /app/target/release/boom-main /usr/local/bin/
COPY config.yaml /etc/boom/
EXPOSE 4000 4001
CMD ["boom-main", "--config", "/etc/boom/config.yaml"]
```

---

## 10. 性能与可靠性

### 10.1 性能特性

- **零拷贝热加载**：ArcSwap 无锁原子 swap，reload 期间无任何请求阻塞
- **moka 缓存**：5min TTL 的密钥认证缓存，单次 auth 查询 < 1μs
- **DashMap 无锁并发**：deployment_store / plan_store 等热路径 store 用 DashMap
- **流式响应零缓冲**：Provider 的 chat_stream 直接 pipe 到 Sse，不缓冲完整响应
- **mpsc 解耦**：Dashboard 写操作不阻塞 forwarding 路径

### 10.2 可靠性特性

- **PlanCharge 未服务不计费**：用户配额不会因为后端故障被误扣
- **catch_unwind + 60s timeout**：热加载永远 hang 不死进程
- **with_db_timeout(15s)**：单步 DB 操作慢不会拖死整体 reload
- **反馈式背压**：慢后端自动旁路，无需人工干预
- **Dashboard DB pool 独立**：max=3 的 dashboard_db_pool 不会被重型统计查询拖死 forwarding（max=30）

---

## 11. 关键文件索引

| 路径 | 内容 |
|------|------|
| `boom-gateway/CLAUDE.md` | 项目硬约束、模块依赖原则、热加载规则 |
| `boom-gateway/boom-main/src/state.rs` | AppState 定义、reload 实现、create_policy |
| `boom-gateway/boom-main/src/routes.rs` | `/v1/chat` 主请求路径 |
| `boom-gateway/boom-routing/src/policy/` | L0~L3 调度策略实现 |
| `boom-gateway/boom-kvindex/src/` | ZMQ subscriber + TokenPrefixIndex Trie |
| `boom-gateway/boom-dashboard/src/admin_command.rs` | AdminCommand enum 定义 |
| `docs/user-facing-modules.md` | 用户感知功能详细描述 |
| `docs/internal-request-flow.md` | 内部请求流逐行追踪 |
| `docs/kvc-aware-design.md` | L3 KvcAware 完整设计 |
| `boom-gateway/ppt/boom-arch-2p/` | 架构图 PPT + 字符画 |

---

## 12. 决策记录（为什么这样设计）

### 为什么不复用 litellm 的全部代码

litellm 是 Python 实现，转发性能不足（GIL + async IO 限制）。我们用 Rust 重写转发层，保留 litellm 的密钥管理兼容性（DB schema 兼容），自建所有上层功能（限流、调度、Dashboard、审计、计费）。

### 为什么 boom-promptlog 不依赖 boom-core

`boom-promptlog` 是纯 IO 写入（JSONL file + 未来 Kafka），不需要 trait 抽象或类型共享。让它独立可以：
- 单独被第三方项目复用（如轻量级日志工具）
- 编译时间优化（修改 boom-core 不会触发 boom-promptlog 重编）
- 测试隔离（不需要启动 boom-core 的 mock）

### 为什么 Dashboard 不直接依赖 boom-provider

Dashboard 的"创建模型"操作本质上是配置变更，不是 provider 实例化的语义。让 Dashboard 依赖 boom-provider 会：
- 引入 tokio/reqwest 等重依赖到 Web UI
- 破坏"Dashboard 是配置层，boom-main 是运行层"的边界
- 让 boom-provider 的修改可能影响 Dashboard 编译

通过 AdminCommand channel 解耦后，Dashboard 只需要定义意图，由 boom-main 解释并执行。

### 为什么 kv_index 是第三种生命周期

KV Trie 是 vLLM block 事件的缓存。如果像 deployment_store 那样跨 reload 保留：
- kvc_aware 配置变化（如 block_size 改了）→ 旧 Trie 数据不匹配
- 查询命中率会得到错误的高分（基于错误 block_size）
- 流量被错误分配

"重建即清缓存"语义保证：kvc_aware 配置一变，旧 Trie 立即 drop，新 Trie 从空开始重新学习。瞬态降级到 lowest-load 是显式的"我不知道命中率"安全行为。

### 为什么用 RAII Drop 链而不是显式 finalize

显式 finalize（在 handler 末尾调用 `finalize_request(...)`)的问题：
- 客户端提前断开连接时，handler 可能在某个 await 点被 cancel
- panic 时 finalize 不会被调用
- 多个早返回点容易遗漏

RAII Drop 链的优势：
- 编译器保证：无论 stream 如何结束（正常完成 / 错误 / panic / 客户端断开），Drop 都会执行
- 每层只关心自己的副作用，组合天然
- 新增计费维度时只需新增一层 wrapper，不修改现有逻辑

---

## 13. 后续 Roadmap

| 项 | 状态 | 说明 |
|----|------|------|
| L2 PromptPrefix 生产验证 | 🟡 进行中 | 需要更多真实流量验证命中率 |
| L4 AgentAffinity | 🔄 规划 | 基于 boom-ctxaware 统计，对特定 agent 客户端做 prefill 节点亲和 |
| Kafka prompt log 外吐 | 🔄 规划 | 替代/补充 JSONL 文件方案 |
| 多实例细粒度限流 | 🔄 规划 | 共享 inflight / 速率计数器到 Redis |
| Shadow reload（无 partial mutation） | 🔄 规划 | 当前 reload 内部步骤失败会留下 partial-built stores，需 shadow-build + atomic swap |
| 前缀感知 prefill 隔离 | 🔄 规划 | L4 的延伸：把特定 agent 调度到独立 prefill 节点池 |

---

*本文档与 `boom-gateway/CLAUDE.md` 互为补充：CLAUDE.md 是"必须遵守"的硬约束清单，本文档是"为什么这样约束"的设计论证。*
