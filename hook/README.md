# pre_auth hook demo

最简 pre_auth hook 示例。行为：

1. gateway 收到请求，extractor 从 `Authorization` / `x-api-key` / `api-key` 提取 `raw_key`，从 body 解析顶层 `model` 字段
2. 调用本 hook 的 `pre_auth` 符号
3. 本 hook 把 key 脱敏（前 3 + 中间全 `*` + 末尾 6）+ 原始 model name 用 `eprintln!` 打到 stderr
4. 返回 `Continue`，让 gateway 用原 `raw_key` 和原 model 继续走原生认证 + 路由（行为不变）

## 改 model name

默认走 `Continue`（透传）。要改 model 时，打开 `src/lib.rs`，把 `pre_auth` 函数里返回 `Continue` 的那 4 行注释掉，启用下面注释里的 `ReplaceModel` 段：

```rust
// —— 默认：透传 ——
// Ok(PreAuthResponse {
//     action: PreAuthAction::Continue,
// })

// —— 改 model name ——
Ok(PreAuthResponse {
    action: PreAuthAction::ReplaceModel {
        new_key: req.raw_key.clone(),               // key 不改
        new_model: "your-real-model".to_string(),   // ← 改成真实 model 名
    },
})
```

gateway 收到 `ReplaceModel` 后：用 `new_key` 认证 + 用 `new_model` 走 `check_model_access` 验证 + 路由。**不绕过权限**——`new_key` 在 DB 里的 `models` 白名单必须列 `new_model` 这个真实名，不能列 logical 名，否则 403。

想连 key 一起改，把 `new_key: req.raw_key.clone()` 换成你想用的真实 key 即可（典型场景：客户端用私有协议 key，hook 翻译成 gateway DB 里的真实 key）。

## 编译

```bash
# 在仓库根目录
cd hook
cargo build --release

# 产物：
#   macOS  → target/release/libpre_auth_demo.dylib
#   Linux  → target/release/libpre_auth_demo.so
```

> 依赖 `boom-hooks-sdk`，路径是 `../crates/boom-hooks-sdk`，所以必须在仓库内编译，不能拷贝出去单独编译。

## 配置 gateway 加载

在 gateway 的 `config.yaml` 末尾加：

```yaml
hooks:
  pre_auth:
    enabled: true
    path: /absolute/path/to/libpre_auth_demo.dylib   # 改成你的实际产物路径
    failure_mode: allow                               # hook 异常时降级走原生认证
    allowed_headers: []                              # 本 demo 不需要任何 header
    config: ""                                       # 本 demo 不读 config
```

## 启动 gateway + 测试

```bash
# 启动 gateway（stderr 重定向到终端，能看到 hook 的 eprintln 输出）
cargo run -p boom-main --release -- --config config.yaml 2>&1 | tee gateway.log

# 另一个终端发请求
curl -X POST http://localhost:4000/v1/chat/completions \
  -H "Authorization: Bearer sk-abcdefghijkl" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}'
```

在 `gateway.log` 里能看到：

```
[pre-auth-demo] key=sk-****efghij model=Some("gpt-4o")
```

## masking 规则

| 输入 key | 输出 | 字符数 |
|---|---|---|
| `sk-abcdefghij` | `sk-****efghij` | 13 (3+4*+6) |
| `sk-abcdefghijkl` | `sk-******ghijkl` | 15 (3+6*+6) |
| `sk-abcdef` | `sk-abcdef` | 9 (3+0*+6,刚好 3+6) |
| `sk-abcd` | `*******(len=7)` | 7 (< 9,全脱敏) |
| `short` | `*****(len=5)` | 5 (< 9,全脱敏) |

**规则**：保留前 3 字符 + 末 6 字符,中间用 `*` 填充(数量 = len - 9)。长度 < 9 时无法同时保留 3 头 + 6 尾,改用"全 `*` × len + (len=N)"避免泄漏。

## 修改方向

把这个 demo 改成实际业务场景：

- **改 model name**：默认 `Continue` → 注释启用 `ReplaceModel`（见上文）
- **加前缀**：`PreAuthAction::Continue` → `PreAuthAction::Replace { new_key: format!("sk-customer-{}", req.raw_key) }`
- **删后缀**：用 `req.raw_key.trim_end_matches("-internal")` 后 Replace
- **查表转换**：加 `hook_init` 符号建 DB 连接池,`pre_auth` 闭包内查表后 Replace / ReplaceModel
- **拒审**：`PreAuthAction::Reject { reason: "..." }` 或 `Err(HookError::Reject("...".into()))`

## 文件结构

```
hook/
├── Cargo.toml         # crate-type = ["cdylib"] + 依赖 boom-hooks-sdk
├── src/
│   └── lib.rs          # pre_auth 符号 + mask_key + 测试
└── README.md           # 本文
```
