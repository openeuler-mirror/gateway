use dashmap::DashMap;
use dashmap::DashSet;
use serde::Serialize;
use sqlx::PgPool;

/// Column list for the alias upsert INSERT. Kept as a named const so the
/// manifest cross-check test (tests::alias_sql_columns_match_manifest_fields)
/// checks the SAME string production code executes. See CLAUDE.md §9.
const ALIAS_UPSERT_INSERT_SQL: &str = r#"INSERT INTO boom_model_alias (alias_name, target_model, hidden, source)
                   VALUES ($1, $2, $3, 'db')"#;

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

    /// Seed aliases from YAML into an EMPTY boom_model_alias table (DB
    /// first-boot import). Rows carry source='yaml' provenance. Runs on the
    /// transaction connection held by the caller (`seed_if_empty`).
    pub async fn seed_rows_db(
        conn: &mut sqlx::PgConnection,
        aliases: &[(String, String, bool)],
    ) -> Result<usize, sqlx::Error> {
        for (alias, target, is_hidden) in aliases {
            sqlx::query(
                r#"INSERT INTO boom_model_alias (alias_name, target_model, hidden, source)
                   VALUES ($1, $2, $3, 'yaml')"#,
            )
            .bind(alias)
            .bind(target)
            .bind(is_hidden)
            .execute(&mut *conn)
            .await?;
        }

        tracing::info!("Seeded {} alias(es) from YAML into DB", aliases.len());
        Ok(aliases.len())
    }

    /// Alias names from the given list that already exist in DB — workflow
    /// namespace validation. The DB is the runtime authority, so every row
    /// counts, regardless of source.
    pub async fn alias_name_conflicts(
        pool: &PgPool,
        model_names: &[String],
    ) -> Result<Vec<String>, sqlx::Error> {
        if model_names.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_scalar::<_, String>(
            r#"SELECT DISTINCT alias_name
               FROM boom_model_alias
               WHERE alias_name = ANY($1)
               ORDER BY alias_name"#,
        )
        .bind(model_names)
        .fetch_all(pool)
        .await
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
        boom_core::gaussdb_upsert!(
            pool,
            || sqlx::query(
                r#"UPDATE boom_model_alias
                   SET target_model = $2, hidden = $3, source = 'db', updated_at = NOW()
                   WHERE alias_name = $1"#,
            )
            .bind(&input.alias_name)
            .bind(&input.target_model)
            .bind(input.hidden),
            || sqlx::query(ALIAS_UPSERT_INSERT_SQL)
            .bind(&input.alias_name)
            .bind(&input.target_model)
            .bind(input.hidden)
        )?;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross-crate half of the CLAUDE.md §9 single-source-of-truth contract:
    /// the operator-facing `boom_model_alias` columns and
    /// `boom_config::manifest::alias_fields()` must describe the same field
    /// set. Bookkeeping columns (`source`, `updated_at`) are store-managed
    /// and intentionally unregistered. The manifest side lives in
    /// boom-config (leaf crate, cannot see this store); this test closes the
    /// loop from the SQL side, in both directions:
    /// - manifest field missing from SQL → the dashboard edits a column
    ///   that never persists;
    /// - SQL column missing from manifest → dead config, invisible on the
    ///   dashboard.
    #[test]
    fn alias_sql_columns_match_manifest_fields() {
        fn sql_tokens(sql: &str) -> std::collections::HashSet<&str> {
            sql.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .collect()
        }

        let tokens = sql_tokens(ALIAS_UPSERT_INSERT_SQL);
        let manifest_fields: Vec<&str> = boom_config::manifest::alias_fields()
            .iter()
            .map(|m| m.field)
            .collect();
        for f in &manifest_fields {
            assert!(
                tokens.contains(f),
                "manifest alias field `{}` is not a boom_model_alias column — \
                 either the manifest is wrong or the store SQL is missing it. \
                 See CLAUDE.md §9.",
                f
            );
        }
        for col in ["alias_name", "target_model", "hidden"] {
            assert!(
                manifest_fields.contains(&col),
                "boom_model_alias column `{}` is missing from \
                 manifest::alias_fields() — it is dead config (invisible on \
                 the dashboard). See CLAUDE.md §9.",
                col
            );
        }
    }
}
