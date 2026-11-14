# 上下文回落（Context Rollback）调度设计

> 状态：定稿（2026-11-11 评审通过）
> 背景：同名模型（如 GLM-5.3）多 vLLM 实例混池，上下文能力不同（200K / 1M）。
> 演进：方案 A（估算过滤，见文末决策记录 #1）→ **本方案：被动回落 + 学习**。

---

## 1. 问题与实测证据

现状链路：候选集按 model_name 分组，无能力维度 → 超长请求落到 200K 实例 → 上游 400 → 网关映射 **502** 返回 agent。

实测日志（GLM 栈，/v1/messages，stream）：

```
status_code: 502, duration_ms: 4107
Upstream error (400): {"error":{"code":"InputTokenExceeded",
  "message":"This model's maximum input length is 204799 tokens.
  However, you requested 210625 tokens. ..."}}
```

三个问题：
1. >200K 请求必然撞 200K 实例（无能力维度）；
2. 400 类错误被映射成 502，agent 视为瞬时错误**重试**，再撞一次失败往返；
3. 失败往返是**秒级**成本（上游 tokenize 21 万 token 花 4.1s 才拒绝）。

## 2. 方案总览

**被动回落 + 学习**：正常路由完全不动（全池公平 + rr/key亲和/kvc + rebalance）；仅在真实上下文错误发生时识别 → 在更大能力层的在线实例中按当前策略单次回落；回落成功后记忆，后续直达。

分层：

```
L0  正常路由（不动）：全池公平 + rr / key亲和 / kvc + rebalance
L1  上下文错误识别        boom-core   （code/message 模式表 + 数值解析）
L2  资源池划分            boom-routing（max_context 分层 + 组激活门）
L3  回落执行              boom-main   （ContextRollbackOrchestrator，对标 kvc.rs 模式）
L4  记忆跟随              无新增机制（复用各策略既有迁移路径，§7）
```

关键性质：
- **正确性下限**：只要更大层在线，集群可服务的请求绝不会打回客户端（估算方案调错参数时做不到）；
- **正常流量零开销**：绝大多数 <200K 请求路径上没有任何新增计算；
- **配置极简**：不需要估算参数（bytes_per_token / margin），只需能力声明。

## 3. 配置面

### 3.1 部署级：编辑弹窗「Rollback 配置」独立分区

- 字段：`max_context_tokens: Option<u64>`（NULL = 不参与）。**编辑界面以 K 为单位**（十进制：1K = 1000 tokens，1M = 1000K = 1,000,000；输入 × 1000 落库，DB 始终存 tokens。声明值仅用于层间相对比较，与上游实现的实际上限无关）；
- 独立分区（非基础配置内），为后续条件预留扩展位（跨模型回落目标、其他能力维度）；
- **列表页不展示字段值**，只在编辑弹窗配置；池头保留聚合的层分布摘要；
- 存储为 DB 业务配置（五点同步）：
  1. boom-config `ProviderParams` 字段
  2. boom-config manifest 注册（home: Db）
  3. boom-routing `DEPLOYMENT_CORE_COLUMNS` + `DeploymentInput` + 5 条 SQL
  4. Dashboard app.js 表单 + i18n
  5. boom-main migration：`ALTER TABLE boom_model_deployment ADD COLUMN max_context_tokens BIGINT NULL`

### 3.2 进程级：YAML-only（无 Dashboard 卡片）

识别模式是上游栈的协议属性，不是业务数据——**不占 Dashboard 界面**，仅 config.yaml 可覆盖（内置默认覆盖 GLM / vLLM / Anthropic 实证措辞）：

```yaml
router_settings:
  rollback:
    context:                 # 上下文长度回落（当前唯一策略）
      patterns:              # 识别模式表（默认内置，可追加）
        - code: "InputTokenExceeded"
        - "maximum input length"
        - "maximum context length"
        - "prompt is too long"
```

（历史：曾设计 Config 卡片承载总开关 + 模型勾选名单，2026-11-12 重设计移除——两级 opt-in 造成"勾了模型仍显示关闭"的困惑；参与状态全部收敛到模型部署页，见决策记录 #12。）

### 3.3 模型部署页：rollback 列 + 主开关（唯一操作入口）

```
[✓ rollback]                              ← 工具栏三态主开关：
┌─────────┬──────────┬─────┬───┐             全开=✓ / 混合=− / 全关=□，
│ 模型     │ rollback │ 别名 │ … │             点击即全开/全关（confirm）
├─────────┼──────────┼─────┼───┤
│ glm-5.3 │          │  -  │ … │ ← 每行自己的格子（跟随行底色）
│ glm-5.3 │  [✓]●激活 │  -  │ … │ ← 勾选框在池的中间行，下带徽标
│ glm-5.3 │          │  -  │ … │
├─────────┼──────────┼─────┼───┤ ← 池间黑框分隔
│ qwen3   │   [ ]    │  -  │ … │
└─────────┴──────────┴─────┴───┘
```

- **rollback 列（模型列之后，每池一个勾选框）**：**每行都有自己的 rollback 单元格**（跟随行底色，斑马纹逐行保持——不用 rowspan，一个跨行格子只能有一种底色，必然与条纹冲突，headless Chrome 复现已验证）；勾选框渲染在**池的中间行**（徽标统一在勾选框下方）：奇数池垂直居中，**偶数池底边对齐该行下边界**（= 池的视觉中线，徽标文字落在中线上）；勾选写组内全部 deployment 行（`POST /admin/models/rollback {model_name, enabled}` → AdminCommand → 批量 UPDATE + reload 路由）。API 另支持 `{deployment_id}` 行级粒度（YAML/seed 写入的行状态以池勾选框的聚合呈现：全开才勾选）；
- **工具栏三态主开关**：checked=全开、indeterminate=混合、unchecked=全关，点击翻转全局（`{enabled}` 无 id/model_name → 全表 UPDATE，RETURNING 收集受影响组逐组 reload；取消 confirm 时重渲染还原视觉态）；
- 编辑弹窗「Rollback 配置」只有最大上下文（K 单位）；`rollback_enabled` 不在弹窗编辑（池勾选框是编辑面），更新 body 回显当前值防误清；
- 池视图单表（所有池列宽一致），池分组由 `pool-start` 行的黑框上边框表达。

徽标（只在池勾选时显示；未勾选 = 池未参与）：

| 池勾选 | 池配置 | 徽标 |
|---|---|---|
| 未勾 | — | 无徽标 |
| 勾选 | 全配 + 多层 | ● 激活 |
| 勾选 | 全配 + 单层 | ○ 无回落目标（tooltip：需存在更大层） |
| 勾选 | 混配（有行缺 max_context_tokens） | ⚠ 未激活 · 列出缺配数（tooltip 说明激活条件） |

## 4. L1 错误识别

- **code 优先**：上游错误体 JSON 的 `error.code` 精确匹配（`InputTokenExceeded`），比 message 文本稳；
- **message 模式兜底**：模式表可配置（GLM / vLLM / Anthropic 三套措辞已实证）；
- **数值解析**：`requested (\d+) tokens` 与 `maximum (?:input|context) length is (\d+)` → 得到需求值与实例真实上限，用于目标层选择与配置校准提示；
- **保守原则**：未匹配的 400 一律透传不重试（误判一次非上下文 400 = 在 1M 上再失败一次）；**重试严格单次**（200K→1M；1M 再报上下文错直返，不循环）；
- **502→400 透传修正**：rollback 启用且兜不住时，返回上游原始 400 语义（保留 message），修复 agent 把 400 类问题当 5xx 重试的现有缺陷。

## 5. L2 资源池划分

- 分层：`groupby(model_name, max_context_tokens)`，同值同层，层内即现有行为；
- **参与条件**：池开关（组内全部行 `rollback_enabled`，模型部署页切换）&& **组激活门（all-or-nothing）**——组内全部实例已配 `max_context_tokens`；混配组 inert + 告警（最坏情况 = 今天的行为，正确性永不劣化）；
- 回落目标层：解析出 requested 值 → 选 **max_context ≥ requested 的最小层**；解析失败 → 最大层（保单次成功）；
- 组内只有一个层：条件永不成立，配置无害；
- 回落的第二次 select 带目标层候选集，记忆跟随由各策略既有迁移路径完成（§7），**无候选收缩等新增机制**。

## 6. L3 回落执行

挂点：routes.rs provider 调用的 Err 分支（三个入口 /v1/chat、/v1/messages、/v1/responses 共用）。

| 衔接点 | 处理 |
|---|---|
| 流控 | 释放旧 deployment 的 fc_guard 槽位，对新 deployment 重新 `acquire_fc_guard` |
| 计费 | plan_charge 在 provider 成功后才 commit（routes.rs:975）→ 回落先于 commit，**天然不双计费** |
| 审计/DFX | 记一行：deployment_id 记**最终**实例 + `rollback` 标记；schedule_policy 标签追加 `+rollback` |
| trace | span 增加 rollback 事件（from/to deployment、识别依据、耗时） |
| 学习 | 回落成功后调用 policy.on_outcome 纠正亲和记忆（key_affinity 幂等重绑；kvc 由 Ok 记录覆盖，§7） |
| 流式安全 | 上下文错误发生在上游 admission（任何 SSE 字节发出之前）→ 对客户端完全透明 |

时序：

```
请求（公平选中 200K 实例 A1）
  ├─ A1 返回 400（上下文错，4.1s 实测）
  ├─ L1 识别命中？──否──→ 原错误路径（401/403 计自动下线；其他透传）
  │       ↓ 是
  ├─ L2 池开启 && 组激活 && 更大层在线？──否──→ 原始 400 直返 agent（+回落失败计数/告警）
  │       ↓ 是
  ├─ L3 换持流控 → 策略选择目标层候选 → rr/亲和/kvc + rebalance
  │       ├─ 成功 → 响应客户端；Ok 记录前缀→1M worker + on_outcome 纠正绑定（下轮直达）
  │       └─ 又上下文错(>1M) → 直返 agent，不二次重试
```

**fusion 确认项**：若失败模型是 fusion 虚拟模型，错误从内层真实 deployment 冒出——rollback 取的 deployment_id 必须是内层；实现前确认 fusion.rs 的错误传播路径。

## 7. 记忆跟随：复用现有机制，无新增（2026-11-11 二次修订：取消独立降级记忆）

**实测三策略 rebalance 后的记忆行为**（源码结论）：

| 策略 | rebalance 后行为 | 记忆语义 |
|---|---|---|
| rr | 无 rebalance（round_robin.rs:40 纯 `counter % len`，无负载感知） | 无记忆 |
| key_affinity | `rebind(key→新实例)`（key_affinity.rs:254-257）：旧实例绑定计数 −1、新 +1、TTL 重置、move_tracker 记 (from,to) | **真迁移**：绑定搬家 |
| kvc_aware | 编排层记录用的是 **post-rebalance** 的 provider（kvc.rs:187）；trie `apply_prepared` 对同前缀多 worker **add-only 并存**（token_prefix.rs:800 Phase B `workers.insert` 只加不删），旧侧记录靠 LRU/TTL 老化（无流量刷新才老化） | **归因跟随实际路由 + 老化**，非迁移（源码注释："aligned with dynamo"） |

**rebalance 语义澄清（2026-11-11 补）：分发时转向（dispatch steering），非队列迁移。** rebalance 代码块在 `select_with_context()` 内部（kvc_aware.rs:368-397 / key_affinity.rs:246-269），动作 = 把**当前正在路由的请求**的 winner 改判给 least-loaded；负载数字（in-flight + 队列深度）只是决策输入（load_helpers.rs 读），不是操作对象。已排队/在飞的请求一律不动（在飞请求物理上不可迁移：生成状态在 GPU、KV cache 在实例本地；排队迁移收益低于重新路由的复杂度）。所谓"迁移"只发生在记忆层：key_affinity rebind 迁移的是绑定、kvc 记录跟随的是 trie 归因——都是"后续请求的默认去向"。负载拉平的时间常数 ≈ 平均请求时长（新流量被引开 + 存量自然排空）。rollback 同性质：第二次 select 改本请求去向，不搬运任何已有工作。

kvc rebalance 的收敛机制（关键澄清：**rebalance 不做任何使 B1 命中率"超过"A1 的操作**——add-only 做不到，只会并列）：

```
轮 N:    trie: prefix→A1(1.0)；打分 A1=1.0 胜出；rebalance 改判 → 最终 B1
         记录 prefix→B1（归因跟随，kvc.rs:187）
轮 N+1:  trie: A1(1.0) + B1(1.0) 并列 → tie 轮询；无论选中谁，
         负载差仍在 → per-request rebalance 再接管 → 最终 B1（kvc_aware.rs:368-397）
轮 N+TTL: A1 记录无刷新 → TTL/LRU 老化归零 → B1=1.0 独占，不再依赖 rebalance
```

即：**并列期间靠 per-request 接管维持，独占靠对侧老化**。若负载差在 TTL 内拉平，tie 轮询把流量分回 A1 并刷新 → 长期共存摊开——kvc rebalance 语义是"负载兜底"而非"缓存迁移"（affinity 让位于负载，aligned with dynamo）。

**rollback 场景的胜出路径不同，不经过 tie**：失败轮 A1 在 Err 路径零记录（PR2）→ 回落成功记录 B1 → 下轮全池打分 B1=1.0 vs 200K 侧全冷 0 → **直接胜出**。不对称由"失败不记录"在第一次失败时创造，不依赖 rebalance/老化。无 PR2 时退化为：A1 在路由时被记录 → 下轮 A1/B1 并列 tie → 轮询约一半回撞。

**rollback 复用同一路径——零新增记忆机制**：rollback 的第二次 select 带 1M 层候选集，各策略沿既有迁移逻辑完成记忆跟随：

- **key_affinity**：旧绑定 A1 不在候选集 → fall-through 重绑 key→B1（key_affinity.rs:273-287 既有路径）→ 下轮直达。与 rebalance 迁移同构；**但仅限回落层 ≥2 实例**（见下方同款陷阱）；
- **kvc_aware**：第二次 select 在 1M 候选上全冷 → 轮询选中 B1；**PR2（Ok 后记录）**保证 A1 在 Err 路径从未记录、B1 获得记录 → 下轮全池打分 B1 hit=1.0 vs 200K 全冷 → B1 胜出，不再回撞；
- **rr**：无记忆可迁移——**残余缺口**：长对话每轮约 N(200K)/N(总) 概率先撞 200K 再回落（2×200K+1×1M ≈ 2/3 轮付 4s）。v1 接受（无状态是 rr 的语义）；实测痛再议候选收缩（独立特性，不影响本方案）。

**key_affinity 的同款陷阱与修法（2026-11-11 补）**：绑定同样在 select 时（结果未知时）写入——且回落层只有 1 个实例时，第二次 select 走 `len==1` 短路（key_affinity.rs:199-201）**不发生重绑**，绑定恒指向 A1 → 下轮 **100% 回撞**（比 trie 的并列 ~50% 更严重；层 ≥2 时 fall-through 重绑可自愈）。不能照搬 PR2 把绑定写入整体搬到 Ok 后：select 时写绑定同时承担**并发摊开**职责（bindings 计数器防止并发冷启动磁吸同一实例，key_affinity.rs:149-158 anti-magnet 语义），搬时机会破坏它。修法 = **结果回调**：SchedulePolicy 加默认空实现 `on_outcome(key, model, deployment, success)`，key_affinity 实现为幂等 rebind；回落成功后由执行链调用，把绑定纠正到最终实例（层=1 补上短路漏掉的重绑，层≥2 幂等无害）。常规成功路径不调用，保持现状语义。

**残余交互：rollback 后再触发 rebalance 的完整流程**（诚实记录；v1 接受）：

关键结构点：**rollback 自己的第二次 select 是层内封闭的**（候选只有 1M 层），rebalance 在其中只能层内均衡（多个 1M 实例间改判），**结构上压不回 200K**——这是需求"回落时接入 rr/亲和/kvc+rebalance"的落点。跨层压回只发生在**后续轮次的全池正常路由**。

（"层"由候选列表定义，rebalance 机制本身无层概念：第一次 select 的候选 = `Router.resolve_candidates` 按 model_name 分组的全池 [A1,A2,B1]——不分能力，这正是残余交互的根源；第二次 = L2 按 max_context 构造的能力层 [B1,B2]。同一份 rebalance 代码、同一把负载尺子（deployment_load 按各实例 max_inflight 归一化），只是输入列表不同。另：单候选时策略短路（kvc_aware.rs:185-195），rebalance 不运行——层内改判仅在层内 ≥2 实例时发生。）

```
轮 N（已回落过，trie: prefix→B1）：
  全池路由（候选 A1, A2, B1）：打分 B1=1.0 胜出
  rebalance: B1负载80 − A1负载5 > 20% → 改判 A1   ← rebalance 不懂能力维度
  → A1 返回 400（+4s；Err 不记录，PR2 保住记忆不被污染）
  → rollback：第二次 select 候选=[B1]（层内封闭）→ B1 成功 → 刷新记录
  客户端无感，但该轮付一次 +4s 失败往返
轮 N+1: B1 仍高 → 重复；B1 回落（<A1+20%）→ rebalance 不触发 → B1 直达
```

性质：正确性始终无损（rollback 兜得住）；trie 亲和信号始终正确（A1 恒 0 分），被覆盖的只是 rebalance 改判环节。三种策略同样暴露（kvc 胜出被改判 / key_affinity 绑定被迁移 / rr 轮询命中）。触发条件：1M 负载持续高于最闲 200K + threshold。若实测频繁：① 候选收缩回归（rollback 过的前缀收缩候选到 ≥1M 层——被删机制回来的充分理由）；② 调高 rebalance_threshold（治标）。

**唯一前提：PR2（trie 记录时机改 provider Ok）**。没有它，kvc_aware 把失败前缀记在 200K 侧（kvc.rs Step 5 现为路由时记录），下轮 A1/B1 并列 tie → 轮询回撞。

记录时机论证：上下文错误是 HTTP 400，必然走 `chat()`/`chat_stream()` 的 Err 分支，**Ok 返回点即"装得下"的确证点**——非流式 Ok 后记录，流式 Ok 后记录（HTTP 200 已到、prefill 已开始，无需等 SSE 消费完）。LoggedStream 的 Drop 服务的是审计的真实 duration，与 trie 记录无关。Ok 点记录同时修复并列陷阱（失败不学习 → 200K 侧无记录 → 无 tie）。

## 8. 与现有机制的交互确认

| 机制 | 结论 |
|---|---|
| 自动下线（request_failure） | `is_deployment_failure` 仅计 ProviderError/UpstreamParseError/401/403（error.rs:178）——上下文 400 **不计数**，不会误下线 200K 实例 ✓ |
| 健康监控探活 | 不变；更大层全下线 → L2 条件不成立 → 400 直返 |
| rebalance | 在（可能被收缩的）候选集内原样运行，无跨层问题 |
| 热加载 | 总开关走 YAML reload；部署字段 CRUD 直改内存重建 Provider |
| plan_charge / 限流 | 回落先于 commit，不双计费；限流按最终 deployment 记账 |

## 9. 边界矩阵

| 场景 | 行为 |
|---|---|
| 200K 报上下文错，1M 层在线 | 回落，客户端无感 |
| 更大层全下线/禁用 | 原始 400 直返 agent + 计数/告警 |
| 1M 上再报上下文错（>1M） | 直返，不二次重试 |
| 组内混配 | 组 inert + 告警 = 今天的行为 |
| 非上下文 400 | 透传（不重试） |
| 401/403 | 原路径（计自动下线，不回落） |
| 回落后的下一轮（key_affinity/kvc_aware） | 沿既有迁移路径直达 1M，零失败往返（§7） |
| 单实例模型 | 上下文错透传（无处回落） |

## 10. 观测性

- 计数器（AppState 顶层，对标 KvcSchedulerStats）：`rollback_success / rollback_no_target / identify_miss`（另观测 rr 残余回撞率）；
- 池头徽标（配置态）+ 回落率（运行态，建议按池告警：回落失败率高 = 1M 池不可用或容量不足）；
- 审计行 rollback 标记 + trace span 事件；
- 配置校准提示：识别时解析到的实例真实上限（如 204799）与声明值偏差大时，池头提示"按实测校准？"（只建议不自动写）。

## 11. 落地清单（分期）

> PR = 独立合入单元（Pull Request）编号，按依赖序排列。PR2 指"trie 记录时机"这一代码改动，与 per-request rebalance（每请求的负载改判运行时机制）是两回事。

| PR | 内容 | 风险 |
|---|---|---|
| PR1 配置面 | migration + 五点同步 + 编辑弹窗 Rollback 分区 + 池视图 + Config「回落策略」卡片 | 无行为变化 |
| PR2 记忆时机统一 | trie 记录改 provider Ok 后（修并列陷阱，§7）+ SchedulePolicy 加 on_outcome 结果回调 + key_affinity 幂等重绑实现（修绑定陷阱：层=1 时 len==1 短路不重绑 → 100% 回撞，§7） | 低，独立可回滚 |
| PR3 识别器 | boom-core 模式表 + 数值解析 + 单测 | 纯新增 |
| PR4 回落执行链 | orchestrator + 流控换持 + 审计/trace + 502→400 修正 + 回落成功后 on_outcome 回调（§7） | 核心变更 |
| PR5 观测 | 计数器 + 池徽标运行态 + 告警 | 纯新增 |

集成测试要点：fake 上游 A(200K) 返回 400 → 断言回落 B(1M)；同 key 第二次请求**直接**路由 B（既有迁移路径：key_affinity 重绑 / kvc_aware trie 命中）；1M 池空时 400 直返；非上下文 400 不重试；401/403 不触发回落。

## 12. 决策记录（Decision Log）

1. **估算过滤（方案 A）→ 放弃**：估算需调参（bytes_per_token/margin）、边界误判会把可服务请求 400 给客户端；rollback 用上游权威信号，正确性下限更强，代价是首次错误多一跳。
2. **共享池 = 公平调度，无层偏好**（用户裁决）：要有保留就分池（方案 B）；后选定 rollback 方案，正常路由维持公平。
3. **组激活 all-or-nothing**：混配组回落目标不可预测，最坏情况 = 现状；徽标 + 告警引导补配。
4. **重试严格单次**：防循环；>1M 直返。
5. **配置归属**：能力声明（max_context_tokens）= 部署级 DB（编辑弹窗独立 Rollback 分区，列表页不展示值）；识别模式与总开关 = 进程级 YAML（Config 页卡片）。
6. **不新建降级记忆，复用策略既有迁移路径**（2026-11-11 二次修订）：实测 rebalance 后 key_affinity rebind（真迁移）、kvc_aware add-only + 归因跟随 + 老化；rollback 第二次 select 天然触发同一路径（key_affinity fall-through 重绑 / kvc_aware Ok 记录后全池打分胜出）。唯一前提 PR2；rr 无记忆可迁移为已知残余缺口。
7. **trie 记录时机 = provider Ok（admission 成功），不用 Drop**（2026-11-11 修订）：上下文错误必走 Err 分支，Ok 即"装得下"确证；LoggedStream Drop 是审计机制，服务的是真实 duration，与 trie 记录无关。同时修复并列陷阱（失败不学习 → 无 tie 轮询）。
8. **回落失败透传 400**：修 UpstreamError→502 映射导致 agent 误重试的现有缺陷。
9. **回落框架通用命名**（2026-11-11 补）：Config 卡片与 YAML 节用通用名 `rollback`（「回落策略」），上下文为首个策略子节 `context`，未来策略平级扩展；总开关 `rollback.enabled` 管整个框架，单策略独立开关留作扩展点。
10. **亲和记忆统一"失败不学习"，key_affinity 用 on_outcome 回调**（2026-11-11 补）：key_affinity 绑定在 select 时写入，回落层单实例时 len==1 短路导致绑定恒指失败实例（100% 回撞，比 trie 并列 ~50% 更严重）；因 select 时写绑定承担并发摊开（anti-magnet）职责，不能整体搬时机——改为结果回调：回落成功后 rebind 到最终实例。kvc 侧由 PR2 的 Ok 记录统一覆盖正常/rebalance/rollback 三条路径。
11. **K 单位 UI**（2026-11-11）：部署编辑的最大上下文输入以 K 为单位（1K = 1024 tokens，1M = 1024K），× 1024 落库，DB 始终存 tokens；池视图池头不展示层分布明细（徽标保留激活态）。
12. **参与状态全部收敛到模型部署页，删除 Config 卡片**（2026-11-12 重设计，取代 #11 的名单方案）：`boom_model_deployment.rollback_enabled`（DB，每部署行）+ 池分隔行开关（批量写组内行）+ 工具栏全局全选/全部去除（替代总开关，一次写全表并逐组 reload）。理由：两级 opt-in（YAML 总开关 × YAML 名单）造成"勾了模型仍显示关闭"的不可推理状态；"哪些模型参与回落"是业务配置，归 DB / 模型部署页本就符合 §7 权威拆分。YAML 仅保留 `rollback.context.patterns`（识别模式，协议属性）。编辑弹窗不暴露单行 rollback_enabled（避免组内混合态），更新回显防误清。
