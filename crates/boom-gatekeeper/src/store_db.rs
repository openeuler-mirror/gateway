//! DB operations for `boom_client_block_rule` and the blocklist's global
//! `enabled` switch (`boom_client_blocklist_state`, single row) — the DB is
//! the runtime authority: `seed_rows_db` (first-boot import from YAML into
//! an empty table), CRUD methods that write DB then update memory in place
//! (dashboard path — no reload round-trip), and `adopt_enabled_db` (boot:
//! read the switch, seeding the row from the YAML default when absent).

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
    /// Seed rules from YAML into an EMPTY boom_client_block_rule table (DB
    /// first-boot import). Rows carry source='yaml' provenance. Runs on the
    /// transaction connection held by the caller (`seed_if_empty`).
    pub async fn seed_rows_db(
        conn: &mut sqlx::PgConnection,
        rules: &[BlockRule],
    ) -> Result<usize, sqlx::Error> {
        for rule in rules {
            sqlx::query(
                r#"INSERT INTO boom_client_block_rule (rule_name, enabled, conditions, action, source)
                   VALUES ($1, $2, $3, $4, 'yaml')"#,
            )
            .bind(&rule.name)
            .bind(rule.enabled)
            .bind(serde_json::to_value(&rule.conditions).unwrap_or(serde_json::Value::Array(vec![])))
            .bind(serde_json::to_value(&rule.action).unwrap_or(serde_json::Value::Null))
            .execute(&mut *conn)
            .await?;
        }

        tracing::info!("Seeded {} block rule(s) from YAML into DB", rules.len());
        Ok(rules.len())
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

    /// Adopt the global `enabled` flag from `boom_client_blocklist_state`.
    /// When the row is absent (fresh DB), insert it with the YAML-provided
    /// default — after that the DB is authoritative and the YAML section is
    /// only a seed. Called from the boot/reload reconcile path.
    pub async fn adopt_enabled_db(&self, pool: &PgPool, yaml_default: bool) -> Result<bool, sqlx::Error> {
        let current: Option<bool> = sqlx::query_scalar::<_, bool>(
            r#"SELECT enabled FROM boom_client_blocklist_state WHERE id = 1"#,
        )
        .fetch_optional(pool)
        .await?;

        let enabled = match current {
            Some(v) => v,
            None => {
                sqlx::query(
                    r#"INSERT INTO boom_client_blocklist_state (id, enabled)
                       VALUES (1, $1)
                       ON CONFLICT (id) DO NOTHING"#,
                )
                .bind(yaml_default)
                .execute(pool)
                .await?;
                tracing::info!(enabled = yaml_default, "Initialized blocklist state row from YAML default");
                yaml_default
            }
        };
        self.set_enabled(enabled);
        Ok(enabled)
    }

    /// Flip the global `enabled` switch in DB and memory.
    pub async fn set_enabled_db(&self, pool: &PgPool, enabled: bool) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"INSERT INTO boom_client_blocklist_state (id, enabled)
               VALUES (1, $1)
               ON CONFLICT (id) DO UPDATE SET enabled = $1, updated_at = NOW()"#,
        )
        .bind(enabled)
        .execute(pool)
        .await?;
        self.set_enabled(enabled);
        Ok(())
    }
}
