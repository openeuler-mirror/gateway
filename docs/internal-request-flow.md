# BooMGateway 内部请求流转图

本文档从**一次请求在网关内部流经的所有模块及其关系**维度描绘。配套文档 [user-facing-modules.md](./user-facing-modules.md) 从用户感知维度展开。

读者:核心开发 / 架构 review / 排障。

---

## 0. 全景总图(全部 13 crate + 调用关系 + 外部交互)

```
   外部触发源                       BooMGateway 进程(Tokio 32 workers)                       外部依赖
  ───────────                     ────────────────────────────────────────                   ───────────

                                                                                          ┌───────────────┐
                                                                                          │  PostgreSQL   │
                                                                                          │  (sqlx pool)  │
                                                                                          │               │
                                                                                          │  • LiteLLM_*  │
                                                                                          │    (key/team) │
                                                                                          │  • boom_req_  │
                                                                                          │    log        │
                                                                                          │  • boom_model_│
                                                                                          │    dep/alias  │
                                                                                          │  • boom_rate_ │
                                                                                          │    limit_*    │
                                                                                          │  • boom_team_ │
                                                                                          │    table      │
                                                                                          │  • boom_config│
                                                                                          └───────▲───────┘
                                                                                                  │ SQL
                                                                                                  │
  ┌──────────┐   HTTPS Bearer                                                                     │
  │ OpenAI   │ ─────────────────┐                                                                 │
  │ SDK      │                  │                                                                 │
  └──────────┘                  │                                                                 │
                                │                                                                 │
  ┌──────────┐   /v1/messages   │                                                                 │
  │Anthropic │ ─────────────────┤                                                                 │
  │ SDK /    │                  │     ┌─────────────────────────────────────────────────┐         │
  │ClaudeCode│                  ├────▶│              boom-main (binary)                 │         │
  └──────────┘                  │     │   (axum HTTP server + 路由组装 + AppState)      │         │
                                │     │                                                 │         │
  ┌──────────┐   JWT cookie     │     │  ┌──────────── HTTP 请求处理线程 ─────────────┐  │         │
  │ Browser  │ ─────────────────┤     │  │                                            │  │         │
  │Dashboard │                  │     │  │  extractor ──┐                             │  │         │
  │ (User +  │                  │     │  │              ▼                             │  │         │
  │  Admin)  │                  │     │  │   ╔═══════════════════════════════════╗    │  │         │
  └──────────┘                  │     │  │   ║ boom-auth  (DbAuthenticator)      ║────┼─────────┤
                                │     │  │   ║   SHA-256 + moka cache            ║    │         │
  ┌──────────┐   kill -HUP      │     │  │   ╚═══════════════════════════════════╝    │  │         │
  │ 运维     │ ─────────────────┤     │  │              │                              │  │         │
  │ (signal) │                  │     │  │              ▼                              │  │         │
  └──────────┘                  │     │  │   ╔═══════════════════════════════════╗    │  │         │
                                │     │  │   ║ boom-limiter                       ║────┼─────────┤
                                │     │  │   ║   ├─ PlanStore (key + team)        ║    │  (读写   │
                                │     │  │   ║   ├─ SlidingWindowLimiter          ║    │   rate_  │
                                │     │  │   ║   │    (counts/tokens/costs 三维   ║    │   limit  │
                                │     │  │   ║   │     + cumulative total)        ║    │   表)    │
                                │     │  │   ║   ├─ ConcurrencyGuard (RAII)       ║    │         │
                                │     │  │   ║   └─ PlanCharge (commit + settle)  ║    │         │
                                │     │  │   ╚═══════════════════════════════════╝    │  │         │
                                │     │  │              ▼                              │  │         │
                                │     │  │   ╔═══════════════════════════════════╗    │  │         │
                                │     │  │   ║ boom-routing  (Router)             ║    │  │         │
                                │     │  │   ║ ★ 调度层 boom-sched ───────────   ║    │  │         │
                                │     │  │   ║  (位于 limiter 之后:通过限流的   ║    │  │         │
                                │     │  │   ║   请求才进入调度,未通过直接 429) ║    │  │         │
                                │     │  │   ║                                   ║    │  │         │
                                │     │  │   ║  Phase A — 决定 model_name:       ║    │  │         │
                                │     │  │   ║   ├─ HybridRouter (TierClassifier)║    │  │         │
                                │     │  │   ║   │    基于 messages/tools 评分   ║    │  │         │
                                │     │  │   ║   └─ AliasStore (别名解析)        ║    │  │         │
                                │     │  │   ║                                   ║    │  │         │
                                │     │  │   ║  Phase B — 抽屉式可插拔策略:      ║    │  │         │
                                │     │  │   ║   ├─ L0 RoundRobin  (兜底默认)    ║    │  │         │
                                │     │  │   ║   ├─ L1 KeyAffinity (key 粘滞)    ║    │  │         │
                                │     │  │   ║   ├─ L2 PromptPrefix(本地hash,🟡)║    │  │         │
                                │     │  │   ║   ├─ L3 KvcAware    (ZMQ 上报)    ║    │  │         │
                                │     │  │   ║   └─ .. LLM-LA / 第三方插件       ║    │  │         │
                                │     │  │   ║                                   ║    │  │         │
                                │     │  │   ║  支撑组件:                        ║    │  │         │
                                │     │  │   ║   ├─ DeploymentStore (含          ║    │  │         │
                                │     │  │   ║   │   quota_count_ratio +          ║    │  │         │
                                │     │  │   ║   │   ModelCostRate)               ║    │  │         │
                                │     │  │   ║   ├─ InFlightTracker              ║    │  │         │
                                │     │  │   ║   ├─ RebalanceMoveTracker         ║    │  │         │
                                │     │  │   ║   └─ RequestRateTracker           ║    │  │         │
                                │     │  │   ║                                   ║    │  │         │
                                │     │  │   ║  兜底:匹配不到 → "*" deployment  ║    │  │         │
                                │     │  │   ╚═══════════════════════════════════╝    │  │         │
                                │     │  │              │                              │  │         │
                                │     │  │              │ ┌──[读]── boom-kvindex ──┐   │  │         │
                                │     │  │              │ │     (TokenPrefixIndex │   │  │         │
                                │     │  │              │ │      + TokenizerPool) │   │  │         │
                                │     │  │              │ └──────────────────────┘   │  │         │
                                │     │  │              ▼                              │  │         │
                                │     │  │   ╔═══════════════════════════════════╗    │  │         │
                                │     │  │   ║ boom-flowcontrol (FlowController)  ║    │  │         │
                                │     │  │   ║   per-dep max_inflight + VIP 队列  ║    │  │         │
                                │     │  │   ╚═══════════════════════════════════╝    │  │         │
                                │     │  │              │                              │  │         │
                                │     │  │              ▼                              │  │         │
                                │     │  │   ╔═══════════════════════════════════╗    │  │         │
                                │     │  │   ║ boom-provider                      ║────┼──HTTPS──┼─▶ LLM API
                                │     │  │   ║   ├─ OpenAIProvider                ║    │          │  (OpenAI /
                                │     │  │   ║   ├─ AnthropicProvider             ║    │          │   Anthropic /
                                │     │  │   ║   ├─ AzureProvider                 ║    │          │   Bedrock /
                                │     │  │   ║   ├─ BedrockProvider               ║    │          │   Gemini /
                                │     │  │   ║   └─ GeminiProvider                ║    │          │   vLLM)
                                │     │  │   ╚═══════════════════════════════════╝    │  │         │
                                │     │  │              │                              │  │         │
                                │     │  │              ▼ (流式响应包装,详见 §4)   │  │         │
                                │     │  │   ╔═══════════════════════════════════╗    │  │         │
                                │     │  │   ║ boom-audit  (log_request)          ║────┼─────────┤ (写
                                │     │  │   ║   boom_request_log 表              ║    │  │        boom_req_
                                │     │  │   ╚═══════════════════════════════════╝    │  │         log)
                                │     │  │              │                              │  │         │
                                │     │  │              ▼                              │  │         │
                                │     │  │   ╔═══════════════════════════════════╗    │  │         │
                                │     │  │   ║ boom-promptlog (PromptLogWriter)   ║────┼──I/O────┼─▶ 文件系统
                                │     │  │   ║   完整 req/resp JSONL              ║    │          │   (prompt_
                                │     │  │   ║   含 raw upstream + 流式 chunks   ║    │          │    log/*.jsonl
                                │     │  │   ╚═══════════════════════════════════╝    │  │         │    +
                                │     │  │              │                              │  │         │    snapshot)
                                │     │  │              ▼                              │  │         │
                                │     │  │   ╔═══════════════════════════════════╗    │  │         │
                                │     │  │   ║ boom-ctxaware                      ║    │  │         │
                                │     │  │   ║   ├─ classify (path → Anthropic/   ║    │  │         │
                                │     │  │   ║   │         Other)                 ║    │  │         │
                                │     │  │   ║   └─ AgentStatsTracker (60 min 环形)║    │  │         │
                                │     │  │   ╚═══════════════════════════════════╝    │  │         │
                                │     │  │              │                              │  │         │
                                │     │  │              ▼                              │  │         │
                                │     │  │   ╔═══════════════════════════════════╗    │  │         │
                                │     │  │   ║ boom-dashboard  (Extension 注入)    ║────┼─────────┤ (读写
                                │     │  │   ║   ├─ handlers_admin (User 端)      ║    │  │        boom_team
                                │     │  │   ║   ├─ handlers_user  (Admin 端)     ║    │  │        /config
                                │     │  │   ║   ├─ auth (JWT 登录)                ║    │  │        表)
                                │     │  │   ║   ├─ stats_timeseries              ║    │  │         │
                                │     │  │   ║   └─ frontend (内嵌 SPA)           ║    │  │         │
                                │     │  │   ╚═══════════════════════╤═════════════╝    │  │         │
                                │     │  │                          │ mpsc            │  │         │
                                │     │  │                          │ AdminCommand    │  │         │
                                │     │  │                          ▼                 │  │         │
                                │     │  │   ╔═══════════════════════════════════╗    │  │         │
                                │     │  │   ║ admin_command_handler (§5 ⑦)      ║    │  │         │
                                │     │  │   ║   CreateModel/UpdateModel/...     ║    │  │         │
                                │     │  │   ╚═══════════════════════════════════╝    │  │         │
                                │     │  └────────────────────────────────────────────┘  │         │
                                │     │                                                  │   │         │
                                │     │   启动 + reload 时(无运行时依赖):            │   │         │
                                │     │   ┌──────────────────────────────────────┐      │   │         │
                                │     │   │ boom-config  (load_config + env 解析)│ ◀YAML┼───┼─────────┤
                                │     │   └──────────────────────────────────────┘      │   │         │
                                │     │                                                  │   │         │
                                │     │   全部模块共享的叶子:                          │   │         │
                                │     │   ┌──────────────────────────────────────┐      │   │         │
                                │     │   │ boom-core                             │      │   │         │
                                │     │   │   traits: Provider/Authenticator/    │      │   │         │
                                │     │   │           RateLimiter/KeyAliasLookup │      │   │         │
                                │     │   │   types:  ChatRequest / AuthIdentity │      │   │         │
                                │     │   │           / RateLimitKey / ...      │      │   │         │
                                │     │   │   错误:   GatewayError                │      │   │         │
                                │     │   │   转码:   anthropic.rs / normalize.rs│      │   │         │
                                │     │   │   kv_event::KvIndexBackend           │      │   │         │
                                │     │   │   debug_store + db_util              │      │   │         │
                                │     │   └──────────────────────────────────────┘      │   │         │
                                │     └──────────────────────────────────────────────────┘   │         │
                                │                                                              │         │
                                │                                                              │         │
                                │     后台任务(Tokio spawn,详见 §5)                        │         │
                                │     ┌─────────────────────────────────────────────────┐    │         │
                                │     │ ① SIGHUP listener        ─▶ AppState::reload    │    │         │
                                │     │ ② Sync task (10 min)     ─▶ limiter/plan_store  │────┼─────────┤
                                │     │                            DB 持久化            │    │  (写    │
                                │     │ ③ Request summary (60s)                        │    │   rate_
                                │     │ ④ FC dispatch (1s)       ─▶ FlowController     │    │   limit_state
                                │     │ ⑤ ZMQ KV subscriber ◀────┘                      │    │   表)
                                │     │   (boom-kvindex)                                │    │         │
                                │     │ ⑥ Deployment health monitor                     │────┼─────────┤
                                │     │                            (auto-disable)       │    │  (写
                                │     │ ⑦ admin_command_handler                         │    │   boom_model_
                                │     │ ⑧ PromptLog writer task ─▶ JSONL                │────┼   deployment
                                │     │   (boom-promptlog)                              │    │   .auto_disabled)
                                │     └─────────────────────────────────────────────────┘    │         │
                                │                                                              │         │
                                                                                              │         │
                                                                                              │         │
                                                                                              │         │
                                                                                              │         │
                  ┌──────────────────┐                                                       │         │
                  │ 文件系统         │ ◀─────────── JSONL / snapshot YAML ───────────────────┼─────────┘
                  │ • config.yaml    │                                                       │
                  │ • prompt_log/    │ ◀─────────── reload ─────────────────────────────────┤
                  │ • {cfg}.时间戳   │                                                       │
                  └──────────────────┘                                                       │
                                                                                             │
                  ┌──────────────────┐                                                       │
                  │ vLLM workers     │ ─── ZMQ PUB (kv@ topic, msgpack) ─────────────────────┘
                  │ (PUB: KV events) │       携带 prefix block events, 后台任务 ⑤ 订阅
                  └──────────────────┘       写入 TokenPrefixIndex 供 KvcAwarePolicy 查询
                                                                                                            │
                  ┌──────────────────┐                                                                                  │
                  │ OS Signal        │ ─── SIGHUP → reload / SIGTERM → exit ─────────────────────────────────────────────────┘
                  │ • SIGHUP         │
                  │ • SIGTERM        │
                  │ • Ctrl+C         │
                  └──────────────────┘
```

**编译期依赖方向**(单向无环):

```
   boom-core  ◀────────── 所有 boom-* 模块都依赖 core (叶子)
       ▲
       │
       ├── boom-auth
       ├── boom-config
       ├── boom-provider
       ├── boom-limiter
       ├── boom-routing  (额外依赖:auth 用于 alias 解析时返回 key 信息)
       ├── boom-flowcontrol  (依赖 core::DeploymentQueueInfo trait)
       ├── boom-audit
       ├── boom-kvindex  (依赖 core::kv_event::KvIndexBackend)
       ├── boom-ctxaware
       ├── boom-promptlog
       ├── boom-dashboard  ❌ 不依赖 boom-provider / boom-config (架构原则)
       │       │
       │       └── mpsc::channel (AdminCommand) ──┐
       │                                          │
       └── boom-main  (依赖全部) ◀────────────────┘
              ▲
              │
       (根,唯一的 binary)
```

**运行期调用方向**(主请求路径):
`extractor → auth → routing → limiter → flowcontrol → provider → audit/promptlog/ctxaware → 客户端`

**外部交互 6 类**:
1. `HTTPS ▶ LLM API` — Provider 转发(boom-provider)
2. `SQL  ▶ PostgreSQL` — 配置/认证/日志/配额(boom-auth, boom-audit, boom-limiter, boom-routing, boom-dashboard)
3. `ZMQ  ◀ vLLM workers` — KV-cache 事件订阅(boom-kvindex)
4. `I/O  ▶ 文件系统` — prompt_log JSONL + 配置 snapshot(boom-promptlog, boom-main)
5. `I/O  ◀ YAML` — 配置文件读取(boom-config)
6. `Signal ▶` — SIGHUP / SIGTERM / Ctrl+C(boom-main)

---

## 1. 请求生命周期总览(自上而下)

```
                  ┌──────────────────────────────────────────────┐
                  │  HTTP 请求到达 axum                           │
                  │  (CorsLayer + request_count 中间件已计数)     │
                  └────────────────────┬─────────────────────────┘
                                       ▼
        ╔═══════════════════════════════════════════════════════════╗
        ║ 1. 接入层      boom-main::extractor (RequiredAuth)        ║
        ║    从 Authorization 头提 Bearer,准备交给认证             ║
        ╚═══════════════════════════════════════════════════════════╝
                                       ▼
        ╔═══════════════════════════════════════════════════════════╗
        ║ 2. 认证层      boom-auth::DbAuthenticator                 ║
        ║    master_key 常量比较  OR  sk- → SHA-256 → moka → DB     ║
        ║    返回 AuthIdentity{key_hash, team_id, models, metadata} ║
        ╚═══════════════════════════════════════════════════════════╝
                                       ▼
        ╔═══════════════════════════════════════════════════════════╗
        ║ 3. 访问控制    boom-main routes + boom-routing::Router    ║
        ║    check_model_access: 白名单 + 别名 + public_models + "*"║
        ╚═══════════════════════════════════════════════════════════╝
                                       ▼
        ╔═══════════════════════════════════════════════════════════╗
        ║ 4. 配额层      boom-limiter + PlanStore                   ║
        ║    Team 维度 peek ─▶ Key 维度 peek                        ║
        ║    (counts / tokens / costs 三维窗口 + cumulative total)  ║
        ║    返回 PlanCharge(未 commit,Drop 时无副作用)            ║
        ║    ⚠️ 未通过直接 429,不进入下方调度层                    ║
        ╚═══════════════════════════════════════════════════════════╝
                                       ▼
        ╔═══════════════════════════════════════════════════════════╗
        ║ ★ 5. 调度层 Phase A — 模型层解析 ★                        ║
        ║    boom-routing::Router::resolve_request_model            ║
        ║    ① HybridRouter.classify(messages+tools) → tier → model ║
        ║    ② AliasStore.resolve(alias) → target_model            ║
        ║    ③ DeploymentStore 精确匹配 → 单 dep / 多 dep          ║
        ║    ④ 都不匹配 → "*" deployment 兜底                       ║
        ║    (输出:model_name + 同名 deployments 列表)             ║
        ╚═══════════════════════════════════════════════════════════╝
                                       ▼
        ╔═══════════════════════════════════════════════════════════╗
        ║ ★ 6. 调度层 Phase B — 后端选择 ★                          ║
        ║    boom-routing::Router::select_provider_with_prefix      ║
        ║    Policy.select_with_context(candidates, key, token_ids) ║
        ║    抽屉式可插拔策略(详见 §2):                          ║
        ║    ├─ L0 RoundRobinPolicy  无状态轮询(兜底)             ║
        ║    ├─ L1 KeyAffinityPolicy key 粘滞 + 负载再平衡          ║
        ║    ├─ L2 PromptPrefix(🟡验证) 本地 hash 前缀树         ║
        ║    ├─ L3 KvcAwarePolicy    KV 前缀命中 + tier + load      ║
        ║    └─ .. LLM-LA / 第三方插件(实现 trait 即可挂载)      ║
        ║    boom-kvindex 提供前缀命中比例(若启用 L3 KvcAware)    ║
        ║    (输出:单一 deployment + kv_hit_ratio)                 ║
        ╚═══════════════════════════════════════════════════════════╝
                                       ▼
        ╔═══════════════════════════════════════════════════════════╗
        ║ 7. 流控层      boom-flowcontrol::FlowController           ║
        ║    per-deployment acquire(max_inflight + max_context)     ║
        ║    VIP 队列优先,AcquireCleanup::drop 防泄漏              ║
        ╚═══════════════════════════════════════════════════════════╝
                                       ▼
        ╔═══════════════════════════════════════════════════════════╗
        ║ 8. Provider    boom-provider (OpenAI/Anthropic/Bedrock/..)║
        ║    provider.chat()  或  provider.chat_stream()             ║
        ║    可能附 X-Gateway-Priority / X-BooM-Client-Type 头      ║
        ╚═══════════════════════════════════════════════════════════╝
                                       ▼
        ╔═══════════════════════════════════════════════════════════╗
        ║ 9. 流包装层 (嵌套洋葱)                                     ║
        ║   从内到外:Provider 原始流                                ║
        ║     ─▶ UsageTracker 旁路 (抽 usage chunk)                 ║
        ║     ─▶ InFlightStream       (持 InFlightGuard)            ║
        ║     ─▶ FlowControlledStream (持 FlowControlGuard)         ║
        ║     ─▶ GuardedStream        (持 ConcurrencyGuard)         ║
        ║     ─▶ LoggedStream         (Drop 时写 audit + settle)    ║
        ║     ─▶ PromptLogStream      (Drop 时写 prompt_log JSONL)  ║
        ║     ─▶ [Anthropic 路径] Transcoder (OpenAI SSE→Anthropic) ║
        ║     ─▶ SSE 响应客户端                                      ║
        ╚═══════════════════════════════════════════════════════════╝
                                       ▼
        ╔═══════════════════════════════════════════════════════════╗
        ║ 10. Drop 时 (流结束 / 连接断 / 错误)                       ║
        ║    ─ InFlightTracker 计数 ─                               ║
        ║    ─ FlowControl slot 让出 + 触发 dispatch                ║
        ║    ─ Concurrency guard 释放                               ║
        ║    ─ LoggedStream 写 boom_request_log + settle PlanCharge ║
        ║    ─ PromptLogStream 异步写 JSONL                          ║
        ║    ─ agent_stats / request_rate 更新                      ║
        ╚═══════════════════════════════════════════════════════════╝
```

---

## 2. 调度层 boom-sched 深度解析 (Scheduling Layer Deep Dive)

★ 这是 LLM 网关区别于通用 API 网关的最核心能力 ★

调度层位于配额限流**之后**(参见 §1 的 step 4 限流 → step 5 Phase A → step 6 Phase B)。限流先决定"能不能服务",不能服务直接 429,避免无谓的 prefix 计算;通过限流后才进入调度层,决定"服务到哪个后端"。

调度层分两阶段:**Phase A** 决定 model_name,**Phase B** 用**抽屉式可插拔策略**选具体后端。Phase B 的策略分 4 个内置等级(L0-L3)+ 第三方扩展。

### 2.1 L0-L3 等级总览

```
   等级   策略名           数据来源             精度   复杂度        状态
   ─────────────────────────────────────────────────────────────────────
   L0    RoundRobin       无                   低     O(1)          ✅ 已实现(默认)
   L1    KeyAffinity      key→dep 粘滞表+负载  中下   O(1)          ✅ 已实现
   L2    PromptPrefix 🟡  网关本地 hash 前缀树 中     O(prefix)    🟡 生产验证
   L3    KvcAware         ZMQ 上报 + Trie      高     O(prefix)    ✅ 已实现
   ..    LLM-LA / 第三方  自定义               自定义 自定义         🔌 可插拔扩展
   ─────────────────────────────────────────────────────────────────────

   精度递增 ─────────────────────────────────────────────────────▶
   复杂度递增 ───────────────────────────────────────────────────▶
   后端配合度递增 ───────────────────────────────────────────────▶  (L0/L1/L2 不需后端配合; L3 需 vLLM ZMQ)
```

### 2.2 两阶段框架

```
                        请求 (req.model + messages + tools)
                                     │
                                     ▼
                      (step 4) check_plan_limits 限流
                                     │
                                     │  429 直接拒绝,不进入调度
                                     ▼
   ┌──────────────────────────────────────────────────────────────┐
   │  Phase A — 模型层解析(决定走哪个 model_name)              │
   │                                                              │
   │  调用:Router::resolve_request_model(model, messages, tools)│
   │                                                              │
   │   ① HybridRouter(若启用)                                  │
   │      │ classify(messages, tools) → StrategyRegistry          │
   │      │ → TierClassifier(默认)                              │
   │      │   ├─ 关键词命中:tool_calls/system/reasoning → 高 tier│
   │      │   ├─ 启发式评分:消息数、token 估计、tool 复杂度      │
   │      │   └─ 输出 tier:fast / standard / pro                 │
   │      │                                                       │
   │      └─ virtual_model → 真实 model_name(tier 选定)        │
   │                                                              │
   │   ② AliasStore(若 req.model 是别名)                       │
   │      └─ alias_name → target_model                           │
   │                                                              │
   │   ③ DeploymentStore.contains(model_name)?                  │
   │      ├─ Y → 取同名 deployments(可能 1 个或多个)            │
   │      └─ N → 看 "*" deployment(兜底)                       │
   │                                                              │
   │   ④ 都没有 → GatewayError::ModelNotFound                    │
   │                                                              │
   │   输出:(model_name, Vec<Deployment>)                       │
   └──────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
   ┌──────────────────────────────────────────────────────────────┐
   │  Phase B — 后端选择(同 model 多 deployment 中选 1)        │
   │                                                              │
   │  调用:Router::select_provider_with_prefix(                  │
   │           model, key, input_chars, token_ids)                │
   │                                                              │
   │   预处理:                                                    │
   │     ├─ DeploymentStore.get_providers(model_name)             │
   │     │  过滤掉 auto_disabled=true 的 dep                     │
   │     │                                                        │
   │     └─ (若启用 L3 KvcAware) TokenizerPool.tokenize_openai   │
   │        → token_ids(用于 Trie 查 prefix)                  │
   │                                                              │
   │   Policy.select_with_context(candidates, ctx):              │
   │     根据 schedule_policy 配置,4 级 + 第三方任选其一:      │
   │       L0 RoundRobinPolicy    (schedule_policy="round_robin")│
   │       L1 KeyAffinityPolicy   (schedule_policy="key_affinity")│
   │       L2 PromptPrefixPolicy  (🟡生产验证,schedule_policy="prompt_prefix")│
   │       L3 KvcAwarePolicy      (schedule_policy="kvc_aware")  │
   │       .. 任意注册的第三方插件(schedule_policy="<plugin_name>")│
   │                                                              │
   │   输出:Selection { deployment, kv_hit_ratio: Option<f64> } │
   └──────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
              (继续 step 7 — FlowController.acquire)
```

### 2.3 Phase B 各等级 Policy 内部对比

#### L0 — RoundRobinPolicy(兜底默认)

```
┌──────────────────────────────────────────────────────────┐
│  state:AtomicUsize(per model)                            │
│                                                          │
│  select_with_context(candidates, _ctx):                  │
│    idx = counter.fetch_add(1) % candidates.len()         │
│    return candidates[idx]                                 │
│                                                          │
│  特点:                                                   │
│    ─ 无状态、O(1)、无内存压力                            │
│    ─ 不感知负载、不感知 KV-cache                         │
│    ─ 适合后端等价、流量平均的场景                         │
│                                                          │
│  fallback:无(总是返回)                                 │
└──────────────────────────────────────────────────────────┘
```

#### L1 — KeyAffinityPolicy

```
┌──────────────────────────────────────────────────────────┐
│  state:                                                  │
│    ├─ DashMap<key_hash, deployment_id>(粘滞表)         │
│    └─ DashMap<deployment_id, last_key_hash>(反向表)    │
│                                                          │
│  select_with_context(candidates, ctx):                   │
│    key = ctx.key_hash                                    │
│    if affinity.contains(key):                            │
│      dep = affinity[key]                                 │
│      if dep in candidates && not overloaded(dep):        │
│        return dep   ← 命中粘滞                           │
│      else:                                               │
│        RebalanceMoveTracker.record(key, old, new)        │
│        ─ 切换次数可观测(Dashboard 展示)                 │
│                                                          │
│    dep = lowest_load(candidates)  ← InFlight + FC queue │
│    affinity[key] = dep                                   │
│    return dep                                            │
│                                                          │
│  负载信号:                                               │
│    ─ InFlightTracker.get_model_input_chars(dep)          │
│    ─ FlowController.queue_info(dep)(VIP/waiters)         │
│                                                          │
│  特点:                                                   │
│    ─ 提升 key 维度的 KV 复用(粗粒度)                   │
│    ─ 当原 dep 过载时自动再平衡                           │
│    ─ 不需后端配合                                        │
│                                                          │
│  fallback:lowest-load                                    │
└──────────────────────────────────────────────────────────┘
```

#### L2 — PromptPrefixPolicy 🟡 生产验证中

```
┌──────────────────────────────────────────────────────────┐
│  ★ 网关完全本地预测,不需后端配合 ★                     │
│                                                          │
│  state:                                                  │
│    └─ local_prefix_trie: LocalPrefixTrie                 │
│         (网关侧自建,与 L3 ZMQ Trie 是两个独立结构)     │
│                                                          │
│  请求路径(每次请求都触发更新):                          │
│    1. tokenize(req) → token_ids                          │
│    2. chunks = token_ids.chunks(block_size)              │
│    3. hashes = chunks.map(hash)                          │
│    4. local_prefix_trie.insert(hashes, dep_id)           │
│       (选定的 dep 也记录到 trie,供后续预测)             │
│                                                          │
│  select_with_context(candidates, ctx):                   │
│    token_ids = ctx.token_ids                             │
│    hashes = compute_block_hashes(token_ids)              │
│    hit_counts = local_prefix_trie.match_prefix(hashes)   │
│      → HashMap<dep_id, count>                           │
│                                                          │
│    dep = argmax(hit_counts) if any > 0                   │
│          else lowest_load(candidates)                    │
│    return dep                                            │
│                                                          │
│  特点:                                                   │
│    ─ 不依赖后端(任何 OpenAI 兼容后端都行)             │
│    ─ 是"预测"(基于历史派发记录),L3 是"实测"          │
│    ─ 精度低于 L3(后端可能已 LRU 淘汰但网关 Trie 不知)  │
│    ─ 适合非 vLLM 后端 / 无法改造后端的场景               │
│                                                          │
│  状态:生产环境验证中(boom-kvindex 提供 Tokenizer、   │
│       block hash、Trie 基础设施)                        │
│                                                          │
│  fallback:lowest-load                                    │
└──────────────────────────────────────────────────────────┘
```

#### L3 — KvcAwarePolicy ★ 杀手锏,降 TTFT

```
┌──────────────────────────────────────────────────────────┐
│  ★ 降低 TTFT 的杀手锏,需配合 vLLM 开启 ZMQ PUB ★       │
│                                                          │
│  state:                                                  │
│    ├─ kv_index:Arc<ArcSwap<Option<TokenPrefixTrie>>>   │
│    │   (第三种生命周期,见 §7)                          │
│    └─ tokenizer_pool:Arc<ArcSwap<Option<...>>>          │
│                                                          │
│  select_with_context(candidates, ctx):                   │
│    token_ids = ctx.token_ids(来自 TokenizerPool)        │
│    kv_index.match_prefix(token_ids) →                   │
│      per-worker 命中比例(0.0 .. 1.0)                   │
│                                                          │
│    combined_score[dep] =                                 │
│        cache_weight * hit_ratio[dep]                     │
│      + tier_weight   * tier_rank[dep]                    │
│      + load_weight   * load_norm[dep]                    │
│                                                          │
│    return argmax(combined_score)                         │
│                                                          │
│  上游数据来源:                                          │
│    vLLM worker ─ZMQ PUB─▶ boom-kvindex::spawn_kv_        │
│      subscriber(后台任务)                               │
│      ├─ tmq::subscribe *N endpoints                      │
│      ├─ 解析 msgpack 3-frame                             │
│      └─ kv_index.insert_blocks(worker_id, tokens)        │
│                                                          │
│  fallback:                                               │
│    若所有 worker 都无 prefix 命中 → lowest-load          │
│    若 Trie 为空(reload 后)→ 退化到 RoundRobin 语义     │
│                                                          │
│  特点:                                                   │
│    ─ KV 复用最大化(vLLM 可跳过 prefill)                │
│    ─ 显著降低 TTFT(尤其长 prefix 场景)                 │
│    ─ 需要 vLLM 配合 ZMQ PUB(kv@ topic)                 │
│                                                          │
│  fallback:lowest-load                                    │
└──────────────────────────────────────────────────────────┘
```

### 2.4 L3 KvcAware 与 boom-kvindex 的耦合

```
       ┌─────────────────────────────────────────────────┐
       │  vLLM worker(s)                                 │
       │                                                 │
       │   ① 每个 worker 启动时建立 ZMQ PUB socket      │
       │   ② topic = "kv@"                               │
       │   ③ KV-cache prefix block 进/出时发 msgpack:   │
       │      frame[0]: worker_id(u64)                  │
       │      frame[1]: block_id(u64)                   │
       │      frame[2]: token_ids(Vec<u32>)             │
       └─────────────────────────────────────────────────┘
                            │
                            │ ZMQ PUB (TCP)
                            ▼
       ┌─────────────────────────────────────────────────┐
       │  boom-kvindex(spawn_kv_subscriber)             │
       │                                                 │
       │   ① tmq::subscribe *N vllm_endpoints            │
       │   ② stream::select_all merge 多 endpoint        │
       │   ③ 每条 message:                               │
       │      ├─ 解析 3-frame msgpack                    │
       │      ├─ tokenizer_pool 校验(可选)              │
       │      └─ kv_index.insert_blocks(worker, tokens)  │
       │                                                 │
       │   Trie 结构:                                   │
       │     root → token_node → token_node → ... → leaf│
       │     leaf: { workers: HashMap<worker_id, count> }│
       └─────────────────────────────────────────────────┘
                            │
                            │ 读:kv_index.match_prefix(req_token_ids)
                            │   返回 HashMap<worker_id, hit_count>
                            ▼
       ┌─────────────────────────────────────────────────┐
       │  L3 KvcAwarePolicy.select_with_context           │
       │                                                 │
       │   ① token_ids = TokenizerPool.tokenize(req)    │
       │   ② hit_ratio = kv_index.match_prefix(token_ids)│
       │   ③ combined = cache·hit + tier·rank + load·norm│
       │   ④ winner = argmax(combined)                   │
       │   ⑤ need_full_kv_report(hit_ratio, threshold)?  │
       │      Y → 给 vLLM 加 header 要求下次全量上报    │
       └─────────────────────────────────────────────────┘
```

### 2.5 可插拔调度架构 ★ 灵活扩展能力 ★

调度机制采用**抽屉式(pluggable)**设计:核心层只定义 trait 与共享上下文,所有策略作为独立"插件"接入,可按场景替换或扩展。

```
                  ┌──────────────────────────────────────────┐
                  │              BooMGateway                 │
                  │                                          │
                  │   ┌──────────────────────────────────┐   │
                  │   │   调度机制核心(机制层)         │   │
                  │   │                                  │   │
                  │   │   • SchedulePolicy trait         │   │
                  │   │     fn select_with_context(     │   │
                  │   │       &self,                    │   │
                  │   │       candidates: &[Deployment],│   │
                  │   │       ctx: &ScheduleCtx,        │   │
                  │   │     ) -> Result<Selection>      │   │
                  │   │                                  │   │
                  │   │   • StrategyRegistry            │   │
                  │   │     (注册中心,name → Arc<dyn   │   │
                  │   │      SchedulePolicy>)           │   │
                  │   │                                  │   │
                  │   │   • 共享调度上下文 ScheduleCtx:│   │
                  │   │     { key_hash, model_name,    │   │
                  │   │       messages, tools,          │   │
                  │   │       token_ids, input_chars,   │   │
                  │   │       inflight_load,            │   │
                  │   │       fc_queue_info,            │   │
                  │   │       vip, kv_index, ... }      │   │
                  │   │                                  │   │
                  │   │   • 配置驱动选择:               │   │
                  │   │     schedule_policy:            │   │
                  │   │       "L0|L1|L2|L3|<plugin>"    │   │
                  │   └──────────────┬───────────────────┘   │
                  │                  │                       │
                  │   ╔══════════════▼══════════════╗       │
                  │   ║  "抽屉式"可插拔插槽         ║       │
                  │   ║  (pluggable slot / drawer)  ║       │
                  │   ║                              ║       │
                  │   ║  impl SchedulePolicy +       ║       │
                  │   ║  StrategyRegistry::register  ║       │
                  │   ║  即可挂载新策略              ║       │
                  │   ╚══════════════╤══════════════╝       │
                  │                  │                       │
                  └──────────────────┼───────────────────────┘
                                     │
                                     │  下方所有策略实现同一 trait,
                                     │  对核心层透明可替换
                                     │
         ┌──────────┬──────────┬─────┴────┬──────────┬─────────────┐
         │          │          │          │          │             │
         ▼          ▼          ▼          ▼          ▼             ▼
   ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐
   │   L0    │ │   L1    │ │   L2    │ │   L3    │ │ LLM-LA  │ │   ...   │
   │         │ │         │ │         │ │         │ │         │ │         │
   │ Round   │ │   Key   │ │ Prompt  │ │   KV    │ │ 第三方  │ │ 更多    │
   │ Robin   │ │Affinity │ │ Prefix  │ │  Aware  │ │ 示例    │ │ 扩展    │
   │         │ │         │ │ (本地)  │ │ (ZMQ)   │ │ 插件    │ │ 插件    │
   │         │ │         │ │         │ │         │ │         │ │         │
   │ ✅内置   │ │ ✅内置   │ │ 🟡 验证 │ │ ✅内置   │ │ 🔌 外部 │ │ 🔌 外部 │
   └─────────┘ └─────────┘ └─────────┘ └─────────┘ └─────────┘ └─────────┘
        └──────────┬──────────┘          │          └────┬────┘
                   │                     │               │
                   └───── 内置策略族(L0-L3,随发行版提供)
                                         │               │
                                         └───── 第三方扩展(实现 trait + 注册名即可挂载)
```

**抽屉式的好处**:
- **核心零改动**:新增策略只需 `impl SchedulePolicy for MyPolicy` + 在 `StrategyRegistry::register("name", Arc::new(...))`,核心调度机制不动。
- **配置热切换**:`schedule_policy` 在 YAML 中改一行 + SIGHUP reload,即可切换活跃策略。
- **运行时共存**:同一进程可注册多个策略,不同 deployment 用不同策略(如核心业务用 L3、长尾用 L1)。
- **共享上下文**:所有策略共享同一 `ScheduleCtx`,新策略无需自己重新收集 inflight/queue/token_ids 等数据。

**典型扩展场景**:
- **LLM-LA**(Load Aware):自定义负载感知,综合 GPU 显存、队列深度、batch 大小等指标
- **GeoRouting**:按客户端地理就近选择后端
- **CostFirst**:优先选最便宜的后端(适合非生产流量)
- **ShadowTraffic**:把流量复制到灰度后端(不影响主路径)

### 2.6 调度策略选择决策树

```
                   部署场景?
                      │
       ┌──────────────┼──────────────┐
       │              │              │
   单 worker      多 worker       多 worker
                  等价            vLLM + ZMQ
       │              │              │
       ▼              ▼              ▼
      L0 RR         L0 RR          L3 KvcAware
     (默认)        (默认)         ★ 杀手锏 ★

                          或
                          │
                          ▼
                  想要后端 KV 复用:
                  ┌─────────────┴─────────────┐
                  │                           │
              非 vLLM 后端                vLLM 但未配 ZMQ
                  │                           │
                  ▼                           ▼
              L2 PromptPrefix 🟡           L1 KeyAffinity
              (生产验证,本地 hash)        (key 粘滞,粗粒度)
```

### 2.7 配置位置

调度策略在 `router_settings.schedule_policy` 中配置(YAML),热加载时切换会重建 Policy。

注意:Phase A 的 HybridRouter 是独立开关(`router_settings.hybrid_router`),与 Phase B 的 schedule_policy 正交,两者可同时启用(典型组合:HybridRouter 选 tier + L3 KvcAware 选 worker)。

---

### 2.8 流控背压与负载感知 (Backpressure & Load Sensing)

调度层 boom-sched 不是"选完就结束"——它会与 boom-flowcontrol、boom-kvindex、InFlightTracker、RebalanceMoveTracker 形成一个**闭环反馈系统**,动态感知后端吞吐性能并调节亲和性。

```
                后端实际吞吐性能变化
                  (TTFT、queue、KV 命中)
                          │
                          ▼
   ┌──────────────────────────────────────────────────────┐
   │  信号源(被各 Policy 共享读取,通过 ScheduleCtx):  │
   │                                                       │
   │   ① InFlightTracker                                   │
   │      ├─ get_model_input_chars(dep_id)                 │
   │      └─ get_inflight_count(dep_id)                    │
   │                                                       │
   │   ② FlowController.queue_info(dep_id)                 │
   │      └─ { vip_waiters, normal_waiters, max_inflight } │
   │                                                       │
   │   ③ kv_index.match_prefix(token_ids)  (L3 专用)      │
   │      └─ HashMap<worker_id, hit_ratio>                 │
   │                                                       │
   │   ④ DeploymentStore                                    │
   │      └─ auto_disabled 标记                            │
   │                                                       │
   │   ⑤ request_failure_counter                           │
   │      └─ 用于 auto-disable 触发阈值                    │
   └────────────────────────┬─────────────────────────────┘
                            │
                            ▼
   ┌──────────────────────────────────────────────────────┐
   │  Policy 内部消费信号:                                │
   │                                                       │
   │   L1 KeyAffinityPolicy:                              │
   │     if overloaded(sticky_dep, inflight + queue):     │
   │       new = lowest_load(candidates)                  │
   │       affinity[key] = new                             │
   │       RebalanceMoveTracker.record(key, old, new)    │
   │                                                       │
   │   L3 KvcAwarePolicy:                                  │
   │     combined[dep] = cache·hit + tier·rank + load·norm│
   │                       ▲                             │
   │                       └─ load·norm 来自 ①+②         │
   │     winner = argmax(combined)                        │
   │     即使 cache·hit 最高,load·norm 大也会拉低总分    │
   └────────────────────────┬─────────────────────────────┘
                            │
                            ▼
        流量自然从慢/忙后端转移到快/闲后端
        (KV 复用与负载均衡的动态折中)
                            │
                            ▼
              后端压力分布趋于均衡
                            │
                            ▼
              下一轮请求继续读信号 → 微调
              (稳态闭环,无需外部介入)
```

**关键实现细节**:

- **L1 的 rebalance 不是手动的**:`KeyAffinityPolicy::select_with_context` 内部检测到 sticky dep 当前的 `InFlightTracker` 计数或 `FlowController.queue_info` 超过内部阈值时,自动迁移;`RebalanceMoveTracker` 只负责"记录"以便 Dashboard 观察,不参与决策。Dashboard `stats/rebalance-moves` API 直接读这个 tracker。

- **L3 的 load·norm 是天然背压**:`KvcAwarePolicy` 的综合分公式里 `load_weight * load_norm[dep]` 这一项是关键。即使某 worker KV 命中率最高,只要它 inflight 满 / queue 堆积,`combined_score` 会被拉低,流量自然流向次优但空闲的 worker。这避免了"所有请求都追逐同一个 KV 命中高的 worker 把它打爆"的反模式。

- **ScheduleCtx 是信号总线**:所有策略共享同一份 `ScheduleCtx`,新策略无需自己重新采集 inflight / queue / token_ids。新增背压维度时,只需在 `ScheduleCtx` 加字段 + 在生成 ctx 处采集 + 在策略内消费。

- **auto_disable 是兜底**:即便调度策略没及时反应(比如某后端突然整体故障),`health_monitor` + `request_failure_counter` 会在累计失败达阈值后直接把 deployment 标记 `auto_disabled=true`,下一轮 `DeploymentStore.get_providers` 直接过滤掉,完全旁路该后端。

- **reload 时的退化语义**:kv_index 任何 kvc_aware 配置变化都会重建空 Trie(详见 §7.1),期间 L3 暂时退化到 lowest-load,这是"重建即清缓存"语义的副作用;InFlightTracker 和 FlowController 不受影响,L1 背压信号持续可用。

---

### 2.9 Roadmap: Agent Affinity 维度 (L4,规划中)

> 状态:🔄 规划中,尚未实现。本节描述目标设计与未决决策点,作为 boom-sched 未来核心竞争力方向。

#### 2.9.1 设计目标

针对**特定 agent 客户端**(Claude Code / Cursor / Cline / Continue / Aider 等)做亲和优化。机理:
- 同类 agent 的 prompt 高度重叠(system / tools / 累积对话)
- 集中调度到专用节点 → KV prefix 命中率提升 → 跳过 prefill → 降 TTFT + 省 GPU
- 隔离避免高频 prefix 被其他流量 LRU 淘汰

与 L3 KvcAware 的关键差异:L3 是**被动追命中率**,所有 worker 看成统一池;Agent Affinity 是**主动按客户端类型隔离池**,在 L3 之上再加一层防护。

#### 2.9.2 架构:正交前置过滤(非新 Policy)

**关键决策:不做"新 Policy",而做前置候选过滤层**。这样不破坏 §2.5 抽屉语义,Agent Affinity 是抽屉之外的**正交 WHERE 维度**(L0-L3 是 ORDER BY)。

```
   Phase B 入口(candidates = DeploymentStore.get_providers(model))
        │
        ▼
   ┌──────────────────────────────────────────────┐
   │  AgentPoolFilter (前置,规划中)               │
   │                                               │
   │  agent = AgentProfile::from(req.headers,      │
   │                              req.path,        │
   │                              req.system)      │
   │  if agent_stats.ewma_ratio(agent)             │
   │       > activate_threshold                    │
   │     && agent_stats.qps(agent) > min_qps:      │
   │    pool = candidates.filter(|d|               │
   │             d.agent_pool == agent)            │
   │    if pool 非空 && !pool_saturated:           │
   │      candidates = pool                        │
   │    else:                                      │
   │      candidates 不变 (溢出到通用池,软隔离)  │
   │  else:                                        │
   │    candidates 不变                            │
   └──────────────────────────────────────────────┘
        │
        ▼
   Policy.select_with_context(candidates, ctx)
   (L0/L1/L2/L3 在过滤后的候选里选)
```

典型组合:**AgentPool(过滤) + L1 KeyAffinity(粗粒度粘滞) + L3 KvcAware(细粒度 KV 命中)**,三层正交。

#### 2.9.3 未决决策点

| 决策点 | 候选方案 | 倾向 |
|--------|---------|------|
| 节点池隔离强度 | 硬隔离(exclusive) vs 软隔离+溢出 | **软隔离**,抗流量抖动 |
| "prefill 节点"语义 | KV 富集 vs PD 分离 | 先做 KV 富集(不需后端改造),PD 分离作为后续 |
| 阈值定义 | 瞬时比例 vs EWMA + hysteresis | **EWMA + hysteresis**,避免震荡 |
| 激活下限 | 仅比例 vs 比例+绝对 QPS | **比例 + 绝对 QPS**,避免小流量误激活 |
| 客户端识别信号 | 仅 path vs UA+anthropic-beta+system prompt | **多信号融合** + confidence scoring |
| 撤销阈值 | 单阈值 vs 双阈值 | **双阈值**(激活 30% / 撤销 20%)避免边界震荡 |
| 池划分方式 | deployment 静态标签 vs 动态分组 | **静态标签** `agent_pool: Option<String>`,运维显式声明 |

#### 2.9.4 需要的代码改动

- **boom-ctxaware**(工作量最大):
  - `ClientKind` → `AgentProfile`(多维:UA / anthropic-beta / system prompt 关键词)
  - `AgentStatsTracker` 加 EWMA ratio + hysteresis 状态机
  - 新增 `AgentProfile::from_headers_and_path()`,带 confidence scoring
- **boom-routing**:
  - `Deployment` schema 加 `agent_pool: Option<String>` 字段(DDL 改 `boom_model_deployment`)
  - `AgentPoolFilter` 作为 Phase B 前置过滤层
  - `ScheduleCtx` 加 `agent_profile: Option<AgentProfile>` 字段
- **boom-config**:YAML 暴露 `agent_pools` 配置块 + 阈值参数(`activate_ratio` / `deactivate_ratio` / `min_qps` / `ewma_alpha`)
- **boom-dashboard**:
  - Agent 占比趋势图(已有,需扩为多 agent 类型而非 Anthropic vs Other 二值)
  - 池视图(每个 agent_pool 的 deployment 列表 + 实时负载)
  - 阈值配置 UI(管理员调激活/撤销阈值)
- **boom-main**:`routes.rs` 在 Phase A 后、Phase B 前注入 `AgentPoolFilter` 调用

#### 2.9.5 反模式提醒(设计时已排除的坑)

- ❌ **按 key 做亲和**——L1 KeyAffinity 已经在做,Agent 维度与之正交而非替代
- ❌ **瞬时阈值**——必然震荡,必须用 EWMA + hysteresis
- ❌ **硬隔离无溢出**——流量峰值翻车,生产事故来源
- ❌ **单信号识别**——UA 易伪造,需多信号 + confidence
- ❌ **做成独立 Policy 而非前置过滤**——破坏 §2.5 抽屉正交性,且撤销时不优雅
- ❌ **网关侧臆测 PD 分离**——prefill/decode 分离需后端配合,网关只能"建议",做硬隔离会出错

#### 2.9.6 与 §3.5 流控背压的协同

AgentPoolFilter 的池饱和判定会复用 §3.5 描述的负载信号(InFlightTracker + FlowController queue)。即:
- 池内任意 deployment 的 inflight 达到 `max_inflight` → 视为池饱和 → 触发溢出
- 池整体 queue 长度过高 → 触发溢出
- 池内所有 deployment `auto_disabled` → 触发溢出

这样 Agent Affinity 不是"无脑专用",而是在保证可用性的前提下尽力隔离,与现有流控背压机制无缝衔接。

## 3. 同步流经路径(分层)

下图展示**主请求线程**里各模块的调用次序,实线箭头是同步调用,虚线是状态读取:

```
┌────────────────────────────────────────────────────────────────────────┐
│                                                                        │
│   axum::serve (Tokio worker)                                           │
│        │                                                               │
│        ▼                                                               │
│   ┌──────────────────────────────────────────────────────────┐         │
│   │ tower middleware: request_count.count.fetch_add(1)       │         │
│   │ (仅 /v1/ /admin/ /chat/ /completions /models)            │         │
│   └──────────────────────────────────────────────────────────┘         │
│        │                                                               │
│        ▼                                                               │
│   routes::chat_completions / messages / completions                    │
│        │                                                               │
│        │  ┌──────────────────────────────────────────────────────┐    │
│        │  │ RequiredAuth extractor → boom-auth::authenticate      │    │
│        │  │   ├─ master_key?  constant-time compare              │    │
│        │  │   └─ sk-?  SHA-256 → moka cache → DB lookup          │    │
│        │  │       └─ blocked / expires / budget 检查             │    │
│        │  │       └─ key.models → team.models → "*"  解析        │    │
│        │  └──────────────────────────────────────────────────────┘    │
│        │                                                               │
│        ▼                                                               │
│   check_model_access(identity, model, router, public_models)           │
│        │                                                               │
│        │   ┌───[读]──▶ AliasStore.resolve                              │
│        │   ┌───[读]──▶ DeploymentStore.contains                        │
│        │   ┌───[读]──▶ HybridRouter.is_virtual_model                   │
│        │                                                               │
│        ▼                                                               │
│   check_plan_limits(...)  ────────────────────────────────────┐         │
│        │   ┌───[读]──▶ PlanStore.resolve_plan (key)            │         │
│        │   ├─                  .resolve_team_plan (team_id)    │         │
│        │   ├─                  .get_default_plan               │         │
│        │   │                                                    │         │
│        │   │   Team 维度 (外箱):                                │         │
│        │   │     PlanStore.try_acquire_team ─▶ ConcurrencyGuard│         │
│        │   │     SlidingWindowLimiter.peek_only (windows)      │         │
│        │   │     SlidingWindowLimiter.peek_cumulative          │         │
│        │   │       (total_token / total_cost)                  │         │
│        │   │                                                    │         │
│        │   │   Key 维度 (内箱):                                 │         │
│        │   │     PlanStore.try_acquire ─▶ ConcurrencyGuard     │         │
│        │   │     SlidingWindowLimiter.peek_only                │         │
│        │   │     SlidingWindowLimiter.peek_cumulative          │         │
│        │   │                                                    │         │
│        │   └─▶ 返回 PlanCharge {                                │         │
│        │           concurrency_guard: Option<>,                │         │
│        │           team_concurrency_guard: Option<>,           │         │
│        │           cost_rate: DeploymentStore.get_cost_rate,   │         │
│        │           committed: false,  ← 关键:此时未计费       │         │
│        │       }                                                │         │
│        │                                                       │         │
│        │   ⚠️ 若 peek 失败(超额)→ 直接 429,不进入下方调度│         │
│        ▼                                                       │         │
│   Router.resolve_request_model(model, messages, tools)         │         │
│        │   ┌───[读]──▶ HybridRouter.classify (可选)            │         │
│        │   └─ fallback → Router.resolve_model_name             │         │
│        │                                                       │         │
│        ▼                                                       │         │
│   tokenizer_pool.load() ─▶ TokenizerPool.tokenize_openai      │         │
│        │   ┌─[读]──▶ (boom-kvindex::TokenizerPool)             │         │
│        │   └─ 返回 token_ids (用于 KV 前缀匹配)                │         │
│        │                                                       │         │
│        ▼                                                       │         │
│   Router.select_provider_with_prefix(model, key, chars, ids)  │         │
│        │   ┌───[读]──▶ DeploymentStore.get_providers            │         │
│        │   ├─                  .get_providers (别名目标)       │         │
│        │   ├─                  .get_providers("*") (兜底)       │         │
│        │   │                                                    │         │
│        │   └─▶ Policy.select_with_context(candidates, ids)     │         │
│        │       (抽屉式可插拔,详见 §2):                       │         │
│        │       ├─ L0 RoundRobinPolicy                          │         │
│        │       ├─ L1 KeyAffinityPolicy                         │         │
│        │       │   ├─ InFlightTracker.get_model_input_chars    │         │
│        │       │   ├─ FlowController.total_load (queue_info)   │         │
│        │       │   ├─ affinity DashMap lookup                  │         │
│        │       │   └─ RebalanceMoveTracker.record (再平衡时)   │         │
│        │       ├─ L2 PromptPrefixPolicy (生产验证,本地 hash) │         │
│        │       └─ L3 KvcAwarePolicy                           │         │
│        │           ├─ kv_index.match_prefix(token_ids)         │         │
│        │           ├─ InFlightTracker (load fallback)          │         │
│        │           └─ 返回 Selection { kv_hit_ratio }          │         │
│        │                                                       │         │
│        ▼                                                       │         │
│   need_full_kv_report(kv_hit_ratio, threshold)                 │         │
│        │   └─ 决定是否请求 vLLM 全量 KV 上报                   │         │
│        │                                                       │         │
│        ▼                                                       │         │
│   FlowController.acquire(deployment_id, input_chars, vip, ...)│         │
│        │   ├─ 排队等待 oneshot (VIP 队列优先)                  │         │
│        │   ├─ max_context 检查                                 │         │
│        │   └─▶ 返回 FlowControlGuard 或 Timeout/ContextExceeded│         │
│        │                                                       │         │
│        ▼                                                       │         │
│   provider.chat_stream(req) 或 provider.chat(req)              │         │
│        │   ┌─[HTTP]──▶ 上游 LLM                                │         │
│        │   ├─ 附加 X-Gateway-Priority (若启用)                 │         │
│        │   ├─ 附加 X-BooM-Client-Type (若 deployment 启用)     │         │
│        │   └─ 返回 ChatStream 或 ChatCompletionResponse        │         │
│        │                                                       │         │
│        ▼ (失败)                                                │         │
│   ┌───────────────────────────────────────────────────┐       │         │
│   │ health_monitor::record_request_failure             │       │         │
│   │   └─ request_failure_counter.fetch_add             │       │         │
│   │      (累计达阈值 → auto_disable deployment)        │       │         │
│   │                                                    │       │         │
│   │ PlanCharge 直接 Drop ─▶ 未 commit,不计费         │       │         │
│   │ log_error ─▶ boom-audit 异步写                    │       │         │
│   └───────────────────────────────────────────────────┘       │         │
│                                                                │         │
│        ▼ (成功)                                                │         │
│   PlanCharge.commit()                                          │         │
│        │   ├─ limiter.commit_counts (key 维度 counts+weight)  │         │
│        │   ├─ limiter.commit_counts (team 维度,如有)         │         │
│        │   ├─ health_monitor::reset_request_failure           │         │
│        │   ├─ agent_stats.record(api_path)                    │         │
│        │   ├─ request_rate.record(deployment_id)              │         │
│        │   └─ committed = true                                 │         │
│        │                                                       │         │
│        ▼ (非流式)                                              │         │
│   ┌──────────────────────────────────────────────────┐        │         │
│   │ plan_charge.settle(input, cached, output)        │        │         │
│   │   ├─ cost_rate.compute_cost_breakdown            │        │         │
│   │   ├─ limiter.settle_usage (key 维度 tokens/costs)│        │         │
│   │   ├─ limiter.settle_usage (team 维度,如有)      │        │         │
│   │   ├─ plan_charge.take_concurrency_guard().drop() │        │         │
│   │   ├─ log_request ─▶ boom-audit 写 boom_request_log│       │         │
│   │   ├─ agent_stats.record_tokens                    │        │         │
│   │   └─ prompt_log_writer.send (若启用)             │        │         │
│   │   return Json(response)                          │        │         │
│   └──────────────────────────────────────────────────┘        │         │
│                                                                │         │
│        ▼ (流式)                                                │         │
│   洋葱包装(详见下一节)                                       │         │
│                                                                │         │
└────────────────────────────────────────────────────────────────┘         │
                                                                        │
   客户端断连 / 流自然结束 → 内层 guard 逐层 Drop,见 §5                  │
                                                                        │
```

---

## 4. 流式响应的洋葱式包装(自内向外)

流式请求的响应流会被多层包装,每层负责一个"在流结束后做某事"的语义。**Drop 是核心机制**。

```
                  ┌────────────────────────────────┐
                  │  Provider 原始流               │
                  │  (reqwest::Response::bytes_stream) │
                  └────────────────┬───────────────┘
                                   │
                  ┌────────────────▼───────────────┐
                  │  Layer 1: sse_stream_from_*    │  在 mpsc::spawn task 里
                  │   抽 usage chunk ─▶ UsageTracker (Arc<Mutex>)│
                  │   flush tool_call arguments (缓冲重组)        │
                  │   /v1/messages 还包一层 AnthropicStreamTranscoder│
                  └────────────────┬───────────────┘
                                   │
                  ┌────────────────▼───────────────┐
                  │  Layer 2: InFlightStream       │  持有 InFlightGuard
                  │  Drop:InFlightTracker ─ count / chars│
                  │  用于调度策略查 load 和 dashboard│
                  └────────────────┬───────────────┘
                                   │
                  ┌────────────────▼───────────────┐
                  │  Layer 3: FlowControlledStream │  持有 FlowControlGuard
                  │  Drop:从 FlowController 队列移除│
                  │       触发 SlotInner::dispatch  │
                  │       ─▶ VIP / 普通 queue 里下个 waiter│
                  │       通过 oneshot 被 wake       │
                  └────────────────┬───────────────┘
                                   │
                  ┌────────────────▼───────────────┐
                  │  Layer 4: GuardedStream        │  持有 PlanCharge 的
                  │                                  │  ConcurrencyGuard
                  │  Drop:plan 并发槽 ─ key + team │
                  └────────────────┬───────────────┘
                                   │
                  ┌────────────────▼───────────────┐
                  │  Layer 5: LoggedStream          │  关键!延迟日志
                  │  Drop:                           │
                  │    ─ 记录真实 duration_ms       │
                  │    ─ 记录 ttft_ms (首字时间)    │
                  │    ─ UsageTracker 读取最终 usage│
                  │    ─ agent_stats.record_tokens   │
                  │    ─ log_request ─▶ boom-audit   │
                  │    ─ PlanCharge.settle(          │
                  │        input, cached, output)    │
                  │       └─ 加 tokens/costs 维度   │
                  │       └─ 加 cumulative counters │
                  └────────────────┬───────────────┘
                                   │
                  ┌────────────────▼───────────────┐
                  │  Layer 6: PromptLogStream       │  仅在 prompt_log 开启时
                  │  Drop:                          │
                  │    ─ 提取 SSE 内容拼接 response │
                  │    ─ send PromptLogEntry        │
                  │       给后台 writer task        │
                  │       (JSONL 文件,按 key/team)│
                  └────────────────┬───────────────┘
                                   │
                  ┌────────────────▼───────────────┐
                  │  Layer 7: sse_item_to_event     │  axum SSE 格式化
                  │                                  │
                  │  Sse::new(...).keep_alive()      │
                  └────────────────┬───────────────┘
                                   │
                                   ▼
                            HTTP/2 chunked
                            text/event-stream
                                   │
                                   ▼
                              OpenAI/Anthropic
                                  客户端
```

**Drop 次序**:外层先 Drop,内层后 Drop,但每个 guard 的副作用都是**幂等**的。客户端断连时 axum 会 cancel handler future,所有 wraps 一起 Drop,仍然写完整 log(只是 usage 可能拿不到 → tokens/costs 不计)。

---

## 5. 上下贯穿的横切模块(并发 + 后台)

上面 §1-§4 是单请求的同步路径。下面这些模块**在请求路径之外**持续运行,影响所有请求:

```
┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│                  AppState(全局共享,Clone + ArcSwap)                │
│                                                                     │
│   ┌─────────────────── 跨 reload 保留 (Arc 顶层) ────────────────┐ │
│   │                                                              │ │
│   │   • db_pool (max=30)         ─ 转发路径使用                  │ │
│   │   • dashboard_db_pool (max=3) ─ Dashboard 隔离,不饿死转发   │ │
│   │   • limiter: SlidingWindowLimiter                            │ │
│   │   • plan_store: PlanStore                                    │ │
│   │   • deployment_store / alias_store                           │ │
│   │   • router (内含 Policy + HybridRouter,各自 ArcSwap)         │ │
│   │   • inflight: InFlightTracker                                │ │
│   │   • flow_controller: FlowController                          │ │
│   │   • debug_store / prompt_log_writer                          │ │
│   │   • rebalance_move_tracker / request_rate / agent_stats      │ │
│   │   • request_failure_counter (auto-disable 用)                │ │
│   │   • deployment_health (auto-offline metric 用)               │ │
│   │   • kv_shutdown_tx (broadcast)                               │ │
│   │                                                              │ │
│   └──────────────────────────────────────────────────────────────┘ │
│                                                                     │
│   ┌─────────────────── Hot-swap (ArcSwap inner) ────────────────┐  │
│   │   inner: Arc<ArcSwap<AppStateInner>>                         │  │
│   │     ├─ config: Config                                        │  │
│   │     ├─ auth: Arc<dyn Authenticator>                          │  │
│   │     ├─ key_alias_lookup (Dashboard 用窄接口)                 │  │
│   │     └─ health: HealthStatus                                  │  │
│   └──────────────────────────────────────────────────────────────┘ │
│                                                                     │
│   ┌─────────────────── 第三种生命周期 (kv_index) ────────────────┐ │
│   │   kv_index: Arc<ArcSwap<Option<...>>>                        │ │
│   │   tokenizer_pool: Arc<ArcSwap<Option<...>>>                  │ │
│   │   kv_subscriber_handle: Arc<Mutex<Option<JoinHandle>>>       │ │
│   │                                                              │ │
│   │   ⚠️ 与上面两层不同:任何 kvc_aware 配置变化都重建空 trie     │ │
│   │      "重建即清缓存"是有意为之的语义                          │ │
│   │      瞬态查询降级到 lowest-load 直到 trie 重填                │ │
│   └──────────────────────────────────────────────────────────────┘ │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘

                        ┌────────────────────────────────────┐
                        │   后台任务 (Tokio spawn)           │
                        │                                    │
                        │   ① SIGHUP listener                │
                        │      └─▶ AppState::reload()        │
                        │                                    │
                        │   ② Sync task (每 10 min)          │
                        │      ├─ limiter.snapshot           │
                        │      ├─ limiter.sync_counters_to_db│
                        │      ├─ plan_store.snapshot_       │
                        │      │       assignments           │
                        │      ├─ plan_store.sync_assignments│
                        │      ├─ limiter.cleanup_expired    │
                        │      └─ plan_store.cleanup_        │
                        │                      concurrency    │
                        │                                    │
                        │   ③ Request summary (每 60s)       │
                        │      └─ 打印上一分钟请求总数        │
                        │                                    │
                        │   ④ Periodic FC dispatch (每 1s)   │
                        │      └─ flow_controller            │
                        │           .periodic_dispatch()     │
                        │         (防止空闲容量滞留)          │
                        │                                    │
                        │   ⑤ ZMQ KV event subscriber        │
                        │      (boom-kvindex::spawn_kv_      │
                        │       subscriber)                  │
                        │      ├─ tmq::subscribe *N endpoints│
                        │      ├─ select_all merge           │
                        │      ├─ 解析 msgpack 3-frame       │
                        │      └─▶ kv_index.insert_blocks    │
                        │                                    │
                        │   ⑥ Deployment health monitor      │
                        │      ├─ 定期探测 (DB deployments)   │
                        │      ├─ 失败计数 → auto_disable    │
                        │      └─ 恢复 → 清 auto_disable     │
                        │                                    │
                        │   ⑦ admin_command_handler          │
                        │      (mpsc::Receiver)              │
                        │      ├─ CreateModel                │
                        │      ├─ UpdateModel                │
                        │      ├─ DeleteModel                │
                        │      ├─ ConfigChanged (snapshot)   │
                        │      └─ ReloadConfig               │
                        │                                    │
                        │   ⑧ PromptLog writer task          │
                        │      (boom-promptlog::spawn)       │
                        │      ├─ mpsc::Receiver<Entry>      │
                        │      ├─ 按天滚动 JSONL 文件         │
                        │      └─ max_file_size_mb 切分      │
                        │                                    │
                        └────────────────────────────────────┘
```

---

## 6. 数据库表的所有权边界

每个模块只 DDL/CRUD 自己的表,**跨模块直写他人表禁止**。

```
┌────────────────── PostgreSQL ──────────────────────────────────┐
│                                                                │
│  litellm 兼容(只读):                                         │
│    LiteLLM_VerificationToken ◀── boom-auth (key 校验)          │
│    LiteLLM_TeamTable         ◀── boom-auth (team 模型解析)     │
│                                                                │
│  boom_* 自有(完整 CRUD):                                     │
│                                                                │
│    boom_request_log            ◀── boom-audit                  │
│      (id, request_id, key_hash, key_alias, team_id, model,     │
│       api_path, is_stream, status, tokens, duration, ttft,     │
│       deployment_id, client_ip, cached_tokens, created_at)     │
│                                                                │
│    boom_model_deployment       ◀── boom-routing                │
│      (id, model_name, litellm_model, api_key, api_base,        │
│       timeout, headers, deployment_id, quota_count_ratio,      │
│       max_inflight_queue_len, max_context_len, source,         │
│       auto_disabled, enabled, client_type_header, ...)         │
│                                                                │
│    boom_model_alias           ◀── boom-routing                 │
│      (alias_name, target_model, hidden, source, updated_at)    │
│                                                                │
│    boom_rate_limit_plan       ◀── boom-limiter                 │
│      (name, type, member_plan, concurrency_limit, rpm_limit,   │
│       tpm_limit, window_limits, total_token_limit,             │
│       total_cost_limit_micros, schedule, is_default, source)   │
│                                                                │
│    boom_key_plan_assignment   ◀── boom-limiter                 │
│      (key_hash, plan_name, assigned_at)                        │
│                                                                │
│    boom_team_plan_assignment  ◀── boom-limiter                 │
│      (team_id, plan_name, assigned_at)                         │
│                                                                │
│    boom_rate_limit_state       ◀── boom-limiter                │
│      (cache_key, count, window_start, window_secs, updated_at) │
│                                                                │
│    boom_cumulative_quota       ◀── boom-limiter                │
│      (scope, scope_id, kind, value_micros, updated_at)         │
│      (kind: TotalInputTokens/TotalOutputTokens/TotalCost)      │
│                                                                │
│    boom_team_table            ◀── boom-dashboard               │
│      (team_id, team_alias, ...)                                │
│                                                                │
│    boom_config                ◀── boom-dashboard               │
│      (key, value JSONB, updated_at)                            │
│      (存 db_seeded / prompt_log 配置 / 杂项 KV)                │
│                                                                │
└────────────────────────────────────────────────────────────────┘
```

跨模块写需要走 `AdminCommand` channel(架构原则见 CLAUDE.md §5)。例:Dashboard 创建模型 → 发 `AdminCommand::CreateModel` → boom-main 处理(因 boom-main 才依赖 boom-provider)。

---

## 7. 关键并发与状态语义

### 7.1 三种生命周期

```
                生命周期               重建时机
   ┌────────────────────────────────────────────────────────┐
   │  AppStateInner       │   每次 SIGHUP / reload          │
   │  (config + auth)     │   ArcSwap 原子交换              │
   ├────────────────────────────────────────────────────────┤
   │  Stores / Trackers   │   跨 reload 保留(顶层 Arc)     │
   │  (limiter / FC /     │   仅 YAML 模式 reload 时        │
   │   deployment /       │   rebuild 内容                  │
   │   alias / inflight)  │                                 │
   ├────────────────────────────────────────────────────────┤
   │  kv_index (Trie)     │   kvc_aware 配置变化时          │
   │                      │   重建空 trie (清缓存语义)      │
   └────────────────────────────────────────────────────────┘
```

### 7.2 流式 RAII 链(Drop 顺序)

```
客户端断连 / 流结束
        │
        ▼
   最外层 Stream Drop
        │
        ▼
   PromptLogStream::drop ─▶ send Entry to writer task (异步)
        │
        ▼
   LoggedStream::drop    ─▶ log_request() ─▶ boom-audit (异步 SQL)
                         ─▶ PlanCharge::settle()
        │
        ▼
   GuardedStream::drop   ─▶ ConcurrencyGuard::drop (key)
        │
        ▼
   FlowControlledStream::drop ─▶ FlowControlGuard::drop
                              ─▶ SlotInner::dispatch()
                              ─▶ VIP queue waiter 被 wake
        │
        ▼
   InFlightStream::drop  ─▶ InFlightGuard::drop (counter --)
        │
        ▼
   inner provider stream ─▶ reqwest connection close
```

### 7.3 未服务不计费(PlanCharge)

```
   check_plan_limits
        │
        ▼
   PlanCharge { committed: false }
        │
        ├──▶ 如果 provider.chat_stream() 返回 Err
        │     └──▶ PlanCharge::drop
        │           committed == false
        │           └─▶ 不计任何 quota,ConcurrencyGuard 自动释放
        │
        └──▶ provider.chat_stream() Ok
              │
              ▼
          PlanCharge::commit()
              │   └─▶ counts += weight (key + team)
              │   └─▶ committed = true
              │
              ▼
          (流进行中 / 响应结束)
              │
              ▼
          PlanCharge::settle(input, cached, output)
              │   ├─▶ cost = cost_rate.compute_cost_breakdown
              │   ├─▶ tokens += input/output (TPM 维度)
              │   ├─▶ costs += cost_micros
              │   └─▶ cumulative_total_token / total_cost +
              │
              ▼
          settled = true (防 double-settle)
```

`PlanCharge::drop` 时若 `committed && !settled`(流被取消、没拿到 usage chunk):
- counts 维度已加,自然过期
- tokens/costs/cumulative **不计** ─ 这是 "未拿到 usage 不计费" 的语义

---

## 8. 健康监测路径(独立于请求路径)

```
                 两条独立路径
                       │
        ┌──────────────┴───────────────┐
        │                              │
        ▼                              ▼
  请求驱动 (sync)               定时探测 (async)
        │                              │
        │ provider.chat() Err          │ health_monitor task
        ▼                              │ 每周期:
  record_request_failure               │   for each DB deployment:
        │                              │     ├─ HTTP probe
        │ request_failure_counter      │     │   成功 ─▶ clear
        │   .fetch_add(1)              │     │   失败 ─▶ ++ failure
        │                              │     │
        │ 累计 ≥ 阈值                   │     └─▶ 累计 ≥ threshold
        ▼                              ▼
  DeploymentStore                     DeploymentStore
   .set_auto_disabled(true)            .set_auto_disabled(...)
        │                              │
        └──────────────┬───────────────┘
                       ▼
              路由决策时跳过 auto_disabled=true
                       │
                       ▼
              包含 "*" deployment 也可被禁用
```

---

## 9. Anthropic 路径的额外步骤

`/v1/messages` 与 OpenAI 路径的差异:

```
   AnthropicMessagesRequest
        │
        ▼
   (可选)strip_claude_code_attribution
        ─▶ rewrite::strip_cc_attribution_anthropic
        ─▶ 清掉 Claude Code 注入的 system 块
        │
        ▼
   boom_core::anthropic::anthropic_request_to_openai
        ─▶ 转成 ChatCompletionRequest
        │
        ▼
   【走完整 OpenAI 路径 §1 step 1-8】
        │
        ▼
   provider.chat / chat_stream (OpenAI 格式)
        │
        ├──▶ 非流式:
        │     openai_response_to_anthropic(response)
        │     ─▶ 返回 Anthropic JSON
        │
        └──▶ 流式:
              AnthropicStreamTranscoder::transcode(chunk)
              ─▶ OpenAI chunk → Anthropic events
              ─▶ message_start / content_block_delta /
                 message_delta / message_stop

              raw_upstream_sink (可选,捕获原始 OpenAI chunks
                                  给 prompt_log)
```

---

## 10. 模块依赖矩阵(精简)

```
                  core  auth  cfg  prov  lim  route  audit  fc  kvix  ctxaw  plog  dash  main
   boom-core       -
   boom-auth       dep    -
   boom-config     dep         -
   boom-provider   dep              -
   boom-limiter    dep                   -
   boom-routing    dep    dep         dep    -
   boom-audit      dep                                  -
   boom-flowcontrol dep(queued info)                       -
   boom-kvindex    dep                                              -
   boom-ctxaware   dep                                                      -
   boom-promptlog  dep                                                            -
   boom-dashboard  dep              dep    dep         dep                                     -  ❌ 不依赖 prov/cfg
   boom-main       dep    dep   dep  dep  dep  dep    dep   dep  dep   dep   dep   dep    -
                                                                                              ▲
                                                                                              └─ AdminCommand
                                                                                                 channel
```

依赖原则:**单向、无环、boom-core 是唯一叶子、boom-main 是唯一根、boom-dashboard 不依赖 boom-provider/boom-config**(架构原则详见 CLAUDE.md)。
