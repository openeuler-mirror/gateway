use chrono::NaiveDateTime;
use serde::Deserialize;
use sqlx::FromRow;

/// Maps to `boom_verification_token` table (formerly litellm's `LiteLLM_VerificationToken`).
/// Only includes fields needed for key authentication.
#[derive(Debug, Clone, FromRow, Deserialize)]
pub struct VerificationToken {
    /// SHA-256 hash of the API key (primary key).
    pub token: String,
    pub key_name: Option<String>,
    pub key_alias: Option<String>,
    /// Total spend on this key.
    pub spend: f64,
    /// Expiration time (null = never expires).
    pub expires: Option<NaiveDateTime>,
    /// Allowed models. May contain model names or model group names (e.g. "all-team-models").
    pub models: Vec<String>,
    pub aliases: Option<serde_json::Value>,
    pub config: Option<serde_json::Value>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub permissions: Option<serde_json::Value>,
    pub max_parallel_requests: Option<i32>,
    pub metadata: Option<serde_json::Value>,
    /// Whether the key is blocked.
    pub blocked: Option<bool>,
    pub tpm_limit: Option<i64>,
    pub rpm_limit: Option<i64>,
    pub max_budget: Option<f64>,
    /// Budget period duration string (e.g. "1d", "7d", "30d").
    pub budget_duration: Option<String>,
    pub budget_reset_at: Option<NaiveDateTime>,
    pub allowed_cache_controls: Option<Vec<String>>,
    pub allowed_routes: Option<Vec<String>>,
    pub model_spend: Option<serde_json::Value>,
    pub model_max_budget: Option<serde_json::Value>,
    pub budget_id: Option<String>,
    pub organization_id: Option<String>,
    pub created_at: Option<NaiveDateTime>,
    pub created_by: Option<String>,
    pub updated_at: Option<NaiveDateTime>,
}

/// Maps to `boom_team_table` (formerly litellm's `LiteLLM_TeamTable`) — only the fields we need.
#[derive(Debug, Clone, FromRow)]
pub struct TeamRow {
    pub models: Vec<String>,
    pub team_alias: Option<String>,
}
