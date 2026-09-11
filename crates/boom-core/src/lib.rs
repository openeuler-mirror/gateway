pub mod alert;
pub mod anthropic;
pub mod db_util;
pub mod debug_store;
pub mod error;
pub mod key_format;
pub mod kv_event;
pub mod normalize;
pub mod otlp_config;
pub mod provider;
pub mod stressmon;
pub mod trace;
pub mod types;

pub use alert::{Alert, AlertApi, AlertKind, AlertNotifier, AlertSnapshot, AlertStatus};
pub use debug_store::{DebugErrorEntry, DebugErrorStore};
pub use error::GatewayError;
pub use key_format::is_valid_prefix;
pub use kv_event::KvIndexBackend;
pub use otlp_config::OtlpConfig;
pub use provider::{Authenticator, DeploymentQueueInfo, KeyAliasLookup, Provider};
pub use stressmon::{StressmonApi, StressmonSample, StressmonSnapshot};
pub use trace::{
    ExporterStatusSnapshot, ProbeResult, RequestSpan, SpanStatus, TraceApi, TraceSnapshot,
};

/// Diagnostic counter for audit-log drops (channel full or batch INSERT
/// failures). Implemented by boom-main's LogWriter; consumed by the dashboard
/// debug page. Narrow trait so dashboard doesn't depend on LogWriter's
/// concrete type.
pub trait LogDroppedCounter: Send + Sync + 'static {
    /// Total logs dropped since process start.
    fn dropped_count(&self) -> u64;
}

/// Hand-maintained release version. Semantic version (`1.0.5`) — bumped
/// manually per release. Do NOT derive from build time, that defeats the
/// purpose (different checkouts would diverge). Frontend prepends "v" for
/// display, so keep this bare (no "v" prefix).
pub const BOOM_VERSION: &str = "1.0.5";
