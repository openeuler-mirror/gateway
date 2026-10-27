//! DB-authoritative store for the `user_tag_headers` list (which HTTP header
//! names to record into `boom_request_log.user_tag`). Owned by boom-audit
//! because user_tag attribution is part of the request-log pipeline.
//!
//! Lifecycle mirrors `boom_client_blocklist_state`: single-row table, adopted
//! from the YAML default on first boot, DB-authoritative afterwards. In-memory
//! state lives in an `RwLock` (per-request read via `get`), so dashboard
//! updates take effect on the next request without a reload.

use std::sync::RwLock;

use sqlx::PgPool;

/// Parse a JSONB `headers` value (array of strings). Returns `None` on
/// malformed data — callers treat that as "use the empty list" rather than
/// failing the boot.
fn headers_from_json(v: serde_json::Value) -> Option<Vec<String>> {
    serde_json::from_value(v).ok()
}

pub struct UserTagStore {
    headers: RwLock<Vec<String>>,
}

impl UserTagStore {
    pub fn new() -> Self {
        Self {
            headers: RwLock::new(Vec::new()),
        }
    }

    /// Current header-name list (snapshot clone; per-request hot path).
    pub fn get(&self) -> Vec<String> {
        self.headers
            .read()
            .expect("user_tag_headers lock poisoned")
            .clone()
    }

    fn set(&self, headers: Vec<String>) {
        *self.headers.write().expect("user_tag_headers lock poisoned") = headers;
    }

    /// Adopt the header list from `boom_user_tag_config`. When the row is
    /// absent (fresh DB), insert it with the YAML-provided default — after
    /// that the DB is authoritative and the YAML value is only a seed.
    /// Called from the boot/reload reconcile path.
    ///
    /// No `ON CONFLICT` (GaussDB/openGauss don't support it): the insert race
    /// with a concurrently-booting instance is handled by re-reading the row
    /// when the insert loses — same tolerance philosophy as
    /// `boom_core::gaussdb_upsert!`.
    pub async fn adopt_headers_db(
        &self,
        pool: &PgPool,
        yaml_default: Option<Vec<String>>,
    ) -> Result<Vec<String>, sqlx::Error> {
        let current: Option<serde_json::Value> =
            sqlx::query_scalar::<_, serde_json::Value>(
                "SELECT headers FROM boom_user_tag_config WHERE id = 1",
            )
            .fetch_optional(pool)
            .await?;

        let headers = match current {
            Some(v) => match headers_from_json(v) {
                Some(list) => list,
                None => {
                    tracing::warn!(
                        "boom_user_tag_config.headers is malformed — treating as empty list"
                    );
                    Vec::new()
                }
            },
            None => {
                let seed = yaml_default.unwrap_or_default();
                let seed_json =
                    serde_json::to_value(&seed).unwrap_or(serde_json::json!([]));
                let inserted = sqlx::query(
                    "INSERT INTO boom_user_tag_config (id, headers) VALUES (1, $1)",
                )
                .bind(&seed_json)
                .execute(pool)
                .await;
                match inserted {
                    Ok(_) => {
                        tracing::info!(
                            count = seed.len(),
                            "Initialized user_tag_config row from YAML default"
                        );
                        seed
                    }
                    // Lost the race to a concurrently-booting instance —
                    // adopt whatever it wrote. Any non-race error resurfaces
                    // through this read (row absent → fetch_one fails).
                    Err(_) => {
                        let v: serde_json::Value = sqlx::query_scalar(
                            "SELECT headers FROM boom_user_tag_config WHERE id = 1",
                        )
                        .fetch_one(pool)
                        .await?;
                        headers_from_json(v).unwrap_or_default()
                    }
                }
            }
        };
        self.set(headers.clone());
        Ok(headers)
    }

    /// Persist the header list to DB and memory (dashboard write path —
    /// DB first, memory only on success).
    pub async fn set_headers_db(
        &self,
        pool: &PgPool,
        headers: Vec<String>,
    ) -> Result<(), sqlx::Error> {
        let json = serde_json::to_value(&headers).unwrap_or(serde_json::json!([]));
        boom_core::gaussdb_upsert!(
            pool,
            || sqlx::query(
                "UPDATE boom_user_tag_config SET headers = $1, updated_at = NOW() WHERE id = 1",
            )
            .bind(&json),
            || sqlx::query(
                "INSERT INTO boom_user_tag_config (id, headers) VALUES (1, $1)",
            )
            .bind(&json)
        )?;
        self.set(headers);
        Ok(())
    }
}

impl Default for UserTagStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_set_roundtrip() {
        let store = UserTagStore::new();
        assert!(store.get().is_empty());
        store.set(vec!["x-user-id".into(), "x-user-tag".into()]);
        assert_eq!(store.get(), vec!["x-user-id", "x-user-tag"]);
    }

    #[test]
    fn headers_from_json_parses_string_array() {
        let v = serde_json::json!(["a", "b"]);
        assert_eq!(headers_from_json(v).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn headers_from_json_rejects_malformed() {
        assert!(headers_from_json(serde_json::json!(42)).is_none());
        assert!(headers_from_json(serde_json::json!([1, 2])).is_none());
        assert_eq!(
            headers_from_json(serde_json::json!([])).unwrap(),
            Vec::<String>::new()
        );
    }
}
