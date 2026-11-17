//! key 哈希与 cache_key 构成——用例 poll DB 行的寻址动词。
//! 契约：SHA-256 hex（litellm hash_token 兼容，boom_verification_token.token 即此值）；
//! 窗口行 `{key_hash}:{model}:{window_secs}`；累计行 `kc:{key_hash}:{suffix}`；
//! team 行 `tc:{team_id}:{suffix}`。

use sha2::{Digest, Sha256};

/// litellm 兼容 key 哈希（= boom_verification_token.token 主键）。
pub fn key_hash(raw_key: &str) -> String {
    let mut h = Sha256::new();
    h.update(raw_key.as_bytes());
    hex::encode(h.finalize())
}

/// 窗口代次行 cache_key（boom_rate_limit_state 复合主键之一）。
pub fn window_cache_key(raw_key: &str, model: &str, window_secs: u64) -> String {
    format!("{}:{}:{}", key_hash(raw_key), model, window_secs)
}

/// key 级累计行 cache_key（boom_rate_limit_cumulative 主键）。
pub fn key_cumulative_cache_key(raw_key: &str, suffix: CumSuffix) -> String {
    format!("kc:{}:{}", key_hash(raw_key), suffix.as_str())
}

/// team 级累计行 cache_key。
pub fn team_cumulative_cache_key(team_id: &str, suffix: CumSuffix) -> String {
    format!("tc:{}:{}", team_id, suffix.as_str())
}

/// boom_rate_limit_cumulative 的 suffix 集（源码 CumulativeKind::suffix）。
/// 注意 cric/ccic/coc 是成本拆分维度，与并发无关。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CumSuffix {
    TotalInputTokens,
    TotalOutputTokens,
    TotalCost,
    TotalRegularInputCost,
    TotalCachedInputCost,
    TotalOutputCost,
    BudgetPeriodStart,
    BudgetPeriodBaseCost,
}

impl CumSuffix {
    pub fn as_str(self) -> &'static str {
        match self {
            CumSuffix::TotalInputTokens => "tin",
            CumSuffix::TotalOutputTokens => "tout",
            CumSuffix::TotalCost => "tcost",
            CumSuffix::TotalRegularInputCost => "cric",
            CumSuffix::TotalCachedInputCost => "ccic",
            CumSuffix::TotalOutputCost => "coc",
            CumSuffix::BudgetPeriodStart => "bpstart",
            CumSuffix::BudgetPeriodBaseCost => "bpbase",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_litellm_shape() {
        // 64 位 hex；已知向量：sha256("sk-test") 的 hex 前缀稳定
        let h = key_hash("sk-test");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(key_hash("sk-test"), key_hash("sk-test"));
        assert_ne!(key_hash("sk-a"), key_hash("sk-b"));
    }
}
