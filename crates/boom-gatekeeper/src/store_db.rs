//! DB operations for `boom_client_block_rule` — the same dual-source pattern
//! as `boom_routing::AliasStore`: `sync_yaml_to_db` (reload: YAML is truth),
//! `load_db_only` (layer DB-only rows on top), and CRUD methods that write DB
//! then update memory in place (dashboard path — no reload round-trip).

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::BlockRuleStore;
use boom_config::{BlockAction, BlockRule, MatchCondition};

/// Row from `boom_client_block_rule`.
#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct BlockRuleRow {
    pub rule_name: String,
    pub enabled: Option<bool>,
    pub conditions: Option<serde_json::Value>,
    pub action: Option<serde_json::Value>,
    pub source: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl BlockRuleRow {
    /// Decode the JSONB columns into the config types. Fails on legacy/corrupt
    /// rows — callers skip+warn (loader) or 500 (dashboard read path).
    pub fn to_rule(&self) -> Result<BlockRule, String> {
        let conditions: Vec<MatchCondition> = self
            .conditions
            .as_ref()
            .ok_or_else(|| format!("rule '{}': null conditions", self.rule_name))?
            .as_array()
            .ok_or_else(|| format!("rule '{}': conditions is not an array", self.rule_name))?
            .iter()
            .map(|v| {
                serde_json::from_value(v.clone())
                    .map_err(|e| format!("rule '{}': bad condition: {}", self.rule_name, e))
            })
            .collect::<Result<_, _>>()?;
        let action: BlockAction = self
            .action
            .as_ref()
            .ok_or_else(|| format!("rule '{}': null action", self.rule_name))
            .and_then(|v| {
                serde_json::from_value(v.clone())
                    .map_err(|e| format!("rule '{}': bad action: {}", self.rule_name, e))
            })?;
        Ok(BlockRule {
            name: self.rule_name.clone(),
            enabled: self.enabled.unwrap_or(true),
            conditions,
            action,
        })
    }
}

/// Input for creating/updating a rule via the dashboard API.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockRuleInput {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub conditions: Vec<MatchCondition>,
    pub action: BlockAction,
}

fn default_true() -> bool {
    true
}

impl From<&BlockRule> for BlockRuleInput {
    fn from(r: &BlockRule) -> Self {
        Self {
            name: r.name.clone(),
            enabled: r.enabled,
            conditions: r.conditions.clone(),
            action: r.action.clone(),
        }
    }
}

impl BlockRuleStore {
    /// Sync YAML rules to DB: delete `source='yaml'` rows, insert current YAML
    /// rules, delete `source='db'` rows conflicting with YAML names. Mirrors
    /// `AliasStore::sync_yaml_to_db`.
    pub async fn sync_yaml_to_db(pool: &PgPool, yaml_rules: &[BlockRule]) -> Result<(), sqlx::Error> {
        sqlx::query(r#"DELETE FROM boom_client_block_rule WHERE source = 'yaml'"#)
            .execute(pool)
            .await?;

        for rule in yaml_rules {
            sqlx::query(
                r#"INSERT INTO boom_client_block_rule (rule_name, enabled, conditions, action, source)
                   VALUES ($1, $2, $3, $4, 'yaml')"#,
            )
            .bind(&rule.name)
            .bind(rule.enabled)
            .bind(serde_json::to_value(&rule.conditions).unwrap_or(serde_json::Value::Array(vec![])))
            .bind(serde_json::to_value(&rule.action).unwrap_or(serde_json::Value::Null))
            .execute(pool)
            .await?;
        }

        if !yaml_rules.is_empty() {
            let names: Vec<String> = yaml_rules.iter().map(|r| r.name.clone()).collect();
            let result = sqlx::query(
                r#"DELETE FROM boom_client_block_rule WHERE source = 'db' AND rule_name = ANY($1)"#,
            )
            .bind(&names)
            .execute(pool)
            .await?;
            if result.rows_affected() > 0 {
                tracing::info!(
                    "Removed {} conflicting source='db' block rule(s)",
                    result.rows_affected()
                );
            }
        }

        tracing::info!("Synced {} block rule(s) from YAML to DB", yaml_rules.len());
        Ok(())
    }

    /// Load `source='db'` rules from DB, compile, and layer on top of the
    /// YAML-seeded store. Broken rows are skipped with a warn — a corrupt row
    /// must not take down rule loading.
    pub async fn load_db_only(&self, pool: &PgPool) {
        let rows: Vec<BlockRuleRow> = match sqlx::query_as::<_, BlockRuleRow>(
            r#"SELECT rule_name, enabled, conditions, action, source, updated_at
               FROM boom_client_block_rule WHERE source = 'db'"#,
        )
        .fetch_all(pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to load DB-only block rules: {}", e);
                return;
            }
        };

        let mut loaded = 0usize;
        let mut skipped = 0usize;
        for row in &rows {
            let rule = match row.to_rule() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Skip DB block rule: {}", e);
                    skipped += 1;
                    continue;
                }
            };
            match self.insert_rule(&rule) {
                Ok(()) => loaded += 1,
                Err(e) => {
                    tracing::warn!("Skip DB block rule '{}': {}", row.rule_name, e);
                    skipped += 1;
                }
            }
        }
        tracing::info!(
            "Loaded {} DB-only block rule(s){}",
            loaded,
            if skipped > 0 {
                format!(", skipped {}", skipped)
            } else {
                String::new()
            }
        );
    }

    /// List all rules from DB (dashboard table view).
    pub async fn list_all_db(pool: &PgPool) -> Result<Vec<BlockRuleRow>, sqlx::Error> {
        sqlx::query_as::<_, BlockRuleRow>(
            r#"SELECT rule_name, enabled, conditions, action, source, updated_at
               FROM boom_client_block_rule ORDER BY rule_name"#,
        )
        .fetch_all(pool)
        .await
    }

    /// Create or upsert a rule in DB (`source='db'`) and update memory.
    /// The caller (dashboard handler) is expected to have run `compile_rule`
    /// for input validation already; a late compile failure here only skips
    /// the memory update (warned) — the DB row still wins on next reload.
    pub async fn create_db(&self, pool: &PgPool, input: &BlockRuleInput) -> Result<(), sqlx::Error> {
        boom_core::gaussdb_upsert!(
            pool,
            || sqlx::query(
                r#"UPDATE boom_client_block_rule
                   SET enabled = $2, conditions = $3, action = $4, source = 'db', updated_at = NOW()
                   WHERE rule_name = $1"#,
            )
            .bind(&input.name)
            .bind(input.enabled)
            .bind(serde_json::to_value(&input.conditions).unwrap_or(serde_json::Value::Array(vec![])))
            .bind(serde_json::to_value(&input.action).unwrap_or(serde_json::Value::Null)),
            || sqlx::query(
                r#"INSERT INTO boom_client_block_rule (rule_name, enabled, conditions, action, source)
                   VALUES ($1, $2, $3, $4, 'db')"#,
            )
            .bind(&input.name)
            .bind(input.enabled)
            .bind(serde_json::to_value(&input.conditions).unwrap_or(serde_json::Value::Array(vec![])))
            .bind(serde_json::to_value(&input.action).unwrap_or(serde_json::Value::Null))
        )?;

        self.sync_memory_from_input(input);
        Ok(())
    }

    /// Update a rule in DB and memory. Returns false when the rule doesn't exist.
    pub async fn update_db(
        &self,
        pool: &PgPool,
        rule_name: &str,
        input: &BlockRuleInput,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"UPDATE boom_client_block_rule
               SET enabled = $2, conditions = $3, action = $4, updated_at = NOW()
               WHERE rule_name = $1"#,
        )
        .bind(rule_name)
        .bind(input.enabled)
        .bind(serde_json::to_value(&input.conditions).unwrap_or(serde_json::Value::Array(vec![])))
        .bind(serde_json::to_value(&input.action).unwrap_or(serde_json::Value::Null))
        .execute(pool)
        .await?;

        if result.rows_affected() > 0 {
            if rule_name != input.name {
                self.remove(rule_name);
            }
            self.sync_memory_from_input(input);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Delete a rule from DB and memory. Returns true if it existed.
    pub async fn delete_db(&self, pool: &PgPool, rule_name: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(r#"DELETE FROM boom_client_block_rule WHERE rule_name = $1"#)
            .bind(rule_name)
            .execute(pool)
            .await?;
        if result.rows_affected() > 0 {
            self.remove(rule_name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn sync_memory_from_input(&self, input: &BlockRuleInput) {
        let rule = BlockRule {
            name: input.name.clone(),
            enabled: input.enabled,
            conditions: input.conditions.clone(),
            action: input.action.clone(),
        };
        if let Err(e) = self.insert_rule(&rule) {
            tracing::warn!("Block rule '{}' written to DB but skipped in memory: {}", input.name, e);
        }
    }
}
