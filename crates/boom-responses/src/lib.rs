//! Response store for the OpenAI Responses API (`previous_response_id`
//! continuation) — Response.md §6.8, the ADR-02 checkpoint model adapted to
//! two tables (no Conversation API consumer here).
//!
//! Ownership: this crate owns `boom_response` / `boom_response_item` DDL and
//! all access. Rehydration never walks the previous_response_id chain: one
//! row read + one bulk item fetch, order restored in application code.
//!
//! Key isolation: every read is scoped by `key_hash`; a mismatched key is
//! indistinguishable from a missing id (uniform 404, no existence leak).

pub mod migrations;

use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, thiserror::Error)]
pub enum ResponseStoreError {
    #[error("response store is not configured (no database)")]
    Disabled,
    #[error("response not found")]
    NotFound,
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

pub type StoreResult<T> = Result<T, ResponseStoreError>;

/// A rehydrated continuation point: ordered history items (Responses shape,
/// ready to prepend to the next request's input) plus the prior turn's tools
/// (offered for inheritance when the client omits `tools`). `instructions`
/// are NOT auto-inherited (OpenAI semantics, Response.md §6.8).
#[derive(Debug, Clone)]
pub struct RehydratedTurn {
    pub items: Vec<Value>,
    pub tools: Option<Vec<Value>>,
}

/// One completed turn to persist. `items` is the FULL ordered history for the
/// NEXT continuation point: prior rehydrated items ‖ current input ‖ current
/// output. Items lacking an `id` get one assigned (`it_*`); output items keep
/// their `msg_/fc_/rs_` ids so client replay and storage agree.
#[derive(Debug)]
pub struct PersistTurn {
    pub response_id: String,
    pub previous_response_id: Option<String>,
    pub key_hash: String,
    pub model: String,
    pub instructions: Option<String>,
    /// Effective tools actually offered this turn (request tools, or the
    /// inherited set when the client omitted them).
    pub tools: Option<Vec<Value>>,
    pub items: Vec<Value>,
    /// "completed" | "incomplete" (callers skip failed turns).
    pub status: String,
    /// The final ResponseObject, served verbatim by GET /v1/responses/{id}.
    pub response_json: Value,
    pub ttl_secs: u64,
    /// History bound; when exceeded the OLDEST items are dropped (trailing
    /// slice kept) so continuation checkpoints stay bounded.
    pub max_history_items: usize,
}

/// DB-backed response store. `pool: None` → every method reports `Disabled`;
/// the handler surfaces that as the P0-style 400 for `previous_response_id`.
#[derive(Clone)]
pub struct ResponseStore {
    pool: Option<PgPool>,
    /// Last-seen tunables from the boom_config general_settings row,
    /// refreshed by every mirror sync (boot / reload). Reads stay cheap and
    /// lock-free.
    settings: std::sync::Arc<std::sync::RwLock<ResponsesStoreSettings>>,
}

impl ResponseStore {
    pub fn new(pool: Option<PgPool>) -> Self {
        Self { pool, settings: Default::default() }
    }

    pub fn disabled() -> Self {
        Self {
            pool: None,
            settings: Default::default(),
        }
    }

    /// Current tunables (last mirror sync's view).
    pub fn settings(&self) -> ResponsesStoreSettings {
        self.settings.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Refresh tunables from a boom_config general_settings row value.
    pub fn update_settings(&self, row: &serde_json::Value) {
        let parsed = ResponsesStoreSettings::from_general_settings_row(row);
        if let Ok(mut g) = self.settings.write() {
            *g = parsed;
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.pool.is_some()
    }

    /// Load the continuation checkpoint for `response_id` scoped to `key_hash`.
    pub async fn rehydrate(&self, response_id: &str, key_hash: &str) -> StoreResult<RehydratedTurn> {
        let pool = self.pool.as_ref().ok_or(ResponseStoreError::Disabled)?;
        let row = sqlx::query_as::<_, (serde_json::Value, Option<Value>)>(
            "SELECT history_item_ids, tools_json FROM boom_response \
             WHERE id = $1 AND key_hash = $2 AND expires_at > NOW()",
        )
        .bind(response_id)
        .bind(key_hash)
        .fetch_optional(pool)
        .await?
        .ok_or(ResponseStoreError::NotFound)?;

        let history_ids: Vec<String> = row
            .0
            .as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Bulk fetch, order restored in application code (ADR-02 hot path).
        let item_rows = sqlx::query_as::<_, (String, Value)>(
            "SELECT id, data FROM boom_response_item WHERE response_id = $1",
        )
        .bind(response_id)
        .fetch_all(pool)
        .await?;
        let by_id: std::collections::HashMap<String, Value> =
            item_rows.into_iter().collect();

        let items = history_ids
            .iter()
            .filter_map(|id| by_id.get(id).cloned())
            .collect::<Vec<_>>();

        let tools = row.1.and_then(|v| v.as_array().cloned());
        Ok(RehydratedTurn { items, tools })
    }

    /// Store a completed turn (checkpoint row + item rows) in one transaction.
    pub async fn persist_turn(&self, turn: PersistTurn) -> StoreResult<()> {
        let pool = self.pool.as_ref().ok_or(ResponseStoreError::Disabled)?;

        // Bound the checkpoint: keep the trailing max_history_items.
        let mut items = turn.items;
        let max = turn.max_history_items.max(1);
        if items.len() > max {
            let drop = items.len() - max;
            items.drain(..drop);
        }

        let mut history_ids: Vec<Value> = Vec::with_capacity(items.len());
        for item in &mut items {
            if item.get("id").and_then(|v| v.as_str()).unwrap_or_default().is_empty() {
                item["id"] = Value::String(format!("it_{}", uuid::Uuid::new_v4().simple()));
            }
            history_ids.push(Value::String(
                item["id"].as_str().unwrap_or_default().to_string(),
            ));
        }

        let mut tx = pool.begin().await?;
        // Vec<Value> would bind as jsonb[] — wrap into one JSONB value.
        let tools_json: Option<Value> = turn.tools.as_ref().map(|t| serde_json::json!(t));
        sqlx::query(
            "INSERT INTO boom_response \
             (id, previous_response_id, key_hash, model, instructions, tools_json, \
              history_item_ids, status, response_json, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW() + ($10::bigint * INTERVAL '1 second'))",
        )
        .bind(&turn.response_id)
        .bind(&turn.previous_response_id)
        .bind(&turn.key_hash)
        .bind(&turn.model)
        .bind(&turn.instructions)
        .bind(tools_json)
        .bind(serde_json::json!(history_ids))
        .bind(&turn.status)
        .bind(&turn.response_json)
        .bind(turn.ttl_secs as i64)
        .execute(&mut *tx)
        .await?;

        for (seq, item) in items.iter().enumerate() {
            sqlx::query(
                "INSERT INTO boom_response_item (id, response_id, seq, data) VALUES ($1, $2, $3, $4)",
            )
            .bind(item["id"].as_str().unwrap_or_default())
            .bind(&turn.response_id)
            .bind(seq as i32)
            .bind(item)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Fetch a stored response's final object (GET /v1/responses/{id}).
    pub async fn get(&self, response_id: &str, key_hash: &str) -> StoreResult<Option<Value>> {
        let pool = self.pool.as_ref().ok_or(ResponseStoreError::Disabled)?;
        let row = sqlx::query_as::<_, (Value,)>(
            "SELECT response_json FROM boom_response \
             WHERE id = $1 AND key_hash = $2 AND expires_at > NOW()",
        )
        .bind(response_id)
        .bind(key_hash)
        .fetch_optional(pool)
        .await?;
        Ok(row.map(|(json,)| json))
    }

    /// Drop expired checkpoints (items cascade). Returns rows removed.
    pub async fn prune_expired(&self) -> StoreResult<u64> {
        let pool = self.pool.as_ref().ok_or(ResponseStoreError::Disabled)?;
        let result = sqlx::query("DELETE FROM boom_response WHERE expires_at < NOW()")
            .execute(pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// GaussDB runs a PG 9.2 kernel: SQL written against modern PostgreSQL
    /// compiles fine on a local native PG and then fails at runtime in
    /// production (fd97457: the upsert conflict clause; 2026-10: the PG 9.4
    /// interval-builder function in the persist INSERT). This self-scan
    /// fails the build if any construct the 9.2 kernel lacks is
    /// reintroduced. Upserts must use `boom_core::gaussdb_upsert!`; interval
    /// math must use `INTERVAL '..'` literals.
    #[test]
    fn sql_avoids_gaussdb_missing_constructs() {
        let src = include_str!("lib.rs");
        // Assembled at runtime so this test's own source doesn't trip the scan.
        let banned: [String; 4] = [
            ["make_", "interval"].concat(),
            ["ON ", "CONFLICT"].concat(),
            ["to_", "jsonb("].concat(),
            ["gen_", "random_uuid"].concat(),
        ];
        for b in &banned {
            assert!(
                !src.contains(b.as_str()),
                "SQL construct `{b}` is not available on the GaussDB (PG 9.2) \
                 deployment target — use boom_core::gaussdb_upsert! for upserts \
                 and `INTERVAL '..'` literals for interval math",
            );
        }
    }

    /// DB-backed tests run only when a test database is provided; otherwise
    /// they soft-skip (local runs without PG stay green).
    async fn test_pool() -> Option<PgPool> {
        let url = std::env::var("BOOM_RESPONSES_TEST_DATABASE_URL").ok()?;
        match sqlx::PgPool::connect(&url).await {
            Ok(pool) => {
                migrations::run(&pool).await.expect("migrations");
                // Each run starts from a clean slate for its own ids.
                Some(pool)
            }
            Err(e) => {
                eprintln!("BOOM_RESPONSES_TEST_DATABASE_URL set but unreachable: {e}");
                None
            }
        }
    }

    fn turn(response_id: &str, key: &str, items: Vec<Value>) -> PersistTurn {
        PersistTurn {
            response_id: response_id.to_string(),
            previous_response_id: None,
            key_hash: key.to_string(),
            model: "m".to_string(),
            instructions: Some("sys".to_string()),
            tools: Some(vec![json!({ "type": "function", "name": "f" })]),
            items,
            status: "completed".to_string(),
            response_json: json!({ "id": response_id, "status": "completed" }),
            ttl_secs: 3600,
            max_history_items: 1000,
        }
    }

    #[tokio::test]
    async fn persist_rehydrate_get_roundtrip_in_order() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping (no BOOM_RESPONSES_TEST_DATABASE_URL)");
            return;
        };
        let store = ResponseStore::new(Some(pool));
        let id = format!("resp_test_{}", uuid::Uuid::new_v4().simple());
        store
            .persist_turn(turn(&id, "key-a", vec![
                json!({ "type": "message", "role": "user", "content": "q1" }),
                json!({ "type": "message", "role": "assistant", "content": "a1", "id": "msg_fixed1" }),
                json!({ "type": "message", "role": "user", "content": "q2" }),
            ]))
            .await
            .unwrap();

        let hydrated = store.rehydrate(&id, "key-a").await.unwrap();
        let texts: Vec<&str> = hydrated
            .items
            .iter()
            .map(|i| i["content"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(texts, vec!["q1", "a1", "q2"]); // order restored, not seq-luck
        assert_eq!(hydrated.tools.as_ref().unwrap()[0]["name"], "f");
        // Existing ids preserved (client replay and storage agree).
        assert!(hydrated.items.iter().any(|i| i["id"] == "msg_fixed1"));

        let got = store.get(&id, "key-a").await.unwrap().unwrap();
        assert_eq!(got["id"], json!(id));
    }

    #[tokio::test]
    async fn cross_key_and_missing_are_uniform_not_found() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping (no BOOM_RESPONSES_TEST_DATABASE_URL)");
            return;
        };
        let store = ResponseStore::new(Some(pool));
        let id = format!("resp_test_{}", uuid::Uuid::new_v4().simple());
        store
            .persist_turn(turn(&id, "key-a", vec![json!({ "type": "message", "role": "user", "content": "x" })]))
            .await
            .unwrap();

        assert!(matches!(
            store.rehydrate(&id, "key-b").await.unwrap_err(),
            ResponseStoreError::NotFound
        ));
        assert!(store.get(&id, "key-b").await.unwrap().is_none());
        assert!(matches!(
            store.rehydrate("resp_nope", "key-a").await.unwrap_err(),
            ResponseStoreError::NotFound
        ));
    }

    #[tokio::test]
    async fn ttl_expiry_and_prune() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping (no BOOM_RESPONSES_TEST_DATABASE_URL)");
            return;
        };
        let store = ResponseStore::new(Some(pool));
        let id = format!("resp_test_{}", uuid::Uuid::new_v4().simple());
        let mut t = turn(&id, "key-a", vec![json!({ "type": "message", "role": "user", "content": "x" })]);
        t.ttl_secs = 0; // expires immediately
        store.persist_turn(t).await.unwrap();
        assert!(matches!(
            store.rehydrate(&id, "key-a").await.unwrap_err(),
            ResponseStoreError::NotFound
        ));
        let removed = store.prune_expired().await.unwrap();
        assert!(removed >= 1);
    }

    #[tokio::test]
    async fn history_cap_keeps_trailing_items() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipping (no BOOM_RESPONSES_TEST_DATABASE_URL)");
            return;
        };
        let store = ResponseStore::new(Some(pool));
        let id = format!("resp_test_{}", uuid::Uuid::new_v4().simple());
        let items: Vec<Value> = (0..5)
            .map(|i| json!({ "type": "message", "role": "user", "content": format!("m{i}") }))
            .collect();
        let mut t = turn(&id, "key-a", items);
        t.max_history_items = 2;
        store.persist_turn(t).await.unwrap();
        let hydrated = store.rehydrate(&id, "key-a").await.unwrap();
        let texts: Vec<&str> = hydrated
            .items
            .iter()
            .map(|i| i["content"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(texts, vec!["m3", "m4"]); // oldest dropped
    }

    #[test]
    fn disabled_store_reports_disabled() {
        let store = ResponseStore::disabled();
        assert!(!store.is_enabled());
        assert!(matches!(
            tokio_test_block_on(store.rehydrate("x", "k")),
            Err(ResponseStoreError::Disabled)
        ));
    }

    fn tokio_test_block_on<T>(fut: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(fut)
    }
}

/// Response store tunables (Response.md §6.8). Owned HERE (the store's own
/// crate), parsed from the boom_config `general_settings` row's raw JSON —
/// boom-config deliberately has no knowledge of this field.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResponsesStoreSettings {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_ttl")]
    pub ttl_secs: u64,
    #[serde(default = "default_max_items")]
    pub max_history_items: usize,
}
fn default_enabled() -> bool { true }
fn default_ttl() -> u64 { 2_592_000 }
fn default_max_items() -> usize { 1000 }

impl Default for ResponsesStoreSettings {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            ttl_secs: default_ttl(),
            max_history_items: default_max_items(),
        }
    }
}

impl ResponsesStoreSettings {
    /// Parse from a boom_config `general_settings` row value; absent or
    /// malformed `responses_store` ≡ defaults.
    pub fn from_general_settings_row(row: &serde_json::Value) -> Self {
        row.get("responses_store")
            .and_then(|v| serde_json::from_value::<Self>(v.clone()).ok())
            .unwrap_or_default()
    }
}
