<div align="center">
  <img src="misc/logo.svg" alt="BooMGateway" width="360">
  <br><br>
  <strong>高性能 LLM API 网关</strong>
</div>

**中文** | [English](README.en.md)

一个使用 Rust 构建的生产级 LLM API 网关。统一接入 OpenAI、Anthropic、Gemini、Bedrock、vLLM、Ollama 等 20+ 上游服务商。兼容 litellm 密钥体系，自建速率限制、套餐管理、流量控制、计费、Web 管理面板、请求审计与完整 prompt 落盘。

[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)

---

## 特性

- **多服务商路由** — OpenAI / Anthropic / Azure / Gemini / Bedrock / vLLM / Ollama 等
- **负载均衡** — 在同名 deployment 之间支持轮询、密钥亲和或 KV-cache 感知调度，并提供实时再平衡计数
- **速率限制** — 滑动窗口 + 并发控制 + 自定义时间窗口 + 定时套餐切换
- **套餐体系** — 灵活的密钥套餐与团队套餐（含 `member_plan` 继承），密钥/团队→套餐绑定，三级回退
- **配额与计费** — 按模型设置 `quota_count_ratio`，并通过 `cost_templates` 定价、按窗口的 token/费用上限
- **流量控制** — 按 deployment 限制最大在途请求数与最大上下文字符数，VIP 优先队列
- **自动熔断** — 连续请求失败检测，自动禁用异常 deployment（含兜底 `*`）
- **Deployment 健康探测** — 主动探测 `/metric`，支持自动下线/恢复（阈值可配）
- **KV-Cache 感知路由** — 通过 ZMQ 订阅 vLLM KV-cache 事件，把请求路由到缓存命中前缀最多的 worker，降低 TTFT
- **公开模型** — 通过 `public_models` 配置，让所有密钥都能访问指定模型，无需逐个更新白名单
- **Anthropic 原生** — 提供 `/v1/messages` 端点（兼容 Claude Code / opencode）
- **Prompt 落盘** — 完整请求/响应内容（含原始 SSE 分块）以滚动、gzip 压缩的 JSONL 写入磁盘；可在面板按团队/密钥动态开关
- **客户端统计** — 客户端类型分类（Anthropic 原生 vs 其他），面板提供 60 分钟细分
- **Web 管理面板** — 单页管理端：密钥、模型、别名、套餐、团队、配额、日志、实时在途 + 客户端统计
- **热加载** — 支持 SIGHUP、API 或面板按钮触发，通过 ArcSwap 实现零停机配置切换
- **请求审计** — 完整请求日志（token 数、耗时、状态），流式请求记录真实耗时
- **调试录制** — 可选的上游响应抓取，便于排错（受 `debug-tools` feature 控制）
- **容器化 LB** — `misc/LB` 前端以 Docker 镜像提供（Rust 多阶段构建，openEuler 运行时）

---

## 快速开始

### 前置条件

- Rust 1.75+（含 cargo）
- PostgreSQL 13+（用于密钥认证与持久化）

### 1. 构建

```bash
git clone https://github.com/your-org/BooMGateway.git
cd BooMGateway

# 构建 release 二进制
cargo build --release

# 二进制位于 target/release/boom-gateway
```

### 2. 准备数据库

创建一个空的 PostgreSQL 数据库。BooMGateway 会在启动时自动创建所需的所有表：

```bash
# 创建数据库（名字任意）
createdb boom_gateway

# 或通过 psql
psql -U postgres -c "CREATE DATABASE boom_gateway;"
```

就这些 —— 无需手动建表。网关会自动创建 11 张 `boom_*` 表：

| 表 | 归属模块 | 用途 |
|-------|-------|---------|
| `boom_request_log` | boom-audit | 请求日志（含 token 数、耗时、状态） |
| `boom_model_deployment` | boom-routing | 模型 deployment 配置 |
| `boom_model_alias` | boom-routing | 模型别名映射 |
| `boom_rate_limit_state` | boom-limiter | 速率限制窗口计数 |
| `boom_rate_limit_cumulative` | boom-limiter | 永久累计的 token/费用总量 |
| `boom_key_plan_assignment` | boom-limiter | 密钥→套餐绑定 |
| `boom_team_plan_assignment` | boom-limiter | 团队→套餐绑定 |
| `boom_rate_limit_plan` | boom-limiter | 套餐定义 |
| `boom_config` | boom-dashboard | 通用 KV 配置存储 |
| `boom_team_table` | boom-dashboard | 团队定义（含模型访问权限） |
| `boom_verification_token` | boom-dashboard / boom-auth | API 密钥/token 记录（兼容 litellm schema） |

### 3. 配置

创建 `config.yaml`（仓库自带 `config.example.yaml`，可拷贝作为起点）：

```bash
cp config.example.yaml config.yaml
```

```yaml
# 模型 deployment
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_key: os.environ/OPENAI_API_KEY

  - model_name: claude-sonnet
    litellm_params:
      model: anthropic/claude-sonnet-4-20250514
      api_key: os.environ/ANTHROPIC_API_KEY

  - model_name: deepseek-chat
    litellm_params:
      model: openai/deepseek-chat
      api_base: https://api.deepseek.com/v1
      api_key: os.environ/DEEPSEEK_API_KEY

  # 兜底路由：匹配不到任何 model_name 的请求路由到这里
  - model_name: "*"
    litellm_params:
      model: openai/gpt-4o-mini
      api_key: os.environ/OPENAI_API_KEY

  # 带流量控制 —— 保护昂贵的后端免受突发流量冲击
  # - model_name: claude-opus
  #   model_info:
  #     id: opus-node-1               # 流量控制槽位的 deployment_id
  #   flow_control:
  #     model_queue_limit: 20          # 最大并发在途请求数
  #     model_context_limit: 2000000   # 在途请求的最大输入字符总和
  #   litellm_params:
  #     model: anthropic/claude-opus-4-20250514
  #     api_key: os.environ/ANTHROPIC_API_KEY

# 通用设置
general_settings:
  master_key: os.environ/MASTER_KEY
  database_url: os.environ/DATABASE_URL
  # 所有密钥均可访问的模型（不受各密钥白名单限制）
  public_models:
    - deepseek-chat

# 服务
server:
  host: 0.0.0.0
  port: 4000
  workers: 4

# 速率限制
rate_limit:
  enabled: true
  default_rpm: 60

# 套餐（密钥套餐与团队套餐）
plan_settings:
  default_plan: "basic"
  default_team_plan: "team_basic"
  plans:
    basic:                                # 密钥套餐
      concurrency_limit: 4
      rpm_limit: 60
      window_limits:
        - [100, 18000]
    pro:                                  # 密钥套餐
      concurrency_limit: 10
      rpm_limit: 120
      window_limits:
        - [500, 18000]
    team_basic:                           # 团队套餐，成员继承 "basic"
      type: team
      member_plan: basic

# 完整请求/响应落盘（默认关闭）
prompt_log:
  enabled: false
  dir: "/data/prompt_logs"
  max_file_size_mb: 50
  capture_raw_upstream: false             # 同时记录格式转换前的上游响应（如 /v1/messages）
  excluded_keys: []
  excluded_teams: []

# Deployment 主动健康探测
deployment_health_check:
  auto_offline_enabled: true
  auto_recovery_enabled: true
  path: /metric
  failure_threshold: 3
  recovery_threshold: 2
```

完整字段说明见 [CONFIG_EXAMPLE.md](CONFIG_EXAMPLE.md)（另含 `router_settings`、`cost_templates`、KV-cache 感知路由等）。

### 4. 运行

```bash
# 设置环境变量
export MASTER_KEY="sk-your-master-key"
export DATABASE_URL="postgres://user:pass@localhost:5432/boom_gateway"
export OPENAI_API_KEY="sk-..."
export ANTHROPIC_API_KEY="sk-ant-..."

# 运行
cargo run --release -p boom-main

# 或直接运行构建好的二进制
./target/release/boom-gateway
```

CLI 参数（覆盖 `config.yaml`）：

```bash
boom-gateway --config config.yaml --host 0.0.0.0 --port 4000
boom-gateway --reboot      # 先优雅停止运行中的实例，再启动
```

### 5. 创建密钥并开始使用

```bash
# 通过面板 API 创建一个 API 密钥
curl -X POST http://localhost:4000/dashboard/api/admin/keys \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_alias": "alice", "plan_name": "pro"}'

# 响应：{"key": "sk-...", "token_hash": "...", "key_name": null}
# 请立即保存该密钥 —— 它只会显示一次！

# 使用它
curl http://localhost:4000/v1/chat/completions \
  -H "Authorization: Bearer sk-your-new-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

### 6. 管理面板

在浏览器打开 `http://localhost:4000/dashboard`，用 master key 以 `admin` 身份登录（或用 API key 以普通用户身份登录）。

---

## 项目结构

```
BooMGateway/
├── boom-gateway/           Rust workspace 根目录（13 个 crate）
│   ├── boom-core/          核心 trait 与公共类型
│   ├── boom-auth/          密钥认证（SHA-256 + DB + master key）
│   ├── boom-config/        YAML 配置解析（含环境变量展开）
│   ├── boom-provider/      LLM Provider 实现
│   ├── boom-limiter/       滑动窗口限流 + 并发控制 + PlanStore
│   ├── boom-flowcontrol/   按 deployment 的流量控制（含 VIP 优先）
│   ├── boom-routing/       DeploymentStore + AliasStore + 调度策略
│   ├── boom-kvindex/       KV-cache 前缀索引 + ZMQ 订阅 + 分词
│   ├── boom-ctxaware/      客户端类型分类 + 客户端统计
│   ├── boom-promptlog/     完整请求/响应落盘（JSONL）
│   ├── boom-audit/         请求日志读写
│   ├── boom-dashboard/     Web UI + REST API + JWT 认证
│   └── boom-main/          入口、路由、状态组装（二进制名：boom-gateway）
├── misc/LB/                Pingora 负载均衡（可选前端，已容器化）
├── misc/logo.svg           项目 logo
├── config.example.yaml     完整配置参考
├── ARCH.md                 架构设计文档
└── CLAUDE.md               开发指南
```

## 技术栈

| 组件 | 技术 |
|-----------|-----------|
| 语言 | Rust（edition 2021） |
| HTTP 框架 | Axum |
| 异步运行时 | Tokio（多线程） |
| 数据库 | PostgreSQL（sqlx，自动迁移） |
| 并发原语 | DashMap、ArcSwap |
| KV 事件 | ZMQ（PUB/SUB）、MessagePack |
| 计费 | rust_decimal |
| CLI | clap |
| 认证 | SHA-256 token 哈希、JWT 会话 |

## API 端点

### 客户端 API（需 API key）

| 端点 | 说明 |
|----------|-------------|
| `POST /v1/chat/completions` | OpenAI 对话（流式 / 非流式） |
| `POST /v1/messages` | Anthropic Messages API |
| `POST /v1/completions` | OpenAI completions |
| `POST /v1/embeddings` | 返回"不支持" |
| `GET /v1/models` | 当前密钥可见的模型（deployment + 别名） |
| `GET /v1/models/{id}` | 单个模型信息 |

> 同时提供 OpenAI 客户端兼容别名（不带 `/v1` 前缀）：`/chat/completions`、`/completions`、`/models`。

### 管理 API（需 master key）

| 端点 | 说明 |
|----------|-------------|
| `POST /admin/config/reload` | 热加载 config.yaml |
| `PUT /admin/plans` · `GET /admin/plans` · `DELETE /admin/plans/{name}` | 套餐 CRUD |
| `POST /admin/plans/assign` · `DELETE /admin/plans/assign/{key_hash}` · `GET /admin/plans/assignments` | 密钥→套餐绑定 |

### 面板 — 认证

| 端点 | 说明 |
|----------|-------------|
| `POST /dashboard/api/auth/login` | 登录（admin 用 master key，用户用 API key） |
| `POST /dashboard/api/auth/logout` | 清除会话 |
| `GET /dashboard/api/auth/me` | 当前会话信息 |

### 面板 — 用户 API（会话认证，仅限调用者自己的密钥）

| 端点 | 说明 |
|----------|-------------|
| `GET /dashboard/api/user/plan` | 当前套餐 + 剩余配额 |
| `GET /dashboard/api/user/usage` | token / 费用 / 请求数用量 |
| `GET /dashboard/api/user/key-info` | 密钥元信息（别名、团队、模型、RPM） |
| `GET /dashboard/api/user/logs` | 自己的请求日志（分页） |
| `GET /dashboard/api/user/request-status` | 在途请求的实时状态 |

### 面板 — 管理 API（admin 会话）

**密钥**
| 端点 | 说明 |
|----------|-------------|
| `GET /dashboard/api/admin/keys` | 列出密钥 |
| `POST /dashboard/api/admin/keys` · `/keys/batch` | 创建 / 批量创建密钥 |
| `PUT /dashboard/api/admin/keys/{token_hash}` | 更新密钥 |
| `POST /dashboard/api/admin/keys/{token_hash}/block` · `/unblock` | 封禁 / 解封密钥 |

**模型与别名**
| 端点 | 说明 |
|----------|-------------|
| `/dashboard/api/admin/models` | 模型 deployment CRUD |
| `/dashboard/api/admin/aliases` | 模型别名 CRUD |

**套餐、绑定与团队**
| 端点 | 说明 |
|----------|-------------|
| `/dashboard/api/admin/plans` | 套餐 CRUD |
| `POST /dashboard/api/admin/assignments` · `DELETE` · `GET` | 密钥→套餐绑定 |
| `POST /dashboard/api/admin/team-assignments` · `DELETE` | 团队→套餐绑定 |
| `/dashboard/api/admin/teams` | 团队 CRUD（含模型访问控制） |

**配额**
| 端点 | 说明 |
|----------|-------------|
| `GET /dashboard/api/admin/quota/overview` | 按团队的配额总览 |
| `GET /dashboard/api/admin/quota/team/{id}` · `/unassigned` · `/key/{hash}/windows` | 配额明细 |
| `POST /dashboard/api/admin/quota/reset/...` | 重置累计 / 窗口配额（按密钥或团队） |

**日志与统计**
| 端点 | 说明 |
|----------|-------------|
| `GET /dashboard/api/admin/logs` | 请求日志（支持列过滤） |
| `GET /dashboard/api/admin/usage/{key_hash}` | 单密钥用量 |
| `GET /dashboard/api/admin/stats/inflight` | 实时在途 + 流量控制 |
| `GET /dashboard/api/admin/stats/agents` | 客户端类型分布（Anthropic / 其他） |
| `GET /dashboard/api/admin/stats/deployments/summary` | 各 deployment 24 小时汇总 |
| `GET /dashboard/api/admin/stats/request_rate` | 各 deployment 请求速率 |
| `GET /dashboard/api/admin/stats/rebalance-moves` | 负载均衡迁移次数 |

**Prompt 日志**
| 端点 | 说明 |
|----------|-------------|
| `GET /dashboard/api/admin/prompt-log/status` | 录制状态 |
| `POST /dashboard/api/admin/prompt-log/toggle` · `/team` · `/key` | 开关录制（全局 / 团队 / 密钥） |
| `GET /dashboard/api/admin/prompt-log/entry/{request_id}` | 读取单条记录 |

**速率限制与配置**
| 端点 | 说明 |
|----------|-------------|
| `POST /dashboard/api/admin/limits/reset/{key_hash}` · `/reset` | 重置速率限制窗口 |
| `POST /dashboard/api/admin/config/reload` | 从面板 UI 热加载 |

**调试**（需开启 `debug-tools` feature）
| 端点 | 说明 |
|----------|-------------|
| `GET /dashboard/api/admin/debug/status` · `POST /toggle` | 调试录制开关 |
| `GET /dashboard/api/admin/debug/errors/{request_id}` | 读取录制的错误 |

### 健康检查（无需认证）

| 端点 | 说明 |
|----------|-------------|
| `GET /health` | 完整健康状态 |
| `GET /health/live` | 存活探针 |
| `GET /health/ready` | 就绪探针 |

### 内部接口（无需认证）

| 端点 | 说明 |
|----------|-------------|
| `GET /internal/kv-index` | KV-cache 前缀索引状态与 Trie 内容 |

---

## 流量控制

按 deployment 的流量控制可保护后端免受突发流量冲击。配置后，网关会把超过并发或上下文上限的请求排队，**VIP 密钥享有优先调度**。

### 工作机制

```
请求到达
  │
  ├─ 检查 max_inflight ──── 超限? ──→ 入队（VIP 优先）
  │                                       │
  ├─ 检查 max_context ──── 超限? ──→ 立即拒绝
  │                                       │
  └─ 获取 guard（RAII） ──→ 流结束 / 响应 drop 时释放
```

### 配置

为任意模型 deployment 添加 `flow_control`（需要 `model_info.id`）：

```yaml
model_list:
  - model_name: claude-opus
    model_info:
      id: opus-node-1
    flow_control:
      model_queue_limit: 20          # 到该后端的最大并发请求数
      model_context_limit: 2000000   # 所有在途请求的最大输入字符总和
    litellm_params:
      model: anthropic/claude-opus-4-20250514
      api_key: os.environ/ANTHROPIC_API_KEY
```

也可以通过面板（模型编辑对话框）按 deployment 配置。

### VIP 优先队列

元数据中带 `"vip": true` 的密钥会跳过普通队列 —— 它们的请求总是先于非 VIP 等待者被调度：

```bash
# 创建一个 VIP 密钥
curl -X POST http://localhost:4000/dashboard/api/admin/keys \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_alias": "vip-user", "metadata": {"vip": true}}'
```

当某个 deployment 槽位释放时，调度器会先贪婪地从 VIP 队列补满容量，再处理普通队列。这保证了在高负载下，高级用户也能获得最低延迟。

### 关键行为

| 场景 | 行为 |
|----------|----------|
| 达到 `max_inflight` | 请求入队（VIP 优先），1200 秒后超时 |
| 超过 `max_context` | 立即拒绝（单个请求过大） |
| 请求完成 | guard 释放，槽位归还，调度下一个等待者 |
| 客户端断开 | guard 释放，自动归还槽位 |

### 监控

面板的「在途」页可实时查看流量控制统计，包括：
- 各 deployment 当前的在途数与上下文用量
- 排队等待者数量（VIP vs 普通）
- 单个等待者详情（密钥别名、已等待时长）

---

## KV-Cache 感知路由

把请求路由到已持有最相关 KV-cache 前缀的 vLLM worker，减少重复计算与 TTFT。网关订阅 vLLM 的 ZMQ KV-cache 事件，构建 token 前缀 Trie 索引，并用它匹配到达的请求。

### 工作机制

```
vLLM worker 通过 ZMQ 发布 KV 事件
  │
  ▼
网关订阅并构建按模型的 Token 前缀 Trie
  │
  ▼
请求到达 → 分词 → 遍历 Trie → 为候选打分 → 选择最优 worker
```

**打分**：`combined_score = cache_weight × hit_ratio + tier_weight × tier_score + load_weight × load_score`

无匹配 → 回退到最低负载选择。

### 配置

```yaml
router_settings:
  schedule_policy: kvc_aware

  kvc_aware:
    block_size: 128                 # 必须与 vLLM 的 block_size 一致
    cache_weight: 0.5               # KV 前缀命中权重
    tier_weight: 0.3                # 存储层级权重
    load_weight: 0.2                # 负载权重
    tokenizer_dir: /data/tokenizers # 按模型的分词器文件
    zmq_endpoints:
      - "tcp://10.0.0.1:5557"
      - "tcp://10.0.0.2:5557"
    zmq_topic_prefix: "kv@"
```

每个模型需要在 `{tokenizer_dir}/{model_name}/` 下放置分词器文件：
- `tokenizer.json` — HuggingFace 分词器
- `tokenizer_config.json` — chat_template、特殊 token
- `chat_template.jinja` — 可选的独立模板文件

### 标识对齐

网关与 vLLM 的标识必须一致：

| 维度 | 网关 | vLLM | 必须一致 |
|-----------|---------|------|-----------|
| 模型名 | YAML 中的 `model_name` | `--served-model-name` | 是 |
| Worker ID | YAML 中的 `model_info.id` | ZMQ topic 的 worker_id | 是 |
| Block 大小 | `kvc_aware.block_size` | `--block-size` | 是 |

```yaml
- model_name: MiniMax-M2.7
  litellm_params:
    model: hosted_vllm/MiniMax-M2.7
    api_base: http://10.0.0.1:8000
  model_info:
    id: "10.0.0.1"                   # 与 ZMQ topic 的 worker_id 一致
```

### 监控

`GET /internal/kv-index` — 查看 Trie 内容、block 数量与 worker 分配。

完整设计文档见 [docs/kvc-aware-design.md](docs/kvc-aware-design.md)。

---

## Prompt 落盘

`boom-promptlog` 抓取每个请求的完整请求与响应（流式请求含原始 SSE 分块），以 JSONL 写入磁盘 —— 用于调试与审计。

- 目录结构 `{dir}/{team_alias}/{key_hash}/log_NNNNNN.jsonl`，后台滚动并 gzip 压缩。
- 流式响应通过流包装器累积原始事件分块录制（不做字段级解析）。
- 可选 `capture_raw_upstream` 在格式转换路径（如 `/v1/messages`）上额外存储转换前的上游响应。
- 可在面板按全局、团队或密钥开关录制 —— 通过热加载即时生效。

```yaml
prompt_log:
  enabled: false
  dir: "/data/prompt_logs"
  max_file_size_mb: 50
  capture_raw_upstream: false
  excluded_keys: []
  excluded_teams: []
```

---

## 热加载

三种触发方式，均通过 ArcSwap 原子交换实现零停机：

1. **信号**：`kill -HUP <pid>`
2. **API**：`POST /admin/config/reload`（需 master key）
3. **面板**：点击「Reload Config」按钮

运行时计数器（限流、并发、绑定）在热加载后保留。

---

## 负载均衡（misc/LB）

基于 Pingora 的独立负载均衡器，可作为可选前端。它以 Docker 镜像提供（Rust 多阶段构建，openEuler 运行时）：

```bash
./misc/LB/start.sh start              # 自动构建 + 生成证书
./misc/LB/start.sh start routes.yaml  # 自定义配置
./misc/LB/start.sh status | stop | restart
```

按 `host`（支持通配符 `*.example.com`）、`path` 前缀、`client_ip` CIDR 进行路由。

---

## 文档

| 文档 | 说明 |
|----------|-------------|
| [ARCH.md](ARCH.md) | 架构：模块图、请求流程、状态管理、DB schema |
| [DESCRIPTOR.md](DESCRIPTOR.md) | 详细架构说明 |
| [CONFIG_EXAMPLE.md](CONFIG_EXAMPLE.md) | 完整配置字段参考 |
| [docs/kvc-aware-design.md](docs/kvc-aware-design.md) | KVC 感知路由设计文档 |
| [docs/kvc-aware-event-reporting-research.md](docs/kvc-aware-event-reporting-research.md) | KV-cache 事件上报机制调研（NVIDIA Dynamo、llm-d） |
| [docs/vip-header-forwarding-design.md](docs/vip-header-forwarding-design.md) | VIP 请求信息透传（`X-Gateway-Priority` header） |
| [CLAUDE.md](CLAUDE.md) | 开发指南与架构原则 |

## 许可证

Apache License 2.0
