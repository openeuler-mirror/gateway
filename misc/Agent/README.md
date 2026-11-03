# misc/Agent — Responses API 测试 Agent

针对 Boom Gateway `POST /v1/responses` 的最小测试 Agent（Rust，独立项目，
与 misc/LB 同级，**不在主 workspace 内**）。它不是简单的 curl 冒烟，而是：

1. **协议契约校验**（对应 `Response.md` §6.5 的验收断言）：
   - 非流式：`resp_` id、`object:"response"`、message output item、usage 三字段映射
   - 流式：`response.created` 必须是 seq 0 且 `status:"in_progress"`；sequence_number
     全程从 0 连续递增；`output_text.done.text` == 全部 delta 拼接；流终止于
     `response.completed`（携带 usage）而非 `data: [DONE]` 尾帧
   - P0 守卫：`previous_response_id` 无 store 时必须显式 400、有 store 时未知
     id 为 404 uniform（静默忽略是反模式）
2. **真实 Agent 循环**：声明 `get_current_time` 工具 → 收到 `function_call` →
   本地执行 → 以 `function_call` + `function_call_output` item 回放 → 拿到终答。
   同一轮 output 兼有 message + `function_call` 时**工具调用优先**（message 视为
   前置说明，不终止循环）。mock 上游会回显工具结果，确定性验证 `call_id`
   往返穿越了网关的协议转换；对真实 LLM 宽容：turn=1 直答（未调工具）或
   turn≥2 自然语言终答均 PASS，仅空终答或 3 轮不终止 FAIL。
3. **自由格式 custom 工具（P0.5，严格契约）**：`type:"custom"` 工具对客户端必须以
   `custom_tool_call`（freeform `input`）呈现——泄漏为 `function_call` 即 FAIL
   （该强转会破坏 Codex，vLLM #49724），且 `input` 必须是解包后的字符串。

## 用法

```bash
cargo build --release          # 在 misc/Agent 目录下
# mock CI（必须 --strict：断言工具结果回显，确定性验证 call_id 往返）
./target/release/agent --base-url http://127.0.0.1:18400 \
                       --key smoke-master-key --model test-model --strict
# 真实 LLM（默认宽容：自然语言终答即可，模型会转述而非引用工具结果）
./target/release/agent --base-url http://127.0.0.1:8000 \
                       --key <KEY> --model nemotron
```

`--strict`（或环境变量 `BOOM_AGENT_STRICT=1`）：终答必须包含 mock 的
工具结果回显（`unix_time_` / `PATCH_RESULT`）——这是 call_id 往返穿越
网关转换的**确定性证明**：若网关回归导致 `function_call_output` 被丢弃、
`call_id` 被重写、或 custom 包装未正确 unwrap，mock 第 2 轮只会返回普通
文本，strict 模式即 FAIL。默认模式仅要求非空终答（真实 LLM 会转述工具
结果而非逐字引用），往返证明由 mock CI 的 strict 运行承担。

退出码 0 = 全部检查通过；输出形如：

```
PASS  non_stream_response         id=resp_... text="..." tokens=5
PASS  stream_event_contract       events=10 chars=11
PASS  guard_previous_response_id  400 with explicit message: "..."
PASS  agent_tool_loop             turns=2 final="The current time is unix_time_..."
PASS  store_continuation          skipped (no response store on this gateway)
PASS  custom_tool_loop            turns=2 final="...PATCH_RESULT:ok:*** Begin Patch..."
PASS  passthrough_fidelity        skipped (deployment is not protocol: responses)
```

## 本地全链路自测（无需真实推理后端）

工程自带 `mock-upstream`（OpenAI chat 兼容 mock：首轮发工具调用、见到
tool 结果后回显终答、流式返回文本）。三步跑通网关 + Agent：

```bash
# 1. 构建网关（仓库根目录）与本工程（misc/Agent）
cargo build -p boom-main && (cd misc/Agent && cargo build)

# 2. 启动 mock 上游 + 网关
./misc/Agent/target/release/mock-upstream 18099 &
cat > /tmp/agent-smoke.yaml <<'YAML'
model_list:
  - model_name: test-model
    litellm_params:
      model: openai/upstream-model
      api_key: sk-upstream
      api_base: http://127.0.0.1:18099/v1
      timeout: 30
general_settings:
  master_key: smoke-master-key
server: { host: 127.0.0.1, port: 18400, workers: 1 }
rate_limit: { enabled: false }
YAML
./target/debug/boom-gateway --config /tmp/agent-smoke.yaml &

# 3. 跑 Agent（mock CI 用 strict）
./misc/Agent/target/release/agent --strict
```

## 指向真实环境

Agent 只依赖标准 Responses 协议，可直接指向任意网关部署；真实上游返回
工具调用即可触发 agent 循环。对真实 LLM 的宽容语义：turn=1 直答（未调工具）
或自然语言终答均 PASS，仅空终答或 3 轮内不终止才 FAIL（mock 下工具循环与
`call_id` 往返是确定性覆盖）。custom 工具检查保持严格：客户端见到
`function_call` 即契约破坏，无宽容路径。

## 文件

| 文件 | 说明 |
|---|---|
| `src/main.rs` | Agent：7 项检查 + SSE 解析 + 工具循环 |
| `src/bin/mock_upstream.rs` | 本地 mock 上游（tiny_http，零推理） |
