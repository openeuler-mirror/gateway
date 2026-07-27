<div align="center">
  <img src="misc/logo.svg" alt="BooMGateway" width="360">
  <br><br>
  <strong>High-Performance LLM API Gateway</strong>
</div>

[中文](README.md) | **English**

A production-grade LLM API gateway built in Rust. Unified request entry point for OpenAI, Anthropic, Gemini, Bedrock, vLLM, Ollama and 20+ providers. Compatible with the litellm key schema, with self-built rate limiting, plan management, flow control, cost/billing, web dashboard, request auditing, and full prompt logging.

[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange)](https://www.rust-lang.org/)
[![License: MulanPSL v2](https://img.shields.io/badge/License-MulanPSL%20v2-green.svg)](LICENSE)

---

## Features

- **Multi-Provider Routing** — OpenAI / Anthropic / Azure / Gemini / Bedrock / vLLM / Ollama etc.
- **Load Balancing** — Round-robin, key-affinity, or KVC-aware scheduling across same-name deployments, with live rebalancing counters
- **Rate Limiting** — Sliding window + concurrency control + custom time windows + scheduled plans
- **Plan System** — Flexible key plans and team plans (with `member_plan` inheritance), key/team→plan assignment, 3-level fallback
- **Quota & Cost** — Per-model `quota_count_ratio`, plus `cost_templates` pricing and per-window token/cost limits
- **Flow Control** — Per-deployment max inflight requests + max context chars, VIP priority queue
- **Auto-Disable** — Consecutive request-failure detection auto-disables faulty deployments (including wildcard `*`)
- **Deployment Health Probes** — Active `/metric` probing with automatic offline/recovery (configurable thresholds)
- **KV-Cache Aware Routing** — Subscribe to vLLM KV-cache events via ZMQ, route requests to the worker with the most cached prefix, reducing TTFT
- **Public Models** — `public_models` config grants all keys access to selected models without whitelist updates
- **Anthropic Native** — `/v1/messages` endpoint (Claude Code / opencode compatible)
- **Prompt Logging** — Full request/response bodies (incl. raw SSE chunks) written to disk as rotating, gzip-compressed JSONL; toggle per team/key from the dashboard
- **Agent Statistics** — Client-type classification (Anthropic-native vs. Other) with a 60-minute breakdown on the dashboard
- **Web Dashboard** — SPA admin panel: keys, models, aliases, plans, teams, quota, logs, real-time inflight + agent stats
- **Hot Reload** — SIGHUP, API, or dashboard button; zero-downtime config swap via ArcSwap
- **Request Auditing** — Full request logs (tokens, duration, status), streaming requests track real duration
- **Debug Recording** — Optional upstream response capture for error diagnosis (behind the `debug-tools` feature)
- **Containerized LB** — The `misc/LB` front-end ships as a Docker image (multi-stage Rust build on openEuler runtime)

---

## Quick Start

### Prerequisites

- Rust 1.75+ (with cargo)
- PostgreSQL 13+ (for key authentication and persistence)

### 1. Build

```bash
git clone https://github.com/your-org/BooMGateway.git
cd BooMGateway

# Build release binary
cargo build --release

# The binary is at target/release/boom-gateway
```

### 2. Prepare Database

Create an empty PostgreSQL database. BooMGateway auto-creates all required tables on startup:

```bash
# Create database (any name works)
createdb boom_gateway

# Or via psql
psql -U postgres -c "CREATE DATABASE boom_gateway;"
```

That's it — no manual schema migration needed. The gateway auto-creates 11 `boom_*` tables:

| Table | Owner | Purpose |
|-------|-------|---------|
| `boom_request_log` | boom-audit | Request logs with token counts, duration, status |
| `boom_model_deployment` | boom-routing | Model deployment configs |
| `boom_model_alias` | boom-routing | Model alias mappings |
| `boom_rate_limit_state` | boom-limiter | Rate-limit window counters |
| `boom_rate_limit_cumulative` | boom-limiter | Permanent cumulative token/cost totals |
| `boom_key_plan_assignment` | boom-limiter | Key-to-plan assignments |
| `boom_team_plan_assignment` | boom-limiter | Team-to-plan assignments |
| `boom_rate_limit_plan` | boom-limiter | Plan definitions |
| `boom_config` | boom-dashboard | Generic KV config store |
| `boom_team_table` | boom-dashboard | Team definitions with model access |
| `boom_verification_token` | boom-dashboard / boom-auth | API key/token records (litellm-compatible schema) |

### 3. Configure

Create `config.yaml` (the repo ships `config.example.yaml` — copy it as a starting point):

```bash
cp config.example.yaml config.yaml
```

```yaml
# Model deployments
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: openai/gpt-4o
      api_key: os.environ/OPENAI_API_KEY

  - model_name: claude-sonnet
    litellm_params:
      model: anthropic/claude-sonnet-4-20250514
      api_key: os.environ/ANTHROPIC_API_KEY

  - model_name: deepseek-chat
    litellm_params:
      model: openai/deepseek-chat
      api_base: https://api.deepseek.com/v1
      api_key: os.environ/DEEPSEEK_API_KEY

  # Catch-all: model names matching no configured model_name route here
  - model_name: "*"
    litellm_params:
      model: openai/gpt-4o-mini
      api_key: os.environ/OPENAI_API_KEY

  # With flow control — protect expensive backends from burst traffic
  # - model_name: claude-opus
  #   model_info:
  #     id: opus-node-1               # deployment_id for flow control slot
  #   flow_control:
  #     model_queue_limit: 20          # max concurrent in-flight requests
  #     model_context_limit: 2000000   # max total input chars across in-flight
  #   litellm_params:
  #     model: anthropic/claude-opus-4-20250514
  #     api_key: os.environ/ANTHROPIC_API_KEY

# General settings
general_settings:
  master_key: os.environ/MASTER_KEY
  database_url: os.environ/DATABASE_URL
  # Models accessible to ALL keys regardless of per-key whitelist
  public_models:
    - deepseek-chat

# Server
server:
  host: 0.0.0.0
  port: 4000
  workers: 4

# Rate limiting
rate_limit:
  enabled: true
  default_rpm: 60

# Plans (key plans and team plans)
plan_settings:
  default_plan: "basic"
  default_team_plan: "team_basic"
  plans:
    basic:                                # key plan
      concurrency_limit: 4
      rpm_limit: 60
      window_limits:
        - [100, 18000]
    pro:                                  # key plan
      concurrency_limit: 10
      rpm_limit: 120
      window_limits:
        - [500, 18000]
    team_basic:                           # team plan, members inherit "basic"
      type: team
      member_plan: basic

# Full request/response logging to disk (off by default)
prompt_log:
  enabled: false
  dir: "/data/prompt_logs"
  max_file_size_mb: 50
  capture_raw_upstream: false             # also store pre-conversion upstream body (/v1/messages etc.)
  excluded_keys: []
  excluded_teams: []

# Active health probing for deployments
deployment_health_check:
  auto_offline_enabled: true
  auto_recovery_enabled: true
  path: /metric
  failure_threshold: 3
  recovery_threshold: 2
```

See [CONFIG_EXAMPLE.md](CONFIG_EXAMPLE.md) for the complete reference (also covers `router_settings`, `cost_templates`, KV-cache-aware routing, and more).

### 4. Run

```bash
# Set environment variables
export MASTER_KEY="sk-your-master-key"
export DATABASE_URL="postgres://user:pass@localhost:5432/boom_gateway"
export OPENAI_API_KEY="sk-..."
export ANTHROPIC_API_KEY="sk-ant-..."

# Run
cargo run --release -p boom-main

# Or run the built binary directly
./target/release/boom-gateway
```

CLI flags (override `config.yaml`):

```bash
boom-gateway --config config.yaml --host 0.0.0.0 --port 4000
boom-gateway --reboot      # gracefully stop a running instance, then start
```

### 5. Create Keys & Start Using

```bash
# Create an API key via dashboard API
curl -X POST http://localhost:4000/dashboard/api/admin/keys \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_alias": "alice", "plan_name": "pro"}'

# Response: {"key": "sk-...", "token_hash": "...", "key_name": null}
# Copy the key — it's shown only once!

# Use it
curl http://localhost:4000/v1/chat/completions \
  -H "Authorization: Bearer sk-your-new-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

### 6. Dashboard

Open `http://localhost:4000/dashboard` in your browser. Log in as `admin` with your master key (or as a regular user with an API key).

---

## Project Structure

```
BooMGateway/
├── boom-gateway/           Rust workspace root (13 crates)
│   ├── boom-core/          Core traits and shared types
│   ├── boom-auth/          Key authentication (SHA-256 + DB + master key)
│   ├── boom-config/        YAML config parsing with env var expansion
│   ├── boom-provider/      LLM provider implementations
│   ├── boom-limiter/       Sliding window rate limiter + concurrency + PlanStore
│   ├── boom-flowcontrol/   Per-deployment flow control with VIP priority
│   ├── boom-routing/       DeploymentStore + AliasStore + scheduling policies
│   ├── boom-kvindex/       KV-cache prefix index + ZMQ subscriber + tokenization
│   ├── boom-ctxaware/      Client-type classification + agent statistics
│   ├── boom-promptlog/     Full request/response logging to disk (JSONL)
│   ├── boom-audit/         Request log read/write
│   ├── boom-dashboard/     Web UI + REST API + JWT auth
│   └── boom-main/          Entry point, routing, state assembly (binary: boom-gateway)
├── misc/LB/                Pingora load balancer (optional front-end, Dockerized)
├── misc/logo.svg           Project logo
├── config.example.yaml     Complete config reference
├── ARCH.md                 Architecture design document
└── CLAUDE.md               Development guidelines
```

## Tech Stack

| Component | Technology |
|-----------|-----------|
| Language | Rust (edition 2021) |
| HTTP Framework | Axum |
| Async Runtime | Tokio (multi-thread) |
| Database | PostgreSQL (sqlx, auto-migrate) |
| Concurrency | DashMap, ArcSwap |
| KV Events | ZMQ (PUB/SUB), MessagePack |
| Cost / Billing | rust_decimal |
| CLI | clap |
| Auth | SHA-256 token hashing, JWT sessions |

## API Endpoints

### Client API (API key)

| Endpoint | Description |
|----------|-------------|
| `POST /v1/chat/completions` | OpenAI chat (streaming / non-streaming) |
| `POST /v1/messages` | Anthropic Messages API |
| `POST /v1/completions` | OpenAI completions |
| `POST /v1/embeddings` | Returns "not supported" |
| `GET /v1/models` | Models visible to the key (deployments + aliases) |
| `GET /v1/models/{id}` | Single model info |

> OpenAI-client aliases (no `/v1` prefix) are also served: `/chat/completions`, `/completions`, `/models`.

### Admin API (master key)

| Endpoint | Description |
|----------|-------------|
| `POST /admin/config/reload` | Hot-reload config.yaml |
| `PUT /admin/plans` · `GET /admin/plans` · `DELETE /admin/plans/{name}` | Plan CRUD |
| `POST /admin/plans/assign` · `DELETE /admin/plans/assign/{key_hash}` · `GET /admin/plans/assignments` | Key→plan assignment |

### Dashboard — Auth

| Endpoint | Description |
|----------|-------------|
| `POST /dashboard/api/auth/login` | Log in (admin via master key, or user via API key) |
| `POST /dashboard/api/auth/logout` | Clear session |
| `GET /dashboard/api/auth/me` | Current session info |

### Dashboard — User API (session, scoped to caller's key)

| Endpoint | Description |
|----------|-------------|
| `GET /dashboard/api/user/plan` | Current plan + remaining quota |
| `GET /dashboard/api/user/usage` | Token / cost / request usage |
| `GET /dashboard/api/user/key-info` | Key metadata (alias, team, models, RPM) |
| `GET /dashboard/api/user/logs` | Own request logs (paginated) |
| `GET /dashboard/api/user/request-status` | Real-time status of in-flight requests |

### Dashboard — Admin API (admin session)

**Keys**
| Endpoint | Description |
|----------|-------------|
| `GET /dashboard/api/admin/keys` | List keys |
| `POST /dashboard/api/admin/keys` · `/keys/batch` | Create / batch-create keys |
| `PUT /dashboard/api/admin/keys/{token_hash}` | Update a key |
| `POST /dashboard/api/admin/keys/{token_hash}/block` · `/unblock` | Block / unblock a key |

**Models & aliases**
| Endpoint | Description |
|----------|-------------|
| `/dashboard/api/admin/models` | Model deployment CRUD |
| `/dashboard/api/admin/aliases` | Model alias CRUD |

**Plans, assignments & teams**
| Endpoint | Description |
|----------|-------------|
| `/dashboard/api/admin/plans` | Plan CRUD |
| `POST /dashboard/api/admin/assignments` · `DELETE` · `GET` | Key→plan assignment |
| `POST /dashboard/api/admin/team-assignments` · `DELETE` | Team→plan assignment |
| `/dashboard/api/admin/teams` | Team CRUD with model access control |

**Quota**
| Endpoint | Description |
|----------|-------------|
| `GET /dashboard/api/admin/quota/overview` | Team-organized quota overview |
| `GET /dashboard/api/admin/quota/team/{id}` · `/unassigned` · `/key/{hash}/windows` | Quota detail |
| `POST /dashboard/api/admin/quota/reset/...` | Reset cumulative / window quota (by key or team) |

**Logs & stats**
| Endpoint | Description |
|----------|-------------|
| `GET /dashboard/api/admin/logs` | Request logs (column filters) |
| `GET /dashboard/api/admin/usage/{key_hash}` | Per-key usage |
| `GET /dashboard/api/admin/stats/inflight` | Real-time inflight + flow control |
| `GET /dashboard/api/admin/stats/agents` | Client-type breakdown (Anthropic / Other) |
| `GET /dashboard/api/admin/stats/deployments/summary` | Per-deployment 24h summary |
| `GET /dashboard/api/admin/stats/request_rate` | Per-deployment request rate |
| `GET /dashboard/api/admin/stats/rebalance-moves` | Load-balance move counts |

**Prompt log**
| Endpoint | Description |
|----------|-------------|
| `GET /dashboard/api/admin/prompt-log/status` | Capture status |
| `POST /dashboard/api/admin/prompt-log/toggle` · `/team` · `/key` | Toggle capture (global / team / key) |
| `GET /dashboard/api/admin/prompt-log/entry/{request_id}` | Fetch one stored entry |

**Rate limits & config**
| Endpoint | Description |
|----------|-------------|
| `POST /dashboard/api/admin/limits/reset/{key_hash}` · `/reset` | Reset rate-limit windows |
| `POST /dashboard/api/admin/config/reload` | Hot-reload from dashboard UI |

**Debug** (with the `debug-tools` feature)
| Endpoint | Description |
|----------|-------------|
| `GET /dashboard/api/admin/debug/status` · `POST /toggle` | Debug recording control |
| `GET /dashboard/api/admin/debug/errors/{request_id}` | Fetch a recorded error |

### Health Checks (no auth)

| Endpoint | Description |
|----------|-------------|
| `GET /health` | Full health status |
| `GET /health/live` | Liveness probe |
| `GET /health/ready` | Readiness probe |

### Internal (no auth)

| Endpoint | Description |
|----------|-------------|
| `GET /internal/kv-index` | KV-cache prefix index status and Trie contents |

---

## Flow Control

Per-deployment flow control protects backends from burst traffic. When configured, the gateway queues requests that exceed concurrency or context limits, with **VIP keys getting priority dispatch**.

### How It Works

```
Request arrives
  │
  ├─ Check max_inflight ──── exceeded? ──→ queue (VIP first)
  │                                           │
  ├─ Check max_context ───── exceeded? ──→ reject immediately
  │                                           │
  └─ Acquire guard (RAII) ──→ release on stream end / response drop
```

### Configuration

Add `flow_control` to any model deployment (requires `model_info.id`):

```yaml
model_list:
  - model_name: claude-opus
    model_info:
      id: opus-node-1
    flow_control:
      model_queue_limit: 20          # max concurrent requests to this backend
      model_context_limit: 2000000   # max total input chars across all in-flight
    litellm_params:
      model: anthropic/claude-opus-4-20250514
      api_key: os.environ/ANTHROPIC_API_KEY
```

Or configure per-deployment via the dashboard (Model Edit dialog).

### VIP Priority Queue

Keys with `"vip": true` in their metadata skip the normal queue — their requests are always dispatched before non-VIP waiters:

```bash
# Create a VIP key
curl -X POST http://localhost:4000/dashboard/api/admin/keys \
  -H "Authorization: Bearer $MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"key_alias": "vip-user", "metadata": {"vip": true}}'
```

When a deployment slot frees up, the dispatcher greedily fills capacity from the VIP queue first, then the normal queue. This ensures premium users experience minimal latency even under heavy load.

### Key Behaviors

| Scenario | Behavior |
|----------|----------|
| `max_inflight` reached | Queue the request (VIP first), timeout after 1200s |
| `max_context` exceeded | Reject immediately (single request too large) |
| Request completes | Guard drops, slot freed, next waiter dispatched |
| Client disconnects | Guard drops, slot freed automatically |

### Monitoring

Real-time flow control stats are visible on the dashboard In-Flight panel, including:
- Current in-flight count and context usage per deployment
- Number of queued waiters (VIP vs. normal)
- Individual waiter details (key alias, wait duration)

---

## KV-Cache Aware Routing

Route requests to the vLLM worker that already holds the most relevant KV-cache prefix, reducing recomputation and TTFT. The gateway subscribes to vLLM's ZMQ KV-cache events, builds a token-prefix Trie index, and matches incoming requests against it.

### How It Works

```
vLLM Workers publish KV events via ZMQ
  │
  ▼
Gateway subscribes and builds per-model Token Prefix Trie
  │
  ▼
Request arrives → Tokenize → Walk Trie → Score candidates → Select best worker
```

**Scoring**: `combined_score = cache_weight × hit_ratio + tier_weight × tier_score + load_weight × load_score`

No match → falls back to lowest-load selection.

### Configuration

```yaml
router_settings:
  schedule_policy: kvc_aware

  kvc_aware:
    block_size: 128                 # must match vLLM's block_size
    cache_weight: 0.5               # KV prefix hit weight
    tier_weight: 0.3                # storage tier weight
    load_weight: 0.2                # load weight
    tokenizer_dir: /data/tokenizers # per-model tokenizer files
    zmq_endpoints:
      - "tcp://10.0.0.1:5557"
      - "tcp://10.0.0.2:5557"
    zmq_topic_prefix: "kv@"
```

Each model needs its tokenizer files under `{tokenizer_dir}/{model_name}/`:
- `tokenizer.json` — HuggingFace tokenizer
- `tokenizer_config.json` — chat_template, special tokens
- `chat_template.jinja` — optional standalone template file

### Identity Alignment

Gateway and vLLM identifiers must match:

| Dimension | Gateway | vLLM | Must match |
|-----------|---------|------|-----------|
| Model name | `model_name` in YAML | `--served-model-name` | Yes |
| Worker ID | `model_info.id` in YAML | ZMQ topic worker_id | Yes |
| Block size | `kvc_aware.block_size` | `--block-size` | Yes |

```yaml
- model_name: MiniMax-M2.7
  litellm_params:
    model: hosted_vllm/MiniMax-M2.7
    api_base: http://10.0.0.1:8000
  model_info:
    id: "10.0.0.1"                   # match ZMQ topic worker_id
```

### Monitoring

`GET /internal/kv-index` — inspect Trie contents, block counts, and worker assignments.

See [docs/kvc-aware-design.md](docs/kvc-aware-design.md) for the full design document.

---

## Prompt Logging

`boom-promptlog` captures the complete request and response of every request (including raw SSE chunks for streaming) and writes them to disk as JSONL — for debugging and audit.

- Layout `{dir}/{team_alias}/{key_hash}/log_NNNNNN.jsonl`, rotated and gzip-compressed in the background.
- Streaming responses are recorded via a stream wrapper that accumulates raw event chunks (no field-level parsing).
- Optional `capture_raw_upstream` stores the pre-conversion upstream body on format-converting paths (e.g. `/v1/messages`).
- Toggle capture globally, per team, or per key from the dashboard — changes apply immediately via hot reload.

```yaml
prompt_log:
  enabled: false
  dir: "/data/prompt_logs"
  max_file_size_mb: 50
  capture_raw_upstream: false
  excluded_keys: []
  excluded_teams: []
```

---

## Hot Reload

Three ways to trigger, all zero-downtime via ArcSwap atomic swap:

1. **Signal**: `kill -HUP <pid>`
2. **API**: `POST /admin/config/reload` (master key auth)
3. **Dashboard**: Click "Reload Config" button

Runtime counters (limiter, concurrency, assignments) survive reload.

---

## Load Balancer (misc/LB)

Pingora-based standalone load balancer as an optional front-end. It ships as a Docker image (multi-stage Rust build on an openEuler runtime):

```bash
./misc/LB/start.sh start              # Auto-build + cert generation
./misc/LB/start.sh start routes.yaml  # Custom config
./misc/LB/start.sh status | stop | restart
```

Routes by `host` (wildcard `*.example.com`), `path` prefix, `client_ip` CIDR.

---

## Documentation

| Document | Description |
|----------|-------------|
| [ARCH.md](ARCH.md) | Architecture: module diagram, request flow, state management, DB schema |
| [DESCRIPTOR.md](DESCRIPTOR.md) | Detailed architecture description |
| [CONFIG_EXAMPLE.md](CONFIG_EXAMPLE.md) | Complete config field reference |
| [docs/kvc-aware-design.md](docs/kvc-aware-design.md) | KVC-Aware routing design document |
| [docs/kvc-aware-event-reporting-research.md](docs/kvc-aware-event-reporting-research.md) | KV-cache event reporting research (NVIDIA Dynamo, llm-d) |
| [docs/vip-header-forwarding-design.md](docs/vip-header-forwarding-design.md) | VIP request info forwarding (`X-Gateway-Priority` header) |
| [CLAUDE.md](CLAUDE.md) | Development guidelines and architecture principles |

## License

Mulan Permissive Software License v2 (MulanPSL-2.0). See [LICENSE](LICENSE) and <http://license.coscl.org.cn/MulanPSL2>.
