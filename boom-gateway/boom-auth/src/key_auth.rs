use crate::models::{TeamRow, VerificationToken};
use boom_core::provider::Authenticator;
use boom_core::types::AuthIdentity;
use boom_core::GatewayError;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::time::Duration;
use tracing;

/// Database-backed authenticator compatible with litellm's schema.
///
/// Flow:
/// 1. Extract raw API key from request
/// 2. SHA-256 hash it (for sk- prefixed keys)
/// 3. Check in-memory cache first (moka)
/// 4. Fall back to PostgreSQL query on `boom_verification_token`
/// 5. Validate: not blocked, not expired, budget OK
pub struct DbAuthenticator {
    db: Option<PgPool>,
    /// Master key for admin access (plain text, compared with constant-time comparison).
    master_key: Option<String>,
    /// Cache: hashed_token → VerificationToken.
    cache: moka::future::Cache<String, VerificationToken>,
}

impl DbAuthenticator {
    pub fn new(db: Option<PgPool>, master_key: Option<String>) -> Self {
        Self {
            db,
            master_key,
            cache: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_idle(Duration::from_secs(300)) // 5 min TTL
                .build(),
        }
    }

    /// SHA-256 hash a raw API key, matching litellm's hash_token function.
    pub fn hash_token(token: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Check if a raw key matches the master key using constant-time comparison.
    fn is_master_key(&self, raw_key: &str) -> bool {
        match &self.master_key {
            Some(master) => {
                // Constant-time comparison to prevent timing attacks.
                let equal = raw_key.as_bytes().len() == master.as_bytes().len()
                    && raw_key
                        .as_bytes()
                        .iter()
                        .zip(master.as_bytes().iter())
                        .fold(0, |acc, (a, b)| acc | (a ^ b))
                        == 0;
                equal
            }
            None => false,
        }
    }

    /// Look up a token by its hash, checking cache first then DB.
    async fn lookup_token(&self, hashed: &str) -> Result<Option<VerificationToken>, GatewayError> {
        // 1. Check cache
        if let Some(cached) = self.cache.get(hashed).await {
            tracing::debug!("Token cache hit: {}", &hashed[..8]);
            return Ok(Some(cached));
        }

        // 2. No database configured — can only use master key.
        let db = match &self.db {
            Some(pool) => pool,
            None => return Ok(None),
        };

        // 3. Query database
        tracing::debug!("Token cache miss, querying DB: {}", &hashed[..8]);
        let result = sqlx::query_as::<_, VerificationToken>(
            r#"SELECT token, key_name, key_alias, spend, expires, models,
                      aliases, config, user_id, team_id, permissions,
                      max_parallel_requests, metadata, blocked,
                      tpm_limit, rpm_limit, max_budget, budget_duration,
                      budget_reset_at, allowed_cache_controls, allowed_routes,
                      model_spend, model_max_budget, budget_id, organization_id,
                      created_at, created_by, updated_at
               FROM "boom_verification_token"
               WHERE token = $1"#,
        )
        .bind(hashed)
        .fetch_optional(db)
        .await
        .map_err(|e| {
            tracing::error!("DB query failed for token lookup: {}", e);
            GatewayError::InternalError(format!("Database error: {}", e))
        })?;

        // 3. Cache the result
        if let Some(ref token) = result {
            self.cache.insert(hashed.to_string(), token.clone()).await;
        }

        Ok(result)
    }

    /// Convert a DB token row into an AuthIdentity.
    fn token_to_identity(&self, token: VerificationToken) -> AuthIdentity {
        AuthIdentity {
            key_hash: token.token.clone(),
            key_name: token.key_name.clone(),
            key_alias: token.key_alias,
            user_id: token.user_id,
            team_id: token.team_id,
            team_alias: None, // resolved later in authenticate()
            models: token.models,
            team_models: vec![], // resolved later in authenticate()
            rpm_limit: token.rpm_limit.map(|v| v as u64),
            tpm_limit: token.tpm_limit.map(|v| v as u64),
            max_budget: token.max_budget,
            spend: token.spend,
            blocked: token.blocked.unwrap_or(false),
            expires_at: token.expires,
            metadata: token.metadata.unwrap_or(serde_json::Value::Null),
        }
    }

    /// Query team's allowed models and alias from boom_team_table.
    async fn lookup_team(&self, team_id: &str) -> Result<(Vec<String>, Option<String>), GatewayError> {
        let db = match &self.db {
            Some(pool) => pool,
            None => return Ok((vec![], None)),
        };

        let result = sqlx::query_as::<_, TeamRow>(
            r#"SELECT models, team_alias FROM "boom_team_table" WHERE team_id = $1"#,
        )
        .bind(team_id)
        .fetch_optional(db)
        .await
        .map_err(|e| {
            tracing::warn!("Failed to query team for team {}: {}", team_id, e);
            GatewayError::InternalError(format!("Database error: {}", e))
        })?;

        Ok(match result {
            Some(r) => (r.models, r.team_alias),
            None => (vec![], None),
        })
    }
}

#[async_trait]
impl Authenticator for DbAuthenticator {
    async fn authenticate(&self, raw_key: &str) -> Result<AuthIdentity, GatewayError> {
        // 1. Check master key
        if self.is_master_key(raw_key) {
            tracing::debug!("Master key authenticated");
            return Ok(AuthIdentity {
                key_hash: "master".to_string(),
                key_name: Some("master".to_string()),
                key_alias: Some("master".to_string()),
                user_id: None,
                team_id: None,
                team_alias: None,
                models: vec![], // master can access all models
                team_models: vec![],
                rpm_limit: None,
                tpm_limit: None,
                max_budget: None,
                spend: 0.0,
                blocked: false,
                expires_at: None,
                metadata: serde_json::Value::Null,
            });
        }

        // 2. Hash the key — all keys are stored as SHA-256 hash in DB.
        let hashed = Self::hash_token(raw_key);

        // 3. Look up in cache / DB
        let token = self
            .lookup_token(&hashed)
            .await?
            .ok_or_else(|| GatewayError::AuthError("Invalid API key".to_string()))?;

        // 4. Validate the token
        let mut identity = self.token_to_identity(token);

        // 5. Resolve litellm special model names.
        //    litellm stores special identifiers in key.models:
        //    - "all-team-models" → use team's models (empty = all allowed)
        //    - "all-proxy-models" → all models on this proxy (empty = all allowed)
        if let Some(ref team_id) = identity.team_id {
            match self.lookup_team(team_id).await {
                Ok((team_models, team_alias)) => {
                    tracing::debug!(
                        "Resolved team for team {}: models={:?}, alias={:?}",
                        team_id, team_models, team_alias
                    );
                    identity.team_models = team_models;
                    identity.team_alias = team_alias;
                }
                Err(e) => {
                    tracing::warn!("Failed to resolve team: {}", e);
                }
            }
        }

        // Resolve special model names: replace key.models with the expanded list.
        if identity.models.contains(&"all-team-models".to_string()) {
            tracing::debug!(
                "Key {:?}: resolving 'all-team-models' → {:?}",
                identity.key_name, identity.team_models
            );
            identity.models = identity.team_models.clone();
        }
        if identity.models.contains(&"all-proxy-models".to_string()) {
            tracing::debug!(
                "Key {:?}: resolving 'all-proxy-models' → all models allowed",
                identity.key_name
            );
            identity.models = vec![];
        }

        // Fallback: if key.models is empty but team has models, use team's list.
        // Matches litellm: key.models > team_models > proxy_model_list
        if identity.models.is_empty() && !identity.team_models.is_empty() {
            tracing::info!(
                "Key {:?}: key.models empty, falling back to team_models → {:?}",
                identity.key_name, identity.team_models
            );
            identity.models = identity.team_models.clone();
        }

        if identity.blocked {
            return Err(GatewayError::KeyBlocked);
        }

        if identity.is_expired() {
            return Err(GatewayError::KeyExpired);
        }

        if identity.is_budget_exceeded() {
            return Err(GatewayError::BudgetExceeded);
        }

        tracing::debug!(
            "Key authenticated: {:?}, team={:?}",
            identity.key_name,
            identity.team_id
        );

        Ok(identity)
    }

    fn check_model_access(&self, identity: &AuthIdentity, model: &str) -> Result<(), GatewayError> {
        tracing::info!(
            "check_model_access: key={:?}, requested={}, key_models={:?}, team_models={:?}",
            identity.key_name, model, identity.models, identity.team_models
        );
        if identity.can_call_model(model) {
            Ok(())
        } else {
            tracing::warn!(
                "Model not allowed: key={:?}, requested={}, key_models={:?}, team_models={:?}",
                identity.key_name, model, identity.models, identity.team_models
            );
            Err(GatewayError::ModelNotAllowed(model.to_string()))
        }
    }
}
