# BooMGateway DT 测试（进程内代码覆盖测试）

开发者测试（DT）工程：**直接调用 boom-\* lib crate 的 pub API**（path 依赖源码级
链接、进程内执行），由 cargo-llvm-cov 统计功能源码覆盖率。不拉子进程、不测黑盒。

## 范围与约定

- **覆盖对象**：workspace 里全部 lib crate（boom-core / config / auth / provider /
  limiter / routing / audit / ctxaware / flowcontrol / fusion / kvindex /
  promptlog / trace / stressmon / alert）。
- **boom-main 不在 DT 覆盖范围**：它是纯 bin crate（组装层），外部无法进程内链接，
  其逻辑由 crate 内已有单测覆盖（这是显式决策，不是遗漏）。
- **boom-hooks-sdk 暂未纳入**：其 FFI raw-pointer API 在 clippy 1.95 有 6 个存量
  lint，待修复后加回 `Cargo.toml` 即可。
- **本包是主 workspace 的成员**（否则 llvm-cov 不会对 boom-\* 插桩），但根
  `Cargo.toml` 的 `default-members` 不含它 —— 裸 `cargo build` / `cargo test`
  行为不变。原有单测仍走 `cargo test`，与 DT 互不影响。

## 目录结构

```
dt-test/
├── Cargo.toml        boom-dt 包（path 依赖全部被测 lib crate）
├── src/lib.rs        DT harness：MockUpstream / TestServer / chat_request / free_port
└── tests/            DT 用例（按 testcase/cases/ 的域分文件）
    └── smoke.rs        冒烟：配置解析 / provider 调用链 / 路由存储

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

# DT 交付模式：JUnit XML（target/nextest/ci/junit.xml）
cargo nextest run -p boom-dt --profile ci
```

注意事项：

- **从仓库根目录跑**（`-p boom-dt` 选择包）。在 dt-test 目录里裸跑会丢覆盖率。
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
  计入功能覆盖率。统计时按路径过滤：只取 `SF:` 以 `/crates/boom-` 开头的条目。
- 终端汇总表只显示 `-p boom-dt` 选中的包（即 harness 自己），不是功能代码覆盖率；
  功能代码覆盖率看 lcov/HTML 报告里 `/crates/boom-` 条目的汇总。
- 增量覆盖率 = lcov 与 git 变更文件列表求交（流水线侧做）。

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
