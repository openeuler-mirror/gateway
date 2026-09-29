//! DT 用例 — boom-auth：密钥认证（无 DB 部分）。
//!
//! 覆盖：
//! - hash_token：整键 SHA-256（空串/已知向量、前缀与内嵌 '-' 全参与）
//! - 主密钥认证：精确匹配 → 全权限 identity（models 空 = 不限模型）；
//!   同长错误键 / 异长错误键 → AuthError（常量时间比较两条路径）
//! - 未配置主密钥且无 DB → 一切键 AuthError（lookup_token 无库早退）
//! - check_model_access：白名单命中 / 未命中 ModelNotAllowed / 空清单全放行
//! - lookup_key_aliases：无 DB → 空 map
//!
//! 跳过（需要 Postgres）：lookup_token / lookup_team 的 DB 查询、
//! token_to_identity、blocked/expired/budget 校验分支（依赖 DB 行）、
//! resolve_team_models（仅 authenticate 的 DB 路径可达）。

use boom_auth::DbAuthenticator;
use boom_core::provider::{Authenticator, KeyAliasLookup};
use boom_core::types::AuthIdentity;

// ───────────────────────── helpers ─────────────────────────

fn identity(models: Vec<&str>) -> AuthIdentity {
    AuthIdentity {
        key_hash: "hashed".to_string(),
        key_name: Some("n".to_string()),
        key_alias: None,
        user_id: None,
        team_id: None,
        team_alias: None,
        models: models.into_iter().map(String::from).collect(),
        team_models: vec![],
        rpm_limit: None,
        tpm_limit: None,
        max_budget: None,
        spend: 0.0,
        blocked: false,
        expires_at: None,
        metadata: serde_json::Value::Null,
    }
}

// ═════════════════════════════════════════════════════════════════
// hash_token — 纯函数
// ═════════════════════════════════════════════════════════════════

/// DT-AU-01：hash_token = 整个原始键逐字节的 SHA-256 hex（litellm 兼容契约）。
#[test]
fn hash_token_is_sha256_of_whole_raw_key() {
    // sha256("abc") 已知向量
    assert_eq!(
        DbAuthenticator::hash_token("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    // 空串的 sha256 常量
    assert_eq!(
        DbAuthenticator::hash_token(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(DbAuthenticator::hash_token("abc").len(), 64);
    // 前缀参与哈希：换前缀 → 摘要必变（前缀防篡改是隐式的）
    assert_ne!(
        DbAuthenticator::hash_token("sk-prod-x1"),
        DbAuthenticator::hash_token("sk-test-x1")
    );
    // 内嵌 '-' 的 litellm 风格键按整串处理
    assert_ne!(
        DbAuthenticator::hash_token("sk-a-beEoPxxx"),
        DbAuthenticator::hash_token("sk-abeEoPxxx")
    );
}

// ═════════════════════════════════════════════════════════════════
// authenticate — 主密钥 / 无库拒绝
// ═════════════════════════════════════════════════════════════════

/// DT-AU-02：主密钥 → 全权限 identity；同长/异长错误键 → AuthError。
#[tokio::test]
async fn master_key_authenticates_with_full_access() {
    let auth = DbAuthenticator::new(None, Some("sk-master-123".to_string()));

    let id = auth.authenticate("sk-master-123").await.expect("master key accepted");
    assert_eq!(id.key_hash, "master");
    assert_eq!(id.key_name.as_deref(), Some("master"));
    assert_eq!(id.key_alias.as_deref(), Some("master"));
    assert!(id.models.is_empty(), "master is unrestricted");
    assert!(id.can_call_model("any-model"));
    assert!(!id.blocked);
    assert_eq!(id.expires_at, None);
    assert_eq!(id.spend, 0.0);

    // 同长错误键（常量时间比较逐字节路径）
    let err = auth.authenticate("sk-master-124").await.err().expect("wrong key");
    assert!(matches!(err, boom_core::GatewayError::AuthError(ref m) if m.contains("Invalid API key")));
    // 异长错误键（长度不等短路）
    let err = auth.authenticate("short").await.err().expect("wrong length");
    assert!(matches!(err, boom_core::GatewayError::AuthError(_)));
}

/// DT-AU-03：无主密钥、无 DB → 任何键都 Invalid（无库早退路径）。
#[tokio::test]
async fn no_master_no_db_rejects_everything() {
    let auth = DbAuthenticator::new(None, None);
    let err = auth.authenticate("sk-master-123").await.err().expect("rejected");
    assert!(matches!(err, boom_core::GatewayError::AuthError(ref m) if m.contains("Invalid API key")));
    let err = auth.authenticate("sk-anything-else").await.err().expect("rejected too");
    assert!(matches!(err, boom_core::GatewayError::AuthError(_)));
}

// ═════════════════════════════════════════════════════════════════
// check_model_access — 模型白名单
// ═════════════════════════════════════════════════════════════════

/// DT-AU-04：check_model_access —— 白名单命中放行 / 未命中 ModelNotAllowed /
/// 空清单（master、未配置）全放行。
#[tokio::test]
async fn check_model_access_matrix() {
    let auth = DbAuthenticator::new(None, None);

    let id = identity(vec!["gpt-4"]);
    assert!(auth.check_model_access(&id, "gpt-4").is_ok());
    let err = auth.check_model_access(&id, "claude-3").err().expect("denied");
    assert!(matches!(err, boom_core::GatewayError::ModelNotAllowed(ref m) if m == "claude-3"));

    let open = identity(vec![]);
    assert!(auth.check_model_access(&open, "whatever").is_ok());
}

// ═════════════════════════════════════════════════════════════════
// lookup_key_aliases — 无 DB 早退
// ═════════════════════════════════════════════════════════════════

/// DT-AU-05：lookup_key_aliases 无 DB → 空 map（含空入参）。
#[tokio::test]
async fn lookup_key_aliases_without_db_returns_empty() {
    let auth = DbAuthenticator::new(None, None);
    assert!(auth.lookup_key_aliases(&["hash-1", "hash-2"]).await.is_empty());
    assert!(auth.lookup_key_aliases(&[]).await.is_empty());
}

// ═════════════════════════════════════════════════════════════════
// 缓存失效 — dashboard 编辑 key 后立即生效的钩子（!98）
// ═════════════════════════════════════════════════════════════════

/// DT-AU-06：invalidate_key / invalidate_all —— AdminCommand 编辑/清理 key 后
/// 使认证缓存立即失效；无 DB、缓存无条目时是安全 no-op，不影响主密钥认证，
/// 重复调用幂等。
#[tokio::test]
async fn cache_invalidate_key_and_all_safe_without_db() {
    let auth = DbAuthenticator::new(None, Some("sk-master-1".to_string()));
    auth.invalidate_key("1234567890abcdef1234").await;
    auth.invalidate_all().await;
    // 失效只清缓存条目，不波及主密钥认证
    assert!(auth.authenticate("sk-master-1").await.is_ok());
    // 幂等：重复失效同样安全
    auth.invalidate_key("1234567890abcdef1234").await;
    auth.invalidate_all().await;
}
