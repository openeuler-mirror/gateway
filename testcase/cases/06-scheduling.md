# 06 调度策略（Scheduling）

覆盖：三种路由策略（round_robin / key_affinity / kvc_aware）的选路行为、粘性与迁移、参数生效、策略切换。

通用判定锚点（各用例共用）：
- 请求日志（Dashboard 日志页或 `boom_request_log` 表）中每条请求记录 `schedule_policy` 及 kvc 相关字段（`kv_hit_blocks` / `kv_input_blocks` / `trie_blocks` / `trie_max_blocks`）。
- 同名多 deployment 的模型：各 deployment 指向可区分的 mock 上游（响应带标记 `node-a` / `node-b` / `node-c`），便于肉眼判定每次请求落在哪个节点。

| ID | 标题 | 角色 | 深度 |
|----|------|------|------|
| SCH-01 | 调度策略配置生效 | admin | smoke |
| SCH-02 | round_robin 多 deployment 均匀摊开 | admin | smoke+trial |
| SCH-03 | key_affinity 同 key 粘同节点 | admin | smoke+trial |
| SCH-04 | key_affinity 不同 key 分散 | admin | trial |
| SCH-05 | key_affinity 过载迁移（rebalance 兜底） | admin | full |
| SCH-06 | key_affinity 节点下线自动切换 | admin | trial |
| SCH-07 | kvc 冷启动请求摊开 | admin | smoke+trial |
| SCH-08 | kvc 相同前缀学习粘性 | admin | smoke+trial |
| SCH-09 | kvc 不同前缀互不干扰 | admin | trial |
| SCH-10 | kvc TTL 过期后回归摊开 | admin | full |
| SCH-11 | kvc max_blocks 容量上限生效 | admin | full |
| SCH-12 | kvc 单 deployment 短路 | admin | trial |
| SCH-13 | kvc 过载迁移后学习跟随新节点 | admin | full |
| SCH-14 | 策略热切换重建学习状态 | admin | trial |
| SCH-15 | reload 不清除运行时计数器 | admin | trial |

---

## SCH-01 调度策略配置生效

- **角色**：admin　**深度**：smoke
- **前置**：某模型配置 ≥2 个 deployment；可修改 `router_settings.schedule_policy` 并热加载（SIGHUP 或面板）。
- **步骤**：
  1. 依次将 `schedule_policy` 设为 `round_robin` / `key_affinity` / `kvc_aware`，每次热加载后发起 2~3 次调用
  2. 查看请求日志的 `schedule_policy` 字段
- **预期**：每次切换后请求全部 200，日志中 `schedule_policy` 字段与配置一致（kvc_aware 显示为 `kvc`），无路由失败。

## SCH-02 round_robin 多 deployment 均匀摊开

- **角色**：admin　**深度**：smoke+trial
- **前置**：`schedule_policy: round_robin`；某模型 3 个 deployment（标记 node-a/b/c）。
- **步骤**：串行发起 12 次非流式调用，记录响应标记序列。
- **预期**：12 次请求按 a→b→c→a… 严格轮询（或接近均匀的交替），无连续长时间压在同一节点；面板 Stats 中三个 deployment 计数大致相等（各 4±1 次）。

## SCH-03 key_affinity 同 key 粘同节点

- **角色**：admin　**深度**：smoke+trial
- **前置**：`schedule_policy: key_affinity`；某模型 3 个 deployment；已建 key K1。
- **步骤**：用 K1 串行发起 ≥10 次调用（内容可不同），记录响应标记。
- **预期**：全部请求落在**同一个** deployment（如全部 node-a）；日志 `deployment_id` 一致。换个时间窗重试仍粘同节点（粘性跨请求持久，不随会话结束丢失）。

## SCH-04 key_affinity 不同 key 分散

- **角色**：admin　**深度**：trial
- **前置**：同 SCH-03；另建 key K2、K3。
- **步骤**：K1/K2/K3 各发起 5 次调用，分别记录标记。
- **预期**：每个 key 内部粘同一节点；不同 key 之间分布于不同节点（3 key × 3 节点场景下理想为各占一个），整体摊开、无单点聚集。

## SCH-05 key_affinity 过载迁移（rebalance 兜底）

- **角色**：admin　**深度**：full
- **前置**：`schedule_policy: key_affinity`；K1 粘在 node-a；记录当前 `rebalance_threshold`（默认 20）；node-a 配置较小 `max_inflight`。
- **步骤**：
  1. 用其他 key 向 node-a 灌入并发长请求，将其在途数压过 `rebalance_threshold`
  2. 此时再用 K1 发起调用
  3. 释放压力后再次用 K1 调用
- **预期**：
  1. 高压期间 K1 的请求落到**其他**节点（过载迁移生效），不排队等待也不报错
  2. 压力释放后 K1 重新粘回（或粘到新节点后保持稳定）——总之收敛到单一节点，不反复震荡
  3. 全程无 5xx

## SCH-06 key_affinity 节点下线自动切换

- **角色**：admin　**深度**：trial
- **前置**：同 SCH-03，K1 粘在 node-a。
- **步骤**：
  1. 停掉 node-a 的 mock 上游（或面板禁用该 deployment）
  2. 用 K1 立即发起调用
- **预期**：请求落到存活节点并正常 200，不随 node-a 一起失败；面板中 node-a 标记为不健康/禁用。

## SCH-07 kvc 冷启动请求摊开

- **角色**：admin　**深度**：smoke+trial
- **前置**：`schedule_policy: kvc_aware`；**刚切换到该策略或重启网关**（trie 为空）；某模型 3 个 deployment。
- **步骤**：用**相同前缀**（固定 system prompt + tools）串行发起 9 次调用（用户内容可变）。
- **预期**：首轮请求后 trie 开始学习；学习完成前的请求被 round-robin 摊开（a/b/c 交替），全部 200；日志 `kv_hit_blocks` 从 0 开始逐渐上升。

## SCH-08 kvc 相同前缀学习粘性

- **角色**：admin　**深度**：smoke+trial
- **前置**：同 SCH-07，且已完成首轮学习（SCH-07 执行过后即可）。
- **步骤**：
  1. 用与 SCH-07 完全相同的 system+tools 前缀再发起 5 次调用（messages 用户内容不同）
  2. 记录响应标记与日志 `kv_hit_blocks` / `kv_input_blocks`
- **预期**：
  1. 5 次请求全部落在**同一** deployment，且与首轮学习记录的节点一致
  2. `kv_hit_blocks` 接近 `kv_input_blocks`（共享前缀段全部命中，仅新增消息部分未命中）
  3. 多轮对话场景下逐轮 `kv_hit_blocks` 递增（历史消息进入共享前缀）

## SCH-09 kvc 不同前缀互不干扰

- **角色**：admin　**深度**：trial
- **前置**：`schedule_policy: kvc_aware`；构造两套差异明显的固定前缀 P1（如英文 system+工具集 A）、P2（如中文 system+工具集 B）。
- **步骤**：P1、P2 交替各发起 5 次调用，记录标记。
- **预期**：P1 的 5 次粘同一节点，P2 的 5 次粘同一节点；两者**可以**不同节点（各自独立学习），互不串扰、互不挤占对方的粘性。

## SCH-10 kvc TTL 过期后回归摊开

- **角色**：admin　**深度**：full
- **前置**：`kvc_aware.router_ttl_secs` 临时调小（如 60s）；已完成 SCH-08 的学习。
- **步骤**：
  1. 记录当前日志 `trie_blocks`（应 > 0）
  2. 停止发送请求，静置超过 `router_ttl_secs`（如 90s）
  3. 用相同前缀再发起 6 次调用
- **预期**：
  1. 静置后首轮调用的 `kv_hit_blocks` 回到 0（前缀记录已被 TTL 老化），请求重新被 round-robin 摊开
  2. 后续调用重新学习并粘到新记录的节点
  3. 全程 `trie_blocks` 表现出先回落、随新学习再上升的生命周期

## SCH-11 kvc max_blocks 容量上限生效

- **角色**：admin　**深度**：full
- **前置**：`kvc_aware.max_blocks` 临时调小（如 1000）；压测工具可用。
- **步骤**：
  1. 用批量不同前缀的请求灌入（如 bench-client 以变化 system prompt 打流量）
  2. 持续观察日志 / 面板中的 `trie_blocks` 与 `trie_max_blocks`
- **预期**：`trie_blocks` 增长到 `trie_max_blocks` 附近后不再无界上涨（LRU 逐出最旧记录，允许小幅超出后收敛）；网关内存随之上界稳定，不随流量持续增长。

## SCH-12 kvc 单 deployment 短路

- **角色**：admin　**深度**：trial
- **前置**：`schedule_policy: kvc_aware`；某模型**只配 1 个** deployment。
- **步骤**：对该模型发起多次调用（相同与不同前缀各若干）。
- **预期**：全部 200，无选路异常；日志中该模型请求的 `schedule_policy` 走默认路由标识（单候选不进 kvc 决策），`kv_hit_blocks` 为空——单 deployment 无路由决策可言，属预期短路而非缺陷。

## SCH-13 kvc 过载迁移后学习跟随新节点

- **角色**：admin　**深度**：full
- **前置**：`schedule_policy: kvc_aware`；前缀 P 已学习粘在 node-a；node-a `max_inflight` 调小。
- **步骤**：
  1. 将 node-a 压过载（并发长请求灌入）
  2. 用前缀 P 发起调用（触发 rebalance 迁移到 node-b）
  3. 释放压力，再用前缀 P 发起 3 次调用
- **预期**：
  1. 过载期间 P 的请求落到其他节点且 200
  2. 释放后 P 的请求**粘到迁移后的节点**（node-b）而非立即跳回 node-a——学习记录跟随实际路由
  3. 不出现同前缀在两节点间反复横跳

## SCH-14 策略热切换重建学习状态

- **角色**：admin　**深度**：trial
- **前置**：当前 `schedule_policy: kvc_aware` 且已有学习记录（`trie_blocks` > 0）。
- **步骤**：
  1. 热加载切换为 `key_affinity`，发起调用观察
  2. 再热加载切回 `kvc_aware`，用原前缀发起调用
- **预期**：
  1. 切换后调用正常 200；kvc 相关字段不再出现在新请求日志；key_affinity 粘性从零开始建立
  2. 切回后 trie 从空重新学习（首轮 `kv_hit_blocks` = 0），不残留旧策略的学习数据
  3. 两次切换均无需重启进程

## SCH-15 reload 不清除运行时计数器

- **角色**：admin　**深度**：trial
- **前置**：任意调度策略；某 key 已产生限流窗口计数与若干请求计数。
- **步骤**：
  1. 记录面板该 key 的已用额度 / 请求计数
  2. 执行热加载（SIGHUP 或 `POST /admin/config/reload`）
  3. 再次查看计数并发起调用
- **预期**：计数与热加载前连续（不清零、不翻倍）；调用路由行为不变（粘性/学习状态按各策略自身的切换规则处理，与 reload 无关）。
