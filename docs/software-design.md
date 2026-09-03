# BooMGateway 全量软件设计文档

> **版本**：v1.0 · 2026-09
> **依据**：当前代码实态（BOOM_VERSION 1.0.4，18 crate workspace）逐模块勘察编写。
> **注意**：`docs/architecture-overview.md` 等旧文档中的 ZMQ KV 订阅、L2 PromptPrefix 独立策略、`LiteLLM_*` 表名、RateLimiter trait 均已被代码演进取代，本文以代码现状为准。
> **配套**：`CLAUDE.md`（硬约束）、`docs/internal-request-flow.md`（逐行请求流）、`USER_GUIDE.md`（使用手册）。

---

## 1. 概述

### 1.1 系统定位

**Intelligence Boom Gateway（BooMGateway）** 是一个高性能 LLM API 网关：

- Rust workspace 实现，单进程多模块，Docker 容器化部署；
- 协议层同时兼容 OpenAI `/v1/chat/completions` 与 Anthropic `/v1/messages`（Claude Code 可直连）；
- 密钥体系兼容 litellm（`boom_verification_token` / `boom_team_table`，schema 级兼容），已有 litellm 部署可平滑迁移；
- 自建全部上层能力：精细调度（含 KV-cache 前缀亲和）、三维限流配额计费、审计、trace、prompt log、Dashboard、热加载。

**设计目标**：

| 目标 | 含义 |
|------|------|
| 高性能 | Rust 转发路径、DashMap 无锁并发、零缓冲流式管道 |
| 精细调度 | 两阶段调度 + 可插拔策略族（RR / key 亲和 / KV-cache 亲和），面向 vLLM 前缀缓存优化 TTFT |
| 零停机运维 | SIGHUP / API / Dashboard 触发热加载，ArcSwap 原子替换，in-flight 请求不受影响 |
| 未服务不计费 | PlanCharge 三段式契约，配额只在真实服务后扣减 |
| 完整可观测 | 审计日志（DB）、trace（OTLP）、prompt log（OTLP）、实时统计、异常检测 |
| 平滑兼容 | litellm 密钥表、YAML 配置格式（`proxy_server_config.yaml` 风格）双兼容 |

**非目标**：不实现自有模型协议；不替换 litellm 密钥管理 UI（只读兼容 + Dashboard 自管增补）；不直接对接训练框架。

### 1.2 设计原则（硬约束）

1. **单向无环依赖**：boom-core 是唯一叶子（trait + 公共类型），boom-main 是唯一根（组装层），中间模块只依赖 boom-core。
2. **DB 表所有权**：每模块只 DDL/CRUD 自己的 `boom_` 前缀表，跨模块写必须走 AdminCommand channel 或公共 API。
3. **三种状态生命周期**：热替换（AppStateInner/ArcSwap）、跨 reload 存活（stores/计数器，AppState 顶层 Arc）、配置变更即清空重建（kv_index trie）。
4. **AdminCommand 解耦**：Dashboard 写操作经 mpsc channel 交 boom-main 执行，Dashboard 不依赖 boom-provider / boom-config。
5. **RAII Drop 收尾链**：流式请求的 duration/usage/计费/日志在 Drop 时回写，覆盖正常结束、错误、panic、客户端断连全部路径。
6. **未服务不计费**：`peek → commit → settle` 三段式，请求未到达 provider 不扣任何配额。
7. **配置字段单一真相源（manifest 原则）**：字段 5 处同步（结构体/manifest/SQL/前端/迁移），编译期测试强制。
8. **反馈式自愈**：负载信号驱动软负载均衡（慢后端自动旁路），健康监测兜底硬摘除。

### 1.3 术语表

| 术语 | 含义 |
|------|------|
| deployment | 一个可直接调用的上游模型实例（model_name 相同可有多份） |
| model_name | 逻辑模型名（一组 deployment 的分组键），区别于上游真实模型 id |
| `"*"` | **真实的兜底 model_name**，请求模型名完全未匹配时路由目标；不是"全权限"通配符。全权限判定唯一依据：`models` 数组为空或含 `all-team-models` |
| plan | 限流套餐（并发/RPM/TPM/自定义窗口/累计 token/费用/时段覆盖） |
| PlanCharge | 一次请求的配额占用状态机（peek→commit→settle） |
| KVC | KV-cache 前缀亲和调度（kvc_aware），网关侧自学习前缀 trie |
| fusion | 虚拟 Provider 编排（panel 多模型并行 + aggregator 聚合） |
| 三数据源 | YAML（声明式真相源）/ DB（运行期权威）/ 内存（路由唯一来源） |

---

## 2. 总体架构设计

### 2.1 架构风格

- **物理形态**：单一 Rust workspace，18 个 crate，编译为单二进制 `boom-main`（内嵌 Dashboard SPA 静态资源）。
- **分层**：核心抽象层（boom-core）→ 功能模块层（auth/config/provider/limiter/routing/...）→ 组装层（boom-main）→ 表现层（boom-dashboard，经 AdminCommand 与组装层通信）。
- **并发模型**：tokio 多线程 runtime（worker 数可配，默认 4）；热路径全部无锁结构（DashMap / ArcSwap / AtomicUsize）。
- **扩展模型**：trait 抽象（Provider / Authenticator / SchedulePolicy / KvIndexBackend / TraceApi / StressmonApi）+ 注册/工厂模式 + 动态库钩子（pre_auth .so）。

### 2.2 模块清单（18 crate + 1 外部钩子工程）

| Crate | 层 | 职责 | 依赖（boom-*） | DB 表 |
|-------|----|------|----------------|-------|
| **boom-core** | 核心抽象 | Provider/Authenticator/KeyAliasLookup/TraceApi/KvIndexBackend 等 trait；ChatCompletionRequest/AuthIdentity/Usage 等公共类型；Anthropic↔OpenAI 双向转码；GatewayError；OtlpConfig | （叶子，无） | 无 |
| **boom-config** | 功能模块 | YAML 解析、env 展开、secret 脱敏、原子写、manifest 字段注册表 | core | 无 |
| **boom-auth** | 功能模块 | 密钥认证（SHA-256 + moka 缓存 + 主密钥）、litellm 特殊模型名解析 | core | 读 boom_verification_token / boom_team_table |
| **boom-provider** | 功能模块 | Provider 工厂（OpenAI/Anthropic/Bedrock/Gemini/Azure...）、上游 HTTP 协议实现 | core | 无 |
| **boom-limiter** | 功能模块 | PlanStore、SlidingWindowLimiter、ConcurrencyGuard、PlanCharge、累计配额 | core | boom_rate_limit_state / boom_rate_limit_plan / boom_key_plan_assignment / boom_team_plan_assignment / boom_rate_limit_cumulative |
| **boom-routing** | 功能模块 | DeploymentStore、AliasStore、调度策略族、Rebalance/InFlight、计费费率 | core | boom_model_deployment / boom_model_alias |
| **boom-fusion** | 功能模块 | DirectSynthesis 工作流（panel 并行 + aggregator 聚合）虚拟 Provider | core | 无 |
| **boom-flowcontrol** | 功能模块 | per-deployment in-flight 队列、max_context、VIP 优先派发 | core | 无 |
| **boom-kvindex** | 功能模块 | 自学习 token 前缀 trie（block 切块 + xxhash3）、LRU/TTL 驱逐 | core | 无 |
| **boom-ctxaware** | 功能模块 | AgentStatsTracker 客户端类型统计（60min 环形） | core | 无 |
| **boom-audit** | 功能模块 | boom_request_log 读写、分页查询 | core | boom_request_log |
| **boom-promptlog** | 功能模块 | 完整 prompt/响应内容采集 + OTLP logs 导出 + 查询回放 | （独立，不依赖 core） | 无 |
| **boom-trace** | 功能模块 | 请求级 span 链路、recent ring、OTLP traces 导出 | core | 无 |
| **boom-stressmon** | 功能模块 | 系统压力时序（CPU/RSS/队列深度/inflight，1Hz 环形） | core | 无 |
| **boom-hooks-sdk** | 功能模块 | pre_auth 钩子 SDK（动态库 ABI） | core | 无 |
| **boom-dashboard** | 表现层 | Web UI + REST API + JWT 认证 + AdminCommand 定义 + 统一 DB 迁移入口 | core, limiter, routing, audit, flowcontrol, ctxaware, promptlog, trace, stressmon（**禁止 provider/config**） | boom_config / boom_team_table / boom_verification_token（DDL 所有权） |
| **boom-main** | 组装层 | axum 路由、AppState 组装、热加载、后台任务、admin_command_handler、请求主流程 | 全部 | （聚合层） |
| example-hooks | 示例 | 钩子编写示例 | core | 无 |
| `hook/`（workspace 外） | 外部工程 | pre-auth-demo，cdylib 产出 .so/.dylib 供网关运行时加载 | boom-hooks-sdk | 无 |

### 2.3 依赖规则

```
boom-core（叶子）← 所有 boom-* 模块
boom-main（根）  → 依赖全部模块
boom-dashboard  → 不依赖 boom-provider / boom-config（硬约束）
boom-promptlog  → 完全独立（纯 IO，可被第三方复用）
boom-trace      → leaf crate，Dashboard 经 boom-core::TraceApi（Arc<dyn TraceApi>）消费
```

编译期防环：新增 crate 检查清单见 CLAUDE.md §8（依赖仅 boom-core、表 `boom_` 前缀、DDL 在本 crate、`pub use` 导出、boom-main 引入）。

### 2.4 运行时架构

**进程内三大区**：

1. **HTTP 请求处理区**：axum router（LLM 代理端点 + admin 端点 + dashboard 端点 + 健康端点），extractor 形式的鉴权（RequiredAuth / CachedJson）。
2. **共享状态区（AppState）**：三种生命周期（见 §4.2）。
3. **后台任务区**（tokio::spawn，挂同一 broadcast shutdown 通道）：

| 任务 | 周期 | 职责 |
|------|------|------|
| SIGHUP listener | 事件驱动 | 触发 reload（单槽 Mutex 防重入） |
| admin_command_handler | channel 驱动 | 消费 AdminCommand（14 变体） |
| 限流计数器/分配快照落库 | 600s | sync_counters_to_db / sync_assignments / cleanup_expired |
| 请求量汇总日志 | 60s | request_count 分钟级摘要 |
| FlowController 周期派发 | 1s | 防空闲容量滞留 |
| KV trie TTL prune | ttl/2（≥5s） | 扫过期 trie 块 |
| 部署离线巡检 | 可配（默认 30s） | 连续失败 N 次 → auto_disable |
| 部署恢复探测 | 可配（默认 60s） | 仅探测 auto_disabled 行，连续成功 N 次 → auto_enable |
| 压力采样 | 1Hz | CPU/RSS/队列深度/inflight → stressmon 环形缓冲 |
| 审计日志写盘 | 常驻 + 100ms/2000 行 | 批量 INSERT boom_request_log |
| prompt-log / trace OTLP flush | 按批量/间隔配置 | 导出器任务 |

**外部交互 5 类**：HTTPS→上游 LLM；SQL→PostgreSQL/GaussDB；HTTP→OTLP Collector（logs + traces）；I/O→YAML/备份文件；Signal→SIGHUP/SIGTERM/CtrlC。

### 2.5 部署架构

- **单机**：单进程 + 外部 PG。裸 TCP 监听（默认 0.0.0.0:4000），TLS 由前置反代承担。
- **LB 多实例**：`misc/LB` Pingora 代理在前，多 boom-main 共享 PG。限制：限流窗口/累计经 DB 共享（粗粒度可用）；inflight / kv_index / 并发 guard 为实例本地状态（细粒度有误差）；配置变更经 DB + 各实例 reload 传播。
- **多实例演进路线**：L1 主备 → L2 请求路径解耦 → L3 全量解耦（改 limiter / kvindex / admin_command 时须评估多实例影响）。

---

## 3. 功能点 → 子功能模块划分

本章为全量功能分解：11 个功能域（F1–F11），每个功能域按功能点拆子功能模块，标注承载 crate 与关键实现。

### F1 协议接入与转码

> 用户感知："OpenAI SDK / Anthropic SDK / Claude Code 改个 base_url 就能用。"

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F1.1 | OpenAI Chat 接入 | `POST /v1/chat/completions`（流式 + 非流式），未知请求字段经 `extra` flatten 原样透传（开放字段集） | boom-main / boom-core | routes.rs:515；ChatCompletionRequest（types.rs:127） |
| F1.2 | OpenAI Legacy 接入 | `POST /v1/completions` 转 Chat 格式复用同一处理链 | boom-main | routes.rs:1148，into_chat_request |
| F1.3 | Anthropic Messages 接入 | `POST /v1/messages` 全套（含 thinking/redacted_thinking/document 内容块，未知块前向兼容）；请求转 OpenAI 走内部管线，响应/流式转回 Anthropic 事件 | boom-core / boom-main | anthropic.rs 双向转码 + AnthropicStreamTranscoder（SSE 事件重建） |
| F1.4 | 无前缀兼容路径 | `/chat/completions`、`/completions`、`/models` 不带 /v1 同样可用 | boom-main | main.rs:169-182 |
| F1.5 | 消息规范化 | Anthropic 严格 user/assistant 角色交替修补；tool_choice / parallel_tool_calls 语义转换；GLM/zhipu `reasoning` 字段别名兼容 | boom-core | normalize.rs |
| F1.6 | Claude Code 适配 | `strip_claude_code_attribution`：剥离 Claude Code 注入的动态 system 块，恢复 KV 前缀命中 | boom-config / boom-main | router_settings 配置项 |
| F1.7 | 模型发现 API | `GET /v1/models`、`/v1/models/{id}`：按密钥白名单 + public_models 过滤可见模型，排除 `"*"` 与 hidden 别名 | boom-main / boom-routing | routes.rs:1163 |
| F1.8 | 显式不支持端点 | `/v1/embeddings`、`/v1/audio/*`、`/v1/moderations` 返回 OpenAI 风格 NotSupported 错误 | boom-main | 宏生成 routes.rs:1257 |
| F1.9 | 请求体缓冲 | LLM 路由挂 `buffer_request_body`（上限 2MB），缓存字节供鉴权探测 model 字段与 JSON 复解析 | boom-main | extractor.rs:230 |

### F2 认证与密钥管理

> 用户感知："sk-xxx 与 litellm 完全兼容；管理员用 master key。"

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F2.1 | 主密钥认证 | master_key 常数时间比较（防时序攻击），命中返回全权 identity（key_hash="master"）；无 DB 时仅此可用 | boom-auth | key_auth.rs:48-63 |
| F2.2 | 密钥哈希认证 | 整串 raw key SHA-256（与 litellm `hash_token` 逐字节兼容，嵌入 `-` 的 key 前缀参与哈希实现防篡改） | boom-auth | key_auth.rs:176-281 |
| F2.3 | 认证缓存 | moka 缓存（容量 10000 / TTI 5min），auth 查询亚微秒 | boom-auth | key_auth.rs:29-38 |
| F2.4 | 密钥状态校验 | blocked / expires_at / max_budget·spend（含 budget_duration 周期预算 budget_reset_at）三重校验 | boom-auth | AuthIdentity::is_expired / is_budget_exceeded |
| F2.5 | litellm 特殊模型名 | `all-team-models` → 展开 team.models；`all-proxy-models` → 清空全放行；优先级 key.models > team_models > 全放行 | boom-auth | resolve_team_models |
| F2.6 | 密钥生命周期管理 | Dashboard 密钥 CRUD、批量创建、litellm 格式导入、block/unblock、key_alias 附加信息维护 | boom-dashboard | /admin/keys 系列端点 |
| F2.7 | 团队管理 | team CRUD（team_id / team_alias / models 白名单） | boom-dashboard | boom_team_table |
| F2.8 | pre_auth 钩子 | 启动加载 .so 动态库（boom-hooks-sdk ABI），认证前可改写 key/model（Continue/Replace/ReplaceModel/Reject/Deny）；失败模式可配（allow 回退原生认证 / deny 500） | boom-main / boom-hooks-sdk | hooks 配置节 + RequiredAuth |
| F2.9 | key 前缀校验 | 前缀 1-50 位 ASCII 字母数字合法性（前缀参与哈希，篡改即失配） | boom-core | key_format.rs |

### F3 授权与模型可见性

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F3.1 | 模型访问控制 | key.models 白名单 → 别名展开 → `"*"` 兜底通配校验；public_models 对全部 key 开放 | boom-main / boom-routing | check_model_access（routes.rs:1590） |
| F3.2 | 别名管理 | alias → target_model 精确单映射（无链式/通配）；hidden 别名不在 /v1/models 展示；YAML + Dashboard CRUD 双入口 | boom-routing | AliasStore + boom_model_alias |
| F3.3 | `"*"` 兜底路由 | `serve_not_match=true` 的 deployment 同时注册到 `"*"`；仅对**完全未知**模型名生效（已配置但全部下线的模型不走兜底，显式报错） | boom-routing / boom-main | router.rs:173-195 |
| F3.4 | auto_router 内容分类 | 虚拟模型按请求内容动态选 tier：`tier_classifier`（关键词 + 长度/代码块/推理/工具调用启发式评分 → small/medium/large）或 `ml_service`（外部 POST /classify，100ms 超时，任何失败回落本地分类） | boom-routing | AutoRouterConfig + TierClassifier |

### F4 模型部署与上游管理

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F4.1 | DeploymentStore | model_name → Vec<Arc<dyn Provider>> 分组；quota_count_ratio 配额倍率；exclusive（fusion 独占）模型保护；内存直改 + DB 持久双轨 | boom-routing | deployment_store.rs（DEPLOYMENT_CORE_COLUMNS 24 列单一真相源） |
| F4.2 | Provider 工厂 | `provider/model-id` 前缀自动探测（gpt-/claude-/gemini-/anthropic./amazon. 等）+ 显式指定；OpenAI 兼容 / Anthropic Native 两类协议；Bedrock AWS 凭证；自定义 per-request headers（值支持 env 展开）；timeout/temperature/max_tokens 覆盖 | boom-provider / boom-config | auto_detect_provider（lib.rs:440） |
| F4.3 | deployment CRUD | 创建/更新/删除（Dashboard 经 AdminCommand）；update 用 COALESCE 保护凭据空值不覆盖；deployment_id 空自动生成 UUID；snapshot 必须含 disabled 行（防 YAML round-trip 变物理删除） | boom-routing / boom-main | create_db / update_db / delete_db |
| F4.4 | 启用/禁用 | `enabled=false` 入 DB 可见但不进路由表；`auto_disabled` 自动摘除标记（与 F4.7 联动） | boom-routing | enabled 字段语义 |
| F4.5 | 健康管理（三通道） | ① 主动巡检：探针 3s 超时，2xx/4xx 存活、5xx 故障，连续 failure_threshold 次 → auto_disable（DB 落库）；② 恢复探测：仅探测 auto_disabled 行，连续 recovery_threshold 次 → auto_enable；③ 请求级熔断：连续请求失败达阈值 → 下线，成功即清零 | boom-main | health_monitor.rs 三通道 |
| F4.6 | Fusion 工作流 | `DirectSynthesis`：panel（≥2 实例，可各带 temperature）并行 + aggregator 聚合；panel_timeout_secs 超时即失败；一轮全无效自动二轮重试；仅 1 个有效降级直返；aggregator 失败 fallback 首个有效 panel 答案；子调用完整走分类/调度/流控/计费/trace 管线；workflow 模型独占命名空间（与现有 deployment/alias 冲突即启动失败）；支持流式 | boom-fusion / boom-main | workflow.rs + fusion.rs RoutingModelInvoker |
| F4.7 | 计费费率 | per-deployment ModelCostRate（input/cached_input/output，USD/百万 token，Decimal 精度）；cost_templates 按名引用；cached_tokens 钳制不超 input；cached 单价 0 时按 input 价（legacy 行为） | boom-routing | compute_cost_breakdown |
| F4.8 | 客户端类型标记 | `client_type_header`：向上游附加 X-BooM-Client-Type 头 | boom-core / boom-provider | Provider::client_type_header |

### F5 智能调度（核心品牌能力）

> 调度位于限流**之后**：限流决定"能不能服务"（429 直接拒绝），调度决定"服务到哪个后端"。

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F5.1 | Phase A 模型层解析 | requested model → 最终 model_name：auto_router 内容分类 → 别名解析 → 精确匹配 → `"*"` 兜底 | boom-routing | Router::resolve 链 |
| F5.2 | Phase B 后端选择策略 trait | `SchedulePolicy::select / select_with_context`，Selection 携带 kv_hit_ratio / degraded 等 DFX 字段；策略 ArcSwap 热切换 | boom-routing | policy/mod.rs |
| F5.3 | L0 round_robin | per-model 原子计数取模；单候选直返；顶分并列时也用它 tie-break | boom-routing | round_robin.rs |
| F5.4 | L1 key_affinity | `{key_hash}:{model} → deployment_id` 粘滞；预热期（总量 < context_threshold）走最低负载；粘滞失效校验；负载差超 rebalance_threshold（默认 20，≥100 禁用）自动迁移并记 RebalanceMoveTracker | boom-routing | key_affinity.rs |
| F5.5 | L3 kvc_aware | 统一评分 `score = cache_weight×hit_ratio + load_weight×(1−load_pct/100)`（默认 0.7/0.3）；过载硬排除（load_pct ≥ overload_threshold_pct=90，100=禁用）；全部过载降级最低负载不丢请求；容量再平衡（winner 超最低非过载候选 rebalance_threshold 即移交）；顶分并列 RR 轮转 | boom-routing | kvc_aware.rs |
| F5.6 | KV 前缀索引（自学习 trie） | per-model trie：请求前缀（tools 先于 messages 固定序）按 block_size 字节切块 → xxhash3-64 → 块链；per-worker 命中深度 → hit_ratio=depth/total；**不订阅上游事件，路由后异步记录实际选中 worker（自学习，add-only）**；over-approximation 由网关侧 LRU（max_blocks 默认 500k，O(1) 反查表驱逐）+ TTL（后台扫描）老化；每节点独立锁 hand-over-hand 读写 | boom-kvindex / boom-main | token_prefix.rs + KvcOrchestrator（kvc.rs 五步路由） |
| F5.7 | KvcOrchestrator 路由编排 | policy≠kvc_aware 或单候选跳过 → 前缀序列化 → trie 查询选 provider → degraded 标记 → 信号量（32）限流 + tokio::spawn 异步 best-effort 学习记录 | boom-main | kvc.rs:47-208 |
| F5.8 | 负载信号总线 | InFlightTracker（model 与 model\0deployment 双维原子计数 + RAII guard）；load_pct = max(inflight, FC queued)/capacity 归一化（防双计）；FlowController 队列深度；RequestRateTracker（per-deployment 成功速率） | boom-routing / boom-flowcontrol | load_helpers.rs |
| F5.9 | 再平衡可观测 | RebalanceMoveTracker：60min 环形桶统计迁移次数，Dashboard 展示 | boom-routing | rebalance.rs |
| F5.10 | KV 全量上报请求 | kv_match_attempted 且命中率低于阈值时，向上游请求全量 cache report | boom-routing / boom-core | policy/mod.rs:22-32 |
| F5.11 | kv_worker_id 派生 | 从上游 api_base host 派生 worker 标识，用于 trie 命中归属 | boom-core | Provider::kv_worker_id |

### F6 配额限流与计费

> 用户感知："我的 key 是 basic plan：4 并发 / 60 RPM；团队有总额度；没服务成功不扣量。"

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F6.1 | Plan 定义 | RateLimitPlan：type（Key/Team 防误配）、member_plan（team 型对成员 key 施加的 plan）、concurrency_limit、rpm/tpm（60s 快捷方式）、window_limits（counts/tokens/costs/window_secs 四维自定义窗口，支持 `[c,t,cost,secs]` 紧凑格式）、total_token_limit、total_cost_limit、schedule 时段覆盖 | boom-limiter | concurrency.rs RateLimitPlan |
| F6.2 | 双层强制 | Team 外箱 + Key 内箱同时检查；3 级回退：key 显式分配 → team 分配 → default_plan / default_team_plan；无任何 plan 且 rate_limit.enabled=false 时该 key 不限流 | boom-limiter / boom-main | check_plan_limits（routes.rs:2320） |
| F6.3 | 三态分配 | key_assignments：None（未配置，跟 default）/ Some(None)（显式无 plan）/ Some(Some(name))；assign_key 拒绝误配 team 型 plan | boom-limiter | PlanStore |
| F6.4 | 三维滑窗 | cache_key `{key_hash}:{model}:{window_secs}`，counts/tokens/costs（内部 micros）三维；窗口过期即重置；counts 维度权重感知（quota_count_ratio：贵模型 1 次 = N 配额）；tokens/costs 历史累计检查（压线放行、下一个拒绝） | boom-limiter | SlidingWindowLimiter |
| F6.5 | 累计配额 | 6 维终身累计（TotalInput/OutputTokens、TotalCost、RegularInput/CachedInput/OutputCost）；**无按日/月自动重置**，仅手动 reset（key 表 budget_duration 走 litellm 预算路径，另一体系） | boom-limiter | CumulativeKind |
| F6.6 | 并发控制 | ConcurrencyGuard（AtomicU32 + RAII Drop）；key 与 team 双维并发槽；GuardedStream 流结束/断连时释放 | boom-limiter | try_acquire / try_acquire_team |
| F6.7 | PlanCharge 契约 | peek_only（只读检查，不计数）→ commit_counts（provider 接受后仅计 counts）→ settle_usage（真实 usage 到账：tokens/costs + 全部累计）；未到 provider 即 Drop → 不计费；流中断无 usage → counts 留存自然过期、tokens/costs 不计 | boom-limiter / boom-main | PlanCharge 状态机 |
| F6.8 | 时段计划 | ScheduleSlot（UTC+8，支持 "9:00-21:00" 与跨午夜 "21:00-9:00"）；生效时段字段覆盖基础限额；切档时清 stale 窗口计数；服务端校验槽位重叠 | boom-limiter | effective_limits / is_active_now |
| F6.9 | 持久化与恢复 | 计数器 600s 周期落库（boom_rate_limit_state / cumulative）；启动 restore 回填未过期窗口 + 全部累计；多实例经 DB 粗粒度共享 | boom-limiter | sync_counters_to_db / restore_counters_from_db |
| F6.10 | 配额运维 | 重置族 API：key/team × cumulative/windows/all；team 重置级联成员 key；重置时从 team rollup 扣减；recompute_team_cumulative SUM 重建；Dashboard 配额总览（overview/team/unassigned/key windows） | boom-limiter / boom-dashboard | clear_*_db 系列 + /admin/quota/* |
| F6.11 | 用量查询 | per-key usage、全 key usage、peek 窗口明细；用户端自查用量 | boom-limiter / boom-dashboard | get_usage 系列 + /user/usage |

### F7 部署级流控

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F7.1 | in-flight 队列 | per-deployment `max_inflight_queue_len` 排队（默认 queue timeout 1200s，FlowControlError::Timeout）；NoSlot 容忍语义（fusion 子调用） | boom-flowcontrol | acquire_fc_guard |
| F7.2 | 上下文总量限制 | per-deployment `max_context_len`：同时在飞 input 总量上限，超限立即拒（ContextExceeded） | boom-flowcontrol | model_context_limit / max_context_len |
| F7.3 | VIP 优先 | key metadata.vip=true 进 VIP 队列优先派发；slot 释放贪婪填满；1s 周期派发防空闲滞留；客户端断连 AcquireCleanup 防计数泄漏 | boom-flowcontrol | SlotInner::dispatch |
| F7.4 | 优先级透传 | `enable_priority_header`：网关注入 X-Gateway-Priority 上游头 | boom-config / boom-main | router_settings |

### F8 可观测性

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F8.1 | 请求审计日志 | boom_request_log（25 列：request_id、key_hash/alias、team、model/model_name、deployment_id、is_stream、status_code、error_type、input/output/cached_tokens、duration_ms、ttft_ms、queue_wait_ms、schedule_policy、kv_hit/input_blocks、client_ip 等）；LoggedStream 在 **Drop 时**写真实 duration/ttft | boom-audit / boom-main | request_log.rs |
| F8.2 | 异步批量写入 | 10 万容量 channel 吸收 DB 抖动；100ms 或 2000 行批量 INSERT（专用 8 连接池）；失败重试 1 次后丢批计数；拒绝去重（同 error_type+key+model 60s 只记首条，401 不去重作为安全信号） | boom-main | LogWriter |
| F8.3 | 日志查询 | 分页 + key/model/status 过滤；admin 全量 / user 本人 | boom-audit / boom-dashboard | list_logs + /admin/logs、/user/logs |
| F8.4 | Prompt Log | 完整 req/resp body 采集（含流式 chunks、raw upstream、thinking）；按 key/team 白名单开关；OTLP logs 导出（batch 512 / flush 5s / max_queue 10000 / attr 4KB 截断）；request_id 查询回放；trace span 与其共享 body 内存（零拷贝） | boom-promptlog / boom-main | PromptLogWriter + 双阶段 entry |
| F8.5 | Trace 链路 | 每请求一个 gateway span（trace_id 复用入站 W3C traceparent，span_id SHA-256 生成，status/attrs）；active 表（cap 100K，满驱最老）+ recent ring（100）；TraceGuard RAII finalize；OTLP traces 导出（Online/Offline 状态机 + probe + 热替换）；`propagate_only=false` 时仍透传 traceparent；report_filter（tracestate keys + trace_id regex 采样） | boom-trace / boom-main | TraceRegistry + otlp_export |
| F8.6 | TraceApi 消费 | Dashboard 经 `Arc<dyn TraceApi>`（boom-core 定义）读 snapshot / otlp_status / probe，不依赖 boom-trace | boom-core / boom-dashboard | trace.rs:108 |
| F8.7 | 实时统计 | in-flight 面板（per model/deployment 并发 + VIP/普通队列 + 排队 waiter 列表）、24h deployment 汇总、rebalance 迁移统计、request_rate、agent 占比（anthropic vs other，60min 环）、audit drop 计数 | boom-dashboard / boom-routing / boom-ctxaware | /admin/stats/* |
| F8.8 | 系统压力监控 | 1Hz 采样 CPU/RSS/队列深度/inflight → 环形时序，Dashboard /admin/stress/timeseries 拉取 | boom-stressmon / boom-main | StressmonCollector |
| F8.9 | 异常检测 | IQR box（P25/P75 ± 1.5×IQR）按维度（key/model/deployment/team 白名单防 SQL 标识符注入）分组检测 duration/token 离群，severity 排序，IQR=0 不误报；range=1d/3d/7d | boom-dashboard | /admin/debug/anomalies |
| F8.10 | Debug 错误现场 | admin API 开关的内存错误捕获（全请求/上游 body，每 key 限 3 条 FIFO），按 request_id 查询 | boom-core / boom-main / boom-dashboard | DebugErrorStore + /debug/* |
| F8.11 | 运行日志 | tracing JSON 格式 non-blocking stdout；60s 请求量分钟汇总 | boom-main | main.rs:642 |

### F9 管理面板（Dashboard）

> 用户感知："浏览器打开 /dashboard，key 登录看自己，master key 登录管全部。"

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F9.1 | 面板认证 | JWT HS256（secret 由 master_key 派生），boom_session HttpOnly cookie 2h；admin 登录 = master key 常量时间比较；user 登录 = SHA-256(api_key) 查表 + blocked 校验；角色仅 admin/user 两级；per-IP 防爆破（5 次锁 10s，之后每次 +30s） | boom-dashboard | auth.rs |
| F9.2 | 用户端 | plan 解析（三态）+ 有效限制展示、个人用量、key 信息、个人日志、实时排队状态（自己前面还有几位） | boom-dashboard | /user/* 端点 |
| F9.3 | 管理端 — 模型配置 | 模型 CRUD、别名 CRUD、配置查看/点路径编辑、config schema 拉取、热加载按钮 | boom-dashboard | /admin/models、/admin/aliases、/admin/config* |
| F9.4 | 管理端 — 密钥与团队 | 密钥 CRUD/批量/导入/block、团队 CRUD、plan 分配（key/team） | boom-dashboard | /admin/keys、/admin/teams、/admin/assignments |
| F9.5 | 管理端 — 配额运维 | plan CRUD、配额总览四视图、7 个重置端点、用量查询 | boom-dashboard | /admin/plans、/admin/quota/* |
| F9.6 | 管理端 — 可观测 | 统计图表（时间窗 1h/4h/8h/24h/custom，1h 走内存 tracker 其余 SQL 聚合，百分位 percentile_cont）、日志、异常检测、压力时序、trace snapshot、prompt-log 查询与 OTLP 探活 | boom-dashboard | /admin/stats、/admin/logs、/trace/*、/prompt-log/* |
| F9.7 | 写操作解耦 | 模型/配置/promptlog 写经 AdminCommand（14 变体）mpsc → boom-main 执行；ConfigChanged 带 reply channel，YAML 写失败 surface 为响应 `warning` 字段（主操作已成功不转 error） | boom-dashboard / boom-main | AdminCommand enum + admin_command_handler |
| F9.8 | 前端 SPA | 内嵌静态资源；用户页三视图（overview/chat 调试/logs）+ 管理页九区（stats/debug/models/plans/keys/config/stress/quota/logs，hash 路由）；en/zh 双语 i18n（data-i18n + localStorage）；统一写操作确认弹窗 | boom-dashboard | frontend/（index.html / app.js / i18n.js） |
| F9.9 | DB 池隔离 | Dashboard 专用 max=3 连接池，与转发主池（max=30）隔离，重型聚合查询不饿死转发 | boom-dashboard / boom-main | DashboardState.db_pool |

### F10 配置管理

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F10.1 | YAML schema | 顶层：model_list / general_settings / router_settings / workflow_settings / server / rate_limit / plan_settings / cost_templates / deployment_health_check / prompt_log / trace / hooks（litellm proxy_server_config 格式兼容） | boom-config | Config（lib.rs:31-65） |
| F10.2 | 环境变量展开 | 双语法 `${VAR}` / `os.environ/VAR`；解析前全文展开 + 解析后敏感字段二次展开；`read_raw_yaml` **不展开**（Web 读-改-写保留 secret 引用不泄露磁盘） | boom-config | resolve_env_vars |
| F10.3 | Secret 保护 | 13 个敏感字段名脱敏 `****`（null 保留区分未配置）；`write_yaml_atomic` truncate 原地写 + fsync（兼容容器 bind mount） | boom-config | mask_secrets_in_place |
| F10.4 | 三数据源模型 | YAML=声明式真相源 + 启动种子（reload 绝对胜出）；DB=运行期权威 + 多实例共享；内存=路由唯一来源 + 最大服务兜底（YAML/DB 均失败仍按内存服务） | 全局 | CLAUDE.md §7 |
| F10.5 | 热加载 | 触发：SIGHUP / POST /admin/config/reload / Dashboard 按钮 / UpdateConfigSection 后自动；60s 总超时 + catch_unwind（失败旧 inner 继续路由）；流程：重读 YAML → 重连 DB（如 URL 变）→ 重建三 store → kv_index 签名比对（不变保留学习成果，变了重建空 trie）→ 重建调度策略 → sync_yaml_to_db（15s 超时）→ 叠加 DB-only 行 → 注册 fusion → 清孤儿分配 → promptlog/OTLP 热更 → ArcSwap 原子换入；**不清除任何运行时计数器** | boom-main | reload_inner（state.rs:446-642） |
| F10.6 | CRUD 写路径（五步规范） | ① DB 写 → ② 直改内存（立即可路由，不依赖 YAML/reload）→ ③ best-effort 写 YAML（失败仅 warn）→ ④ 响应带 warning 字段提示运维修复 → ⑤ handler 不调 reload()（reload 是 YAML→内存单向覆盖，会中断路由） | boom-main / boom-dashboard | §5.4 详述 |
| F10.7 | manifest 单一真相源 | FieldMeta（field/section/input_type/label_key/tip_key）登记 deployment/general/router 全部字段；`GET /admin/config/schema` 透出前端；编译期强制测试：结构体字段未注册 / SQL 漏列 / manifest 重复即编译失败 | boom-config / boom-routing | manifest.rs + deployment_store 测试 |
| F10.8 | 配置快照回写 | build_config_snapshot_value：从 DB 读全量组装 JSON（含 normalize 旧 JSONB 形状），persist_config_in_place 先 .bak 备份只改运行时段落，保留 `${VAR}` 原文 | boom-main | state.rs:1655-1905 |
| F10.9 | 配置校验 | 分节 validate（KvcAwareSettings 语义校验：权重 ∈[0,1] 且和 ≤1、overload 1..=100、ttl 防 Duration panic；workflow 引用闭环；rebalance_threshold 1..=100；schedule 重叠检测） | boom-config / boom-limiter | validate() 族 |

### F11 运行与运维

| 编号 | 子功能模块 | 功能描述 | 承载 | 关键实现 |
|------|-----------|----------|------|----------|
| F11.1 | 启动 | clap：--config（默认 config.yaml）/ --host / --port / --reboot；config.server.workers 决定 tokio 线程数；DB 未配置进入 master-key-only 模式 | boom-main | main.rs:40-151 |
| F11.2 | 健康端点 | /health（版本/uptime/db 状态/模型数/reload 次数）、/health/live、/health/ready（DB 配置未连上 503） | boom-main | routes.rs:1277 |
| F11.3 | 优雅停机 | CtrlC/SIGTERM → broadcast 通知全部后台任务 → 关 DB 池 → prompt-log OTLP best-effort flush | boom-main | main.rs:120-148 |
| F11.4 | --reboot 自替换 | pgrep 旧进程 SIGTERM，用 /health 探测区分"优雅退出中"与"冻结"，冻结则 SIGKILL | boom-main | main.rs:692-778 |
| F11.5 | DB 迁移 | 启动时统一执行（boom_dashboard::migrations::run_migrations，幂等 CREATE/ALTER IF NOT EXISTS，单连接 + lock_timeout 10s；GaussDB 兼容：无 ON CONFLICT 走 UPDATE→INSERT 宏、REPLICATION 分布） | boom-dashboard | migrations.rs |
| F11.6 | Admin API（master key） | /admin/config/reload、/admin/plans CRUD + assign + assignments 查询 | boom-main | routes.rs:1322+ |
| F11.7 | 内部调试端点 | GET /internal/kv-index：dump trie 块数与生效 kvc 配置 | boom-main | routes.rs:3724 |
| F11.8 | 安全头过滤 | forward_client_headers 白名单透传客户端头；硬封禁 13 类（authorization/cookie/x-gateway-*/x-boom-* 等）防伪造网关内部头 | boom-config / boom-main | lib.rs:610-630 |

---

## 4. 数据设计

### 4.1 数据库表总表（11 张）

| 表 | 所有者（DDL） | 用途 | 关键列 |
|----|--------------|------|--------|
| boom_verification_token | boom-dashboard（迁移入口） | API 密钥（litellm schema 兼容） | token(SHA-256 主键)、key_name/alias/prefix、spend/max_budget/budget_duration、expires、models、team_id、rpm/tpm_limit、blocked、metadata |
| boom_team_table | boom-dashboard | 团队 | team_id、team_alias、models |
| boom_model_deployment | boom-routing | 模型部署 | DEPLOYMENT_CORE_COLUMNS 24 用户列 + id/source(yaml\|db)/auto_disabled/created_at/updated_at |
| boom_model_alias | boom-routing | 模型别名 | alias_name(PK)、target_model、hidden、source |
| boom_rate_limit_plan | boom-limiter | 套餐定义 | name、type、member_plan、concurrency/rpm/tpm、window_limits、total_*、schedule、is_default（唯一索引） |
| boom_key_plan_assignment | boom-limiter | key→plan（三态：NULL=显式无） | key_hash、plan_name |
| boom_team_plan_assignment | boom-limiter | team→plan | team_id、plan_name |
| boom_rate_limit_state | boom-limiter | 滑窗计数器持久化 | cache_key、counts/tokens/costs_micros、window_start/secs |
| boom_rate_limit_cumulative | boom-limiter | 累计配额（6 维） | scope/scope_id/kind/value_micros |
| boom_request_log | boom-audit | 请求审计（25 列） | request_id、key/team、model、deployment_id、status、tokens 三列、duration/ttft/queue_wait、kv 指标、client_ip |
| boom_config | boom-dashboard | 通用 KV 配置（JSONB） | key、value |

### 4.2 内存状态（三种生命周期）

```
AppState (Clone, 进程生命)
 ├─ inner: Arc<ArcSwap<AppStateInner>>     ① 热替换：config + auth + hooks + health
 ├─ db_pool(30) / dashboard_db_pool(3) / log_writer 池(8)   ② 跨 reload 存活
 ├─ deployment_store / alias_store / plan_store / limiter
 ├─ router / inflight / flow_controller / request_count
 ├─ debug_store / prompt_log_writer / rebalance_move_tracker
 ├─ request_rate / agent_stats / stressmon / trace
 └─ kv_index: Arc<ArcSwap<Option<…>>>      ③ 签名（policy/block_size…）变 → 重建空 trie
```

### 4.3 关键配置结构

- **ProviderParams**：model（provider/model-id）、api_key/base、aws 凭证、rpm/tpm、timeout（默认 1200s）、headers、temperature/max_tokens。
- **router_settings**：schedule_policy、model_group_alias（Simple/Extended{hidden}）、key_affinity_context_threshold、rebalance_threshold、auto_router、kvc_aware{block_size/cache_weight/load_weight/max_blocks/overload_threshold_pct/router_ttl_secs}、enable_priority_header、flow_control_queue_timeout_secs、strip_claude_code_attribution、forward_client_headers。
- **plan_settings**：default_plan / default_team_plan / plans[]（含 schedule 时段）。
- **deployment_health_check**：path、failure/recovery_threshold、检查间隔。
- **trace / prompt_log**：OTLP 端点与采样/截断参数（共享 OtlpConfig）。

---

## 5. 关键流程设计

### 5.1 请求主流程（以 /v1/chat/completions 为例）

```
客户端 → axum（CORS + request_count 计数）
 ① buffer_request_body（≤2MB 缓存）→ RequiredAuth：pre_auth 钩子 → authenticate
    （master key 常量比较 / SHA-256 → moka → DB → team 解析 → 特殊模型名 → blocked/expired/budget）
 ② prompt-log 采集判定 + traceparent 解析 + trace 过滤 + TraceGuard::start
 ③ check_model_access（白名单/public/别名/"*"）→ auto_router 内容分类 → 最终 model_name
 ④ check_plan_limits：team 层 peek → key 层 peek（并发 guard + 三维窗口 + 累计）→ PlanCharge
    （不通过 → 429，不进调度）
 ⑤ 调度：KvcOrchestrator.route（前缀序列化 → trie 查询 → 评分选择 → 异步学习记录）
    失败/未启用 → router.select_provider_with_prefix 兜底
 ⑥ acquire_fc_guard：deployment 级流控排队（VIP 优先 / Timeout / ContextExceeded）
 ⑦ 组装上游头（客户端白名单过滤 + 网关注入头 + 子 traceparent）→ provider.chat[_stream]_with_context
 ⑧ 流式包装链（内→外）：UsageTracker → InFlightStream → FlowControlledStream
    → GuardedStream → LoggedStream → [PromptLogStream] → [AnthropicStreamTranscoder] → SSE
 ⑨ Drop 时收尾（见 §5.2）；错误路径：GatewayErrorReply（JSON 或 SSE 形式）+ record_request_failure
```

### 5.2 RAII Drop 收尾链

```
流结束/断连/panic → 外层先 Drop：
  LoggedStream   → 写审计日志（真实 duration/ttft/usage）+ PlanCharge.settle + trace finalize
  GuardedStream  → 释放 key/team 并发槽
  FlowControlledStream → 释放 FC slot + dispatch 唤醒下一个 waiter
  InFlightStream → inflight 计数递减
  PromptLogStream → 异步写 prompt log entry
```

选 Drop 链而非显式 finalize 的原因：handler 可能被 cancel、panic、多早返回点遗漏；Drop 由编译器保证全路径执行，且新增计费维度只需加一层 wrapper。

### 5.3 热加载流程

见 F10.5。安全保证：60s timeout + catch_unwind（SIGHUP 永不 hang 死进程）；早期 Err 不 swap 旧 inner 继续路由；慢 DB 步骤 15s 超时包装。已知权衡（场景 D）：YAML 只读时 CRUD 写 DB 成功但 reload 会以旧 YAML 覆盖删除 source='db' 新行——显式选择"YAML 绝对胜出"，经响应 warning 字段提示运维修复。

### 5.4 CRUD 写路径（五步规范）

```
DB 写（store.*_db）
 → 内存直改（reload_model_deployments / store 内存方法）★立即可路由，不依赖 YAML
 → best-effort 写 YAML（persist_yaml_with_reply，失败仅 warn）
 → 响应（YAML 失败 → warning 字段，不转 error）
 （handler 不调 reload()——reload 是 YAML→内存单向覆盖，会清 store 中断路由）
```

### 5.5 健康自愈闭环

```
软（秒级）：负载信号（inflight + FC queue）→ 策略评分/再平衡 → 慢后端流量旁路 → 恢复后回流
硬（分钟级）：主动巡检连续失败 N 次 → auto_disable（DB+内存）→ 恢复探测连续成功 M 次 → auto_enable
请求级：连续请求失败达阈值 → 立即下线；成功即清零
```

---

## 6. 接口设计（外部 API 汇总）

### 6.1 LLM 代理端点（API key 鉴权）

| 端点 | 方法 | 说明 |
|------|------|------|
| /v1/chat/completions（+/chat/completions） | POST | OpenAI Chat，流+非流 |
| /v1/completions（+/completions） | POST | Legacy，内部转 Chat |
| /v1/messages | POST | Anthropic Messages，双向转码 |
| /v1/models、/v1/models/{id}（+无前缀） | GET | 按权限过滤的模型发现 |
| /v1/embeddings、/v1/audio/*、/v1/moderations | POST | 显式 NotSupported |

### 6.2 Admin 端点（master key）

/admin/config/reload；/admin/plans（GET/PUT/DELETE）、/admin/plans/assign（POST/DELETE）、/admin/plans/assignments。

### 6.3 Dashboard 端点（JWT，/dashboard/api 前缀，约 60 个）

- **auth**：login / logout / me
- **user**：plan / usage / key-info / logs / request-status
- **models & aliases**：/admin/models、/admin/aliases CRUD
- **keys & teams**：/admin/keys CRUD + batch + import + block/unblock；/admin/teams CRUD
- **plans & assignments**：/admin/plans、/admin/assignments、/admin/team-assignments
- **quota**：/admin/quota/overview|team/{id}|unassigned|key/{hash}/windows + 7 个 reset 端点 + /admin/limits/reset
- **observability**：/admin/logs、/admin/stats/{inflight,deployments/summary,rebalance-moves,audit-log,request_rate,agents}、/admin/stress/timeseries、/admin/debug/anomalies
- **prompt-log**：status / toggle / team / key / entry/{request_id} / otlp-{ping,status,probe}
- **trace**：snapshot / otlp-status / otlp-probe / otlp-ping
- **config**：/admin/config（GET/PUT）、/admin/config/schema、/admin/config/reload
- **debug**：status / toggle / errors/{request_id}

---

## 7. 非功能设计

### 7.1 性能

- 热路径无锁：DashMap（stores/windows/trie）、ArcSwap（inner/policy/kv_index）、Atomic（计数器/RR）。
- 认证 moka 缓存（10k/5min）；审计异步批量（channel 削峰 + 批量 INSERT）；prompt log 异步 writer。
- 流式零缓冲直 pipe；请求体单次缓冲多 extractor 复用（CachedJson）。
- Dashboard 独立 DB 池，统计查询不侵占转发连接。

### 7.2 可靠性

- reload：60s 超时 + catch_unwind + 失败保留旧配置；慢 DB 步骤 15s 超时。
- 未服务不计费；Drop 链全路径收尾；audit 丢批可观测（drop 计数器 + 60s 报告）。
- 自愈：软负载均衡 + 硬摘除 + 恢复探测三层。
- 内存兜底：YAML/DB 均失败时按内存状态继续服务（最大服务能力原则）。

### 7.3 安全

- 主密钥/登录口令常数时间比较（防时序攻击）；密钥只存 SHA-256 哈希。
- 网关内部头防伪造（x-gateway-*/x-boom-*/authorization 等 13 类硬封禁）；gateway_headers serde skip 防客户端注入。
- JWT HttpOnly cookie；登录 per-IP 防爆破；key 日志只打前 8 位哈希。
- SQL 全参数化；异常检测维度列白名单（防标识符注入）；secret 字段脱敏；read_raw_yaml 不展开 env 引用。
- 错误分层：预期拒绝不落库（should_log_to_db）、上游 401/403 判定为 deployment 故障触发摘除。

### 7.4 可扩展性（扩展点清单）

| 扩展类型 | 步骤 |
|----------|------|
| 新增上游 Provider | boom-provider 新模块 + impl Provider trait + create_provider match 分支；不动 main/routing/limiter |
| 新增调度策略 | boom-routing/src/policy/ 新模块 + impl SchedulePolicy + create_policy 分支 + YAML 切换（热生效） |
| 新增 pre_auth 钩子 | 独立 cdylib 工程（模板 hook/），boom-hooks-sdk ABI，YAML hooks 节挂载 |
| 新增业务模块 | 新 boom-* crate（依赖仅 boom-core）+ boom-main 引入 + 需要时 AdminCommand 加变体 |
| 新增 deployment 字段 | 5 处同步：config 结构体 / manifest / DEPLOYMENT_CORE_COLUMNS+SQL / 前端+i18n / migration（编译期测试强制） |
| 新增 Dashboard 写操作 | AdminCommand 加变体 + admin_command_handler 加分支（禁止 dashboard 依赖 provider/config） |

---

## 8. 关键设计决策记录

| 决策 | 理由 |
|------|------|
| Rust 重写而非复用 litellm | Python GIL 制约转发性能；保留 DB schema 兼容获得迁移能力，上层全部自建 |
| trie 自学习而非订阅上游 KV 事件 | 不依赖 vLLM 改造（ZMQ 方案已废弃）；add-only + LRU/TTL 老化的 over-approximation 换取零后端耦合；命中率偏低时可请求上游全量 cache report 校正 |
| kv_index 第三种生命周期 | block_size 等签名变化后旧 trie 数据语义失配，保留会产出错误高分路由；"重建即清缓存"是显式安全行为（瞬态降级最低负载） |
| Dashboard 不依赖 provider/config | 写操作本质是意图声明，由 boom-main（拥有全部模块访问权）解释执行；保护编译边界与职责边界 |
| boom-promptlog 零依赖 | 纯 IO 组件可独立复用、编译隔离 |
| manifest + 编译期测试 | 字段 5 处同步靠人必然腐化，用编译失败当 forcing function |
| CRUD 直改内存而非 reload | reload 是 YAML→内存单向覆盖会中断路由；"DB 写成功 → 立即可路由"是运行期权威语义 |
| 场景 D 权衡（YAML 只读 + DB 写） | YAML 绝对胜出是显式选择，代价经 warning 字段透明化，不静默丢配置 |
| 并发限制走 plan 而非 litellm 列 | boom_verification_token.max_parallel_requests 为兼容死列，实际并发由 plan.concurrency_limit 执行（单一执行体系避免双轨） |

---

## 9. 已知边界与演进路线

**已知边界（设计时需知）**：

- 累计配额（total_*）为终身累计，无日/月周期自动重置（周期预算走 litellm budget_duration 认证路径）。
- 单实例内存态（inflight/并发 guard/trie）不跨实例共享；多实例下细粒度限流有误差（演进中的 L2/L3 解耦方案）。
- reload 中间步骤失败可能留下 partial-built stores（shadow-build + atomic swap 在 roadmap）。
- 无内置 TLS（前置反代承担）；无独立用户账号体系（认证即密钥）。
- Dashboard 前端表单为手工同步 manifest，未自动渲染 schema。

**演进路线**：L4 AgentAffinity（agent 类型前置过滤池，EWMA + hysteresis，正交于现有策略）；shadow reload；多实例细粒度限流共享（Redis）；prefill 隔离池。
