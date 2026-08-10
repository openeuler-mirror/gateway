# Fusion 压测报告

## 1. 报告信息

| 项目 | 内容 |
|---|---|
| 报告日期 | 2026-08-10 |
| 完整压测日期 | 2026-08-08 |
| Gateway 代码基线 | `bc5d2fb` (`fix(fusion): harden direct synthesis streaming and diagnostics`) |
| 压测入口 | `boom-gateway/test/fusion-load-e2e.sh` |
| 负载客户端 | `boom-gateway/test/bench-client` |
| 模拟上游 | `boom-gateway/test/mock-backend` |
| 完整运行输出目录 | `/tmp/boom-fusion-readme-20260808T163447Z-1056292` |

本报告测试的是 BooMGateway 在明确 Mock Backend 行为下的吞吐、延迟、鉴权和
Fusion 调用拓扑，不代表真实模型推理服务的生产容量。

## 2. 结论摘要

1. README 定义的 5 类场景、共 7 次运行全部完成，共发送 `8,700,934` 个请求，
   客户端记录 `8,700,934` 个成功结果和 `0` 个错误。
2. 场景 A-D 的严格功能断言通过：客户端无 4xx、5xx、超时、连接、解析或流错误；
   Backend 无 503、错误模型或空 `tools`；100 把 virtual key 全部被使用。
3. Ramp 的延迟拐点约为 `2100 QPS`，`3100 QPS` 开始严重排队，
   `3100-3600 QPS` 之间进入并发饱和区；到 `3600 QPS` 档结束时，客户端在途请求
   约为 `10,072`。
4. Ramp 期间 Aggregator Mock 共拒绝 `1,149,846` 次请求，但客户端仍全部成功。
   原因是 Aggregator 在流建立前失败时，Fusion 会降级返回第一个有效 Panel。
   因此客户端成功率不能单独证明 Aggregator 健康。
5. Mock Backend 的 `max-concurrent=10000` 是人工测试上限。第 `10001` 个同时到达
   该 Mock 的请求会收到 503；该数值不是 Gateway 自身固有的并发上限。
6. Ramp 报告中的 `3929.61 QPS` 是整个阶梯过程（并包含最终请求收尾时间）的累计
   平均值，不能解释为 Gateway 的稳定吞吐上限。

## 3. 测试拓扑

```text
bench-client
    |
    v
real boom-gateway
    |-- Fusion --> Panel A Mock
    |           -> Panel B Mock
    |           -> Aggregator Mock
    |
    |-- OpenAI/Anthropic protocol path --> Protocol Mock
    |
    `-- virtual-key authentication --> temporary PostgreSQL
```

测试使用真实 release `boom-gateway` 二进制、真实 HTTP/SSE 链路和真实 PostgreSQL
迁移及鉴权流程。四个 Mock 和测试 Gateway 均使用随机本地端口，不占用已有 Gateway
服务端口。

三个 Fusion Mock 分别只接受自己的上游模型名。请求发错 Backend 会增加
`rejected_model`。所有 Mock 均启用 `--reject-empty-tools`，显式收到 `tools: []`
会增加 `rejected_empty_tools` 并返回 400。

## 4. 测试环境

| 项目 | 值 |
|---|---|
| 主机 | `gpu77` |
| 内核 | Linux `5.15.0-185-generic` x86_64 |
| 可见内存 | 502 GiB |
| Rust | `rustc 1.97.1 (2026-07-14)` |
| Cargo | `cargo 1.97.1 (2026-06-30)` |
| Docker | `29.6.1` |
| PostgreSQL | `postgres:16-alpine` |

当前执行环境无法从容器命名空间可靠读取 CPU 拓扑，因此本报告不提供 CPU 核数。
跨机器比较绝对 QPS 前，必须补齐 CPU、NUMA、容器限额和系统负载信息。

## 5. Mock 参数

完整运行使用 README 默认值：

```text
输出长度：          100-400 字符
流式 chunk 间隔：  2 ms
最大并发：          10000
TTFT 延迟：         0 ms
非流式额外延迟：    0 ms
```

Mock 的输出长度、chunk 间隔和并发上限会直接影响请求持有连接的时间及吞吐结果。
因此本报告的数值只能在相同参数下横向比较。

## 6. 场景定义

| 场景 | 参数 | 目的 |
|---|---|---|
| A 基线吞吐 | Fusion，单 key，1000 QPS，120 秒，流式，1K-5K Prompt | 测 Fusion 基线吞吐和流稳定性 |
| B OpenAI | 普通模型，500 QPS，60 秒，流式，2K Prompt | 协议转换对照组 |
| B Anthropic | `/v1/messages`，普通模型，500 QPS，60 秒，流式，2K Prompt | 测 Anthropic 入站转换和响应转码 |
| C DB 认证 | Fusion，100 keys，2000 并发，120 秒，非流式，2K Prompt | 测 virtual-key 鉴权和缓存路径 |
| D 短 Prompt | Fusion，200 QPS，60 秒，流式，1K Prompt | 长 Prompt 对照组 |
| D 长 Prompt | Fusion，200 QPS，60 秒，流式，100K Prompt | 测请求体解析影响 |
| E Ramp | Fusion，`ramp=100,5000,500,30`，15 分钟，流式 | 寻找延迟和并发拐点 |

场景 B 不使用 Fusion。Gateway 普通模型支持 Anthropic 兼容的 `/v1/messages`
入站协议，并转换为内部 OpenAI 请求；Fusion 模型本身仍只支持
`/v1/chat/completions`。

## 7. 汇总结果

延迟单位均为毫秒。

| 场景 | Sent | OK | 错误 | 实际 QPS | TTFT P50 | TTFT P99 | E2E P50 | E2E P99 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| A 基线 | 120,000 | 120,000 | 0 | 994.71 | 44.99 | 49.79 | 416.77 | 680.96 |
| B OpenAI | 29,997 | 29,997 | 0 | 494.68 | 43.90 | 48.13 | 413.18 | 667.65 |
| B Anthropic | 30,000 | 30,000 | 0 | 494.86 | 43.71 | 49.18 | 433.92 | 706.56 |
| C 100 keys | 4,951,479 | 4,951,479 | 0 | 41,250.91 | 47.01 | 93.63 | 47.01 | 93.63 |
| D 1K Prompt | 12,000 | 12,000 | 0 | 197.97 | 43.71 | 49.18 | 415.23 | 675.33 |
| D 100K Prompt | 12,000 | 12,000 | 0 | 198.07 | 45.79 | 52.38 | 420.86 | 678.91 |
| E Ramp | 3,545,458 | 3,545,458 | 0 | 3929.61* | 68.99 | 220.16 | 1935.36 | 6475.78 |

`*` Ramp 的实际 QPS 是全程累计平均值，不是单一压力档位的稳定吞吐。

## 8. 分场景分析

### 8.1 A：Fusion 基线

1000 QPS 目标下实际完成 `994.71 QPS`，客户端无错误；Panel A、Panel B 和
Aggregator 的调用数均与用户请求数一致，未出现模型错路由、空 tools 或 Backend 503。

### 8.2 B：协议转换

OpenAI 和 Anthropic 两条路径均稳定完成约 500 QPS。Anthropic E2E P50 比 OpenAI
高约 `20.74 ms`。这个差值包含转换、响应转码和当时调度噪声，不能仅凭一次运行归因
为纯协议转换开销。

该场景证明普通模型的 Anthropic 兼容路径可以工作，不代表 Fusion 支持
`/v1/messages`。

### 8.3 C：100 keys 和 2000 并发

120 秒内完成 `4,951,479` 个非流式 Fusion 请求，100 把通过 Dashboard API 创建的
virtual key 全部被实际选择：

```text
keys_configured: 100
keys_used:       100
key_requests_min: 49084
key_requests_max: 50089
```

该结果覆盖真实数据库迁移、Dashboard Key 创建、API 鉴权和随机 Key 使用路径。

### 8.4 D：1K 与 100K Prompt

100K Prompt 相比 1K Prompt：

```text
TTFT P50: 43.71 ms -> 45.79 ms，增加约 2.08 ms
E2E  P50: 415.23 ms -> 420.86 ms，增加约 5.63 ms
```

在 200 QPS 下未观察到超过 100 ms 的 TTFT 增量，当前环境中请求体解析尚未形成主要
瓶颈。

## 9. Ramp 分析

Ramp 从 100 QPS 开始，每 30 秒增加 500 QPS；达到 5000 QPS 后持续到 15 分钟结束。
日志包含完整 30 个阶梯，其中 20 个阶梯保持在 5000 QPS。

`sent - ok` 在本次零错误运行中可近似表示客户端尚未完成的在途请求。边界数据如下：

| 压力档位 | 档位结束时间 | 在途请求约值 | 累计 TTFT P99 | 累计 E2E P99 | 判断 |
|---:|---:|---:|---:|---:|---|
| 1600 QPS | 120s | 626 | 49 ms | 656 ms | 基本健康 |
| 2100 QPS | 150s | 1,251 | 51 ms | 1,116 ms | 延迟首次明显恶化 |
| 2600 QPS | 180s | 1,625 | 52 ms | 1,117 ms | 仍可维持 |
| 3100 QPS | 210s | 3,504 | 134 ms | 3,737 ms | 严重排队 |
| 3600 QPS | 240s | 10,072 | 147 ms | 4,575 ms | 进入约 10000 并发饱和 |
| 4100 QPS | 270s | 10,072 | 165 ms | 4,915 ms | 已饱和 |
| 4600 QPS | 300s | 10,109 | 168 ms | 4,976 ms | 已饱和 |
| 5000 QPS | 330s | 10,170 | 171 ms | 5,013 ms | 超过稳定区 |

这里的延迟值是从测试开始累计到该时刻的分位数，不是该 30 秒档位的独立分位数。
因此可用于观察趋势，但不能替代逐档独立 Histogram。

可得到两个不同的拐点：

```text
延迟拐点：       约 2100 QPS
严重排队：       约 3100 QPS
并发饱和区间：   3100-3600 QPS
确定进入饱和：   3600 QPS 档
```

## 10. Aggregator 503 与 Fusion 降级

Ramp 的 Backend 增量：

```json
{
  "panel-a": {
    "total_received": 3545458,
    "rejected_503": 0
  },
  "panel-b": {
    "total_received": 3545458,
    "rejected_503": 0
  },
  "aggregator": {
    "total_received": 3545458,
    "rejected_503": 1149846
  },
  "protocol": {
    "total_received": 0,
    "rejected_503": 0
  }
}
```

Aggregator Mock 获取不到 `max-concurrent=10000` 的 semaphore permit 时返回 503。
如果错误发生在 Aggregator 流建立前，`direct_synthesis` 会使用第一个有效 Panel
构造成功响应。因此出现：

```text
客户端错误：        0
Aggregator 503：    1,149,846
```

这不是 bench-client 漏报，也不是 Gateway 重试成功，而是 Fusion 的显式降级行为。
监控 Fusion 容量时，必须同时观察客户端结果、Panel/Aggregator 调用状态和降级次数。

## 11. Ramp 原始报告

```json
{
  "duration_secs": 902.241903293,
  "mode": "Ramp { from: 100, to: 5000, step: 500, step_duration: 30s }",
  "format": "openai",
  "stream": true,
  "sent": 3545458,
  "ok": 3545458,
  "err_429": 0,
  "err_5xx": 0,
  "err_4xx": 0,
  "err_timeout": 0,
  "err_connect": 0,
  "err_parse": 0,
  "err_stream": 0,
  "actual_qps": 3929.609107113954,
  "success_qps": 3929.609107113954,
  "ttft": {
    "p50_us": 68991,
    "p99_us": 220159,
    "max_us": 10526719,
    "count": 3545458
  },
  "e2e": {
    "p50_us": 1935359,
    "p99_us": 6475775,
    "max_us": 17334271,
    "count": 3545458
  }
}
```

## 12. 验收规则

场景 A-D 使用严格策略：

```text
sent == ok
所有客户端错误计数 == 0
Backend rejected_503 == 0
Backend rejected_model == 0
Backend rejected_empty_tools == 0
Backend inflight == 0
Backend 收包数符合场景拓扑
```

Ramp 使用容量分析策略：

```text
sent == ok + 所有错误
完整执行 30 个阶梯
5000 QPS 保持 20 个阶梯
Panel 调用数处于一次调用到一次重试的合法范围
Aggregator 调用数不超过用户请求数
错误模型和空 tools 仍必须为 0
Backend 503 被记录为容量指标，不直接判定脚本失败
```

## 13. 运行完整性说明

完整默认参数运行完成了 7 次发压并生成所有 bench 报告和 Backend 前后快照。运行结束时，
旧版脚本因为要求 Ramp 的 Backend 503 必须为 0 而退出。观察到 Fusion 降级语义后，
脚本仅修改了 Ramp 验收策略和 Backend 增量报告，不改变负载参数或负载生成方式。

最终版本另执行了缩短版 7 场景端到端回归：

```text
cases:          7
total_sent:     12266
total_ok:       12266
total_errors:   0
delta reports:  7
```

完整运行的原始数据已重新应用最终断言并通过。没有在最终断言修改后再次重复完整约
23 分钟的默认参数运行。

## 14. 已知限制

1. Ramp 只在整个场景开始和结束时读取 Backend stats，无法确定第一条 Aggregator 503
   发生的精确秒数和压力档位。
2. 实时 TTFT/E2E 是累计 Histogram，不能直接得到每个 30 秒档位的独立分位数。
3. `max-concurrent=10000` 会主动限制 Mock，无法单独测出没有 Backend 人工上限时的
   Gateway 极限。
4. 所有组件运行在同一台机器，bench-client、Gateway、PostgreSQL 和四个 Mock 会争用
   CPU、内存和网络栈。
5. 标准压测关闭 Prompt Log，以避免详细日志 I/O 改写吞吐结果。
6. 场景 B 测普通模型的 Anthropic 协议转换，没有覆盖
   `/v1/messages + model=fusion` 的预期拒绝。

## 15. 后续建议

1. Ramp 每秒或每个阶梯保存 Backend `inflight`、`rejected_503` 增量和 Gateway
   资源指标，精确定位首个过载点。
2. 为每个 Ramp 档位建立独立 Histogram，避免累计分位数掩盖当前档位变化。
3. 增加一组 `max-concurrent` 足够高的 Mock 对照实验，用于分离 Gateway 瓶颈和
   Backend 人工上限。
4. 单独加入 `/v1/messages + model=fusion` 负向 E2E，确认 Fusion 的协议限制。
5. 对 Fusion 降级增加独立指标，避免用户侧 200 掩盖 Aggregator 持续不可用。

## 16. 复现

完整 README 参数：

```bash
./boom-gateway/test/fusion-load-e2e.sh
```

已构建 release 二进制时：

```bash
FUSION_LOAD_SKIP_BUILD=1 ./boom-gateway/test/fusion-load-e2e.sh
```

报告、实时输出、Gateway/Mock/PostgreSQL 日志和 Backend 增量默认保存在：

```text
/tmp/boom-fusion-readme-<UTC timestamp>-<PID>
```
