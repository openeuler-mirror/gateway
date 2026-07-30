//! Example pre_auth hook — prepends a configured prefix to the raw key.
//!
//! Demonstrates the "add prefix" scenario: a customer's incoming key
//! `sk-abc` is mapped to `sk-customer-sk-abc` (the hash of which is the
//! row stored in `boom_verification_token`).
//!
//! The prefix is read from the `config` string via `hook_init`. The config
//! string is JSON-decoded and the `prefix` field is extracted. Example
//! YAML in the gateway:
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
            let prefix = PREFIX.get().map(|s| s.as_str()).unwrap_or("sk-default-");
            let new_key = format!("{prefix}{}", req.raw_key);
            Ok(PreAuthResponse {
                action: PreAuthAction::Replace { new_key },
            })
        },
    )
}
