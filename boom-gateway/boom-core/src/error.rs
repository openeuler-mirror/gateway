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
        /// Specific limit type for diagnostics: "plan_rpm_limit", "plan_window_limit",
        /// "rpm_limit", "window_limit".
        limit_type: &'static str,
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
            Self::ProviderError(_) => "provider_error",
            Self::FlowControlQueueTimeout { .. } => "flow_control_timeout",
            Self::ConfigError(_) | Self::InternalError(_) => "internal_error",
        }
    }
}
