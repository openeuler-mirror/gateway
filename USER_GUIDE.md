# BooMGateway 用户操作指导手册

> 本手册面向 BooMGateway 的部署与运维人员，描述网关对外暴露的功能特性、配置项含义、推荐配置实践、配置注意事项以及常见运维操作。
>
> 配置文件为 YAML 格式，兼容 litellm 的 `proxy_server_config.yaml`。所有顶级字段均可选，未配置时使用合理默认值。

---

## 0. 配置权威模型（先读）

网关配置分两类，权威归属不同：

| 类别 | 内容 | 权威 | 修改方式 |
|------|------|------|----------|
| **业务配置** | 模型部署（`model_list`）、模型别名、套餐（`plan_settings`）、默认套餐指针、客户端拦截规则（`client_blocklist`）、Workflow（`workflow_settings`）、计费模板（`cost_templates`） | **DB**（唯一运行时权威） | Dashboard CRUD——写 DB + 直改内存，**写完即生效**，无需 reload |
| **进程级配置** | `server`、`general_settings`（master_key / database_url）、router 调度参数（`schedule_policy` / `kvc_aware` / `auto_router` 等）、`deployment_health_check`、`prompt_log`、`trace`、`hooks` | **YAML**（+ 热加载） | 改 YAML → SIGHUP / reload API / 面板按钮 |

**YAML 中业务 section 的语义 = 首次 seed + 回滚工件**：

- 只在「业务表全空且无 seed 标记」的全新库首次启动时导入（一次性）；此后完全 inert——手改 YAML + reload **不会生效**，Dashboard 删除的条目也不会被重启复活。
- 灾备恢复：空库 + 回写按钮生成的 YAML 快照重启即可恢复全部配置。
- 导出 / 回写按钮在 Dashboard Config 页；生成的文件含明文 secrets（0600），视同备份工件。

**无 YAML 启动（env bootstrap）**：不带 `--config` 且 cwd 无 `config.yaml` 时，设 `BOOM_MASTER_KEY` + `BOOM_DATABASE_URL` 两个环境变量即可启动（业务配置全部来自 DB）；可选 `BOOM_DASHBOARD_SESSION_SECRET` 固定 Dashboard 会话签名密钥。回写按钮可在 cwd 生成完整 `config.yaml`，重启即切换为文件模式。

**无 DB 回退**：未配 `database_url` 时（master-key-only 模式），YAML 业务 section 恢复构建路径——这是部署形态选择（单机 / 无 DB 环境），不是第二权威。

---

## 1. 功能特性模块说明

本节只描述用户/客户端能直接感知到的能力，内部实现细节不展开。

### 1.1 多 Provider 统一入口

- 一个端点同时承接 OpenAI、Anthropic、Azure、Gemini、Bedrock、vLLM、Ollama、DeepSeek、Groq、Together、Fireworks、DeepInfra、SambaNova、Cerebras、NVIDIA NIM、火山引擎、阿里灵积、Moonshot、xAI、AI21 等 20+ 上游。
- 客户端协议兼容：
  - `POST /v1/chat/completions`、`POST /v1/completions` —— OpenAI 协议
  - `POST /v1/messages` —— Anthropic 原生协议（Claude Code、opencode 直连可用）
  - `GET /v1/models`、`GET /v1/models/{id}` —— 模型列表
- 不带前缀的 `model` 字段会按名字自动识别 provider（`gpt-*`/`o1-*`/`o3-*`/`o4-*` → openai，`claude-*` → anthropic，`gemini-*`/`gemma-*` → gemini，`anthropic.*`/`amazon.*` 等 → bedrock）。

```yaml
model_list:
  # OpenAI — 显式前缀
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_key: ${OPENAI_API_KEY}

  # Anthropic — 原生 Messages API
  - model_name: claude-sonnet
    litellm_params:
      model: anthropic/claude-sonnet-4-20250514
      api_key: ${ANTHROPIC_API_KEY}

  # Azure OpenAI
  - model_name: azure-gpt4
    litellm_params:
      model: azure/my-gpt4-deployment
      api_base: https://my-resource.openai.azure.com
      api_key: ${AZURE_API_KEY}
      api_version: "2024-06-01"

  # AWS Bedrock
  - model_name: bedrock-claude
    litellm_params:
      model: bedrock/anthropic.claude-3-sonnet
      aws_region_name: us-east-1
      aws_access_key_id: ${AWS_ACCESS_KEY_ID}
      aws_secret_access_key: ${AWS_SECRET_ACCESS_KEY}

  # vLLM 自部署（OpenAI 兼容，api_key 可省略）
  - model_name: my-llama
    litellm_params:
      model: hosted_vllm/my-model
      api_base: http://10.0.0.1:8000/v1

  # 无前缀自动识别 — gpt- 开头识别为 openai
  - model_name: gpt-4o-mini
    litellm_params:
      model: gpt-4o-mini
      api_key: ${OPENAI_API_KEY}
```

### 1.2 负载均衡与调度策略

同名 `model_name` 配置多个 deployment 时，网关在它们之间自动负载均衡。调度策略由 `router_settings.schedule_policy` 决定：

| 策略 | 行为 |
|------|------|
| `round_robin`（默认） | 轮询，请求均匀分布到所有同名 deployment |
| `key_affinity` | 会话亲和：同一 API key 的请求尽量固定到同一 deployment，命中本地 KV-cache；当偏好节点利用率比最低负载节点高出 `rebalance_threshold` 个百分点时迁移到最低负载节点；上下文小于 `key_affinity_context_threshold` 字符时跳过亲和、走最低负载（冷启动预热） |
| `kvc_aware` | 前缀缓存亲和：相同会话前缀（system+tools+messages）的请求路由到已学到该前缀的 worker，命中上游 KV-cache 降低 TTFT。网关侧自学习 trie（512B 块、LRU + TTL 老化），不依赖 vLLM 事件订阅、无需上游改造；冷请求由轮询摊开，过载干预由 `rebalance_threshold` 控制 |
| `auto_router`（配合项） | 语义路由：客户端请求一个虚拟模型名（如 `model: "auto"`），网关按请求内容（推理链 / 代码 / 工具调用信号）分级 small / medium / large，路由到各档 `target_model`。内置启发式分类，也可外接 ML 分类服务，失败自动回落本地 |

```yaml
# 同名多 deployment — 自动负载均衡
model_list:
  - model_name: gpt-4o
    model_info:
      id: gpt4o-node-a
    litellm_params:
      model: openai/gpt-4o
      api_base: http://10.0.0.1:8000/v1
  - model_name: gpt-4o        # 同名 → 自动负载均衡
    model_info:
      id: gpt4o-node-b
    litellm_params:
      model: openai/gpt-4o
      api_base: http://10.0.0.2:8000/v1

# 调度策略
router_settings:
  # 默认轮询
  schedule_policy: round_robin

  # 或：会话亲和（自部署推理场景）
  # schedule_policy: key_affinity
  # key_affinity_context_threshold: 2000       # 低于 2000 字符先走最低负载预热
  # rebalance_threshold: 20                    # 偏好节点利用率高出 20% 时迁移

  # 或：前缀缓存亲和（vLLM 自部署场景，推荐）
  # schedule_policy: kvc_aware
  # kvc_aware:
  #   max_blocks: 500000                       # trie 容量上限（块数），超出 LRU 淘汰
  #   router_ttl_secs: 1200                    # 自学习块 TTL（秒），0 = 禁用后台扫描
```

### 1.3 模型别名

`router_settings.model_group_alias` 支持把别名映射到真实 `model_name`：

- 简单格式：`"gpt-4": "gpt-4o"`
- 扩展格式（可设 `hidden: true`，使其不在 `/v1/models` 列表中暴露）：`"GPT-4o": { model: "gpt-4o", hidden: true }`

```yaml
router_settings:
  model_group_alias:
    # 简单格式 — 客户端请求 "gpt-4" 实际路由到 "gpt-4o"
    "gpt-4": "gpt-4o"

    # 扩展格式 — hidden: true 后不在 /v1/models 列表中暴露
    "GPT-4o":
      model: "gpt-4o"
      hidden: true

    "claude": "claude-sonnet"
```

### 1.4 兜底路由（通配 `*`）

把 `model_name` 配为 `"*"` 的 deployment 会承接所有匹配不到任何已配置 `model_name` 的请求。也可在某个普通 deployment 上设 `serve_not_match: true`，让该 deployment 同时承担兜底职责（无需再单独写一条 `"*"`）。

> 注意：`"*"` 在本网关里是一个真实的 `model_name`（兜底路由），**不是** litellm 的"全权限通配符"。判断"全权限"的唯一依据是 key 的 `models` 数组为空或包含 `"all-team-models"`。

```yaml
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_key: ${OPENAI_API_KEY}

  # 方式 1：显式 "*" 兜底，承接未匹配的模型名
  - model_name: "*"
    litellm_params:
      model: openai/gpt-4o-mini
      api_key: ${OPENAI_API_KEY}

  # 方式 2：在普通 deployment 上设 serve_not_match: true
  # 该 deployment 同时承担兜底职责，无需单独再写一条 "*"
  # - model_name: deepseek-chat
  #   serve_not_match: true
  #   litellm_params:
  #     model: openai/deepseek-chat
  #     api_base: https://api.deepseek.com/v1
  #     api_key: ${DEEPSEEK_API_KEY}
```

### 1.5 速率限制与套餐（Plan）

- 限流维度：每 key 滑动窗口 RPM + 自定义时间窗口 + 并发数。
- 套餐系统：`plan_settings.plans` 定义多档套餐，每个套餐可设 `concurrency_limit`、`rpm_limit`、`window_limits`；通过 Dashboard 或 `POST /admin/plans/assign` 给 key 分配套餐。
- 三级回退：key 显式分配的 plan > key 自身 `rpm_limit`/`tpm_limit` > `default_plan` > `rate_limit.default_rpm`。
- 时段调度：套餐可带 `schedule`，按 `"H:MM-H:MM"` 分时段覆盖限流参数，支持跨午夜（如 `"21:00-9:00"`）。适合"白天高峰严控、夜间宽松"的策略。
- 配额倍率：`model_info.quota_count_ratio` 控制每次请求消耗的配额单位数。设为 3 表示一次请求消耗 3 个配额。昂贵模型设高倍率，廉价模型保持默认 1。

```yaml
# 全局限流兜底
rate_limit:
  enabled: true
  default_rpm: 60                       # 无套餐 key 的默认 RPM
  window_limits:                        # 全局自定义窗口
    - [100, 18000]                      # 100 次 / 5 小时

# 套餐定义
plan_settings:
  default_plan: basic                   # 未分配套餐 key 的默认套餐
  plans:
    basic:
      concurrency_limit: 4
      rpm_limit: 60
      window_limits:
        - [100, 18000]                  # 100 次 / 5 小时
    pro:
      concurrency_limit: 10
      rpm_limit: 120
      window_limits:
        - [500, 18000]
    enterprise:
      concurrency_limit: 50
      rpm_limit: 600
      window_limits:
        - [2000, 18000]
        - [500, 3600]                   # 500 次 / 1 小时

    # 时段调度套餐 — 白天严控、夜间放宽（跨午夜）
    scheduled-plan:
      concurrency_limit: 4
      rpm_limit: 40
      window_limits:
        - [80, 18000]
      schedule:
        - hours: "9:00-21:00"           # 高峰期
          concurrency_limit: 4
          rpm_limit: 40
          window_limits:
            - [80, 18000]
        - hours: "21:00-9:00"           # 低峰期（跨午夜）
          concurrency_limit: 8
          rpm_limit: 80
          window_limits:
            - [160, 18000]

# 配额倍率 — 昂贵模型一次请求消耗多倍配额
model_list:
  - model_name: claude-opus
    model_info:
      id: opus-node-1
      quota_count_ratio: 3              # 1 次请求消耗 3 个配额
    litellm_params:
      model: anthropic/claude-opus-4-20250514
      api_key: ${ANTHROPIC_API_KEY}
```

### 1.6 流控（Flow Control）

针对单个 deployment 的过载保护，配置在 `model_list[].flow_control`：

- `model_queue_limit`：最大在途请求数，超出则排队等待。
- `model_context_limit`：所有在途请求的输入字符总量上限，超出立即拒绝（单个请求就超限的情况）。
- VIP 优先队列：key 的 `metadata.vip = true` 时，其请求在排队时优先调度，槽位空出时先填 VIP 队列再填普通队列。
- 客户端断开 / 流结束 → 自动释放槽位 → 触发下一个等待者调度。
- 排队等待超时 1200s。

Dashboard 的 In-Flight 面板可实时查看每个 deployment 的在途数、上下文占用、排队人数（VIP / 普通分开）、单个等待者的 key alias 与等待时长。

```yaml
model_list:
  - model_name: claude-opus
    model_info:
      id: opus-node-1                   # 流控依赖 id 作为槽位标识，必须配
    flow_control:
      model_queue_limit: 20             # 最大在途请求数（0/不设 = 无限制）
      model_context_limit: 2000000      # 在途输入字符总量上限
    litellm_params:
      model: anthropic/claude-opus-4-20250514
      api_key: ${ANTHROPIC_API_KEY}
```

VIP key 通过 Dashboard 创建（`metadata.vip = true`），流控排队时优先调度：

```bash
curl -X POST http://localhost:4000/dashboard/api/admin/keys \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_alias": "vip-user", "metadata": {"vip": true}}'
```

### 1.7 部署健康检查与自动隔离

`deployment_health_check` 配置自动探活与故障隔离：

- 在线节点周期探活：连续失败达 `failure_threshold` 次 → 自动离线。
- 离线节点周期探活：连续成功达 `recovery_threshold` 次 → 自动上线。
- 请求失败自动离线：仅统计确定性失败（ProviderError / 401 / 403），连续达 `request_failure_threshold` 次自动隔离。
- 探活路径默认 `/metric`，按 `GET {api_base}{path}` 拼接。
- 兜底 deployment `"*"` 同样受自动隔离保护。

```yaml
deployment_health_check:
  auto_offline_enabled: true            # 在线节点探活失败自动离线
  auto_recovery_enabled: true           # 离线节点探活成功自动上线
  path: /metric                         # 探活路径：GET {api_base}{path}
  failure_threshold: 3                  # 连续失败 3 次后离线
  recovery_threshold: 2                 # 连续成功 2 次后上线
  offline_check_interval_secs: 30       # 在线节点探活间隔
  recovery_check_interval_secs: 60      # 离线节点探活间隔
  request_failure_auto_offline_enabled: true   # 请求失败也自动离线
  request_failure_threshold: 3                  # 连续请求失败 3 次后离线
```

### 1.8 Public Models（公共模型）

`general_settings.public_models` 列出的模型对所有 key 开放，**绕过 key/team 的模型白名单校验**。新增"全员可用"的模型时改这里，不必逐个 key 更新白名单。

```yaml
general_settings:
  master_key: ${MASTER_KEY}
  database_url: ${DATABASE_URL}
  public_models:                        # 列出的模型对所有 key 开放，绕过白名单
    - deepseek-chat
    - gpt-4o-mini
```

### 1.9 Prompt 日志（Prompt Log）

`prompt_log` 配置可对请求 / 响应内容落盘，用于合规审计与问题复盘：

- 按 `{dir}/{team_alias}/{key_hash}/log_NNNNNN.jsonl` 组织，单文件超过 `max_file_size_mb` 自动滚动并后台 gzip 压缩。
- `excluded_keys` / `excluded_teams` 排除特定 key 或团队。
- `capture_raw_upstream` 同时记录格式转换前的原始上游响应（仅 `/v1/messages` 等做了协议转换的端点有意义）。
- 默认关闭；热加载可在线开关。

```yaml
prompt_log:
  enabled: true                         # 总开关，默认 false
  dir: /data/prompt_logs                # 日志根目录，默认 /data/prompt_logs
  max_file_size_mb: 50                  # 单文件上限，超出滚动并 gzip
  capture_raw_upstream: false           # 同时记录协议转换前的上游原始响应
  excluded_keys:                        # 排除的 key hash（SHA-256）
    - "abc123..."
  excluded_teams:                       # 排除的 team_id
    - internal-ops
```

### 1.10 请求审计与 Debug 录制

- `boom_request_log` 表记录每次请求的 token 数、duration、状态。流式请求由 `LoggedStream` 包装，在 Drop 时写入真实 duration，避免在流开始时误记。
- Dashboard Debug 页可一键开启"错误录制"：捕获上游返回错误时的响应体，按 `request_id` 查询，便于定位上游异常。该开关只驱动内存中的错误录制，不影响 prompt-log 配置；`capture_raw_upstream` 由 prompt-log 配置独立控制。

### 1.11 Web Dashboard

`http://<host>:<port>/dashboard` 提供单页管理面板，使用 master key 登录（admin 视角，普通 key 登录看自助页）：

- Stats：请求量 / 成功率 / token 统计图表、实时在途与流控排队（In-Flight）、Agent 类型分布
- Models：deployment CRUD、流控参数编辑
- Workflows：workflow 定义与模型绑定 CRUD（见 1.15）
- Plans：套餐 CRUD、默认套餐指针
- Keys：创建 / 批量创建 / 搜索 / VIP 过滤 / 文件导入
- Quota：配额总览（team 组织）与多维重置
- Logs：请求日志，支持列过滤
- Config：进程级配置点路径编辑、客户端拦截规则卡片（见 1.14）、配置导出 / 回写按钮、Reload
- Stress / Status：系统压力时序、进程状态
- Debug：错误录制开关与查询

### 1.12 热加载（Hot Reload）

修改 **YAML 进程级配置**（server / router 调度参数 / prompt_log / trace / hooks）后，三种方式零停机触发热加载（基于 ArcSwap 原子交换）：

1. 信号：`kill -HUP <pid>`（仅 Unix）
2. API：`POST /admin/config/reload`（需 master key）
3. Dashboard：点击 "Reload Config"

热加载同时与 DB 业务配置做**增量对账**（外部改库 / 多实例场景的收敛机制；面板 CRUD 本身写完即生效，不依赖 reload）。

限流计数器、并发占用、key-plan 分配等运行时状态跨 reload 保留，不丢业务上下文。

> 业务配置（模型 / 套餐 / 别名 / 拦截规则 / workflow / 计费模板）改 YAML **无效**——它们以 DB 为权威，走 Dashboard 管理（见 §0）。

### 1.13 客户端类型标识

在 deployment 上设 `client_type_header: true` 后，网关会向上游注入 `X-BooM-Client-Type: anthropic|anonymous` 头，标识请求来自 `/v1/messages`（anthropic）还是其他端点（anonymous）。配合 boom-ctxaware 的 Agent Statistics 统计，可在 Dashboard 区分 Claude Code 等 anthropic 客户端与普通 OpenAI 客户端的流量分布。

```yaml
model_list:
  - model_name: claude-sonnet
    client_type_header: true            # 向上游注入 X-BooM-Client-Type 头
    litellm_params:
      model: anthropic/claude-sonnet-4-20250514
      api_key: ${ANTHROPIC_API_KEY}
```

### 1.14 客户端拦截（Client Blocklist）

按「请求头 / 请求体字段 / 提示词内容」匹配并拒绝特定 agent 客户端的请求（如封禁某个客户端版本）。规则与总开关在 Dashboard Config 页管理（DB 权威，写完即生效）；被拒请求记审计日志（`error_type=client_blocked`）并强制写 prompt log 全文。

定位是**运维手段，不是安全边界**——header 与 prompt 都可被客户端伪造，强边界仍是禁用 key / 模型权限。

（seed 配置格式见 §2.2.9）

### 1.15 Direct Synthesis Workflow

客户端请求一个 workflow 模型名（如 `model: "fusion"`），网关并行调用多个 panel 模型、再由 aggregator 合成标准 OpenAI 响应——对客户端表现为一个普通模型。workflow 定义与模型绑定在 Dashboard Workflows 页管理（DB 权威，写完即生效）。

panel / aggregator 引用真实模型名或别名，受与普通模型一致的鉴权和套餐限制；子调用 token usage 聚合计入父请求。

（seed 配置格式见 §2.2.10；设计详见 [docs/direct-synthesis-workflow-design.md](docs/direct-synthesis-workflow-design.md)）

---

## 2. 参数配置说明

按重要程度先列必配项，再列选配项；选配项标注不配置时的默认行为。

### 2.1 必配项

最小可启动配置只需要两样东西：一个 model deployment、一个 master key。

```yaml
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_key: sk-your-openai-key

general_settings:
  master_key: sk-your-master-key
```

| 字段 | 位置 | 说明 |
|------|------|------|
| `model_list[].model_name` | 顶级 | 对外暴露的模型名 |
| `model_list[].litellm_params.model` | 顶级 | 上游模型标识，`provider/model-id` 格式 |
| `model_list[].litellm_params.api_key` | 顶级 | 上游 API key（OpenAI/Gemini/DeepSeek 等需要） |
| `general_settings.master_key` | 顶级 | 管理 API 与 Dashboard 登录密钥 |

> 上述 YAML 是**新装机形态**：业务 section 只在空库首次启动时 seed 进 DB（见 §0）。已运行网关的模型增删改走 Dashboard CRUD，不再改 YAML。
>
> 也可以完全不用 YAML 启动（业务配置全来自 DB）：
>
> ```bash
> export BOOM_MASTER_KEY="sk-your-master-key"
> export BOOM_DATABASE_URL="postgres://user:pass@localhost:5432/boom_gateway"
> ./target/release/boom-gateway
> ```

> 数据库并非启动硬依赖：未配 `database_url` 时网关可启动，但 key 鉴权、套餐、审计、Dashboard 写操作都依赖 DB，生产环境强烈建议配置。

### 2.2 选配项

#### 2.2.1 general_settings

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `database_url` | `null` | PostgreSQL 连接串。不配则不持久化 key/套餐/日志，仅 master key 鉴权可用 |
| `dashboard_session_secret` | `null` | Dashboard 会话 JWT 签名密钥（可选）。必须独立于 `master_key`（与 `master_key` 相同时打 warn，但不阻断启动），建议 ≥32 字符（不足仅打 warn）；不配则每次启动随机生成，重启后会话失效、多副本之间不共享。env bootstrap 模式取 `BOOM_DASHBOARD_SESSION_SECRET` |
| `public_models` | `[]` | 全员可访问的模型列表，绕过 key/team 白名单。不配则所有模型按白名单管控 |

```yaml
general_settings:
  master_key: ${MASTER_KEY}             # 必配：管理 API 与 Dashboard 登录密钥
  database_url: ${DATABASE_URL}         # 选配：PostgreSQL 连接串
  dashboard_session_secret: ${DASHBOARD_SESSION_SECRET}  # 选配：Dashboard 会话签名密钥
  public_models:                        # 选配：全员可访问模型，默认空
    - gpt-4o-mini
    - deepseek-chat
```

> `store_model_in_db`（v2 双模式开关）已废弃：业务配置现在无条件 DB 权威。YAML 中残留该字段会被静默忽略。

#### 2.2.2 server

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `host` | `0.0.0.0` | 绑定地址 |
| `port` | `4000` | 绑定端口 |
| `workers` | `4` | 工作线程数（实际 tokio runtime 固定 32 线程，此字段保留兼容） |

```yaml
server:
  host: 0.0.0.0                         # 默认 0.0.0.0
  port: 4000                            # 默认 4000
  workers: 4                            # 默认 4（tokio runtime 固定 32 线程，此字段保留兼容）
```

#### 2.2.3 model_list[]

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `litellm_params.api_base` | `null` | 上游 API base URL。不配则用 provider 默认地址 |
| `litellm_params.api_version` | `null` | Azure API 版本，仅 azure/ 需要 |
| `litellm_params.aws_region_name` / `aws_access_key_id` / `aws_secret_access_key` | `null` | Bedrock 凭证 |
| `litellm_params.rpm` | `null` | 部署级 RPM 限制。不配则只受 key 套餐约束 |
| `litellm_params.tpm` | `null` | 部署级 TPM 限制 |
| `litellm_params.timeout` | `1200` | 单次请求超时（秒） |
| `litellm_params.headers` | `{}` | 自定义上游请求头 |
| `litellm_params.temperature` | `null` | 温度覆盖 |
| `litellm_params.max_tokens` | `null` | 最大输出 token 覆盖 |
| `model_info.id` | `null` | deployment 标识。流控必须配 |
| `model_info.input_cost_per_token` / `output_cost_per_token` | `null` | 成本统计（内联定价，一次性配置用） |
| `model_info.cost_template` | `null` | 引用计费模板名（见 §2.2.11），同名模板覆盖内联价格。多模型共用定价时推荐 |
| `model_info.quota_count_ratio` | `1` | 配额消耗倍率 |
| `flow_control.model_queue_limit` | `null`(=无限制) | 最大在途请求数 |
| `flow_control.model_context_limit` | `null`(=无限制) | 在途输入字符总量上限 |
| `serve_not_match` | `false` | `true` 时该 deployment 同时承担 `"*"` 兜底 |
| `enabled` | `true` | `false` 时写入 DB 但不进入内存路由表（在 Dashboard 可见但不参与调度） |
| `client_type_header` | `false` | `true` 时向上游注入 `X-BooM-Client-Type` 头 |

```yaml
model_list:
  # 带 model_info + flow_control + 自定义 headers 的完整 deployment
  - model_name: claude-sonnet
    model_info:
      id: claude-node-a                 # deployment 标识，流控必须配
      input_cost_per_token: 0.000003    # 成本统计
      output_cost_per_token: 0.000015
      quota_count_ratio: 3              # 配额消耗倍率，默认 1
    flow_control:
      model_queue_limit: 50             # 最大在途请求数（不设 = 无限制）
      model_context_limit: 5000000      # 在途输入字符总量上限
    serve_not_match: false              # true 时同时承担 "*" 兜底
    enabled: true                       # false 时只写 DB 不进路由表
    client_type_header: false           # true 时注入 X-BooM-Client-Type
    litellm_params:
      model: anthropic/claude-sonnet-4-20250514
      api_key: ${ANTHROPIC_API_KEY}
      api_base: https://api.anthropic.com   # 不配用 provider 默认
      timeout: 1200                     # 单次请求超时（秒），默认 1200
      rpm: 60                           # 部署级 RPM（不设 = 只受 key 套餐约束）
      tpm: 100000                       # 部署级 TPM
      temperature: 0.7                  # 温度覆盖
      max_tokens: 4096                  # 最大输出 token 覆盖
      headers:                          # 自定义上游请求头
        X-Custom-Header: custom-value

  # Bedrock deployment — AWS 凭证
  - model_name: bedrock-claude
    litellm_params:
      model: bedrock/anthropic.claude-3-sonnet
      aws_region_name: us-east-1
      aws_access_key_id: ${AWS_ACCESS_KEY_ID}
      aws_secret_access_key: ${AWS_SECRET_ACCESS_KEY}

  # Azure deployment — 需要 api_base + api_version
  - model_name: azure-gpt4
    litellm_params:
      model: azure/my-gpt4-deployment
      api_base: https://my-resource.openai.azure.com
      api_key: ${AZURE_API_KEY}
      api_version: "2024-06-01"
```

#### 2.2.4 router_settings

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `schedule_policy`（别名 `routing_strategy`） | `round_robin` | 调度策略：`round_robin` / `key_affinity` / `kvc_aware` |
| `model_group_alias` | `{}` | 别名映射（seed-only；运行期经 Dashboard Models 页别名 CRUD 管理） |
| `key_affinity_context_threshold` | `0` | key_affinity 下上下文字符阈值，低于此值走最低负载。`0` 表示始终走亲和 |
| `rebalance_threshold`（兼容旧名 `key_affinity_rebalance_threshold`） | `20` | 再均衡阈值（百分比 1..=100），key_affinity 与 kvc_aware 共用：优选/命中 worker 利用率比最低负载候选高出此百分比时改交最低负载 |
| `kvc_aware.max_blocks` | `500000` | 前缀 trie 容量上限（块数），超出按 LRU 淘汰最久未访问块 |
| `kvc_aware.router_ttl_secs` | `1200` | 自学习块 TTL（秒），到期由后台扫描删除。`0` = 禁用 TTL 扫描 |
| `auto_router.model_name` | `null` | 语义路由虚拟模型名（如 `"auto"`），客户端请求此名触发内容分级 |
| `auto_router.strategy` | `tier_classifier` | `tier_classifier`（内置启发式）或 `ml_service`（外部 HTTP 分类服务） |
| `auto_router.default_tier` | `medium` | 分类不确定时的默认档位 |
| `auto_router.tiers` | — | 档位到目标模型的映射（small / medium / large → `target_model`） |
| `auto_router.ml_service` | — | `strategy: ml_service` 时的分类服务 `{url, timeout_ms(默认 100)}`；任何失败自动回落 tier_classifier |
| `enable_priority_header` | `false` | 向上游注入 `X-Gateway-Priority` 头，供下游调度器识别优先级 |
| `strip_claude_code_attribution` | `false` | 剥离 Claude Code 在 `/v1/messages` 注入的 `x-anthropic-billing-header` 块 |

```yaml
router_settings:
  # 调度策略（别名 routing_strategy 也可用）
  schedule_policy: kvc_aware            # round_robin（默认）/ key_affinity / kvc_aware
  rebalance_threshold: 20               # 偏好节点利用率高出 20% 时迁移（1..=100）

  # kvc_aware 参数（仅 schedule_policy: kvc_aware 时生效）
  kvc_aware:
    max_blocks: 500000                  # trie 容量上限（块数）
    router_ttl_secs: 1200               # 自学习块 TTL（秒），0 = 禁用扫描

  # 模型别名（seed-only；运行期走 Dashboard）
  model_group_alias:
    "gpt-4": "gpt-4o"
    "GPT-4o":
      model: "gpt-4o"
      hidden: true

  # 语义路由（可选）：请求 "auto" 触发内容分级
  # auto_router:
  #   model_name: "auto"
  #   strategy: "tier_classifier"       # 或 "ml_service" 外接分类
  #   default_tier: "medium"
  #   tiers:
  #     small:  { target_model: "gpt-4o-mini" }
  #     medium: { target_model: "gpt-4o" }
  #     large:  { target_model: "gpt-4o" }

  # 向上游注入优先级头（默认 false）
  enable_priority_header: false

  # 剥离 Claude Code 注入的 billing-header 块（默认 false）
  # 仅在转发到非 Anthropic backend 时建议开
  strip_claude_code_attribution: false
```

#### 2.2.5 rate_limit

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `enabled` | `true` | 全局限流开关 |
| `default_rpm` | `60` | 无套餐 key 的默认 RPM |
| `window_limits` | `[]` | 全局自定义时间窗口 `[[count, seconds], ...]`。不配则只有 RPM 维度 |

```yaml
rate_limit:
  enabled: true                         # 默认 true
  default_rpm: 60                       # 无套餐 key 的默认 RPM，默认 60
  window_limits:                        # 自定义时间窗口 [[次数, 秒数], ...]
    - [100, 18000]                      # 100 次 / 5 小时
    - [50, 3600]                        # 50 次 / 1 小时
```

#### 2.2.6 plan_settings

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `default_plan` | `null` | 未分配套餐 key 的默认套餐名（必须在 `plans` 中存在）。不配则无套餐 key 走 `rate_limit.default_rpm` |
| `plans.<name>.concurrency_limit` | `null`(=无限制) | 最大并发 |
| `plans.<name>.rpm_limit` | `null`(=无限制) | 每分钟请求数 |
| `plans.<name>.window_limits` | `[]` | 套餐自定义时间窗口 |
| `plans.<name>.schedule` | `[]` | 时段调度列表。不配则全时段统一参数 |

```yaml
plan_settings:
  default_plan: basic                   # 未分配套餐 key 的默认套餐

  plans:
    basic:
      concurrency_limit: 4              # 最大并发（不设 = 无限制）
      rpm_limit: 60                     # 每分钟请求数
      window_limits:
        - [100, 18000]                  # 100 次 / 5 小时

    pro:
      concurrency_limit: 10
      rpm_limit: 120
      window_limits:
        - [500, 18000]

    # 时段调度套餐 — 跨午夜
    scheduled-plan:
      concurrency_limit: 4
      rpm_limit: 40
      window_limits:
        - [80, 18000]
      schedule:                         # 时段调度列表，不配则全时段统一
        - hours: "9:00-21:00"           # 高峰期
          concurrency_limit: 4
          rpm_limit: 40
          window_limits:
            - [80, 18000]
        - hours: "21:00-9:00"           # 低峰期（跨午夜）
          concurrency_limit: 8
          rpm_limit: 80
          window_limits:
            - [160, 18000]
```

#### 2.2.7 deployment_health_check

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `auto_offline_enabled` | `false` | 在线节点探活失败自动离线。不配则不自动离线，只记录告警 |
| `auto_recovery_enabled` | `false` | 离线节点探活成功自动上线。不配则需手动恢复 |
| `path` | `/metric` | 探活路径 |
| `failure_threshold` | `3` | 连续失败几次后离线 |
| `recovery_threshold` | `2` | 连续成功几次后上线 |
| `offline_check_interval_secs` | `30` | 在线节点探活间隔 |
| `recovery_check_interval_secs` | `60` | 离线节点探活间隔 |
| `request_failure_auto_offline_enabled` | `false` | 请求失败自动离线。不配则只靠探活 |
| `request_failure_threshold` | `3` | 连续请求失败几次后离线 |

```yaml
deployment_health_check:
  auto_offline_enabled: false           # 默认 false：在线节点探活失败自动离线
  auto_recovery_enabled: false          # 默认 false：离线节点探活成功自动上线
  path: /metric                         # 默认 /metric：探活路径 GET {api_base}{path}
  failure_threshold: 3                  # 默认 3：连续失败几次后离线
  recovery_threshold: 2                 # 默认 2：连续成功几次后上线
  offline_check_interval_secs: 30       # 默认 30：在线节点探活间隔
  recovery_check_interval_secs: 60      # 默认 60：离线节点探活间隔
  request_failure_auto_offline_enabled: false   # 默认 false：请求失败也自动离线
  request_failure_threshold: 3                   # 默认 3：连续请求失败几次后离线
```

#### 2.2.8 prompt_log

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `enabled` | `false` | 总开关。不配则不写 prompt 日志 |
| `dir` | `/data/prompt_logs` | 日志根目录 |
| `max_file_size_mb` | `50` | 单文件大小上限，超出滚动并 gzip |
| `capture_raw_upstream` | `false` | 同时记录协议转换前的上游原始响应 |
| `excluded_keys` | `[]` | 排除的 key hash 列表 |
| `excluded_teams` | `[]` | 排除的 team_id 列表 |

```yaml
prompt_log:
  enabled: false                        # 默认 false：总开关
  dir: /data/prompt_logs                # 默认 /data/prompt_logs：日志根目录
  max_file_size_mb: 50                  # 默认 50：单文件上限，超出滚动并 gzip
  capture_raw_upstream: false           # 默认 false：同时记录上游原始响应
  excluded_keys: []                     # 默认空：排除的 key hash 列表
  excluded_teams: []                    # 默认空：排除的 team_id 列表
```

#### 2.2.9 client_blocklist（seed-only）

按请求头 / 请求体字段 / 提示词内容匹配并拒绝特定客户端。规则与总开关运行期以 DB 为权威，经 Dashboard Config 页管理；本 section 仅在全新库首次启动时 seed。

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `enabled` | `false` | 总开关，`false` 时热路径直接跳过（运行期经 Dashboard / `PUT /admin/blocklist/enabled` 修改） |
| `rules[].name` | — | 规则名，唯一（DB 主键） |
| `rules[].enabled` | `true` | 单规则开关 |
| `rules[].conditions[]` | — | `{field, op, value}`，规则内多条件 AND |
| `rules[].action` | 403 | `{status, message, code}` 拒绝响应 |

条件字段寻址：`header.<name>`（大小写不敏感）、`body.<json_pointer>`（如 `body.model`、`body.messages.0.content`，提取值字符串化）、`body`（整个请求体序列化文本）。操作符：`eq` / `contains` / `regex` / `prefix` / `exists`。

```yaml
client_blocklist:
  enabled: true
  rules:
    - name: block-cursor-agent
      enabled: true
      conditions:                       # 规则内多条件 AND
        - { field: header.user-agent, op: contains, value: "cursor" }
        - { field: body.model, op: eq, value: "gpt-4o" }
      action:
        status: 403
        message: "该客户端已被禁用，请联系 admin@example.com"
        code: client_blocked
```

#### 2.2.10 workflow_settings（seed-only）

Direct Synthesis Workflow 的定义与模型映射。运行期经 Dashboard Workflows 页管理（DB 权威）；本 section 仅首次 seed。

| 字段 | 说明 |
|------|------|
| `models` | 客户端模型名 → workflow 名映射（如 `fusion: direct_synthesis`） |
| `workflows.<name>.type` | workflow 类型，当前仅 `direct_synthesis` |
| `workflows.<name>.roles.panel[]` | panel 实例列表（≥2 个），每项 `{model, temperature?}` |
| `workflows.<name>.roles.aggregator` | aggregator 实例（恰 1 个），`{model, temperature?}` |
| `workflows.<name>.panel_timeout_secs` | panel 调用超时（秒） |

```yaml
workflow_settings:
  models:
    fusion: direct_synthesis            # 客户端请求 model: "fusion" 即触发
  workflows:
    direct_synthesis:
      type: direct_synthesis
      roles:
        panel:
          - { model: gpt-4o-mini }
          - { model: deepseek-chat, temperature: 0.3 }
        aggregator: { model: gpt-4o }
      panel_timeout_secs: 120
```

校验：panel / aggregator 引用的模型名或别名必须在当前可路由集合内，否则启动 / 保存被拒绝。

#### 2.2.11 cost_templates（seed-only）

计费模板：可复用的模型定价（USD / 1M tokens），由 `model_info.cost_template` 引用。运行期经 Dashboard Config 页管理（DB 权威，改模板会对引用方模型重算费率）；本 section 仅首次 seed。

| 字段 | 说明 |
|------|------|
| `name` | 模板名，`model_info.cost_template` 引用此名 |
| `input_cost_per_million_tokens` | 输入 token 单价（USD / 1M） |
| `cached_input_cost_per_million_tokens` | KV-cache 命中输入单价（通常为输入价的 1/2） |
| `output_cost_per_million_tokens` | 输出 token 单价（USD / 1M） |

```yaml
cost_templates:
  - name: gpt-4o
    input_cost_per_million_tokens: 2.5
    cached_input_cost_per_million_tokens: 1.25
    output_cost_per_million_tokens: 10.0
```

#### 2.2.12 hooks（进程级）

pre_auth 钩子：把场景化定制逻辑（key 加前缀、删后缀、查表转换）从 gateway 主体剥离，由业务方自己写 `.so` 接入，网关二进制无需为定制逻辑重编。

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `pre_auth.enabled` | `false` | 总开关。整个字段不写等同关闭 |
| `pre_auth.path` | `""` | `.so` / `.dylib` 绝对路径 |
| `pre_auth.failure_mode` | `allow` | hook 调用失败（panic / 错误码）时：`allow` = 降级走原生认证；`deny` = 返回 500 |
| `pre_auth.allowed_headers` | `[]` | 只把这些 header 传给 hook，默认不传 |
| `pre_auth.config` | `""` | 自由文本，原样传给 hook_init |

hook 返回三种 action：`continue`（原 key 走原生认证）/ `replace`（替换 key 后认证）/ `reject`（401）。编写示例见 `crates/example-hooks/` 与 `hook/README.md`。

```yaml
hooks:
  pre_auth:
    enabled: false
    path: ""
    failure_mode: allow
    allowed_headers: []                 # 如业务需要从 header 取字段: [ucid]
    config: ""                          # 如 '{"prefix":"sk-customer-"}'
```

#### 2.2.13 trace（进程级）

OTel trace 链路与 OTLP traces 导出。与 `prompt_log` 相互独立（两通道四组合均支持）。

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `enabled` | `false` | 总开关。关闭时不建 span、不上报；`propagate_only` 控制是否仍透传 traceparent |
| `otlp` | — | OTLP exporter 配置（与 `prompt_log.otlp` 同构）：`endpoint`、`service_name`、`timeout_secs`、`batch_size`、`flush_interval_secs`、`max_attribute_bytes` |
| `capture_body` | `true` | span 属性包含请求 / 响应体（与 prompt log 共享 body Arc，零拷贝） |
| `max_body_bytes` | `16384` | body 截断阈值（字节），超出截断并标记 |
| `propagate_only` | `true` | `enabled=false` 时仍向上游透传 `traceparent` / `tracestate`（W3C proxy 最低义务） |
| `report_filter` | — | 采样过滤：`{tracestate_keys: [], trace_id_regex: null}`，匹配任一才 trace；两者全空 = 全量 |

```yaml
trace:
  enabled: true
  capture_body: true
  max_body_bytes: 16384
  otlp:
    endpoint: http://otlp-collector:4318
    service_name: boom-gateway
  report_filter:
    tracestate_keys: []                 # 如只 trace 携带某 key 的流量
    trace_id_regex: null
```

---

## 3. 建议配置项

### 3.1 自部署推理场景建议用 kvc_aware（或 key_affinity）

- **场景**：上游是 vLLM / Ollama 等自部署推理服务，且同一会话多次请求会复用上下文。
- **收益**：相同会话前缀的请求命中同一 worker 的 KV-cache，降低 TTFT 与重算成本。
- **建议参数**（前缀缓存亲和，推荐）：
  ```yaml
  router_settings:
    schedule_policy: kvc_aware
    rebalance_threshold: 20                # 默认即可
    kvc_aware:
      max_blocks: 500000                   # 默认即可
      router_ttl_secs: 1200                # 默认即可
  ```
- **或**会话亲和（按 key 粒度粘住，实现更简单）：
  ```yaml
  router_settings:
    schedule_policy: key_affinity
    key_affinity_context_threshold: 2000   # 短上下文先走最低负载预热
    rebalance_threshold: 20                # 默认即可
  ```
- **不适用**：上游是 OpenAI/Anthropic 官方 API（无本地 KV 概念），用 `round_robin` 即可。

### 3.2 Plan 配置建议

- 至少分 3 档：`basic` / `pro` / `enterprise`，对应不同团队或外部客户。
- 用 `window_limits` 兜住长周期配额（如 5 小时 100 次），用 `rpm_limit` 兜住瞬时尖峰。
- 内部团队用 `schedule` 在夜间放宽（`21:00-9:00` 翻倍并发与 RPM），白天高峰严控。
- 昂贵模型（Claude Opus、GPT-4o 大上下文）配 `quota_count_ratio: 3~5`，让单次请求消耗多倍配额，避免少数大请求吃掉整组预算。
- 设 `default_plan: basic` 兜底，避免新 key 无套餐时无限流量。

```yaml
# 套餐：3 档 + 时段调度
plan_settings:
  default_plan: basic                   # 兜底，避免新 key 无套餐无限流量
  plans:
    basic:                              # 外部客户
      concurrency_limit: 4
      rpm_limit: 60
      window_limits:                    # 长周期配额兜底
        - [100, 18000]                  # 100 次 / 5 小时
    pro:
      concurrency_limit: 10
      rpm_limit: 120
      window_limits:
        - [500, 18000]
    enterprise:
      concurrency_limit: 50
      rpm_limit: 600
      window_limits:
        - [2000, 18000]
        - [500, 3600]                   # 瞬时尖峰兜底：500 次 / 1 小时
    internal-night:                     # 内部团队夜间放宽
      concurrency_limit: 4
      rpm_limit: 40
      window_limits:
        - [80, 18000]
      schedule:
        - hours: "9:00-21:00"
          concurrency_limit: 4
          rpm_limit: 40
        - hours: "21:00-9:00"           # 夜间翻倍
          concurrency_limit: 8
          rpm_limit: 80

# 昂贵模型配高倍率
model_list:
  - model_name: claude-opus
    model_info:
      id: opus-node-1
      quota_count_ratio: 5              # 1 次 opus 请求消耗 5 个配额
    litellm_params:
      model: anthropic/claude-opus-4-20250514
      api_key: ${ANTHROPIC_API_KEY}
```

### 3.3 Prompt 日志配置建议

- 生产环境**默认关闭**，只在合规审计或问题排查时开启——开启会增加磁盘 I/O 与存储压力。
- 用 `excluded_teams` 排掉无需审计的内部团队。
- `dir` 指向独立磁盘，避免与日志或 DB 争 I/O。
- `max_file_size_mb` 用默认 50 即可；滚动后自动 gzip，长期保留成本低。
- 排查上游异常时临时打开 `capture_raw_upstream`，配合 Dashboard Debug 录制定位问题。

```yaml
prompt_log:
  enabled: false                        # 生产默认关闭，按需开启
  dir: /data/prompt_logs                # 指向独立磁盘
  max_file_size_mb: 50                  # 默认值即可，滚动后自动 gzip
  capture_raw_upstream: false           # 排查上游异常时临时打开
  excluded_teams:                       # 排掉无需审计的内部团队
    - internal-ops
```

### 3.4 模型实例配置建议

- **同名多部署**：把同一模型的多个上游实例配成相同 `model_name`，网关自动在它们之间负载均衡。
  ```yaml
  model_list:
    - model_name: gpt-4o           # 实例 A
      model_info:
        id: gpt4o-node-a
      litellm_params:
        model: openai/gpt-4o
        api_base: http://10.0.0.1:8000/v1
    - model_name: gpt-4o           # 实例 B，同名
      model_info:
        id: gpt4o-node-b
      litellm_params:
        model: openai/gpt-4o
        api_base: http://10.0.0.2:8000/v1
  ```
- **给每个实例配 `model_info.id`**：流控、健康检查都以 `id` 作为 deployment 标识，缺失会导致这些功能无法正确归属。
- **流控按实例容量配**：`model_queue_limit` 设为该实例能稳定承担的并发（如 vLLM 单卡跑 7B 模型，建议 10~20）；`model_context_limit` 按显存换算的上下文总量设。

```yaml
# 流控按实例容量配 — vLLM 单卡 7B 模型典型配置
model_list:
  - model_name: my-llama
    model_info:
      id: llama-node-a                  # 流控依赖 id
    flow_control:
      model_queue_limit: 15             # 单卡稳定承担的并发
      model_context_limit: 1000000      # 按显存换算的上下文总量
    litellm_params:
      model: hosted_vllm/my-model
      api_base: http://10.0.0.1:8000/v1
```

---

## 4. 配置注意事项

### 4.1 public_models 绕过权限管控

`general_settings.public_models` 列出的模型对**所有 key 开放**，不受 key/team 白名单约束。**不要把高价值或敏感模型放进 public_models**，否则任何持有有效 key 的客户端都能调用。新增公共模型前确认其计费与合规边界。

```yaml
# ✅ 安全：只放廉价模型进 public_models
general_settings:
  public_models:
    - gpt-4o-mini
    - deepseek-chat

# ❌ 危险：把昂贵模型放进 public_models，任何 key 都能调用
# general_settings:
#   public_models:
#     - claude-opus
```

### 4.2 `"*"` 是兜底路由，不是全权限

`"*"` 是一个真实的 `model_name`，用来承接未匹配到任何已配置模型的请求。**判断一个 key 是否"全权限"的唯一依据是它的 `models` 数组为空或包含 `"all-team-models"`**，与 `"*"` 无关。把 `"*"` 误当全权限标记会导致权限模型错乱。

```yaml
# "*" 是兜底路由 —— 承接未匹配的模型名，与 key 权限无关
model_list:
  - model_name: "*"
    litellm_params:
      model: openai/gpt-4o-mini
      api_key: ${OPENAI_API_KEY}
```

```bash
# key 的"全权限"由 models 字段决定，与 "*" 无关
# 全权限 key：models 为空或含 "all-team-models"
curl -X POST http://localhost:4000/dashboard/api/admin/keys \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_alias": "full-access", "models": ["all-team-models"]}'
```

### 4.3 YAML 业务 section 是 seed，不是日常配置入口

`model_list` / `plan_settings` / `client_blocklist` / `model_group_alias` / `workflow_settings` / `cost_templates` 在 YAML 中只用于**全新库首次启动导入**（业务表全空且无 seed 标记时一次性 seed，见 §0）。此后这些 section 完全 inert：

- 手改 YAML + 热加载**不会生效**（日志会提示 inert，建议改用 Dashboard）；
- Dashboard 删除的模型 / 套餐**不会**被重启复活（seed 标记防复活）；
- 运行期的业务配置变更一律走 Dashboard CRUD（写 DB + 直改内存，立即生效）。

需要快照 / 灾备工件时用 Dashboard Config 页的「导出」/「回写」按钮；回写生成的 YAML 可用于空库恢复（重启即重新 seed）。

### 4.4 strip_claude_code_attribution 的反向风险

`true` 会整块剥离 Claude Code 在 `/v1/messages` system prompt 开头注入的 `x-anthropic-billing-header` text block，恢复非 Anthropic backend（vLLM / Bedrock / OpenAI 兼容）的 KV-cache prefix matching。

**但**：上游是 Anthropic 官方 API 时**不要开**，剥离可能触发反盗版风控。仅在转发到非 Anthropic backend 时开启。`/v1/chat/completions` 不受此开关影响（Claude Code 不会在该协议下注入）。

```yaml
router_settings:
  # ✅ 转发到 vLLM/Bedrock/OpenAI 兼容 backend 时开启
  strip_claude_code_attribution: true

  # ❌ 上游是 Anthropic 官方 API 时不要开（可能触发反盗版风控）
  # strip_claude_code_attribution: true
```

### 4.5 启用流控必须配 model_info.id

`flow_control` 以 deployment 为单位排队，依赖 `model_info.id` 作为槽位标识。没配 `id` 的 deployment 即使写了 `flow_control` 也不会生效。

```yaml
# ✅ 流控生效：配了 model_info.id
model_list:
  - model_name: claude-opus
    model_info:
      id: opus-node-1                   # 必须配，流控槽位标识
    flow_control:
      model_queue_limit: 20
      model_context_limit: 2000000
    litellm_params:
      model: anthropic/claude-opus-4-20250514
      api_key: ${ANTHROPIC_API_KEY}

# ❌ 流控不生效：缺 model_info.id，flow_control 被忽略
# - model_name: claude-opus
#   flow_control:
#     model_queue_limit: 20
#   litellm_params:
#     model: anthropic/claude-opus-4-20250514
#     api_key: ${ANTHROPIC_API_KEY}
```

### 4.6 健康检查只对 DB 中的 deployment 生效

自动离线/恢复探活的目标来自 DB（`DeploymentStore::list_health_check_targets`）。**纯 YAML 模式（无 `database_url`）下，健康检查与自动隔离功能不工作**。需要自动隔离能力就配 DB。

```yaml
# 健康检查需要 DB 支持 — 必须配 database_url
general_settings:
  database_url: ${DATABASE_URL}

deployment_health_check:
  auto_offline_enabled: true
  auto_recovery_enabled: true
  path: /metric
  failure_threshold: 3
  recovery_threshold: 2
```

### 4.7 环境变量未设置时保留原文

`${VAR}` / `os.environ/VAR` 在环境变量不存在时**保留原文本不替换**（如 `api_key: ${OPENAI_API_KEY}` 在变量未设时会变成字面量 `${OPENAI_API_KEY}` 被当成 key），不会启动失败。生产环境务必确认环境变量已注入，避免把占位符当真实凭证。

```yaml
# 两种语法等价，变量未设时保留原文不替换
general_settings:
  master_key: ${MASTER_KEY}             # ${VAR} 语法
  database_url: os.environ/DATABASE_URL # os.environ/VAR 语法

model_list:
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_key: ${OPENAI_API_KEY}         # 若 OPENAI_API_KEY 未设，
                                         # 会变成字面量 "${OPENAI_API_KEY}" 当 key 用
```

### 4.8 启用 enabled: false 仍占 DB 行

`enabled: false` 的 deployment 会写入 DB（Dashboard 可见）但不进入内存路由表。可用于"先登记、后启用"的灰度场景，但要注意它仍占用 DB 行，长期不用应删除而非置 false。

```yaml
# 灰度场景：先登记到 DB（Dashboard 可见），不进路由表
model_list:
  - model_name: new-model
    enabled: false                      # 写 DB 但不参与调度
    litellm_params:
      model: openai/new-model
      api_key: ${OPENAI_API_KEY}
  # 验证完毕后改 enabled: true 或直接删除条目
```

---

## 5. 常见网关操作 / 运维手册

### 5.1 启动

```bash
# 设置环境变量
export MASTER_KEY="sk-your-master-key"
export DATABASE_URL="postgres://user:pass@localhost:5432/boom_gateway"
export OPENAI_API_KEY="sk-..."
export ANTHROPIC_API_KEY="sk-ant-..."

# 直接运行（默认读 config.yaml）
./target/release/boom-gateway

# 指定配置文件与端口
./target/release/boom-gateway --config /etc/boom/config.yaml --port 4000

# 覆盖 host
./target/release/boom-gateway --host 127.0.0.1
```

CLI 参数：

| 参数 | 说明 |
|------|------|
| `-c, --config <PATH>` | 配置文件路径。缺省时依次回退：cwd 的 `config.yaml` → env bootstrap（`BOOM_MASTER_KEY` + `BOOM_DATABASE_URL`，两者必需）。指定的文件不存在时同样回退 env bootstrap 并打 warn |
| `--host <HOST>` | 覆盖配置文件中的 host |
| `--port <PORT>` | 覆盖配置文件中的 port |
| `--reboot` | 启动前优雅停止已有实例（见 5.3） |

### 5.2 配置热加载

修改 YAML **进程级配置**（server / router 调度参数 / prompt_log / trace / hooks）后任选其一：

```bash
# 方式 1：SIGHUP（仅 Unix）
kill -HUP $(pgrep -x boom-gateway)

# 方式 2：Admin API（需 master key）
curl -X POST http://localhost:4000/admin/config/reload \
  -H "Authorization: Bearer $MASTER_KEY"

# 方式 3：Dashboard → Config Reload 按钮
```

热加载行为：

- 重新读取 YAML **进程级配置**，重建 provider HTTP client（开销低）。
- 业务配置与 DB 做**增量对账**：内存有 DB 无 → 移除；DB 有内存无或有变化 → 应用；**永不清空 store**。对账失败则中止 reload，旧配置继续服务。
- 复用 DB 连接池、限流计数器、并发占用、key-plan 分配，**无状态丢失**。
- 原子交换内部状态（ArcSwap），零停机。
- env bootstrap 模式（无 YAML 启动）下 reload 只做 DB 对账，进程配置保持当前值。
- YAML 中残留非空业务 section 时打 warn 提示 inert（该改走 Dashboard）。

### 5.3 --reboot：安全重启

```bash
./target/release/boom-gateway --reboot
```

`--reboot` 会先优雅停止已运行的 boom-gateway 实例再启动新进程：

1. 通过 `pgrep -x boom-gateway` 找到其他实例 PID。
2. 发送 SIGTERM，请求其优雅退出。
3. 周期性探活 `/health` 判断老进程状态：
   - `/health` 响应 → 仍在优雅退出，继续等
   - 连接拒绝 → listener 已关，继续等
   - 超时无响应 → runtime 冻结，立即 SIGKILL
4. 老进程退出（或被强杀）后启动新实例。

硬上限 30 秒，正常情况不会触发强杀。适合脚本化部署/CI 滚动更新，避免端口占用与双实例并存。

### 5.4 业务配置走 Dashboard，进程配置走 YAML

- **业务配置**（模型 / 别名 / 套餐 / 默认套餐 / 拦截规则 / workflow / 计费模板）：一律通过 Dashboard（或对应 Admin API）修改。写 DB + 直改内存，**立即可路由**；不写 YAML、不依赖 reload。改 YAML 业务段是无效操作（见 §0 / 4.3）。
- **进程级配置**（server / router 调度参数 / prompt_log / trace / hooks）：改 YAML + 热加载。YAML 是版本可控的配置源，便于审计与回滚。
- 改完 YAML **务必热加载**，否则改动不生效。
- 需要 YAML 形式的业务配置快照（回滚 / 灾备 / 审计）时，用 Dashboard Config 页「导出」下载或「回写」刷新启动文件——这是手动操作，不是同步机制；回退二进制版本前建议先回写一次。

### 5.5 创建 API Key

```bash
# 单个创建（响应里 key 只展示一次，立即保存）
curl -X POST http://localhost:4000/dashboard/api/admin/keys \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_alias": "alice", "plan_name": "pro"}'

# VIP key（流控优先调度）
curl -X POST http://localhost:4000/dashboard/api/admin/keys \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_alias": "vip-user", "metadata": {"vip": true}}'

# 批量创建 — 一次创建多个 key
curl -X POST http://localhost:4000/dashboard/api/admin/keys/batch \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "keys": [
      {"key_alias": "alice", "plan_name": "pro"},
      {"key_alias": "bob",   "plan_name": "basic"},
      {"key_alias": "carol", "metadata": {"vip": true}}
    ]
  }'
```

Key 的关键字段（litellm 兼容）：

| 字段 | 语义 |
|------|------|
| `key_alias` | 人类可读别名 |
| `models` | 允许的模型白名单。`[]` 或含 `"all-team-models"` 表示全权限 |
| `team_id` | 所属团队，团队 `models` 作为白名单兜底 |
| `metadata.vip` | `true` 时流控优先调度 |
| `max_parallel_requests` | 该 key 的并发上限 |
| `rpm_limit` / `tpm_limit` | 该 key 自身的 RPM/TPM（优先级高于 plan） |
| `blocked` | `true` 立即拉黑 |

### 5.6 分配套餐

`key_hash` 是 API key 的 SHA-256 哈希（创建 key 时响应里会返回 `token_hash`），也可自己算：

```bash
# 计算 key_hash（去掉 sk- 前缀的原始 key 做 SHA-256）
echo -n "sk-the-actual-key-string" | sha256sum
```

```bash
# 给 key 分配套餐
curl -X POST http://localhost:4000/admin/plans/assign \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_hash": "<sha256-of-key>", "plan_name": "pro"}'

# 解除分配
curl -X DELETE http://localhost:4000/admin/plans/assign/<key_hash> \
  -H "Authorization: Bearer $MASTER_KEY"

# 查看全部分配
curl http://localhost:4000/admin/plans/assignments \
  -H "Authorization: Bearer $MASTER_KEY"
```

### 5.7 健康检查与就绪探针

```bash
# 完整状态（含 version/uptime/reload_count/db_connected/models_count）
curl http://localhost:4000/health

# 存活探针（进程存活即 200）
curl http://localhost:4000/health/live

# 就绪探针（DB 已连或未配 DB 时 200；配了 DB 但连不上时 503）
curl http://localhost:4000/health/ready
```

Kubernetes / 负载均衡建议：

- livenessProbe → `/health/live`
- readinessProbe → `/health/ready`
- startupProbe → `/health`

### 5.8 重置限流窗口

某 key 触发限流后需要紧急放行：

```bash
curl -X POST http://localhost:4000/dashboard/api/admin/limits/reset \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_hash": "<sha256>"}'
```

### 5.9 排查上游异常

1. Dashboard → Debug → 开启"Debug"开关（只开启错误录制；如需完整原始交换需单独开启 `prompt_log.capture_raw_upstream`）。
2. 复现问题，记录失败请求的 `request_id`。
3. Dashboard → Logs 按 `request_id` 查日志，或：
   ```bash
   curl http://localhost:4000/dashboard/api/admin/debug/errors/<request_id> \
     -H "Authorization: Bearer $MASTER_KEY"
   ```
4. 拿到上游原始响应体定位根因。
5. 排查完关闭 Debug，避免长期捕获增加开销。

### 5.10 日志与监控

- 进程日志输出到 stdout，tracing JSON 格式，由 Docker 日志驱动或 systemd journal 收集。
- 每 60 秒打印一次"过去一分钟请求数"摘要。
- 每 10 分钟把内存中的限流计数器与 key-plan 分配快照写回 DB（崩溃恢复用）。
- 后台周期任务（FC 派发每 1s、健康检查每 30/60s）的启停都会在日志中记录。

### 5.11 优雅退出

- `Ctrl+C` 或 `SIGTERM` 触发优雅退出：
  1. 通知所有后台任务停止。
  2. 关闭 DB 连接池，释放连接与表锁。
- 不要用 `SIGKILL` 强杀，会丢失未落盘的限流计数器（最多丢 10 分钟窗口）。

### 5.12 配置快照导出与回写

业务配置的 YAML 快照是**手动操作**（回滚种子 / 灾备工件），在 Dashboard Config 页：

- **导出**（GET `/dashboard/api/admin/config/export`）：DB 全量业务配置 + 原始 YAML 未触碰 section（`${VAR}` 引用保留）→ 浏览器下载，不落盘。
- **回写**（POST `/dashboard/api/admin/config/writeback`）：同一管道写回启动文件（先滚动 `.bak` 单槽备份）；env bootstrap 模式下在 cwd 生成新 `config.yaml`（0600，secrets 写成 `${BOOM_MASTER_KEY}` / `${BOOM_DATABASE_URL}` / `${BOOM_DASHBOARD_SESSION_SECRET}` 引用），重启即切换为文件模式。

```bash
# 回写也可用 curl
curl -X POST http://localhost:4000/dashboard/api/admin/config/writeback \
  -H "Cookie: boom_session=$JWT"
```

注意：

- 文件**含明文 secrets**（api_key、master_key），权限 0600，视同备份工件妥善保管。
- 回写完的文件随即开始过期（业务配置继续在 DB 演进）——它不是同步机制。
- **升级 / 回退二进制版本前建议先回写一次**：新版本回退老版本时，老二进制按 YAML 权威构建，需要新鲜快照。
- 灾备恢复：空库 + 快照文件重启 → 自动重新 seed 全部配置。

### 5.13 常见问题速查

| 现象 | 可能原因 | 处理 |
|------|----------|------|
| 启动后 key 鉴权全失败 | `database_url` 未配或连不上 | 检查 DB 与连接串 |
| 改了 YAML 进程段不生效 | 未触发热加载 | `kill -HUP` 或调 reload API |
| 改了 YAML 业务段（模型/套餐等）不生效 | DB 权威：业务段仅首次 seed，之后 inert | 走 Dashboard CRUD 修改 |
| 同名多 deployment 不均衡 | 上游自身限流或 `key_affinity` 亲和导致 | 切 `round_robin` 验证 |
| 流控不生效 | 缺 `model_info.id` | 给 deployment 配 id |
| `--reboot` 找不到老进程 | 二进制名不是 `boom-gateway` | `pgrep -x boom-gateway` 验证 |
| 启动后 prompt 日志目录报错 | `dir` 路径无写权限 | 调整目录权限或换路径 |
