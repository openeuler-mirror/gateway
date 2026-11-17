//! DB fixture：seed litellm key / plan / assignment + 可选 decision_base_shared KV。
//! 纯 SQL 直写（psql -f，ON_ERROR_STOP=1），SQL 工件留在 runs/<id>/configs/seed.sql 供取证。
//! 时序：网关 boot 迁移建表之后执行。key 无需 bump（boom-auth 每请求直查 DB，60s 缓存）；
//! plan/assignment 需 revision bump 让两实例 watcher 对账（≤5s）。

use crate::postgres::PostgresInstance;
use crate::EnvParams;
use e2e_tools::keys::key_hash;
use std::path::Path;

/// 主计量 key：不挂 plan（无限制），MI-001 等常规用例用。
pub const KEY_MAIN: &str = "sk-e2e-key-main";
/// 预热探针 key：挂 total_token_limit=1 的 plan，plan 执行就绪探测用（首个请求即 429）。
pub const KEY_WARMUP: &str = "sk-e2e-key-warmup";
pub const PLAN_WARMUP_TTL1: &str = "e2e-warmup-ttl1";

pub fn seed(pg: &PostgresInstance, run_root: &Path, params: &EnvParams) -> anyhow::Result<()> {
    let sql = build_seed_sql(params.decision_base_shared);
    let path = run_root.join("configs").join("seed.sql");
    std::fs::write(&path, &sql)?;
    pg.psql_file(&path)?;
    Ok(())
}

fn build_seed_sql(decision_base_shared: bool) -> String {
    let mut s = String::new();

    for (raw, name) in [(KEY_MAIN, "e2e-main"), (KEY_WARMUP, "e2e-warmup")] {
        s.push_str(&format!(
            "INSERT INTO boom_verification_token (token, key_name, user_id, models, spend)\nVALUES ('{}', '{}', 'e2e-user', '{{}}', 0.0)\nON CONFLICT (token) DO NOTHING;\n\n",
            key_hash(raw),
            name
        ));
    }

    s.push_str(&format!(
        "INSERT INTO boom_rate_limit_plan (name, type, total_token_limit, window_limits, schedule, is_default, source)\nVALUES ('{}', 'key', 1, '[]'::jsonb, '[]'::jsonb, false, 'db')\nON CONFLICT (name) DO NOTHING;\n\n",
        PLAN_WARMUP_TTL1
    ));
    s.push_str(&format!(
        "INSERT INTO boom_key_plan_assignment (key_hash, plan_name)\nVALUES ('{}', '{}')\nON CONFLICT (key_hash) DO NOTHING;\n\n",
        key_hash(KEY_WARMUP),
        PLAN_WARMUP_TTL1
    ));

    if decision_base_shared {
        s.push_str("INSERT INTO boom_config (key, value)\nVALUES ('decision_base_shared', 'true'::jsonb)\nON CONFLICT (key) DO UPDATE SET value = 'true'::jsonb, updated_at = NOW();\n\n");
    }

    // revision bump：与 boom_state::revision::bump 同语义（UPDATE→0 行则 INSERT 1）。
    // 多余的 bump 无害（对账幂等）。
    s.push_str(&bump_categories_sql(&["plan", "assignment", "quota"]));
    s
}

/// revision bump SQL（与 boom_state::revision::bump 同语义：UPDATE→0 行则 INSERT 1）。
/// 用例级 seed 后调 bump 让两实例 watcher 对账（≤5s）。
pub fn bump_categories_sql(cats: &[&str]) -> String {
    let mut s = String::new();
    for cat in cats {
        s.push_str(&format!(
            "UPDATE boom_config_revision SET revision = revision + 1 WHERE category = '{cat}';\n\
             INSERT INTO boom_config_revision (category, revision)\n\
             SELECT '{cat}', 1 WHERE NOT EXISTS (SELECT 1 FROM boom_config_revision WHERE category = '{cat}');\n\n"
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_sql_contains_fixture_anchors() {
        let sql = build_seed_sql(true);
        assert!(sql.contains(&key_hash(KEY_MAIN)));
        assert!(sql.contains("total_token_limit"));
        assert!(sql.contains("'decision_base_shared', 'true'::jsonb"));
        assert!(sql.contains("category = 'plan'"));
    }

    #[test]
    fn local_mode_omits_shared_kv() {
        let sql = build_seed_sql(false);
        assert!(!sql.contains("decision_base_shared"));
    }
}
