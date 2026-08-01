# Pingora Load Balancer (misc/LB)

A standalone HTTP load balancer built on [Pingora](https://github.com/cloudflare/pingora).
It can serve as an optional front-end for BooMGateway or run independently. It ships
as a Docker image (multi-stage Rust build on an openEuler runtime) and also runs
natively.

## Features

- **Routing**: first-match by `host` (wildcard `*.example.com`), `path` prefix
  (segment-boundary aware: `/api` does not match `/api2`), and `client_ip` CIDR;
  unmatched requests fall through to the default backends.
- **Backend policies**:
  - `active-active` (default): consistent hashing (FNV-1a, stable across processes)
    with API-key session affinity;
  - `active-standby`: ordered failover — traffic prefers `backends[0]` and only
    switches when earlier backends are unhealthy;
  - default backends support a failover list (`default_backends`).
- **Health checks**: TCP connect or HTTP GET (configurable path and expected
  status); probe interval and failure threshold are configurable; probes run on
  at most 8 worker threads so large pools do not churn threads.
- **Failure retry**: connect failures re-select a backend (excluding attempted
  ones), with a configurable attempt cap (default 3).
- **5xx retry** (optional): retries idempotent methods (GET/HEAD/OPTIONS by
  default, configurable) on upstream 5xx responses and excludes the failing
  backend from the next pick; non-idempotent methods are never retried.
- **Per-route timeouts**: individual routes can override the global
  connect/read/etc. timeouts; unset fields inherit the global values.
- **Security**:
  - IP blacklist (file-based, hot-reloaded; exact IPs in a `HashSet` for O(1)
    lookups even with very large lists);
  - `trusted_proxies` + X-Forwarded-For: only the first untrusted hop is trusted,
    so clients cannot spoof XFF;
  - upstream TLS (`upstream_tls`, certificate and hostname verification on by
    default);
  - request body size limit (Content-Length pre-check returns 413; chunked bodies
    are aborted mid-stream);
  - request-smuggling defense-in-depth: rejects `Content-Length` + `Transfer-Encoding`
    together and TE on HTTP/1.0.
- **Observability**:
  - `GET /__lb_healthz`: liveness, returns 200 directly;
  - `GET /__lb_metrics`: Prometheus text-format counters (requests / blocked /
    proxied / unhealthy backends, etc.);
  - structured access logs (dispatch / redirect / complete) with status and
    latency, deterministically sampled.
- **Hot reload**: config and blacklist file changes take effect automatically
  (notify + lock-free ArcSwap swaps, no downtime).
- **HTTP/2**: TLS listeners enable h2 automatically; host routing understands
  `:authority`.

## Layout

```
src/
  main.rs      entry point: args, worker/retry tuning, listener wiring
  config.rs    config structs, deserialization, validation, route resolution
  routes.rs    Route matching (host/path/client_ip), request_host
  client.rs    parse_client_ip, trusted proxies + XFF resolution
  blacklist.rs blacklist parse/load/lookup (HashSet + CIDR + ranges)
  backends.rs  consistent-hash ring, active-standby/active-active selection
  health.rs    TCP/HTTP health probes, failure thresholds
  metrics.rs   in-process counters and /__lb_metrics rendering
  logging.rs   syslog/stderr logging, sanitization, request IDs
  proxy.rs     ProxyHttp implementation, hot-reload watcher, runtime state
config.yaml    example config (with inline comments)
Dockerfile     multi-stage build + non-root + HEALTHCHECK
start.sh       Docker helper (start/status/stop/restart)
```

## Quick start

### Run natively

```bash
cargo build --release
./target/release/gateway-lb -c config.yaml
# or via the environment variable
CONFIG_PATH=/path/to/routes.yaml ./target/release/gateway-lb
```

### Docker (recommended)

```bash
./start.sh start              # build image + generate self-signed cert + start
./start.sh start routes.yaml  # custom config (copy config.yaml and edit)
./start.sh status | stop | restart
```

The container runs as a non-root user (65534); `HEALTHCHECK` polls
`/__lb_healthz` every 30s. The config is mounted at `/etc/gateway/routes.yaml`;
the blacklist file `/etc/gateway/blacklist.txt` can be edited on the host and is
hot-reloaded inside the container.

## Configuration reference

See [config.yaml](config.yaml) for a fully commented example. Top-level keys:

| Key | Type | Default | Description |
|---|---|---|---|
| `listen_port` | u16 | 6198 (when no TLS) | Plaintext HTTP port; optional when `tls` is set |
| `tls` | object | none | `{ port, cert, key }`; enables HTTPS + HTTP/2 |
| `default_backend` | string | one required | Default backend `ip:port` (mutually exclusive with `default_backends`) |
| `default_backends` | list | ditto | Default backend failover list; first entry preferred |
| `max_body_size` | u64 | unlimited | Request body limit in bytes; 413 / chunked abort when exceeded |
| `upstream_tls` | object | disabled | `{ enabled, sni, verify }` for https backends; `sni` defaults to the backend IP, `verify` defaults to true |
| `blacklist` | string | none | Blacklist file (one IP / CIDR / `a-b` range per line, `#` comments) |
| `trusted_proxies` | list | empty | Trusted reverse-proxy CIDRs; XFF is only honored when the direct peer is inside one |
| `upstream_*_timeout` | u64 (s) | connect 3 / total 5 / read 30 / idle 60 | Upstream timeouts |
| `worker_threads` | usize | CPU cores | Pingora worker threads |
| `max_retries` | usize | 3 | Max upstream attempts per request (connect-failure failover) |
| `retry_5xx` | object | disabled | `{ enabled, methods, max_tries }`; retries the listed idempotent methods on 5xx |
| `health_check` | object | TCP probe | `{ path, expected_status, interval_secs, fail_threshold }`; setting `path` switches to HTTP GET |
| `access_log` | object | enabled, full | `{ enabled, sample_rate }`; deterministic per-request sampling |
| `routes` | list | required | Route table, see below |

### Route table `routes`

Each route must set exactly one of `backend` / `backends` / `redirect`:

```yaml
routes:
  - host: "api.example.com"            # exact host (case-insensitive)
    backend: "10.0.0.1:8080"
  - host: "*.example.com"              # wildcard subdomains
    path: "/api/"                      # segment-boundary prefix match (optional)
    client_ip: "10.0.0.0/24"           # source IP match (optional, combinable)
    backends:                          # multiple backends
      - "10.0.0.10:8080"
      - "10.0.0.11:8080"
    mode: active-active                # default: consistent hash + API-key affinity
  - host: "ha.example.com"
    mode: active-standby               # prefer backends[0], fail over when unhealthy
    backends: ["10.0.0.20:8080", "10.0.0.21:8080"]
    timeouts:                          # per-route timeout overrides (optional, seconds)
      read: 60
  - host: "old.example.com"
    redirect: "https://new.example.com/"
    redirect_code: 301                 # optional: 301/302/303/307/308, default 302
```

The first matching route wins; unmatched requests use `default_backends`. For
multi-backend routes, the API-key affinity key is taken from `Authorization: Bearer`,
`X-API-Key`, or the client IP, in that order.

## Built-in endpoints

| Endpoint | Description |
|---|---|
| `GET /__lb_healthz` | Liveness, returns 200 directly; bypasses blacklist/routing |
| `GET /__lb_metrics` | Prometheus text format: total/blocked/redirected/proxied requests, body-rejections, status buckets (2xx/3xx/4xx/5xx), upstream errors, retries, response bytes, latency histogram (5ms–60s), unhealthy backends, uptime |

Both endpoints answer before blacklist/routing but have **no authentication** —
keep them on internal networks only.

## Logging

- Defaults to the local syslog (RFC 3164, `/dev/log` or macOS/BSD variants);
  falls back to stderr when no syslog socket exists.
- Line shapes: `dispatch method=.. host=.. path=.. client=.. backend=..`,
  `redirect code hostpath -> location`, and
  `complete backend=.. status=.. ms=.. bytes=..`.
- Client-controlled fields (host/path, etc.) are sanitized of control characters
  to prevent log injection; blacklist-block warnings are throttled per IP.
- Request IDs: if the client sends no `X-Request-Id`, one is generated and
  forwarded upstream for end-to-end tracing.

## Operations & security notes

- `/__lb_metrics` and `/__lb_healthz` are unauthenticated; restrict to internal
  networks or the host itself.
- Never set `trusted_proxies` to catch-all ranges like `0.0.0.0/0`, or XFF can be
  spoofed by anyone.
- For backends over public networks, enable `upstream_tls` (verification is on by
  default); `sni` defaults to the backend IP — set it explicitly when the cert
  does not match the IP.
- Keep `max_retries` small (default 3); a high cap amplifies tail latency behind a
  failing backend.
- Blacklist file format: one entry per line — IP, CIDR, or `start-end` range
  (`#` comments); invalid lines are skipped with a warning.
- Config and blacklist changes hot-reload without restart; TLS certificate
  changes require a container restart.

## Tests

```bash
cargo test
```

57 tests cover route resolution, consistent hashing, blacklist, XFF, probe status
parsing, config validation, and more; 3 tests that need real sockets (syslog I/O,
TCP probe) are skipped in restricted sandboxes but run in normal environments.
