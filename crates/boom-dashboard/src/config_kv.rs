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

fn value_to_bool(v: serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(b),
        _ => None,
    }
}

/// Read a boolean KV entry. `None` when the key is absent or the value is
/// not a JSON bool (caller decides whether to seed or warn).
pub async fn get_bool(pool: &PgPool, key: &str) -> Result<Option<bool>, sqlx::Error> {
    let v: Option<serde_json::Value> =
        sqlx::query_scalar::<_, serde_json::Value>("SELECT value FROM boom_config WHERE key = $1")
            .bind(key)
            .fetch_optional(pool)
            .await?;
    Ok(v.and_then(value_to_bool))
}

/// Write a boolean KV entry (UPDATE → INSERT race-tolerant upsert).
pub async fn set_bool(pool: &PgPool, key: &str, value: bool) -> Result<(), sqlx::Error> {
    let json = serde_json::Value::Bool(value);
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
    let json = serde_json::Value::Bool(seed);
    let inserted = sqlx::query("INSERT INTO boom_config (key, value) VALUES ($1, $2)")
        .bind(key)
        .bind(&json)
        .execute(pool)
        .await;
    match inserted {
        Ok(_) => {
            tracing::info!(key, seed, "Initialized boom_config KV entry from YAML default");
            Ok(seed)
        }
        Err(_) => {
            // Lost the race to a concurrently-booting instance — adopt
            // whatever it wrote. A genuine non-race failure resurfaces here.
            let v: serde_json::Value =
                sqlx::query_scalar("SELECT value FROM boom_config WHERE key = $1")
                    .bind(key)
                    .fetch_one(pool)
                    .await?;
            Ok(value_to_bool(v).unwrap_or(seed))
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
