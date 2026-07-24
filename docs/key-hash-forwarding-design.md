# Key 哈希透传设计：`X-Gateway-Key-Hash` Header（部署级开关）

## 1. Background

### 1.1 问题陈述

在 BoomGateway 与 vLLM 之间加入中间调度层后，若中间层想按「调用方」维度做 key 亲和路由（把同一个用户的请求粘到同一个后端 worker，以提升 KV-cache 命中率与一致性），就需要感知每个请求来自哪个 key。然而请求经过 BoomGateway 认证后，`Authorization` 已被替换为 provider 侧的 key，原始用户身份在转发阶段丢失，下游无法据此做亲和。

### 1.2 身份信息在网关内的生命周期

```
1. 认证阶段（身份产生）
   boom-main/src/extractor.rs: RequiredAuth::from_request_parts
     -> boom-auth/src/key_auth.rs: DbAuthenticator::authenticate
          -> token_to_identity() -> AuthIdentity { key_hash, key_alias, metadata }

2. 网关内部阶段（身份被使用）
   boom-main/src/routes.rs: chat_completions_inner
     -> acquire_fc_guard(..., Some(identity.key_hash.clone()), ...)   <- 仅用于网关内部

3. 转发阶段（身份丢失）
   boom-main/src/routes.rs: provider.chat_stream(req)
     -> boom-provider/src/openai.rs: OpenAIProvider::build_request
          -> Authorization 变成 provider key，body 无用户身份
     -> POST to 下游 / vLLM        <- 下游拿不到 key 身份
```

`identity.key_hash` 是用户 key 的哈希（非明文 key），在认证阶段已算好，网关内部一直可用。

---

## 2. High-Level Design

### 2.1 设计决策：部署级开关 `forward_key_hash`（默认关闭）

**核心决策：在 `model_list` 条目上新增部署级布尔开关 `forward_key_hash`（`#[serde(default)]`，默认 `false`）。**

只有显式开启该开关的部署，网关转发时才会注入 `X-Gateway-Key-Hash: <key_hash>`；其余部署完全不受影响。

```
X-Gateway-Key-Hash: a1b2c3d4...      （调用方 key 的哈希）
```

下游调度器读取该 header 做 key 亲和调度，转发给 vLLM 前将其移除。

**为什么是部署级开关，而不是像 `enable_priority_header` 那样的全局开关？**

`enable_priority_header` 是 `router_settings` 下的全局开关，一旦开启对所有上游生效。但 key 哈希属于网关内部身份信息，**不应泄露给第三方厂商上游**（OpenAI / Anthropic / Bedrock 等）——它们既不消费该信息，透传过去也只会造成不必要的身份泄露。因此本特性按部署粒度开启：只对可信的自建上游（如自家的调度器 / 路由器）启用，第三方部署默认拿不到任何网关身份信息。这也与既有的部署级开关 `client_type_header`（控制 `X-BooM-Client-Type`）保持一致的粒度和风格。

| 维度 | `enable_priority_header` | `forward_key_hash`（本方案） |
|------|--------------------------|------------------------------|
| 作用域 | 全局（`router_settings`） | 部署级（`model_list[]`） |
| 典型用途 | 优先级调度 | key 亲和路由 |
| 第三方上游 | 会收到 | **不会收到**（除非该部署显式开启） |

### 2.2 对现有服务的影响

**部署侧只在需要的 `model_list` 条目上加一个 `forward_key_hash: true`，其余不动。**
默认关闭，存量部署零影响。

即使开启后 `X-Gateway-Key-Hash` 透传给普通 vLLM 也无影响——HTTP 协议规范允许自定义 header（`X-` 前缀），收到未知 header 会自动忽略：

- **不会拒绝请求**
- **不会返回错误**
- **不会影响推理结果**

因此开启对不消费该 header 的上游完全无害。

### 2.3 请求流转 curl 示例

请求经过两段链路，header 完全不同：

**第一段：客户端 -> 网关**（用户手动发起，携带用户自己的 API key）

```bash
curl -X POST http://gateway.example.com/v1/chat/completions \
  -H "Authorization: Bearer sk-your-user-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "my-llama",
    "messages": [{"role": "user", "content": "give me 10 emoji"}],
    "stream": true
  }'
```

> 这一段 **没有** `X-Gateway-Key-Hash`，网关尚未介入。

**第二段：网关 -> 下游调度器**（网关内部自动构造，对用户不可见；仅当该部署 `forward_key_hash: true`）

```bash
# 以下是网关内部等效发出的请求（用户不会手动执行）
curl -X POST http://scheduler.router:30080/v1/chat/completions \
  -H "Authorization: Bearer sk-provider-key" \
  -H "Content-Type: application/json" \
  -H "X-Gateway-Key-Hash: a1b2c3d4..." \
  -d '{
    "model": "actual-model-id",
    "messages": [{"role": "user", "content": "give me 10 emoji"}],
    "stream": true
  }'
```

> - `Authorization` 变成了 **provider 侧的 key**，用户的 key 在认证层已被消费。
> - `X-Gateway-Key-Hash` 是 **本次新增的**，只出现在开启开关的部署这段链路。
> - 同一个用户 key -> 同一个 `X-Gateway-Key-Hash`，这正是下游做稳定 key 亲和的依据。

### 2.4 Extensibility（可扩展性）

`X-Gateway-*` 作为 BoomGateway 的 header 前缀，未来可透传更多网关内部元数据给中间调度层（如 `X-Gateway-Team-Id` 团队标识）。这些 header 对不认识它们的上游 **完全无影响**，可随需增加，无兼容性风险。

---

## 3. Low-Level Design

### 3.1 核心思路

复用既有的 `gateway_headers` 旁路通道（`ChatCompletionRequest` 上 `#[serde(skip)]` 的字段，不进 body），与 `X-Gateway-Priority` / `X-BooM-Client-Type` 同一机制。是否注入由**部署级**开关 `forward_key_hash` 控制，默认关闭。

部署级开关沿用 `client_type_header` 的既有链路：config -> DB -> provider -> trait method -> `build_gateway_headers`，无需引入新机制。

### 3.2 改动文件

| 文件 | 改动 |
|------|------|
| `boom-config/src/lib.rs` | `ModelEntry` 新增部署级开关 `forward_key_hash: bool`（`#[serde(default)]`，默认 `false`），与 `client_type_header` 并列 |
| `boom-routing/src/deployment_store.rs` | 各 deployment row/input 结构体与 INSERT/SELECT/UPDATE SQL 增加 `forward_key_hash` 列 |
| `boom-routing/src/migrations.rs` | `boom_model_deployment` 建表 DDL 增加 `forward_key_hash BOOLEAN NOT NULL DEFAULT false` |
| `boom-dashboard/src/migrations.rs` | 增量 `ALTER TABLE ... ADD COLUMN IF NOT EXISTS forward_key_hash ...` |
| `boom-dashboard/src/handlers_admin.rs` | `CreateDeploymentRequest` 增加 `forward_key_hash` 字段 |
| `boom-main/src/admin_command.rs`、`state.rs` | 构造 `DeploymentInput` / 调用 `create_provider` 时透传该开关 |
| `boom-provider/src/lib.rs` + 5 个 provider | `create_provider` 增加参数；各 provider 存字段并实现 `Provider::forward_key_hash()` |
| `boom-core/src/provider.rs` | `Provider` trait 新增 `fn forward_key_hash(&self) -> bool { false }` |
| `boom-main/src/routes.rs` | `build_gateway_headers()` 增加 `key_hash` + `forward_key_hash` 入参，开启时注入 `X-Gateway-Key-Hash`；两个调用点传入 `&identity.key_hash` 与 `provider.forward_key_hash()` |

### 3.3 关键代码

**1. 部署级开关（`config/lib.rs`）**

```rust
pub struct ModelEntry {
    // ...
    #[serde(default)]
    pub client_type_header: bool,
    #[serde(default)]
    pub forward_key_hash: bool,
}
```

**2. Provider trait 默认实现（`provider.rs`）**

```rust
fn forward_key_hash(&self) -> bool {
    false
}
```

**3. 按开关注入（`routes.rs`）** —— 关闭或 `key_hash` 为空时不注入。

```rust
if forward_key_hash && !key_hash.is_empty() {
    headers.insert("X-Gateway-Key-Hash".to_string(), key_hash.to_string());
}

// handler 中：
req.gateway_headers = build_gateway_headers(
    is_vip,
    inner.config.router_settings.enable_priority_header,
    api_path,
    provider.client_type_header(),
    &identity.key_hash,
    provider.forward_key_hash(),
);
```

provider 层无需额外改动：`gateway_headers` 里的每一条都会被逐条加到发往上游的 HTTP 请求上（与 `X-Gateway-Priority` 同一路径）。

### 3.4 数据流

```
routes.rs   : identity.key_hash + provider.forward_key_hash() -> build_gateway_headers -> req.gateway_headers
  | (内存传递，不进 JSON body)
openai.rs   : mem::take(gateway_headers) -> 逐条 .header(k, v)
  | (HTTP 请求头)
下游调度器  : 读 X-Gateway-Key-Hash 做 key 亲和 -> strip -> 转发给 vLLM
```

### 3.5 改动影响范围

| 模块 | 改动类型 | 风险 |
|------|---------|------|
| `boom-config` | `ModelEntry` 加一个布尔开关 | 低，默认 `false` 向后兼容 |
| `boom-routing` | deployment 存储与 SQL 加一列 | 低，DB 列默认 `false`，`ADD COLUMN IF NOT EXISTS` 幂等 |
| `boom-provider` | `create_provider` 加参数 + trait 默认实现 | 低 |
| `boom-main` | routes.rs 加入参 + 填充（开关控制） | 低 |

---

## 4. 验证方案

### 4.1 单元测试

| 模块 | 测试文件 | 覆盖内容 |
|------|---------|---------|
| `boom-main` | `routes.rs` | `build_gateway_headers`：开启且 `key_hash` 非空时注入 `X-Gateway-Key-Hash`；关闭时不注入；`key_hash` 为空时不注入 |
| `boom-provider` | `openai.rs` | `gateway_headers` 携带 `X-Gateway-Key-Hash` 时，实际发出的请求头包含该 header（`wiremock` 断言） |

### 4.2 集成验证

对网关发送第一段的 curl 命令；若目标部署配置了 `forward_key_hash: true`，可在下游调度器 / 上游的访问日志中确认 `X-Gateway-Key-Hash` 存在且同一用户 key 得到稳定一致的值；未开启的部署则不应出现该 header。
