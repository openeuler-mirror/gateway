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
| boom-gatekeeper | 客户端拦截（client blocklist 规则匹配 + enabled 开关，DB 权威） | 不做密钥校验、不做路由 |
| boom-audit | 请求日志读写（boom_request_log 表） | 不做路由决策 |
| boom-trace | 请求级 trace 链路 + 延迟分布（in-memory 直方图、慢请求 ring buffer、OTLP traces 导出） | 不写 DB 表（持久化归 boom-audit）、不记 prompt 内容（归 boom-promptlog）、不做路由决策 |
| boom-dashboard | Web UI + REST API + JWT 认证 | 不直接操作 Provider/Config |
| boom-main | 路由处理、状态组装、热加载、后台任务 | 不在 handler 里写业务逻辑 |

### 3. DB 表所有权（DDL Ownership）

每个模块拥有且只操作自己的表。**跨模块直接操作他人的表是禁止的**。

- boom-audit: `boom_request_log`
- boom-routing: `boom_model_deployment`, `boom_model_alias`, `boom_workflow_definition`, `boom_workflow_model`, `boom_cost_template`
- boom-limiter: `boom_rate_limit_state`, `boom_rate_limit_cumulative`, `boom_key_plan_assignment`, `boom_rate_limit_plan`, `boom_team_plan_assignment`
- boom-gatekeeper: `boom_client_block_rule`, `boom_client_blocklist_state`
- boom-dashboard: `boom_config`
- boom-main: `boom_config_meta`（seed 幂等标记 — 唯一例外：表本身属于启动编排层，由 from_config/reload 的 seed_if_empty 读写）
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

### 7. 热加载（Hot Reload）与配置权威模型

- SIGHUP / `POST /admin/config/reload` 触发热加载。
- **DB 是唯一运行时权威（业务配置）**：model_list、alias、plan、client_blocklist、workflow_settings、cost 模板全部以 DB 为准。YAML 中的这些 runtime section **完全 inert**（reload 忽略、启动只作 seed）。
- **YAML 只保留进程/主机级配置**：server、general_settings（master_key/database_url）、router 调度参数、prompt_log、trace、hooks、deployment_health_check。自举链（db_url/master_key/监听地址）永远来自 YAML 或环境变量。
- **seed 幂等**：启动时若业务表全空且 `boom_config_meta` 无 seed 标记，才从 YAML runtime section 导入（`seed_if_empty`，单事务 + advisory lock，GaussDB 不支持时 warn 降级）。标记存在则跳过——"删光模型后重启被 YAML 复活"不会发生。灾备恢复 = 空库 + 回写生成的 YAML 快照重启。
- **无 DB 回退模式**（`general_settings.database_url` 缺失，master-key-only/e2e fixture）：保留 YAML 构建路径（clear+rebuild），这是部署形态选择（Kong dbless 模式），不是双权威合并。
- **无 YAML 启动（env bootstrap）**：`--config` 缺省且 cwd 无 config.yaml（或指定文件不存在）时，从 `BOOM_MASTER_KEY` + `BOOM_DATABASE_URL`（两者必需）构建最小进程配置，业务配置全部来自 DB。回写按钮在 cwd 生成完整 `config.yaml`（0600，`${BOOM_*}` 引用），重启即切换为文件模式。
- **SIGHUP 语义**：重读 YAML 进程配置（env bootstrap 模式则保持当前进程配置）+ 对 DB 业务配置做**增量对账**（per-key diff，永不清空 store——内存有 DB 无 → remove，DB 有内存无/有变化 → apply）。对账失败中止 reload（旧 inner 继续路由）。YAML 仍含非空 runtime section 时打 warn 提示 inert。
- **reload 不得清除运行时计数器**（limiter、concurrency guard、assignment 等不受影响）。
- **kv_index 是第三种生命周期**：它放在 AppState 顶层（`Arc<ArcSwap<Option<…>>>`），但与 deployment_store / plan_store 等"跨 reload 内容存活"不同——`schedule_policy` 切换会**重建空 trie**（旧 trie 随 ArcSwap 替换被 drop，新 trie 从空开始）；`max_blocks` / `router_ttl_secs` 变化不 wipe trie（重启 prune task / LRU 容量自适应）。**trie 是自学习的**：不订阅 vLLM ZMQ 事件，由 `KvcOrchestrator` 在每次路由后把请求前缀（system+tools+messages 字节序列化 → 按 512B 代码常量切块 → xxhash，`KV_BLOCK_SIZE`，非配置项）记录到选中 worker 下；下一个相同前缀的请求即命中。驱逐只有 gateway 侧 LRU（`max_blocks`）+ TTL（`router_ttl_secs`，后台扫描）。这是 over-approximation（vLLM 实际 evict 后 trie 仍乐观保留，靠 LRU/TTL 老化）。打分是**纯前缀亲和**（score = hit_ratio，无负载项、无过载门）；冷请求并列 0 分由 round-robin 摊开，负载干预仅剩 router 级 `rebalance_threshold`。瞬态查询命中空 trie → 0 hit → round-robin（无 key_affinity 回退）。
- **已废弃字段**：`general_settings.store_model_in_db`（v2 双模式开关）在 v3 中已废弃。YAML 中残留该字段会被静默忽略（`#[serde(alias)]`），导出/回写时不再写出。不要在新 YAML 中使用。

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
2. **`boom-config/src/manifest.rs`** — `model_deployment_fields()` 中注册 `FieldMeta`（label_key、tip_key、section、input_type、**home**）
3. **`boom-routing/src/deployment_store.rs`** — `DEPLOYMENT_CORE_COLUMNS` const + `DeploymentInput` + 5 个 SQL 语句（INSERT ×2 / UPDATE / SELECT ×2）
4. **`boom-dashboard/src/frontend/app.js`** — 表单字段 HTML + 提交 body 字段；`i18n.js` 加 label/tip 翻译
5. **DB migration** — `boom-main/migrations/` 加 ALTER TABLE

**强制约束（forcing functions）：**
- `boom-config::manifest::tests::manifest_covers_all_struct_fields` —— `ProviderParams`/`ModelEntry` 等结构体的字段必须在 manifest 中注册，否则编译失败
- `boom-config::manifest::tests::manifest_homes_match_authority_split` —— 业务字段（模型/cost 模板/workflow）必须 `home: Db`，进程字段（general/router）必须 `home: Yaml`；`ConfigHome` 是 §7 权威拆分在字段级的登记
- `boom-routing::deployment_store::tests::{update_sql_mentions_all_core_columns, insert_sql_mentions_all_core_columns, select_sql_mentions_all_core_columns}` —— `DEPLOYMENT_CORE_COLUMNS` 中每个列名必须出现在 SQL 字符串里，否则编译失败
- `GET /admin/config/schema`（由 `AdminCommand::GetConfigSchema` 透传 `boom_config::manifest::*`，响应含 home 字段）—— 前端可拉取 manifest 自动渲染（当前前端是手工同步，未自动渲染）

**不要做的反模式：**
- 不要在 `state.rs::build_config_snapshot_value` 中临时拼字段 —— 它应当只是 manifest + 结构体的派生输出（业务转换除外，如 `Decimal/per_token → per_million`）
- 不要新增"只在前端 / 只在 DB / 只在 YAML"出现的字段 —— manifest 是登记处，未登记即不存在
- 不要绕过 manifest 直接 grep SQL 加字段 —— 走 const → 测试 → SQL 的链路，让编译失败当 guard

### 10. 配置写路径（DB 权威两步化）模式

CRUD handler（model/alias/plan/quota/blocklist/workflow/cost template 等）的写路径只有两步，**没有任何自动 YAML 回写**：

1. **DB 写**（store.*_db / AdminCommand handler 内部完成 INSERT/UPDATE/DELETE）。
2. **直改内存**：DB 写成功后立即调 `reload_model_deployments`（model/cost 路径）或对应的 `*_store.*` 内存方法（alias/plan/blocklist/workflow 路径）——新配置立即可路由，不依赖 reload。

**CRUD handler 不调 `reload()` 也不写 YAML**：reload 是对账 + 策略重建（清空式重建只在 no-DB 模式）；CRUD 已直改内存，再 reload 反而违反"最大服务能力"原则。YAML 快照的刷新是**手动操作**（导出/回写按钮，见下），不是写路径的一环。

**YAML 快照工具（手动）**：
- **导出按钮**（`GET /dashboard/api/admin/config/export`）：DB 快照 + 原始 YAML 未触碰 section（保留 `${VAR}` 引用）→ 下载，不落盘。
- **回写按钮**（`POST /dashboard/api/admin/config/writeback`）：同上管道 → 写回启动文件（先 `.bak` 单槽备份）。env bootstrap 模式下则在 cwd 生成新 `config.yaml`（0600）。用途 = 回滚种子/灾备工件，**不是**恢复同步语义。
- 文件含明文 secrets（api_key 等）——0600、视同备份工件。

**进程级配置写路径不变**：Config 页 section editor 走 `update_config_section`（读 YAML → 改单 section → 写回 → reload），但带**黑名单**：`model_list` / `plan_settings` / `client_blocklist` / `router_settings.model_group_alias` / `workflow_settings` / `cost_templates` 一律拒绝并指向 Dashboard CRUD。env bootstrap 模式（无文件）下 section editor 报错指向回写按钮。

**plan_settings.default_plan / default_team_plan / client_blocklist.enabled 三个原 YAML 项**已 DB 化（`boom_rate_limit_plan.is_default_team` 部分唯一索引 + `boom_client_blocklist_state` 单行表），各有专属 Dashboard 端点。

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
