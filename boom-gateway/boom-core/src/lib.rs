pub mod anthropic;
pub mod db_util;
pub mod debug_store;
pub mod error;
pub mod kv_event;
pub mod normalize;
pub mod provider;
pub mod types;

pub use debug_store::{DebugErrorEntry, DebugErrorStore};
pub use error::GatewayError;
pub use kv_event::KvIndexBackend;
pub use provider::{Authenticator, DeploymentQueueInfo, KeyAliasLookup, Provider};

/// Hand-maintained release version. Format: `YY.MMDD.HHMM` (e.g. `26.0710.1723`).
/// Bumped manually per release — do NOT derive from build time, that defeats
/// the purpose (different checkouts would diverge).
pub const BOOM_VERSION: &str = "26.0710.1750";
