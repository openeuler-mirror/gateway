# BooMGateway 架构描述

## 定位

BooMGateway 是一个高性能 LLM API 网关，提供统一的请求入口来访问多个 LLM 提供商。兼容 litellm 数据库表结构（密钥认证层），其余模块（限流、套餐、管理 API、Dashboard）均为自建。

## 技术栈

- **语言**: Rust (edition 2021)
- **HTTP 框架**: Axum
- **异步运行时**: Tokio (32 worker threads)
- **数据库**: PostgreSQL (sqlx, 异步)
- **并发数据结构**: DashMap (无锁并发 HashMap)
- **热重载**: ArcSwap (原子状态交换)
- **Token 缓存**: moka (异步缓存, 5 min TTL)
- **日志**: tracing + tracing-subscriber (JSON 格式输出, 非阻塞 writer)

## 项目结构

```
boom-gateway/
├── boom-core/         核心类型、接口定义、Anthropic 协议转换
├── boom-config/       YAML 配置加载、环境变量解析
├── boom-auth/         密钥认证（litellm DB 兼容）
├── boom-provider/     上游 LLM 提供商适配器
├── boom-limiter/      速率限制、并发控制、套餐管理
├── boom-routing/      DeploymentStore、AliasStore、调度策略、InFlightTracker
├── boom-audit/        请求日志读写（boom_request_log）
├── boom-dashboard/    Web 管理面板（SPA + REST API）
└── boom-main/         HTTP 服务、路由、状态管理、启动入口
```

## 模块分层

```
┌─────────────────────────────────────────────────┐
│  boom-main (HTTP 层)                            │
│  路由 / 认证提取器 / 状态管理 / 热重载          │
├─────────────────────────────────────────────────┤
│  boom-auth    boom-limiter    boom-provider     │
│  密钥认证     限流 + 套餐      提供商适配        │
│              boom-routing     boom-audit         │
│              部署+别名+调度    请求日志           │
├─────────────────────────────────────────────────┤
│  boom-config                 boom-core          │
│  配置加载                     类型 / 接口 / 转换  │
└─────────────────────────────────────────────────┘
```

---

## 各模块详细描述

### boom-core

核心类型和接口，所有其他模块共同依赖。

**导出内容：**

| 类型 / 接口 | 说明 |
|---|---|
| `GatewayError` | 统一错误类型，12 种变体，每种映射到 HTTP 状态码 |
| `Provider` trait | 提供商接口：`chat()` / `chat_stream()` |
| `RateLimiter` trait | 限流器接口：`check_and_record(key, rpm_limit, window_limits, weight)` |
| `Authenticator` trait | 认证器接口：`authenticate()` / `check_model_access()` |
| `ChatCompletionRequest` | OpenAI 格式请求 |
| `ChatCompletionResponse` | OpenAI 格式响应 |
| `ChatStreamChunk` | SSE 流式分片 |
| `AnthropicMessagesRequest/Response` | Anthropic Messages API 格式 |
| `AuthIdentity` | 认证后身份信息（key_hash, models, rpm_limit 等） |
| `RateLimitKey` | 限流键（key_hash + model） |
| `RateLimitDecision` | 限流判定结果 |

**Anthropic 协议转换（`anthropic.rs`）：**
- `anthropic_request_to_openai()` — 请求转换（system → System role, tool_use → tool_calls, image block → image_url）
- `openai_response_to_anthropic()` — 响应转换（tool_calls → ToolUse blocks, finish_reason → stop_reason）
- `AnthropicStreamTranscoder` — 流式转码（有状态），将 OpenAI SSE chunks 实时转换为 Anthropic SSE 事件序列（message_start → content_block_start → content_block_delta → content_block_stop → message_delta → message_stop）

---

### boom-config

YAML 配置加载，兼容 litellm 的 `proxy_server_config.yaml` 格式。

**主要结构：**

| 结构体 | YAML 键 | 说明 |
|---|---|---|
| `Config` | (根) | 顶级配置，所有子项均有 `#[serde(default)]` |
| `ModelEntry` | `model_list[]` | 模型部署条目（含 `litellm_params` + `model_info`） |
| `ModelInfo` | `model_list[].model_info` | 模型元信息（id, cost, quota_count_ratio） |
| `ProviderParams` | `model_list[].litellm_params` | 提供商参数（model, api_key, api_base, timeout 等） |
| `GeneralSettings` | `general_settings` | master_key, database_url, store_model_in_db |
| `RouterSettings` | `router_settings` | schedule_policy, model_group_alias, key_affinity 参数 |
| `ServerSettings` | `server` | host, port, workers |
| `RateLimitSettings` | `rate_limit` | enabled, default_rpm, window_limits |
| `PlanSettings` | `plan_settings` | default_plan, plans HashMap |
| `PlanConfig` | `plan_settings.plans.xxx` | 套餐配置（concurrency_limit, rpm_limit, window_limits, schedule） |
| `ScheduleSlotConfig` | `plan_settings.plans.xxx.schedule[]` | 时段配置（hours, limits） |
| `ModelGroupAlias` | `router_settings.model_group_alias` | 模型别名（Simple / Extended + hidden） |

**环境变量解析：**
- 支持 `${VAR_NAME}` 和 `os.environ/VAR_NAME` 语法
- 在原始 YAML 文本层面解析（解析前替换）
- 对 master_key, database_url, api_key 等字符串字段二次解析

**提供商自动检测：**
- `openai/` 前缀 → OpenAI，`anthropic/` → Anthropic，`azure/` → Azure，`gemini/` → Gemini，`bedrock/` → Bedrock
- 无前缀时按模型名自动检测：`gpt-*`, `o1-*`, `o3-*`, `o4-*` → OpenAI；`claude-*` → Anthropic；`gemini-*` → Gemini；`anthropic.*`, `amazon.*` → Bedrock

---

### boom-auth

密钥认证，读取 litellm PostgreSQL 数据库表（只读兼容层）。

**核心组件：**

| 组件 | 说明 |
|---|---|
| `DbAuthenticator` | 数据库认证器，实现 `Authenticator` trait |
| `VerificationToken` | 映射 `LiteLLM_VerificationToken` 表 |
| `TeamRow` | 映射 `LiteLLM_TeamTable` 表 |

**认证流程：**
1. Master key 恒定时间比较（防时序攻击）
2. `sk-` 前缀密钥进行 SHA-256 哈希
3. 缓存查找（moka, 5 min TTL）→ 缓存未命中则查询数据库
4. 特殊模型名解析：`all-team-models` → 团队模型列表；`all-proxy-models` / `*` → 全部允许
5. 密钥校验：blocked / expired / budget exceeded

---

### boom-provider

上游 LLM 提供商适配器，实现 `Provider` trait。

| 提供商 | 文件 | 说明 |
|---|---|---|
| OpenAI 兼容 | `openai.rs` | OpenAI / vLLM / Ollama / DeepSeek / Groq 等 20+ OpenAI 兼容服务 |
| Anthropic | `anthropic.rs` | Claude Messages API，含 SSE 流式处理 |
| Azure OpenAI | `azure.rs` | Azure 部署名 + API 版本 + api-key 头部认证 |
| Google Gemini | `gemini.rs` | Gemini API Key 查询参数 + generationConfig |
| AWS Bedrock | `bedrock.rs` | Bedrock 调用结构（SigV4 待实现） |

**提供商分派（`lib.rs::create_provider()`）：**
- 按 `provider_type` 字符串匹配创建对应 Provider 实例
- 非 OpenAI 类型（vLLM / Ollama 等）无 api_key 时使用占位符
- 不需要 api_key 的自部署服务自动适配

---

### boom-limiter

速率限制、并发控制和套餐管理。

**核心组件：**

| 组件 | 说明 |
|---|---|
| `SlidingWindowLimiter` | 滑动窗口限流器，DashMap 无锁并发，支持 RPM + 自定义时间窗口 + 加权计数 |
| `PlanStore` | 套餐存储，DashMap 无锁并发，跨 reload 存活 |
| `RateLimitPlan` | 套餐定义（name, concurrency_limit, rpm_limit, window_limits, schedule） |
| `ScheduleSlot` | 时段定义（hours, limits），支持跨午夜（如 `"21:00-9:00"`） |
| `ConcurrencyGuard` | RAII 并发守卫，drop 时自动递减 |
| `GuardedStream` | 流式守卫，stream 结束或客户端断开时释放并发槽位 |

**限流层级（三级 fallback）：**
1. Key 显式分配了套餐（Admin API） → 用套餐的 `effective_limits()`
2. 无显式分配但有 `default_plan` → 用默认套餐
3. 都没有 → 用 config 级 `rate_limit`（per-key per-model 滑动窗口）

**加权计数（quota_count_ratio）：**
- `check_and_record` 接受 `weight` 参数
- 计数器增加 `weight` 而非固定 1
- 大杯模型配置 `quota_count_ratio: 3` 时，每次请求消耗 3 个配额
- 默认 weight=1，未配置时行为不变

**时段调度：**
- 每个套餐可配置 `schedule` 时段列表
- `hours` 格式 `"H:MM-H:MM"`，支持跨午夜
- 遍历 schedule，第一个匹配当前本地时间的 slot 生效
- 无匹配时 fallback 到套餐基础配置

---

### boom-routing

模型部署管理、别名解析、调度策略和实时请求追踪。

**核心组件：**

| 组件 | 说明 |
|---|---|
| `DeploymentStore` | 模型→Provider 映射，轮询选择，配额倍率管理 |
| `AliasStore` | 模型别名→目标模型映射，支持 hidden 标记 |
| `Router` | 封装 DeploymentStore + AliasStore + SchedulePolicy |
| `SchedulePolicy` trait | 调度策略接口 |
| `RoundRobinPolicy` | 轮询负载均衡 |
| `KeyAffinityPolicy` | 会话亲和（同一 key 倾向同一 deployment） |
| `InFlightTracker` | 实时请求追踪（per model + per deployment） |

**调度策略参数（key_affinity 模式）：**
- `key_affinity_context_threshold`: 上下文字符数阈值，低于此值优先最低负载
- `key_affinity_rebalance_threshold`: 再均衡阈值（请求数差异超过此值时重新分配）

---

### boom-audit

请求日志读写。

**核心功能：**
- `log_request()` — 写入 boom_request_log
- `RequestLog` — 日志记录结构体
- 记录字段：request_id, key_hash, key_name, key_alias, team_id, model, api_path, is_stream, status_code, error_type, error_message, input_tokens, output_tokens, duration_ms, deployment_id, created_at
- team_alias 通过 JOIN boom_team_table 获取（不存储在日志表中）

---

### boom-dashboard

Web 管理面板（SPA + REST API）。

**前端：**
- 纯 JavaScript SPA（`app.js`），嵌入到 binary 中
- CSS-only tooltip（`.field-tip` + `::after` 伪元素）
- Datalist 下拉选择（模型名、套餐名、key hash 等）
- 所有配置字段均有 `?` 帮助图标

**管理页面：**
- Stats（模型统计 + 实时 in-flight 请求）
- Models（模型部署 CRUD + quota ratio 列）
- Aliases（模型别名 CRUD）
- Plans（套餐 CRUD）
- Keys（密钥管理 + 实时使用量 + 重置倒计时 + 全局使用量排序）
- Assignments（key → plan 分配）
- Teams（团队列表 + token 统计）
- Logs（请求日志 + 列级过滤 + 分页）
- Config（KV 配置编辑）

**用户面板：**
- Plan 信息（套餐详情）
- Usage（实时限流窗口 + 进度条 + 配额重置倒计时）
- Token 统计（输入/输出 token 总量）
- Key 信息（密钥详情）
- Logs（个人请求日志）

---

### boom-main

HTTP 服务器，路由处理，状态管理，启动入口。

**启动流程（`main.rs`）：**
1. 解析 CLI 参数（`--config`, `--host`, `--port`, `--reboot`）
2. 加载 YAML 配置
3. `AppState::from_config()` 构建状态（连接 DB，初始化提供商，加载套餐）
4. 安装 SIGHUP 热重载监听器（Unix）
5. 构建后台任务（sync 10min, request summary 60s）
6. 构建 Axum Router 并启动 HTTP 服务
7. 优雅关停（Ctrl+C / SIGTERM → 广播 shutdown → 关闭 DB pool）

**状态管理（`state.rs`）：**
- `AppState` 包含热交换的 `inner` (ArcSwap) 和跨 reload 存活的 `db_pool` / `limiter` / `plan_store` / `deployment_store` / `alias_store` / `inflight`
- `AppStateInner` 包含 config, auth, health
- `reload()` 原子交换 inner，新请求立即看到新配置，进行中的请求不受影响
- `select_deployment()` 精确匹配 → 别名匹配 → `"*"` 通配兜底 → 调度策略选择

**认证提取器（`extractor.rs`）：**
- `RequiredAuth` 从请求头提取 API key
- 支持 `Authorization: Bearer xxx`、`x-api-key: xxx`（Anthropic 风格）、`api-key: xxx`（Azure 风格）

**路由处理（`routes.rs`）：**
- 模型访问校验：考虑部署、别名、通配符的完整匹配逻辑
- 限流检查：三级 fallback 调用 `check_plan_or_default_limits(key, weight)`
- 配额倍率：`resolve_quota_weight()` 先解析别名再查 deployment_store.get_quota_ratio()
- 错误响应：OpenAI 格式和 Anthropic 格式两种错误包装
- 非流式请求超时覆盖为 600s（防止 provider 默认超时过短）

---

## API 端点一览

### 客户端 API（需 API key）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/v1/chat/completions` | OpenAI 格式聊天（流式 / 非流式） |
| POST | `/chat/completions` | 同上（无 /v1 前缀兼容） |
| POST | `/v1/messages` | Anthropic Messages API（流式 / 非流式） |
| POST | `/v1/completions` | OpenAI 格式补全（流式 / 非流式） |
| POST | `/completions` | 同上（无 /v1 前缀兼容） |
| GET | `/v1/models` | 模型列表（按 key 权限过滤） |
| GET | `/v1/models/{id}` | 单个模型信息 |
| GET | `/models` | 同上（无 /v1 前缀兼容） |
| POST | `/v1/embeddings` | 返回不支持错误 |
| POST | `/v1/audio/speech` | 返回不支持错误 |
| POST | `/v1/audio/transcriptions` | 返回不支持错误 |
| POST | `/v1/moderations` | 返回不支持错误 |

### 健康检查（无需认证）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 完整健康状态 |
| GET | `/health/live` | 存活探针 |
| GET | `/health/ready` | 就绪探针（DB 状态） |

### 管理 API（需 master key）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/admin/config/reload` | 热重载配置 |
| PUT | `/admin/plans` | 创建 / 更新套餐 |
| GET | `/admin/plans` | 列出所有套餐 |
| DELETE | `/admin/plans/{name}` | 删除套餐 |
| POST | `/admin/plans/assign` | 分配 key 到套餐 |
| DELETE | `/admin/plans/assign/{key_hash}` | 取消 key 的套餐分配 |
| GET | `/admin/plans/assignments` | 列出所有 key-套餐分配 |

### Dashboard API（需 JWT 认证）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/dashboard/api/auth/login` | 登录（admin 或 user） |
| POST | `/dashboard/api/auth/logout` | 登出 |
| GET | `/dashboard/api/auth/me` | 当前会话信息 |
| GET | `/dashboard/api/user/plan` | 用户套餐信息 |
| GET | `/dashboard/api/user/usage` | 用户限流窗口使用量 |
| GET | `/dashboard/api/user/key-info` | 用户密钥详情 + token 统计 |
| GET | `/dashboard/api/user/logs` | 用户请求日志（分页） |
| GET | `/dashboard/api/admin/models` | 模型部署列表 |
| POST | `/dashboard/api/admin/models` | 创建模型部署 |
| PUT | `/dashboard/api/admin/models/{id}` | 更新模型部署 |
| DELETE | `/dashboard/api/admin/models/{id}` | 删除模型部署 |
| GET | `/dashboard/api/admin/aliases` | 别名列表 |
| POST | `/dashboard/api/admin/aliases` | 创建别名 |
| PUT | `/dashboard/api/admin/aliases/{name}` | 更新别名 |
| DELETE | `/dashboard/api/admin/aliases/{name}` | 删除别名 |
| GET | `/dashboard/api/admin/plans` | 套餐列表 |
| PUT | `/dashboard/api/admin/plans` | 创建/更新套餐 |
| DELETE | `/dashboard/api/admin/plans/{name}` | 删除套餐 |
| POST | `/dashboard/api/admin/plans/assign` | 分配 key 到套餐 |
| DELETE | `/dashboard/api/admin/plans/assign/{key_hash}` | 取消分配 |
| GET | `/dashboard/api/admin/plans/assignments` | 列出所有分配 |
| GET | `/dashboard/api/admin/keys` | 密钥列表（分页 + 搜索 + 全局使用量排序） |
| POST | `/dashboard/api/admin/keys` | 创建密钥 |
| PUT | `/dashboard/api/admin/keys/{hash}` | 更新密钥 |
| POST | `/dashboard/api/admin/keys/{hash}/block` | 封禁密钥 |
| POST | `/dashboard/api/admin/keys/{hash}/unblock` | 解封密钥 |
| POST | `/dashboard/api/admin/keys/batch` | 批量创建密钥 |
| GET | `/dashboard/api/admin/keys/{hash}/usage` | 密钥限流详情 |
| GET | `/dashboard/api/admin/assignments` | 分配列表 |
| GET | `/dashboard/api/admin/logs` | 请求日志（分页 + 列级过滤） |
| GET | `/dashboard/api/admin/teams` | 团队列表 + token 统计 |
| GET | `/dashboard/api/admin/stats/models` | 模型统计（请求数/成功率/token） |
| GET | `/dashboard/api/admin/stats/inflight` | 实时 in-flight 请求统计 |
| POST | `/dashboard/api/admin/limits/reset/{key_hash}` | 重置单 key 限流窗口 |
| POST | `/dashboard/api/admin/limits/reset` | 重置所有限流窗口 |
| GET | `/dashboard/api/admin/config` | KV 配置列表 |
| PATCH | `/dashboard/api/admin/config` | 更新 KV 配置项 |

---

## 请求处理流程

```
客户端请求
  │
  ├─ 提取 API key (RequiredAuth)
  │    └─ Authorization: Bearer / x-api-key / api-key
  │
  ├─ 认证 (DbAuthenticator.authenticate)
  │    ├─ Master key 恒定时间比较
  │    ├─ SHA-256 hash → moka 缓存 → DB 查询
  │    ├─ 特殊模型名解析 (all-team-models / all-proxy-models)
  │    └─ 校验: blocked / expired / budget
  │
  ├─ 模型访问校验 (check_model_access)
  │    ├─ 精确匹配 key.models
  │    ├─ 别名匹配 (alias ↔ target 双向)
  │    └─ 通配符 "*" 兜底
  │
  ├─ 配额倍率 (resolve_quota_weight)
  │    ├─ alias_store.resolve(model) → actual_model
  │    └─ deployment_store.get_quota_ratio(actual_model) → weight
  │
  ├─ 限流检查 (check_plan_or_default_limits, weight)
  │    ├─ 显式套餐 → effective_limits() (含时段调度)
  │    ├─ 默认套餐 → effective_limits()
  │    └─ 无套餐 → config 级 per-key per-model 滑动窗口
  │    └─ counter += weight (加权计数)
  │
  ├─ 选路 (select_deployment)
  │    ├─ 精确匹配 → 别名 → "*" 通配
  │    └─ RoundRobin / KeyAffinity 调度
  │
  └─ 路由到提供商
       ├─ OpenAI 格式 → 直接转发
       └─ Anthropic 格式 → 转换 → 转发 → 转换响应
```

---

## 错误类型

| GatewayError 变体 | HTTP 状态码 | error_type |
|---|---|---|
| `AuthError` | 401 | authentication_error |
| `KeyBlocked` | 403 | key_blocked |
| `ModelNotAllowed` | 403 | model_not_allowed |
| `BudgetExceeded` | 402 | budget_exceeded |
| `ModelNotFound` | 404 | model_not_found |
| `RateLimitExceeded` | 429 | rate_limit_exceeded |
| `ConcurrencyExceeded` | 429 | concurrency_exceeded |
| `UpstreamTimeout` | 504 | timeout |
| `ProviderError` | 502 | provider_error |
| `UpstreamError` | 502 | upstream_error |
| `ConfigError` | 500 | internal_error |
| `InternalError` | 500 | internal_error |
| `KeyExpired` | 401 | key_expired |
