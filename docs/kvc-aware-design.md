# KVC-Aware Routing 设计文档

## 概述

KVC-Aware（KV-Cache Aware）路由通过 ZMQ 实时订阅 vLLM 推理引擎的 KV cache 块事件，构建 Token 前缀 Trie 索引。当新请求到达时，网关对请求消息做 Tokenize，在 Trie 中查找与已缓存 KV 前缀匹配度最高的 Worker，优先将请求路由到该 Worker，从而复用 KV cache、减少重复计算、降低首 Token 延迟（TTFT）。

## 架构

```
┌─────────────────────────────────────────────────────────────────────┐
│                         vLLM Workers                                │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐       ┌─────────┐          │
│  │ Worker0 │  │ Worker1 │  │ Worker2 │  ...  │ WorkerN │          │
│  │ZMQ PUB  │  │ZMQ PUB  │  │ZMQ PUB  │       │ZMQ PUB  │          │
│  └────┬────┘  └────┬────┘  └────┬────┘       └────┬────┘          │
└───────┼────────────┼────────────┼──────────────────┼───────────────┘
        │            │            │                  │
        │  ZMQ PUB/SUB (topic="kv@", msgpack)       │
        │            │            │                  │
┌───────┼────────────┼────────────┼──────────────────┼───────────────┐
│       └────────────┼────────────┼──────────────────┘               │
│              ┌─────▼─────┐                                         │
│              │ZMQ SUB    │  subscriber.rs                          │
│              │select_all │                                         │
│              └─────┬─────┘                                         │
│                    │ VllmEventBatch                                │
│              ┌─────▼─────┐                                         │
│              │vllm_event │  vllm_event.rs                          │
│              │parse+adapt│                                         │
│              └─────┬─────┘                                         │
│                    │ GatewayKvEvent                                │
│              ┌─────▼──────────────┐                                │
│              │  TokenPrefixIndex  │  token_prefix.rs               │
│              │  (Per-Model Trie)  │                                │
│              └─────┬──────────────┘                                │
│                    │                                                │
│  Boom Gateway      │  find_matches(model, token_ids, worker_ids)   │
│                    │                                                │
│  ┌─────────────────▼──────────────────┐                            │
│  │         Request Flow               │                            │
│  │                                     │                            │
│  │  Request ──► TokenizerPool ──► KvcAwarePolicy                  │
│  │              (tokenize)       (select_with_context)             │
│  │                  │                    │                          │
│  │             token_ids          best worker_id                   │
│  │                                     │                          │
│  │                              Selected Provider ──► vLLM        │
│  └─────────────────────────────────────┘                          │
└───────────────────────────────────────────────────────────────────┘
```

## 数据流

### 1. KV Cache 事件采集（ZMQ）

```
vLLM Worker --ZMQ PUB--> [topic, seq, msgpack] --ZMQ SUB--> Gateway
```

- **协议**: ZMQ PUB/SUB，3-frame multipart 消息
  - Frame 0: topic，格式 `kv@{worker_id}@{model_name}`
  - Frame 1: 序列号，8 bytes big-endian u64
  - Frame 2: msgpack payload（msgspec `tag=True` 编码）
- **事件类型**（来自 vLLM）:
  - `BlockStored`: 新 KV cache 块被缓存，包含 `block_hashes`、`token_ids`、`block_size`、`medium`
  - `BlockRemoved`: KV cache 块被驱逐
  - `AllBlocksCleared`: Worker 的所有 KV cache 被清除
- **多 Worker 连接**: 每个 endpoint 创建独立 SUB socket，通过 `futures::stream::select_all` 合并
- **自动重连**: ZMQ 协议层处理，断线期间消息丢失可接受（KV 事件是临时状态）

### 2. 事件解析与适配

`vllm_event.rs` 将 vLLM 的 msgpack 事件转换为内部 `GatewayKvEvent`:

| vLLM 事件 | 内部事件 | 说明 |
|-----------|---------|------|
| `BlockStored` (N hashes) | N × `Store` | 一个 BlockStored 可能包含多个 block_hashes，扇出为多个 Store |
| `BlockRemoved` | `EvictBlocks` | 驱逐指定 hash 的块 |
| `AllBlocksCleared` | `Remove` | 清除该 Worker 的所有数据 |

**类型映射要点**:
- vLLM `block_hashes` 是 `i64`（有符号），内部 `local_hash` 是 `u64` → 用 `as u64` 重解释位模式
- vLLM `medium`: `None/"gpu"` → Gpu, `"cpu"` → Cpu, `"disk"/"ssd"` → Ssd

### 3. Token 前缀 Trie 索引

`TokenPrefixIndex` 使用 per-model Trie 存储已缓存的 KV 块:

```
Trie Root (per model)
  │
  ├─ [block_0_tokens] → {Worker0, Worker1}
  │    │
  │    ├─ [block_1_tokens] → {Worker0}
  │    │    └─ [block_2_tokens] → {Worker0}
  │    │
  │    └─ [block_1'_tokens] → {Worker1}
  │
  └─ [block_0'_tokens] → {Worker2}
```

- **Trie Key**: 每个 TrieNode 的 children 以 `Vec<u32>`（block_size 个 token ID）为键
- **Trie Value**: 每个 TrieNode 记录拥有该前缀的 `workers: HashSet<String>`
- **辅助索引**:
  - `worker_sequences`: `(model, worker) → Vec<Vec<u32>>`，按序记录该 Worker 的所有 block，用于 Remove/Evict
  - `hash_to_tokens`: `(model, worker, hash) → Vec<u32>`，用于 EvictBlocks 定位被驱逐的 block
  - `tiers`: `(model, worker) → StorageTier`，存储层级
  - `loads`: `worker → LoadState`，负载指标

### 4. 请求 Tokenization

`TokenizerPool` 对请求消息做 Tokenize，生成与 vLLM 一致的 token ID 序列:

```
OpenAI Messages → render chat_template → tokenizer.encode() → Vec<u32>
```

**Tokenizer 加载**（从 `{tokenizer_dir}/{model}/` 目录）:
- `tokenizer.json` — HuggingFace tokenizer（编码用）
- `tokenizer_config.json` — chat_template、bos_token、eos_token
- `chat_template.jinja` — 独立模板文件（fallback）

**模板渲染**:
- 使用 minijinja（Rust Jinja2 引擎）渲染 HuggingFace chat_template
- `trim_blocks=true` + `lstrip_blocks=true`，对齐 vLLM 的 Jinja2 配置
- `add_special_tokens=false`（模板已包含特殊 token）
- 预处理 Python Jinja2 不兼容语法:
  - `var.split('sep')` → `split(var, 'sep')` 自定义函数
  - `.strip(...)` → 移除
  - `.items()` → `[]`（跳过，不影响前缀匹配）
  - `tojson(ensure_ascii=False)` → `tojson`
  - `raise_exception(...)` → `''`
- minijinja 原生支持 `namespace()`、`[1:]` 切片、`[-1]` 负索引，无需预处理
- 无模板时 fallback 到 `role: content\n` 简单格式

### 5. 路由决策

`KvcAwarePolicy.select_with_context()` 执行路由:

```
token_ids → 按 block_size 分块 → BFS 遍历 Trie → 计算综合得分 → 选最优 Worker
```

**评分公式**:
```
combined_score = cache_weight × hit_ratio
               + tier_weight × tier_score
               + load_weight × load_score
```

- `hit_ratio` = 匹配的 block 数 / 请求总 block 数
- `tier_score`: Gpu=1.0, Cpu=0.7, Ssd=0.4, Remote=0.2
- `load_score`: `(1 - kv_utilization) × 1/(1 + queue_depth)`

**降级策略**: 无匹配时 fallback 到最低负载选择（`select_lowest_load`）

## 运维配置指南

### 完整配置示例

```yaml
# Gateway 配置
router_settings:
  # 通过 schedule_policy 统一驱动，无需单独的 enabled 开关
  schedule_policy: kvc_aware

  kvc_aware:
    # 必须与 vLLM 的 --block-size 完全一致。
    # 常见值: 16 (默认), 64, 128。MiniMax-M2.7 使用 128。
    block_size: 128

    # 评分权重（三者之和应为 1.0）
    cache_weight: 0.5          # KV 前缀命中率权重
    tier_weight: 0.3           # 存储层级权重
    load_weight: 0.2           # 负载权重

    # Tokenizer 文件目录。每个模型一个子目录，目录名 = model_name。
    # 如不配置，tokenization 禁用，路由退化为最低负载选择。
    tokenizer_dir: /data/tokenizers

    # vLLM ZMQ PUB socket 地址列表。每个 vLLM 实例一个。
    # 通常与 vLLM --kv-events-address 一致。
    zmq_endpoints:
      - "tcp://10.0.0.1:5557"
      - "tcp://10.0.0.2:5557"

    # ZMQ topic 过滤前缀。vLLM 默认使用 "kv@" 前缀。
    zmq_topic_prefix: "kv@"

# 模型部署配置
model_list:
  - model_name: MiniMax-M2.7
    litellm_params:
      model: hosted_vllm/MiniMax-M2.7
      api_base: http://10.0.0.1:8000
      api_key: os.environ/VLLM_API_KEY
    # model_info.id 必须与 vLLM ZMQ topic 中的 worker_id 一致。
    # vLLM 默认使用主机 IP 或 data-parallel rank 作为 worker_id。
    model_info:
      id: "10.0.0.1"
```

### 权重调优指南

三个权重控制路由决策的偏好，**三者之和应为 1.0**:

| 权重 | 默认值 | 含义 | 调高效果 | 调低效果 |
|------|--------|------|---------|---------|
| `cache_weight` | 0.5 | KV 前缀命中率 | 强偏好有缓存前缀的 Worker | 弱化缓存复用，接近随机分配 |
| `tier_weight` | 0.3 | 存储层级优先级 | 偏好 GPU 内存 > CPU > SSD | 不区分存储介质 |
| `load_weight` | 0.2 | Worker 负载 | 避免向繁忙 Worker 发请求 | 可能向高负载 Worker 发请求 |

**典型场景推荐**:

| 场景 | cache | tier | load | 说明 |
|------|-------|------|------|------|
| 通用（默认） | 0.5 | 0.3 | 0.2 | 均衡缓存复用和负载 |
| 高并发短对话 | 0.3 | 0.2 | 0.5 | 避免热点，负载均衡优先 |
| 长上下文重复提示 | 0.7 | 0.2 | 0.1 | 最大化 KV cache 复用 |
| 纯 GPU 集群 | 0.6 | 0.1 | 0.3 | tier 差异小，降低其权重 |

**注意事项**:
- `cache_weight=0, tier_weight=0, load_weight=1.0` 等效于最低负载策略
- 当前 `load_score` 基于静态默认值 0.5（ZMQ 通道暂不传输负载指标），因此 `load_weight` 实际暂无效果
- 当 `hit_ratio=0`（无匹配前缀）时，无论权重如何设置，都会 fallback 到最低负载选择

### Tokenizer 目录结构

```
/data/tokenizers/                          # tokenizer_dir 配置路径
  MiniMax-M2.7/                            # 子目录名 = gateway model_name
    tokenizer.json                         # HuggingFace tokenizer（必须）
    tokenizer_config.json                  # chat_template + bos/eos token（必须）
    chat_template.jinja                    # 独立模板文件（可选，部分模型需要）

  glm-5.1/                                 # 另一个模型
    tokenizer.json
    tokenizer_config.json                  # 内含 "chat_template" 字段

  Qwen3-235B/
    tokenizer.json
    tokenizer_config.json
```

**获取 tokenizer 文件**:

从 HuggingFace 模型仓库下载 `tokenizer.json` 和 `tokenizer_config.json`:

```bash
# 示例: 下载 MiniMax-M2.7 的 tokenizer
mkdir -p /data/tokenizers/MiniMax-M2.7
cd /data/tokenizers/MiniMax-M2.7

# 从 HuggingFace 下载（需要 huggingface-cli 或手动下载）
huggingface-cli download MiniMaxAI/MiniMax-M2.7 \
  tokenizer.json tokenizer_config.json \
  --local-dir .

# 如果 chat_template 在独立文件中（检查 tokenizer_config.json 中是否有 chat_template 字段）
# 如果没有，从仓库下载 chat_template.jinja:
huggingface-cli download MiniMaxAI/MiniMax-M2.7 \
  chat_template.jinja \
  --local-dir .
```

**验证 tokenizer 文件**:

```bash
# 1. 检查文件存在
ls -la /data/tokenizers/MiniMax-M2.7/
# 应该看到: tokenizer.json  tokenizer_config.json  [chat_template.jinja]

# 2. 检查 tokenizer_config.json 包含 chat_template
python3 -c "import json; c=json.load(open('tokenizer_config.json')); print('chat_template:', 'YES' if c.get('chat_template') else 'NO')"
# 如果输出 "NO"，需要确保 chat_template.jinja 文件存在

# 3. 检查 bos_token / eos_token
python3 -c "import json; c=json.load(open('tokenizer_config.json')); print('bos:', c.get('bos_token')); print('eos:', c.get('eos_token'))"
```

### vLLM 侧配置要求

vLLM 需要开启 KV cache 事件上报功能。以下是必要的配置:

#### 1. 启用 KV Events（vLLM >= 0.8）

```bash
# vLLM 启动参数
python -m vllm.entrypoints.openai.api_server \
  --model /models/MiniMax-M2.7 \
  --served-model-name MiniMax-M2.7 \        # 与 gateway model_name 一致
  --block-size 128 \                        # 与 gateway kvc_aware.block_size 一致
  --enable-kv-events \                      # 启用 ZMQ KV 事件
  --kv-events-address tcp://*:5557          # ZMQ PUB 监听地址
```

或通过 vLLM 的 `kv_events_config.json`:

```json
{
  "enable_kv_cache_events": true,
  "zmq_pubsub_address": "tcp://*:5557",
  "zmq_topic_prefix": "kv@"
}
```

#### 2. ZMQ Topic 格式

vLLM 发出的 ZMQ multipart 消息格式:

```
Frame 0 (topic): "kv@{worker_id}@{model_name}"
Frame 1 (seq):   8 bytes big-endian u64
Frame 2 (body):  msgpack payload
```

- `worker_id`: 默认使用 vLLM Worker 的主机名或 IP。通过 `--kv-events-worker-id` 可自定义
- `model_name`: 来自 `--served-model-name`
- `topic_prefix`: 默认 `"kv@"`，通过 `--kv-events-topic-prefix` 可自定义

#### 3. 标识对齐检查清单

| 检查项 | Gateway 配置 | vLLM 配置 | 如何验证 |
|--------|-------------|-----------|---------|
| Model 名称 | `model_name: MiniMax-M2.7` | `--served-model-name MiniMax-M2.7` | 查看 ZMQ topic 第二段 |
| Worker ID | `model_info.id: "10.0.0.1"` | `--kv-events-worker-id` 或默认 IP | 查看 ZMQ topic 第一段 |
| Block Size | `kvc_aware.block_size: 128` | `--block-size 128` | 对比 gateway 日志 token 数 |
| Tokenizer | `/data/tokenizers/MiniMax-M2.7/` | vLLM 的 model 目录 | 对比 first_8 token IDs |

**验证对齐是否成功**:

```bash
# 1. 查看 gateway 日志，确认 tokenization 输出
# 日志中应包含:
#   "openai tokenized" first_8=[...] text_preview="..."

# 2. 查看 kv-index 端点
curl http://gateway:4000/internal/kv-index | jq '.blocks[0]'
# 应包含 token_sample 和 workers

# 3. 对比 token_sample 和 gateway first_8 是否一致
# 一致 → 对齐成功
# 不一致 → 检查 model_name、tokenizer 版本、block_size

# 4. 确认路由命中
# 日志中应出现 "KVC selected" 而非 "no KV match, fallback to lowest-load"
```

### 常见问题排查

| 现象 | 可能原因 | 排查方法 |
|------|---------|---------|
| `no KV match, fallback to lowest-load` | model_name 不一致 | 对比 kv-index 中的 model 与 gateway 日志中的 model |
| 同上 | worker_id 不一致 | 对比 kv-index 中的 workers 与 YAML 中 model_info.id |
| 同上 | block_size 不一致 | 对比 kv-index token_count 与 gateway block_size |
| `chat_template render failed` | 模板语法不兼容 | 检查 tokenizer_config.json 或 chat_template.jinja |
| `tokenizer not found at ...` | tokenizer 文件缺失 | 检查 `{tokenizer_dir}/{model}/tokenizer.json` 是否存在 |
| ZMQ 无事件 | vLLM 未开启 KV events | 确认 `--enable-kv-events` 参数 |
| ZMQ 无事件 | 防火墙阻止 | 确认 gateway 能访问 vLLM 的 5557 端口 |
| token_sample 与 first_8 不一致 | tokenizer 版本不同 | 确保 gateway 和 vLLM 使用同一版本 tokenizer 文件 |

## 模块结构

| Crate | 文件 | 职责 |
|-------|------|------|
| boom-core | `kv_event.rs` | `KvIndexBackend` trait、`GatewayKvEvent`、`KvMatchResult`、`StorageTier` |
| boom-kvindex | `backend/token_prefix.rs` | Token 前缀 Trie 实现 |
| boom-kvindex | `vllm_event.rs` | vLLM msgpack 解析 + GatewayKvEvent 适配 |
| boom-kvindex | `subscriber.rs` | ZMQ SUB subscriber 生命周期管理 |
| boom-kvindex | `tokenizer.rs` | HuggingFace tokenizer + chat_template 渲染 |
| boom-config | `lib.rs` | `KvcAwareSettings` 配置定义 |
| boom-routing | `policy/kvc_aware.rs` | KV cache 感知调度策略 |
| boom-routing | `router.rs` | `select_provider_with_prefix()` 路由入口 |
| boom-main | `state.rs` | 组装 kv_index、tokenizer_pool、policy |
| boom-main | `main.rs` | ZMQ subscriber 启动 |
| boom-main | `routes.rs` | 请求 tokenization 调用 + `/internal/kv-index` 端点 |

## 监控与调试

### 日志

| 级别 | 消息 | 说明 |
|------|------|------|
| INFO | `openai tokenized` / `anthropic tokenized` | token 数量、前 8 个 token、文本预览 |
| INFO | `KVC selected` | 命中 Worker、匹配深度、得分详情 |
| DEBUG | `no KV match, fallback to lowest-load` | token 数量、候选数 |
| DEBUG | `KV event batch processed` | stored/removed/cleared 计数 |
| INFO | `loaded chat_template` | 模板长度、bos/eos token |
| WARN | `chat_template render failed, falling back to simple format` | 渲染错误信息 |

### API 端点

`GET /internal/kv-index` — KV 索引状态:

```json
{
  "status": "active",
  "block_count": 1024,
  "models": ["MiniMax-M2.7"],
  "blocks": [
    {
      "model": "MiniMax-M2.7",
      "tier": "gpu",
      "token_count": 128,
      "token_sample": [200034, 200019, 28463, 10, 2985, 457, 2610, 3689],
      "workers": ["10.0.0.1"]
    }
  ]
}
```

## 部署依赖

- **libzmq**: tmq 通过 zmq crate 链接 libzmq C 库
  - Docker: `apt-get install libzmq3-dev`
  - macOS: `brew install zeromq`
- **Tokenizer 文件**: 需要提前下载到 `tokenizer_dir`，每个模型一个子目录
- **网络**: Gateway 需要能访问 vLLM 的 ZMQ PUB 端口（默认 5557）

## 已知限制

1. **消息丢失**: ZMQ PUB/SUB 断线期间消息丢失，仅导致短暂路由次优，不影响正确性
2. **Tokenization 精度**: 复杂模板的预处理（链式 split/strip 简化）可能导致少量 token 偏差，不影响前缀匹配效果
3. **Worker ID 映射**: 需要手动确保 Gateway 的 `model_info.id` 与 vLLM ZMQ topic 中的 worker_id 一致
4. **Load Metrics**: 当前 ZMQ 通道不传输负载指标，`load_score` 默认 0.5（可后续通过扩展支持）
5. **单 Trie 锁**: 每个 model 的 Trie 使用 `RwLock`，高频 Store 事件时写锁可能成为瓶颈
