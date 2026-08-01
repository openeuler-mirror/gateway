# Pingora 负载均衡器 (misc/LB)

基于 [Pingora](https://github.com/cloudflare/pingora) 的独立 HTTP 负载均衡器,可作为
BooMGateway 的可选前端,也可单独部署。以 Docker 镜像提供(Rust 多阶段构建,openEuler
运行时),同时支持本地直接运行。

## 特性

- **路由**:按 `host`(支持通配符 `*.example.com`)、`path` 前缀(按段边界匹配,`/api`
  不会误配 `/api2`)、`client_ip` CIDR 首条匹配生效;未命中落默认后端。
- **后端策略**:
  - `active-active`(默认):一致性哈希(FNV-1a,跨进程稳定)+ API Key 会话亲和;
  - `active-standby`:按序 failover,前面的后端不健康才切备机;
  - 默认后端支持 failover 列表(`default_backends`)。
- **健康检查**:TCP connect 或 HTTP GET(可配路径与期望状态码);探测周期与失败阈值可配;
  探针最多 8 个 worker 分片,大后端池不抖线程。
- **失败重试**:连接失败自动换节点(排除已尝试者),重试上限可配(默认 3)。
- **安全**:
  - IP 黑名单(文件化、热加载;精确 IP 走 HashSet,大列表也保持 O(1));
  - `trusted_proxies` + X-Forwarded-For 取首个不可信跳,防客户端伪造;
  - 上游 TLS(`upstream_tls`,默认校验证书与主机名);
  - 请求体大小上限(Content-Length 预检 413 + chunked 流式中断);
  - 请求走私纵深防御:拒绝 `Content-Length` 与 `Transfer-Encoding` 并存、HTTP/1.0 携带 TE。
- **可观测**:
  - `GET /__lb_healthz`:探活,直接 200;
  - `GET /__lb_metrics`:Prometheus 文本格式计数(请求/拦截/转发/不健康后端等);
  - 结构化访问日志(dispatch / redirect / complete),含状态码与耗时,支持确定性采样。
- **热加载**:配置与黑名单文件变更自动生效(notify + ArcSwap 无锁切换,无停机)。
- **HTTP/2**:TLS 监听自动启用 h2,Host 路由兼容 `:authority`。

## 目录结构

```
src/
  main.rs      入口:参数、worker/重试配置、监听装配
  config.rs    配置结构、反序列化与校验、路由解析
  routes.rs    Route 匹配(host/path/client_ip)、request_host
  client.rs    parse_client_ip、trusted proxies + XFF 解析
  blacklist.rs 黑名单解析/加载/匹配(HashSet + CIDR + 区间)
  backends.rs  一致性哈希 Ring、主备/多活选择、探针后端集
  health.rs    TCP/HTTP 健康探针、失败阈值
  metrics.rs   进程内计数与 /__lb_metrics 渲染
  logging.rs   syslog/stderr 日志、sanitize、请求 ID
  proxy.rs     ProxyHttp 实现、热加载 watcher、运行时状态
config.yaml    配置示例(含注释)
Dockerfile     多阶段构建 + 非 root + HEALTHCHECK
start.sh       Docker 辅助脚本(start/status/stop/restart)
```

## 快速开始

### 本地运行

```bash
cargo build --release
./target/release/gateway-lb -c config.yaml
# 或通过环境变量指定配置
CONFIG_PATH=/path/to/routes.yaml ./target/release/gateway-lb
```

### Docker(推荐)

```bash
./start.sh start              # 自动构建镜像 + 生成自签证书 + 启动
./start.sh start routes.yaml  # 自定义配置(复制 config.yaml 后修改)
./start.sh status | stop | restart
```

容器内以非 root 用户(65534)运行,`HEALTHCHECK` 每 30s 请求
`/__lb_healthz`。配置挂载在 `/etc/gateway/routes.yaml`,黑名单文件
`/etc/gateway/blacklist.txt` 可在宿主机编辑,容器内热加载。

## 配置参考

示例见 [config.yaml](config.yaml)(内含逐项中文注释)。顶层键如下:

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `listen_port` | u16 | 6198(无 TLS 时) | 明文 HTTP 监听端口;配置 TLS 后可不设 |
| `tls` | 对象 | 无 | `{ port, cert, key }`,启用 HTTPS + HTTP/2 |
| `default_backend` | string | 必填其一 | 默认后端 `ip:port`(与 `default_backends` 二选一) |
| `default_backends` | list | 同上 | 默认后端 failover 列表,优先第一个 |
| `max_body_size` | u64 | 不限制 | 请求体上限(字节);超限 413 / chunked 中断 |
| `upstream_tls` | 对象 | 关闭 | `{ enabled, sni, verify }`,上游 https;`sni` 缺省用后端 IP,`verify` 默认 true |
| `blacklist` | string | 无 | 黑名单文件路径(每行 IP / CIDR / `a-b` 区间,# 注释) |
| `trusted_proxies` | list | 空 | 信任的反向代理网段;直连 IP 在其中时才采信 XFF |
| `upstream_*_timeout` | u64(秒) | connect 3 / total 5 / read 30 / idle 60 | 上游超时 |
| `worker_threads` | usize | CPU 核数 | Pingora worker 线程数 |
| `max_retries` | usize | 3 | 单请求最大上游尝试次数(连接失败 failover) |
| `health_check` | 对象 | TCP 探测 | `{ path, expected_status, interval_secs, fail_threshold }`;path 设置后改为 HTTP GET |
| `access_log` | 对象 | 开启全量 | `{ enabled, sample_rate }`;采样按请求特征确定性进行 |
| `routes` | list | 必填 | 路由表,见下 |

### 路由表 `routes`

每条路由必须且只能设置 `backend` / `backends` / `redirect` 之一:

```yaml
routes:
  - host: "api.example.com"            # 精确 host(大小写不敏感)
    backend: "10.0.0.1:8080"
  - host: "*.example.com"              # 通配子域
    path: "/api/"                      # 段边界前缀匹配(可选)
    client_ip: "10.0.0.0/24"           # 来源 IP 匹配(可选,可与 host/path 组合)
    backends:                          # 多后端
      - "10.0.0.10:8080"
      - "10.0.0.11:8080"
    mode: active-active                # 默认:一致性哈希 + API Key 亲和
  - host: "ha.example.com"
    mode: active-standby               # 主备:优先 backends[0],不健康才切换
    backends: ["10.0.0.20:8080", "10.0.0.21:8080"]
  - host: "old.example.com"
    redirect: "https://new.example.com/"
    redirect_code: 301                 # 可选:301/302/303/307/308,默认 302
```

匹配顺序为首条命中生效;没有匹配的路由时使用 `default_backends`。多后端路由的
API Key 亲和取自 `Authorization: Bearer`、`X-API-Key` 或客户端 IP(按此优先级)。

## 内置端点

| 端点 | 说明 |
|---|---|
| `GET /__lb_healthz` | 探活,直接返回 200;不经过黑名单/路由 |
| `GET /__lb_metrics` | Prometheus 文本格式:请求总数、黑名单拦截、重定向、转发、body 超限拒绝、不健康后端数、uptime |

两个端点都在黑名单与路由判断之前应答,但**没有鉴权**,请勿暴露到公网。

## 日志

- 默认写本地 syslog(RFC 3164,`/dev/log` 或 macOS/BSD 变体);无 syslog socket 时回退
  stderr。
- 每行格式:`dispatch method=.. host=.. path=.. client=.. backend=..`(转发)、
  `redirect code hostpath -> location`、`complete backend=.. status=.. ms=.. bytes=..`。
- 客户端可控字段(host/path 等)会先做控制字符清洗,防止日志注入;黑名单拦截告警
  按 IP 每分钟节流。
- 请求 ID:客户端未带 `X-Request-Id` 时自动生成透传上游,便于跨链排障。

## 运维与安全注意事项

- `/__lb_metrics` 与 `/__lb_healthz` 无鉴权,仅限内网/同机访问。
- `trusted_proxies` 切勿配置 `0.0.0.0/0` 等全通网段,否则 XFF 可被任意伪造。
- 上游走公网时务必开启 `upstream_tls`(默认校验证书);`sni` 缺省为后端 IP,证书
  与 IP 不匹配时需显式配置 SNI。
- `max_retries` 不宜过大(默认 3),否则坏后端会放大尾部延迟。
- 黑名单文件每行一条:IP、CIDR 或 `start-end` 区间(`#` 注释),非法行跳过并告警。
- 配置或黑名单变更后自动热加载,无需重启;TLS 证书变更需要重启容器。

## 测试

```bash
cargo test
```

57 个测试覆盖路由解析、一致性哈希、黑名单、XFF、探针状态码解析、配置校验等;
其中 3 个依赖真实 socket(syslog 收发、TCP 探针)在受限沙箱中会跳过,正常环境可跑。
