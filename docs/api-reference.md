# Boom Gateway API 速查手册

> 生成时间：2026-08-12 · 适用版本：仓库 master 分支当前提交
> 路由源码：`boom-main/src/main.rs::build_router`、`boom-dashboard/src/lib.rs::build_router`

## 0. 速查总览

### 0.1 三类路由与认证方式

| 路由前缀 | 认证方式 | 用途 |
|----------|---------|------|
| `/v1/*`、`/chat/*`、`/completions`、`/models` | API key（`Authorization: Bearer`、`x-api-key`、`api-key` 三选一） | LLM 代理（OpenAI/Anthropic 兼容） |
| `/admin/*`（boom-main 直挂） | API key，且必须等于 master_key | 旧式计划管理与配置热加载（兼容 curl） |
| `/dashboard/api/auth/*` | 无（登录端点） | 账号密码登录 |
| `/dashboard/api/user/*` | JWT cookie（role=user 或 admin） | 用户自助（仅本人 key） |
| `/dashboard/api/admin/*` | JWT cookie（必须 role=admin） | 控制面：Keys/Plans/Teams/Models/Logs/Stats/Quota/Config |
| `/health*`、`/internal/*`、`/dashboard`、`/dashboard/*` 静态资源 | 无 | 健康检查/内部调试/前端 |

### 0.2 环境变量约定

下文示例统一用以下变量，复制后只需替换即可：

```bash
BASE=http://localhost:8080            # 网关地址
MASTER=sk-master-xxxxxx               # config.yaml: general_settings.master_key
APIKEY=sk-xxxxxxxxxxxxxxxx            # 普通用户 key（创建后获得，明文，仅展示一次）
KEYHASH=64位hex                       # SHA-256(APIKEY)，DB 主键
JWT=eyJ...                            # /dashboard/api/auth/login 返回的 cookie 值
```

获取 `JWT`（即 cookie 值）：

```bash
curl -i -X POST $BASE/dashboard/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"admin","api_key":"'"$MASTER"'"}' \
  | grep -i set-cookie | sed 's/.*boom_session=//;s/;.*//'
```

`JWT` 在后续所有 `/dashboard/api/admin/*` 与 `/dashboard/api/user/*` 请求中作为 `Cookie: boom_session=$JWT` 携带。有效期 2 小时。

### 0.3 通用响应约定

- 成功：HTTP 2xx，body 为 JSON。
- 失败：HTTP 4xx/5xx，body 形如 `{"error":"..."}` 或纯文本。
- 限流：HTTP 429，可能带 `retry-after` 字段。
- 鉴权失败：HTTP 401（无 key/无效 key）或 403（已登录但权限不足）。
- 登录失败累计 5 次后锁 IP：首次锁 10s，每次再失败加 30s。

---

## 1. LLM 代理（OpenAI / Anthropic 兼容）

### 1.1 Chat Completions（OpenAI 格式）

**POST** `/v1/chat/completions`（别名 `/chat/completions`） · 认证：API key

请求体即 OpenAI 标准格式，网关会按 `model` 路由到对应 deployment。支持流式（`stream:true`）。

```bash
curl -X POST $BASE/v1/chat/completions \
  -H "Authorization: Bearer $APIKEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [
      {"role":"system","content":"你是助手"},
      {"role":"user","content":"用一句话介绍 Rust"}
    ],
    "temperature": 0.7,
    "stream": false
  }'
```

流式：

```bash
curl -N -X POST $BASE/v1/chat/completions \
  -H "Authorization: Bearer $APIKEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true}'
```

可选字段：`max_tokens`、`max_completion_tokens`、`tools`、`tool_choice`、`response_format`、`temperature`、`top_p`、`frequency_penalty`、`presence_penalty`、`seed`、`stop`、`n`、`logprobs`、`top_logprobs`、`logit_bias`、`user`。其他字段会被 catch-all 接受但不转发（如 `service_tier`、`store`）。

### 1.2 Completions（OpenAI 旧式）

**POST** `/v1/completions`（别名 `/completions`） · 认证：API key

内部转换：`prompt` 包成一条 user message 后走 chat completions。

```bash
curl -X POST $BASE/v1/completions \
  -H "Authorization: Bearer $APIKEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-3.5-turbo-instruct","prompt":"Once upon a time","max_tokens":50}'
```

### 1.3 Messages（Anthropic 格式）

**POST** `/v1/messages` · 认证：API key（`x-api-key` 或 `Authorization: Bearer`）

Anthropic 原生协议；网关内部转 OpenAI 调上游，再转回 Anthropic 格式（流式也走 `AnthropicStreamTranscoder`）。

```bash
curl -X POST $BASE/v1/messages \
  -H "x-api-key: $APIKEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-3-7-sonnet",
    "max_tokens": 1024,
    "system": "你是助手",
    "messages": [{"role":"user","content":"hi"}]
  }'
```

字段：`model`、`system`、`messages`、`max_tokens`、`tools`、`tool_choice`、`thinking`、`temperature`、`top_p`、`stop_sequences`、`stream`、`metadata`。content 支持文本块、`tool_use`/`tool_result`、`thinking`、`image`、`document` 等。

### 1.4 List Models

**GET** `/v1/models`（别名 `/models`） · 认证：API key

返回当前 key 可见的全部 model_name（不含 `"*"` 通配，不含 hidden alias）。受限 key 仅看到 `models` 数组内 + `public_models`。

```bash
curl $BASE/v1/models -H "Authorization: Bearer $APIKEY"
```

返回：`{"object":"list","data":[{"id":"...","object":"model","created":0,"owned_by":"boom-gateway"}]}`

### 1.5 Get Model

**GET** `/v1/models/{id}` · 认证：API key

```bash
curl $BASE/v1/models/gpt-4o -H "Authorization: Bearer $APIKEY"
```

### 1.6 未实现的端点（一律返回 NotSupported 错误）

| 方法 | 路径 |
|------|------|
| POST | `/v1/embeddings` |
| POST | `/v1/audio/speech` |
| POST | `/v1/audio/transcriptions` |
| POST | `/v1/moderations` |

---

## 2. 健康检查与内部端点（无认证）

### 2.1 Health

**GET** `/health` — 综合：版本、uptime、DB 连通、模型数、reload 数。

```bash
curl $BASE/health
```

### 2.2 Liveness

**GET** `/health/live` — 返回字符串 `"ok"`。

### 2.3 Readiness

**GET** `/health/ready` — DB 通则 200，否则 503。

### 2.4 KV Index 调试

**GET** `/internal/kv-index` — 查询 kvc_aware 路由器内部 trie 状态、block 数、model 列表、当前生效配置。

```bash
curl $BASE/internal/kv-index | jq .
```

---

## 3. Master-Key 直挂管理接口（/admin/*）

> 路由定义：`boom-main/src/main.rs::build_router` 的 `admin_routes`。
> 认证：`Authorization: Bearer $MASTER`（master_key 本身即 API key）。仅在 boom-main 顶层路由上提供，**不**经 dashboard JWT。

### 3.1 配置热加载

**POST** `/admin/config/reload` — 重新读取 `config.yaml`，原子交换运行状态。SIGHUP 等价。

```bash
curl -X POST $BASE/admin/config/reload -H "Authorization: Bearer $MASTER"
```

### 3.2 Plan 列表 / Upsert / 删除

**GET** `/admin/plans` — 列出所有 rate-limit plan。

**PUT** `/admin/plans` — 创建或覆盖 plan。body 即 `RateLimitPlan`。

**DELETE** `/admin/plans/{name}` — 删除 plan（清理 key 指派）。

```bash
# 列表
curl $BASE/admin/plans -H "Authorization: Bearer $MASTER"

# 创建/覆盖
curl -X PUT $BASE/admin/plans -H "Authorization: Bearer $MASTER" \
  -H "Content-Type: application/json" \
  -d '{
    "name":"pro",
    "type":"key",
    "concurrency_limit": 4,
    "rpm_limit": 60,
    "tpm_limit": 60000,
    "window_limits": [
      [60, null, null, 60],
      [null, 100000, null, 3600]
    ],
    "total_token_limit": 1000000,
    "total_cost_limit": "10.00"
  }'

# 删除
curl -X DELETE $BASE/admin/plans/pro -H "Authorization: Bearer $MASTER"
```

`window_limits` 支持紧凑数组 `[counts, tokens, costs, window_secs]` 或对象形式。`type` 可选 `key`/`team`；`team` 类型需带 `member_plan`。

### 3.3 Key ↔ Plan 指派

**POST** `/admin/plans/assign` — `{key_hash, plan_name}`

**DELETE** `/admin/plans/assign/{key_hash}` — 取消指派（回落到 default_plan）

**GET** `/admin/plans/assignments` — 全部指派列表

```bash
curl -X POST $BASE/admin/plans/assign \
  -H "Authorization: Bearer $MASTER" \
  -H "Content-Type: application/json" \
  -d '{"key_hash":"'"$KEYHASH"'","plan_name":"pro"}'

curl $BASE/admin/plans/assignments -H "Authorization: Bearer $MASTER"

curl -X DELETE $BASE/admin/plans/assign/$KEYHASH -H "Authorization: Bearer $MASTER"
```

---

## 4. Dashboard 认证（JWT Cookie）

### 4.1 Login

**POST** `/dashboard/api/auth/login` — body：`{user_id, api_key}`

- `user_id="admin"` + `api_key=master_key` → 管理员登录
- `user_id=任意` + `api_key=用户 key` → 普通用户登录（DB 比对 token_hash）

```bash
# 管理员
curl -i -X POST $BASE/dashboard/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"admin","api_key":"'"$MASTER"'"}'
# 响应头：Set-Cookie: boom_session=<JWT>; HttpOnly; SameSite=Lax; Path=/dashboard; Max-Age=7200
# 响应体：{"role":"admin","user_id":"admin"}

# 普通用户
curl -i -X POST $BASE/dashboard/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"alice","api_key":"'"$APIKEY"'"}'
# 响应体：{"role":"user","user_id":"<alias>","api_key":"<原 key>"}
```

### 4.2 Me（当前会话）

**GET** `/dashboard/api/auth/me` · 需登录

```bash
curl $BASE/dashboard/api/auth/me -H "Cookie: boom_session=$JWT"
# {"user_id":"admin","role":"admin"}
```

### 4.3 Logout

**POST** `/dashboard/api/auth/logout` — 清 cookie。

```bash
curl -X POST $BASE/dashboard/api/auth/logout
```

---

## 5. 用户自助端点（/dashboard/api/user/*）

> 认证：JWT cookie，role=user 或 admin。所有端点自动绑定当前会话的 `key_hash`，无权访问他人数据。

### 5.1 Plan（本人生效的 plan）

**GET** `/dashboard/api/user/plan`

```bash
curl $BASE/dashboard/api/user/plan -H "Cookie: boom_session=$JWT"
```

返回 `plan_name`、`concurrency_limit`、`rpm_limit`、`window_limits`（数组形式 `[counts, tokens, costs, window_secs]`）、`schedule`。三态：未指派→回落 default；显式 no-plan→`is_explicit_no_plan:true`。

### 5.2 Usage（实时用量 + 累计）

**GET** `/dashboard/api/user/usage`

返回每个 window 的 current/limit（counts/tokens/costs 三维），以及累计 `cumulative`：`total_input_tokens`、`total_output_tokens`、`total_cost`、各类成本拆分、`total_token_limit`、`total_cost_limit`。

### 5.3 Key Info（本人 key 元数据）

**GET** `/dashboard/api/user/key-info`

```bash
curl $BASE/dashboard/api/user/key-info -H "Cookie: boom_session=$JWT"
```

返回 `key_name`、`key_alias`、`token_prefix`、`spend`/`total_cost`、`expires`、`blocked`、`rpm_limit`、`tpm_limit`、`max_budget`、`budget_duration`、`metadata`、`created_at`、`total_input_tokens`、`total_output_tokens`。

### 5.4 Logs（本人请求日志）

**GET** `/dashboard/api/user/logs?page=1&per_page=50`

Query：`page`（默认 1）、`per_page`（默认 50）。返回 `logs[]`、`page`、`per_page`、`total`。每条含 `model`、`api_path`、`is_stream`、`status_code`、`input_tokens`、`output_tokens`、`duration_ms`、`error_*`、`created_at`、`client_ip`、`cached_tokens`。

### 5.5 Request Status（在途请求）

**GET** `/dashboard/api/user/request-status`

返回当前 key 的在途请求：`model`、`status`（`waiting`/`processing`）、`wait_time_secs`、`is_vip`、`ahead`（waiting 时排队前方数）、`processing_secs`、`parallel_count`（processing 时）。

---

## 6. Admin — Plans

### 6.1 列出

**GET** `/dashboard/api/admin/plans` · admin

```bash
curl $BASE/dashboard/api/admin/plans -H "Cookie: boom_session=$JWT"
```

### 6.2 Upsert

**PUT** `/dashboard/api/admin/plans` · admin · body 即 `UpsertPlanRequest`

```bash
curl -X PUT $BASE/dashboard/api/admin/plans \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{
    "name":"pro",
    "type":"key",
    "concurrency_limit":4,
    "rpm_limit":60,
    "tpm_limit":60000,
    "window_limits":[[60,null,null,60]],
    "total_token_limit":1000000,
    "total_cost_limit":"10.00",
    "schedule":[
      {"start":"09:00","end":"18:00","concurrency_limit":8}
    ]
  }'
```

字段：`name`、`type`（`key`/`team`）、`member_plan`（team 用）、`concurrency_limit`、`rpm_limit`、`tpm_limit`、`window_limits`（三维数组或对象）、`total_token_limit`、`total_cost_limit`（Decimal 字符串）、`schedule`（时间段覆盖，校验时间不重叠）。

### 6.3 删除

**DELETE** `/dashboard/api/admin/plans/{name}` · admin

```bash
curl -X DELETE $BASE/dashboard/api/admin/plans/pro -H "Cookie: boom_session=$JWT"
```

---

## 7. Admin — Keys

### 7.1 列表（含搜索/过滤/分页）

**GET** `/dashboard/api/admin/keys?page=1&per_page=50&search=alice&vip_only=true&plan=pro` · admin

Query：

| 参数 | 含义 |
|------|------|
| `page`、`per_page` | 默认 1 / 50，per_page ≤1000 |
| `search` | 对 `key_name`、`key_alias`、`user_id`、`token` 做 ILIKE 模糊匹配 |
| `vip_only` | `true`/`1` 只返回 metadata.vip=true |
| `plan` | `unassigned`/`none`（无指派）/`no_plan`（显式空）/具体 plan 名 |

返回 `keys[]`（含 `token_prefix`、`token_hash`、`key_alias`、`tag`、`models`、`spend`、`total_cost`、`usage_*`、`plan_name`、`plan_assignment_kind`）、`page`、`per_page`、`total`。全局按 `usage_count` DESC 排序后再分页。

### 7.2 创建单个

**POST** `/dashboard/api/admin/keys` · admin · body：`CreateKeyRequest`

```bash
curl -X POST $BASE/dashboard/api/admin/keys \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{
    "key_alias":"alice-prod",
    "key_name":"Alice Production",
    "key_prefix":"prod",
    "tag":"payments",
    "user_id":"alice",
    "team_id":"team-payments",
    "models":["gpt-4o","claude-3-7-sonnet"],
    "rpm_limit":60,
    "tpm_limit":60000,
    "max_budget":50.0,
    "budget_duration":"30d",
    "expires":"2027-01-01 00:00:00",
    "metadata":{"vip":true},
    "plan_name":"pro"
  }'
```

**响应：`{"key":"sk-prod-xxxxxxxx...","token_hash":"...","key_alias":"..."}` — `key` 明文只此一次。**

字段说明：

| 字段 | 说明 |
|------|------|
| `key_alias` | 别名，唯一 |
| `key_name` | 显示名，缺省取 alias |
| `key_prefix` | 1-8 位 ASCII 字母数字，嵌入 `sk-<prefix>-<secret>` |
| `tag` | 自由文本 ≤64 字符 |
| `models` | 数组，`["all-team-models"]` 视为全权限（即空数组的语义） |
| `expires` | `"YYYY-MM-DD HH:MM:SS"` |
| `metadata` | JSON 对象，`{"vip":true}` 触发 VIP 调度 |
| `plan_name` | 三态：省略→回落 default；`null`→显式无 plan；`"name"`→指派 |

### 7.3 批量创建（JSON 数组）

**POST** `/dashboard/api/admin/keys/batch` · admin · body：`[CreateKeyRequest, ...]`

```bash
curl -X POST $BASE/dashboard/api/admin/keys/batch \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '[
    {"key_alias":"batch-1","plan_name":"pro"},
    {"key_alias":"batch-2","plan_name":null}
  ]'
```

返回 `{created[], skipped[], created_count, skipped_count}`。

### 7.4 文件导入（CSV / JSONL）

**POST** `/dashboard/api/admin/keys/import` · admin · `multipart/form-data`，字段名 `file`，扩展名 `.csv` 或 `.jsonl`，≤1 MiB，≤10000 行。

```bash
# CSV：列序 key_alias,key_name,key_prefix,tag,user_id,team_id,models(rsp 用 | 分隔),rpm_limit,tpm_limit,max_budget,budget_duration,expires,metadata(JSON),plan_name
curl -X POST $BASE/dashboard/api/admin/keys/import \
  -H "Cookie: boom_session=$JWT" \
  -F 'file=@/path/to/keys.csv'
```

响应含 `parsed`、`inserted`、`truncated`、`parse_errors[]`、`created[]`、`skipped[]`、`download`（同名 + `api_key` 列的回写附件，filename `<stem>-with-keys.<ext>`、`content`、`mime`）。

### 7.5 修改

**PUT** `/dashboard/api/admin/keys/{token_hash}` · admin · body：`UpdateKeyRequest`

```bash
curl -X PUT $BASE/dashboard/api/admin/keys/$KEYHASH \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{
    "key_alias":"alice-prod-v2",
    "team_id":"team-payments",
    "models":["gpt-4o"],
    "tag":"beta",
    "rpm_limit":120,
    "metadata":{"vip":false}
  }'
```

字段全部 optional，省略即不变；`team_id:""` 表示从 team 移出。`key_prefix` 不可改。

### 7.6 阻断 / 解封 / 删除

**POST** `/dashboard/api/admin/keys/{token_hash}/block` — 置 `blocked=true`
**POST** `/dashboard/api/admin/keys/{token_hash}/unblock` — 置 `blocked=false`
**DELETE** `/dashboard/api/admin/keys/{token_hash}` — 硬删除：清理 limiter、plan 指派、token 行

```bash
curl -X POST $BASE/dashboard/api/admin/keys/$KEYHASH/block -H "Cookie: boom_session=$JWT"
curl -X DELETE $BASE/dashboard/api/admin/keys/$KEYHASH -H "Cookie: boom_session=$JWT"
```

### 7.7 单 key 用量

**GET** `/dashboard/api/admin/usage/{key_hash}` · admin

返回 `key_hash`、`concurrency`、`windows[]`（`cache_key`、`count`、`window_secs`、`elapsed_secs`）。

---

## 8. Admin — Assignments

### 8.1 Key→Plan 列表（分页）

**GET** `/dashboard/api/admin/assignments?page=1&page_size=20` · admin

Query：`page`（默认 1）、`page_size`（默认 20，上限 200）。

返回 `assignments[]`（`key_hash`、`plan_name`、`key_alias`、`token_prefix`）、`total`、`page`、`page_size`。

### 8.2 指派 Key→Plan

**POST** `/dashboard/api/admin/assignments` · admin · body：`{key_hash, plan_name}`

`plan_name` 三态：省略→400（要求显式 null）；`null`→显式 no-plan；`"name"`→指派。

```bash
curl -X POST $BASE/dashboard/api/admin/assignments \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"key_hash":"'"$KEYHASH"'","plan_name":"pro"}'

# 显式无 plan
curl -X POST $BASE/dashboard/api/admin/assignments \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"key_hash":"'"$KEYHASH"'","plan_name":null}'
```

### 8.3 取消 Key 指派

**DELETE** `/dashboard/api/admin/assignments/{key_hash}` · admin

### 8.4 Team→Plan 指派

**POST** `/dashboard/api/admin/team-assignments` · admin · body：`{team_id, plan_name}`

```bash
curl -X POST $BASE/dashboard/api/admin/team-assignments \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"team_id":"team-payments","plan_name":"team-pro"}'
```

### 8.5 取消 Team 指派

**DELETE** `/dashboard/api/admin/team-assignments/{team_id}` · admin

---

## 9. Admin — Teams

### 9.1 创建

**POST** `/dashboard/api/admin/teams` · admin · body：`{team_id, team_alias?, models?}`

`models` 为空数组或含 `"all-team-models"` 即视为全权限。

```bash
curl -X POST $BASE/dashboard/api/admin/teams \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"team_id":"team-payments","team_alias":"支付组","models":["gpt-4o"]}'
```

### 9.2 修改

**PUT** `/dashboard/api/admin/teams/{team_id}` · admin · body：`{team_alias?, models?}`

### 9.3 删除

**DELETE** `/dashboard/api/admin/teams/{team_id}` · admin · 若 team 下有 key 返回 409

```bash
curl -X DELETE $BASE/dashboard/api/admin/teams/team-payments -H "Cookie: boom_session=$JWT"
```

> 列表端点没有独立 `/admin/teams` GET，统一由 `/admin/quota/overview` 提供（带累计用量）。

---

## 10. Admin — Models（Deployment CRUD）

### 10.1 列表

**GET** `/dashboard/api/admin/models` · admin

```bash
curl $BASE/dashboard/api/admin/models -H "Cookie: boom_session=$JWT"
```

每条含 `id`、`model_name`、`litellm_model`、`api_key`（显示 `****`）、`api_base`、`api_version`、`aws_*`、`rpm`、`tpm`、`timeout`、`headers`、`temperature`、`max_tokens`、`enabled`、`auto_disabled`、`source`（`yaml`/`db`）、`deployment_id`、`quota_count_ratio`、`max_inflight_queue_len`、`max_context_len`、`client_type_header`、`serve_not_match`、`model_info`、`cost_per_million.{input,cached_input,output}`、`created_at`、`updated_at`。

### 10.2 创建

**POST** `/dashboard/api/admin/models` · admin · body：`CreateDeploymentRequest`

```bash
curl -X POST $BASE/dashboard/api/admin/models \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{
    "model_name":"gpt-4o",
    "litellm_model":"openai/gpt-4o",
    "api_key":"sk-upstream-xxxxx",
    "api_base":"https://api.openai.com/v1",
    "rpm":500,
    "tpm":150000,
    "timeout":1200,
    "enabled":true,
    "quota_count_ratio":1,
    "max_inflight_queue_len":100,
    "max_context_len":0,
    "client_type_header":false,
    "serve_not_match":false,
    "model_info":{"cost_template":"openai-gpt-4o"}
  }'
```

字段：`model_name`、`litellm_model`、`api_key?`、`api_key_env?(bool)`、`api_base?`、`api_version?`、`aws_region_name?`、`aws_access_key_id?`、`aws_secret_access_key?`、`rpm?`、`tpm?`、`timeout`（默认 1200）、`headers`（默认 `{}`）、`temperature?`、`max_tokens?`、`enabled`（默认 true）、`deployment_id?`、`quota_count_ratio?`、`max_inflight_queue_len?`、`max_context_len?`、`client_type_header`（默认 false）、`serve_not_match`（默认 false）、`model_info?`。

> 写操作走 AdminCommand channel → boom-main 处理 → 写 DB → `persist_config_in_place` 同步 YAML 并触发热加载。响应可能带 `warning` 字段表示 DB 已写但 reload 失败。

### 10.3 修改

**PUT** `/dashboard/api/admin/models/{id}` · admin · body 同 `CreateDeploymentRequest` · `id` 是 UUID

```bash
curl -X PUT $BASE/dashboard/api/admin/models/$UUID \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"model_name":"gpt-4o","litellm_model":"openai/gpt-4o","api_key":"****","enabled":true,"timeout":1200,"headers":{},"client_type_header":false,"serve_not_match":false}'
```

> `api_key` 等敏感字段填 `"****"` 表示不变；前端检测到 `****` 会清空输入，COALESCE 保留原值。

### 10.4 删除

**DELETE** `/dashboard/api/admin/models/{id}` · admin

---

## 11. Admin — Aliases

**GET** `/dashboard/api/admin/aliases` · admin — 列表（`alias_name`、`target_model`、`hidden`、`source`、`updated_at`）

**POST** `/dashboard/api/admin/aliases` · admin · body：`{alias_name, target_model, hidden?}`

**PUT** `/dashboard/api/admin/aliases/{alias_name}` · admin · body 同上

**DELETE** `/dashboard/api/admin/aliases/{alias_name}` · admin

```bash
# 创建别名：claude-code → gpt-4o
curl -X POST $BASE/dashboard/api/admin/aliases \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"alias_name":"claude-code","target_model":"gpt-4o","hidden":false}'

# 隐藏别名（不在 /v1/models 列出，但仍可路由）
curl -X PUT $BASE/dashboard/api/admin/aliases/claude-code \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"alias_name":"claude-code","target_model":"gpt-4o","hidden":true}'

curl -X DELETE $BASE/dashboard/api/admin/aliases/claude-code -H "Cookie: boom_session=$JWT"
```

---

## 12. Admin — Request Logs

**GET** `/dashboard/api/admin/logs` · admin · 多维过滤 + 分页

Query 全部 optional：

| 参数 | 含义 |
|------|------|
| `page`、`per_page` | 默认 1 / 50 |
| `key_hash` | 精确等值 |
| `model` | ILIKE 模糊 |
| `status` | 仅支持 `error`（status_code ≠ 200） |
| `request_id` | ILIKE 模糊 |
| `key_alias` | 同时匹配 key_alias / key_name |
| `api_path` | ILIKE 模糊 |
| `status_code` | 精确等值（int） |
| `stream` | `yes`/`true`/`1` 或 `no`/`false`/`0` |
| `error` | ILIKE 匹配 error_message |
| `team_alias` | JOIN boom_team_table 后 ILIKE |
| `client_ip` | ILIKE 模糊 |

```bash
curl "$BASE/dashboard/api/admin/logs?page=1&per_page=20&status=error&model=gpt-4o" \
  -H "Cookie: boom_session=$JWT"
```

返回 `logs[]`、`page`、`per_page`、`has_next`（按 `per_page+1` 探测下一页）。每条含 `request_id`、`key_*`、`team_*`、`model`（带 `:deployment_id` 后缀）、`api_path`、`is_stream`、`status_code`、`error_*`、`input_tokens`、`output_tokens`、`cached_tokens`、`duration_ms`、`ttft_ms`、`created_at`、`client_ip`、`policy`（`kvc`/`key`/`rr`/`kvc→key`）、`kv_hit_blocks`、`kv_input_blocks`、`trie_blocks`、`trie_max_blocks`、`request_tokens`。

---

## 13. Admin — Stats & Observability

所有 stats 端点接受统一 Query：`range=1h|4h|8h|24h|custom`（默认 `1h`），或 `from`+`to`（ISO-8601，自定义区间，最长 90 天）。`range=1h` 且无自定义区间时走内存环形缓冲（60 桶 × 1 分钟）；其他走 DB 聚合（10s statement_timeout）。

### 13.1 In-Flight 实时

**GET** `/dashboard/api/admin/stats/inflight` · admin

```bash
curl $BASE/dashboard/api/admin/stats/inflight -H "Cookie: boom_session=$JWT"
```

返回 `deployments[]`：`model`、`deployment_id`、`fc_queue`、`in_reqs`/`in_reqs_max`、`in_context`/`in_context_max`、`queued_keys[]`（`key_alias`、`is_vip`）、`key_stats[]`（`key_alias`、`request_count`、`is_vip`）。

### 13.2 系统压力时序

**GET** `/dashboard/api/admin/stress/timeseries?range=5m` · admin

`range` ∈ `5m`/`15m`/`30m`/`60m`（默认 `5m`）。返回 stressmon 1Hz 环形缓冲快照。

### 13.3 Deployment 24h 汇总

**GET** `/dashboard/api/admin/stats/deployments/summary` · admin（不进自动刷新，按需拉取）

返回 `deployments[]`（`deployment_id`、`total_requests`、`avg_input_tokens`、`avg_output_tokens`、`avg_ttft_ms`、`avg_prefix_hit_rate`）、`window_hours:24`。

### 13.4 Rebalance Move Stats

**GET** `/dashboard/api/admin/stats/rebalance-moves` · admin — 路由器重平衡统计（per-deployment in/out 计数，lifetime）。

### 13.5 Audit Log Drop 计数

**GET** `/dashboard/api/admin/stats/audit-log` · admin — `dropped`（channel full 或 batch 失败累计）、`db_configured`。

### 13.6 Request Rate 时序

**GET** `/dashboard/api/admin/stats/request_rate?range=1h` · admin

返回 `window`（`from`/`to`/`bucket_secs`）、`charts[]`：每条 `model`（含 `ALL` 汇总）+ `deployments[]` 顺序 + `events[]`（`ts`、`count` 或 `segments[{deployment_id,count}]`）。

### 13.7 Agent Stats（客户端类型分布）

**GET** `/dashboard/api/admin/stats/agents?range=1h` · admin

返回 `window`、`events[]`（每桶 `total`、`anthropic`、各 token 拆分）、`summary`（`total`、`anthropic`、`ratio`、各 token 总计与比例）。

---

## 14. Admin — Quota 管理

### 14.1 总览（team 组织）

**GET** `/dashboard/api/admin/quota/overview` · admin

```bash
curl $BASE/dashboard/api/admin/quota/overview -H "Cookie: boom_session=$JWT"
```

返回 `teams[]`（按 total_cost DESC 排序）：`team_id`、`team_alias`、`models`、`key_count`、`plan_name`、`plan_explicit`、`effective_limits`、`prompt_log_excluded`、`total_input_tokens`、`total_output_tokens`、`total_cost_micros`、`total_cost`。`no_team`：未归组的 key 汇总。`default_team_plan`。

### 14.2 Team 内分页 Key

**GET** `/dashboard/api/admin/quota/team/{team_id}?page=1&per_page=50&search=&sort=cost` · admin

Query：`page`、`per_page`（默认 50）、`search`（模糊匹配 alias/name/user_id/token）、`sort`（`cost`/`tokens`/`alias`/`created`，默认 `created`）。

返回 `keys[]`（`token`、`token_prefix`、`key_alias`、`key_name`、`user_id`、`blocked`、`created_at`、`plan_name`、`concurrency`、`total_input_tokens`、`total_output_tokens`、`total_cost_micros`、`total_cost`）、`page`、`per_page`、`total`、`team_id`、`sort_truncated`（>5000 截断标记）。

### 14.3 未分组的 Key

**GET** `/dashboard/api/admin/quota/unassigned?page=1&per_page=50&sort=cost` · admin — 同上，但 team_id 为 null。

### 14.4 单 Key 窗口明细

**GET** `/dashboard/api/admin/quota/key/{key_hash}/windows` · admin

返回 `key_hash`、`windows[]`（`window_secs`、`remaining_secs`、`dims.{counts,tokens,costs}` 各自 `current`/`limit`）、`cumulative`（`total_*_tokens`、`total_cost_micros`、`total_cost`、`total_token_limit`、`total_cost_limit`）。

### 14.5 重置 — Key 全量

**POST** `/dashboard/api/admin/quota/reset/key/{key_hash}` · admin — 清窗口 + 累计（内存 + DB），返回 `previous` 快照。

### 14.6 重置 — Team 全量

**POST** `/dashboard/api/admin/quota/reset/team/{team_id}` · admin — 清 team + 级联所有成员 key。

### 14.7 重置 — Key 仅累计

**POST** `/dashboard/api/admin/quota/reset/key/{key_hash}/cumulative` · admin — 窗口保留，team rollup 同步减去。

### 14.8 重置 — Key 仅窗口

**POST** `/dashboard/api/admin/quota/reset/key/{key_hash}/windows` · admin — 累计保留，清当前窗口（counts + tokens + costs）。

### 14.9 重置 — Team 仅累计

**POST** `/dashboard/api/admin/quota/reset/team/{team_id}/cumulative` · admin

### 14.10 重置 — Team 仅窗口

**POST** `/dashboard/api/admin/quota/reset/team/{team_id}/windows` · admin

```bash
# 完全重置某 key
curl -X POST $BASE/dashboard/api/admin/quota/reset/key/$KEYHASH -H "Cookie: boom_session=$JWT"

# 只清当前窗口
curl -X POST $BASE/dashboard/api/admin/quota/reset/key/$KEYHASH/windows -H "Cookie: boom_session=$JWT"
```

---

## 15. Admin — 速率限制窗口重置（旧式便捷端点）

**POST** `/dashboard/api/admin/limits/reset/{key_hash}` · admin — 清该 key 全部窗口计数器（仅内存 limiter，不动 DB 累计）。

**POST** `/dashboard/api/admin/limits/reset` · admin — 清所有 key 的窗口计数器。

```bash
curl -X POST $BASE/dashboard/api/admin/limits/reset/$KEYHASH -H "Cookie: boom_session=$JWT"
curl -X POST $BASE/dashboard/api/admin/limits/reset -H "Cookie: boom_session=$JWT"
```

> 与 §14 的区别：这俩只清 limiter 多维窗口内存，不动 `boom_rate_limit_cumulative` DB 表与 team rollup。

---

## 16. Admin — Config 读写

### 16.1 读取全量配置

**GET** `/dashboard/api/admin/config` · admin — 返回内存中的 `Config` 序列化，敏感字段（`master_key`、`database_url`、`api_key`、`aws_*_key`）被 `mask_secrets_in_place` 抹成 null。

```bash
curl $BASE/dashboard/api/admin/config -H "Cookie: boom_session=$JWT" | jq .
```

### 16.2 字段 manifest（声明式 UI schema）

**GET** `/dashboard/api/admin/config/schema` · admin — 返回 `model_deployments`、`general_settings`、`router_settings` 三个 FieldMeta 数组（CLAUDE.md §9 单一真相源）。

### 16.3 局部更新（dotted path → 写 YAML → reload）

**PUT** `/dashboard/api/admin/config` · admin · body：`{path, value}`

`path` 为 dotted 路径，如 `router_settings.kvc_aware`、`general_settings.public_models`、`server.port`。`value` 为任意 JSON 可序列化值。会先备份 `config.yaml.bak`，原子写盘后触发热加载。

```bash
# 调整 kvc_aware 配置
curl -X PUT $BASE/dashboard/api/admin/config \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{
    "path":"router_settings.kvc_aware",
    "value":{
      "block_size": 256,
      "max_blocks": 4096,
      "cache_weight": 0.5,
      "load_weight": 0.5,
      "router_ttl_secs": 300,
      "overload_threshold_pct": 90
    }
  }'
```

### 16.4 热加载

**POST** `/dashboard/api/admin/config/reload` · admin — 等价于 SIGHUP / `/admin/config/reload`（旧式），走 AdminCommand channel。

```bash
curl -X POST $BASE/dashboard/api/admin/config/reload -H "Cookie: boom_session=$JWT"
```

---

## 17. Admin — Prompt Log

### 17.1 状态

**GET** `/dashboard/api/admin/prompt-log/status` · admin — `enabled`、`excluded_keys`、`excluded_teams`。

### 17.2 全局开关

**POST** `/dashboard/api/admin/prompt-log/toggle` · admin · body：`{enabled:bool}`

### 17.3 Team 级排除

**POST** `/dashboard/api/admin/prompt-log/team` · admin · body：`{team_id, excluded:bool}`

### 17.4 Key 级排除

**POST** `/dashboard/api/admin/prompt-log/key` · admin · body：`{key_hash, excluded:bool}`

```bash
curl -X POST $BASE/dashboard/api/admin/prompt-log/toggle \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"enabled":true}'

curl -X POST $BASE/dashboard/api/admin/prompt-log/team \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"team_id":"team-payments","excluded":true}'
```

### 17.5 查询单条 Prompt Log

**GET** `/dashboard/api/admin/prompt-log/entry/{request_id}?key_hash=xxx&team_alias=xxx` · admin

返回 `{request, response}` 两阶段 JSONL 条目；只命中单阶段时返回该阶段对象；都没有 404。

### 17.6 OTLP Collector 探测

**POST** `/dashboard/api/admin/prompt-log/otlp-ping` · admin · body：`{endpoint, headers?, timeout_secs?}`

```bash
curl -X POST $BASE/dashboard/api/admin/prompt-log/otlp-ping \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"endpoint":"http://otlp-collector:4318","timeout_secs":5}'
# 成功：{"ok":true,"latency_ms":42}
# 失败：{"ok":false,"error":"..."}
```

---

## 18. Admin — Debug Error 录制

### 18.1 状态

**GET** `/dashboard/api/admin/debug/status` · admin — `enabled`、`entries`（当前条数）。

### 18.2 开关

**POST** `/dashboard/api/admin/debug/toggle` · admin · body：`{enabled:bool}` — 同时切换 prompt-log 的 `capture_raw_upstream`。

```bash
curl -X POST $BASE/dashboard/api/admin/debug/toggle \
  -H "Cookie: boom_session=$JWT" \
  -H "Content-Type: application/json" \
  -d '{"enabled":true}'
```

### 18.3 查询单条 Debug Error

**GET** `/dashboard/api/admin/debug/errors/{request_id}` · admin — 返回 `{debug_error: <entry>}`，未命中 404。

```bash
curl $BASE/dashboard/api/admin/debug/errors/$REQUEST_ID -H "Cookie: boom_session=$JWT"
```

---

## 19. 静态资源与 SPA（无认证）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/` | 302 → `/dashboard` |
| GET | `/dashboard`、`/dashboard/` | 返回 `index.html` |
| GET | `/dashboard/style.css`、`/app.js`、`/i18n.js` | 前端静态资源 |
| GET | `/dashboard/assets/vendor/{name}` | vendor logo（`glm`/`minimax`/`qwen`/`deepseek`/`kimi`/`mimo`，未知名走 `default.svg`），内容类型按 magic bytes 自动识别 PNG/SVG |
| GET | `/dashboard/assets/login.png` | 登录页 hero 图（实际为 webp） |
| GET | `/dashboard/{*path}` | SPA fallback，未匹配路径全部返回 `index.html` |

> 独立 Debug 页面（导航链接 + `/dashboard/debug` 入口）需要 `--features boom-dashboard/debug-tools` 编译；toggle/error 端点本身始终可用。

---

## 20. 速查 · 典型工作流

### 20.1 创建用户 key 并立即拿 JWT

```bash
# 1) admin 登录拿 cookie
JWT=$(curl -i -X POST $BASE/dashboard/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"admin","api_key":"'"$MASTER"'"}' \
  | grep -i set-cookie | sed 's/.*boom_session=//;s/;.*//')

# 2) 创建 key（拿到明文 key，仅此一次）
RESP=$(curl -X POST $BASE/dashboard/api/admin/keys \
  -H "Cookie: boom_session=$JWT" -H "Content-Type: application/json" \
  -d '{"key_alias":"alice","plan_name":"pro","metadata":{"vip":true}}')
APIKEY=$(echo $RESP | jq -r .key)
KEYHASH=$(echo $RESP | jq -r .token_hash)

# 3) 用新 key 走 LLM 代理
curl -X POST $BASE/v1/chat/completions \
  -H "Authorization: Bearer $APIKEY" -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}'

# 4) 用户登录 dashboard
USERJWT=$(curl -i -X POST $BASE/dashboard/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"alice","api_key":"'"$APIKEY"'"}' \
  | grep -i set-cookie | sed 's/.*boom_session=//;s/;.*//')

curl $BASE/dashboard/api/user/plan -H "Cookie: boom_session=$USERJWT"
```

### 20.2 排查 429 / 限流

```bash
# 看某 key 当前窗口状态
curl $BASE/dashboard/api/admin/quota/key/$KEYHASH/windows -H "Cookie: boom_session=$JWT"

# 清当前窗口（保留累计）
curl -X POST $BASE/dashboard/api/admin/quota/reset/key/$KEYHASH/windows -H "Cookie: boom_session=$JWT"

# 完全清零
curl -X POST $BASE/dashboard/api/admin/quota/reset/key/$KEYHASH -H "Cookie: boom_session=$JWT"
```

### 20.3 改路由策略并热加载

```bash
curl -X PUT $BASE/dashboard/api/admin/config \
  -H "Cookie: boom_session=$JWT" -H "Content-Type: application/json" \
  -d '{"path":"router_settings.schedule_policy","value":"kvc_aware"}'
# 内部自动 reload，无需额外触发
```

### 20.4 看最近 1 小时请求分布

```bash
curl "$BASE/dashboard/api/admin/stats/request_rate?range=1h" -H "Cookie: boom_session=$JWT" | jq .
curl "$BASE/dashboard/api/admin/stats/agents?range=1h" -H "Cookie: boom_session=$JWT" | jq .summary
```

---

## 附录 A · 路由 → 源码定位

| 路由集合 | 源文件 | 关键函数 |
|----------|--------|----------|
| `/v1/*`、`/chat/*`、`/completions`、`/models`、`/admin/*`、`/health*`、`/internal/*` | `boom-main/src/main.rs::build_router` + `boom-main/src/routes.rs` | `chat_completions`、`messages`、`completions`、`list_models`、`get_model`、`health_check`、`admin_reload_config`、`admin_upsert_plan` 等 |
| `/dashboard/api/auth/*` | `boom-dashboard/src/auth.rs` | `login`、`logout`、`me` |
| `/dashboard/api/user/*` | `boom-dashboard/src/handlers_user.rs` | `get_plan`、`get_usage`、`get_key_info`、`get_user_logs`、`get_request_status` |
| `/dashboard/api/admin/plans`、`keys*`、`assignments`、`team-assignments`、`models*`、`aliases*`、`logs`、`teams*`、`stats/*`、`limits/*`、`quota/*`、`config*`、`prompt-log/*`、`debug/*` | `boom-dashboard/src/handlers_admin.rs` | 见文件顶部函数表 |
| 静态资源 + SPA fallback | `boom-dashboard/src/handlers_static.rs` | `index`、`style_css`、`app_js`、`i18n_js`、`vendor_logo`、`login_image`、`spa_fallback` |
| 时间窗辅助 | `boom-dashboard/src/stats_timeseries.rs` | `ResolvedRange::parse`、`TimeWindow` |
| 写操作后台执行 | `boom-main/src/admin_command.rs` | `admin_command_handler` |

## 附录 B · 字段命名约定速查

- `token` / `key_hash` / `token_hash` —— 在 DB 中是同一个值：SHA-256 hex（64 位）。
- `key_alias` —— 用户可见别名，唯一约束。
- `key_prefix` —— 1-8 位 ASCII 字母数字，仅创建时可设；不可改。
- `tag` —— 自由文本，≤64 字符，纯展示用。
- `plan_name` —— 在 Key/Team 指派上下文为三态：省略→回落 default；`null`→显式无 plan；`"name"`→指派。
- `models`（key/team 字段）—— 空数组或含 `"all-team-models"` 即视为全权限。**不要把 `"*"` 当全权限**——它是真实的兜底 model_name（路由用）。
- `cost_per_million.{input,cached_input,output}` —— 显示用 per-million；内部按 per-token Decimal 存储。
- `schedule_policy` —— `kvc_aware` / `key_affinity` / `round_robin`（YAML 配置字符串）。
