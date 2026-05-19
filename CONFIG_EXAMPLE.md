# BooMGateway 配置示例

配置文件为 YAML 格式，兼容 litellm 的 `proxy_server_config.yaml`。所有顶级字段均可选，有合理默认值。

---

## 最小配置

```yaml
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_key: sk-your-openai-key
```

只需一个模型即可启动。

---

## 完整配置示例

```yaml
# ──────────────────────────────────────────────
# 服务器设置
# ──────────────────────────────────────────────
server:
  host: "0.0.0.0"        # 默认 0.0.0.0
  port: 4000             # 默认 4000
  workers: 4             # 默认 4

# ──────────────────────────────────────────────
# 通用设置
# ──────────────────────────────────────────────
general_settings:
  master_key: ${MASTER_KEY}                  # 支持 ${ENV_VAR} 引用
  database_url: os.environ/DATABASE_URL      # 支持 os.environ/VAR 引用
  store_model_in_db: false                   # true 时 DB 为权威数据源，YAML 仅首次 seed

# ──────────────────────────────────────────────
# 模型部署列表
# ──────────────────────────────────────────────
model_list:
  # OpenAI
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_key: ${OPENAI_API_KEY}
      rpm: 60                     # 可选：部署级 RPM 限制
      tpm: 100000                 # 可选：部署级 TPM 限制
      timeout: 1200               # 默认 1200 秒

  # 同名多部署 → 自动负载均衡（轮询）
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_key: ${OPENAI_API_KEY_BACKUP}
      api_base: https://api.openai.com/v1

  # Anthropic — 带 model_info
  - model_name: claude-sonnet-4-20250514
    model_info:
      id: node-a                            # 可选：部署标识
      input_cost_per_token: 0.000003        # 可选：输入成本
      output_cost_per_token: 0.000015       # 可选：输出成本
      quota_count_ratio: 3                  # 可选：配额消耗倍率（默认 1）
    litellm_params:
      model: anthropic/claude-sonnet-4-20250514
      api_key: ${ANTHROPIC_API_KEY}
      temperature: 0.7                      # 可选：温度覆盖
      max_tokens: 4096                      # 可选：最大输出 token

  # Azure OpenAI
  - model_name: my-gpt4
    litellm_params:
      model: azure/my-deployment-name
      api_key: ${AZURE_API_KEY}
      api_base: https://my-resource.openai.azure.com
      api_version: "2024-06-01"

  # Google Gemini
  - model_name: gemini-2.0-flash
    litellm_params:
      model: gemini/gemini-2.0-flash
      api_key: ${GOOGLE_API_KEY}

  # AWS Bedrock
  - model_name: bedrock-claude
    litellm_params:
      model: bedrock/anthropic.claude-3-sonnet
      aws_region_name: us-east-1
      aws_access_key_id: ${AWS_ACCESS_KEY_ID}
      aws_secret_access_key: ${AWS_SECRET_ACCESS_KEY}

  # vLLM / Ollama 等自部署服务（OpenAI 兼容）
  - model_name: my-llama
    litellm_params:
      model: hosted_vllm/my-model
      api_base: http://localhost:8000/v1
      # api_key 不需要时可省略

  - model_name: local-qwen
    litellm_params:
      model: ollama/qwen2.5:72b
      api_base: http://localhost:11434/v1

  # DeepSeek
  - model_name: deepseek-chat
    litellm_params:
      model: deepseek/deepseek-chat
      api_key: ${DEEPSEEK_API_KEY}

  # 通配部署：匹配所有未配置的模型名
  - model_name: "*"
    litellm_params:
      model: openai/*
      api_key: ${OPENAI_API_KEY}

  # 自定义请求头
  - model_name: custom-provider
    litellm_params:
      model: openai/custom-model
      api_key: ${CUSTOM_API_KEY}
      api_base: https://custom.api.com/v1
      headers:
        X-Custom-Header: custom-value

# ──────────────────────────────────────────────
# 路由设置
# ──────────────────────────────────────────────
router_settings:
  routing_strategy: round_robin     # 默认 round_robin，可选 key_affinity

  # 模型别名：客户端用别名请求，实际路由到目标模型
  model_group_alias:
    # 简单别名
    "gpt-4": "gpt-4o"

    # 扩展格式（可设置 hidden: true 使其不在 /v1/models 中展示）
    "GPT-4o":
      model: "gpt-4o"
      hidden: true

    "claude": "claude-sonnet-4-20250514"

  # key_affinity 策略参数（仅 routing_strategy: key_affinity 时生效）
  key_affinity_context_threshold: 0     # 上下文字符数阈值，低于此值优先最低负载
  key_affinity_rebalance_threshold: 10  # 再均衡阈值（请求数差异超过此值时重新分配）

# ──────────────────────────────────────────────
# 全局限流设置
# ──────────────────────────────────────────────
rate_limit:
  enabled: true                     # 默认 true
  default_rpm: 60                   # 无套餐的 key 的默认 RPM，默认 60
  window_limits:                    # 自定义时间窗口 [[count, seconds], ...]
    - [100, 18000]                  # 100 次请求 / 5 小时

# ──────────────────────────────────────────────
# 套餐设置
# ──────────────────────────────────────────────
plan_settings:
  default_plan: "basic"             # 可选：未分配套餐的 key 使用的默认套餐

  plans:
    basic:
      concurrency_limit: 4          # 可选：最大并发请求数
      rpm_limit: 60                 # 可选：每分钟请求数
      window_limits:                # 可选：自定义时间窗口
        - [100, 18000]              # 100 次 / 5 小时

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
        - [500, 3600]               # 500 次 / 1 小时

    # 支持时段调度的套餐
    scheduled-plan:
      concurrency_limit: 4
      rpm_limit: 40
      window_limits:
        - [80, 18000]
      schedule:
        - hours: "9:00-21:00"       # 高峰期（9am - 9pm）
          concurrency_limit: 4
          rpm_limit: 40
          window_limits:
            - [80, 18000]
        - hours: "21:00-9:00"       # 低峰期（9pm - 次日 9am，跨午夜）
          concurrency_limit: 8
          rpm_limit: 80
          window_limits:
            - [160, 18000]
```

---

## 配置字段参考

### server

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `host` | string | `"0.0.0.0"` | 绑定地址 |
| `port` | u16 | `4000` | 绑定端口 |
| `workers` | usize | `4` | 工作线程数 |

### general_settings

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `master_key` | string | null | 管理 API 密钥，支持环境变量 |
| `database_url` | string | null | PostgreSQL 连接串（litellm DB） |
| `store_model_in_db` | bool | `false` | DB 权威模式。true 时 DB 为模型/别名/套餐的数据源，YAML 仅首次 seed |

### model_list[]

| 字段 | 类型 | 说明 |
|---|---|---|
| `model_name` | string | 对外暴露的模型名（客户端请求用） |
| `litellm_params.model` | string | 上游模型标识，`provider/model-id` 格式 |
| `litellm_params.api_key` | string | API 密钥（支持环境变量） |
| `litellm_params.api_base` | string | API 基础 URL |
| `litellm_params.api_version` | string | Azure API 版本 |
| `litellm_params.aws_region_name` | string | AWS 区域 |
| `litellm_params.aws_access_key_id` | string | AWS Access Key |
| `litellm_params.aws_secret_access_key` | string | AWS Secret Key |
| `litellm_params.rpm` | u64 | 部署级 RPM |
| `litellm_params.tpm` | u64 | 部署级 TPM |
| `litellm_params.timeout` | u64 | 请求超时（秒），默认 1200 |
| `litellm_params.headers` | map | 自定义请求头 |
| `litellm_params.temperature` | f64 | 温度覆盖 |
| `litellm_params.max_tokens` | u32 | 最大 token 数覆盖 |

### model_list[].model_info

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `id` | string | null | 部署标识 |
| `input_cost_per_token` | f64 | null | 输入成本（每 token） |
| `output_cost_per_token` | f64 | null | 输出成本（每 token） |
| `quota_count_ratio` | u64 | `1` | 配额消耗倍率。每次请求消耗的配额数，例如设为 3 则一次请求消耗 3 个配额 |

### router_settings

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `routing_strategy` | string | `"round_robin"` | 路由策略：`round_robin` 或 `key_affinity` |
| `model_group_alias` | map | {} | 模型别名映射 |
| `key_affinity_context_threshold` | u64 | `0` | key_affinity 模式下，上下文字符数低于此值时优先最低负载 |
| `key_affinity_rebalance_threshold` | u64 | `10` | key_affinity 模式下，再均衡阈值（请求数差异） |

### rate_limit

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `enabled` | bool | `true` | 是否启全局限流 |
| `default_rpm` | u64 | `60` | 无套餐 key 的默认 RPM |
| `window_limits` | [[u64,u64]] | [] | 自定义窗口 [[次数, 秒数], ...] |

### plan_settings

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `default_plan` | string | null | 默认套餐名（必须在 plans 中存在） |
| `plans` | map | {} | 套餐定义 |

### plan_settings.plans.* (套餐定义)

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `concurrency_limit` | u32 | null | 最大并发请求数 |
| `rpm_limit` | u64 | null | 每分钟请求数 |
| `window_limits` | [[u64,u64]] | [] | 自定义窗口 [[次数, 秒数], ...] |
| `schedule` | [] | [] | 时段调度列表 |

### plan_settings.plans.*.schedule[] (时段定义)

| 字段 | 类型 | 说明 |
|---|---|---|
| `hours` | string | 时间范围 `"H:MM-H:MM"`，支持跨午夜（如 `"21:00-9:00"`） |
| `concurrency_limit` | u32 | 时段并发限制 |
| `rpm_limit` | u64 | 时段 RPM |
| `window_limits` | [[u64,u64]] | 时段自定义窗口 |

---

## 支持的提供商前缀

| 前缀 | 上游服务 |
|---|---|
| `openai/` | OpenAI API |
| `anthropic/` | Anthropic Messages API |
| `azure/` | Azure OpenAI |
| `gemini/` | Google Gemini |
| `bedrock/` | AWS Bedrock |
| `hosted_vllm/` | vLLM (OpenAI 兼容) |
| `vllm/` | vLLM |
| `ollama/`, `ollama_chat/` | Ollama |
| `deepseek/` | DeepSeek |
| `groq/` | Groq |
| `together_ai/` | Together AI |
| `fireworks_ai/` | Fireworks AI |
| `perplexity/` | Perplexity |
| `deepinfra/` | DeepInfra |
| `sambanova/` | SambaNova |
| `cerebras/` | Cerebras |
| `nvidia_nim/` | NVIDIA NIM |
| `volcengine/` | 火山引擎 |
| `dashscope/` | 阿里灵积 |
| `moonshot/` | Moonshot (Kimi) |
| `xai/` | xAI (Grok) |
| `ai21/`, `ai21_chat/` | AI21 Labs |

无前缀时自动按模型名检测：`gpt-*` / `o1-*` / `o3-*` / `o4-*` → OpenAI, `claude-*` → Anthropic, `gemini-*` → Gemini。

---

## 环境变量

支持两种语法：

```yaml
api_key: ${OPENAI_API_KEY}
database_url: os.environ/DATABASE_URL
```

环境变量不存在时保留原文本不替换。

---

## 热重载

修改配置文件后，两种方式触发热重载：

1. **信号**：`kill -HUP <pid>`（仅 Unix）
2. **API**：`POST /admin/config/reload`（需 master key）

热重载行为：
- 重新读取 YAML 配置
- 重建提供商（HTTP client，开销低）
- 复用 DB 连接池和限流计数器（无状态丢失）
- 原子交换内部状态（ArcSwap），零停机
- 套餐从配置重新加载（`plan_store` 跨 reload 存活）
