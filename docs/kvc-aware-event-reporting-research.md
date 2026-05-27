# KV Cache 事件上报机制调研：NVIDIA Dynamo、llm-d 与业界现状

> 调研时间：2026-05-25
> 目的：为本网关 KVC-Aware 路由的下一步演进提供参考

---

## 1. NVIDIA Dynamo

Dynamo 是目前架构最完整的 KV-aware 路由系统，支持从单机到分布式 K8s 的多种部署形态。

### 1.1 事件上报架构：三层通信平面

Dynamo 将组件间通信拆分为三个独立的平面（Communication Planes）：

| 平面 | 用途 | 传输方式 |
|------|------|---------|
| **Request Plane** | 请求路由（Frontend → Worker） | TCP / HTTP / NATS |
| **Event Plane** | KV cache 事件 + 负载指标上报 | ZMQ 或 NATS |
| **Discovery Plane** | Worker 注册与发现 | etcd / Kubernetes / file / mem |

#### Event Plane 两种传输模式

```
模式一：ZMQ（推荐用于本地/裸金属部署）
  Worker ──ZMQ PUB socket──► Subscriber (Local Indexer)
  - 无需外部服务
  - Worker 绑定 ZMQ PUB socket，通过 Discovery 广播地址
  - Subscriber 自动发现并连接所有活跃 publisher
  - Worker 扩缩容时 subscriber 动态调整连接

模式二：NATS（推荐用于分布式/K8s 部署）
  Worker ──publish──► NATS subject (namespace/component 划分) ──► Subscriber
  - 需要 NATS 服务器
  - 内建重连和断线消息缓冲
  - 支持 JetStream 持久化模式（deprecated，将逐步移除）
```

自动选择逻辑：
- `--discovery-backend file/mem`（本地后端）→ 默认 ZMQ
- `--discovery-backend etcd/kubernetes`（分布式后端）→ 默认 NATS
- 可通过 `DYN_EVENT_PLANE` 环境变量显式覆盖

#### 无事件模式（Prediction-based Fallback）

```bash
--no-router-kv-events
```

Worker 不上报任何 KV 事件，路由器根据自身路由历史 **预测** 缓存状态：
- TTL 过期（默认 120s）自动淘汰预测的 block
- LRU 淘汰 + `--router-max-tree-size` 限制内存上限
- 适用于后端不支持 KV 事件或事件准确度不可信的场景

### 1.2 索引数据结构：Radix Tree

Dynamo 不用简单 HashMap，而是用 **Radix Tree（基数树）** 存储 KV block 索引。

**为什么用 Radix Tree 而不是 HashMap？**

1. **天然前缀共享**：树的结构本身就编码了 token 前缀关系，父子节点天然代表 block 的连续前缀
2. **高效前缀查询**：查找最长连续前缀匹配是 O(L)（L = 前缀长度），不需要逐 block 线性扫描
3. **空间效率**：共享前缀的多个 block 只存一份公共路径，节省内存

Dynamo 提供两种实现：

| 实现 | 并发模型 | 适用场景 |
|------|---------|---------|
| `RadixTree` | 单线程 + channel 事件处理 | `--router-event-threads 1` |
| `ConcurrentRadixTree` | 线程池 + sticky-routed writes | `--router-event-threads > 1`（默认 4） |

两种都支持 `PositionalIndexer`：在 radix tree 之上维护位置感知索引，用于快速定位某个 worker 持有的特定位置 block。

### 1.3 事件类型

Worker 上报的事件（与 TRT-LLM KV Cache Event API 对齐）：

```python
struct KVCacheEvent:
    event_id: long        # 自增 ID
    data: variant         # 事件数据

struct StoredBlockData:
    blockHash: id         # block 唯一标识
    tokens: list          # block 中的 token 列表
    loraId: id            # LoRA adapter ID
    cacheLevel: int       # 缓存层级 (0=primary/GPU, 1=secondary/CPU)
    priority: int         # 优先级

struct StoredData:
    parentHash: optional  # 父 block（前缀链）
    blocks: list          # 存储的 block 列表

struct RemovedData:
    blockHashes: list     # 被驱逐的 block hash 列表
```

### 1.4 路由成本计算

```
cost = overlap_score_weight × prefill_blocks + decode_blocks
```

| 参数 | 含义 | 调优方向 |
|------|------|---------|
| `prefill_blocks` | 需要新计算的 block = input_blocks - cached_blocks | 越少越好（缓存命中多） |
| `decode_blocks` | 当前活跃 decode block 数（负载指标） | 越少越好（负载低） |
| `overlap_score_weight` | 权重，默认 1.0 | **高值** → 偏向缓存复用（优化 TTFT）；**低值** → 偏向负载均衡（优化 ITL） |

示例（`overlap_score_weight = 1.0`）：
- Worker 1: `cost = 1.0 × 8 + 10 = 18`
- Worker 2: `cost = 1.0 × 5 + 5 = 10` ← **选中**（成本最低）
- Worker 3: `cost = 1.0 × 2 + 9 = 11`

### 1.5 高级特性

| 特性 | 说明 |
|------|------|
| **优先级队列** | `--router-queue-threshold` 控制请求暂存，`--router-queue-policy` 支持 FCFS（优化尾 TTFT）或 WSPT（优化平均 TTFT） |
| **Temperature 采样** | `--router-temperature` > 0 时用 softmax 采样替代确定性最低成本选择 |
| **多副本同步** | `--router-replica-sync` 通过 NATS core 共享 active block 状态 |
| **JetStream 持久化** | 前缀 block 状态持久化到 NATS JetStream + object store snapshot（deprecated） |
| **Output block 追踪** | `--router-track-output-blocks`（实验性）追踪 decode 阶段产生的 block |
| **KV reuse 假设** | `--no-router-assume-kv-reuse` 在 disaggregated 模式下禁用去重假设 |
| **Disaggregated serving** | 自动检测 prefill worker 并激活 prefill router，prefill/decode 分离 |
| **AIC Prefill Load Model** | 用 AIC 预估 prefill 时间，更精确的负载建模 |

### 1.6 持久化与恢复

| 模式 | 持久化行为 |
|------|----------|
| **Local Indexer**（默认） | Worker 侧持久化状态；Router 重启时查询各 Worker 重建 |
| **JetStream**（deprecated） | JetStream 1h 保留 + NATS Object Store snapshot |
| **No KV Events** | 无持久化，纯内存 TTL 预测 |

---

## 2. llm-d

llm-d（IBM / Red Hat / Google 联合，Kubernetes-native）走的是云原生的 EPP + Envoy 路线。

### 2.1 事件上报架构

```
vLLM Pod
  │ ZMQ publish (topic: kv@{pod-id}@{model})
  │ payload: msgpack 批量事件
  ▼
kvevents.Pool (sharded ZMQ worker pool)
  │ Pod ID → hash → 固定 shard（保证同一 Pod 事件有序）
  │ decode msgpack
  ▼
kvblock.Index (in-memory two-level LRU cache)
  │ block_hash → Set<pod_id>
  │ 两级 LRU：hot layer + warm layer
  ▼
kvcache.Indexer (主协调器)
  │
  ▼
EPP (External Processing Pod, Envoy ext_proc gRPC filter)
  │ 拦截请求 → tokenize → 查 index → 打分 → 选最优 pod
  │
  ▼
Gateway API Inference Extension → 路由到选中的 vLLM Pod
```

### 2.2 核心组件

| 组件 | 实现 | 说明 |
|------|------|------|
| `kvcache.Indexer` | 主协调器 | 处理评分请求，协调所有内部模块 |
| `kvevents.Pool` | 分片 ZMQ worker 池 | 实时消费 vLLM KV cache 事件 |
| `kvblock.Index` | 两级 LRU 缓存 | block hash → pod 映射，亚毫秒查询 |
| `tokenization.PrefixStore` | LRU 缓存 | 缓存已 tokenize 的 prompt 前缀，避免重复 tokenize |
| `kvblock.TokenProcessor` | 切分 + 哈希 | 与 vLLM 内部实现完全一致的 block 切分逻辑 |
| `kvblock.Scorer` | 连续前缀匹配 | 最长连续前缀命中策略 |

### 2.3 写路径（事件消费）

1. vLLM Pod 通过 ZMQ 发布 KV cache 事件（`BlockStored`、`BlockRemoved`）
2. `kvevents.Pool` 按 topic 格式 `kv@pod-id@model` 接收消息
3. Pod ID 哈希到固定 shard，**保证同一 Pod 的事件有序处理**
4. Worker 解码 msgpack 批量事件
5. 更新内存中的 `kvblock.Index`

### 2.4 读路径（Pod 评分）

1. 检查 `PrefixStore` 获取最长已缓存 token 序列
2. `TokenProcessor` 将 tokens 转为 KV block keys（匹配 vLLM 内部逻辑）
3. 查询 `kvblock.Index` 找到哪些 Pod 持有连续 block
4. `Scorer` 按连续前缀匹配数排名
5. 返回 Pod 排名给 EPP

### 2.5 关键设计决策

- **Gateway API Inference Extension (GAIE)** 标准：通过 Envoy ext_proc filter 拦截请求，利用 K8s 标准接口
- **EPP 不做 HTTP 路由**：只做 gRPC ext_proc 处理，路由由 Envoy 完成
- **vLLM 深度集成**：block 切分/哈希逻辑完全匹配 vLLM 内部实现
- **无外部依赖**：纯内存索引，不需要 etcd/NATS 等外部服务
- **Pod 自动发现**：通过 label selector `llm-d.ai/inferenceServing=true` 自动识别 decode Pod

### 2.6 实测数据（Red Hat 2025-10 报告）

| 指标 | 无 KV Cache 路由 | 有 KV Cache 路由 | 改善 |
|------|----------------|-----------------|------|
| TTFT（cold） | 2,850 ms | 2,850 ms | 基线 |
| TTFT（warm hit） | 2,850 ms（最差） | 340 ms | **88% 更快** |
| 缓存命中率 | — | 87.4% | — |
| GPU 内存利用率 | — | 90% | — |

Session affinity 导致 99.92% 流量集中在主 Pod（4,776 总请求中 4,772 去了主 Pod）。

---

## 3. TensorRT-LLM KV Cache Event API

TensorRT-LLM 是 NVIDIA 推理引擎层，它提供的 Event API 是 Dynamo 和 TRT-LLM 原生部署的底层事件源。

### 3.1 事件模型

```python
# 配置
kv_cache_config = KvCacheConfig(event_buffer_max_size=16384)
executor_config = ExecutorConfig(kv_cache_config)
executor = Executor(executor_config)

# 消费事件
event_manager = executor.getKvCacheEventManager()
events = event_manager.getLatestEvents()
```

### 3.2 事件类型

```python
struct KVCacheEvent:
    event_id: long              # 自增 event id
    data: StoredData | RemovedData

struct StoredBlockData:
    blockHash: id               # block 唯一 ID
    tokens: list                # block 中的 token 列表
    loraId: id                  # LoRA adapter ID
    cacheLevel: int             # 缓存层级 (0=primary, 1=secondary)
    priority: int               # 优先级（用于优先级驱逐）

struct StoredData:
    parentHash: optional<id>    # 父 block hash（编码前缀链）
    blocks: list<StoredBlockData>

struct RemovedData:
    blockHashes: list<id>       # 被驱逐的 block hash 列表
```

### 3.3 Priority-based Eviction

TRT-LLM 还支持 **优先级驱逐**，允许用户控制哪些 block 优先保留：

```python
struct TokenRangeRetentionConfig:
    start: int
    end: optional<int>
    priority: int               # 保留优先级
    duration: optional<int>     # 优先级持续时间

struct KvCacheRetentionConfig:
    ranges: list<TokenRangeRetentionConfig>
    decode_priority: optional<int>
    decode_duration: optional<int>
```

- **高优先级**（如 system prompt）的 block 几乎不会被驱逐
- **低优先级**（一次性请求）的 block 优先被驱逐
- 内部基准测试：优先级驱逐提升缓存命中率约 **20%**

### 3.4 语义保证

- **最终一致**：事件是异步的，消费者获得的是最终一致的 KV cache 视图
- **单 Executor 规模**：可预判哪些请求会有更多复用
- **多 Executor 聚合**：用于 KV-aware 路由和调度决策

---

## 4. vLLM 原生 Prefix Caching

vLLM 是目前最广泛使用的开源推理引擎，其 prefix caching 是 KV-aware 路由的基础。

### 4.1 机制

- **PagedAttention + Block 管理**：KV cache 按 block 组织（默认 block_size=16）
- **Hash-based 前缀匹配**：每个 block 计算 hash，相同 hash 的 block 可复用
- **Block 级粒度**：block 内任何一个 token 不同，整个 block 不可复用
- **Automatic Prefix Caching (APC)**：自动识别共享前缀，无需手动标记

### 4.2 KV 事件发布配置

```bash
# vLLM 启用 prefix caching（默认也自动启用 KV 事件发布）
--enable-prefix-caching

# 显式控制 KV 事件发布
--kv-events-config '{"enable_kv_cache_events": true, "publisher": "zmq", "endpoint": "tcp://*:5557"}'

# 禁用 KV 事件发布
--kv-events-config '{"enable_kv_cache_events": false}'
```

注意：vLLM 当前在 prefix caching 激活时默认发布 KV 事件，但这个行为将在未来版本中默认关闭。

---

## 5. 三方对比：本网关 vs Dynamo vs llm-d

| 维度 | **本网关 (FlatIndex)** | **NVIDIA Dynamo** | **llm-d** |
|------|----------------------|-------------------|-----------|
| **索引结构** | `DashMap<(model,hash), Set<worker>>` | Radix Tree（前缀树） | 两级 LRU Cache |
| **事件传输** | HTTP `POST /internal/kv-events` | ZMQ / NATS Event Plane | ZMQ (topic: `kv@pod-id@model`) |
| **序列化** | JSON | MessagePack | MessagePack |
| **Tokenizer 位置** | 网关侧（HuggingFace tokenizers） | Worker 上报 block hash，路由侧不 tokenize | EPP 侧（PrefixStore + vLLM 兼容切分） |
| **匹配策略** | 连续前缀 block 匹配 | 连续前缀匹配（Radix Tree 天然支持） | 连续前缀匹配 |
| **评分模型** | `cache_w × hit_ratio + tier_w × tier + load_w × load` | `overlap_weight × prefill + decode`（选成本最低） | Pod scoring by consecutive matches |
| **存储层级感知** | GPU/CPU/SSD/Remote 四级 | primary/secondary 两级 | 无 |
| **部署形态** | 独立网关进程 | 独立 runtime (Python) | K8s EPP + Envoy |
| **Fallback** | 最低负载选择 | 预测模式 (TTL + LRU) 或随机/轮询 | 最低负载 |
| **持久化** | 无（纯内存） | JetStream + snapshot / Worker 查询重建 | 无（纯内存） |
| **多副本同步** | 无 | NATS replica sync | EPP 状态共享 |
| **Disaggregated** | 不支持 | 自动检测 prefill worker 并激活 prefill router | 支持 |
| **优先级驱逐** | 不涉及 | 不涉及（TRT-LLM 引擎层能力） | 不涉及 |
| **预测模式** | 无 | 有（`--no-router-kv-events`） | 无 |

---

## 6. 业界趋势与建议

### 6.1 主流趋势

1. **Push 模式为主**：Worker 主动上报 KV cache 事件，Router 被动消费。vLLM、TRT-LLM、SGLang 都已原生支持事件发布。

2. **ZMQ + MessagePack 成为主流事件传输**：Dynamo 和 llm-d 都选择 ZMQ + MessagePack，亚毫秒延迟、无需外部服务。HTTP 接口（本网关当前方案）简单但在吞吐和延迟上不如 ZMQ。

3. **Radix Tree 渐成标配**：Dynamo 用 Radix Tree 替代简单 HashMap，天然支持前缀共享发现和高效前缀匹配。HashMap 方案需要逐 block 线性扫描验证连续性，Radix Tree 可以在树遍历中直接确定连续前缀深度。

4. **Disaggregated Serving（Prefill/Decode 分离）** 是下一代方向：Dynamo 和 llm-d 都支持 prefill 和 decode 在不同 GPU 池执行，KV cache 通过网络传输（UCCL / NCCL）。

5. **Gateway API Inference Extension (GAIE)** 成为 K8s 标准接口：llm-d 用 Envoy ext_proc 拦截请求做路由，避免自建 HTTP 路由层。

6. **预测模式作为零依赖降级**：Dynamo 的 `--no-router-kv-events` 模式值得参考 — 路由器根据自身路由历史预测缓存状态，TTL 过期淘汰，不需要 worker 上报任何事件。

### 6.2 量化收益

IBM Research 的研究结论：

> **命中 KV cache 的请求比冷启动便宜 10 倍，快 50 倍。**

Red Hat 实测（llm-d）：warm cache hit 时 TTFT 从 2850ms 降至 340ms（**快 88%**），缓存命中率 87.4%。

### 6.3 本网关可能的演进方向

| 阶段 | 改进 | 优先级 |
|------|------|--------|
| **短期** | 支持 ZMQ 事件接收（可选），兼容 vLLM 的 ZMQ KV 事件发布 | 中 |
| **短期** | 添加预测模式降级（不依赖 Worker 上报） | 高 |
| **中期** | 索引从 DashMap 升级为 Radix Tree，提升前缀查询效率 | 中 |
| **中期** | 支持 MessagePack 序列化（当前 JSON 开销较大） | 低 |
| **长期** | Prefill/Decode 分离路由 | 低（取决于业务需求） |
| **长期** | 多网关实例间状态同步 | 低（取决于部署规模） |

---

## 参考资料

- [NVIDIA Dynamo KV Cache Aware Routing](https://docs.nvidia.com/dynamo/user-guides/kv-cache-aware-routing)
- [NVIDIA Dynamo Router Guide (v1.0.0)](https://docs.nvidia.com/dynamo/v1.0.0/components/router/router-guide)
- [NVIDIA Dynamo Event Plane](https://docs.nvidia.com/dynamo/design-docs/communication-planes/event-plane)
- [TensorRT-LLM KV Cache Reuse Optimizations](https://developer.nvidia.com/blog/introducing-new-kv-cache-reuse-optimizations-in-nvidia-tensorrt-llm/)
- [llm-d KV Cache Aware Routing (Red Hat)](https://developers.redhat.com/articles/2025/10/07/master-kv-cache-aware-routing-llm-d-efficient-ai-inference)
- [llm-d GitHub](https://github.com/llm-d/llm-d)
- [IBM Research: KV-Cache Wins You Can Feel](https://research.ibm.com/publications/kv-cache-wins-you-can-feel-building-ai-aware-llm-routing-on-kubernetes)
- [LMCache + NVIDIA Dynamo 1.0](https://blog.lmcache.ai/en/2026/03/16/lmcache-nvidia-dynamo-1-0-a-match-made-in-inference-heaven/)
- [vLLM Automatic Prefix Caching](https://docs.vllm.ai/en/stable/design/prefix_caching/)
- [llm-d KV-Cache Wins You Can See](https://llm-d.ai/blog/kvcache-wins-you-can-see)
- [NVIDIA Dynamo v0.9.0 Release](https://www.marktechpost.com/2026/02/19/nvidia-releases-dynamo-v0-9-0-a-massive-infrastructure-overhaul-featuring-flashindexer-multi-modal-support-and-removed-nats-and-etcd/)
