# 按模型部署配置默认请求参数

原始实现于 2026-10-26，现已移植到上游 master。

## 控制台使用

进入“模型 → 编辑 → 模型参数 → 默认请求参数（JSON）”，填写该部署所需的请求体默认值并保存。保存成功后，同一网关实例后续新请求立即生效，无需重启或点击重新加载；已开始的请求继续使用原 Provider。多个 Worker 共享新的部署状态。

支持 OpenAI Provider 及使用同一 Provider 的 vLLM、hosted_vllm、DeepSeek、DashScope 等兼容部署。Qwen 本地服务可选择 OpenAI Provider 并配置自己的 API Base URL。独立实现的 Azure、Anthropic、Gemini、Bedrock Provider 暂不支持该字段的非空配置，保存时会明确拒绝。

支持 `reasoning_effort` 的算力服务：

```json
{"reasoning_effort":"medium"}
```

使用模板开关的 Qwen3/vLLM 部署（须确认现场模型、框架版本与模板支持）：

```json
{"chat_template_kwargs":{"enable_thinking":true}}
```

留空或填写 `{}`，保存后取消默认补入。数据库保存为 JSONB，重启后仍保留。未配置的现有部署默认为 `{}`，不会统一补入思考参数。

## 合并规则

- 在实际部署选定后、发送上游请求前补入默认值；同名模型的不同部署可以采用不同参数。
- 递归补入缺失的对象字段，客户端已有值优先；`false`、`0`、数组均不会被默认值覆盖。明确关闭思考请使用后端支持的 `none` 或 `false`，不要用 `null` 表达关闭。
- 客户端已有 `reasoning_effort`、`thinking`、`enable_thinking` 或模板内的思考设置时，不再补入另一种方言的默认思考开关，避免 `none` 与 `enable_thinking: true` 并存。其他非思考参数仍正常合并。
- 不能通过默认值修改 `model`、`messages`、`stream`、网关内部字段，也不能包装为 `extra` / `extra_body`。配置对象直接对应实际 HTTP JSON 请求体。
- 该能力补默认值，不负责把客户端显式参数转换成其他后端方言；不自动探测后端能力，也不在后端拒绝后自动重试。仅为实际支持对应参数的部署配置默认值。

例如默认值为：

```json
{"chat_template_kwargs":{"enable_thinking":true,"other_setting":1}}
```

客户端传 `{"chat_template_kwargs":{"enable_thinking":false}}`，上游收到 `false` 并补齐 `other_setting: 1`；客户端传 `{"reasoning_effort":"none"}`，保留 `none`，不补 `enable_thinking`，仍可补入非思考的 `other_setting`。

## YAML 与管理 API

YAML 配置位于每个部署的 `litellm_params.request_defaults`：

```yaml
model_list:
  - model_name: qwen3
    litellm_params:
      model: openai/Qwen3-8B
      api_base: http://server-b:8000/v1
      request_defaults:
        chat_template_kwargs:
          enable_thinking: true
```

沿用仓库原有配置权威规则：数据库模式下 YAML 仅用于空库初始导入；日常修改使用控制台。仅修改已有 YAML 后执行 reload 不会覆盖数据库。无数据库模式从 YAML 构建 Provider。配置查询、YAML 导出和回写包含该字段。

控制台模型管理接口的请求和列表响应使用顶层 `request_defaults` 对象。POST 省略该字段时以 `{}` 初始化。PUT 省略该字段时保留已保存的值，传入 `{}` 时显式清空。更换到不支持非空默认参数的 Provider 时，可在同一次 PUT 中传 `{}` 清空。非对象输入（包括 `null`）返回 422，非法保留字段或与 Provider 不兼容的默认值会在写库前拒绝。

更新随控制台命令在当前网关实例生效，共享数据库的其他实例沿用现有路由配置同步机制。数据库迁移在启动时先探测列是否存在，需要时才执行 DDL，为 `boom_model_deployment` 增加 `request_defaults JSONB NOT NULL DEFAULT '{}'`，DDL 保持在表所属的 boom-routing 中。

## 原始版本验证与复跑

- 构建：`cargo build --locked -p boom-main --bin boom-gateway` 通过。
- 单元测试：boom-config 40 项、boom-provider 46 项、boom-routing 63 项通过；新增默认值合并/校验 3 项及 config_reconcile 7 项通过。存在原有 unused import / dead code 警告。
- 独立 PostgreSQL 数据库、真实网关、控制台管理 API、记录型模拟上游：30 项通过，覆盖默认补入、显式覆盖、跨方言关闭、嵌套合并、同名多部署隔离、即时修改、进行中请求、清空、创建、导出、reload、重启持久化和删除。
- Edge 无头真实浏览器：6 项通过，覆盖表单回显、提交与重新打开、非法 JSON、数组拒绝、嵌套 Qwen 参数、留空清除。截图经检查，无新增字段遮挡。
- 原有 OpenAI 透传回归：12 项通过；本地开发环境业务 Hook 冒烟：9 项通过；JS 语法检查及 `git diff --check` 通过。
- 本地开发容器已更新并恢复 healthy；测试数据库和临时控制台部署已清理。现有业务部署未配置新的默认值。

本机测试生成的证据（不纳入 Git 提交，可用测试脚本重新生成）：

- `docs/analysis/20261026/request-defaults-live.json`
- `docs/analysis/20261026/request-defaults-ui.json`
- `docs/analysis/20261026/request-defaults-console.png`

复跑集成测试需先创建专用空库 `boom_defaults_test_*`，在开发容器中执行：

```bash
python3 scripts/check-request-defaults-live.py \
  --database-name boom_defaults_test_YOUR_RUN \
  --output /tmp/request-defaults-live.json
```

脚本使用容器现有 `DATABASE_URL` 的连接信息，仅替换数据库名，不在报告中输出凭据。复跑后由操作者清理该专用测试库。

浏览器检查需在仓库外安装 `playwright-core`，通过 `PLAYWRIGHT_MODULE` 指向它，并在安装 Edge 的 Windows 主机运行 `node scripts/check-request-defaults-ui.cjs`。脚本默认访问本地开发环境，可通过 `GATEWAY_URL`、`MASTER_KEY` 指定其他测试环境；只应连接允许创建临时部署的测试环境。

本次模拟证明配置、持久化和传输行为；实际模型是否开启思考以及强度是否生效，仍需真实算力服务验证。
