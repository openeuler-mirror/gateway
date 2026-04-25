<div align="center">
  <img src="logo.svg" alt="BooMGateway" width="260">
  <br><br>
  <strong>High-Performance LLM API Gateway</strong>
</div>

A production-grade LLM API gateway built in Rust. Unified request entry point for OpenAI, Anthropic, Gemini, Bedrock, vLLM, Ollama and 20+ providers. Compatible with litellm key schema, with built-in rate limiting, plan management, flow control, web dashboard, and request auditing.

[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)

---

## Features

- **Multi-Provider Routing** — OpenAI / Anthropic / Azure / Gemini / Bedrock / vLLM / Ollama etc.
- **Load Balancing** — Round-robin or key-affinity scheduling across same-name deployments
- **Rate Limiting** — Sliding window + concurrency control + custom time windows + scheduled plans
- **Plan System** — Flexible plans with key-to-plan assignment, 3-level fallback
- **Quota Ratio** — Per-model `quota_count_ratio` so expensive models consume more quota
- **Flow Control** — Per-deployment max inflight requests + max context chars, VIP priority queue
- **Auto-Disable** — Consecutive failure detection auto-disables faulty deployments (including wildcard `*`)
- **Public Models** — `public_models` config grants all keys access to selected models without whitelist updates
- **Anthropic Native** — `/v1/messages` endpoint (Claude Code / opencode compatible)
- **Web Dashboard** — SPA admin panel: keys, models, aliases, plans, teams, logs, real-time inflight stats
- **Hot Reload** — SIGHUP, API, or dashboard button; zero-downtime config swap via ArcSwap
- **Request Auditing** — Full request logs (tokens, duration, status), streaming requests track real duration
- **Debug Recording** — Optional upstream response capture for error diagnosis
- **Containerized** — Docker multi-stage build, openEuler runtime

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

# The binary is at target/release/boom-main
```

### 2. Prepare Database

Create an empty PostgreSQL database. BooMGateway auto-creates all required tables on startup:

```bash
# Create database (any name works)
createdb boom_gateway

# Or via psql
psql -U postgres -c "CREATE DATABASE boom_gateway;"
```

That's it — no manual schema migration needed. The gateway creates 8 tables automatically:

| Table | Owner | Purpose |
|-------|-------|---------|
| `boom_request_log` | boom-audit | Request logs with token counts, duration, status |
| `boom_model_deployment` | boom-routing | Model deployment configs |
| `boom_model_alias` | boom-routing | Model alias mappings |
| `boom_rate_limit_state` | boom-limiter | Rate limit window state |
| `boom_key_plan_assignment` | boom-limiter | Key-to-plan assignments |
| `boom_rate_limit_plan` | boom-limiter | Plan definitions |
| `boom_config` | boom-dashboard | Generic KV config store |
| `boom_team_table` | boom-dashboard | Team definitions with model access |

### 3. Configure

Create `config.yaml`:

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

  # Wildcard catch-all: unmatched model names route here
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

# Plans
plan_settings:
  default_plan: "basic"
  plans:
    basic:
      concurrency_limit: 4
      rpm_limit: 60
      window_limits:
        - [100, 18000]
    pro:
      concurrency_limit: 10
      rpm_limit: 120
      window_limits:
        - [500, 18000]
```

See [CONFIG_EXAMPLE.md](CONFIG_EXAMPLE.md) for the complete reference.

### 4. Run

```bash
# Set environment variables
export MASTER_KEY="sk-your-master-key"
export DATABASE_URL="postgres://user:pass@localhost:5432/boom_gateway"
export OPENAI_API_KEY="sk-..."
export ANTHROPIC_API_KEY="sk-ant-..."

# Run
cargo run --release --bin boom-main

# Or run the built binary directly
./target/release/boom-main
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

Open `http://localhost:4000/dashboard` in your browser. Login with your master key.

---

## Project Structure

```
BooMGateway/
├── boom-gateway/           Rust workspace root
│   ├── boom-core/          Core traits and shared types
│   ├── boom-auth/          Key authentication (SHA-256 + DB + master key)
│   ├── boom-config/        YAML config parsing with env var expansion
│   ├── boom-provider/      LLM provider implementations
│   ├── boom-limiter/       Sliding window rate limiter + concurrency + PlanStore
│   ├── boom-flowcontrol/   Per-deployment flow control with VIP priority
│   ├── boom-routing/       DeploymentStore + AliasStore + scheduling policies
│   ├── boom-audit/         Request log read/write
│   ├── boom-dashboard/     Web UI + REST API + JWT auth
│   └── boom-main/          Entry point, routing, state assembly
├── misc/LB/                Pingora load balancer (optional frontend)
├── watchdog.sh             Process watchdog with auto-restart
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
| Auth | SHA-256 token hashing, JWT sessions |

## API Endpoints

### Client API (API key required)

| Endpoint | Description |
|----------|-------------|
| `POST /v1/chat/completions` | OpenAI chat (streaming / non-streaming) |
| `POST /v1/messages` | Anthropic Messages API |
| `POST /v1/completions` | OpenAI completions |
| `GET /v1/models` | Model list |

### Admin API (master key required)

| Endpoint | Description |
|----------|-------------|
| `POST /admin/config/reload` | Hot-reload config.yaml |
| `/admin/plans` | Plan CRUD |
| `/admin/plans/assign` | Key-plan assignment |

### Dashboard API (session auth)

| Endpoint | Description |
|----------|-------------|
| `/dashboard/api/admin/models` | Model deployment CRUD |
| `/dashboard/api/admin/aliases` | Model alias CRUD |
| `/dashboard/api/admin/keys` | Key management + search + VIP filter |
| `/dashboard/api/admin/keys/batch` | Batch key creation |
| `/dashboard/api/admin/plans` | Plan management |
| `/dashboard/api/admin/teams` | Team CRUD with model access control |
| `/dashboard/api/admin/assignments` | Key-plan assignments |
| `/dashboard/api/admin/logs` | Request logs with column filters |
| `/dashboard/api/admin/stats/models` | Model statistics |
| `/dashboard/api/admin/stats/inflight` | Real-time inflight + flow control |
| `/dashboard/api/admin/limits/reset` | Rate limit window reset |
| `/dashboard/api/admin/config/reload` | Hot-reload from dashboard UI |
| `/dashboard/api/admin/config` | KV config store |
| `/dashboard/api/admin/debug/*` | Debug error recording |

### Health Checks

| Endpoint | Description |
|----------|-------------|
| `GET /health` | Full health status |
| `GET /health/live` | Liveness probe |
| `GET /health/ready` | Readiness probe |

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

## Hot Reload

Three ways to trigger, all zero-downtime via ArcSwap atomic swap:

1. **Signal**: `kill -HUP <pid>`
2. **API**: `POST /admin/config/reload` (master key auth)
3. **Dashboard**: Click "Reload Config" button

Runtime counters (limiter, concurrency, assignments) survive reload.

---

## Load Balancer (misc/LB)

Pingora-based standalone load balancer as optional frontend:

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
| [CLAUDE.md](CLAUDE.md) | Development guidelines and architecture principles |

## License

Apache License 2.0
