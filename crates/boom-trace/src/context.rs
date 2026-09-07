//! W3C Trace Context parsing — `traceparent` / `tracestate` HTTP headers.
//!
//! The parser is the same shape as `boom_promptlog::otlp::parse_traceparent`
//! (4-part hyphen-separated `version-traceid-spanid-flags`) but lives here
//! so boom-trace can be the producer of W3C context for the gateway. The
//! promptlog crate still uses its own copy for OTLP logs correlation —
//! the two crates are leaf crates (both depend only on boom-core) and
//! keeping the parser duplicated avoids introducing a boom-trace → boom-promptlog
//! or reverse dependency.
//!
//! `build_child_traceparent` is the inverse: given a parent context and
//! the new gateway span id, emit the `traceparent` header that gets injected
//! into `req.gateway_headers` so upstream providers forward the gateway
//! span as the new parent.

/// W3C trace context parsed from inbound `traceparent` (+ optional `tracestate`).
///
/// `trace_id` is the 16-byte (32 hex) trace id; `parent_span_id` is the 8-byte
/// (16 hex) parent span id from the inbound header; `sampled` is the low
/// bit of the flags field (1 = sampled). `trace_state` is the raw
/// `tracestate` header value (vendor-specific list, comma-separated).
#[derive(Debug, Clone)]
pub struct W3cContext {
    pub trace_id: [u8; 16],
    pub parent_span_id: [u8; 8],
    pub sampled: bool,
    pub trace_state: String,
}

impl W3cContext {
    /// Lowercase hex encoding of the 16-byte trace id (32 chars).
    pub fn trace_id_hex(&self) -> String {
        hex::encode(self.trace_id)
    }

    /// Lowercase hex encoding of the 8-byte parent span id (16 chars).
    pub fn parent_span_id_hex(&self) -> String {
        hex::encode(self.parent_span_id)
    }

    /// Extract a vendor key from `trace_state`. `tracestate` is formatted
    /// as `key1=val1,key2=val2`. Returns the value for the first matching
    /// key (case-sensitive, since W3C keys are case-sensitive per spec).
    pub fn tracestate_value(&self, key: &str) -> Option<String> {
        if self.trace_state.is_empty() {
            return None;
        }
        for pair in self.trace_state.split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                if k.trim() == key {
                    return Some(v.trim().to_string());
                }
            }
        }
        None
    }
}

/// Parse a W3C `traceparent` header. Returns `None` on any malformation
/// (wrong section count, wrong length, non-hex chars). Format:
/// `00-{32 hex trace id}-{16 hex span id}-{2 hex flags}`.
pub fn parse_traceparent(header: &str) -> Option<W3cContext> {
    let header = header.trim();
    let parts: Vec<&str> = header.split('-').collect();
    if parts.len() != 4
        || parts[0].len() != 2
        || parts[1].len() != 32
        || parts[2].len() != 16
        || parts[3].len() != 2
    {
        return None;
    }
    let mut trace_id = [0u8; 16];
    let mut parent_span_id = [0u8; 8];
    for (i, byte_str) in parts[1].as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(byte_str).ok()?;
        trace_id[i] = u8::from_str_radix(s, 16).ok()?;
    }
    for (i, byte_str) in parts[2].as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(byte_str).ok()?;
        parent_span_id[i] = u8::from_str_radix(s, 16).ok()?;
    }
    let flags = u8::from_str_radix(parts[3], 16).ok()?;
    Some(W3cContext {
        trace_id,
        parent_span_id,
        sampled: flags & 0x01 == 0x01,
        trace_state: String::new(),
    })
}

/// Build a `traceparent` header for the outbound request. The gateway's
/// span id becomes the parent for the upstream provider's call. `trace_id`
/// is propagated unchanged from the inbound context (W3C: all spans in
/// one trace share the trace id). `sampled` is preserved from the inbound
/// flags so the upstream knows whether to sample.
pub fn build_child_traceparent(
    trace_id: [u8; 16],
    gateway_span_id: [u8; 8],
    sampled: bool,
) -> String {
    let trace_hex = hex::encode(trace_id);
    let span_hex = hex::encode(gateway_span_id);
    let flags = if sampled { "01" } else { "00" };
    format!("00-{trace_hex}-{span_hex}-{flags}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_well_formed_traceparent() {
        let ctx = parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01",
        )
        .expect("well-formed traceparent parses");
        assert_eq!(ctx.trace_id[0], 0x0a);
        assert_eq!(ctx.trace_id[1], 0xf7);
        assert_eq!(ctx.parent_span_id[0], 0x00);
        assert_eq!(ctx.parent_span_id[1], 0xf0);
        assert!(ctx.sampled);
    }

    #[test]
    fn parse_unsampled_traceparent() {
        let ctx = parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-00",
        )
        .expect("flags=00 parses");
        assert!(!ctx.sampled);
    }

    #[test]
    fn parse_rejects_malformed_inputs() {
        assert!(parse_traceparent("garbage").is_none());
        assert!(parse_traceparent("00-short").is_none());
        // Wrong section count.
        assert!(parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7"
        )
        .is_none());
        // Non-hex chars.
        assert!(parse_traceparent(
            "00-xx-00f067aa0ba902b7-01"
        )
        .is_none());
    }

    #[test]
    fn build_child_round_trips_through_parse() {
        let trace_id = [0x0a; 16];
        let span_id = [0x42; 8];
        let tp = build_child_traceparent(trace_id, span_id, true);
        let ctx = parse_traceparent(&tp).expect("child traceparent parses");
        assert_eq!(ctx.trace_id, trace_id);
        assert_eq!(ctx.parent_span_id, span_id);
        assert!(ctx.sampled);
    }

    #[test]
    fn tracestate_value_finds_key() {
        let mut ctx = parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01",
        )
        .unwrap();
        ctx.trace_state =
            "opencode_user_id=alice,vendor=acme".to_string();
        assert_eq!(
            ctx.tracestate_value("opencode_user_id"),
            Some("alice".to_string())
        );
        assert_eq!(ctx.tracestate_value("vendor"), Some("acme".to_string()));
        assert_eq!(ctx.tracestate_value("missing"), None);
    }

    #[test]
    fn tracestate_value_handles_empty_state() {
        let ctx = parse_traceparent(
            "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01",
        )
        .unwrap();
        assert_eq!(ctx.tracestate_value("any"), None);
    }
}
