# e2e 多实例测试框架 — AI 介入操作手册

- **定位**：多实例机制黑盒测试框架的独立工作区。设计定案见 `docs/e2e-multi-instance-design.md`（用例矩阵、环境组、确定性策略均以它为准，本文不重复）。
- **状态**：本文档是框架的操作契约。runner 命令未实现时，按设计文档 §10 落地顺序实现，并保证命令接口与本文一致。

## 红线（介入必守）

1. **不修改 `crates/` 功能代码**——测试发现 bug 时走"报告证据 + 台账登记用例"，修复归产品侧 PR，与转 `automated` 同步。
2. **纯黑盒**：只通过六个观察面操作网关（设计文档 §2）。禁止链接或调用 boom-* 内部 crate API 来做验证。
3. 等待禁止固定 sleep，一律 `poll_until`（超时 = 3×interval + 余量）。
4. 台账（`cases/`）是唯一登记处：新用例先入台账再写实现；ID 一次性，删除即退役、永不复用。
5. `status: specified` 用例跑红是预期（已知未修复 bug 证据），不要为转绿而放宽断言；发现"红得不对"（bug 已修仍红 / 剧本失真）才修改用例。

## 环境前提

- 独占 Linux 容器，包管理器可装任意包。所有子进程（postgres / 网关实例 / mock）由 runner 管理生命周期，结束自动回收。
- 首次进入容器先跑 `doctor` 自检，按提示补缺：
  - Debian/Ubuntu：`apt-get install -y postgresql postgresql-client`（需要 initdb/psql 可执行）
  - rust 工具链缺失则装 rustup；网关二进制由 runner 编译（workspace 根 `cargo build`）
- runner 使用随机端口段 + 独立临时数据目录（`runs/<run_id>/`），互不冲突；异常中断后用 `clean` 清残留。

## 常用命令

| 命令 | 作用 |
|---|---|
| `cargo run -p e2e-runner -- doctor` | 环境自检：依赖、编译、端口可用性 |
| `cargo run -p e2e-runner -- run --suite smoke` | 冒烟套件 MI-001/002/004/005（~8min） |
| `cargo run -p e2e-runner -- run --suite nightly` | 全量（~25min） |
| `cargo run -p e2e-runner -- run --case MI-002` | 单用例复跑（环境组按需起） |
| `cargo run -p e2e-runner -- bind-check` | 台账 ↔ 实现绑定双向校验 |
| `cargo run -p e2e-runner -- report <run_id>` | 查看 / 重放某次报告 |
| `cargo run -p e2e-runner -- clean` | 杀残留子进程、删临时目录 |

## 排障 playbook

| 症状 | 先查什么 | 常见原因与处置 |
|---|---|---|
| doctor 失败 | 报错信息 | 包未装；rust 缺失；`initdb` 因 locale 报错（临时实例统一 `--no-locale`） |
| 实例探活超时 | `tail runs/<run_id>/logs/gw-<x>.log` | 迁移未完成（等首次 boot 慢）；DB 未起；端口冲突（重跑即可，runner 随机端口） |
| 用例 poll 超时挂起 | 实例日志有无 sync cycle 痕迹；手查水位行：`psql $DSN -c 'SELECT * FROM boom_quota_delta ORDER BY seq DESC LIMIT 5'`（DSN 在 `runs/<run_id>/env`） | 网关 config 的 interval ≠30s；postmaster 被上次 SIGSTOP 卡住（`clean` 后重跑） |
| 用例失败 | 先读报告 divergence → 单用例复跑 ×3 | 稳定失败 = 按证据链定位（水位行 → 表值 → 两实例日志时间线）；偶发 = flaky，按窗口边界竞态排查（请求是否恰在代次切换点发出） |
| 广播类用例超时 | B 实例日志有无 revision watcher 痕迹 | DB 不可达期间 watcher 静默失败（MI-012 正在验证此 bug）；恢复后应 ≤5s 追平 |
| 端口/进程残留 | `ps aux | grep boom-gateway` | 上次运行异常中断 → `clean` |

诊断失败用例的取证顺序固定：**报告 divergence → `boom_quota_delta` 水位行 → `boom_rate_limit_cumulative/state` 表值 → 双实例日志时间线对齐**。取证的 SQL 与日志路径由 tools crate 提供动词，不要手拼。

## 用例生命周期

1. 台账：`cases/<domain>/MI-xxx.md`，frontmatter + 目的/前置/步骤/预期四段（≤15 行，模板见设计文档 §7）
2. 实现：`crates/e2e-impl/` 测试函数，登记进中心注册表 `all_cases()`（先代码后抽象，`#[case]` 宏待模式第三次出现再沉淀）
3. `bind-check` 通过 → 跑通 → 台账状态置 `automated`
4. bug→case 管线：产品 bug 结案必须产出台账条目（复现条件 + 预期）；实现中第三次重复出现的模式才沉淀为表驱动抽象（先代码后抽象）

## 目录结构

```
test/e2e/             独立 workspace（与 bench-client/mock-backend 同级，不进主 workspace）
  CLAUDE.md     本手册
  Cargo.toml    workspace
  crates/
    e2e-tools/  L1 动词库 crate
    e2e-envs/   编排器（initdb / spawn / fixture / 故障注入 / 环境组调度）
    e2e-impl/   L3 用例实现 crate（中心注册表）
    e2e-runner/ L4 runner 二进制
  cases/        L2 台账
  reports/      报告产出（gitignore）
  runs/         运行期日志与临时配置（gitignore）
```

**所有命令在 `test/e2e/` 目录下执行。**

## 报告

- `reports/<run_id>/report.md`，按 case ID 键控 + 关键证据行
- `specified` 用例的红单独成节"已知 bug 证据"，不计入套件失败判定
- 汇总含 capability 覆盖热图，缺用例的 capability 显式列出
