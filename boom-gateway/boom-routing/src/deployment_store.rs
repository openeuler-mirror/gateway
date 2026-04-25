use boom_core::provider::Provider;
use dashmap::DashMap;
use serde::Serialize;
use sqlx::Row;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// In-memory store for model deployments.
/// Survives config reloads — updated incrementally via DB or YAML seed.
pub struct DeploymentStore {
    /// model_name → list of provider deployments.
    deployments: DashMap<String, Vec<Arc<dyn Provider>>>,
    /// model_name → round-robin counter.
    rr_counters: DashMap<String, AtomicUsize>,
    /// model_name → quota count ratio (default 1).
    quota_ratios: DashMap<String, u64>,
}

/// Full deployment row from boom_model_deployment table.
/// Used for list queries and snapshots.
#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct DeploymentRow {
    pub id: uuid::Uuid,
    pub model_name: String,
    pub litellm_model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<bool>,
    pub api_base: Option<String>,
    pub api_version: Option<String>,
    pub aws_region_name: Option<String>,
    pub aws_access_key_id: Option<String>,
    pub aws_secret_access_key: Option<String>,
    pub rpm: Option<i64>,
    pub tpm: Option<i64>,
    pub timeout: i64,
    pub headers: serde_json::Value,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i32>,
    pub enabled: Option<bool>,
    pub auto_disabled: Option<bool>,
    pub source: Option<String>,
    pub deployment_id: Option<String>,
    pub quota_count_ratio: Option<i64>,
    pub max_inflight_queue_len: Option<i32>,
    pub max_context_len: Option<i64>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Minimal deployment row for provider creation.
#[derive(Debug, sqlx::FromRow)]
pub struct DeploymentProviderRow {
    pub model_name: String,
    pub litellm_model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<bool>,
    pub api_base: Option<String>,
    pub api_version: Option<String>,
    pub aws_region_name: Option<String>,
    pub timeout: i64,
    pub headers: serde_json::Value,
    pub deployment_id: Option<String>,
}

/// Input for creating/updating a deployment in DB.
#[derive(Debug, Clone)]
pub struct DeploymentInput {
    pub model_name: String,
    pub litellm_model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<bool>,
    pub api_base: Option<String>,
    pub api_version: Option<String>,
    pub aws_region_name: Option<String>,
    pub aws_access_key_id: Option<String>,
    pub aws_secret_access_key: Option<String>,
    pub rpm: Option<i64>,
    pub tpm: Option<i64>,
    pub timeout: i64,
    pub headers: serde_json::Value,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i32>,
    pub enabled: bool,
    pub deployment_id: Option<String>,
    pub quota_count_ratio: i64,
    pub max_inflight_queue_len: Option<i32>,
    pub max_context_len: Option<i64>,
}

/// YAML deployment data for sync (no provider needed).
#[derive(Clone)]
pub struct YamlDeploymentData {
    pub model_name: String,
    pub litellm_model: String,
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    pub api_version: Option<String>,
    pub aws_region_name: Option<String>,
    pub aws_access_key_id: Option<String>,
    pub aws_secret_access_key: Option<String>,
    pub rpm: Option<i64>,
    pub tpm: Option<i64>,
    pub timeout: i64,
    pub headers: serde_json::Value,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i32>,
    pub deployment_id: Option<String>,
    pub quota_count_ratio: i64,
    pub max_inflight_queue_len: Option<i32>,
    pub max_context_len: Option<i64>,
    pub enabled: bool,
}

impl DeploymentStore {
    pub fn new() -> Self {
        Self {
            deployments: DashMap::new(),
            rr_counters: DashMap::new(),
            quota_ratios: DashMap::new(),
        }
    }

    // ── In-memory operations ──

    /// Replace all deployments for a model name.
    pub fn set_deployments(&self, model_name: String, providers: Vec<Arc<dyn Provider>>) {
        self.rr_counters.remove(&model_name);
        self.deployments.insert(model_name, providers);
    }

    /// Add a single deployment to an existing model group (or create it).
    pub fn add_deployment(&self, model_name: &str, provider: Arc<dyn Provider>) {
        self.deployments
            .entry(model_name.to_string())
            .or_default()
            .push(provider);
    }

    /// Remove all deployments for the given model name.
    /// Returns true if the model existed.
    pub fn remove_deployments(&self, model_name: &str) -> bool {
        self.rr_counters.remove(model_name);
        self.deployments.remove(model_name).is_some()
    }

    /// Clear all deployments (used before full reload).
    pub fn clear(&self) {
        self.rr_counters.clear();
        self.deployments.clear();
        self.quota_ratios.clear();
    }

    /// Set the quota count ratio for a model.
    pub fn set_quota_ratio(&self, model_name: &str, ratio: u64) {
        self.quota_ratios.insert(model_name.to_string(), ratio);
    }

    /// Get the quota count ratio for a model. Returns 1 if not set.
    pub fn get_quota_ratio(&self, model_name: &str) -> u64 {
        self.quota_ratios.get(model_name).map(|r| *r).unwrap_or(1)
    }

    /// Select a provider via round-robin for the given model.
    /// Returns None if no deployments exist.
    pub fn select(&self, model_name: &str) -> Option<Arc<dyn Provider>> {
        let providers = self.deployments.get(model_name)?;
        if providers.is_empty() {
            return None;
        }
        if providers.len() == 1 {
            return Some(providers[0].clone());
        }

        let counter = self
            .rr_counters
            .entry(model_name.to_string())
            .or_insert_with(|| AtomicUsize::new(0));
        let idx = counter.fetch_add(1, Ordering::Relaxed);
        Some(providers[idx % providers.len()].clone())
    }

    /// Get all providers for a model (for custom scheduling).
    pub fn get_providers(&self, model_name: &str) -> Option<Vec<Arc<dyn Provider>>> {
        self.deployments.get(model_name).map(|r| r.value().clone())
    }

    /// Get all model names (deployment keys).
    pub fn model_names(&self) -> Vec<String> {
        self.deployments.iter().map(|r| r.key().clone()).collect()
    }

    /// Check if a model name has deployments.
    pub fn contains(&self, model_name: &str) -> bool {
        self.deployments.contains_key(model_name)
    }

    /// Get the number of deployments for a model.
    pub fn deployment_count(&self, model_name: &str) -> usize {
        self.deployments
            .get(model_name)
            .map(|r| r.value().len())
            .unwrap_or(0)
    }

    /// Total number of unique model names.
    pub fn len(&self) -> usize {
        self.deployments.len()
    }

    /// Total deployment count across all models.
    pub fn total_deployments(&self) -> usize {
        self.deployments
            .iter()
            .map(|r| r.value().len())
            .sum()
    }

    /// Reverse lookup: find the model_name that owns a deployment with the given deployment_id.
    pub fn find_model_by_deployment_id(&self, deployment_id: &str) -> Option<String> {
        for entry in self.deployments.iter() {
            for provider in entry.value().iter() {
                if provider.deployment_id() == Some(deployment_id) {
                    return Some(entry.key().clone());
                }
            }
        }
        None
    }

    // ── DB operations (boom_routing owns boom_model_deployment table) ──

    /// Sync YAML deployments to DB: delete source='yaml' rows, insert current YAML,
    /// delete conflicting source='db' rows.
    pub async fn sync_yaml_to_db(
        pool: &sqlx::PgPool,
        yaml_model_names: &[String],
        yaml_deployments: &[YamlDeploymentData],
    ) -> Result<(), sqlx::Error> {
        // 1. Delete all source='yaml' rows.
        sqlx::query(r#"DELETE FROM boom_model_deployment WHERE source = 'yaml'"#)
            .execute(pool)
            .await?;

        // 2. Delete source='db' deployments that conflict with YAML model_names.
        if !yaml_model_names.is_empty() {
            let result = sqlx::query(
                r#"DELETE FROM boom_model_deployment WHERE source = 'db' AND model_name = ANY($1)"#,
            )
            .bind(yaml_model_names)
            .execute(pool)
            .await?;
            if result.rows_affected() > 0 {
                tracing::info!(
                    "Removed {} conflicting source='db' deployment(s)",
                    result.rows_affected()
                );
            }
        }

        // 3. Insert YAML deployments.
        for d in yaml_deployments {
            sqlx::query(
                r#"INSERT INTO boom_model_deployment
                   (model_name, litellm_model, api_key, api_key_env, api_base, api_version,
                    aws_region_name, aws_access_key_id, aws_secret_access_key,
                    rpm, tpm, timeout, headers, temperature, max_tokens, enabled, source, deployment_id,
                    quota_count_ratio, max_inflight_queue_len, max_context_len)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, 'yaml', $17,
                   $18, $19, $20)"#,
            )
            .bind(&d.model_name)
            .bind(&d.litellm_model)
            .bind(&d.api_key)
            .bind(false) // api_key already resolved
            .bind(&d.api_base)
            .bind(&d.api_version)
            .bind(&d.aws_region_name)
            .bind(&d.aws_access_key_id)
            .bind(&d.aws_secret_access_key)
            .bind(d.rpm)
            .bind(d.tpm)
            .bind(d.timeout)
            .bind(&d.headers)
            .bind(d.temperature)
            .bind(d.max_tokens.map(|v| v as i32))
            .bind(d.enabled)
            .bind(&d.deployment_id)
            .bind(d.quota_count_ratio)
            .bind(d.max_inflight_queue_len)
            .bind(d.max_context_len)
            .execute(pool)
            .await?;
        }

        tracing::info!("Synced {} deployment(s) from YAML to DB", yaml_deployments.len());
        Ok(())
    }

    /// Load source='db' deployment rows from DB (provider creation happens in caller).
    pub async fn load_db_only_rows(pool: &sqlx::PgPool) -> Result<Vec<DeploymentProviderRow>, sqlx::Error> {
        sqlx::query_as::<_, DeploymentProviderRow>(
            r#"SELECT model_name, litellm_model, api_key, api_key_env, api_base, api_version,
                      aws_region_name, timeout, headers, deployment_id
               FROM boom_model_deployment
               WHERE source = 'db' AND enabled IS NOT FALSE
               ORDER BY model_name, created_at"#,
        )
        .fetch_all(pool)
        .await
    }

    /// Load enabled deployment rows for a specific model (for reload after update/delete).
    pub async fn load_model_rows(pool: &sqlx::PgPool, model_name: &str) -> Result<Vec<DeploymentProviderRow>, sqlx::Error> {
        sqlx::query_as::<_, DeploymentProviderRow>(
            r#"SELECT model_name, litellm_model, api_key, api_key_env, api_base, api_version,
                      aws_region_name, timeout, headers, deployment_id
               FROM boom_model_deployment
               WHERE model_name = $1 AND enabled IS NOT FALSE
               ORDER BY created_at"#,
        )
        .bind(model_name)
        .fetch_all(pool)
        .await
    }

    /// Create a deployment in DB. Returns the new row ID.
    pub async fn create_db(pool: &sqlx::PgPool, input: &DeploymentInput) -> Result<uuid::Uuid, sqlx::Error> {
        let row = sqlx::query(
            r#"INSERT INTO boom_model_deployment
               (model_name, litellm_model, api_key, api_key_env, api_base, api_version,
                aws_region_name, aws_access_key_id, aws_secret_access_key,
                rpm, tpm, timeout, headers, temperature, max_tokens, enabled, source, deployment_id,
                quota_count_ratio, max_inflight_queue_len, max_context_len)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, 'db', $17,
                $18, $19, $20)
               RETURNING id"#,
        )
        .bind(&input.model_name)
        .bind(&input.litellm_model)
        .bind(&input.api_key)
        .bind(input.api_key_env.unwrap_or(false))
        .bind(&input.api_base)
        .bind(&input.api_version)
        .bind(&input.aws_region_name)
        .bind(&input.aws_access_key_id)
        .bind(&input.aws_secret_access_key)
        .bind(input.rpm)
        .bind(input.tpm)
        .bind(input.timeout)
        .bind(&input.headers)
        .bind(input.temperature)
        .bind(input.max_tokens)
        .bind(input.enabled)
        .bind(&input.deployment_id)
        .bind(input.quota_count_ratio)
        .bind(input.max_inflight_queue_len)
        .bind(input.max_context_len)
        .fetch_one(pool)
        .await?;

        Ok(row.get("id"))
    }

    /// Update a deployment in DB by ID.
    pub async fn update_db(pool: &sqlx::PgPool, id: uuid::Uuid, input: &DeploymentInput) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"UPDATE boom_model_deployment
               SET model_name = $2, litellm_model = $3,
                   api_key = COALESCE($4, api_key),
                   api_key_env = $5,
                   api_base = COALESCE($6, api_base),
                   api_version = COALESCE($7, api_version),
                   aws_region_name = COALESCE($8, aws_region_name),
                   aws_access_key_id = COALESCE($9, aws_access_key_id),
                   aws_secret_access_key = COALESCE($10, aws_secret_access_key),
                   rpm = $11, tpm = $12, timeout = $13, headers = $14,
                   temperature = $15, max_tokens = $16, enabled = $17,
                   auto_disabled = CASE WHEN $17 = true THEN false ELSE auto_disabled END,
                   deployment_id = COALESCE($18, deployment_id),
                   quota_count_ratio = $19,
                   max_inflight_queue_len = $20, max_context_len = $21,
                   updated_at = NOW()
               WHERE id = $1"#,
        )
        .bind(id)
        .bind(&input.model_name)
        .bind(&input.litellm_model)
        .bind(&input.api_key)
        .bind(input.api_key_env.unwrap_or(false))
        .bind(&input.api_base)
        .bind(&input.api_version)
        .bind(&input.aws_region_name)
        .bind(&input.aws_access_key_id)
        .bind(&input.aws_secret_access_key)
        .bind(input.rpm)
        .bind(input.tpm)
        .bind(input.timeout)
        .bind(&input.headers)
        .bind(input.temperature)
        .bind(input.max_tokens)
        .bind(input.enabled)
        .bind(&input.deployment_id)
        .bind(input.quota_count_ratio)
        .bind(input.max_inflight_queue_len)
        .bind(input.max_context_len)
        .execute(pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Delete a deployment from DB by ID. Returns (model_name, deployment_id) of the deleted row.
    pub async fn delete_db(pool: &sqlx::PgPool, id: uuid::Uuid) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
        // Get info before deleting.
        let info: Option<(String, Option<String>)> = sqlx::query_as(
            r#"SELECT model_name, deployment_id FROM boom_model_deployment WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(pool)
        .await?;

        let (model_name, deployment_id) = match info {
            Some(t) => t,
            None => return Ok(None),
        };

        sqlx::query(r#"DELETE FROM boom_model_deployment WHERE id = $1"#)
            .bind(id)
            .execute(pool)
            .await?;

        Ok(Some((model_name, deployment_id)))
    }

    /// Auto-disable a deployment: set enabled=false, auto_disabled=true.
    /// Returns the actual model_name of the disabled deployment, or None if not found.
    pub async fn auto_disable_db(pool: &sqlx::PgPool, deployment_id: &str) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(String,)> = sqlx::query_as(
            r#"UPDATE boom_model_deployment
               SET enabled = false, auto_disabled = true, updated_at = NOW()
               WHERE deployment_id = $1
               RETURNING model_name"#,
        )
        .bind(deployment_id)
        .fetch_optional(pool)
        .await?;

        if row.is_none() {
            tracing::warn!(
                deployment_id = %deployment_id,
                "No rows updated — deployment_id may not exist in DB"
            );
        }
        Ok(row.map(|(name,)| name))
    }

    /// List all deployments from DB (for dashboard).
    pub async fn list_all_db(pool: &sqlx::PgPool) -> Result<Vec<DeploymentRow>, sqlx::Error> {
        sqlx::query_as::<_, DeploymentRow>(
            r#"SELECT id, model_name, litellm_model, api_key, api_key_env, api_base, api_version,
                      aws_region_name, aws_access_key_id, aws_secret_access_key,
                      rpm, tpm, timeout, headers, temperature, max_tokens, enabled, auto_disabled,
                      source, deployment_id, quota_count_ratio,
                      max_inflight_queue_len, max_context_len,
                      created_at, updated_at
               FROM boom_model_deployment
               ORDER BY model_name, created_at"#,
        )
        .fetch_all(pool)
        .await
    }

    /// Snapshot all deployments from DB (for config export).
    pub async fn snapshot_db(pool: &sqlx::PgPool) -> Result<Vec<DeploymentRow>, sqlx::Error> {
        sqlx::query_as::<_, DeploymentRow>(
            r#"SELECT id, model_name, litellm_model, api_key, api_key_env, api_base, api_version,
                      aws_region_name, aws_access_key_id, aws_secret_access_key,
                      rpm, tpm, timeout, headers, temperature, max_tokens, enabled, auto_disabled,
                      source, deployment_id, quota_count_ratio,
                      max_inflight_queue_len, max_context_len,
                      created_at, updated_at
               FROM boom_model_deployment
               WHERE enabled IS NOT FALSE
               ORDER BY model_name, created_at"#,
        )
        .fetch_all(pool)
        .await
    }
}

impl Default for DeploymentStore {
    fn default() -> Self {
        Self::new()
    }
}
