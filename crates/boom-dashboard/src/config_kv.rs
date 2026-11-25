//! `boom_config` KV table access — the dashboard-owned home for small
//! DB-authoritative switches that don't warrant a dedicated table.
//!
//! Table shape (migrations.rs, Migration 7/8): `boom_config (key TEXT PRIMARY
//! KEY, value JSONB NOT NULL, updated_at TIMESTAMPTZ DEFAULT NOW())`. Values
//! are stored as bare JSON scalars (`true` / `false` for the bool helpers).
//!
//! Authority model mirrors the user_tag_headers / client_blocklist.enabled
//! precedents: boot/reload `adopt_*` seeds from YAML when the row is absent,
//! after which the DB value is authoritative and the YAML field is inert.
//! Dashboard writes go through `set_*` (DB first, memory second via the
//! caller). GaussDB has no `ON CONFLICT` — upserts use
//! `boom_core::gaussdb_upsert!`.

use sqlx::PgPool;

/// Metrics channel switch (`trace.metrics.enabled` YAML field is seed-only).
pub const TRACE_METRICS_ENABLED: &str = "trace_metrics_enabled";

/// Quota decision base: `true` = shared (admission checks use global base +
/// local increment, bases refreshed from peer pushes at each sync cycle),
/// `false` = local (instances enforce independently; counter deltas still
/// push incrementally for billing convergence in both modes). Default shared.
pub const DECISION_BASE_SHARED: &str = "decision_base_shared";

/// Fallback rate-limit settings for plan-less keys (`rate_limit` YAML
/// section is seed-only). Value is a JSON object with the YAML field
/// names (`enabled` / `default_rpm` / `default_tpm` / `window_limits`) —
/// parsed into `boom_limiter::RateLimitFallback` by the callers. CATEGORY_PLAN
/// broadcast: counters are cluster-shared, so thresholds must be too.
pub const RATE_LIMIT_FALLBACK: &str = "rate_limit_fallback";

/// Router behavior trio (`router_settings` YAML counterparts are
/// seed-only): `enable_priority_header` / `strip_claude_code_attribution`
/// / `forward_client_headers`. Cluster forwarding semantics — parsed back
/// into boom-main's `RouterBehavior` by the callers. CATEGORY_ROUTER_BEHAVIOR
/// broadcast: instances diverging here would treat the same client
/// request differently depending on which instance served it.
pub const ROUTER_BEHAVIOR: &str = "router_behavior";

/// Prompt-log retention policy (`prompt_log.retention` YAML section is
/// seed-only): `enabled` / `max_age_days` / `max_total_gb` /
/// `disk_pct_watermark` — parsed back into
/// `boom_promptlog::RetentionConfig` by the callers. CATEGORY_PROMPTLOG
/// broadcast: the aging policy is an operator business decision enforced
/// uniformly; each instance applies it to its own local segment directory.
/// `enabled` defaults to false — nothing is ever deleted until the
/// operator opts in.
pub const PROMPT_LOG_RETENTION: &str = "prompt_log_retention";

fn value_to_bool(v: serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(b),
        _ => None,
    }
}

/// Read a boolean KV entry. `None` when the key is absent or the value is
/// not a JSON bool (caller decides whether to seed or warn).
pub async fn get_bool(pool: &PgPool, key: &str) -> Result<Option<bool>, sqlx::Error> {
    Ok(get_value(pool, key).await?.and_then(value_to_bool))
}

/// Read an arbitrary JSON KV entry. `None` when the key is absent.
pub async fn get_value(
    pool: &PgPool,
    key: &str,
) -> Result<Option<serde_json::Value>, sqlx::Error> {
    sqlx::query_scalar::<_, serde_json::Value>("SELECT value FROM boom_config WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
}

/// Write a boolean KV entry (UPDATE → INSERT race-tolerant upsert).
pub async fn set_bool(pool: &PgPool, key: &str, value: bool) -> Result<(), sqlx::Error> {
    set_value(pool, key, serde_json::Value::Bool(value)).await
}

/// Write an arbitrary JSON KV entry (UPDATE → INSERT race-tolerant upsert).
pub async fn set_value(
    pool: &PgPool,
    key: &str,
    json: serde_json::Value,
) -> Result<(), sqlx::Error> {
    boom_core::gaussdb_upsert!(
        pool,
        || {
            sqlx::query("UPDATE boom_config SET value = $1, updated_at = NOW() WHERE key = $2")
                .bind(&json)
                .bind(key)
        },
        || {
            sqlx::query("INSERT INTO boom_config (key, value) VALUES ($1, $2)")
                .bind(key)
                .bind(&json)
        }
    )
}

/// Boot/reload reconcile: return the DB value, seeding `seed` (the YAML
/// value) on first boot when the row is absent. A malformed stored value is
/// treated as absent — the next `set_bool` heals it. An insert race with a
/// concurrently-booting instance resolves by re-reading the winner's row.
pub async fn adopt_bool(pool: &PgPool, key: &str, seed: bool) -> Result<bool, sqlx::Error> {
    if let Some(v) = get_bool(pool, key).await? {
        return Ok(v);
    }
    Ok(value_to_bool(adopt_value(pool, key, serde_json::Value::Bool(seed)).await?)
        .unwrap_or(seed))
}

/// JSON-object variant of [`adopt_bool`]: seed the row when absent, return
/// the DB value when present. Callers parse the returned value into their
/// typed struct; a malformed row should be treated by the caller as "use the
/// seed" (the next `set_value` heals it).
pub async fn adopt_value(
    pool: &PgPool,
    key: &str,
    seed: serde_json::Value,
) -> Result<serde_json::Value, sqlx::Error> {
    if let Some(v) = get_value(pool, key).await? {
        return Ok(v);
    }
    let inserted = sqlx::query("INSERT INTO boom_config (key, value) VALUES ($1, $2)")
        .bind(key)
        .bind(&seed)
        .execute(pool)
        .await;
    match inserted {
        Ok(_) => {
            tracing::info!(key, "Initialized boom_config KV entry from YAML default");
            Ok(seed)
        }
        Err(_) => {
            // Lost the race to a concurrently-booting instance — adopt
            // whatever it wrote. A genuine non-race failure resurfaces here.
            sqlx::query_scalar("SELECT value FROM boom_config WHERE key = $1")
                .bind(key)
                .fetch_one(pool)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_to_bool_accepts_only_json_bool() {
        assert_eq!(value_to_bool(serde_json::Value::Bool(true)), Some(true));
        assert_eq!(value_to_bool(serde_json::Value::Bool(false)), Some(false));
        // JSONB stores numbers as i64 — a hand-edited row like `1` or the
        // string "true" must not silently read as enabled.
        assert_eq!(value_to_bool(serde_json::json!(1)), None);
        assert_eq!(value_to_bool(serde_json::json!("true")), None);
        assert_eq!(value_to_bool(serde_json::Value::Null), None);
    }
}
