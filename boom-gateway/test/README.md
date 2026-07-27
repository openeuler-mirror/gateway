# BooMGateway 吞吐压测工具

测网关吞吐上限的两个配套工具：

- **`mock-backend`**：模拟 OpenAI API 后端，收到请求不做推理直接回复（100~400 字符随机内容）。所有端点（包括 Anthropic `/v1/messages`）都返回 OpenAI 格式 —— 这正好测出网关的协议转换路径。
- **`bench-client`**：客户端打流工具。支持 OpenAI / Anthropic 两种请求格式、1~N 个 API key 随机轮询（压 DB 认证鉴权性能）、3 种负载模式（恒定 QPS / 恒定并发 / 阶梯加压）、HDR 延迟分桶。

```
 ┌──────────────┐      ┌──────────────┐      ┌──────────────┐
 │ bench-client │ ───▶ │   gateway    │ ───▶ │ mock-backend │
 │              │ ◀─── │   (被测)     │ ◀─── │              │
 └──────────────┘      └──────────────┘      └──────────────┘
   吞吐/延迟指标         网关侧 CPU/FD/          /internal/stats
   HDR histogram         限流拒绝数             接收 QPS / 并发
```

## 编译

两个工具都是独立的 Cargo 项目（**不**在 gateway workspace 内，避免拖累日常 build）：

```bash
cd boom-gateway/test/mock-backend && cargo build --release
cd boom-gateway/test/bench-client && cargo build --release
```

产物在各自 `target/release/` 下。

## 快速开始

**最小可用流程**：mock → gateway → bench，本地三件套跑通。

```bash
# 1. 起 mock 后端
./boom-gateway/test/mock-backend/target/release/mock-backend \
  --bind 127.0.0.1:9000 --min-chars 100 --max-chars 400

# 2. 起 gateway，model_list 指向 mock
cat > /tmp/bench-config.yaml <<'EOF'
model_list:
  - model_name: mock-gpt-4o
    litellm_params:
      model: openai/mock-gpt-4o
      api_base: http://127.0.0.1:9000/v1
      api_key: sk-mock
general_settings:
  master_key: sk-master-xxx
EOF
./target/release/boom-gateway --config /tmp/bench-config.yaml --port 8080

# 3. （另起一个终端）验证 gateway → mock 链路
curl -s -X POST http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-master-xxx" \
  -H "Content-Type: application/json" \
  -d '{"model":"mock-gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":false}'

# 4. 跑压测（OpenAI 流式，50 QPS，5 秒）
./boom-gateway/test/bench-client/target/release/bench-client \
  --target http://127.0.0.1:8080 \
  --format openai \
  --keys sk-master-xxx \
  --model mock-gpt-4o \
  --prompt-min 1000 --prompt-max 5000 \
  --mode qps=50 --duration 5s --stream=true
```

**预期输出**（节选）：

```json
{
  "duration_secs": 5.01,
  "actual_qps": 49.98,
  "success_qps": 49.98,
  "ok": 250,
  "ttft": { "p50_us": 322, "p95_us": 649, "p99_us": 900 },
  "e2e":  { "p50_us": 531, "p95_us": 1036, "p99_us": 1320 }
}
```

## mock-backend CLI

```
mock-backend [--bind ADDR] [--min-chars N] [--max-chars N]
             [--chunk-interval-ms N] [--max-concurrent N] [--ttft-ms N]
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--bind` | `0.0.0.0:8000` | 监听地址 |
| `--min-chars` | `100` | 输出最短字符数 |
| `--max-chars` | `400` | 输出最长字符数 |
| `--chunk-interval-ms` | `2` | 流式每 chunk 间隔（ms）。设 0 让 mock 尽快推 |
| `--max-concurrent` | `10000` | 自身并发上限。超出时返回 503，模拟后端过载 |
| `--ttft-ms` | `0` | 首 token 延迟（ms）。非 0 时模拟 reasoning 模型 |

**端点**：
- `POST /v1/chat/completions` — OpenAI 格式（流式 / 非流式）
- `POST /v1/completions` — OpenAI 格式（流式 / 非流式）
- `POST /v1/messages` — Anthropic 风格入参，**仍返回 OpenAI 格式**（测协议转换）
- `GET /v1/models` — 返回 mock 模型列表
- `GET /health` — `{"status":"ok"}`
- `GET /internal/stats` — 自身指标（inflight / 接收 QPS / 各端点计数 / 503 拒绝数）

**SSE 格式**：vLLM 标准 —— `delta` chunk 一个个发，最后一个 chunk 带 `usage` 字段（`prompt_tokens` / `completion_tokens` / `total_tokens`），结尾 `data: [DONE]\n\n`。**usage 必须在最后一个 chunk**，这是网关 `LoggedStream` 提取计费用的。

> **关于 end-of-stream 检测**：mock 直发 `data: [DONE]\n\n`；但网关成功路径**不**转发 `[DONE]`（OpenAI 流以 `"finish_reason":"stop"` 终止，Anthropic 流以 `"type":"message_stop"` 终止），只有错误路径才发 `[DONE]`。bench-client 同时认这三种信号，所以 mock 直连和经网关两种链路都能正确判完。

## bench-client CLI

```
bench-client --target URL --format FMT --keys K1,K2,...
             --model NAME --mode MODE [options...]
```

**必填**：

| 参数 | 说明 |
|---|---|
| `--target` | gateway 基地址，如 `http://127.0.0.1:8080` |
| `--format` | `openai`（POST `/v1/chat/completions`）或 `anthropic`（POST `/v1/messages`）|
| `--keys` | API key 列表（逗号分隔），每请求随机选一个 |
| `--model` | 请求体里的 model 字段 |
| `--mode` | `qps=N` / `concurrent=N` / `ramp=FROM,TO,STEP,DURATION_SECS` |

**可选**：

| 参数 | 默认 | 说明 |
|---|---|---|
| `--auth-style` | `bearer` | `bearer` 用 `Authorization: Bearer`，`anthropic` 用 `x-api-key` + `anthropic-version: 2023-06-01` |
| `--prompt-min` | `1000` | 最短 prompt 字符数 |
| `--prompt-max` | `5000` | 最长 prompt 字符数（可拉到 `200000`） |
| `--duration` | `60s` | 总时长，支持 `Ns` / `Nm` / `Nms` 或纯数字（秒） |
| `--stream` | `true` | `--stream=true` 流式（默认），`--stream=false` 非流式 |
| `--report` | 空 | JSON 报告写出路径；空=只 stdout |
| `--pool-idle-per-host` | `2048` | reqwest 连接池大小 |
| `--request-timeout-secs` | `120` | 单请求超时 |

**实时输出**（每秒一行）：

```
[t+  3.0s] sent=150 ok=148 429=0 5xx=0 tmo=0 cnt=0 | ttft p50=3ms p99=12ms | e2e p50=24ms p99=38ms | ok_qps=49
```

**最终 JSON** 含完整分桶（P50/P90/P95/P99/P99.9/max）+ 错误分类 + QPS。

## 5 个典型场景

### 场景 A：基线吞吐（OpenAI 单 key 流式）

```bash
bench-client --target http://gw:8080 --format openai \
  --keys sk-aaa --model mock-gpt-4o \
  --prompt-min 1000 --prompt-max 5000 \
  --mode qps=1000 --duration 120s --stream=true \
  --report /tmp/A.json
```

看 `success_qps` 是否接近 1000、`e2e.p99` 是否稳定（< 200ms 视为健康）。

### 场景 B：协议转换开销（OpenAI vs Anthropic 对比）

```bash
# OpenAI 路径
bench-client --target http://gw:8080 --format openai \
  --keys sk-aaa --model mock-gpt-4o \
  --prompt-min 2000 --prompt-max 2000 \
  --mode qps=500 --duration 60s --stream=true \
  --report /tmp/B-openai.json

# Anthropic 路径（同样的 prompt/model）
bench-client --target http://gw:8080 --format anthropic --auth-style anthropic \
  --keys sk-aaa --model mock-claude \
  --prompt-min 2000 --prompt-max 2000 \
  --mode qps=500 --duration 60s --stream=true \
  --report /tmp/B-anthropic.json
```

对比两份 JSON 的 `e2e.p50` 差值即为协议转换的额外开销（通常 < 1ms）。

### 场景 C：DB 认证性能（多 key 大并发）

```bash
# 生成 100 个 key 列表（先在 gateway 里把这些 key 都建好）
keys=$(for i in $(seq 1 100); do echo -n "sk-key-$i,"; done | sed 's/,$//')

bench-client --target http://gw:8080 --format openai \
  --keys "$keys" --model mock-gpt-4o \
  --prompt-min 2000 --prompt-max 2000 \
  --mode concurrent=2000 --duration 120s --stream=false \
  --report /tmp/C.json
```

`concurrent=2000` 意味着同时 2000 个 inflight 请求，每个用随机 key —— 这能压出 DB 认证的缓存命中率上限。`err_5xx` 数量是网关侧 auth/lookup 异常的指标。

### 场景 D：长 prompt 影响（1K vs 100K）

```bash
# 短 prompt
bench-client ... --prompt-min 1000 --prompt-max 1000 \
  --mode qps=200 --duration 60s --report /tmp/D-short.json

# 长 prompt
bench-client ... --prompt-min 100000 --prompt-max 100000 \
  --mode qps=200 --duration 60s --report /tmp/D-long.json
```

对比 `ttft.p50`。如果长 prompt 下 TTFT 显著升高（> 100ms），说明网关在请求体解析/JSON 反序列化上有瓶颈。

### 场景 E：找吞吐拐点（ramp 模式）

```bash
bench-client --target http://gw:8080 --format openai \
  --keys sk-aaa --model mock-gpt-4o \
  --prompt-min 1000 --prompt-max 5000 \
  --mode ramp=100,5000,500,30 \
  --duration 15m --stream=true \
  --report /tmp/E.json
```

`ramp=100,5000,500,30` = 从 100 QPS 起，每 30s +500，直到 5000。实时输出里看 `ok_qps` 何时不再随 sent_qps 上升、`e2e.p99` 何时开始陡升 —— 拐点就是吞吐上限。

## 指标解读

| 字段 | 含义 |
|---|---|
| `sent` | bench 发出的请求总数 |
| `ok` | 收到完整 200 响应（流式收完终止信号：`[DONE]` / `finish_reason` / `message_stop`）的请求数 |
| `err_429` | 网关限流拒绝数（期望错误，单算） |
| `err_5xx` | 网关内部错误（异常） |
| `err_4xx` | 客户端错误（鉴权失败 / 参数错误等） |
| `err_timeout` | 单请求超时 |
| `err_connect` | TCP 连接失败 |
| `err_parse` | 响应体不是合法 JSON |
| `err_stream` | 流式未收到任何终止信号（`[DONE]` / `finish_reason` / `message_stop`）就断开 |
| `ttft` | Time To First Token（首 chunk 字节时间） |
| `e2e` | End-to-End（请求发出 → 流完成时间） |
| `actual_qps` | `sent / duration`，理想情况下接近 `mode` 设定值 |
| `success_qps` | `ok / duration`，真正的有效吞吐 |

**健康度判定**：
- `actual_qps` ≈ `mode` 设定值 → bench 自己没瓶颈
- `success_qps` ≈ `actual_qps` → 几乎无错误
- `ttft.p99` / `ttft.p50` < 3 → 后端响应稳定（mock 是 0 计算延迟，P99/P50 比应接近 1）
- `e2e.p99` / `e2e.p50` < 5 → 流式调度稳定；> 10 说明有 occasional stall
- `err_5xx` = 0 → 网关本身无 bug
- `err_429` = 0 → 限流未生效（或未配置）；> 0 但符合预期 → 限流正常工作

## 常见问题

**`Too many open files`**：并发超过 OS 文件描述符限制。
```bash
ulimit -n 65535  # macOS / Linux 都需要
```

**`Connection refused`**：mock 或 gateway 没起，或端口错。

**`err_429 too many`**：网关的 plan/limit 还开着，需要在 gateway config 里把 rpm/tpm/concurrency 全关，或用 master_key（默认无限）。

**`err_stream > 0`**：流式断了，常见原因：gateway timeout < mock 推流时间；或 mock 的 `--chunk-interval-ms` 过大导致单 stream 时长超过 gateway 默认 stream timeout。

**bench 的 `actual_qps` 远低于目标**：
- 检查 bench 的 `--pool-idle-per-host`（默认 2048，够用）
- 检查 gateway 是不是被打满（看 mock 的 `/internal/stats` 接收 QPS 是不是也低）
- 检查 bench-client 进程的 CPU（应 < 100%；如果满说明 bench 自己瓶颈）

**mock 的 `/internal/stats` 显示 `rejected_503 > 0`**：mock 自身到 `--max-concurrent` 上限了。调大 `--max-concurrent` 或开多 mock 实例。

## 不在范围内（后续按需扩展）

- **分布式压测**：单 client 当前能撑 ~5K 并发，超过用多机协同
- **Prometheus + Grafana 看板**：手动接 `bench-client` 的 JSON 报告到 Pushgateway
- **KV-cache 路由专项测试**：mock 不模拟 `cached_tokens` 字段
- **docker-compose 一键起**：当前需要 `cargo build` + 手动起，确认稳定后再封装

## 设计决策

- **不加入 gateway workspace**：避免 `cargo build --workspace` 时编译这两个工具，拖累正常开发循环
- **mock 用 axum + tokio**：与 gateway 同栈，单实例能到 ~10K QPS，自身不成为瓶颈
- **bench 用 reqwest + HDR histogram**：reqwest 的 `bytes_stream()` 正确处理 SSE；HDR 提供低开销分桶
- **mock 所有端点返回 OpenAI 格式**：与老板需求一致，正好测网关的 `boom-core::anthropic::anthropic_request_to_openai()` 协议转换路径
- **bench 每请求随机选 key**：模拟真实生产环境多用户，压 DB 认证缓存
- **流式默认开**：流式是 LLM 网关的真正负载形态；非流式作为 baseline 对比
