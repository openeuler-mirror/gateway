//! 最简 pre_auth hook demo —— 打印 masked key 后透传。
//!
//! 行为：
//!   1. gateway 每收到一个请求，extractor 提取 raw_key 后调本 hook
//!   2. 本 hook 把 key 脱敏后用 eprintln! 打印到 stderr
//!      - 前 3 个字符原样显示
//!      - 中间所有字符替换为 *
//!      - 末尾 6 个字符原样显示
//!   3. 返回 Continue，让 gateway 用原 raw_key 继续走原生认证
//!
//! masking 示例：
//!   "sk-abcdefghij"  (13 字符) → "sk-*******ghij"
//!   "sk-abcd"        (7 字符)  → "sk-*abcd"      (末尾 6 位 = 整个后半段)
//!   "short"          (5 字符)  → "***short" 的反向？见 mask_key 函数
//!
//! 注意：本 demo 不需要 hook_init（没有初始化逻辑），所以不导出该符号。
//! gateway 加载时找不到 hook_init 会跳过，不影响 pre_auth 调用。

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
            // 打印 masked key + 本 hook 配置的 allowed_headers 里的 header
            // （本 demo 默认 allowed_headers: []，所以 headers 为空）
            eprintln!(
                "[pre-auth-demo] key={} headers={:?}",
                mask_key(&req.raw_key),
                req.headers
            );
            // 透传：用原 raw_key 走原生认证
            Ok(PreAuthResponse {
                action: PreAuthAction::Continue,
            })
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
