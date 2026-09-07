//! API key prefix validation.
//!
//! Hash input is always the entire raw key string (e.g. `sk-{prefix}-{secret}`
//! or `sk-{secret}`), matching litellm's `hash_token` byte-for-byte. The
//! prefix is purely a display/audit field stored in `boom_verification_token.key_prefix`
//! — tamper detection is implicit: any change to the prefix changes the hash,
//! so the DB lookup naturally fails.

/// Validate that a candidate prefix string is acceptable for a new key.
///
/// ASCII alphanumeric (uppercase allowed), 1–50 chars. Used by dashboard
/// key-creation handlers to reject invalid prefixes with a 400.
pub fn is_valid_prefix(p: &str) -> bool {
    (1..=50).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_prefix_accepts_legal_prefixes() {
        assert!(is_valid_prefix("a"));
        assert!(is_valid_prefix("ab"));
        assert!(is_valid_prefix("abc123"));
        assert!(is_valid_prefix("TeamA"));
        assert!(is_valid_prefix(&"a".repeat(8)));
        assert!(is_valid_prefix(&"a".repeat(50)));
    }

    #[test]
    fn is_valid_prefix_rejects_illegal_prefixes() {
        assert!(!is_valid_prefix(&"a".repeat(51)));
        assert!(!is_valid_prefix(""));
        assert!(!is_valid_prefix("team_a"));
        assert!(!is_valid_prefix("team.a"));
        assert!(!is_valid_prefix("team-a"));
    }
}
