# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

**注意：`"*"` 在本网关中是一个真实的 model_name（兜底路由），不是 litellm 的"全权限"通配符。** 当用户请求的模型名匹配不到任何已配置的 model_name 时，路由到 `"*"` 对应的 deployment。判断"全权限"的唯一依据是 `models` 数组为空或包含 `"all-team-models"`，不要把 `"*"` 作为全权限标记。

**Intelligence Boom Gateway** — 高性能 LLM API 网关。Rust workspace 实现，Docker 容器化部署。兼容 litellm 密钥体系，自建速率限制、Dashboard、审计、计费等全部上层功能。

## Commit 规范

提交代码时必须使用 `git commit -s`，这会自动添加作者的 `Signed-off-by` 签名行。同时 Claude Code 在 commit message 末尾添加 `Co-Authored-By` 信息。典型 commit message 格式：

```
fix: capture streaming response content in prompt log

描述改动的目的和原因...

Signed-off-by: 作者 <email>
Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

## PR Review

使用 `/atomgit_pr_review` skill 进行 PR 审查（含拉取代码、审查、发表评论的完整流程）。

### 审查重点

1. **架构合规**：是否符合上述模块依赖原则（单向、无环、职责边界清晰）？是否引入了禁止的跨模块依赖？DB 表操作是否越界？
2. **架构破坏**：是否破坏了现有约定（AppState 生命周期、AdminCommand 模式、热加载规则等）？
3. **代码质量**：编译是否通过、测试是否覆盖、是否有明显的逻辑错误或安全漏洞（OWASP）？

## Architecture Principles（架构原则）

以下原则是本项目迭代开发的硬约束，新增功能或修改代码时必须严格遵守。

### 1. 模块依赖方向：单向、无环

```
boom-core ← boom-auth, boom-provider, boom-config, boom-limiter, boom-audit, boom-trace
boom-routing → boom-core, boom-config, boom-ctxaware, boom-flowcontrol, boom-fusion
boom-main → 依赖所有 boom-* 模块（组装层）
boom-dashboard → boom-core, boom-limiter, boom-routing, boom-audit（禁止依赖 boom-provider, boom-config）
```

- **boom-core 是唯一的叶子依赖**。所有功能模块只依赖 boom-core，不互相依赖。
- **boom-main 是唯一的根**。它负责组装所有模块、管理生命周期、处理路由。其他模块之间不直接通信。
- **boom-dashboard 不依赖 boom-provider 和 boom-config**。Dashboard 需要操作模型/配置时，通过 AdminCommand channel 异步通知 boom-main 处理。
- **boom-trace 是 leaf crate**（与 boom-stressmon 同款），只依赖 boom-core。`TraceApi` trait 定义在 boom-core（§5 trait-over-concrete-type 模式），让 boom-dashboard 用 `Arc<dyn TraceApi>` 消费而不依赖本 crate。

### 2. 每个模块有清晰的职责边界

| Crate | 职责 | 禁止 |
|-------|------|------|
| boom-core | 定义核心 trait（Provider, Authenticator, RateLimiter）和公共类型 | 不包含具体实现 |
| boom-auth | 密钥认证（SHA-256 + DB 查询 + 主密钥） | 不做路由、不做速率限制 |
| boom-config | YAML 配置解析、环境变量展开 | 不持有运行时状态 |
| boom-provider | 构建 Provider 实例（OpenAI/Anthropic/Bedrock 等） | 不做密钥校验、不做计费 |
| boom-limiter | 滑动窗口限流、并发控制、PlanStore | 不感知 Provider |
| boom-routing | DeploymentStore、AliasStore、调度策略、Fusion 虚拟 Provider 编排 | 不实现上游 HTTP 协议；只调用 `Provider` trait |
| boom-audit | 请求日志读写（boom_request_log 表） | 不做路由决策 |
| boom-trace | 请求级 trace 链路 + 延迟分布（in-memory 直方图、慢请求 ring buffer、OTLP traces 导出） | 不写 DB 表（持久化归 boom-audit）、不记 prompt 内容（归 boom-promptlog）、不做路由决策 |
| boom-dashboard | Web UI + REST API + JWT 认证 | 不直接操作 Provider/Config |
| boom-main | 路由处理、状态组装、热加载、后台任务 | 不在 handler 里写业务逻辑 |

### 3. DB 表所有权（DDL Ownership）

每个模块拥有且只操作自己的表。**跨模块直接操作他人的表是禁止的**。

- boom-audit: `boom_request_log`
- boom-routing: `boom_model_deployment`, `boom_model_alias`
- boom-limiter: `boom_rate_limit_state`, `boom_key_plan_assignment`, `boom_rate_limit_plan`
- boom-dashboard: `boom_config`
- boom-auth: 读取 `LiteLLM_VerificationToken`, `LiteLLM_TeamTable`（litellm 兼容，只读）

需要跨模块写表时，通过 AdminCommand channel 或公共 API 间接操作。

### 4. 状态生命周期

```
AppState (Clone, 整个生命周期存活)
  ├─ inner: Arc<ArcSwap<AppStateInner>>  — 热替换（config + auth + health）
  ├─ db_pool: Option<PgPool>             — 跨 reload 存活
  ├─ deployment_store                    — 跨 reload 存活（DashMap）
  ├─ alias_store                         — 跨 reload 存活（DashMap）
  ├─ plan_store                          — 跨 reload 存活（DashMap）
  └─ limiter                             — 跨 reload 存活（滑动窗口计数器）
```

**规则：**
- 可变配置（会被热加载替换的）放入 `AppStateInner`，通过 `ArcSwap` 原子交换。
- 持久化状态（DeploymentStore、PlanStore 等）放入 AppState 顶层，跨 reload 存活。
- 新增需要跨 reload 存活的状态时，放入 AppState 顶层 Arc，不要放入 AppStateInner。

### 5. AdminCommand Channel 模式

Dashboard 需要执行写操作（创建模型、修改配置等）时：
1. Dashboard 发送 `AdminCommand` 到 `mpsc::channel`。
2. boom-main 的 `admin_command_handler` 接收并执行（拥有所有模块的完整访问权限）。
3. 这确保了 boom-dashboard 不需要依赖 boom-provider 和 boom-config。

**新增 Dashboard 写操作时，必须：**
- 在 boom-dashboard 的 `AdminCommand` enum 中添加变体。
- 在 boom-main 的 `admin_command_handler` 中添加处理分支。
- 不要在 boom-dashboard 中引入对 boom-provider 或 boom-config 的依赖。

### 6. 路由处理原则

- boom-main 的 routes.rs 只做：提取参数 → 调用模块 API → 组装响应。不在 handler 里写业务逻辑。
- 流式响应必须使用 `LoggedStream`（或类似 Drop-trait 包装器）记录真实 duration，不要在流开始时记录日志。
- 中间件放在路由之后，注意过滤条件（如只对 `/v1/` 和 `/admin/` 路径计数）。

### 7. 热加载（Hot Reload）规则

- SIGHUP / `POST /admin/config/reload` 触发热加载。
- **三数据源角色**：
  - **YAML** — 声明式真相源 + 启动种子。reload 时绝对胜出：覆盖 DB 中 `source='yaml'` 行 + 删除冲突的 `source='db'` 行。运行期写为 best-effort，失败只 warn 不阻断。
  - **DB** — 运行期权威 + 多实例共享。CRUD 写操作目标；`store.*_db` 方法 DB 写成功后立即调 `reload_model_deployments`（boom-main 层）直改内存，新部署立即可路由，不依赖 YAML 写或 reload。
  - **内存** — 路由唯一来源 + 最大服务兜底。启动从 YAML+DB 拉；运行期 CRUD 直改；YAML 和 DB 都失败时仍按内存状态服务。
- **reload 不得清除运行时计数器**（limiter、concurrency guard、assignment 等不受影响）。
- **场景 D（YAML 只读 + DB 写成功后 reload）的已知权衡**：reload 走 YAML→内存→sync_yaml_to_db，YAML 是旧值会把 DB 中 `source='db'` 的新行视为冲突删除。这是显式选择（YAML 绝对胜出），通过 CRUD handler 返回的 `warning` 字段提示运维修复 YAML 权限后重新应用。
- **kv_index 是第三种生命周期**：它放在 AppState 顶层（`Arc<ArcSwap<Option<…>>>`），但与 deployment_store / plan_store 等"跨 reload 内容存活"不同——`schedule_policy` / `block_size` / `max_blocks` / `router_ttl_secs` 变化会**重建空 trie**（旧 trie 随 ArcSwap 替换被 drop，新 trie 从空开始）。**trie 是自学习的**：不订阅 vLLM ZMQ 事件，由 `KvcOrchestrator` 在每次路由后把请求前缀（system+tools+messages 字节序列化 → 按 `block_size` 切块 → xxhash）记录到选中 worker 下；下一个相同前缀的请求即命中。驱逐只有 gateway 侧 LRU（`max_blocks`）+ TTL（`router_ttl_secs`，后台扫描）。这是 over-approximation（vLLM 实际 evict 后 trie 仍乐观保留，靠 LRU/TTL 老化）。`cache_weight`/`load_weight`/`tier_weight` 纯权重变化不 wipe trie（policy 热重建即可）。瞬态查询命中空 trie → 0 hit → 按负载评分路由（无 key_affinity 回退）。
- **已废弃字段**：`general_settings.store_model_in_db`（v2 双模式开关）在 v3 中已废弃。新模型总是 YAML 真相源 + DB 运行期权威。YAML 中残留该字段会被静默忽略（`#[serde(alias)]`），不要在新 YAML 中使用。

### 8. 新增模块检查清单

新增一个 boom-* crate 时，确认：
- [ ] `Cargo.toml` 中依赖只包含 boom-core（或明确确认的单向依赖）
- [ ] 不引入对其他 boom-* 模块的循环依赖
- [ ] DB 表（如有）以 `boom_` 前缀命名，DDL 只在本 crate 内
- [ ] 公共 API 通过 `pub use` 在 `lib.rs` 明确导出
- [ ] boom-main 的 `Cargo.toml` 和 `state.rs` 已更新以引入新模块

### 9. 配置字段单一真相源（manifest 原则）

YAML / DB / Dashboard 前端涉及"同一个字段"的多个定义点。新增或修改 deployment 字段时，必须同步以下 5 处，否则会出现"DB 写得进、前端看不见"或"前端能编辑但 SQL 漏字段"等腐化场景：

1. **`boom-config/src/lib.rs`** — `ProviderParams` / `ModelEntry` / `ModelInfo` / `FlowControlEntry` 结构体（YAML 解析）
2. **`boom-config/src/manifest.rs`** — `model_deployment_fields()` 中注册 `FieldMeta`（label_key、tip_key、section、input_type）
3. **`boom-routing/src/deployment_store.rs`** — `DEPLOYMENT_CORE_COLUMNS` const + `DeploymentInput` + 5 个 SQL 语句（INSERT ×2 / UPDATE / SELECT ×2）
4. **`boom-dashboard/src/frontend/app.js`** — 表单字段 HTML + 提交 body 字段；`i18n.js` 加 label/tip 翻译
5. **DB migration** — `boom-main/migrations/` 加 ALTER TABLE

**强制约束（forcing functions）：**
- `boom-config::manifest::tests::manifest_covers_all_struct_fields` —— `ProviderParams`/`ModelEntry` 等结构体的字段必须在 manifest 中注册，否则编译失败
- `boom-routing::deployment_store::tests::{update_sql_mentions_all_core_columns, insert_sql_mentions_all_core_columns, select_sql_mentions_all_core_columns}` —— `DEPLOYMENT_CORE_COLUMNS` 中每个列名必须出现在 SQL 字符串里，否则编译失败
- `GET /admin/config/schema`（由 `AdminCommand::GetConfigSchema` 透传 `boom_config::manifest::*`）—— 前端可拉取 manifest 自动渲染（当前前端是手工同步，未自动渲染）

**不要做的反模式：**
- 不要在 `state.rs::build_config_snapshot_value` 中临时拼字段 —— 它应当只是 manifest + 结构体的派生输出（业务转换除外，如 `Decimal/per_token → per_million`）
- 不要新增"只在前端 / 只在 DB / 只在 YAML"出现的字段 —— manifest 是登记处，未登记即不存在
- 不要绕过 manifest 直接 grep SQL 加字段 —— 走 const → 测试 → SQL 的链路，让编译失败当 guard

### 10. 配置写路径（CRUD + YAML 同步）模式

CRUD handler（model/alias/plan/quota reset 等）的写路径必须遵循以下顺序，违反任何一条都会复现 "YAML 只读 → 新模型 model_not_found" 的旧 bug：

1. **DB 写**（store.*_db 内部完成 DB INSERT/UPDATE/DELETE）。
2. **直改内存**：DB 写成功后立即调 `reload_model_deployments`（model 路径）或对应的 `*_store.*` 内存方法（alias/plan 路径）——**不依赖后续 reload 或 YAML 写**。这是关键：即使 YAML 是只读的，新配置也必须立即可路由。
3. **best-effort 写 YAML**：调 `DashboardState::persist_yaml_with_reply()`（内部走 `AdminCommand::ConfigChanged` → boom-main 的 `persist_config_in_place`）。失败只 warn，不阻断。
4. **响应前端**：YAML 写失败时，handler 在 JSON 响应中加 `warning: Option<String>` 字段（不能转成 error——主操作已成功）；前端弹提示让运维修复 YAML 权限。
5. **CRUD handler 不调 `reload()`**：reload 是 YAML→内存的单向覆盖，会清空 store 短暂中断路由。CRUD 路径已经直改内存，再 reload 反而违反"最大服务能力"原则。

**DeploymentStore 与 PlanStore/AliasStore 的对齐**：v2 时 DeploymentStore 的 `create_db/update_db/delete_db` 是关联函数（只写 DB 不改内存），是配置写路径的唯一例外。v3 中已对齐——所有 store 的 `*_db` 方法在 DB 写成功后由调用方（boom-main 的 handler 层）调内存直改 helper，与 PlanStore/AliasStore 行为一致。

**ConfigChanged reply channel**：所有 18 处 alias/plan/quota CRUD handler 的 `admin_tx.send(ConfigChanged)` 都改走 `persist_yaml_with_reply()`，YAML 写状态通过 reply surface 到 HTTP 响应的 `warning` 字段——满足"YAML 写失败必须有明确提示"的要求，不再静默丢。

## Key Patterns

- **配置共享**：`Arc<ArcSwap<T>>` 原子交换，零停机热加载
- **Provider 选择**：`DeploymentStore` 按 model_name 分组，round-robin 选择
- **别名解析**：`AliasStore` 提供 alias → target_model 映射
- **速率限制**：PlanStore（key → plan → limits）+ SlidingWindowLimiter + ConcurrencyGuard（RAII）
- **请求审计**：`LoggedStream<S>` 包装 SSE 流，在 Drop 时写入真实 duration
- **Dashboard 解耦**：AdminCommand channel 实现跨模块写操作

## Repository Layout

```
crates/                 — 全部 workspace 成员 crate
  boom-core/            — 核心 trait 和公共类型
  boom-auth/            — 密钥认证（litellm 兼容）
  boom-config/          — YAML 配置解析
  boom-provider/        — LLM Provider 实现（OpenAI/Anthropic/Bedrock 等）
  boom-limiter/         — 速率限制 + 并发控制 + PlanStore
  boom-routing/         — DeploymentStore + AliasStore
  boom-audit/           — 请求日志读写
  boom-trace/           — 请求级 trace 链路 + 延迟分布
  boom-dashboard/       — Web 管理 UI + REST API
  boom-main/            — 主程序入口、路由、状态组装
test/                   — 压测与测试工具（独立于主 workspace 的子项目）
hook/                   — pre_auth hook demo（独立 workspace）
misc/LB/                — Pingora 负载均衡代理（独立项目）
```
