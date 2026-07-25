# Session-Affinity Load Balancing Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 让 gateway-lb 对声明了 `backends` 的路由做基于 API Key 的一致性哈希 + TCP 健康检查的会话亲和负载均衡；单后端路由与 `default_backend` 零回归。

**Architecture:** 配置从 raw 字符串在 load 时一次性 parse 成 `SocketAddr`（非法即拒绝启动）。多后端路由预构建一致性哈希环（150 虚拟节点/后端）；`upstream_peer` 提取 API key → 环上顺时针跳过不健康节点选后端。独立 std 线程周期性 TCP 探活，维护 `unhealthy` 集合，热加载时重建环 + 刷新探活集合 + 清理幽灵记录。

**Tech Stack:** Rust 2021、Pingora 0.8（`pingora-proxy`/`pingora-core`/`pingora-http`）、std `DefaultHasher`、std `net::TcpStream` + `thread`。零新增依赖。

**Design doc:** `misc/LB/docs/plans/2026-07-25-session-affinity-lb-design.md`

**文件：** 全部改动集中在 `misc/LB/src/main.rs`（单文件项目，保持其风格）。测试放在同文件 `#[cfg(test)] mod tests`。

**TDD 约定（Rust）：** 每个 Task 先在 `mod tests` 写失败测试 → `cargo test <name>` 确认失败 → 写实现 → `cargo test <name>` 确认通过 → 提交。每个 Task 结束保证 `cargo build` 通过（每 commit 可编译）。

---

### Task 0：构建卫生（.gitignore）

**Files:**
- Create: `misc/LB/.gitignore`

**Step 1：创建 .gitignore**

```
/target
```

**Step 2：提交**

```bash
git add misc/LB/.gitignore
git commit -s -m "chore(lb): 忽略 target 构建产物"
```

---

### Task 1：Config 重构 —— eager SocketAddr + `backends` 字段

把 `Route.backend: String` 改为 `Option<SocketAddr>`，新增 `backends: Option<Vec<SocketAddr>>`；删除自定义 `Deserialize`，改为 `ConfigRaw`/`RouteRaw`（derive Deserialize）+ `from_raw` 校验/解析；`resolve` → `resolve_route` 返回 `Option<(usize, &Route)>`。

**Step 1：写失败测试**（在文件末尾新增 `#[cfg(test)] mod tests`）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_single_and_multi_backend() {
        let yaml = r#"
default_backend: "127.0.0.1:8080"
routes:
  - host: a.com
    backend: "10.0.0.1:80"
  - host: b.com
    backends: ["10.0.0.2:80", "10.0.0.3:80"]
"#;
        let cfg = Config::from_str(yaml).unwrap();
        assert_eq!(cfg.default_backend, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(cfg.routes[0].backend, Some("10.0.0.1:80".parse().unwrap()));
        assert!(cfg.routes[0].backends.is_none());
        assert!(cfg.routes[1].backend.is_none());
        assert_eq!(cfg.routes[1].backends.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn config_rejects_invalid_routes() {
        let both = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    backend: \"1.1.1.1:80\"\n    backends: [\"2.2.2.2:80\"]\n";
        let empty = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    backends: []\n";
        let neither = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n";
        let bad = "default_backend: \"127.0.0.1:80\"\nroutes:\n  - host: a\n    backends: [\"not-an-addr\"]\n";
        assert!(Config::from_str(both).is_err());
        assert!(Config::from_str(empty).is_err());
        assert!(Config::from_str(neither).is_err());
        assert!(Config::from_str(bad).is_err());
    }
}
```

**Step 2：运行确认失败**

```bash
cargo test config_single_and_multi_backend config_rejects_invalid_routes
```
Expected: 编译失败 / `no function named from_str`。

**Step 3：实现**

替换 `Route` 定义（原 L38-44）与 `impl Deserialize for Route`（原 L46-72）为：

```rust
#[derive(Debug)]
struct Route {
    host: Option<String>,
    path: Option<String>,
    client_ip: Option<IpNet>,
    backend: Option<SocketAddr>,
    backends: Option<Vec<SocketAddr>>,
}

impl Route {
    fn from_raw(raw: RouteRaw) -> std::result::Result<Self, String> {
        let client_ip = raw
            .client_ip
            .as_deref()
            .map(parse_client_ip)
            .transpose()?;
        match (raw.backend, raw.backends) {
            (Some(b), None) => Ok(Route {
                host: raw.host,
                path: raw.path,
                client_ip,
                backend: Some(parse_addr(&b)?),
                backends: None,
            }),
            (None, Some(list)) => {
                if list.is_empty() {
                    return Err("route with `backends` has empty list".into());
                }
                let mut addrs = Vec::with_capacity(list.len());
                for s in list {
                    addrs.push(parse_addr(&s)?);
                }
                Ok(Route {
                    host: raw.host,
                    path: raw.path,
                    client_ip,
                    backend: None,
                    backends: Some(addrs),
                })
            }
            (Some(_), Some(_)) => Err("route has both `backend` and `backends`".into()),
            (None, None) => Err("route has neither `backend` nor `backends`".into()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RouteRaw {
    host: Option<String>,
    path: Option<String>,
    client_ip: Option<String>,
    backend: Option<String>,
    backends: Option<Vec<String>>,
}

fn parse_addr(s: &str) -> std::result::Result<SocketAddr, String> {
    s.parse()
        .map_err(|e: std::net::SocketAddrParseError| format!("invalid address '{s}': {e}"))
}
```

把 `Config` 定义（原 L23-29）改为含 raw 形态 + resolved 形态：

```rust
#[derive(Debug, Deserialize)]
struct ConfigRaw {
    listen_port: Option<u16>,
    tls: Option<TlsConfig>,
    default_backend: String,
    routes: Vec<RouteRaw>,
}

#[derive(Debug)]
struct Config {
    listen_port: Option<u16>,
    tls: Option<TlsConfig>,
    default_backend: SocketAddr,
    routes: Vec<Route>,
}
```

把 `impl Config`（原 L83-113）替换为：

```rust
impl Config {
    fn load(path: &str) -> std::result::Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {path}: {e}"))?;
        Self::from_str(&content)
    }

    fn from_str(content: &str) -> std::result::Result<Self, String> {
        let raw: ConfigRaw =
            serde_yaml::from_str(content).map_err(|e| format!("failed to parse config: {e}"))?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: ConfigRaw) -> std::result::Result<Self, String> {
        let default_backend = parse_addr(&raw.default_backend)?;
        let mut routes = Vec::with_capacity(raw.routes.len());
        for r in raw.routes {
            routes.push(Route::from_raw(r)?);
        }
        Ok(Config {
            listen_port: raw.listen_port,
            tls: raw.tls,
            default_backend,
            routes,
        })
    }

    /// First-match routing. Returns (route_index, &Route); None => use default_backend.
    fn resolve_route(
        &self,
        host: &str,
        path: &str,
        client_ip: Option<IpAddr>,
    ) -> Option<(usize, &Route)> {
        for (idx, route) in self.routes.iter().enumerate() {
            if let Some(ref r_host) = route.host {
                if !host_matches(host, r_host) {
                    continue;
                }
            }
            if let Some(ref r_path) = route.path {
                if !path.starts_with(r_path) {
                    continue;
                }
            }
            if let Some(ref r_net) = route.client_ip {
                match client_ip {
                    Some(ip) if r_net.contains(&ip) => {}
                    _ => continue,
                }
            }
            return Some((idx, route));
        }
        None
    }
}
```

更新 import（文件顶部，原 L10-14 区域）：新增 `use std::net::SocketAddr;`，并把 `use std::net::IpAddr;` 保留。最终顶部 net 导入为：

```rust
use std::net::{IpAddr, SocketAddr};
```

并新增集合导入：

```rust
use std::collections::{HashMap, HashSet};
```

**Step 4：运行确认通过**

```bash
cargo test config_single_and_multi_backend config_rejects_invalid_tests
```
注意：此时 `upstream_peer` 仍引用旧的 `config.resolve(...)`，整体 `cargo build` 会报错（预期，下个 Task 修）。先单独跑测试模块：

```bash
cargo test --bin gateway-lb config_
```
Expected: 两个测试 PASS（`upstream_peer` 的编译错误会在后续 Task 修复；若测试无法独立编译因 `upstream_peer` 报错，先跳到 Task 5 Step 把 `upstream_peer` 用 `resolve_route` 接好，再回跑）。

**Step 5：提交**

```bash
git add misc/LB/src/main.rs
git commit -s -m "refactor(lb): Config 改为 eager SocketAddr 解析并支持 backends 字段"
```

> 注：此 Task 与 Task 5 的 `upstream_peer` 接线有耦合。若希望每个 commit 可编译，可把 Task 1 的最小接线（见 Task 5 Step 中 `None => config.default_backend` 那条分支先接上）合并进此 Task。实施时按编译驱动调整。

---

### Task 2：一致性哈希环 `Ring`

**Files:** Modify `misc/LB/src/main.rs`

**Step 1：写失败测试**（追加到 `mod tests`）

```rust
    #[test]
    fn ring_same_key_same_backend_and_skip_unhealthy() {
        let backends: Vec<SocketAddr> = ["127.0.0.1:8001", "127.0.0.1:8002", "127.0.0.1:8003"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let ring = Ring::new(&backends);
        let a = ring.pick("user-key-1", &HashSet::new());
        let b = ring.pick("user-key-1", &HashSet::new());
        assert_eq!(a, b, "same key must map to same backend");

        let mut un = HashSet::new();
        un.insert(a);
        let c = ring.pick("user-key-1", &un);
        assert_ne!(c, a, "must skip unhealthy primary");
        assert!(!un.contains(&c));

        let mut all: HashSet<SocketAddr> = backends.iter().copied().collect();
        let d = ring.pick("user-key-1", &all);
        assert!(all.contains(&d), "all-unhealthy best-effort still returns a node");
        let _ = &mut all;
    }

    #[test]
    fn ring_consistency_on_eviction() {
        let backends: Vec<SocketAddr> = (8001..=8004)
            .map(|p| format!("127.0.0.1:{p}").parse().unwrap())
            .collect();
        let ring = Ring::new(&backends);
        let full: HashMap<String, SocketAddr> = (0..2000)
            .map(|i| {
                let k = format!("k{i}");
                (k.clone(), ring.pick(&k, &HashSet::new()))
            })
            .collect();
        let mut un = HashSet::new();
        un.insert(backends[1]);
        let on_evicted = full.values().filter(|a| **a == backends[1]).count();
        let moved = full
            .iter()
            .filter(|(k, orig)| ring.pick(k, &un) != **orig)
            .count();
        assert_eq!(moved, on_evicted, "only keys on the evicted node should move");
    }
```

**Step 2：运行确认失败**

```bash
cargo test ring_
```
Expected: FAIL（`Ring` 未定义）。

**Step 3：实现**（在 `host_matches` 函数之后新增）

```rust
use std::hash::{Hash, Hasher};

const VNODES: usize = 150;

fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[derive(Debug, Clone)]
struct Ring {
    nodes: Vec<(u64, SocketAddr)>,
}

impl Ring {
    fn new(backends: &[SocketAddr]) -> Self {
        let mut nodes = Vec::with_capacity(backends.len() * VNODES);
        for (bi, addr) in backends.iter().enumerate() {
            for vi in 0..VNODES {
                nodes.push((hash_str(&format!("{addr}#{bi}#{vi}")), *addr));
            }
        }
        nodes.sort_unstable_by_key(|(h, _)| *h);
        Ring { nodes }
    }

    /// Clockwise from hash(key), return first healthy node; if all unhealthy,
    /// best-effort return the primary (start) node.
    fn pick(&self, key: &str, unhealthy: &HashSet<SocketAddr>) -> SocketAddr {
        debug_assert!(!self.nodes.is_empty(), "Ring must have >=1 backend");
        let h = hash_str(key);
        let n = self.nodes.len();
        let start = match self.nodes.binary_search_by_key(&h, |(p, _)| *p) {
            Ok(i) => i,
            Err(i) => i % n,
        };
        for off in 0..n {
            let (_, addr) = self.nodes[(start + off) % n];
            if !unhealthy.contains(&addr) {
                return addr;
            }
        }
        self.nodes[start].1
    }
}
```

注意 `use std::hash::{Hash, Hasher};` 放到文件顶部 import 区，不要放在函数体内。

**Step 4：运行确认通过**

```bash
cargo test ring_
```
Expected: PASS。

**Step 5：提交**

```bash
git add misc/LB/src/main.rs
git commit -s -m "feat(lb): 一致性哈希环 Ring（150 虚拟节点，跳过不健康节点）"
```

---

### Task 3：API Key 提取

**Files:** Modify `misc/LB/src/main.rs`

**Step 1：写失败测试**

```rust
    #[test]
    fn api_key_extraction_priority() {
        let ip = Some(IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(extract_api_key(Some("Bearer abc123"), None, ip), "abc123");
        assert_eq!(extract_api_key(Some("bearer XYZ"), None, ip), "XYZ"); // case-insensitive prefix, preserve case
        assert_eq!(extract_api_key(None, Some("sk-xyz"), ip), "sk-xyz");
        assert_eq!(extract_api_key(Some("Bearer a"), Some("b"), ip), "a"); // auth precedence
        assert_eq!(extract_api_key(None, None, ip), "10.0.0.1"); // IP fallback
        assert_eq!(extract_api_key(None, None, None), "unknown");
    }
```

**Step 2：运行确认失败**

```bash
cargo test api_key_extraction_priority
```
Expected: FAIL（`extract_api_key` 未定义）。

**Step 3：实现**

```rust
/// Affinity key: Authorization: Bearer <key> → x-api-key → client IP → "unknown".
fn extract_api_key(
    auth: Option<&str>,
    x_api_key: Option<&str>,
    client_ip: Option<IpAddr>,
) -> String {
    if let Some(a) = auth {
        let trimmed = a.trim();
        if let Some(prefix) = trimmed.get(..6) {
            if prefix.eq_ignore_ascii_case("Bearer") {
                if let Some(rest) = trimmed.get(6..) {
                    let rest = rest.trim();
                    if !rest.is_empty() {
                        return rest.to_string();
                    }
                }
            }
        }
        return trimmed.to_string();
    }
    if let Some(k) = x_api_key {
        return k.trim().to_string();
    }
    match client_ip {
        Some(ip) => ip.to_string(),
        None => "unknown".to_string(),
    }
}
```

**Step 4：运行确认通过**

```bash
cargo test api_key_extraction_priority
```
Expected: PASS。

**Step 5：提交**

```bash
git add misc/LB/src/main.rs
git commit -s -m "feat(lb): API Key 提取（Bearer/x-api-key/客户端 IP 回退）"
```

---

### Task 4：健康状态 + TCP 探活

**Files:** Modify `misc/LB/src/main.rs`

**Step 1：写失败测试**

```rust
    #[test]
    fn classify_health_threshold_and_recovery() {
        assert_eq!(classify_health(0, true), (0, false));
        assert_eq!(classify_health(1, false), (2, false));
        assert_eq!(classify_health(2, false), (3, true)); // crosses threshold (3)
        assert_eq!(classify_health(9, true), (0, false)); // recovery resets
    }

    #[test]
    fn probe_once_alive_then_closed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(probe_once(addr));
        drop(listener);
        assert!(!probe_once(addr));
    }
```

**Step 2：运行确认失败**

```bash
cargo test classify_health_threshold_and_recovery probe_once_alive_then_closed
```
Expected: FAIL。

**Step 3：实现**

```rust
const PROBE_INTERVAL: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const FAIL_THRESHOLD: u32 = 3;

struct HealthState {
    backends: Arc<RwLock<Vec<SocketAddr>>>,
    unhealthy: Arc<RwLock<HashSet<SocketAddr>>>,
}

impl HealthState {
    fn new(backends: Vec<SocketAddr>) -> Arc<Self> {
        Arc::new(HealthState {
            backends: Arc::new(RwLock::new(backends)),
            unhealthy: Arc::new(RwLock::new(HashSet::new())),
        })
    }
}

/// (prev_consecutive_failures, probe_healthy) -> (new_failures, mark_unhealthy)
fn classify_health(prev_failures: u32, probe_healthy: bool) -> (u32, bool) {
    if probe_healthy {
        (0, false)
    } else {
        let n = prev_failures + 1;
        (n, n >= FAIL_THRESHOLD)
    }
}

fn probe_once(addr: SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}

fn run_health_probe(state: Arc<HealthState>) {
    use std::collections::HashMap as StdMap;
    std::thread::spawn(move || {
        let mut failures: StdMap<SocketAddr, u32> = StdMap::new();
        loop {
            let backends = state.backends.read().unwrap().clone();
            for addr in &backends {
                let healthy = probe_once(*addr);
                let prev = *failures.get(addr).unwrap_or(&0);
                let (nf, mark) = classify_health(prev, healthy);
                failures.insert(*addr, nf);
                let mut set = state.unhealthy.write().unwrap();
                if mark {
                    set.insert(*addr);
                } else {
                    set.remove(addr);
                }
            }
            std::thread::sleep(PROBE_INTERVAL);
        }
    });
}
```

> 用 `StdMap` 别名仅避免与已 import 的 `HashMap` 命名混淆；二者等价。

**Step 4：运行确认通过**

```bash
cargo test classify_health_threshold_and_recovery probe_once_alive_then_closed
```
Expected: PASS。

**Step 5：提交**

```bash
git add misc/LB/src/main.rs
git commit -s -m "feat(lb): TCP 健康探活（5s 间隔 / 2s 超时 / 3 次阈值，自动剔除恢复）"
```

---

### Task 5：集成 —— Gateway/rings/upstream_peer/main/reload

**Files:** Modify `misc/LB/src/main.rs`

**Step 1：写失败测试**

```rust
    #[test]
    fn build_rings_and_collect_backends() {
        let yaml = r#"
default_backend: "127.0.0.1:8080"
routes:
  - host: a.com
    backend: "10.0.0.1:80"
  - host: b.com
    backends: ["10.0.0.2:80", "10.0.0.3:80"]
  - host: c.com
    backends: ["10.0.0.2:80", "10.0.0.4:80"]
"#;
        let cfg = Config::from_str(yaml).unwrap();
        let rings = build_rings(&cfg);
        assert!(rings.contains_key(&1) && rings.contains_key(&2));
        assert!(!rings.contains_key(&0), "single-backend route has no ring");
        let all = collect_all_backends(&cfg);
        let set: HashSet<SocketAddr> = all.iter().copied().collect();
        assert_eq!(set.len(), 3, "dedup: .2/.3/.4");
        assert!(set.contains(&"10.0.0.2:80".parse().unwrap()));
    }
```

**Step 2：运行确认失败**

```bash
cargo test build_rings_and_collect_backends
```
Expected: FAIL（`build_rings`/`collect_all_backends` 未定义）。

**Step 3：实现**

新增构建/收集辅助（放在 `Ring` 之后）：

```rust
fn build_rings(config: &Config) -> HashMap<usize, Ring> {
    config
        .routes
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.backends.as_ref().map(|bs| (i, Ring::new(bs))))
        .collect()
}

fn collect_all_backends(config: &Config) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in &config.routes {
        if let Some(list) = &r.backends {
            for a in list {
                if seen.insert(*a) {
                    out.push(*a);
                }
            }
        }
    }
    out
}
```

改 `Gateway` 结构（原 L125-127）：

```rust
pub struct Gateway {
    config: Arc<RwLock<Config>>,
    rings: Arc<RwLock<HashMap<usize, Ring>>>,
    health: Arc<HealthState>,
}
```

改 `upstream_peer`（原 L135-169）—— 注意 `type CTX = ();` 与 `new_ctx` 不变：

```rust
    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let header = session.req_header();
        let host = header
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        let path = header.uri.path();
        let client_ip = session
            .client_addr()
            .and_then(|a| a.as_inet())
            .map(|a| a.ip());

        let config = self.config.read().unwrap();
        let addr = match config.resolve_route(host, path, client_ip) {
            Some((idx, route)) => match &route.backends {
                Some(_) => {
                    let key = extract_api_key(
                        header.headers.get("authorization").and_then(|v| v.to_str().ok()),
                        header.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
                        client_ip,
                    );
                    let unhealthy = self.health.unhealthy.read().unwrap();
                    let rings = self.rings.read().unwrap();
                    let ring = rings
                        .get(&idx)
                        .expect("ring must exist for a backends-route");
                    ring.pick(&key, &*unhealthy)
                }
                None => route
                    .backend
                    .expect("single-backend route must have backend"),
            },
            None => config.default_backend,
        };

        Ok(Box::new(HttpPeer::new(addr, false, String::new())))
    }
```

`upstream_request_filter`（原 L171-189）**保持不变**。

改 `watch_config`（原 L192-267）：签名加 `rings` 和 `health`，reload 时重建。函数签名与 reload 分支替换为：

```rust
fn watch_config(
    path: String,
    config: Arc<RwLock<Config>>,
    rings: Arc<RwLock<HashMap<usize, Ring>>>,
    health: Arc<HealthState>,
) {
    use notify::{Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

    std::thread::spawn(move || {
        // ...（watcher 创建、watch_dir、file_name、loop 接收 event 的代码全部保留原样）...

            // Debounce: drain rapid successive events
            while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}

            match Config::load(&path) {
                Ok(new_config) => {
                    let new_rings = build_rings(&new_config);
                    let new_backends = collect_all_backends(&new_config);
                    let backend_set: HashSet<SocketAddr> =
                        new_backends.iter().copied().collect();
                    {
                        let mut u = health.unhealthy.write().unwrap();
                        u.retain(|a| backend_set.contains(a));
                    }
                    *health.backends.write().unwrap() = new_backends;
                    *config.write().unwrap() = new_config;
                    *rings.write().unwrap() = new_rings;
                    log::info!("config reloaded from {path}");
                }
                Err(e) => {
                    log::error!("failed to reload config: {e}");
                }
            }
        }
    });
}
```

> 实施时把原 `match Config::load` 分支整体替换为上面这段；watcher/event/debounce 中间逻辑一字不动。

改 `main`（原 L269-298）—— 在 `server.bootstrap();` 之后、构造 `Gateway` 之前插入：

```rust
    let config = Arc::new(RwLock::new(config));
    let rings = Arc::new(RwLock::new(build_rings(&config.read().unwrap())));
    let health = HealthState::new(collect_all_backends(&config.read().unwrap()));
    run_health_probe(health.clone());
    watch_config(config_path, config.clone(), rings.clone(), health.clone());

    let gateway = Gateway {
        config,
        rings,
        health,
    };
```

删除原 `let config = Arc::new(RwLock::new(config)); watch_config(config_path, config.clone()); let gateway = Gateway { config };` 三行。`proxy` / `server.add_service` / `run_forever` 不变。

**Step 4：运行确认通过 + 全量编译**

```bash
cargo build
cargo test
```
Expected: 编译通过；全部测试 PASS。

**Step 5：提交**

```bash
git add misc/LB/src/main.rs
git commit -s -m "feat(lb): 多后端路由接入一致性哈希 + 健康检查（单后端零回归）"
```

---

### Task 6：更新示例配置 + 冒烟验证

**Files:** Modify `misc/LB/config.yaml`

**Step 1：加一条多后端示例**（在 routes 末尾追加）

```yaml
  - host: "api.lb.local"
    backends:
      - "127.0.0.1:9001"
      - "127.0.0.1:9002"
```

**Step 2：验证配置可被新 loader 解析**

```bash
cargo test config_single_and_multi_backend
cargo run --bin gateway-lb -- --config config.yaml &  # 启动后应无 "invalid address" 报错
# Ctrl-C 停止
```

Expected: 启动正常，无解析错误。

**Step 3：提交**

```bash
git add misc/LB/config.yaml
git commit -s -m "docs(lb): config.yaml 增加多后端路由示例"
```

---

## 验收清单（最终）

- [ ] `cargo build` 通过
- [ ] `cargo test` 全绿
- [ ] 单后端路由 + `default_backend`：行为与改动前完全一致（人工核对 `upstream_peer` 的 `None` / 单 backend 分支走原路径）
- [ ] 多后端路由：同一 API key 两次请求落到同一后端
- [ ] 标记某后端 unhealthy（构造测试）后，该 key 自动落到环上下一健康节点
- [ ] 热加载：编辑 `config.yaml` 的 backends 后，日志出现 `config reloaded`，新环生效、旧幽灵 unhealthy 记录被清理
- [ ] 非法配置（both / empty / 坏地址）启动被拒
