use thiserror::Error;

/// Unified error type for the gateway.
///
/// Each variant maps to a specific HTTP status code for client responses.
#[derive(Error, Debug)]
pub enum GatewayError {
    #[error("Authentication failed: {0}")]
    AuthError(String),

    #[error("{message}")]
    RateLimitExceeded {
        retry_after_secs: Option<u64>,
        message: String,
        /// Specific limit type for diagnostics. Per-minute window variants:
        /// `rpm_limit`/`tpm_limit`/`cost_limit` (default key, no plan),
        /// `plan_rpm_limit`/`plan_tpm_limit`/`plan_cost_limit` (key with plan),
        /// `team_rpm_limit`/`team_tpm_limit`/`team_cost_limit` (team scope).
        /// Custom-window variants (non-60s):
        /// `window_limit`/`plan_window_limit`/`team_window_limit` (the message
        /// body disambiguates counts/tokens/costs since these share a code).
        /// Cumulative totals (separate path, not via map_decision_to_err):
        /// `key_total_token`/`team_total_token`/`key_total_cost`/`team_total_cost`.
        limit_type: &'static str,
        /// Which entity hit the limit: "key" or "team". None for legacy paths.
        scope: Option<&'static str>,
        /// Identifier of the scope (key_alias / team_alias / key_hash / team_id).
        scope_id: Option<String>,
        /// Plan name that defined the limit.
        plan_name: Option<String>,
    },

    #[error("Concurrency limit exceeded: {message}")]
    ConcurrencyExceeded { limit: u32, message: String },

    #[error("Model not found: {0}")]
    ModelNotFound(String),

    #[error("Provider error: {0}")]
    ProviderError(String),

    #[error("Budget exceeded for key")]
    BudgetExceeded,

    #[error("Config error: {0}")]
    ConfigError(String),

    #[error("Key expired")]
    KeyExpired,

    #[error("Key blocked")]
    KeyBlocked,

    #[error("Model not allowed: {0}")]
    ModelNotAllowed(String),

    #[error("Upstream timeout")]
    UpstreamTimeout,

    #[error("Upstream error ({status}): {message}")]
    UpstreamError { status: u16, message: String },

    #[error("Endpoint not supported: {0}")]
    NotSupported(String),

    #[error("Unsupported mode: {0}")]
    UnsupportedMode(String),

    #[error("Flow control queue timeout: {message}")]
    FlowControlQueueTimeout {
        deployment_id: String,
        waiters: usize,
        message: String,
    },

    #[error("Internal error: {0}")]
    InternalError(String),
}

impl GatewayError {
    /// Map to HTTP status code.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::AuthError(_) => 401,
            Self::RateLimitExceeded { .. } | Self::ConcurrencyExceeded { .. } => 429,
            Self::ModelNotFound(_) => 404,
            Self::ProviderError(_) => 502,
            Self::BudgetExceeded => 402,
            Self::ConfigError(_) => 500,
            Self::KeyExpired => 401,
            Self::KeyBlocked => 403,
            Self::ModelNotAllowed(_) => 403,
            Self::UpstreamTimeout => 504,
            Self::UpstreamError { .. } => 502,
            Self::NotSupported(_) => 404,
            Self::UnsupportedMode(_) => 400,
            Self::FlowControlQueueTimeout { .. } => 503,
            Self::InternalError(_) => 500,
        }
    }

    /// Whether this error should be persisted to the request log DB.
    /// Expected rejections (rate limit, concurrency, budget) are too frequent
    /// to audit individually — they're tracked by the in-memory limiter instead.
    pub fn should_log_to_db(&self) -> bool {
        !matches!(
            self,
            Self::RateLimitExceeded { .. }
                | Self::ConcurrencyExceeded { .. }
                | Self::BudgetExceeded
                | Self::FlowControlQueueTimeout { .. }
        )
    }

    /// Whether this error should be deduplicated per `(key_hash, model, 60s)`
    /// before being logged (tracing + DB).
    ///
    /// Covers expected rejections (rate-limit / concurrency / budget /
    /// flow-control) AND not-found / not-allowed / expired / blocked —
    /// clients hitting the same wall retry repeatedly, so logging every
    /// attempt produces log spam and DB bloat without adding signal.
    ///
    /// `AuthError` (401) is NOT here: failed auth is a security signal we
    /// want in full. `UpstreamError` / `ProviderError` / `ConfigError` /
    /// `InternalError` are not here either — they reflect per-request
    /// upstream behavior, not repeated client rejections.
    pub fn should_dedup_log(&self) -> bool {
        matches!(
            self,
            Self::RateLimitExceeded { .. }
                | Self::ConcurrencyExceeded { .. }
                | Self::BudgetExceeded
                | Self::FlowControlQueueTimeout { .. }
                | Self::ModelNotFound(_)
                | Self::ModelNotAllowed(_)
                | Self::KeyExpired
                | Self::KeyBlocked
        )
    }

    /// Whether this error represents a deterministic deployment failure
    /// (unreachable upstream or authentication failure) that will not self-heal.
    pub fn is_deployment_failure(&self) -> bool {
        match self {
            Self::ProviderError(_) => true,
            Self::UpstreamError { status, .. } => *status == 401 || *status == 403,
            _ => false,
        }
    }

    /// OpenAI-style error type string.
    pub fn error_type(&self) -> &str {
        match self {
            Self::AuthError(_) => "authentication_error",
            Self::RateLimitExceeded { limit_type, .. } => limit_type,
            Self::ConcurrencyExceeded { .. } => "concurrency_exceeded",
            Self::ModelNotFound(_) => "model_not_found",
            Self::BudgetExceeded => "budget_exceeded",
            Self::KeyExpired => "key_expired",
            Self::KeyBlocked => "key_blocked",
            Self::ModelNotAllowed(_) => "model_not_allowed",
            Self::UpstreamTimeout => "timeout",
            Self::UpstreamError { .. } => "upstream_error",
            Self::NotSupported(_) => "not_supported",
            Self::UnsupportedMode(_) => "unsupported_mode_error",
            Self::ProviderError(_) => "provider_error",
            Self::FlowControlQueueTimeout { .. } => "flow_control_timeout",
            Self::ConfigError(_) | Self::InternalError(_) => "internal_error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rl() -> GatewayError {
        GatewayError::RateLimitExceeded {
            retry_after_secs: None,
            message: "rpm".into(),
            limit_type: "rpm_limit",
            scope: Some("key"),
            scope_id: None,
            plan_name: None,
        }
    }
    fn cc() -> GatewayError {
        GatewayError::ConcurrencyExceeded { limit: 1, message: "x".into() }
    }
    fn fc() -> GatewayError {
        GatewayError::FlowControlQueueTimeout {
            deployment_id: "d".into(),
            waiters: 0,
            message: "x".into(),
        }
    }

    #[test]
    fn should_dedup_log_membership() {
        // Dedup members — repeated client rejections.
        assert!(rl().should_dedup_log());
        assert!(cc().should_dedup_log());
        assert!(GatewayError::BudgetExceeded.should_dedup_log());
        assert!(fc().should_dedup_log());
        assert!(GatewayError::ModelNotFound("gpt-x".into()).should_dedup_log());
        assert!(GatewayError::ModelNotAllowed("gpt-x".into()).should_dedup_log());
        assert!(GatewayError::KeyExpired.should_dedup_log());
        assert!(GatewayError::KeyBlocked.should_dedup_log());

        // Non-members — keep full per-request logging.
        assert!(!GatewayError::AuthError("bad key".into()).should_dedup_log());
        assert!(!GatewayError::ProviderError("upstream".into()).should_dedup_log());
        assert!(!GatewayError::UpstreamTimeout.should_dedup_log());
        assert!(!GatewayError::UpstreamError { status: 500, message: "x".into() }.should_dedup_log());
        assert!(!GatewayError::ConfigError("cfg".into()).should_dedup_log());
        assert!(!GatewayError::InternalError("boom".into()).should_dedup_log());
        assert!(!GatewayError::NotSupported("embeddings".into()).should_dedup_log());
        assert!(!GatewayError::UnsupportedMode("x".into()).should_dedup_log());
    }

    /// `should_dedup_log` is a strict superset of `!should_log_to_db` —
    /// everything that was deduped before stays deduped, plus the four
    /// new client-rejection variants.
    #[test]
    fn dedup_log_superset_of_not_log_to_db() {
        let non_db = [
            rl(),
            cc(),
            GatewayError::BudgetExceeded,
            fc(),
        ];
        for e in non_db {
            assert!(e.should_dedup_log(), "{:?} should remain deduped", e);
            assert!(!e.should_log_to_db());
        }
        let new_dedup = [
            GatewayError::ModelNotFound("gpt-x".into()),
            GatewayError::ModelNotAllowed("gpt-x".into()),
            GatewayError::KeyExpired,
            GatewayError::KeyBlocked,
        ];
        for e in new_dedup {
            assert!(e.should_dedup_log(), "{:?} should be deduped", e);
            // These still "should log to DB" in the sense that the first
            // occurrence within a window writes to DB — dedup happens
            // upstream of the DB write.
            assert!(e.should_log_to_db());
        }
    }
}
