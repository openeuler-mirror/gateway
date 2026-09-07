# 01 接入与认证（Access & Auth）

覆盖：key 获取、认证鉴权、key 前缀、面板登录、权限隔离。

| ID | 标题 | 角色 | 深度 |
|----|------|------|------|
| ACC-01 | 获取 API key 并完成首次调用 | user | smoke+trial |
| ACC-02 | 无 key 请求返回 401 | none | smoke+trial |
| ACC-03 | 错误 key 请求返回 401 | none | smoke+trial |
| ACC-04 | 创建带前缀的 API key | admin | trial |
| ACC-05 | key 前缀格式校验 | admin | trial |
| ACC-06 | master key 以管理员身份登录面板 | admin | smoke+trial |
| ACC-07 | API key 以普通用户身份登录面板 | user | trial |
| ACC-08 | 普通用户无法访问管理页面 | user | trial |
| ACC-09 | 被封禁的 key 调用返回 403 | user | trial |
| ACC-10 | key 只能调用被授权的模型 | user | trial |

---

## ACC-01 获取 API key 并完成首次调用

- **角色**：user　**深度**：smoke+trial
- **前置**：管理员已创建一个授权了某可用模型的 API key，并将明文 key 交付给试用者；网关正常运行（`/health/live` 返回 200）。
- **步骤**：
  ```bash
  curl http://<gateway-host>:4000/v1/chat/completions \
    -H "Authorization: Bearer sk-<你的key>" \
    -H "Content-Type: application/json" \
    -d '{"model": "<授权模型名>", "messages": [{"role": "user", "content": "Hello!"}]}'
  ```
- **预期**：HTTP 200，响应体为标准 OpenAI 格式（含 `choices` 与 `usage` 字段），`usage.prompt_tokens` > 0。

## ACC-02 无 key 请求返回 401

- **角色**：none　**深度**：smoke+trial
- **前置**：网关正常运行。
- **步骤**：同 ACC-01 的请求，但不携带 `Authorization` 头。
- **预期**：HTTP 401，错误体为 OpenAI 错误格式，`error.type` 为认证类错误（如 `authentication_error`），错误信息提示缺少凭证而非内部堆栈。

## ACC-03 错误 key 请求返回 401

- **角色**：none　**深度**：smoke+trial
- **前置**：网关正常运行。
- **步骤**：同 ACC-01，但使用一个随机伪造的 key（如 `sk-invalid-000000`）。
- **预期**：HTTP 401，错误格式同 ACC-02。注意网关不应区分"key 不存在"与"key 格式错误"的提示（避免枚举探测）。

## ACC-04 创建带前缀的 API key

- **角色**：admin　**深度**：trial
- **前置**：以管理员身份登录面板。
- **步骤**：
  1. 面板 → Keys → 创建密钥，填写 `key_alias`（如 `trial-alice`）与 key 前缀（如 `Alice`）
  2. 保存后立即复制返回的明文 key（**只显示一次**）
  3. 用该 key 执行 ACC-01 的调用
- **预期**：
  1. 明文 key 形如 `sk-Alice-<32位十六进制>`，前缀出现在第二段
  2. 调用成功（200），该 key 功能与无前缀 key 完全一致
  3. 面板密钥列表中该 key 显示前缀 `Alice`，可按前缀识别归属

## ACC-05 key 前缀格式校验

- **角色**：admin　**深度**：trial
- **前置**：以管理员身份登录面板。
- **步骤**：分别尝试用以下前缀创建 key：`a`（1 字符）、`TeamA`（含大写）、`abc123XYZ`（混合）、`team-a`（含连字符）、`team_a`（含下划线）、空、`a`×51（超长）。
- **预期**：前三者创建成功；后四者被拒绝并提示"前缀须为 1-50 位 ASCII 字母数字"。合法与非法用例应在单条、批量、导入三种创建入口表现一致。

## ACC-06 master key 以管理员身份登录面板

- **角色**：admin　**深度**：smoke+trial
- **前置**：网关启动配置中的 `master_key`。
- **步骤**：浏览器打开 `http://<gateway-host>:4000/dashboard`，用 master key 登录。
- **预期**：登录成功，可见管理页面（Models / Plans / Keys / Logs / Stats 等全部模块）。

## ACC-07 API key 以普通用户身份登录面板

- **角色**：user　**深度**：trial
- **前置**：持有 ACC-01 的 API key。
- **步骤**：打开 dashboard 登录页，用该 API key 登录。
- **预期**：登录成功，仅可见用户面板（套餐信息、用量、个人日志），不出现管理入口。

## ACC-08 普通用户无法访问管理页面

- **角色**：user　**深度**：trial
- **前置**：已以 API key 登录面板（ACC-07 通过）。
- **步骤**：直接在地址栏访问任一管理 API（如 `/dashboard/api/admin/keys`）。
- **预期**：返回 403，且用户面板数据不受影响。管理数据不因该请求泄露。

## ACC-09 被封禁的 key 调用返回 403

- **角色**：user　**深度**：trial
- **前置**：管理员已创建 key 并交付试用者；试用者先用该 key 成功调用一次（确认可用）。
- **步骤**：
  1. 通知管理员在面板封禁该 key
  2. 试用者立刻用同一 key 再次调用
  3. 通知管理员解封，再次调用
- **预期**：封禁后立即 403（无需等待、无需重启）；解封后调用恢复 200。

## ACC-10 key 只能调用被授权的模型

- **角色**：user　**深度**：trial
- **前置**：key A 只授权模型 `model-a`；网关还配置了 `model-b`（对 A 不可见）。
- **步骤**：
  1. 用 key A 调用 `model-a`
  2. 用 key A 调用 `model-b`
  3. 用 key A 请求 `GET /v1/models`
- **预期**：
  1. `model-a` 调用成功
  2. `model-b` 被拒绝（403 `model_not_allowed`），错误信息明确指出无权访问该模型
  3. `/v1/models` 列表中**不出现** `model-b`
