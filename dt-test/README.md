# BooMGateway DT 测试（进程内代码覆盖测试）

开发者测试（DT）工程：**直接调用 boom-\* lib crate 的 pub API**（path 依赖源码级
链接、进程内执行），由 cargo-llvm-cov 统计功能源码覆盖率。不拉子进程、不测黑盒。

> **用例 ↔ AR 验收项映射**见 [AR_MAPPING.md](AR_MAPPING.md)（566 用例 → 5 条 AR，
> 含 kvc 调度两 AR 的函数级清单与覆盖缺口说明）。

## 覆盖范围（如实三档）

DT 的实际覆盖状态分三档（判定依据 = lcov 报告里出现且被用例驱动过的 crate）：

1. **已覆盖（13 个 lib crate）**：boom-core / config / auth / provider / limiter /
   routing / ctxaware / flowcontrol / fusion / kvindex / trace / stressmon / alert。
2. **显式排除**：
   - **boom-main** —— 纯 bin crate（组装层），外部无法进程内链接，逻辑由 crate
     内已有单测覆盖（显式决策，不是遗漏）。
   - **boom-hooks-sdk** —— FFI raw-pointer API 在 clippy 1.95 有 6 个存量 lint，
     待修复后加回 `Cargo.toml` 即可。
3. **待补（再议）**：boom-audit / boom-promptlog / boom-dashboard —— 尚无 DT
   用例。它们虽在依赖清单里（dashboard 除外），但没有任何测试引用时链接器不会
   把 crate 代码链进测试二进制，因此**不占覆盖率分母**，不虚增也不虚减。

**DB 路径约定**：DT 环境不起 Postgres/GaussDB，routing / limiter / auth 的
`*_db` 方法、DB 恢复/同步逻辑按约定跳过（无 Docker）。后果：

- boom-limiter ~84%（漏的是 `sync_counters_to_db` 等纯 DB 函数）；
- boom-auth ~44% —— 漏掉的 74 行**已逐行核对全部是 DB 分支**
  （`lookup_token` 查询 / `token_to_identity` / `lookup_team` /
  authenticate 中段的 team 解析与 blocked/expired/budget 校验），
  无 DB 可达面（哈希、主密钥、check_model_access、缓存失效钩子）已覆盖；
- boom-routing 的缺口同理（`*_db` SQL 行）。

## 其他约定

- **本包是主 workspace 的成员**（否则 llvm-cov 不会对 boom-\* 插桩），但根
  `Cargo.toml` 的 `default-members` 不含它 —— 裸 `cargo build` / `cargo test`
  行为不变。原有单测仍走 `cargo test`，与 DT 互不影响。

## 目录结构

```
dt-test/
├── Cargo.toml        boom-dt 包（path 依赖全部被测 lib crate）
├── src/lib.rs        DT harness：MockUpstream / TestServer / chat_request / free_port
└── tests/            DT 用例（按被测 crate / testcase 域分文件，一个 crate 至少一个）
    ├── smoke.rs        冒烟：配置解析 / provider 调用链 / 路由存储
    ├── harness.rs      DT harness 自身（free_port / MockUpstream / TestServer）
    ├── core.rs config.rs auth.rs provider.rs limiter.rs routing*.rs
    ├── ctxaware.rs flowcontrol.rs fusion.rs kvindex.rs trace*.rs
    ├── stressmon.rs alert.rs anthropic.rs azure.rs ml_service.rs …

仓库根：
├── .config/nextest.toml   nextest 配置（ci profile 出 JUnit XML）
```

## 全新环境跑 DT（从零开始）

依赖：Rust stable（用 rustup 安装）、llvm-tools-preview 组件、cargo-nextest、cargo-llvm-cov。

```bash
# 0.（国内网络必做）镜像配置，否则 crates.io / rustup 官方源会慢到卡死
cat >> ~/.cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
[net]
git-fetch-with-cli = true
EOF
cat >> ~/.zshrc <<'EOF'
export RUSTUP_DIST_SERVER="https://mirrors.ustc.edu.cn/rust-static"
export RUSTUP_UPDATE_ROOT="https://mirrors.ustc.edu.cn/rust-static/rustup"
EOF

# 1. 工具安装（brew 装 cargo-nextest 会卡 ghcr.io，用 cargo 源码装）
rustup component add llvm-tools-preview
cargo install cargo-nextest --locked
cargo install cargo-llvm-cov --locked

# 2. 跑 DT 用例（在仓库根目录，注意不是 cd 进 dt-test）
cargo nextest run -p boom-dt

# 3. 代码覆盖率（两步：先跑测试收 profraw，再出报告）
cargo llvm-cov nextest -p boom-dt
cargo llvm-cov report --lcov --output-path dt-test/target/lcov.info   # lcov（流水线解析）
cargo llvm-cov report --html --output-dir dt-test/target/html         # HTML（人工看）
# 终端直接看全量总表（--summary-only 只出 TOTAL 行；排除 DT 脚手架自身）
cargo llvm-cov report --summary-only --ignore-filename-regex 'dt-test/src'

# DT 交付模式：JUnit XML（target/nextest/ci/junit.xml）
cargo nextest run -p boom-dt --profile ci
```

注意事项：

- **从仓库根目录跑**（`-p boom-dt` 选择包）。在 dt-test 目录里裸跑会丢覆盖率。
  **裸跑 `cargo llvm-cov nextest`（不带 `-p`）是错的**：它作用于 default-members
  （全部 boom-\* 功能 crate），跑的是各 crate 原有单测而非 DT 用例，还会把
  boom-main / dashboard 等未覆盖 crate 插桩编译进缓存，污染分母（见下条）。
- **report 数字异常暴涨（分母翻数倍、出现整片 0.00% 的 boom-main/dashboard
  文件）时**：说明 `target/llvm-cov-target` 里残留了裸跑编译的对象。自愈三连：
  `rm -rf target/llvm-cov-target && cargo llvm-cov nextest -p boom-dt`，再出报告。
  （另：裸跑还会撞上 boom-routing 存量失败单测
  `auto_router::tests::code_request_routes_to_large`，属 master 已知问题。）
- **报告必须用 `--lcov` / `--html` 输出**。cargo-llvm-cov 的终端汇总表只显示
  `-p` 选中的包（boom-dt 自己），boom-\* 的覆盖数据在 lcov/HTML 报告里才完整
  （当前版本工具的行为）。

## 覆盖率口径

统计的是**功能源码**的覆盖率，不含测试代码：

- **各源文件内的 `#[cfg(test)] mod tests { ... }` 天然不参与统计**。boom-\* 作为
  依赖被 boom-dt 链接编译时 `cfg(test)` 是关闭的，测试模块整块不参与编译、不被
  插桩、不出现在 lcov 数据里。（已实测验证：`ml_service_client.rs` 501 行中
  268 行起是测试模块，lcov 数据行号止于 265。）
- **dt-test 自身的 harness/用例代码**（`dt-test/src/`、`dt-test/tests/`）也不应
  计入功能覆盖率。统计时按路径过滤：只取 `SF:` 以 `/crates/boom-` 开头的条目；
  终端表则用 `--ignore-filename-regex 'dt-test/src'` 排除。
- **`cargo llvm-cov nextest -p boom-dt` 跑完打印的那张表不是总覆盖率**——它跟着
  `-p` 过滤，只显示 boom-dt 包自己的源码（`dt-test/src/lib.rs` 这个 harness）。
  boom-\* 的覆盖数据在同一份 profdata 里，第二步 `cargo llvm-cov report`（不带
  `-p`）才展开全量表。harness 自身由 `tests/harness.rs` 的 `DT-HAR-*` 7 例覆盖
  （98.77% lines，仅剩 1 行 `expect` 的 panic 分支）。
- 当前基准（566 用例，2026-09，已同步 master !101）：终端全量表 TOTAL Lines
  含脚手架 85.87%，`--ignore-filename-regex 'dt-test/src'` 排除后 **85.77%**
  （达标口径 ≥85%）。残余缺口分类见 AR_MAPPING.md"覆盖缺口"第 5 条
  （gemini 硬编码 URL / DB 路径 / 纯竞态分支 / 防御性死代码，均经 lcov 逐行核对）。
- 增量覆盖率 = lcov 与 git 变更文件列表求交（流水线侧做）。

### 报告表字段含义

`cargo llvm-cov report` 的表有四个维度，各自带一个 Cover 百分比（所以有多个
"Cover" 列——它们是**四个不同指标**，不是同一个数的重复）：

| 列 | 含义 |
| --- | --- |
| Regions / Missed Regions / Cover | **区域覆盖**。region 是 llvm 源码覆盖的最小计数单元：一段没有分支跳进跳出的连续指令区间（类似基本块），每个 region 插一个计数器。一行代码可含多个 region（单行 `if/else`、`&&`/`\|\|` 短路、闭包），所以 region 覆盖通常 ≤ 行覆盖，对"行执行了但只走了一半"更敏感。 |
| Functions / Missed Functions / Executed | **函数覆盖**（最粗粒度）。整个函数是否至少被调用过一次；Executed = 被调用过的函数个数，Cover = Executed / Functions。 |
| Lines / Missed Lines / Cover | **行覆盖**——只统计可执行行（有指令映射的行；注释/空行/纯声明不计）。**80% 目标看的就是这一列**。 |
| Branches / Missed Branches / Cover | **分支覆盖**：每个条件跳转的两个方向是否都执行过。当前恒为 0——stable 工具链未启用分支插桩（需 nightly `-Zcoverage-options=branch`），此维度无数据，忽略即可。 |

Missed X = 该维度一次都没执行到的数量；四个维度粒度从粗到细：
Functions（函数被调过吗）< Lines（行执行过吗）< Regions（行内的区间都走到过吗）
< Branches（分支两边都走过吗，未启用）。

## 写新 DT 用例

- 用例放 `tests/<域>.rs`，与 `testcase/cases/` 编号域对应
  （access / invocation / billing / quota / admin / scheduling）。
- 基础设施从 `boom_dt` 拿：
  - `MockUpstream` — wiremock 模拟 OpenAI 上游（错误/延迟/断流自行 mount）
  - `TestServer` — 把 axum Router 起在随机端口（测 HTTP 组件）
  - `chat_request(json)` / `simple_chat_request(model, content)` — 构造请求
- 直调各 crate pub API，断言行为；每个用例头注释写用例 ID（如 `DT-INV-07`）。

## CI

`.github/workflows/ci.yml` 的 `dt-tests` job：装 llvm-tools + nextest + llvm-cov →
跑用例出 JUnit XML → 跑覆盖率出 lcov → 上传 `dt-reports` artifact。
