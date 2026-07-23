//! API key format parsing.
//!
//! Central definition of the two key shapes the gateway accepts:
//! - **Legacy**: `sk-{32 hex}` — the entire raw string participates in the
//!   SHA-256 hash. Matches the pre-prefix behavior byte-for-byte.
//! - **Prefixed**: `sk-{prefix}-{secret}` — only `secret` participates in the
//!   hash; `prefix` is metadata for display/audit only.
//!
//! Discrimination between the two is driven by the presence of a `-`
//! separator after `sk-`. Since hex (the legacy secret encoding) never
//! contains `-`, the parser is unambiguous.

/// Result of parsing a raw API key string.
///
/// Determines what gets fed into SHA-256 for DB lookup:
/// - `Legacy`: the entire raw_key is hashed (matches pre-prefix behavior, so
///   old keys keep working byte-for-byte).
/// - `Prefixed`: only the secret portion is hashed; the prefix is metadata
///   for display/audit only and never participates in the hash.
///
/// Discrimination between the two is driven by the presence of a `-`
/// separator after `sk-`. Since hex (the legacy secret encoding) never
/// contains `-`, the parser is unambiguous.
pub enum ParsedKey<'a> {
    /// Hash the entire raw_key. Used when key has no `sk-` prefix, when the
    /// remainder contains no `-`, or when the would-be prefix fails the
    /// `[a-zA-Z0-9]{1,8}` charset check.
    Legacy,
    /// Key in `sk-{prefix}-{secret}` form. Hash only `secret`.
    Prefixed { prefix: &'a str, secret: &'a str },
}

/// Parse a raw API key into Legacy or Prefixed form.
///
/// See [`ParsedKey`] for the contract.
pub fn parse_raw_key(raw: &str) -> ParsedKey<'_> {
    let Some(rest) = raw.strip_prefix("sk-") else {
        return ParsedKey::Legacy;
    };
    let Some((prefix, secret)) = rest.split_once('-') else {
        return ParsedKey::Legacy;
    };
    let prefix_valid = (1..=8).contains(&prefix.len())
        && !secret.is_empty()
        && prefix.chars().all(|c| c.is_ascii_alphanumeric());
    if prefix_valid {
        ParsedKey::Prefixed { prefix, secret }
    } else {
        ParsedKey::Legacy
    }
}

/// Validate that a candidate prefix string is acceptable for a new key.
///
/// Same charset/length rule as [`parse_raw_key`]: ASCII alphanumeric
/// (uppercase allowed), 1–8 chars. Used by dashboard key-creation handlers
/// to reject invalid prefixes with a 400 rather than silently falling back
/// to the legacy form.
pub fn is_valid_prefix(p: &str) -> bool {
    (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legacy_key_without_dash() {
        // 32 hex chars, no '-' — exact pre-prefix shape.
        let raw = "sk-0123456789abcdef0123456789abcdef";
        assert!(matches!(parse_raw_key(raw), ParsedKey::Legacy));
    }

    #[test]
    fn parses_prefixed_key() {
        let raw = "sk-teama-0123456789abcdef0123456789abcdef";
        match parse_raw_key(raw) {
            ParsedKey::Prefixed { prefix, secret } => {
                assert_eq!(prefix, "teama");
                assert_eq!(secret, "0123456789abcdef0123456789abcdef");
            }
            _ => panic!("expected Prefixed"),
        }
    }

    #[test]
    fn accepts_single_char_prefix() {
        // 1-char prefix is now valid (charset widened, lower bound dropped to 1).
        match parse_raw_key("sk-p-abcdef") {
            ParsedKey::Prefixed { prefix, secret } => {
                assert_eq!(prefix, "p");
                assert_eq!(secret, "abcdef");
            }
            _ => panic!("expected Prefixed"),
        }
    }

    #[test]
    fn accepts_uppercase_prefix() {
        // Charset is now [a-zA-Z0-9], so mixed case is fine.
        match parse_raw_key("sk-TeamA-aabbccdd") {
            ParsedKey::Prefixed { prefix, secret } => {
                assert_eq!(prefix, "TeamA");
                assert_eq!(secret, "aabbccdd");
            }
            _ => panic!("expected Prefixed"),
        }
    }

    #[test]
    fn rejects_non_sk_prefix_as_legacy() {
        // No `sk-` prefix at all — fall back to legacy so caller hashes the
        // whole string (used by master_key path, which never reaches here).
        assert!(matches!(parse_raw_key("master-xxx"), ParsedKey::Legacy));
        assert!(matches!(parse_raw_key("plain"), ParsedKey::Legacy));
    }

    #[test]
    fn rejects_too_long_prefix_as_legacy() {
        // 9 chars: over the 8-char cap.
        let long = "a".repeat(9);
        let raw = format!("sk-{}-secret", long);
        assert!(matches!(parse_raw_key(&raw), ParsedKey::Legacy));
    }

    #[test]
    fn rejects_symbol_prefix_as_legacy() {
        // Charset is [a-zA-Z0-9] only — underscores, dots, dashes inside
        // the prefix are rejected.
        assert!(matches!(parse_raw_key("sk-team_a-secret"), ParsedKey::Legacy));
        assert!(matches!(parse_raw_key("sk-team.a-secret"), ParsedKey::Legacy));
    }

    #[test]
    fn accepts_eight_char_prefix_boundary() {
        // Exactly 8 chars — boundary should pass.
        match parse_raw_key("sk-prod123-aabbccdd") {
            ParsedKey::Prefixed { prefix, .. } => assert_eq!(prefix, "prod123"),
            _ => panic!("expected Prefixed"),
        }
    }

    #[test]
    fn accepts_numeric_prefix() {
        match parse_raw_key("sk-01-abcdef") {
            ParsedKey::Prefixed { prefix, secret } => {
                assert_eq!(prefix, "01");
                assert_eq!(secret, "abcdef");
            }
            _ => panic!("expected Prefixed"),
        }
    }

    #[test]
    fn accepts_mixed_alphanumeric_prefix() {
        match parse_raw_key("sk-prod7-aabbccdd") {
            ParsedKey::Prefixed { prefix, secret } => {
                assert_eq!(prefix, "prod7");
                assert_eq!(secret, "aabbccdd");
            }
            _ => panic!("expected Prefixed"),
        }
    }

    #[test]
    fn empty_secret_after_dash_is_legacy() {
        // `sk-prefix-` with nothing after — invalid, fall back.
        assert!(matches!(parse_raw_key("sk-team-"), ParsedKey::Legacy));
    }

    #[test]
    fn is_valid_prefix_matches_parse_semantics() {
        assert!(is_valid_prefix("a"));
        assert!(is_valid_prefix("ab"));
        assert!(is_valid_prefix("abc123"));
        assert!(is_valid_prefix("TeamA"));
        assert!(is_valid_prefix(&"a".repeat(8)));
        assert!(!is_valid_prefix(&"a".repeat(9)));
        assert!(!is_valid_prefix(""));
        assert!(!is_valid_prefix("team_a"));
        assert!(!is_valid_prefix("team.a"));
    }
}
