//! # Example pre_auth hook — `ReplaceModel` demo（判断 + 分派函数形态）
//!
//! 演示老板的真实场景：用户用 key 区分场景，不同场景的同名 model
//! 路由到不同 real model。
//!
//! ## 主流程
//!
//! ```text
//! pre_auth 进入
//!   │
//!   ▼
//! classify_key(raw_key) → KeyType { A | B | C | Passthrough }
//!   │
//!   ▼
//! 按 KeyType 分派到 resolve_model_a / resolve_model_b / resolve_model_c
//!   │
//!   ▼
//! 转换函数返回 Option<String>：
//!   Some(new_model) → ReplaceModel { new_key, new_model }
//!   None            → Replace { new_key }（model 原样透传）
//! ```
//!
//! ## 各角色职责
//!
//! - **classify_key**：只回答"这个 key 属于哪种场景"。**不**关心 model 怎么改。
//! - **resolve_model_a/b/c**：每个场景一个转换函数，内部各自定义 model
//!   映射表（auto → glm / auto → qwen / auto → minimax），互不干扰。
//! - **pre_auth**：组合两者——先判断 key 类型，再分派到对应转换函数。
//!
//! ## 自定义 header（自定义请求头）
//!
//! hook 返回的 `headers` 字段（`PreAuthResponse::headers`）可以让 hook
//! 为请求构造自定义 HTTP header。本 demo 把 key 场景标记写入
//! `X-User-Tag`——配合审计日志的 `general_settings.user_tag_header:
//! "X-User-Tag"` 配置，即可实现"hook 动态计算归因标记 + 审计日志按标记
//! 统计用量"（比如这里从 key 推场景，生产环境可换成解 JWT / 查内部表）。
//!
//! 网关把这些 header **当成普通请求头**处理：
//!
//! - **覆盖语义**：同名（大小写不敏感）时覆盖客户端自带的值——hook 是
//!   管理员安装的可信代码，它构造的值是权威值。
//! - **审计归属**：`user_tag` 提取发生在注入之后，成功 / 401 / 限流等
//!   所有路径都能记录到 hook 构造的标记。
//! - **上游透传**：走 deployment 的客户端 header 白名单
//!   （`forward_client_headers`），白名单放行才会转发到上游；
//!   `authorization` / `x-gateway-*` 等硬屏蔽名单照常生效，hook 无法
//!   借此伪造凭证或优先级。
//!
//! ## 老板怎么扩展
//!
//! 1. **新增场景**：在 `KeyType` enum 加一个变体，写一个对应的
//!    `resolve_model_xxx` 函数，在 `pre_auth` 的 `match` 加一个 arm。
//! 2. **场景内增改映射**：只改对应 `resolve_model_xxx` 的 `match`，
//!    其它场景不动。
//! 3. **key 多到 match 写不下**：把 `classify_key` 改成查外部表
//!    （JSON / DB / Redis），主流程不变。
//!
//! ## 不绕过权限
//!
//! hook 返回 `ReplaceModel` 时，gateway 用 `new_model` 做
//! `check_model_access`。所以 `new_key` 在 DB 里的 `models` 白名单
//! 必须列 **real model 名**（`glm` / `qwen` / `minimax`），不能
//! 列 logical 名（`auto`），否则 403。
//!
//! ## YAML 配置
//!
//! ```yaml
//! hooks:
//!   pre_auth:
//!     enabled: true
//!     path: /path/to/libexample_pre_auth_hook.so
//!     failure_mode: allow
//!     allowed_headers: []
//!     config: '{"prefix":"sk-customer-"}'
//! ```
//!
//! `prefix` 决定 key 改写规则：raw key `sk-abc` → `sk-customer-sk-abc`
//! （其 hash 是 DB `boom_verification_token` 表的行）。

use boom_hooks_sdk::{
    hook_init_entry, pre_auth_entry, PreAuthAction, PreAuthRequest, PreAuthResponse,
};
use std::ffi::c_char;
use std::sync::OnceLock;

static PREFIX: OnceLock<String> = OnceLock::new();

#[no_mangle]
pub extern "C" fn hook_init(config: *const c_char, config_len: u32) -> i32 {
    hook_init_entry(config as *const u8, config_len, |config| {
        let prefix = config
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.get("prefix").and_then(|p| p.as_str()).map(String::from))
            .unwrap_or_else(|| "sk-default-".to_string());
        let _ = PREFIX.set(prefix);
        Ok(())
    })
}

// ============================================================
// 1. key 类型判断
//
// 只回答"这个 key 属于哪种场景"。不关心 model 怎么改。
//
// 升级路径：
//   - 少量固定规则：match
//   - key 多到写不下：查外部表（JSON / DB / Redis）→ match Some("A") => A
//   - key 命名有规律：按前缀分派
// ============================================================

enum KeyType {
    A,            // 场景 A：auto → glm
    B,            // 场景 B：auto → qwen
    C,            // 场景 C：auto → minimax
    Passthrough,  // 不改 model
}

fn classify_key(raw_key: &str) -> KeyType {
    match raw_key {
        // 场景 A 的 key 列表（演示；生产时改成查表）
        "customer-A-001" | "customer-A-002" => KeyType::A,
        // 场景 B 的 key 列表
        "customer-B-001" | "customer-B-002" => KeyType::B,
        // 场景 C 的 key 列表
        "customer-C-001" | "customer-C-002" => KeyType::C,
        // 其它 key：不动 model
        _ => KeyType::Passthrough,
    }
}

// ============================================================
// 2. 每个场景一个转换函数
//
// 内部各自定义 model 映射，互不干扰。
// 返回 Option<String>：
//   Some(new_model) → 改 model
//   None            → 当前 model 不在改写列表，原样透传
// ============================================================

/// 场景 A：auto → glm，gpt-4 → glm-4。
fn resolve_model_a(req_model: Option<&str>) -> Option<String> {
    match req_model {
        Some("auto")  => Some("glm".to_string()),
        Some("gpt-4") => Some("glm-4".to_string()),
        _ => None,  // 其它 model（如 claude-3）原样透传
    }
}

/// 场景 B：auto → qwen，gpt-3.5 → qwen-2.5。
fn resolve_model_b(req_model: Option<&str>) -> Option<String> {
    match req_model {
        Some("auto")    => Some("qwen".to_string()),
        Some("gpt-3.5") => Some("qwen-2.5".to_string()),
        _ => None,
    }
}

/// 场景 C：auto → minimax。
fn resolve_model_c(req_model: Option<&str>) -> Option<String> {
    match req_model {
        Some("auto") => Some("minimax".to_string()),
        _ => None,
    }
}

// ============================================================
// 3. pre_auth 主流程：判断 key 类型 → 替换 key → 分派到转换函数
// ============================================================

#[no_mangle]
pub extern "C" fn pre_auth(
    req: *const c_char,
    req_len: u32,
    out: *mut c_char,
    out_cap: u32,
    out_len: *mut u32,
) -> i32 {
    pre_auth_entry(
        req as *const u8,
        req_len,
        out as *mut u8,
        out_cap,
        out_len,
        |req: PreAuthRequest| {
            // 1. 替换 key（prefix 拼接）
            let prefix = PREFIX.get().map(|s| s.as_str()).unwrap_or("sk-default-");
            let new_key = format!("{prefix}{}", req.raw_key);

            // 2. 判断 key 类型
            let key_type = classify_key(&req.raw_key);

            // 3. 按类型分派到对应转换函数
            let new_model = match key_type {
                KeyType::A => resolve_model_a(req.model_name.as_deref()),
                KeyType::B => resolve_model_b(req.model_name.as_deref()),
                KeyType::C => resolve_model_c(req.model_name.as_deref()),
                KeyType::Passthrough => None,
            };

            // 4. 构造自定义 header：场景标记写入 X-User-Tag。网关把它
            //    当成普通请求头——审计 user_tag 归属、promptlog 记录、
            //    deployment 白名单透传全部生效（见模块文档）。
            let scene = match key_type {
                KeyType::A => "scene-a",
                KeyType::B => "scene-b",
                KeyType::C => "scene-c",
                KeyType::Passthrough => "uncategorized",
            };
            let mut headers = std::collections::HashMap::new();
            headers.insert("X-User-Tag".to_string(), scene.to_string());

            // 5. 转换函数返回 Some → ReplaceModel；返回 None → 只换 key
            match new_model {
                Some(new_model) => Ok(PreAuthResponse {
                    action: PreAuthAction::ReplaceModel { new_key, new_model },
                    headers,
                }),
                None => Ok(PreAuthResponse {
                    action: PreAuthAction::Replace { new_key },
                    headers,
                }),
            }
        },
    )
}
