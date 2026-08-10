# mock-otlp-collector — OTLP 接收端测试工具

模拟 OTLP/HTTP 后端，解码 `ExportLogsServiceRequest` protobuf 并打印每条 `LogRecord`。用于本地验证网关（或离线 replay 工具）的 OTLP 上报内容，**不需要**真的起一个 otel-collector 容器。

```
 ┌────────────┐   POST /v1/logs        ┌─────────────────────┐
 │  gateway   │ ──────────────────▶   │ mock-otlp-collector │
 │  或 replay │   application/x-protobuf │                     │
 └────────────┘                        └─────────────────────┘
```

## 编译

独立 Cargo 项目（不在 gateway workspace 内，避免拖累日常 `cargo build --workspace`）：

```bash
cd boom-gateway/test/mock-otlp-collector && cargo build --release
```

产物在 `boom-gateway/test/mock-otlp-collector/target/release/mock-otlp-collector`。

## 启动

最简单：

```bash
./boom-gateway/test/mock-otlp-collector/target/release/mock-otlp-collector
```

默认监听 `0.0.0.0:4318`（OTLP/HTTP 标准端口），简要打印模式。

切完整打印：

```bash
./boom-gateway/test/mock-otlp-collector/target/release/mock-otlp-collector --full
```

可选参数：

| 参数 | 默认 | 说明 |
|---|---|---|
| `--bind` | `0.0.0.0:4318` | 监听地址 |
| `--full` | 关 | 切完整打印模式（打印所有 attribute + 完整 body） |
| `--brief-chars` | `80` | 简要模式中 body 截断长度 |

## 简要 vs 完整

**简要模式**（默认）每条 LogRecord 一行，只打 OTel 头 + body 摘要，适合开发期反复跑、目测关联性：

```
[16:01:58 req#0] batch=2 records
  trace=7144ce63ae255d55c77c548240f163ef span=6bab56924ce89bf0 sev=INFO body="RESP /v1/chat/completions req-001 model=gpt-4 (150ms, 200)"
  trace=7144ce63ae255d55c77c548240f163ef span=72ef79fe5c2f85a0 sev=INFO body="REQ /v1/chat/completions req-001 model=gpt-4"
```

可以看到同一 request 的两条 LogRecord 共享 `trace_id`、`span_id` 因阶段不同而不同。

**完整模式**（`--full`）展开所有字段，适合验证字段映射是否正确：

```
-- ResourceLogs[0] --
  resource.attr: service.name = "boom-gateway"
  resource.attr: service.version = "0.1.0"

  ScopeLogs[0] scope.name="boom-promptlog" 1 records

    LogRecord[0]:
      time_unix_nano: 1786348957000000000
      severity: INFO ("INFO")
      trace_id=7144ce63... span_id=6bab5692...
      body: "RESP /v1/chat/completions req-001 model=gpt-4 (150ms, 200)"
      attr: url.path = "/v1/chat/completions"
      attr: http.request.method = "POST"
      attr: http.response.status_code = 200
      attr: duration_ms = 150
      attr: prompt_log.phase = "response"
      attr: prompt_log.request_id = "req-001"
      attr: prompt_log.response = "{"id":"req-001","object":"chat.completion",...}"
```

## 端到端示例

```bash
# 终端 1：起 mock-otlp-collector
./boom-gateway/test/mock-otlp-collector/target/release/mock-otlp-collector

# 终端 2（gateway workspace 根目录）：跑离线 replay 工具推一批日志
cd boom-gateway
cargo run --example replay -p boom-promptlog --features otlp -- \
  --dir /data/prompt_logs \
  --otlp-endpoint http://127.0.0.1:4318
```

或者直接起网关，`config.yaml` 里：

```yaml
prompt_log:
  enabled: true
  dir: "/data/prompt_logs"
  otlp:
    enabled: true
    endpoint: "http://127.0.0.1:4318"
```

然后向网关发请求，终端 1 会实时看到 LogRecord 流。

## 几个验证点

- **trace 关联性**：同一请求的 request/response 两条 LogRecord 应有相同 `trace_id`、不同 `span_id`（按 phase 隔离）
- **severity**：request 阶段 = INFO；response 阶段 2xx=INFO、4xx=WARN、5xx=ERROR
- **截断**：故意把 `max_attribute_bytes` 调小（比如 100），完整模式能看到 `dropped_attributes_count` 大于 0
- **流断开**：客户端中途断开，response LogRecord 应有 `attr: prompt_log.error_code = "CLIENT_DISCONNECTED"` + `http.response.status_code = 499`
- **headers 白名单**：`record_headers` 配置的 header 才出现在 `prompt_log.headers` attribute 里，没配置的不出现
