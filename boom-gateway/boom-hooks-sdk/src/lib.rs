//! Boom gateway hook SDK.
//!
//! Shared types and helpers for writing gateway hook plugins (.so / .dylib).
//!
//! A hook plugin is a `cdylib` crate that exports C-ABI symbols. The gateway
//! loads it via `libloading`, calls `hook_init` once at load time, then calls
//! the per-point symbol (e.g. `pre_auth`) on each request.
//!
//! Wire contract between gateway and plugin is JSON over a C-ABI buffer:
//!
//! ```c
//! int32_t hook_init(const char* config_json, uint32_t config_len);
//! int32_t pre_auth(
//!     const char*  request_json,    // serialized PreAuthRequest
//!     uint32_t     request_len,
//!     char*        response_buf,    // gateway-allocated, 64KB cap
//!     uint32_t     response_buf_cap,
//!     uint32_t*    response_len    // out: actual bytes written
//! );
//! ```
//!
//! Return codes:
//! - `0` — success, response_buf holds serialized response
//! - `1` — business reject (response_buf still holds serialized PreAuthResponse)
//! - `2` — response buffer overflow
//! - `3` — internal error / panic
//!
//! Plugins should wrap their logic in `catch_unwind` (via [`pre_auth_entry`])
//! so a panic cannot abort the gateway process.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Return codes for the C-ABI boundary.
pub mod return_codes {
    pub const OK: i32 = 0;
    pub const BUSINESS: i32 = 1;
    pub const OVERFLOW: i32 = 2;
    pub const INTERNAL: i32 = 3;
}

/// Input passed to the `pre_auth` hook point.
///
/// `raw_key` is the API key extracted from the request (`Authorization: Bearer …`
/// / `x-api-key` / `api-key`). `headers` contains only the names listed in
/// `hooks.pre_auth.allowed_headers` in the gateway YAML — anything else is
/// filtered out before serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreAuthRequest {
    pub raw_key: String,
    pub headers: HashMap<String, String>,
}

/// The hook's decision for the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum PreAuthAction {
    /// Forward the original raw_key to the authenticator unchanged.
    Continue,
    /// Replace the raw_key with `new_key` before authentication.
    Replace { new_key: String },
    /// Reject the request with 401. `reason` is surfaced in the error body.
    Reject { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreAuthResponse {
    #[serde(flatten)]
    pub action: PreAuthAction,
}

/// Errors a hook implementation can produce.
///
/// `Reject` is a *business* outcome — the request is rejected but the hook
/// ran successfully. `Internal` is a runtime failure (DB error, panic) and
/// triggers the gateway's failure-mode fallback.
#[derive(Debug)]
pub enum HookError {
    Reject(String),
    Internal(String),
}

impl HookError {
    pub fn reject(msg: impl Into<String>) -> Self {
        HookError::Reject(msg.into())
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        HookError::Internal(msg.into())
    }

    pub fn code(&self) -> i32 {
        match self {
            HookError::Reject(_) => return_codes::BUSINESS,
            HookError::Internal(_) => return_codes::INTERNAL,
        }
    }
}

/// SHA-256 hash a raw API key, matching `boom_auth::DbAuthenticator::hash_token`.
///
/// Plugins that store key hashes in their own mapping table should use this
/// so the hash format matches the gateway's `boom_verification_token` table.
pub fn hash_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    hex::encode(h.finalize())
}

/// Standard entry-point helper for the `pre_auth` C-ABI symbol.
///
/// Plugins call this from their `#[no_mangle] extern "C" fn pre_auth(...)` and
/// pass a closure that does the actual work. It handles:
/// - deserializing the request JSON
/// - invoking the closure inside `catch_unwind` (panic → INTERNAL error)
/// - serializing the response
/// - writing into the gateway-provided buffer with capacity check
/// - returning the right return code
///
/// A plugin's `pre_auth` symbol can be as small as:
///
/// ```ignore
/// #[no_mangle]
/// pub extern "C" fn pre_auth(
///     req: *const u8, req_len: u32,
///     out: *mut u8, out_cap: u32, out_len: *mut u32,
/// ) -> i32 {
///     boom_hooks_sdk::pre_auth_entry(req, req_len, out, out_cap, out_len, |req| {
///         // user logic here
///         Ok(boom_hooks_sdk::PreAuthResponse {
///             action: boom_hooks_sdk::PreAuthAction::Continue,
///         })
///     })
/// }
/// ```
///
/// # Safety
/// Pointers must be valid for their stated lengths. The gateway is the only
/// intended caller and constructs them from Rust slices.
pub fn pre_auth_entry<F>(
    req_ptr: *const u8,
    req_len: u32,
    out_ptr: *mut u8,
    out_cap: u32,
    out_len: *mut u32,
    handler: F,
) -> i32
where
    F: Fn(PreAuthRequest) -> Result<PreAuthResponse, HookError> + std::panic::RefUnwindSafe,
{
    let result = std::panic::catch_unwind(|| {
        // Safety: caller (gateway) passes a valid slice.
        let req_bytes = unsafe {
            std::slice::from_raw_parts(req_ptr, req_len as usize)
        };
        let req: PreAuthRequest = match serde_json::from_slice(req_bytes) {
            Ok(r) => r,
            Err(e) => return Err(HookError::internal(format!("deserialize req: {e}"))),
        };

        let resp = handler(req)?;
        let json = match serde_json::to_vec(&resp) {
            Ok(v) => v,
            Err(e) => return Err(HookError::internal(format!("serialize resp: {e}"))),
        };

        if json.len() > out_cap as usize {
            return Err(HookError::internal("response buffer overflow"));
        }

        // Safety: out_ptr is valid for out_cap bytes; we just checked len <= cap.
        unsafe {
            std::ptr::copy_nonoverlapping(json.as_ptr(), out_ptr, json.len());
            *out_len = json.len() as u32;
        }
        Ok(())
    });

    match result {
        Ok(Ok(())) => return_codes::OK,
        Ok(Err(e)) => {
            // Business reject or internal error — both still emit a response
            // body so the gateway can surface the reason. We re-serialize as
            // a Reject-style response so the gateway path is uniform.
            // For Reject, the closure already returned a PreAuthResponse with
            // action=Reject, so the body is already written. For Internal,
            // we synthesize a Reject response with the error message.
            let resp = PreAuthResponse {
                action: PreAuthAction::Reject {
                    reason: match &e {
                        HookError::Reject(r) => r.clone(),
                        HookError::Internal(r) => r.clone(),
                    },
                },
            };
            if let Ok(json) = serde_json::to_vec(&resp) {
                if json.len() <= out_cap as usize {
                    // Safety: same as above.
                    unsafe {
                        std::ptr::copy_nonoverlapping(json.as_ptr(), out_ptr, json.len());
                        *out_len = json.len() as u32;
                    }
                }
            }
            e.code()
        }
        Err(_) => return_codes::INTERNAL,
    }
}

/// Standard entry-point helper for `hook_init`.
///
/// Pass the raw config bytes (or None) to a closure. Returns 0 on success,
/// 3 on any error or panic. Plugins use this so they don't have to write
/// the catch_unwind boilerplate themselves.
pub fn hook_init_entry<F>(config_ptr: *const u8, config_len: u32, handler: F) -> i32
where
    F: Fn(Option<&str>) -> Result<(), HookError> + std::panic::RefUnwindSafe,
{
    let result = std::panic::catch_unwind(|| {
        let config = if config_ptr.is_null() {
            None
        } else {
            // Safety: caller (gateway) passes valid slice.
            let bytes = unsafe {
                std::slice::from_raw_parts(config_ptr, config_len as usize)
            };
            Some(std::str::from_utf8(bytes).unwrap_or(""))
        };
        handler(config)
    });
    match result {
        Ok(Ok(())) => return_codes::OK,
        Ok(Err(_)) => return_codes::INTERNAL,
        Err(_) => return_codes::INTERNAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that runs `pre_auth_entry` with a serialized request and a
    /// fresh 64KB buffer, returning (rc, response_bytes).
    fn run_pre_auth<F>(req: PreAuthRequest, handler: F) -> (i32, Vec<u8>)
    where
        F: Fn(PreAuthRequest) -> Result<PreAuthResponse, HookError> + std::panic::RefUnwindSafe,
    {
        let req_bytes = serde_json::to_vec(&req).unwrap();
        let mut buf = vec![0u8; RESPONSE_BUF_SIZE_FOR_TESTS];
        let mut out_len: u32 = 0;
        let rc = pre_auth_entry(
            req_bytes.as_ptr(),
            req_bytes.len() as u32,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut out_len,
            handler,
        );
        buf.truncate(out_len as usize);
        (rc, buf)
    }

    const RESPONSE_BUF_SIZE_FOR_TESTS: usize = 64 * 1024;

    #[test]
    fn hash_key_matches_sha256_hex() {
        use sha2::Digest;
        let key = "sk-test-abc";
        let h = hash_key(key);
        let mut sha = sha2::Sha256::new();
        sha.update(key.as_bytes());
        let expected = hex::encode(sha.finalize());
        assert_eq!(h, expected);
    }

    #[test]
    fn pre_auth_action_serializes_with_snake_case_tag() {
        let resp = PreAuthResponse {
            action: PreAuthAction::Replace {
                new_key: "sk-internal".into(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""action":"replace""#));
        assert!(json.contains(r#""new_key":"sk-internal""#));

        let resp2 = PreAuthResponse {
            action: PreAuthAction::Continue,
        };
        assert_eq!(
            serde_json::to_string(&resp2).unwrap(),
            r#"{"action":"continue"}"#
        );
    }

    #[test]
    fn pre_auth_entry_continue_returns_zero_and_continue_body() {
        let req = PreAuthRequest {
            raw_key: "sk-abc".into(),
            headers: HashMap::new(),
        };
        let (rc, body) = run_pre_auth(req, |_| {
            Ok(PreAuthResponse {
                action: PreAuthAction::Continue,
            })
        });
        assert_eq!(rc, return_codes::OK);
        let resp: PreAuthResponse = serde_json::from_slice(&body).unwrap();
        assert!(matches!(resp.action, PreAuthAction::Continue));
    }

    #[test]
    fn pre_auth_entry_replace_writes_new_key() {
        let req = PreAuthRequest {
            raw_key: "sk-abc".into(),
            headers: HashMap::new(),
        };
        let (rc, body) = run_pre_auth(req, |r| {
            Ok(PreAuthResponse {
                action: PreAuthAction::Replace {
                    new_key: format!("sk-prefix-{}", r.raw_key),
                },
            })
        });
        assert_eq!(rc, return_codes::OK);
        let resp: PreAuthResponse = serde_json::from_slice(&body).unwrap();
        match resp.action {
            PreAuthAction::Replace { new_key } => assert_eq!(new_key, "sk-prefix-sk-abc"),
            _ => panic!("expected Replace"),
        }
    }

    #[test]
    fn pre_auth_entry_reject_returns_business_code_with_reason() {
        let req = PreAuthRequest {
            raw_key: "sk-abc".into(),
            headers: HashMap::new(),
        };
        let (rc, body) = run_pre_auth(req, |_| {
            Err(HookError::Reject("missing ucid".into()))
        });
        assert_eq!(rc, return_codes::BUSINESS);
        let resp: PreAuthResponse = serde_json::from_slice(&body).unwrap();
        match resp.action {
            PreAuthAction::Reject { reason } => assert_eq!(reason, "missing ucid"),
            _ => panic!("expected Reject"),
        }
    }

    #[test]
    fn pre_auth_entry_internal_error_returns_internal_code() {
        let req = PreAuthRequest {
            raw_key: "sk-abc".into(),
            headers: HashMap::new(),
        };
        let (rc, _body) = run_pre_auth(req, |_| {
            Err(HookError::Internal("db down".into()))
        });
        assert_eq!(rc, return_codes::INTERNAL);
    }

    #[test]
    fn pre_auth_entry_panicking_handler_returns_internal_not_abort() {
        let req = PreAuthRequest {
            raw_key: "sk-abc".into(),
            headers: HashMap::new(),
        };
        let (rc, _body) = run_pre_auth(req, |_| panic!("boom"));
        assert_eq!(rc, return_codes::INTERNAL);
    }

    #[test]
    fn pre_auth_entry_malformed_request_returns_internal() {
        // Pass invalid JSON bytes directly — bypass serialization.
        let bad_bytes = b"not json at all";
        let mut buf = vec![0u8; 64 * 1024];
        let mut out_len: u32 = 0;
        let rc = pre_auth_entry(
            bad_bytes.as_ptr(),
            bad_bytes.len() as u32,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut out_len,
            |_| Ok(PreAuthResponse {
                action: PreAuthAction::Continue,
            }),
        );
        assert_eq!(rc, return_codes::INTERNAL);
    }

    #[test]
    fn hook_init_entry_passes_config_through() {
        let cfg = b"{\"prefix\":\"sk-customer-\"}";
        let rc = hook_init_entry(cfg.as_ptr(), cfg.len() as u32, |config| {
            assert_eq!(config, Some("{\"prefix\":\"sk-customer-\"}"));
            Ok(())
        });
        assert_eq!(rc, return_codes::OK);
    }

    #[test]
    fn hook_init_entry_null_config_passes_none() {
        let rc = hook_init_entry(std::ptr::null(), 0, |config| {
            assert!(config.is_none());
            Ok(())
        });
        assert_eq!(rc, return_codes::OK);
    }

    #[test]
    fn hook_init_entry_error_returns_internal() {
        let cfg = b"cfg";
        let rc = hook_init_entry(cfg.as_ptr(), cfg.len() as u32, |_| {
            Err(HookError::Internal("init failed".into()))
        });
        assert_eq!(rc, return_codes::INTERNAL);
    }

    #[test]
    fn hook_init_entry_panicking_returns_internal() {
        let cfg = b"cfg";
        let rc = hook_init_entry(cfg.as_ptr(), cfg.len() as u32, |_| panic!("boom"));
        assert_eq!(rc, return_codes::INTERNAL);
    }
}

