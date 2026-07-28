pub mod anthropic;
pub mod db_util;
pub mod debug_store;
pub mod error;
pub mod key_format;
pub mod kv_event;
pub mod normalize;
pub mod provider;
pub mod types;

pub use debug_store::{DebugErrorEntry, DebugErrorStore};
pub use error::GatewayError;
pub use key_format::is_valid_prefix;
pub use kv_event::KvIndexBackend;
pub use provider::{Authenticator, DeploymentQueueInfo, KeyAliasLookup, Provider};

/// Diagnostic counter for audit-log drops (channel full or batch INSERT
/// failures). Implemented by boom-main's LogWriter; consumed by the dashboard
/// debug page. Narrow trait so dashboard doesn't depend on LogWriter's
/// concrete type.
pub trait LogDroppedCounter: Send + Sync + 'static {
    /// Total logs dropped since process start.
    fn dropped_count(&self) -> u64;
}

/// Hand-maintained release version. Format: `YY.MMDD.HHMM` (e.g. `26.0710.1723`).
/// Bumped manually per release — do NOT derive from build time, that defeats
/// the purpose (different checkouts would diverge).
pub const BOOM_VERSION: &str = "26.0715.0940";
