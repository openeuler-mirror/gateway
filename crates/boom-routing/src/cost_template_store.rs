//! Cost template store (boom_cost_template table, owned by this crate).
//!
//! DB is the runtime authority in DB mode; the no-DB (master-key-only) mode
//! seeds the store from YAML `cost_templates`. Model cost-rate derivation
//! (`model_info.cost_template` references) resolves through this store —
//! never through the YAML config, which is only the bootstrap seed.

use boom_config::CostTemplate;
use dashmap::DashMap;
use sqlx::PgPool;

/// Guard-test anchors: the statements below must mention every column.
pub const COST_TEMPLATE_UPSERT_SQL: &str = r#"
    UPDATE boom_cost_template
    SET input_cost_per_million_tokens = $2,
        cached_input_cost_per_million_tokens = $3,
        output_cost_per_million_tokens = $4,
        updated_at = NOW()
    WHERE name = $1
"#;
pub const COST_TEMPLATE_INSERT_SQL: &str = r#"
    INSERT INTO boom_cost_template (
        name, input_cost_per_million_tokens,
        cached_input_cost_per_million_tokens, output_cost_per_million_tokens
    ) VALUES ($1, $2, $3, $4)
"#;
pub const COST_TEMPLATE_SELECT_SQL: &str = r#"
    SELECT name, input_cost_per_million_tokens,
           cached_input_cost_per_million_tokens, output_cost_per_million_tokens,
           created_at, updated_at
    FROM boom_cost_template
    ORDER BY name
"#;

/// Row from boom_cost_template.
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct CostTemplateRow {
    pub name: String,
    pub input_cost_per_million_tokens: Option<f64>,
    pub cached_input_cost_per_million_tokens: Option<f64>,
    pub output_cost_per_million_tokens: Option<f64>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<CostTemplateRow> for CostTemplate {
    fn from(r: CostTemplateRow) -> Self {
        CostTemplate {
            name: r.name,
            input_cost_per_million_tokens: r.input_cost_per_million_tokens,
            cached_input_cost_per_million_tokens: r.cached_input_cost_per_million_tokens,
            output_cost_per_million_tokens: r.output_cost_per_million_tokens,
        }
    }
}

/// In-memory store for billing rate templates. Survives reloads; content is
/// reconciled from the DB (DB mode) or rebuilt from YAML (no-DB mode).
#[derive(Debug, Default)]
pub struct CostTemplateStore {
    templates: DashMap<String, CostTemplate>,
}

impl CostTemplateStore {
    pub fn new() -> Self {
        Self::default()
    }

    // ── In-memory operations ──

    /// Resolve a template by name — the single lookup point behind
    /// `model_info.cost_template` rate derivation.
    pub fn lookup(&self, name: &str) -> Option<CostTemplate> {
        self.templates.get(name).map(|r| r.value().clone())
    }

    pub fn upsert(&self, template: CostTemplate) {
        self.templates.insert(template.name.clone(), template);
    }

    pub fn remove(&self, name: &str) -> bool {
        self.templates.remove(name).is_some()
    }

    pub fn list(&self) -> Vec<CostTemplate> {
        let mut out: Vec<CostTemplate> = self
            .templates
            .iter()
            .map(|r| r.value().clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn clear(&self) {
        self.templates.clear();
    }

    // ── DB operations (boom_routing owns boom_cost_template) ──

    /// Seed from YAML into an EMPTY table (DB first-boot import). Runs on the
    /// transaction connection held by the caller (`seed_if_empty`).
    pub async fn seed_rows_db(
        conn: &mut sqlx::PgConnection,
        templates: &[CostTemplate],
    ) -> Result<usize, sqlx::Error> {
        for t in templates {
            sqlx::query(COST_TEMPLATE_INSERT_SQL)
                .bind(&t.name)
                .bind(t.input_cost_per_million_tokens)
                .bind(t.cached_input_cost_per_million_tokens)
                .bind(t.output_cost_per_million_tokens)
                .execute(&mut *conn)
                .await?;
        }
        if !templates.is_empty() {
            tracing::info!("Seeded {} cost template(s) from YAML into DB", templates.len());
        }
        Ok(templates.len())
    }

    /// Load all rows (reconcile source of truth).
    pub async fn list_all_db(pool: &PgPool) -> Result<Vec<CostTemplateRow>, sqlx::Error> {
        sqlx::query_as::<_, CostTemplateRow>(COST_TEMPLATE_SELECT_SQL)
            .fetch_all(pool)
            .await
    }

    /// Upsert in DB and memory.
    pub async fn upsert_db(&self, pool: &PgPool, template: &CostTemplate) -> Result<(), sqlx::Error> {
        boom_core::gaussdb_upsert!(
            pool,
            || {
                sqlx::query(COST_TEMPLATE_UPSERT_SQL)
                    .bind(&template.name)
                    .bind(template.input_cost_per_million_tokens)
                    .bind(template.cached_input_cost_per_million_tokens)
                    .bind(template.output_cost_per_million_tokens)
            },
            || {
                sqlx::query(COST_TEMPLATE_INSERT_SQL)
                    .bind(&template.name)
                    .bind(template.input_cost_per_million_tokens)
                    .bind(template.cached_input_cost_per_million_tokens)
                    .bind(template.output_cost_per_million_tokens)
            }
        )?;
        self.upsert(template.clone());
        Ok(())
    }

    /// Delete from DB and memory. Returns true if it existed.
    pub async fn delete_db(&self, pool: &PgPool, name: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM boom_cost_template WHERE name = $1")
            .bind(name)
            .execute(pool)
            .await?;
        let existed = result.rows_affected() > 0;
        if existed {
            self.remove(name);
        }
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLUMNS: &[&str] = &[
        "name",
        "input_cost_per_million_tokens",
        "cached_input_cost_per_million_tokens",
        "output_cost_per_million_tokens",
    ];
    const TIMESTAMP_COLUMNS: &[&str] = &["created_at", "updated_at"];

    /// Adding a column without touching every statement must fail the build.
    /// Timestamp columns are DB-managed (DEFAULT NOW()) and only appear in
    /// SELECT.
    #[test]
    fn cost_template_sql_mentions_all_columns() {
        for col in COLUMNS {
            for sql in [
                COST_TEMPLATE_UPSERT_SQL,
                COST_TEMPLATE_INSERT_SQL,
                COST_TEMPLATE_SELECT_SQL,
            ] {
                assert!(
                    sql.contains(col),
                    "column `{col}` missing from a boom_cost_template statement"
                );
            }
        }
        for col in TIMESTAMP_COLUMNS {
            assert!(
                COST_TEMPLATE_SELECT_SQL.contains(col),
                "timestamp column `{col}` missing from the boom_cost_template SELECT"
            );
        }
    }

    #[test]
    fn lookup_resolves_and_remove_clears() {
        let store = CostTemplateStore::new();
        store.upsert(CostTemplate {
            name: "deepseek-v3".into(),
            input_cost_per_million_tokens: Some(0.27),
            cached_input_cost_per_million_tokens: Some(0.014),
            output_cost_per_million_tokens: Some(1.10),
        });
        let t = store.lookup("deepseek-v3").expect("present");
        assert_eq!(t.input_cost_per_million_tokens, Some(0.27));
        assert!(store.lookup("missing").is_none());
        assert!(store.remove("deepseek-v3"));
        assert!(store.lookup("deepseek-v3").is_none());
    }
}
