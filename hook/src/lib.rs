//! 最简 pre_auth hook demo —— 打印 key + model name 后透传。
//!
//! 行为：
//!   1. gateway 每收到一个请求，extractor 提取 raw_key + 从 body 解析
//!      顶层 `model` 字段后调本 hook
//!   2. 本 hook 把 key 脱敏（前 3 + 中间全 * + 末尾 6）+ model 原样，
//!      用 eprintln! 打印到 stderr
//!   3. 返回 Continue，让 gateway 用原 raw_key 和原 model 继续走原生
//!      认证 + 路由（行为不变）
//!
//! ## 改 model name（老板的需求）
//!
//! 默认走 `PreAuthAction::Continue`（透传，不改 key 也不改 model）。
//! 要改 model 时，把 `pre_auth` 函数里返回 Continue 的那行换成：
//!
//! ```ignore
//! Ok(PreAuthResponse {
//!     action: PreAuthAction::ReplaceModel {
//!         new_key: req.raw_key.clone(),   // key 不改，原样
//!         new_model: "your-real-model".to_string(),  // ← 改这里
//!     },
//! })
//! ```
//!
//! gateway 收到 `ReplaceModel` 后：用 `new_key` 认证 + 用 `new_model`
//! 走 `check_model_access` 验证 + 路由。**不绕过权限**——`new_key` 在
//! DB 里的 `models` 白名单必须列 `new_model` 这个真实名，不能列
//! logical 名，否则 403。
//!
//! ## 想连 key 一起改？
//!
//! 把上面的 `new_key: req.raw_key.clone()` 换成你想用的真实 key 即可。
//! 典型场景：客户端用私有协议 key，hook 翻译成 gateway DB 里的真实 key。
//!
//! ## 不需要 hook_init
//!
//! 本 demo 没有初始化逻辑，不导出 `hook_init` 符号。gateway 加载时找
//! 不到 `hook_init` 会跳过，不影响 `pre_auth` 调用。

use boom_hooks_sdk::{pre_auth_entry, PreAuthAction, PreAuthRequest, PreAuthResponse};
use std::ffi::c_char;

/// 把 key 脱敏：前 3 + 中间全 * + 末尾 6。
///
/// 长度 < 9 时无法同时保留 3 头 + 6 尾（共 9 字符），退化为"全 *"加长度提示。
/// 长度 = 9 时刚好 3+6，中间 0 颗 *，输出 "sk-abcdef" 这种。
fn mask_key(key: &str) -> String {
    let len = key.chars().count();
    if len < 9 {
        // 太短，全脱敏 + 长度，避免泄漏
        format!("{}(len={})", "*".repeat(len), len)
    } else {
        let head: String = key.chars().take(3).collect();
        let tail: String = key.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
        let stars = "*".repeat(len - 9);
        format!("{head}{stars}{tail}")
    }
}

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
            // 打印 masked key + 原始 model name（None 表示 body 里没有
            // 顶层 model 字段，或 body 不是合法 JSON）
            eprintln!(
                "[pre-auth-demo] key={} model={:?}",
                mask_key(&req.raw_key),
                req.model_name,
            );

            // —— 默认：透传，不改 key 也不改 model ——
            Ok(PreAuthResponse {
                action: PreAuthAction::Continue,
            })

            // —— 改 model name：把上面 4 行注释掉，用下面这段 ——
            // Ok(PreAuthResponse {
            //     action: PreAuthAction::ReplaceModel {
            //         new_key: req.raw_key.clone(),
            //         new_model: "your-real-model".to_string(),
            //     },
            // })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_key_13_chars() {
        // "sk-abcdefghij" = 13 字符
        // 前 3 = "sk-"，末 6 = "efghij"，中间 4 字符 "abcd" → 4 颗 *
        assert_eq!(mask_key("sk-abcdefghij"), "sk-****efghij");
    }

    #[test]
    fn mask_key_15_chars() {
        // "sk-abcdefghijkl" = 15 字符（sk- + abcdefghijkl = 3 + 12）
        // 前 3 = "sk-"，末 6 = "ghijkl"，中间 6 字符 → 6 颗 *
        assert_eq!(mask_key("sk-abcdefghijkl"), "sk-******ghijkl");
    }

    #[test]
    fn mask_key_boundary_9_chars() {
        // 长度 9（= 3+6），中间无字符，0 颗 *
        assert_eq!(mask_key("sk-abcdef"), "sk-abcdef");
    }

    #[test]
    fn mask_key_too_short() {
        // 长度 < 9 走退化分支：全 * × len + (len=N)
        assert_eq!(mask_key("short"), "*****(len=5)");
        assert_eq!(mask_key("sk-abcd"), "*******(len=7)");
        assert_eq!(mask_key("sk-abcdef"), "sk-abcdef"); // 9 字符走 else 分支
    }

    #[test]
    fn mask_key_unicode_safe() {
        // 中文按字符数算（不是字节）
        // "sk-中文key测试一下abc" 字符数：
        // s k - 中 文 k e y 测 试 一 下 a b c = 15 字符
        // 前 3 = "sk-"，末 6 = "试一下abc"，中间 6 字符 → 6 颗 *
        let key = "sk-中文key测试一下abc";
        let masked = mask_key(key);
        assert!(masked.starts_with("sk-"));
        assert!(masked.ends_with("试一下abc"));
        assert_eq!(masked, "sk-******试一下abc");
    }
}
