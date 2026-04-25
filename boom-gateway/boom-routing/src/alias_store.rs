use dashmap::DashMap;
use dashmap::DashSet;
use serde::Serialize;
use sqlx::PgPool;

/// In-memory store for model aliases.
/// Survives config reloads — updated incrementally via DB or YAML seed.
#[derive(Debug)]
pub struct AliasStore {
    /// alias_name → target_model_name.
    aliases: DashMap<String, String>,
    /// Alias names that should NOT appear in model list.
    hidden: DashSet<String>,
}

/// Row from boom_model_alias table.
#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct AliasRow {
    pub alias_name: String,
    pub target_model: String,
    pub hidden: Option<bool>,
    pub source: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Input for creating/updating an alias.
#[derive(Debug, Clone)]
pub struct AliasInput {
    pub alias_name: String,
    pub target_model: String,
    pub hidden: bool,
}

impl AliasStore {
    pub fn new() -> Self {
        Self {
            aliases: DashMap::new(),
            hidden: DashSet::new(),
        }
    }

    // ── In-memory operations ──

    /// Set or update an alias mapping.
    pub fn set_alias(&self, alias: String, target: String, is_hidden: bool) {
        self.aliases.insert(alias.clone(), target);
        if is_hidden {
            self.hidden.insert(alias);
        } else {
            self.hidden.remove(&alias);
        }
    }

    /// Remove an alias. Returns true if it existed.
    pub fn remove_alias(&self, alias: &str) -> bool {
        self.hidden.remove(alias);
        self.aliases.remove(alias).is_some()
    }

    /// Resolve a model name through alias mapping.
    /// Returns the original name if not an alias.
    pub fn resolve(&self, name: &str) -> Option<String> {
        self.aliases.get(name).map(|r| r.value().clone())
    }

    /// Check if an alias name is hidden.
    pub fn is_hidden(&self, alias: &str) -> bool {
        self.hidden.contains(alias)
    }

    /// Get all alias → target mappings.
    pub fn all_aliases(&self) -> Vec<(String, String)> {
        self.aliases
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    /// Get all model names visible to clients (non-hidden aliases).
    pub fn visible_names(&self) -> Vec<String> {
        self.aliases
            .iter()
            .filter(|r| !self.hidden.contains(r.key()))
            .map(|r| r.key().clone())
            .collect()
    }

    /// Clear all aliases.
    pub fn clear(&self) {
        self.aliases.clear();
        self.hidden.clear();
    }

    /// Number of aliases.
    pub fn len(&self) -> usize {
        self.aliases.len()
    }

    /// Number of hidden aliases.
    pub fn hidden_count(&self) -> usize {
        self.hidden.len()
    }

    // ── DB operations (boom_routing owns boom_model_alias table) ──

    /// Sync YAML aliases to DB: delete source='yaml' rows, insert current YAML aliases,
    /// delete conflicting source='db' rows.
    pub async fn sync_yaml_to_db(
        pool: &PgPool,
        yaml_aliases: &[(String, String, bool)],
    ) -> Result<(), sqlx::Error> {
        // 1. Delete all source='yaml' rows.
        sqlx::query(r#"DELETE FROM boom_model_alias WHERE source = 'yaml'"#)
            .execute(pool)
            .await?;

        // 2. Insert YAML aliases.
        for (alias, target, is_hidden) in yaml_aliases {
            sqlx::query(
                r#"INSERT INTO boom_model_alias (alias_name, target_model, hidden, source)
                   VALUES ($1, $2, $3, 'yaml')"#,
            )
            .bind(alias)
            .bind(target)
            .bind(is_hidden)
            .execute(pool)
            .await?;
        }

        // 3. Delete source='db' aliases that conflict with YAML names.
        if !yaml_aliases.is_empty() {
            let yaml_names: Vec<String> = yaml_aliases.iter().map(|(a, _, _)| a.clone()).collect();
            let result = sqlx::query(
                r#"DELETE FROM boom_model_alias WHERE source = 'db' AND alias_name = ANY($1)"#,
            )
            .bind(&yaml_names)
            .execute(pool)
            .await?;
            if result.rows_affected() > 0 {
                tracing::info!(
                    "Removed {} conflicting source='db' alias(es)",
                    result.rows_affected()
                );
            }
        }

        tracing::info!(
            "Synced {} alias(es) from YAML to DB",
            yaml_aliases.len()
        );
        Ok(())
    }

    /// Load source='db' aliases from DB into this store.
    pub async fn load_db_only(&self, pool: &PgPool) {
        let rows: Vec<AliasRow> = match sqlx::query_as::<_, AliasRow>(
            r#"SELECT alias_name, target_model, hidden, source, updated_at
               FROM boom_model_alias WHERE source = 'db'"#,
        )
        .fetch_all(pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to load DB-only aliases: {}", e);
                return;
            }
        };

        for row in &rows {
            self.set_alias(
                row.alias_name.clone(),
                row.target_model.clone(),
                row.hidden.unwrap_or(false),
            );
        }

        tracing::info!("Loaded {} DB-only alias(es)", rows.len());
    }

    /// List all aliases from DB (for dashboard).
    pub async fn list_all_db(pool: &PgPool) -> Result<Vec<AliasRow>, sqlx::Error> {
        sqlx::query_as::<_, AliasRow>(
            r#"SELECT alias_name, target_model, hidden, source, updated_at
               FROM boom_model_alias ORDER BY alias_name"#,
        )
        .fetch_all(pool)
        .await
    }

    /// Create or upsert an alias in DB (source='db') and update memory.
    pub async fn create_db(&self, pool: &PgPool, input: &AliasInput) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"INSERT INTO boom_model_alias (alias_name, target_model, hidden, source)
               VALUES ($1, $2, $3, 'db')
               ON CONFLICT (alias_name) DO UPDATE
               SET target_model = EXCLUDED.target_model,
                   hidden = EXCLUDED.hidden,
                   source = 'db',
                   updated_at = NOW()"#,
        )
        .bind(&input.alias_name)
        .bind(&input.target_model)
        .bind(input.hidden)
        .execute(pool)
        .await?;

        self.set_alias(
            input.alias_name.clone(),
            input.target_model.clone(),
            input.hidden,
        );
        Ok(())
    }

    /// Update an alias in DB and memory.
    pub async fn update_db(&self, pool: &PgPool, alias_name: &str, input: &AliasInput) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"UPDATE boom_model_alias
               SET target_model = $2, hidden = $3, updated_at = NOW()
               WHERE alias_name = $1"#,
        )
        .bind(alias_name)
        .bind(&input.target_model)
        .bind(input.hidden)
        .execute(pool)
        .await?;

        if result.rows_affected() > 0 {
            self.remove_alias(alias_name);
            self.set_alias(
                input.alias_name.clone(),
                input.target_model.clone(),
                input.hidden,
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Delete an alias from DB and memory. Returns true if existed.
    pub async fn delete_db(&self, pool: &PgPool, alias_name: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"DELETE FROM boom_model_alias WHERE alias_name = $1"#,
        )
        .bind(alias_name)
        .execute(pool)
        .await?;

        if result.rows_affected() > 0 {
            self.remove_alias(alias_name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Snapshot all aliases from DB (for config export).
    pub async fn snapshot_db(pool: &PgPool) -> Result<Vec<(String, String)>, sqlx::Error> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT alias_name, target_model FROM boom_model_alias ORDER BY alias_name"#,
        )
        .fetch_all(pool)
        .await?;
        Ok(rows)
    }
}

impl Default for AliasStore {
    fn default() -> Self {
        Self::new()
    }
}
