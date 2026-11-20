//! Deployment defaults for OpenAI-compatible request bodies.
use serde_json::{Map, Value};

/// Defaults cannot change routing, messages, transport mode or internal channels.
pub fn validate(defaults: &Map<String, Value>) -> Result<(), String> {
    for key in [
        "model",
        "messages",
        "stream",
        "extra",
        "extra_body",
        "gateway_headers",
        "kv_cache_report_full",
        "from_anthropic_protocol",
        "raw_capture",
    ] {
        if defaults.contains_key(key) {
            return Err(format!("request_defaults must not contain `{key}`"));
        }
    }
    for key in ["chat_template_kwargs", "reasoning"] {
        if defaults.get(key).is_some_and(|v| !v.is_object()) {
            return Err(format!("request_defaults.{key} must be a JSON object"));
        }
    }
    Ok(())
}

fn has_thinking_intent(body: &Map<String, Value>) -> bool {
    [
        "reasoning_effort",
        "enable_thinking",
        "thinking",
        "reasoning",
    ]
    .iter()
    .any(|key| body.contains_key(*key))
        || body
            .get("chat_template_kwargs")
            .and_then(Value::as_object)
            .is_some_and(|kwargs| {
                ["enable_thinking", "thinking", "reasoning_effort"]
                    .iter()
                    .any(|key| kwargs.contains_key(*key))
            })
}

fn thinking_key(path: &str, key: &str) -> bool {
    (path.is_empty()
        && [
            "reasoning_effort",
            "enable_thinking",
            "thinking",
            "reasoning",
        ]
        .contains(&key))
        || (path == "chat_template_kwargs"
            && ["enable_thinking", "thinking", "reasoning_effort"].contains(&key))
}

fn merge(
    body: &mut Map<String, Value>,
    defaults: &Map<String, Value>,
    path: &str,
    skip_thinking: bool,
) {
    for (key, value) in defaults {
        if skip_thinking && thinking_key(path, key) {
            continue;
        }
        if let Some(object) = value.as_object() {
            let next = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            // Do not insert empty containers when all default children were skipped.
            let mut missing = Map::new();
            match body.get_mut(key) {
                Some(Value::Object(existing)) => merge(existing, object, &next, skip_thinking),
                Some(_) => (), // Explicit null/scalar/array also wins.
                None => {
                    merge(&mut missing, object, &next, skip_thinking);
                    if !missing.is_empty() || object.is_empty() {
                        body.insert(key.clone(), Value::Object(missing));
                    }
                }
            }
        } else {
            body.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
}

/// Fill absent keys recursively; never overwrite explicit client intent.
/// Thinking switches across dialects form one group, so `none` cannot acquire
/// a contradictory default `enable_thinking: true` (or vice versa).
pub fn apply(body: &mut Value, defaults: &Map<String, Value>) {
    if let Some(body) = body.as_object_mut() {
        let skip_thinking = has_thinking_intent(body);
        merge(body, defaults, "", skip_thinking);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_defaults_preserve_explicit_values() {
        let defaults = json!({"temperature": 0.6, "chat_template_kwargs": {
            "enable_thinking": true, "nested": {"a": 1, "b": 2}, "keep": [1, 2]
        }});
        let mut body = json!({"temperature": 0, "chat_template_kwargs": {
            "nested": {"a": false}, "keep": null
        }});
        apply(&mut body, defaults.as_object().unwrap());
        assert_eq!(
            body,
            json!({"temperature": 0, "chat_template_kwargs": {
                "enable_thinking": true, "nested": {"a": false, "b": 2}, "keep": null
            }})
        );
    }

    #[test]
    fn thinking_intent_prevents_cross_dialect_conflicts() {
        for client in [
            json!({"reasoning_effort": "none"}),
            json!({"reasoning_effort": "high"}),
            json!({"chat_template_kwargs": {"enable_thinking": false}}),
            json!({"thinking": {"type": "disabled"}}),
        ] {
            for defaults in [
                json!({"reasoning_effort": "medium"}),
                json!({"chat_template_kwargs": {"enable_thinking": true}}),
            ] {
                let mut body = client.clone();
                apply(&mut body, defaults.as_object().unwrap());
                assert_eq!(body, client);
            }
        }
    }

    #[test]
    fn rejects_transport_and_internal_fields() {
        for defaults in [
            json!({"model": "other"}),
            json!({"messages": []}),
            json!({"stream": true}),
            json!({"extra_body": {}}),
            json!({"chat_template_kwargs": []}),
        ] {
            assert!(validate(defaults.as_object().unwrap()).is_err());
        }
        assert!(validate(json!({"reasoning_effort": "medium"}).as_object().unwrap()).is_ok());
    }
}
