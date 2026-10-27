# misc/Agent — Responses API 测试 Agent

针对 Boom Gateway `POST /v1/responses` 的最小测试 Agent（Rust，独立项目，
与 misc/LB 同级，**不在主 workspace 内**）。它不是简单的 curl 冒烟，而是：

1. **协议契约校验**（对应 `Response.md` §6.5 的验收断言）：
   - 非流式：`resp_` id、`object:"response"`、message output item、usage 三字段映射
   - 流式：`response.created` 必须是 seq 0 且 `status:"in_progress"`；sequence_number
     全程从 0 连续递增；`output_text.done.text` == 全部 delta 拼接；流终止于
     `response.completed`（携带 usage）而非 `data: [DONE]` 尾帧
   - P0 守卫：`previous_response_id` 必须显式 400（静默忽略是反模式）
2. **真实 Agent 循环**：声明 `get_current_time` 工具 → 收到 `function_call` →
   本地执行 → 以 `function_call` + `function_call_output` item 回放 → 拿到终答，
   并断言终答包含工具结果（证明 `call_id` 往返穿越了网关的协议转换）。

## 用法

```bash
cargo build --release          # 在 misc/Agent 目录下
./target/release/agent --base-url http://127.0.0.1:18400 \
                       --key smoke-master-key \
                       --model test-model
```

退出码 0 = 全部检查通过；输出形如：

```
PASS  non_stream_response         id=resp_... text="..." tokens=5
PASS  stream_event_contract       events=10 chars=11
PASS  guard_previous_response_id  400 with explicit message: "..."
PASS  agent_tool_loop             turns=2 final="The current time is unix_time_..."
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

# 3. 跑 Agent
./misc/Agent/target/release/agent
```

## 指向真实环境

Agent 只依赖标准 Responses 协议，可直接指向任意网关部署；真实上游返回
工具调用即可触发 agent 循环（模型不调工具时该检查会在 3 轮后失败，属预期）。

## 文件

| 文件 | 说明 |
|---|---|
| `src/main.rs` | Agent：4 项检查 + SSE 解析 + 工具循环 |
| `src/bin/mock_upstream.rs` | 本地 mock 上游（tiny_http，零推理） |
