//! M1 classifier — path-based, deliberately minimal.
//!
//! Later milestones will widen this to a multi-signal pipeline
//! (User-Agent, anthropic-beta header, system-prompt features) with
//! confidence scoring. Keeping the surface tiny for now so the
//! dashboard can ship a useful "is this Anthropic-native?" signal
//! while the richer detection is still being designed.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    Anthropic,
    OpenAI,
    Other,
}

/// Returns true when the request targets the Anthropic-native
/// `/v1/messages` endpoint. Used by `AgentStatsTracker::record` to
/// bucket requests without inspecting headers or body.
pub fn is_anthropic_path(api_path: &str) -> bool {
    api_path == "/v1/messages" || api_path.starts_with("/v1/messages/")
}
