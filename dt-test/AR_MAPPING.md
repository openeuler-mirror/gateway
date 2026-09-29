# DT 用例 ↔ AR 验收项映射

把 boom-dt 的 **508 个 DT 用例**（20 个测试文件）对应到 5 条 AR。规则：

- 每个用例按其验证的行为归入**一个主 AR**；横切用例在"关联"栏标注（计入主 AR 的数量，
  不重复计数）。总数 508 = 各 AR 之和。
- 用例 ID 前缀与文件一一对应（如 `DT-ANT-*` 都在 `tests/anthropic.rs`），
  函数名可用 ID 前缀在文件内检索；AR4/AR5（kvc 调度核心交付）给出全量函数名清单。

| AR | 用例数 | 主要文件 |
| --- | ---: | --- |
| AR1 推理链路可观测 | 76 | trace.rs / trace_otlp.rs / alert.rs / stressmon.rs / core.rs / routing.rs / ctxaware.rs |
| AR2 权限·配额计费·路由兜底·限流 | 79 | limiter.rs / flowcontrol.rs / routing.rs / core.rs / ml_service.rs |
| AR3 协议转换·team 层级·认证校验 | 280 | anthropic.rs / provider.rs / azure.rs / core.rs / fusion.rs / routing_fusion.rs / routing.rs / auth.rs / config.rs |
| AR4 用户级 kvc 亲和调度 | 19 | routing.rs（KeyAffinityPolicy + 负载/迁移原语） |
| AR5 前缀 kvc 缓存感知调度 | 30 | kvindex.rs / routing.rs（KvcAwarePolicy）/ routing_fusion.rs / config.rs |
| 公共支撑（不专属单一 AR） | 24 | smoke.rs / config.rs / harness.rs |

---

## AR1 推理链路可观测（审计日志 / 缓存命中率统计 / 详细 prompt 上下文记录 / 模型检测与自恢复）— 76 例

| 用例 | 数量 | 覆盖点 |
| --- | ---: | --- |
| `DT-TRC-01..21`（trace.rs） | 21 | 请求级 trace 链路：W3C traceparent 解析/采样传播、RequestSpan 属性与 LLM 请求/响应体记录、finalize ok/error、recent ring、TraceGuard RAII |
| `DT-OTLP-01..12`（trace_otlp.rs） | 12 | OTLP 导出：攒批/队列溢出/后台 flush；**失败重试→Offline→探测自恢复**（03/04/09）；protobuf 映射；ping 健康检查 |
| `DT-ALR-01..09`（alert.rs） | 9 | 告警状态机：raise/clear 幂等、history ring 有界、notifier 仅迁移触发 |
| `DT-STR-01..09`（stressmon.rs） | 9 | worker 压力监控：CPU 时序采样环形缓冲、80% 阈值累计 |
| `DT-CORE-03..06`（core.rs） | 4 | **审计日志记录判定**（should_log_to_db / dedup 白名单）；**部署故障判定** is_deployment_failure（检测输入） |
| `DT-CORE-31..36`（core.rs） | 6 | **prompt 上下文记录**：DebugStore 按 key FIFO 记录、启停/清空 |
| `DT-CORE-43..44`（core.rs） | 2 | **详细 prompt 上下文**：raw_capture 原始请求体 + 响应帧记录 |
| `DT-RT-36..42`（routing.rs） | 7 | 调用可观测：每模型/部署请求量、成败、时延 min/max、汇总发射窗口 |
| `DT-CTX-04..09`（ctxaware.rs） | 6 | anthropic/openai 流量占比统计（关联 AR3 的协议识别） |

关联（计入其他 AR）：`DT-PRV-OAI-21` raw_capture 端到端（AR3）、`DT-PRV-OAI-22`
kv_cache_report_full 命中率条件全量上报开关（AR3）、`DT-CORE-41` usage 的
cached_tokens 统计（AR2）、`DT-KV-16/17` trie 命中率排序与 TTL 老化（AR5）。

## AR2 模型权限和配额计费管理 / 路由兜底 / 滑动窗口限流等 — 79 例

| 用例 | 数量 | 覆盖点 |
| --- | ---: | --- |
| `DT-LM-01..18`（limiter.rs） | 18 | **滑动窗口限流**三段式（peek 权重感知/commit/settle，counts·tokens·costs 三维度）；窗口过期复位；累计配额 6 计数器与 reset；计划模板 schedule/stale；PlanStore key/team 分配三态与类型校验；并发守卫 RAII |
| `DT-FC-01..20`（flowcontrol.rs） | 20 | 并发限流：槽位创建/派发/超时出队、VIP 优先、guard Drop 释放、排队/in飞可观测（关联 AR4 负载口径） |
| `DT-RT-01..06`（routing.rs） | 6 | 别名存储（路由解析链） |
| `DT-RT-16..20, 26, 27`（routing.rs） | 7 | **模型权限**：quota_ratio、费率、可见性 Public/Private + team ACL（DB 行解析） |
| `DT-RT-22..25`（routing.rs） | 4 | **配额计费**：compute_cost、cached 折扣价、cached_tokens 截断 |
| `DT-RT-43..46, 72`（routing.rs） | 5 | tier 分级路由（成本感知分流 + 分类器挂载） |
| `DT-RT-64..66`（routing.rs） | 3 | **路由兜底**：精确→别名→`"*"` 通配级联；空组抑制 fallback |
| `DT-CORE-39..42`（core.rs） | 4 | ProviderCost/ProviderBilling 成本与 usage 累计（含 cached_tokens） |
| `DT-CORE-56..58`（core.rs） | 3 | WindowLimit 判空与紧凑/对象两种反序列化（限流配置容错） |
| `DT-ML-01..08`（ml_service.rs） | 8 | 外置 ML 分级服务：合法 tier 直用、非 2xx/坏 JSON/连接失败全回退矩阵 |
| `DT-CFG-11`（config.rs） | 1 | flow_control 排队超时配置默认/覆盖 |

关联：`DT-CORE-49` 预算超限判定（AR3 认证链路）、`DT-CORE-69` PlanType（AR3）。

## AR3 集群调度网关接入：openai/anthropic 等主流协议转换 / team 用户层级管理 / 认证和权限校验等 — 280 例

| 用例 | 数量 | 覆盖点 |
| --- | ---: | --- |
| `DT-ANT-01..40`（anthropic.rs） | 40 | **Anthropic↔OpenAI 协议转换**：system/消息块（Image/ToolResult/ToolUse/Thinking/Document）、tools、finish_reason 映射、流式转码器（message_start/delta/块切换/usage 释放） |
| `DT-PRV-SSE-01..13`（provider.rs） | 13 | SSE 分帧容错：LF/CRLF/CR、跨 push 分割、BOM、注释、原始帧捕获 |
| `DT-PRV-LIB-01..14`（provider.rs） | 14 | provider 工厂：类型/protocol、custom_headers 净化、reserved key、auto_detect、timeout 钳制（LIB-13 kv_worker_id 派生关联 AR4/5） |
| `DT-PRV-OAI-01..24`（provider.rs） | 24 | OpenAI 兼容 chat/stream：usage 透传、错误映射、gateway_headers、extra 透传、SSE 装配、raw_capture（21，关联 AR1）、kv_cache_report_full（22，关联 AR5） |
| `DT-PRV-ANT-01..07`（provider.rs） | 7 | Anthropic provider 行为：头注入（x-api-key/anthropic-version）、流式 |
| `DT-PRV-GEM-01..23`（provider.rs，16 例） | 16 | **Gemini 协议转换**：systemInstruction、functionCall/Response parts、generationConfig、tools |
| `DT-PRV-AZU-01..03` + `DT-PRV-BD-01..02`（provider.rs） | 5 | Azure / Bedrock 构造与协议 |
| `DT-AZ-01..10`（azure.rs） | 10 | Azure OpenAI：deployment URL 改写、api-version、api-key 头、流式 |
| `DT-CORE-01, 02, 07..09`（core.rs） | 5 | 错误→HTTP 状态码/type 映射（接入行为契约）、raw_upstream_body |
| `DT-CORE-10..26`（core.rs） | 17 | 协议归一原语：消息角色交替、tool_choice 五型转换、image source 三型 |
| `DT-CORE-27..30`（core.rs） | 4 | api_base host 解析（kv_worker_id 基础，关联 AR4/5） |
| `DT-CORE-37, 38, 45..49`（core.rs） | 7 | **认证和权限校验**：key 前缀格式、网关 header 硬阻断、can_call_model/is_expired/is_budget_exceeded |
| `DT-CORE-50..55, 59..64, 70`（core.rs） | 12 | 消息/usage/响应 serde 容错（兼容各家属格式） |
| `DT-CORE-65, 66`（core.rs） | 2 | StorageTier 集群分层存储优先级 |
| `DT-CORE-67, 68`（core.rs） | 2 | OtlpConfig 默认与 serde（关联 AR1） |
| `DT-RT-07..15, 21`（routing.rs） | 10 | **集群调度接入**：DeploymentStore 多 provider 轮询、独占模型、增量增删 |
| `DT-RT-67..71`（routing.rs） | 5 | Router 选择委托、策略热替换、可见模型列表 |
| `DT-AU-01..05`（auth.rs） | 5 | **认证**：整键 SHA-256（litellm 兼容）、主密钥常量时间比较、无库拒绝、check_model_access |
| `DT-FUS-01..40`（fusion.rs，39 例） | 39 | **team/多模型编排**：工作流 panel+aggregator、重试、回退、流式聚合、usage/成本归账、prompt 模板 |
| `DT-RF-01..19, 21`（routing_fusion.rs） | 20 | fusion 虚拟模型端到端接入：独占候选集、冲突校验、非流式/流式全链路、队列与上下文限流联动、网关头构造（RF-20 归 AR5） |
| `DT-CFG-01..05`（config.rs） | 5 | provider 前缀拆分与别名配置 |
| `DT-CFG-26..37`（config.rs） | 12 | workflow（fusion）配置校验全矩阵 |
| `DT-CTX-01..03`（ctxaware.rs） | 3 | 协议路径识别（anthropic 流量判定，关联 AR1 统计） |

**team 用户层级管理**的证据分散在：`DT-RT-19/26/27`（team ACL 模型访问）、
`DT-LM-15`（team plan 分配/默认 team plan）、`DT-LM-03`（team 累计配额 scope）、
`DT-FUS-*`（多模型编排）——auth 的 team 解析本体（lookup_team/
resolve_team_models）在 DB 路径上，见下"缺口"。

## AR4 推理集群 kvc 亲和调度器：用户级请求前缀 kvc 亲和调度 — 19 例

用户（key_hash）维度的亲和：同用户请求粘到同一 worker，含 warm-up 与过载迁移。

| 函数名（routing.rs） | 用例 | 覆盖点 |
| --- | --- | --- |
| `strategy_registry_register_lookup` | DT-RT-47 | 策略注册表（调度策略框架） |
| `round_robin_policy` | DT-RT-48 | RR 基线（无亲和时的行为） |
| `shuffle_policy_distribution` | DT-RT-49 | Shuffle 基线 |
| `key_affinity_empty_and_single` | DT-RT-50 | KeyAffinityPolicy 空候选/单候选 |
| `key_affinity_no_key_falls_back_to_lowest_load` | DT-RT-51 | 无 key_hash → 最低负载 |
| `key_affinity_initial_assignment_and_hit` | DT-RT-52 | 首次最低负载 + 记录亲和，同 key 再来命中 |
| `key_affinity_warmup_below_threshold` | DT-RT-53 | warm-up：上下文不足阈值不走亲和 |
| `key_affinity_rebalance_migrates` | DT-RT-54 | 亲和 worker 过载 → 迁移最低负载 |
| `inflight_new_empty` .. `inflight_multiple_guards_accumulate` | DT-RT-28..31 | 模型/部署级在飞计数（负载信号，RAII 增减） |
| `rebalance_counter_record_snapshot` .. `rebalance_move_tracker_same_id` | DT-RT-32..35 | 迁移计数与 move 追踪（亲和迁移可观测） |
| `load_helpers_should_rebalance` / `load_helpers_deployment_load` / `load_helpers_min_load_candidate` | DT-RT-61..63 | 共享负载原语：迁移判定、部署负载口径、最低负载候选（与 AR5 共用） |

关联：`DT-KV-09` record_request_prefix（用户请求前缀入库，AR5）、
`DT-FC-17` DeploymentQueueInfo total_load（AR2）。

## AR5 推理集群 kvc 亲和调度器：请求前缀 kvc 缓存感知和预测的调度策略 — 30 例

请求前缀（system+tools+messages 按 512B 块哈希）维度的缓存感知调度：trie 自学习、
命中率评分、降级回退。

| 函数名 | 用例 | 覆盖点 |
| --- | --- | --- |
| `single_root_block_match` | DT-KV-01 | 单块前缀命中（kvindex.rs） |
| `chained_prefix_partial_match` | DT-KV-02 | 链式前缀部分命中深度 |
| `store_batch_equivalent_to_stores` | DT-KV-03 | 批量存块等价性 |
| `find_matches_unknown_model_returns_empty` | DT-KV-04 | 未知模型空结果 |
| `find_matches_excludes_non_matching_worker` | DT-KV-05 | 前缀不匹配 worker 排除 |
| `remove_worker_clears_claims` | DT-KV-06 | worker 下线清 claim |
| `evict_blocks_removes_specific_hash` | DT-KV-07 | 按哈希精确驱逐 |
| `lru_evicts_oldest_when_full` | DT-KV-08 | LRU 容量驱逐（over-approximation 老化） |
| `record_request_prefix_stores_blocks` | DT-KV-09 | 自学习：请求前缀直接入库（不经事件） |
| `prefix_block_count_chunks_by_block_size` | DT-KV-10 | 前缀按块大小切分计数 |
| `find_matches_empty_inputs_return_empty` | DT-KV-11 | 空输入边界 |
| `empty_block_bytes_store_is_ignored` | DT-KV-12 | 空块忽略 |
| `node_count_tracks_trie_nodes` | DT-KV-13 | trie 节点统计 |
| `model_names_lists_registered_models` | DT-KV-14 | 注册模型列表 |
| `block_capacity_returns_configured_max` | DT-KV-15 | 容量配置 |
| `multiple_workers_sorted_by_score` | DT-KV-16 | 多 worker 命中按 combined_score（命中率）降序 |
| `prune_expired_removes_old_blocks` | DT-KV-17 | TTL prune 过期块 |
| `debug_dump_returns_populated_nodes` | DT-KV-18 | trie 调试转储 |
| `kvc_aware_empty_and_single` | DT-RT-55 | KvcAwarePolicy 空候选/单候选（routing.rs） |
| `kvc_aware_empty_prefix_degraded` | DT-RT-56 | 空前缀 → 最低负载（degraded） |
| `kvc_aware_no_kv_worker_id_falls_back` | DT-RT-57 | 候选无 kv_worker_id → 最低负载 |
| `kvc_aware_cold_round_robin` | DT-RT-58 | 冷启动 trie 空 → 全 0 分 round-robin |
| `kvc_aware_affinity_hit` | DT-RT-59 | trie 命中 → 亲和派发到命中 worker（hit_ratio>0） |
| `kvc_aware_rebalance_when_winner_overloaded` | DT-RT-60 | winner 过载超阈值 → 迁移 |
| `fusion_records_request_prefix_into_kv_index`（routing_fusion.rs） | DT-RF-20 | 端到端：路由后前缀写入 trie → 同前缀请求命中 |
| `kvc_aware_valid_passes` / `kvc_aware_invalid_ttl_rejected` / `kvc_aware_zero_max_blocks_rejected` / `kvc_aware_zero_ttl_is_valid` / `load_config_rejects_invalid_rebalance_threshold`（config.rs） | DT-CFG-07..10, 15 | kvc_aware 配置校验（TTL/max_blocks/rebalance_threshold 边界） |

关联：`DT-PRV-OAI-22` kv_cache_report_full（命中率驱动条件全量上报，AR3）、
`DT-CORE-27..30` kv_worker_id 从 api_base 派生（AR3）、`DT-RT-61..63` 共享负载原语（AR4）。

## 公共支撑 — 24 例

不验证单一 AR 的业务行为，是全部 AR 的地基：

- `DT-SMOKE-01..03`（smoke.rs，3 例）：配置解析 / provider 构造 / 路由存储冒烟。
- `DT-CFG-06, 16, 12..14, 17..25`（config.rs，14 例）：环境变量展开、配置加载/
  缺失文件、原 YAML 读取、原子写、嵌套路径设置、密钥识别与递归掩码、hooks 空态。
- `DT-HAR-01..07`（harness.rs，7 例）：DT harness 自身（free_port / chat_request /
  simple_chat_request / MockUpstream 起停与 remount / TestServer 服务与 Drop 清理），
  保证测试基础设施本身被验证。

## 覆盖缺口（验收时需知）

1. **审计日志（AR1）**：boom-audit（boom_request_log 落库）本体无 DT —— 现有证据是
   记录判定原语（`DT-CORE-03..05`）与 trace 链路；落库 SQL 属 `*_db` 跳过范围。
2. **详细 prompt 上下文记录（AR1）**：boom-promptlog 落盘本体无 DT —— 现有证据是
   raw_capture（`DT-CORE-43/44`、`DT-PRV-OAI-21`）与 DebugStore（`DT-CORE-31..36`）。
3. **模型检测与自恢复（AR1）**：部署健康检查/熔断无独立 DT —— 现有证据是故障判定
   （`DT-CORE-06`）、告警状态机（`DT-ALR-*`）、OTLP 导出器自恢复（`DT-OTLP-03/04/09`）。
4. **team 用户层级管理（AR3）**：auth 的 team 解析（lookup_team/
   resolve_team_models、blocked/expired/budget 校验）在 DB 路径上无 Postgres 不可达，
   见 README"DB 路径约定"。
