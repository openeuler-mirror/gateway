//! boom-ctxaware — context-aware client perception.
//!
//! Long-term home for client identification, classification, and
//! affinity scheduling. M1 ships only a path-based classifier and a
//! 60-bucket ring-buffer statistics tracker, sufficient to expose
//! "Agent Statistics" on the dashboard. Later milestones will widen
//! the signal set (User-Agent, `anthropic-beta`, system-prompt
//! features), add configurable rules, and feed affinity hints to
//! boom-routing.

pub mod classifier;
pub mod stats;

pub use classifier::{classify, is_anthropic_path, ClientKind, CLIENT_TYPE_HEADER};
pub use stats::{AgentStatsSnapshot, AgentStatsTracker, MinuteBucket, AgentSummary};
