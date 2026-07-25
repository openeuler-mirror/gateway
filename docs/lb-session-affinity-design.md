# gateway-lb：基于 Session（API Key）的后端负载均衡

- **日期**：2026-07-25
- **状态**：已评审 → 实施中
- **范围**：`misc/LB`（独立 Pingora 反向代理）
- **分支**：`feat/lb-session-affinity`

## 目标

在现有"按 host/path/client_ip 选后端"的基础上，让一条路由支持**多个后端**；对多后端路由启用
**API-key 一致性哈希 + TCP 健康检查**的会话亲和（sticky）负载均衡。**单后端路由完全保持现状（零回归）。**

## 关键决策

| 维度 | 选择 |
|------|------|
| 亲和 key 来源 | **API Key**（`Authorization: Bearer <key>` → `x-api-key` → 回退客户端 IP） |
| 失败处理 | **主动健康检查 + 自动剔除/恢复** |
| 探活机制 | **TCP 连接探活**（`std::net::TcpStream::connect_timeout`） |
| 选择算法 | **一致性哈希环**（虚拟节点 150/后端，顺时针跳过不健康节点） |

## 配置形态（向后兼容）

保留 `backend`（单后端，行为不变），新增 `backends`（多后端，启用亲和）：

```yaml
routes:
  - host: "api.example.com"
    backend: "10.0.0.1:8080"          # 单后端：原行为

  - host: "*.example.com"
    backends:                          # 多后端：一致性哈希 + 健康检查
      - "10.0.0.1:8080"
      - "10.0.0.2:8080"
      - "10.0.0.3:8080"

  - host: "app.example.com"
    path: "/assets/"
    backends: ["10.0.0.4:80", "10.0.0.5:80"]
```

**校验**：一条路由 `backend` 与 `backends` 互斥；`backends` 为空报错并拒绝启动/拒绝 reload。

**范围划分**：
- 亲和**只作用于声明了 `backends` 的路由**；
- `default_backend` 仍是单个，兜底路由**不做 LB**；
- 单后端路由走原逻辑。

## 亲和算法与 API Key 提取

**`fn api_key(header)`** 提取顺序：
1. `Authorization: Bearer <key>` → 取 Bearer 后部分
2. 否则 `x-api-key` 头
3. 都没有 → 回退**客户端 IP**（保证无 key 客户端也有稳定亲和）

**`Ring`（一致性哈希环）**：
- 每后端生成 **150 个虚拟节点**，位置 = `hash("addr#i")`，排序存 `Vec<(u64, SocketAddr)>`。
- `pick(key, &unhealthy)`：`h = hash(key)`，二分定位首个 `position >= h`，**顺时针**扫描，跳过
  `unhealthy` 集合里的地址，返回首个健康节点（绕回环头）。
- 多副本 + 顺时针跳过 = **分布均匀** + **亲和稳定** + **故障节点摘除时仅其上的 key 重分布到环上下一健康节点，其余 key 不动**。

**哈希**：std `DefaultHasher`（零依赖，非密码学用途足够）。`backends` 在 load 时统一 parse 成
`Vec<SocketAddr>`，非法地址即拒绝启动。

**兜底**：该路由全部 unhealthy 时，仍返回环选中的主节点（best-effort，避免整体不可用）。

## 健康检查与生命周期

**TCP 探活**（std 线程，仿现有 `watch_config`）：
- 间隔 **5s** / 连接超时 **2s** / 连续失败 **3 次** → 标记 unhealthy；**1 次**成功即恢复。
- 判定：`TcpStream::connect_timeout(&addr, 2s)` 成功/超时。

**状态**：`HealthState { unhealthy: Arc<RwLock<HashSet<SocketAddr>>> }`。一个全局探活线程遍历
**所有路由去重后的后端集合**更新它。

**与热加载配合**：探活线程持有 `Arc<RwLock<Vec<SocketAddr>>>`（当前应探活集合），reload 时整体
替换；同时清理 `unhealthy` 里已不存在的幽灵地址；该路由的 `Ring` 一并重建。

**参数**：间隔/超时/阈值先**硬编码常量**，不进 YAML（YAGNI）。

## Pingora 集成与边界

**`upstream_peer` 流程**：

```
route = config.resolve_route(host, path, client_ip)   // 返回 &Route
if route.backends 非空:
    key  = api_key(session.req_header())
    ring = rings[idx]                                  // 预构建，按路由索引取
    addr = ring.pick(key, &health.unhealthy)
else:
    addr = route.backend  // 单后端 / default_backend，原逻辑
return HttpPeer::new(addr, false, String::new())      // 明文上游，不变
```

**存放**：`rings: Arc<RwLock<HashMap<usize, Ring>>>` 与 `Config` 平级放 `Gateway`；key 用路由索引
（usize，稳定）。探活线程在 `main` 里 `bootstrap()` 后、`run_forever()` 前 spawn，用 `Arc` 共享
`unhealthy` 和后端集合。Pingora 的 `Server` 自管 runtime，独立 std 线程不冲突。

**边界保证**：
- 单后端路由 & `default_backend` **完全走原路径零回归**；
- API key 缺失 → 回退客户端 IP，仍有亲和；
- 全部 unhealthy → best-effort 返回环主节点；
- TLS 终止在 LB（不变），上游始终明文（`HttpPeer::new(addr, false, ...)` 不变）。

## 测试要点

- `Ring::pick` 单测：固定 key → 固定后端；摘除某节点后该 key 落到下一健康节点、其余 key 不变。
- `api_key` 单测：三种来源顺序与回退。
- 配置互斥校验单测：`backend`+`backends` 同存报错、`backends` 空报错。
- TCP 探活翻转：用 `std::net::TcpListener` 起假后端验证 healthy→unhealthy→healthy。

## 依赖

无需新增 crate：哈希用 std `DefaultHasher`，TCP 探活用 `std::net` + `std::thread`（与现有
`watch_config` 同一套阻塞线程模式）。
