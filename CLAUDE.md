# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

**注意：`"*"` 在本网关中是一个真实的 model_name（兜底路由），不是 litellm 的"全权限"通配符。** 当用户请求的模型名匹配不到任何已配置的 model_name 时，路由到 `"*"` 对应的 deployment。判断"全权限"的唯一依据是 `models` 数组为空或包含 `"all-team-models"`，不要把 `"*"` 作为全权限标记。

**Intelligence Boom Gateway** — 高性能 LLM API 网关。Rust workspace 实现，Docker 容器化部署。兼容 litellm 密钥体系，自建速率限制、Dashboard、审计、计费等全部上层功能。

## Build & Run

```bash
# 构建 Docker 镜像（网关主程序）
docker build -t boom-gateway boom-gateway/

# 构建 Docker 镜像（负载均衡器）
docker build -t gateway-lb misc/LB/

# LB 启停脚本（自动构建 + 生成证书）
./misc/LB/start.sh start
```

运行时依赖：Docker。构建镜像内使用 rsproxy.cn 作为 crates.io 镜像。

## Architecture Principles（架构原则）

以下原则是本项目迭代开发的硬约束，新增功能或修改代码时必须严格遵守。

### 1. 模块依赖方向：单向、无环

```
boom-core ← boom-auth, boom-provider, boom-config, boom-limiter, boom-routing, boom-audit
boom-main → 依赖所有 boom-* 模块（组装层）
boom-dashboard → boom-core, boom-limiter, boom-routing, boom-audit（禁止依赖 boom-provider, boom-config）
```

- **boom-core 是唯一的叶子依赖**。所有功能模块只依赖 boom-core，不互相依赖。
- **boom-main 是唯一的根**。它负责组装所有模块、管理生命周期、处理路由。其他模块之间不直接通信。
- **boom-dashboard 不依赖 boom-provider 和 boom-config**。Dashboard 需要操作模型/配置时，通过 AdminCommand channel 异步通知 boom-main 处理。

### 2. 每个模块有清晰的职责边界

| Crate | 职责 | 禁止 |
|-------|------|------|
| boom-core | 定义核心 trait（Provider, Authenticator, RateLimiter）和公共类型 | 不包含具体实现 |
| boom-auth | 密钥认证（SHA-256 + DB 查询 + 主密钥） | 不做路由、不做速率限制 |
| boom-config | YAML 配置解析、环境变量展开 | 不持有运行时状态 |
| boom-provider | 构建 Provider 实例（OpenAI/Anthropic/Bedrock 等） | 不做密钥校验、不做计费 |
| boom-limiter | 滑动窗口限流、并发控制、PlanStore | 不感知 Provider |
| boom-routing | DeploymentStore（模型→Provider 映射）、AliasStore（别名解析） | 不做 HTTP 请求 |
| boom-audit | 请求日志读写（boom_request_log 表） | 不做路由决策 |
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
- `store_model_in_db=true` 模式：只重新 seed `source='yaml'` 的 DB 行，`source='db'` 的行不动。
- `store_model_in_db=false` 模式：从 YAML 重建所有 store。
- **热加载不得清除运行时计数器**（limiter、concurrency guard、assignment 等不受影响）。

### 8. 新增模块检查清单

新增一个 boom-* crate 时，确认：
- [ ] `Cargo.toml` 中依赖只包含 boom-core（或明确确认的单向依赖）
- [ ] 不引入对其他 boom-* 模块的循环依赖
- [ ] DB 表（如有）以 `boom_` 前缀命名，DDL 只在本 crate 内
- [ ] 公共 API 通过 `pub use` 在 `lib.rs` 明确导出
- [ ] boom-main 的 `Cargo.toml` 和 `state.rs` 已更新以引入新模块

## Key Patterns

- **配置共享**：`Arc<ArcSwap<T>>` 原子交换，零停机热加载
- **Provider 选择**：`DeploymentStore` 按 model_name 分组，round-robin 选择
- **别名解析**：`AliasStore` 提供 alias → target_model 映射
- **速率限制**：PlanStore（key → plan → limits）+ SlidingWindowLimiter + ConcurrencyGuard（RAII）
- **请求审计**：`LoggedStream<S>` 包装 SSE 流，在 Drop 时写入真实 duration
- **Dashboard 解耦**：AdminCommand channel 实现跨模块写操作

## Repository Layout

```
boom-gateway/           — Rust workspace root
  boom-core/            — 核心 trait 和公共类型
  boom-auth/            — 密钥认证（litellm 兼容）
  boom-config/          — YAML 配置解析
  boom-provider/        — LLM Provider 实现（OpenAI/Anthropic/Bedrock 等）
  boom-limiter/         — 速率限制 + 并发控制 + PlanStore
  boom-routing/         — DeploymentStore + AliasStore
  boom-audit/           — 请求日志读写
  boom-dashboard/       — Web 管理 UI + REST API
  boom-main/            — 主程序入口、路由、状态组装
misc/LB/                — Pingora 负载均衡代理（独立项目）
```
