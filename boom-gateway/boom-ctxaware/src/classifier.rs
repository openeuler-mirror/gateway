//! M1 classifier — path-based, deliberately minimal.
//!
//! Later milestones will widen this to a multi-signal pipeline
//! (User-Agent, anthropic-beta header, system-prompt features) with
//! confidence scoring. Keeping the surface tiny for now so the
//! dashboard can ship a useful "is this Anthropic-native?" signal
//! while the richer detection is still being designed.

/// Wire-format header name for the per-deployment client-type signal.
/// Sent only when the routed deployment has `client_type_header: true`.
pub const CLIENT_TYPE_HEADER: &str = "X-BooM-Client-Type";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    Anthropic,
    Other,
}

impl ClientKind {
    /// Stable wire label suitable for HTTP header values. Future milestones
    /// adding `ClaudeCode` and similar variants will land here as new arms.
    pub fn wire_label(&self) -> &'static str {
        match self {
            ClientKind::Anthropic => "anthropic",
            ClientKind::Other => "anonymous",
        }
    }
}

/// Returns true when the request targets the Anthropic-native
/// `/v1/messages` endpoint. Used by `AgentStatsTracker::record` to
/// bucket requests without inspecting headers or body.
pub fn is_anthropic_path(api_path: &str) -> bool {
    api_path == "/v1/messages" || api_path.starts_with("/v1/messages/")
}

/// Classify an incoming request by its api_path. Currently binary
/// (Anthropic vs Other); will gain nuance as the classifier learns
/// more signals (UA, anthropic-beta, system-prompt features).
pub fn classify(api_path: &str) -> ClientKind {
    if is_anthropic_path(api_path) {
        ClientKind::Anthropic
    } else {
        ClientKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_messages_is_anthropic() {
        assert_eq!(classify("/v1/messages"), ClientKind::Anthropic);
        assert_eq!(classify("/v1/messages/count_tokens"), ClientKind::Anthropic);
    }

    #[test]
    fn classify_chat_completions_is_other() {
        assert_eq!(classify("/v1/chat/completions"), ClientKind::Other);
        assert_eq!(classify("/v1/completions"), ClientKind::Other);
        assert_eq!(classify("/anything-else"), ClientKind::Other);
    }

    #[test]
    fn wire_label_matches_spec() {
        assert_eq!(ClientKind::Anthropic.wire_label(), "anthropic");
        assert_eq!(ClientKind::Other.wire_label(), "anonymous");
    }
}
