<div align="center">
  <img src="misc/logo.svg" alt="BooMGateway" width="360">
  <br><br>
  <strong>高性能 LLM API 网关</strong>
</div>

**中文** | [English](README.en.md)

使用 Rust 构建的生产级 LLM API 网关。统一接入 OpenAI、Anthropic、Gemini、Bedrock、vLLM、Ollama 等 20+ 上游服务商；兼容 litellm 密钥体系，自建限流、套餐、计费、流量治理、Web 管理面板、审计与完整 prompt 落盘。核心差异化能力：**面向 vLLM 前缀缓存优化的智能调度**——会话前缀亲和、密钥亲和、语义路由与反馈式负载均衡。

[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange)](https://www.rust-lang.org/)
[![License: MulanPSL v2](https://img.shields.io/badge/License-MulanPSL%20v2-green.svg)](LICENSE)

---

## 智能调度

调度策略可插拔、可热切换（`router_settings.schedule_policy`），全部为网关侧实现，对上游零侵入：

- **会话前缀亲和（`kvc_aware`）** — 网关侧自学习前缀 Trie：把每个请求的会话前缀（system + tools + messages，固定序）按块切分并哈希为块链，记录到实际选中的 worker 名下；下一个相同前缀的请求直接命中。按 per-worker 命中深度统一评分 `score = cache_weight × hit_ratio + load_weight × (1 − load_pct/100)`，把请求送往缓存命中最多且未过载的 worker，显著降低 TTFT。命中率不足时可主动向上游请求全量 KV cache report 修正索引；LRU + TTL 老化，不依赖 vLLM 事件订阅。
- **密钥亲和（`key_affinity`）** — 同一密钥 + 模型的请求粘滞到同一 deployment，最大化上游会话级缓存复用；新密钥预热期按最低负载散开，负载差超过阈值自动迁移再平衡。
- **语义路由（`auto_router`）** — 虚拟模型按请求内容动态分级：内置启发式分类器（关键词 / 长度 / 代码块 / 推理链 / 工具调用信号评分），或对接外部 ML 分类服务（失败自动回落本地），将请求路由到 small / medium / large 不同规格的真实模型。
- **反馈式负载均衡** — 实时负载信号总线（在途计数、流控队列深度、请求成功速率）驱动调度：统一评分选优、过载节点硬排除（阈值可配）、容量再平衡（优势节点超出最低候选一定百分比即移交流量）、顶分并列轮转打散；60 分钟窗口的迁移统计可在面板观测。
- **异常节点治理** — 连续失败自动熔断、`/metric` 主动探活自动上下线（阈值可配）、过载节点流量旁路、恢复后自动回流；兜底部署 `"*"` 同样受保护。

## 流量治理与可观测

- **流控** — 按 deployment 限制最大在途请求数与在途上下文字符总量：超限排队（VIP 密钥优先派发），超大请求立即拒绝；槽位随响应结束 / 客户端断连自动释放。
- **三维限流与计费** — 密钥 / 团队双层套餐，counts / tokens / costs 三维滑动窗口 + 终身累计配额，时段计划（支持跨午夜）自动切档。
- **全链路可观测** — 每请求落库 TTFT、耗时、排队时长、调度策略与 KV 命中率；异常离群检测（IQR）；OTel trace 导出；完整 prompt 落盘（滚动 + gzip，可按团队 / 密钥动态开关）。

## 网关基础能力

统一接入 20+ 上游服务商；litellm 密钥体系兼容；Anthropic 原生 `/v1/messages`（Claude Code / opencode 可直连）；Direct Synthesis Workflow（并发调用多个 panel 模型后由 aggregator 合成标准 OpenAI 响应）；公开模型、模型别名与 `"*"` 兜底路由；Web 管理面板（密钥 / 模型 / 套餐 / 配额 / 日志 / 实时在途与统计）；SIGHUP / API / 面板按钮零停机热加载。

---

## 性能实测

测试环境：**kunpeng920**（ARM64）。使用 `boom-gateway/test/` 下的 `mock-backend`（无推理延迟，仅返回 100~400 字符随机内容）与 `bench-client` 对网关压测。负载形态：**约 50K 输入上下文 + 输出 100~500 token**，**打开 prompt log 写盘 + OTLP 上报**。**所有数据单位均为 ms**。

### 非流式

| 目标 RPS | p50 | p90 | p99 | p999 | mean |
|---------|-----|-----|-----|------|------|
| 50    | 10.3 | 14.6 | 17.4 | 20.0 | 11.1 |
| 100   | 9.1  | 10.5 | 21.7 | 23.0 | 9.8 |
| 500   | 9.4  | 10.8 | 13.3 | 33.2 | 9.5 |
| 1000  | 9.6  | 11.0 | 22.8 | 34.6 | 9.8 |
| 2000  | 9.7  | 11.0 | 26.7 | 36.5 | 10.0 |

### 流式 TTFT（首 token 延迟）

| 目标 RPS | p50 | p90 | p99 | p999 | mean |
|---------|-----|-----|-----|------|------|
| 50    | 54.1 | 58.0 | 62.4 | 80.9 | 43.4 |
| 100   | 54.0 | 56.7 | 57.9 | 58.7 | 46.2 |
| 500   | 54.4 | 57.5 | 60.5 | 697.0 | 52.2 |
| 1000  | 65.3 | 99.0 | 169.0 | 5607.0 | 80.2 |
| 2000  | 87.3 | 281.0 | 1047.0 | 6295.0 | 155.0 |

### 流式 E2E（含 mock-backend 模拟 500 tps 吐 token，每请求约 200~1000ms）

| 目标 RPS | p50 | p90 | p99 | p999 | mean |
|---------|-----|-----|-----|------|------|
| 50    | 539 | 811 | 875 | 909 | 537 |
| 100   | 536 | 794 | 878 | 920 | 532 |
| 500   | 567 | 872 | 966 | 1235 | 571 |
| 1000  | 2373 | 6512 | 7794 | 8130 | 2996 |
| 2000  | 1564 | 7254 | 8839 | 9232 | 2822 |

**解读**：非流式 p50 全程稳定在 9~10ms，p999 ≤ 36.5ms——网关自身在压测下未成为瓶颈。流式 TTFT 在 500 RPS 内维持 ~54ms；1000 RPS 起出现排队，饱和拐点由首 token 阶段决定，后续吐 token 阶段未额外放大延迟。

> 复现方式见 [boom-gateway/test/README.md](boom-gateway/test/README.md)。

---

## 快速开始

### 源码部署

前置条件：Rust 1.75+（含 cargo）、PostgreSQL 13+。

```bash
git clone https://atomgit.com/openeuler/gateway.git
cd gateway

cargo build --release
# 二进制位于 target/release/boom-gateway
```

**1. 准备数据库** — 只需一个空库，启动时自动创建全部表：

```bash
createdb boom_gateway
# 或：psql -U postgres -c "CREATE DATABASE boom_gateway;"
```

**2. 编写最小配置** `config.yaml`（完整字段参考见 [CONFIG_EXAMPLE.md](CONFIG_EXAMPLE.md)）：

```yaml
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_key: os.environ/OPENAI_API_KEY

  # 兜底路由：匹配不到任何 model_name 的请求路由到这里
  - model_name: "*"
    litellm_params:
      model: openai/gpt-4o
      api_key: os.environ/OPENAI_API_KEY

general_settings:
  master_key: os.environ/MASTER_KEY
  database_url: os.environ/DATABASE_URL

server:
  host: 0.0.0.0
  port: 4000
```

**3. 启动**：

```bash
export MASTER_KEY="sk-your-master-key"
export DATABASE_URL="postgres://user:pass@localhost:5432/boom_gateway"
export OPENAI_API_KEY="sk-..."

./target/release/boom-gateway --config config.yaml
```

**4. 创建密钥并调用**：

```bash
# 通过面板 API 创建一个 API 密钥（响应中的明文密钥只显示一次，请立即保存）
curl -X POST http://localhost:4000/dashboard/api/admin/keys \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_alias": "alice"}'

# 像使用 OpenAI 一样使用网关
curl http://localhost:4000/v1/chat/completions \
  -H "Authorization: Bearer sk-your-new-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

浏览器打开 `http://localhost:4000/dashboard` 进入管理面板（master key 以 admin 登录，API key 以普通用户登录）。

### 容器部署

容器化部署方案正在完善中，当前请使用源码部署。

---

## 项目结构

```
gateway/
├── Cargo.toml             Rust workspace 根（成员见 boom-gateway/）
├── boom-gateway/          全部 crate
│   ├── boom-core/         核心 trait 与公共类型（叶子依赖）
│   ├── boom-auth/         密钥认证（SHA-256 + DB + master key，litellm 兼容）
│   ├── boom-config/       YAML 配置解析 + 环境变量展开 + 字段 manifest
│   ├── boom-provider/     LLM Provider 实现（OpenAI / Anthropic / Bedrock 等）
│   ├── boom-routing/      DeploymentStore + AliasStore + 调度策略族
│   ├── boom-kvindex/      KV 前缀索引（自学习 Trie）
│   ├── boom-fusion/       Workflow 抽象与 Direct Synthesis 编排
│   ├── boom-flowcontrol/  按 deployment 的流量控制（VIP 优先队列）
│   ├── boom-limiter/      滑动窗口限流 + 并发控制 + PlanStore
│   ├── boom-ctxaware/     客户端类型识别与统计
│   ├── boom-promptlog/    完整请求/响应落盘（JSONL）
│   ├── boom-audit/        请求日志读写
│   ├── boom-trace/        OTel trace 链路 + 延迟分布
│   ├── boom-stressmon/    压力监控
│   ├── boom-hooks-sdk/    Hook SDK
│   ├── boom-dashboard/    Web UI + REST API + JWT 认证
│   └── boom-main/         入口、路由、状态组装（二进制名：boom-gateway）
├── misc/LB/               Pingora 负载均衡（可选前端，已容器化）
├── config.example.yaml    完整配置参考
└── docs/                  设计文档与 API 手册
```

## 技术栈

| 组件 | 技术 |
|-----------|-----------|
| 语言 | Rust（edition 2021） |
| HTTP 框架 | Axum |
| 异步运行时 | Tokio（多线程） |
| 数据库 | PostgreSQL（sqlx，自动迁移） |
| 并发原语 | DashMap、ArcSwap |
| 计费 | rust_decimal |
| CLI | clap |
| 认证 | SHA-256 token 哈希、JWT 会话 |

## 文档

| 文档 | 说明 |
|----------|-------------|
| [USER_GUIDE.md](USER_GUIDE.md) | 用户操作手册：功能模块、参数配置、建议配置 |
| [docs/api-reference.md](docs/api-reference.md) | API 速查手册：全部端点 + curl 示例 |
| [CONFIG_EXAMPLE.md](CONFIG_EXAMPLE.md) | 完整配置字段参考 |
| [docs/software-design.md](docs/software-design.md) | 软件设计：模块职责、特性清单、DB schema |
| [docs/architecture-overview.md](docs/architecture-overview.md) | 架构总览 |
| [docs/internal-request-flow.md](docs/internal-request-flow.md) | 内部请求流转详解 |
| [ARCH.md](ARCH.md) · [DESCRIPTOR.md](DESCRIPTOR.md) | 架构设计文档 |
| [docs/direct-synthesis-workflow-design.md](docs/direct-synthesis-workflow-design.md) | Direct Synthesis Workflow 设计 |
| [docs/kvc-aware-design.md](docs/kvc-aware-design.md) | KV 亲和路由早期设计（基于 ZMQ 订阅架构；现行为网关侧自学习 Trie，以 software-design.md 为准） |
| [misc/LB/README.md](misc/LB/README.md) | Pingora 负载均衡前端 |
| [CLAUDE.md](CLAUDE.md) | 开发指南与架构原则 |

## 许可证

木兰宽松许可证 v2（Mulan Permissive Software License v2，MulanPSL-2.0）。详见 [LICENSE](LICENSE) 与 <http://license.coscl.org.cn/MulanPSL2>。
