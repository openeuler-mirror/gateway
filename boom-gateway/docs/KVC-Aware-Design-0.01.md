# KVC-Aware Routing Design Document

> Version: 0.01
> Date: 2026-05-18
> Status: Draft
> Author: Claude & Luan

---

## Table of Contents

1. [Overview](#1-overview)
2. [Motivation & Background](#2-motivation--background)
3. [Industry Analysis: llm-d vs Dynamo](#3-industry-analysis-llm-d-vs-dynamo)
4. [Architecture Design](#4-architecture-design)
5. [Tokenizer & Hash Alignment](#5-tokenizer--hash-alignment)
6. [Event System Design](#6-event-system-design)
7. [Positional Index with Jump Search](#7-positional-index-with-jump-search)
8. [SchedulePolicy Integration](#8-schedulepolicy-integration)
9. [Configuration](#9-configuration)
10. [Module Dependencies](#10-module-dependencies)
11. [Performance Budget](#11-performance-budget)
12. [Deployment Topology](#12-deployment-topology)
13. [Implementation Roadmap](#13-implementation-roadmap)
14. [References](#14-references)

---

## 1. Overview

BooM Gateway 的 KV-Cache Aware Routing（KVC-Aware）是在网关层实现精准 KV 缓存感知路由的能力。通过在路由决策前对请求进行 tokenize、计算 prefix block hash，结合下游推理引擎实时上报的 KV cache 事件构建全局索引，将请求路由到缓存命中率最高的 deployment，从而大幅降低 TTFT（Time To First Token）并提升系统吞吐。

**核心指标目标：**
- 路由额外开销 < 2ms（含 tokenize + hash + index lookup）
- TTFT 改善 > 10x（缓存命中场景）
- 吞吐提升 > 2x（同等硬件）
- 支持 500+ Pods 行星级部署

---

## 2. Motivation & Background

### 2.1 问题：分布式环境下的缓存失效

在单实例环境中，vLLM 的 Automatic Prefix Caching（APC）通过 hash-based block 匹配实现高效的前缀复用。但在分布式多副本环境中：

```
问题链:
  标准负载均衡 → 请求被分散到不同 Pod
  → 不同 Pod 各自维护独立 KV Cache
  → 相同前缀在不同 Pod 重复计算
  → TTFT 恶化, GPU 资源浪费
```

**典型场景：**

| 场景 | 前缀特征 | 缓存失效影响 |
|------|---------|-------------|
| 多租户 B2B SaaS | 每客户 6K token system prompt | 150 客户 × 5 用户 = 750 sessions |
| Agent 循环 | 100:1 input/output ratio | 每轮重复完整上下文 |
| 多轮对话 | 对话历史持续增长 | 新轮次需要重新 prefill 全部历史 |

### 2.2 解决思路

```
传统路由:  request → round_robin → Pod (可能 cache miss)
KVC-Aware: request → tokenize → hash → 查全局索引 → Pod (cache hit)
```

核心思想：**在路由层维护全局 KV Cache 索引，将请求精准路由到已持有相关缓存块的 deployment。**

---

## 3. Industry Analysis: llm-d vs Dynamo

### 3.1 共同范式

两个系统遵循同一架构范式：**Event-Driven Index + Affinity Scoring**

```
┌──────────┐    events     ┌──────────────┐   query   ┌────────┐
│ Inference │ ──────────►  │  Global KV    │ ────────► │ Scorer │
│ Engines   │  (实时流)    │  Index        │           │        │
│ (vLLM...) │              │ (radix tree / │           │ affinity│
└──────────┘              │  block map)   │           │ score  │
                          └──────────────┘           └───┬────┘
                                                      ┌────┴─────┐
                                                      │ Fusion   │
                                                      │ Combiner │
                                                      │ cache×w₁ │
                                                      │ +load×w₂ │
                                                      └──────────┘
```

**五个共同点：**

| # | 共性 | llm-d | Dynamo |
|---|------|-------|--------|
| 1 | 事件驱动索引 | KVEvents 流 (block create/evict) | Event Plane (NATS/ZMQ) |
| 2 | 全局前缀→Worker 映射 | `kvcache.Index` (token prefix → pod) | Global radix tree / Positional Index |
| 3 | Affinity 评分 | Precise Prefix-Cache Scorer | Cache overlap score |
| 4 | 分数融合 | cache_score + load_score 加权 | cache hit rate + load balance |
| 5 | 两层架构 | `kvevents.Pool`(block级) + `kvcache.Index`(prefix级) | KVBM(block管理) + Flash Indexer(路由) |

### 3.2 关键差异

| 维度 | llm-d | Dynamo |
|------|-------|--------|
| **定位** | K8s-native 调度框架，坐在 vLLM 上层 | 全栈分布式推理运行时 |
| **语言** | Go (indexer) + Python CGO (chat template) | Rust (全栈) |
| **Tokenize** | HuggingFace Rust tokenizer (Go binding) + Python CGO | Rust 原生 / 引擎 Python |
| **事件传输** | ZMQ (vLLM 原生) | NATS / ZMQ (可切换) |
| **多级存储** | 规划中 (CPU offload) | 已落地 (GPU→CPU→SSD→Remote) |
| **Index 数据结构** | InMemoryIndex (golang-lru) / Redis | Flash Indexer (Vec<DashMap> + Jump Search) |
| **性能** | 未公开 ops/s | 170M ops/s |
| **引擎支持** | vLLM | vLLM + SGLang + TRT-LLM |

### 3.3 Tokenize 方案对比

| 挑战 | llm-d | Dynamo | Gateway 方案 |
|------|-------|--------|-------------|
| Tokenize | Go + HF Rust tokenizer + Python CGO | Rust 原生 / 引擎 Python | Rust `tokenizers` crate |
| Chat Template | Python CGO (vLLM 模板) | Rust 原生 (Option A) | `tokenizers.encode_chat()` |
| Hash 对齐 | FNV-64a + CBOR, 配置 hashSeed | 与引擎同进程，天然对齐 | `xxhash_cbor` 模式 |
| 不支持模型 | 回退近似路由 | 回退 round-robin | 回退 `KeyAffinityPolicy` |

### 3.4 Dynamo Flash Indexer 演进（6 次迭代）

| 迭代 | 数据结构 | 复杂度 | 瓶颈 |
|------|---------|--------|------|
| 1 | Python nested dict | O(W×D) | 不可用 |
| 2 | Rust HashMap + Actor | O(D) | 单线程 |
| 3 | Inverted Index | O(D+W) | remove 昂贵 |
| 4 | Radix Tree | O(D) | pointer chasing, 单线程 |
| 5 | Concurrent Radix Tree | O(D) | Arc<RwLock> + DashMap 并行 |
| 6 | **Positional + Jump Search** | **O(D/J + W)** | **170M ops/s** |

### 3.5 Dynamo KV Events 三层架构

```
Layer 3: Event Plane (传输层)
  NATS (分布式) 或 ZMQ (单机)
  Pub/Sub, 自动发现, 断线重连

Layer 2: KVBM (KV Block Manager) — 事件产生层
  RAII PublishHandle: register → StoreEvent, drop → RemoveEvent
  4 级存储池: GPU(G1) → CPU(G2) → SSD(G3) → Remote(G4)
  每级存储的 register/drop 都触发事件

Layer 1: Inference Engine (事件源)
  vLLM / SGLang / TRT-LLM → 通过 Connector API 集成
  引擎内部的 block lifecycle 触发 KVBM 操作
```

**Multi-tier 事件流：**

```
GPU(G1) block 注册 → StoreEvent(tier="G1")
KVBM offload 到 CPU:
  RemoveEvent(tier="G1") + StoreEvent(tier="G2")
KVBM offload 到 SSD:
  RemoveEvent(tier="G2") + StoreEvent(tier="G3")

Router 可用 tier 信息做更智能的路由:
  优先路由到 G1(GPU) 命中的 worker
  其次 G2(CPU)
  再次 G3(SSD)
```

---

## 4. Architecture Design

### 4.1 整体架构

```
┌─ Dynamo 集群 ─────────────────────────────────────────────────┐
│                                                                │
│  Worker-0 (vLLM)  Worker-1 (vLLM)  ...  Worker-N (SGLang)     │
│    │ KVBM             │ KVBM                  │ KVBM           │
│    │ PublishHandle    │ PublishHandle         │ PublishHandle  │
│    │ RAII             │ RAII                  │ RAII           │
│    ▼                  ▼                       ▼                │
│  ┌──────────────────────────────────────────────────────┐      │
│  │              Event Plane (NATS / ZMQ)                 │      │
│  └──────────────────────┬───────────────────────────────┘      │
│                         │ Subscribe                             │
└─────────────────────────┼──────────────────────────────────────┘
                          │
                          ▼
┌─ BooM Gateway ────────────────────────────────────────────────┐
│                                                                │
│  KvEventSubscriber (NATS/ZMQ consumer)                         │
│    │ decode StoreEvent / RemoveEvent / LoadMetrics              │
│    ▼                                                           │
│  KvEventDispatcher (sticky routing → thread pool)               │
│    │ FNV-1a(worker_id) % N → 固定线程                           │
│    ▼                                                           │
│  PositionalIndex  (Vec<DashMap<LocalHash, SeqEntry>>)           │
│    │ + Jump Search (O(D/64 + W))                                │
│    │ + Multi-tier scoring (G1 > G2 > G3 > G4)                  │
│    ▼                                                           │
│  KvAwarePolicy::select_with_context()                           │
│    │ find_matches() → Vec<MatchResult>                          │
│    ▼                                                           │
│  provider.chat_stream(req)                                      │
└────────────────────────────────────────────────────────────────┘
```

### 4.2 请求处理全流程

```
用户请求 POST /v1/chat/completions
       │
       ▼
  ┌─────────────────────────────────────────────────────────┐
  │ routes.rs                                               │
  │                                                         │
  │ 1. 认证 + 限流（现有逻辑不变）                             │
  │                                                         │
  │ 2. Tokenize（新增，< 1ms）                                │
  │    tokenizer_pool.encode_and_hash(model, &messages)      │
  │      → Vec<(usize, u64)>  // (position, local_hash)      │
  │                                                         │
  │ 3. KV-Aware 路由（新增，< 0.1ms）                         │
  │    router.select_provider_with_prefix(                   │
  │      model, key_hash, input_chars, &prefix_hashes)       │
  │                                                         │
  │ 4. Flow Control（现有逻辑不变）                            │
  │                                                         │
  │ 5. Provider dispatch（现有逻辑不变）                       │
  └─────────────────────────────────────────────────────────┘

后台事件流（异步，不阻塞请求）:
  Dynamo Workers → Event Plane → KvEventSubscriber
    → KvEventDispatcher (sticky) → PositionalIndex 实时更新
```

---

## 5. Tokenizer & Hash Alignment

### 5.1 三层对齐清单

```
┌─────────────────────────────────────────────────────────────┐
│ Layer 1: Chat Template                                      │
│                                                             │
│ Python: tokenizer.apply_chat_template(messages)             │
│ Rust:   tokenizers.encode_chat(messages)                    │
│                                                             │
│ 一致性: 同一个 minijinja 模板引擎                            │
│ 前提: 加载同一个 tokenizer_config.json 中的 template          │
├─────────────────────────────────────────────────────────────┤
│ Layer 2: Tokenize                                           │
│                                                             │
│ Python: tokenizer.encode(text) → Rust tokenizers crate      │
│ Rust:   tokenizer.encode(text) → 同一个 Rust crate          │
│                                                             │
│ 一致性: 底层是同一个 Rust 二进制（HuggingFace tokenizers）    │
├─────────────────────────────────────────────────────────────┤
│ Layer 3: Block Hash                                         │
│                                                             │
│ vLLM:   sha256_cbor(serialize(parent_hash, chunk, extra))   │
│ Rust:   sha256(cbor_encode([parent_hash, chunk, extra]))    │
│                                                             │
│ 一致性: 前提是 vLLM 启动时加 --prefix-caching-hash-algo     │
│         sha256_cbor 或 xxhash_cbor                           │
│ 不一致: vLLM 默认 sha256 (Pickle) 无法跨语言复现             │
└─────────────────────────────────────────────────────────────┘
```

### 5.2 Hash 算法选择

| 算法 | 速度 | 跨语言 | 碰撞安全 | 推荐 |
|------|------|--------|---------|------|
| `sha256` (Pickle) | ~500 MB/s | 否 | 强 | 不推荐 |
| `sha256_cbor` | ~500 MB/s | 是 | 强 | 可用 |
| `xxhash` (Pickle) | ~12 GB/s | 否 | 弱 | 不推荐 |
| **`xxhash_cbor`** | **~12 GB/s** | **是** | **足够** | **推荐** |

vLLM 启动参数：
```bash
vllm serve --prefix-caching-hash-algo xxhash_cbor
```

### 5.3 TokenizerPool 设计

```rust
// boom-kvindex/src/tokenizer.rs

pub struct TokenizerPool {
    tokenizers: DashMap<String, Arc<Tokenizer>>,
    block_size: u32,
    tokenizer_dir: String,
}

impl TokenizerPool {
    /// 组合方法：encode + compute_block_hashes
    pub fn encode_and_hash(
        &self,
        model: &str,
        messages: &[Message],
    ) -> Option<Vec<(usize, u64)>> {
        let tokenizer = self.get_or_load(model)?;

        // 使用 HuggingFace tokenizers crate 内置的 encode_chat
        // 与 Python 端 apply_chat_template 输出一致
        let encoding = tokenizer.encode_chat(
            &messages_to_chat_requests(messages),
            false,  // add_generation_prompt
        )?;
        let token_ids = encoding.get_ids();

        // 切块 + xxhash
        let block_size = self.block_size as usize;
        let full_blocks = token_ids.len() / block_size;

        let hashes: Vec<(usize, u64)> = (0..full_blocks)
            .map(|i| {
                let block = &token_ids[i * block_size..(i + 1) * block_size];
                (i, xxhash_block(block))
            })
            .collect();

        Some(hashes)
    }
}
```

---

## 6. Event System Design

### 6.1 Dynamo KV Event Schema（Gateway 侧）

```rust
// boom-core/src/kv_event.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type")]
pub enum DynamoKvEvent {
    /// Block 注册（RAII PublishHandle 创建时触发）
    #[serde(rename = "store")]
    Store {
        worker_id: String,
        sequence_hash: u64,      // 滚动 hash (position-dependent)
        prefix_hash: u64,        // 父 block hash
        local_hash: u64,          // chunk hash (position-independent)
        block_index: u32,         // 序列中的位置
        storage_tier: String,     // G1=GPU, G2=CPU, G3=SSD, G4=Remote
    },

    /// Block 驱逐（RAII PublishHandle Drop 时触发）
    #[serde(rename = "remove")]
    Remove {
        worker_id: String,
        sequence_hash: u64,
        storage_tier: String,
    },

    /// 负载指标
    #[serde(rename = "load_metrics")]
    LoadMetrics {
        worker_id: String,
        queue_depth: u32,
        kv_utilization: f32,
        running_requests: u32,
        tier_usage: HashMap<String, u32>,
    },
}
```

### 6.2 Transport Layer

**EventTransport trait — 可插拔 NATS / ZMQ：**

```rust
// boom-kvindex/src/transport/mod.rs

#[async_trait]
pub trait EventTransport: Send + Sync {
    async fn subscribe(&self, namespace: &str) -> Result<(), TransportError>;
    async fn recv(&self) -> Result<DynamoEventBatch, TransportError>;
    async fn shutdown(&self);
}
```

**自动选择逻辑：**

| Discovery Backend | Default Transport |
|-------------------|-------------------|
| file / mem (本地) | ZMQ (无需外部服务) |
| etcd / kubernetes | NATS (需 NATS server) |

### 6.3 Sticky Routing Dispatcher

```rust
// boom-kvindex/src/dispatcher.rs

pub struct KvEventDispatcher {
    workers: Vec<Sender<DispatchItem>>,
    num_threads: usize,
}

impl KvEventDispatcher {
    /// 分派事件到固定线程（FNV-1a sticky routing）
    /// 同一 worker 的事件始终由同一线程处理 → 零写冲突
    pub fn dispatch(&self, worker_id: &str, event: DynamoKvEvent) {
        let idx = fnv1a(worker_id) % self.num_threads;
        let _ = self.workers[idx].send(DispatchItem {
            worker_id: worker_id.to_string(),
            event,
        });
    }
}

fn fnv1a(s: &str) -> usize {
    let mut hash: u64 = 2166136261;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(16777619);
    }
    hash as usize
}
```

### 6.4 Worker Discovery

```rust
// boom-kvindex/src/discovery.rs

/// 自动发现 Dynamo workers
/// 支持: etcd / Kubernetes / file(静态)
pub struct WorkerDiscovery {
    transport_config: TransportConfig,
    discovered: DashMap<String, WorkerEndpoint>,
}

impl WorkerDiscovery {
    /// 后台 task: 监听 Discovery Plane
    /// - New worker ready → connect + start consuming
    /// - Worker gone → disconnect + cleanup index
    pub async fn run(&self, dispatcher: Arc<KvEventDispatcher>);
}
```

---

## 7. Positional Index with Jump Search

### 7.1 数据结构

```rust
// boom-kvindex/src/backend/positional.rs

type LocalHash = u64;
type SeqHash = u64;
type WorkerId = String;

/// 存储层级优先级
fn tier_priority(tier: &str) -> f32 {
    match tier {
        "G1" => 1.0,   // GPU — 最快
        "G2" => 0.6,   // CPU — 需要 D2H
        "G3" => 0.3,   // SSD — 需要 disk read + D2H
        "G4" => 0.1,   // Remote — 最慢
        _ => 0.2,
    }
}

/// 位置索引的单条 entry — 处理 chunk hash 碰撞
enum SeqEntry {
    /// 无碰撞（常见）：inline 存储，零 HashMap 分配
    Single {
        seq_hash: SeqHash,
        workers: HashMap<WorkerId, TierInfo>,
    },
    /// 碰撞（罕见）：多个 seq_hash 共享 (position, local_hash)
    Multi {
        entries: HashMap<SeqHash, HashMap<WorkerId, TierInfo>>,
    },
}

struct TierInfo {
    tier: String,
    registered_at: std::time::Instant,
}

pub struct PositionalIndex {
    /// 正向索引: position → (local_hash → SeqEntry)
    /// Vec 索引即位置，O(1) 随机访问
    positions: Vec<DashMap<LocalHash, SeqEntry>>,

    /// 反向索引: worker_id → RwLock<HashMap<seq_hash, Position>>
    /// RemoveEvent O(1) 定位
    reverse: DashMap<WorkerId, RwLock<HashMap<SeqHash, usize>>>,

    /// 负载快照
    loads: DashMap<WorkerId, LoadState>,

    /// Jump search 步长
    jump_size: usize,  // default: 64

    /// 最大深度
    max_depth: usize,  // 128K / 16 = 8192
}
```

### 7.2 Jump Search 算法

```
O(D/J + W), J = jump_size (default 64)

1. 初始化: 从 position 0 收集所有活跃 workers
2. 跳跃: 直接检查 pos + jump_size 处的检查点
3. 全部通过: 确认整个跳跃区间, 继续跳
4. 部分失配: 线性回扫 [pos+1..checkpoint], 找精确淘汰点
5. 重复直到所有 position 检查完毕
```

**性能对比：**

```
场景: 375 blocks (6K token 前缀), 500 workers

Flat DashMap:     375 × ~50 lookups = ~37μs
Radix Tree:      375 × pointer chase = ~19μs
Jump Search:     375/64 = 6 jumps + ~5 回扫 = ~0.5μs

场景: 8000 blocks (128K context), 500 workers

Flat DashMap:     8000 × 50 = ~800μs
Jump Search:     8000/64 + 500 = ~25μs  (32x faster)
```

### 7.3 Multi-Tier Scoring

```rust
pub struct MatchResult {
    pub worker_id: WorkerId,
    pub match_depth: usize,
    pub hit_ratio: f32,
    pub best_tier: String,       // 最佳存储层级 (G1 优先)
    pub tier_score: f32,
    pub load_score: f32,
    pub combined_score: f32,
}

// combined_score = cache_weight × hit_ratio
//               + load_weight × load_score
//               + tier_weight × tier_score
```

---

## 8. SchedulePolicy Integration

### 8.1 Trait Extension

```rust
// boom-routing/src/policy/mod.rs

pub trait SchedulePolicy: Send + Sync {
    /// 旧签名（向后兼容）
    fn select(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        input_chars: u64,
    ) -> Option<Arc<dyn Provider>> {
        self.select_with_context(model, candidates, key_hash, input_chars, &[])
    }

    /// 新签名：支持 prefix block hashes
    fn select_with_context(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        input_chars: u64,
        prefix_block_hashes: &[(usize, u64)],
    ) -> Option<Arc<dyn Provider>> {
        self.select(model, candidates, key_hash, input_chars)
    }

    fn name(&self) -> &str;
}
```

### 8.2 Router Extension

```rust
// boom-routing/src/router.rs

impl Router {
    /// 新增：带 prefix block hashes 的路由
    pub fn select_provider_with_prefix(
        &self,
        model: &str,
        key_hash: Option<&str>,
        input_chars: u64,
        prefix_block_hashes: &[(usize, u64)],
    ) -> Option<Arc<dyn Provider>> {
        let candidates = self.resolve_candidates(model)?;
        self.policy.load().inner.select_with_context(
            model, &candidates, key_hash, input_chars, prefix_block_hashes,
        )
    }
}
```

### 8.3 KvAwarePolicy

```rust
// boom-routing/src/policy/kv_aware.rs

pub struct KvAwarePolicy {
    kv_index: Arc<PositionalIndex>,
    tracker: Arc<InFlightTracker>,
    queue_info: Option<Arc<dyn DeploymentQueueInfo>>,
    cache_weight: f32,
    load_weight: f32,
    tier_weight: f32,
}

impl SchedulePolicy for KvAwarePolicy {
    fn select_with_context(
        &self,
        model: &str,
        candidates: &[Arc<dyn Provider>],
        key_hash: Option<&str>,
        input_chars: u64,
        prefix_block_hashes: &[(usize, u64)],
    ) -> Option<Arc<dyn Provider>> {
        // 1. 收集 candidate deployment_id → worker_id
        // 2. Jump Search + Multi-Tier Scoring
        // 3. 选 best combined_score
        // 4. 无命中 → 退化 select_lowest_load()
    }

    fn name(&self) -> &str { "kv_aware" }
}
```

### 8.4 Routes.rs Integration

```rust
// boom-main/src/routes.rs — chat_completions_inner

// 现有: 字符级计数（保留）
let input_chars: usize = req.messages.iter().map(|m| ...).sum();

// 新增: Token 级 prefix block hashes
let prefix_block_hashes = state.tokenizer_pool
    .encode_and_hash(&req.model, &req.messages)
    .unwrap_or_default();

// 路由选择
let provider = state.router
    .select_provider_with_prefix(
        &req.model,
        Some(&identity.key_hash),
        input_chars as u64,
        &prefix_block_hashes,
    )
    .ok_or_else(|| GatewayError::ModelNotFound(req.model.clone()))?;
```

---

## 9. Configuration

```yaml
# config.yaml
router_settings:
  schedule_policy: kv_aware

  kv_aware:
    enabled: true

    # ── Index 配置 ──
    index_backend: positional       # flat | positional
    jump_size: 64                   # Jump search 步长
    max_depth: 8192                 # 128K context / 16 block_size
    dispatcher_threads: 16          # Sticky routing 线程数

    # ── 评分权重 ──
    cache_weight: 0.5               # 缓存命中率权重
    load_weight: 0.2                # 负载权重
    tier_weight: 0.3                # 存储层级权重 (G1 > G2 > G3 > G4)

    # ── Event Plane ──
    event_plane: nats               # nats | zmq
    namespace: default              # Dynamo namespace
    nats_url: "nats://nats.default.svc:4222"

    # ── Tokenizer ──
    tokenizer_dir: /data/tokenizers
    block_size: 16

    # ── ZMQ 模式 (event_plane=zmq 时) ──
    # pod_endpoints:
    #   vllm-deploy-1: "tcp://10.0.1.1:5557"
    #   vllm-deploy-2: "tcp://10.0.1.2:5557"
```

---

## 10. Module Dependencies

```
boom-core
  └─ 新增 DynamoKvEvent enum, KvEventError

boom-kvindex (新 crate, 只依赖 boom-core)
  ├─ boom-core
  ├─ dashmap
  ├─ parking_lot
  ├─ crossbeam (channel for dispatcher)
  ├─ tokenizers (HuggingFace Rust tokenizer)
  ├─ twox-hash (xxhash)
  ├─ rmp-serde (msgpack)
  ├─ serde / serde_json
  ├─ thiserror
  ├─ tracing
  ├─ zmq (optional, feature gate)
  ├─ async-nats (optional, feature gate)
  └─ tokio

  [features]
  default = ["nats", "zmq"]
  nats = ["dep:async-nats"]
  zmq = ["dep:zmq"]
  http-only = []

boom-routing
  ├─ SchedulePolicy trait 扩展 (select_with_context)
  └─ KvAwarePolicy (可选，依赖 boom-kvindex)

boom-main
  ├─ 依赖 boom-kvindex
  ├─ AppState 新增字段
  ├─ 启动初始化 (transport + dispatcher + backend)
  └─ /internal/kv-events endpoint (HTTP fallback)
```

符合架构原则：boom-kvindex 只依赖 boom-core，无循环依赖。

---

## 11. Performance Budget

### 11.1 规模测算

```
500 Pods × H100 (80GB)
  单 Pod blocks ≈ 6,700
  集群总 blocks ≈ 3.35M
  去重后唯一 entries ≈ 500K ~ 1M

索引内存:
  正向索引: ~50MB
  反向索引: ~54MB
  总计 ≈ 100-150MB (完全可接受)
```

### 11.2 延迟预算

| 操作 | 目标延迟 | 实现 |
|------|---------|------|
| Tokenize 6K tokens | < 1ms | `tokenizers` crate (Rust) |
| Compute 375 block hashes | < 0.05ms | xxhash_cbor |
| Jump Search (8 candidates × 375 blocks) | < 0.1ms | Vec<DashMap> |
| **总路由额外开销** | **< 2ms** | |
| NATS 收事件 (batch) | ~0.5ms | 批量 50-200 events |
| Dispatch 到线程 | ~0.1μs | crossbeam channel |
| PositionalIndex 写入 (1 event) | ~0.2μs | DashMap insert |

### 11.3 收益对比

| 指标 | 无 KV-Aware | KVC-Aware | 改善 |
|------|------------|-----------|------|
| TTFT P90 | 92s | 0.54s | 170x |
| 吞吐 | 4,429 tok/s | 8,730 tok/s | 2x |
| GPU 利用率 | 大量重复 prefill | 最大化 cache 复用 | 显著提升 |

---

## 12. Deployment Topology

### 12.1 模式 A: Gateway → Dynamo Frontend → Workers

```
Gateway (认证/限流/计费) → Dynamo Frontend (KV-Aware 路由) → Workers
```

- 不需要自建 PositionalIndex
- 直接利用 Dynamo 的 Flash Indexer
- 适合已部署 Dynamo 全栈的场景

### 12.2 模式 B: Gateway → Workers 直连（本设计）

```
                         ┌─ NATS Server ─┐
                         │                │
Dynamo Workers ──publish─►                ◄──subscribe── Gateway
                         │                │
                         └────────────────┘

Gateway:
  认证/限流/计费 + KV-Aware 路由
  订阅 Event Plane 构建自有 PositionalIndex
  直连 Worker Pods
```

### 12.3 模式 C: 混合

```
Gateway (认证/限流/计费)
  ├─ 有 Dynamo 的 model → 模式 B (直连 + Event Plane)
  └─ 纯 vLLM 的 model  → ZMQ 直连 vLLM 原生 events
```

---

## 13. Implementation Roadmap

| Phase | 内容 | 依赖 |
|-------|------|------|
| **0** | `boom-core` 新增 `DynamoKvEvent` / `KvEventError` 类型 | 无 |
| **1** | `boom-kvindex` crate: `KvIndexBackend` trait + `FlatIndex` | Phase 0, dashmap |
| **2** | `KvAwarePolicy` + `TokenizerPool` + routes.rs 集成 | Phase 1, tokenizers |
| **3** | HTTP `/internal/kv-events` endpoint (非 Dynamo 场景 fallback) | Phase 1 |
| **4** | `KvEventDispatcher` (sticky routing thread pool) | Phase 1, crossbeam |
| **5** | `ZmqTransport` (vLLM 原生事件直连) | Phase 4, zmq crate |
| **6** | `NatsTransport` (Dynamo Event Plane) | Phase 4, async-nats |
| **7** | `PositionalIndex` + Jump Search (500+ Pods) | Phase 1 |
| **8** | Multi-tier scoring (G1-G4 存储感知) | Phase 7 |
| **9** | `WorkerDiscovery` (K8s/etcd 自动发现) | Phase 6, kube-rs |
| **10** | `StorageAdvisor` 集成 (智能 tier 迁移建议) | Phase 8 |

**MVP (Phase 0-3)** 可以先用 HTTP fallback 跑通全链路，再逐步接入 ZMQ 和 NATS。

---

## 14. References

### Industry Systems

- [llm-d KV-Cache Indexer Architecture](https://github.com/llm-d/llm-d-kv-cache-manager/blob/main/docs/architecture.md)
- [llm-d: KV-Cache Wins You Can See](https://llm-d.ai/blog/kvcache-wins-you-can-see)
- [Red Hat: Master KV Cache Aware Routing with llm-d](https://developers.redhat.com/articles/2025/10/07/master-kv-cache-aware-routing-llm-d-efficient-ai-inference)
- [llm-d: Predicted-Latency Based Scheduling](https://llm-d.ai/blog/predicted-latency-based-scheduling-for-llms)
- [IBM Research: KV-Cache Wins You Can Feel](https://research.ibm.com/publications/kv-cache-wins-you-can-feel-building-ai-aware-llm-routing-on-kubernetes)

### NVIDIA Dynamo

- [Dynamo Overall Architecture](https://docs.nvidia.com/dynamo/v-0-9-1/design-docs/overall-architecture)
- [Dynamo Disaggregated Serving](https://docs.dynamo.nvidia.com/dynamo/design-docs/disaggregated-serving)
- [Dynamo Flash Indexer](https://docs.nvidia.com/dynamo/dev/digest/flash-indexer)
- [Dynamo KVBM Design](https://docs.nvidia.com/dynamo/design-docs/component-design/kvbm-design)
- [Dynamo Event Plane](https://docs.nvidia.com/dynamo/design-docs/communication-planes/event-plane)
- [Dynamo KV Events for Custom Engines](https://docs.nvidia.com/dynamo/integrations/kv-events-for-custom-engines)
- [Dynamo Chat Processor Options](https://docs.nvidia.com/dynamo/v1.1.0/user-guides/chat-processors)

### vLLM

- [vLLM Automatic Prefix Caching](https://docs.vllm.ai/en/v0.17.1/design/prefix_caching/)
- [vLLM KV Events API](https://docs.vllm.ai/en/latest/api/vllm/config/kv_events/)
- [vLLM kv_events Module](https://docs.vllm.ai/en/latest/api/vllm/distributed/kv_events/)
- [vLLM KV Events Subscriber Example](https://docs.vllm.ai/en/stable/examples/online_serving/kv_events_subscriber/)

### Academic

- [Online Scheduling for LLM Inference with KV Cache (arXiv)](https://arxiv.org/html/2502.07115v5)
- [Predictive Multi-Tier Memory Management for KV Cache (arXiv)](https://arxiv.org/html/2604.26968v1)
