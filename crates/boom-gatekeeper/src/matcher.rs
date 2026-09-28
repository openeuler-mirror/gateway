//! Field resolution and operator application for block rules.
//!
//! Field addressing (see `boom_config::ClientBlocklistConfig` docs):
//! - `header.<name>` — looked up via the caller-provided closure; axum's
//!   `HeaderMap::get` is case-insensitive, so the lookup closure should be too.
//! - `body.<json_pointer>` — dotted path resolved as a JSON pointer
//!   (`body.messages.0.content` → `/messages/0/content`). The extracted value
//!   is stringified: strings verbatim, everything else JSON-encoded.
//! - `body` — the whole request body serialized as text (lazily, once).

use boom_config::{BlockOp, MatchCondition};
use serde_json::Value;

pub enum FieldValue {
    Missing,
    Text(String),
}

/// Resolve a condition's `field` against the request. `header_lookup` receives
/// the header name as written in the rule; `body_text` is a lazily-filled
/// cache for whole-body matching (pass `&mut None` and it will be populated on
/// first use).
pub fn resolve_field(
    field: &str,
    header_lookup: &dyn Fn(&str) -> Option<String>,
    body: &Value,
    body_text: &mut Option<String>,
) -> FieldValue {
    if let Some(name) = field.strip_prefix("header.") {
        return match header_lookup(name) {
            Some(v) => FieldValue::Text(v),
            None => FieldValue::Missing,
        };
    }
    if field == "body" {
        if body_text.is_none() {
            *body_text = Some(serde_json::to_string(body).unwrap_or_default());
        }
        return FieldValue::Text(body_text.clone().unwrap_or_default());
    }
    if let Some(path) = field.strip_prefix("body.") {
        // Dotted path → JSON pointer: "messages.0.content" → "/messages/0/content"
        let pointer = format!("/{}", path.replace('.', "/"));
        return match body.pointer(&pointer) {
            Some(Value::String(s)) => FieldValue::Text(s.clone()),
            Some(other) => FieldValue::Text(other.to_string()),
            None => FieldValue::Missing,
        };
    }
    // Unknown selector form — treat as missing so a typo'd rule never matches.
    FieldValue::Missing
}

/// Apply a condition's operator to a resolved field value.
pub fn apply_op(op: BlockOp, field: &FieldValue, expected: &str, regex: Option<&regex::Regex>) -> bool {
    match op {
        BlockOp::Exists => matches!(field, FieldValue::Text(_)),
        BlockOp::Eq => match field {
            FieldValue::Text(actual) => actual == expected,
            FieldValue::Missing => false,
        },
        BlockOp::Contains => match field {
            FieldValue::Text(actual) => actual.contains(expected),
            FieldValue::Missing => false,
        },
        BlockOp::Prefix => match field {
            FieldValue::Text(actual) => actual.starts_with(expected),
            FieldValue::Missing => false,
        },
        BlockOp::Regex => match (field, regex) {
            (FieldValue::Text(actual), Some(re)) => re.is_match(actual),
            _ => false,
        },
    }
}

/// Evaluate a single condition (resolve + apply).
pub fn eval_condition(
    cond: &MatchCondition,
    regex: Option<&regex::Regex>,
    header_lookup: &dyn Fn(&str) -> Option<String>,
    body: &Value,
    body_text: &mut Option<String>,
) -> bool {
    let field = resolve_field(&cond.field, header_lookup, body, body_text);
    apply_op(cond.op, &field, &cond.value, regex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.to_string())
        }
    }

    fn body() -> Value {
        json!({
            "model": "gpt-4o",
            "system": ["part one", "part two"],
            "messages": [
                {"role": "user", "content": "You are Cursor"}
            ],
            "tools": [{"name": "bash"}],
            "metadata": null
        })
    }

    #[test]
    fn header_lookup_is_case_insensitive_via_closure() {
        let h = headers(&[("User-Agent", "Cursor/0.45")]);
        let mut cache = None;
        let f = resolve_field("header.user-agent", &h, &body(), &mut cache);
        assert!(matches!(f, FieldValue::Text(ref s) if s == "Cursor/0.45"));
        let missing = resolve_field("header.x-absent", &h, &body(), &mut cache);
        assert!(matches!(missing, FieldValue::Missing));
    }

    #[test]
    fn body_pointer_stringifies_strings_verbatim_and_others_as_json() {
        let b = body();
        let h = headers(&[]);
        let mut cache = None;
        // String field → verbatim.
        let f = resolve_field("body.model", &h, &b, &mut cache);
        assert!(matches!(f, FieldValue::Text(ref s) if s == "gpt-4o"));
        // Array index works, nested content extracted verbatim.
        let f = resolve_field("body.messages.0.content", &h, &b, &mut cache);
        assert!(matches!(f, FieldValue::Text(ref s) if s == "You are Cursor"));
        // Non-string values are JSON-encoded (array of system blocks).
        let f = resolve_field("body.system", &h, &b, &mut cache);
        assert!(matches!(f, FieldValue::Text(ref s) if s.contains("part one") && s.starts_with('[')));
        // Whole tools array.
        let f = resolve_field("body.tools", &h, &b, &mut cache);
        assert!(matches!(f, FieldValue::Text(ref s) if s.contains("bash")));
    }

    #[test]
    fn null_pointer_counts_as_existing_and_encodes_as_null() {
        let b = body();
        let h = headers(&[]);
        let mut cache = None;
        let f = resolve_field("body.metadata", &h, &b, &mut cache);
        assert!(matches!(f, FieldValue::Text(ref s) if s == "null"));
        assert!(apply_op(BlockOp::Exists, &f, "", None));
    }

    #[test]
    fn body_full_text_matches_across_fields_and_is_cached() {
        let b = body();
        let h = headers(&[]);
        let mut cache = None;
        let f = resolve_field("body", &h, &b, &mut cache);
        assert!(matches!(f, FieldValue::Text(ref s) if s.contains("gpt-4o") && s.contains("Cursor")));
        assert!(cache.is_some(), "full-body serialization must be cached");
    }

    #[test]
    fn op_matrix() {
        let text = FieldValue::Text("Cursor/0.45".to_string());
        let missing = FieldValue::Missing;
        assert!(apply_op(BlockOp::Eq, &text, "Cursor/0.45", None));
        assert!(!apply_op(BlockOp::Eq, &text, "cursor/0.45", None));
        assert!(apply_op(BlockOp::Contains, &text, "Cursor", None));
        assert!(!apply_op(BlockOp::Contains, &missing, "Cursor", None));
        assert!(apply_op(BlockOp::Prefix, &text, "Cursor/", None));
        assert!(!apply_op(BlockOp::Prefix, &text, "/0.45", None));
        assert!(apply_op(BlockOp::Exists, &text, "", None));
        assert!(!apply_op(BlockOp::Exists, &missing, "", None));

        let re = regex::Regex::new("(?i)cursor").unwrap();
        assert!(apply_op(BlockOp::Regex, &text, "", Some(&re)));
        assert!(!apply_op(BlockOp::Regex, &missing, "", Some(&re)));
        // Missing compiled regex never matches (defensive).
        assert!(!apply_op(BlockOp::Regex, &text, "", None));
    }

    #[test]
    fn unknown_field_selector_is_missing() {
        let h = headers(&[]);
        let mut cache = None;
        let f = resolve_field("cookies.jar", &h, &body(), &mut cache);
        assert!(matches!(f, FieldValue::Missing));
    }
}
