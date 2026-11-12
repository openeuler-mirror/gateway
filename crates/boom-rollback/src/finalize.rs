//! 502→400 passthrough (design §12 #8): when the rollback feature is on
//! and the final error is an identified context rejection, convert the
//! reply error so clients see the upstream's 400 semantics instead of the
//! misleading 502 (agents retry 5xx).

use boom_core::context_error::RollbackStats;
use boom_core::error::GatewayError;

/// 502→400 passthrough (design §12 #8): when the rollback feature is on
/// and the final error is an identified context rejection, convert the
/// reply error so clients see the upstream's 400 semantics instead of the
/// misleading 502 (agents retry 5xx). The original error is preserved for
/// logging; only the reply carries the conversion.
pub fn finalize_error(
    e: GatewayError,
    patterns: &[String],
    stats: Option<&RollbackStats>,
) -> GatewayError {
    if patterns.is_empty() {
        return e;
    }
    if let GatewayError::UpstreamError { status, message } = &e {
        if boom_core::context_error::identify(*status, message, patterns).matched {
            if let Some(stats) = stats {
                stats.record_passthrough_400();
            }
            return GatewayError::ContextLengthExceeded(message.clone());
        }
    }
    e
}

