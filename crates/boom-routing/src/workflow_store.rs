//! Workflow definition + model-binding store (boom_workflow_definition /
//! boom_workflow_model tables, owned by this crate).
//!
//! DB is the runtime authority in DB mode; the no-DB (master-key-only) mode
//! seeds the store from YAML `workflow_settings`. The store's `settings()`
//! snapshot is what fusion registration consumes — the YAML config's copy is
//! only the bootstrap seed.

use boom_config::{WorkflowDefinitionConfig, WorkflowSettings};
use dashmap::DashMap;
use sqlx::PgPool;

/// Guard-test anchors: the SQL statements below must mention every column of
/// their table, so adding a column without touching all statements fails the
/// build (see `tests` below).
pub const WORKFLOW_DEFINITION_UPSERT_SQL: &str = r#"
    UPDATE boom_workflow_definition
    SET definition = $2, updated_at = NOW()
    WHERE name = $1
"#;
pub const WORKFLOW_DEFINITION_INSERT_SQL: &str = r#"
    INSERT INTO boom_workflow_definition (name, definition)
    VALUES ($1, $2)
"#;
pub const WORKFLOW_DEFINITION_SELECT_SQL: &str = r#"
    SELECT name, definition, created_at, updated_at
    FROM boom_workflow_definition
    ORDER BY name
"#;
pub const WORKFLOW_MODEL_UPSERT_SQL: &str = r#"
    UPDATE boom_workflow_model
    SET workflow_name = $2, updated_at = NOW()
    WHERE model_name = $1
"#;
pub const WORKFLOW_MODEL_INSERT_SQL: &str = r#"
    INSERT INTO boom_workflow_model (model_name, workflow_name)
    VALUES ($1, $2)
"#;
pub const WORKFLOW_MODEL_SELECT_SQL: &str = r#"
    SELECT model_name, workflow_name, created_at, updated_at
    FROM boom_workflow_model
    ORDER BY model_name
"#;

/// Row from boom_workflow_definition.
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct WorkflowDefinitionRow {
    pub name: String,
    pub definition: serde_json::Value,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Row from boom_workflow_model.
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct WorkflowModelRow {
    pub model_name: String,
    pub workflow_name: String,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// In-memory store for workflow definitions and virtual-model bindings.
/// Survives config reloads — content is reconciled from the DB (DB mode) or
/// rebuilt from YAML (no-DB mode), the same lifecycle as DeploymentStore.
#[derive(Debug, Default)]
pub struct WorkflowStore {
    /// workflow name → definition.
    definitions: DashMap<String, WorkflowDefinitionConfig>,
    /// virtual model name → workflow name.
    models: DashMap<String, String>,
}

impl WorkflowStore {
    pub fn new() -> Self {
        Self::default()
    }

    // ── In-memory operations ──

    /// Snapshot as a `WorkflowSettings` — the shape fusion registration and
    /// validation consume. Cheap (workflows are few).
    pub fn settings(&self) -> WorkflowSettings {
        WorkflowSettings {
            models: self
                .models
                .iter()
                .map(|r| (r.key().clone(), r.value().clone()))
                .collect(),
            workflows: self
                .definitions
                .iter()
                .map(|r| (r.key().clone(), r.value().clone()))
                .collect(),
        }
    }

    pub fn upsert_definition(&self, name: &str, definition: WorkflowDefinitionConfig) {
        self.definitions.insert(name.to_string(), definition);
    }

    /// Remove a definition. Bindings referencing it are the caller's concern
    /// (delete rejects while bindings exist).
    pub fn remove_definition(&self, name: &str) -> bool {
        self.definitions.remove(name).is_some()
    }

    pub fn upsert_model(&self, model: &str, workflow_name: &str) {
        self.models
            .insert(model.to_string(), workflow_name.to_string());
    }

    pub fn remove_model(&self, model: &str) -> bool {
        self.models.remove(model).is_some()
    }

    pub fn contains_model(&self, model: &str) -> bool {
        self.models.contains_key(model)
    }

    /// All virtual model names (workflow bindings).
    pub fn model_names(&self) -> Vec<String> {
        self.models.iter().map(|r| r.key().clone()).collect()
    }

    /// All definition names.
    pub fn definition_names(&self) -> Vec<String> {
        self.definitions.iter().map(|r| r.key().clone()).collect()
    }

    /// Workflow definition ids whose panel or aggregator references `name`.
    /// Guards destructive operations on models/aliases: deleting a referenced
    /// child would fail the next `validate_with_namespaces` and abort
    /// re-registration, so callers must reject the delete up front.
    pub fn workflows_referencing(&self, name: &str) -> Vec<String> {
        self.definitions
            .iter()
            .filter(|entry| {
                let definition = entry.value();
                match definition {
                    WorkflowDefinitionConfig::DirectSynthesis { roles, .. } => roles
                        .panel
                        .iter()
                        .map(|i| i.model.as_str())
                        .chain(std::iter::once(roles.aggregator.model.as_str()))
                        .any(|m| m == name),
                }
            })
            .map(|entry| entry.key().clone())
            .collect()
    }

    pub fn clear(&self) {
        self.definitions.clear();
        self.models.clear();
    }

    // ── DB operations (boom_routing owns the two workflow tables) ──

    /// Seed both tables from a YAML `workflow_settings` section into EMPTY
    /// tables (DB first-boot import). Runs on the transaction connection held
    /// by the caller (`seed_if_empty`).
    pub async fn seed_rows_db(
        conn: &mut sqlx::PgConnection,
        settings: &WorkflowSettings,
    ) -> Result<usize, sqlx::Error> {
        for (name, definition) in &settings.workflows {
            let definition = serde_json::to_value(definition).map_err(|e| {
                sqlx::Error::Encode(format!("serialize workflow '{name}': {e}").into())
            })?;
            sqlx::query(WORKFLOW_DEFINITION_INSERT_SQL)
                .bind(name)
                .bind(definition)
                .execute(&mut *conn)
                .await?;
        }
        for (model, workflow_name) in &settings.models {
            sqlx::query(WORKFLOW_MODEL_INSERT_SQL)
                .bind(model)
                .bind(workflow_name)
                .execute(&mut *conn)
                .await?;
        }
        let n = settings.workflows.len() + settings.models.len();
        if n > 0 {
            tracing::info!("Seeded {n} workflow row(s) from YAML into DB");
        }
        Ok(n)
    }

    /// Load all definition rows (reconcile source of truth).
    pub async fn list_definitions_db(pool: &PgPool) -> Result<Vec<WorkflowDefinitionRow>, sqlx::Error> {
        sqlx::query_as::<_, WorkflowDefinitionRow>(WORKFLOW_DEFINITION_SELECT_SQL)
            .fetch_all(pool)
            .await
    }

    /// Load all model-binding rows.
    pub async fn list_models_db(pool: &PgPool) -> Result<Vec<WorkflowModelRow>, sqlx::Error> {
        sqlx::query_as::<_, WorkflowModelRow>(WORKFLOW_MODEL_SELECT_SQL)
            .fetch_all(pool)
            .await
    }

    /// Upsert a definition in DB and memory.
    pub async fn upsert_definition_db(
        &self,
        pool: &PgPool,
        name: &str,
        definition: &WorkflowDefinitionConfig,
    ) -> Result<(), sqlx::Error> {
        let value = serde_json::to_value(definition).map_err(|e| {
            sqlx::Error::Encode(format!("serialize workflow '{name}': {e}").into())
        })?;
        boom_core::gaussdb_upsert!(
            pool,
            || sqlx::query(WORKFLOW_DEFINITION_UPSERT_SQL).bind(name).bind(&value),
            || sqlx::query(WORKFLOW_DEFINITION_INSERT_SQL).bind(name).bind(&value)
        )?;
        self.upsert_definition(name, definition.clone());
        Ok(())
    }

    /// Delete a definition from DB and memory. Bindings referencing it must
    /// already be gone (enforced by the CRUD handler).
    pub async fn delete_definition_db(&self, pool: &PgPool, name: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM boom_workflow_definition WHERE name = $1")
            .bind(name)
            .execute(pool)
            .await?;
        let existed = result.rows_affected() > 0;
        if existed {
            self.remove_definition(name);
        }
        Ok(existed)
    }

    /// Upsert a virtual-model binding in DB and memory.
    pub async fn upsert_model_db(
        &self,
        pool: &PgPool,
        model: &str,
        workflow_name: &str,
    ) -> Result<(), sqlx::Error> {
        boom_core::gaussdb_upsert!(
            pool,
            || sqlx::query(WORKFLOW_MODEL_UPSERT_SQL).bind(model).bind(workflow_name),
            || sqlx::query(WORKFLOW_MODEL_INSERT_SQL).bind(model).bind(workflow_name)
        )?;
        self.upsert_model(model, workflow_name);
        Ok(())
    }

    /// Delete a virtual-model binding from DB and memory.
    pub async fn delete_model_db(&self, pool: &PgPool, model: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM boom_workflow_model WHERE model_name = $1")
            .bind(model)
            .execute(pool)
            .await?;
        let existed = result.rows_affected() > 0;
        if existed {
            self.remove_model(model);
        }
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFINITION_COLUMNS: &[&str] = &["name", "definition"];
    const MODEL_COLUMNS: &[&str] = &["model_name", "workflow_name"];
    const TIMESTAMP_COLUMNS: &[&str] = &["created_at", "updated_at"];

    /// Adding a column to a workflow table without touching every statement
    /// must fail the build — same guard pattern as DEPLOYMENT_CORE_COLUMNS.
    /// Timestamp columns are DB-managed (DEFAULT NOW()) and only appear in
    /// SELECT statements.
    #[test]
    fn workflow_sql_mentions_all_columns() {
        for sql in [
            WORKFLOW_DEFINITION_UPSERT_SQL,
            WORKFLOW_DEFINITION_INSERT_SQL,
            WORKFLOW_DEFINITION_SELECT_SQL,
        ] {
            for col in DEFINITION_COLUMNS {
                assert!(
                    sql.contains(col),
                    "column `{col}` missing from a boom_workflow_definition statement"
                );
            }
        }
        for sql in [
            WORKFLOW_MODEL_UPSERT_SQL,
            WORKFLOW_MODEL_INSERT_SQL,
            WORKFLOW_MODEL_SELECT_SQL,
        ] {
            for col in MODEL_COLUMNS {
                assert!(
                    sql.contains(col),
                    "column `{col}` missing from a boom_workflow_model statement"
                );
            }
        }
        for col in TIMESTAMP_COLUMNS {
            assert!(
                WORKFLOW_DEFINITION_SELECT_SQL.contains(col)
                    && WORKFLOW_MODEL_SELECT_SQL.contains(col),
                "timestamp column `{col}` missing from a workflow SELECT"
            );
        }
    }

    #[test]
    fn settings_snapshot_roundtrips_store_contents() {
        let store = WorkflowStore::new();
        let mut settings = WorkflowSettings::default();
        settings.models.insert("fusion".into(), "ds".into());
        settings
            .workflows
            .insert("ds".into(), sample_definition());
        // Seed memory the way load/reconcile does.
        for (n, d) in &settings.workflows {
            store.upsert_definition(n, d.clone());
        }
        for (m, w) in &settings.models {
            store.upsert_model(m, w);
        }
        assert_eq!(store.settings().models, settings.models);
        assert!(store.contains_model("fusion"));
        assert_eq!(store.model_names(), vec!["fusion".to_string()]);
        assert_eq!(store.definition_names(), vec!["ds".to_string()]);

        store.remove_model("fusion");
        assert!(!store.contains_model("fusion"));
        assert!(store.remove_definition("ds"));
        assert!(store.settings().workflows.is_empty());
    }

    fn sample_definition() -> WorkflowDefinitionConfig {
        serde_yaml::from_str(
            r#"
type: direct_synthesis
roles:
  panel:
    - model: glm-5.2
      temperature: 0.3
    - model: glm-5.2
  aggregator:
    model: glm-5.2
"#,
        )
        .unwrap()
    }
}
