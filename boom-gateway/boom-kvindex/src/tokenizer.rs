use minijinja::value::{Enumerator, Object, ObjectRepr};
use minijinja::{Environment, Value};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokenizers::Tokenizer;

/// Result of tokenization: raw token IDs.
#[derive(Debug, Clone)]
pub struct PrefixTokens {
    pub token_ids: Vec<u32>,
}

/// Per-model loaded assets.
struct ModelAssets {
    tokenizer: Arc<Tokenizer>,
    /// Jinja2 chat template string from tokenizer_config.json.
    chat_template: Option<String>,
    /// BOS token string (e.g., "<|endoftext|>").
    bos_token: String,
    /// EOS token string (e.g., "<|im_end|>").
    eos_token: String,
}

/// Pool of tokenizers keyed by model name.
///
/// Loads from `{tokenizer_dir}/{model}/`:
/// - `tokenizer.json` — tokenizer for encoding
/// - `tokenizer_config.json` — chat_template for message rendering
pub struct TokenizerPool {
    assets: RwLock<HashMap<String, Arc<ModelAssets>>>,
    tokenizer_dir: PathBuf,
}

impl TokenizerPool {
    pub fn new(tokenizer_dir: PathBuf) -> Self {
        Self {
            assets: RwLock::new(HashMap::new()),
            tokenizer_dir,
        }
    }

    fn get_assets(&self, model: &str) -> Option<Arc<ModelAssets>> {
        {
            let guard = self.assets.read();
            if let Some(a) = guard.get(model) {
                return Some(Arc::clone(a));
            }
        }

        let dir = self.tokenizer_dir.join(model);

        // Load tokenizer.json
        let tokenizer_path = dir.join("tokenizer.json");
        if !tokenizer_path.exists() {
            tracing::warn!("tokenizer not found at {:?}", tokenizer_path);
            return None;
        }
        let tokenizer = match Tokenizer::from_file(&tokenizer_path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(model, "failed to load tokenizer: {}", e);
                return None;
            }
        };

        // Load chat_template and special tokens from tokenizer_config.json
        let (chat_template, bos_token, eos_token) = load_tokenizer_config(&dir);
        match &chat_template {
            Some(t) => tracing::info!(
                model,
                tmpl_len = t.len(),
                bos = %bos_token,
                eos = %eos_token,
                "loaded chat_template"
            ),
            None => tracing::warn!(model, "no chat_template found, using simple format"),
        }

        let assets = Arc::new(ModelAssets {
            tokenizer: Arc::new(tokenizer),
            chat_template,
            bos_token,
            eos_token,
        });

        let mut guard = self.assets.write();
        guard.insert(model.to_string(), Arc::clone(&assets));
        Some(assets)
    }

    /// Tokenize OpenAI chat messages using the model's chat_template.
    pub fn tokenize_openai(
        &self,
        model: &str,
        messages: &[serde_json::Value],
        tools: Option<&[serde_json::Value]>,
    ) -> PrefixTokens {
        let Some(assets) = self.get_assets(model) else {
            return PrefixTokens { token_ids: Vec::new() };
        };

        let text = if let Some(ref tmpl) = assets.chat_template {
            match render_chat_template(tmpl, &assets.bos_token, &assets.eos_token, messages, true, tools) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(model, error = %e, "chat_template render failed, falling back to simple format");
                    format_openai_chat(messages)
                }
            }
        } else {
            format_openai_chat(messages)
        };

        let token_ids = match assets.tokenizer.encode(text.as_str(), false) {
            Ok(enc) => enc.get_ids().to_vec(),
            Err(e) => {
                tracing::warn!(model, "tokenization failed: {}", e);
                return PrefixTokens { token_ids: Vec::new() };
            }
        };

        tracing::debug!(
            model,
            tokens = token_ids.len(),
            has_tools = tools.is_some(),
            tools_count = tools.map(|t| t.len()).unwrap_or(0),
            "openai tokenized"
        );

        PrefixTokens { token_ids }
    }

    /// Decode token IDs back to text for a model. Used by break-point
    /// diagnostics to surface the actual diverging text. Returns None if the
    /// model's tokenizer isn't loaded or decoding fails.
    pub fn decode(&self, model: &str, token_ids: &[u32]) -> Option<String> {
        let assets = self.get_assets(model)?;
        if let Ok(s) = assets.tokenizer.decode(token_ids, false) {
            return Some(s);
        }
        assets.tokenizer.decode(token_ids, true).ok()
    }
}

/// Load chat_template, bos_token, eos_token from tokenizer_config.json.
/// Falls back to chat_template.jinja file if no template in config.
fn load_tokenizer_config(dir: &PathBuf) -> (Option<String>, String, String) {
    let config_path = dir.join("tokenizer_config.json");
    if !config_path.exists() {
        // No config file — try chat_template.jinja directly.
        let template = try_load_jinja_template(dir);
        return (template, String::new(), String::new());
    }

    let data = match std::fs::read_to_string(&config_path) {
        Ok(d) => d,
        Err(_) => {
            let template = try_load_jinja_template(dir);
            return (template, String::new(), String::new());
        }
    };
    let config: serde_json::Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => {
            let template = try_load_jinja_template(dir);
            return (template, String::new(), String::new());
        }
    };

    // Extract chat_template
    let chat_template = match config.get("chat_template") {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Array(templates)) => {
            let mut found = None;
            for tmpl in templates {
                let name = tmpl.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if name == "default" || name.is_empty() {
                    if let Some(s) = tmpl.get("template").and_then(|t| t.as_str()) {
                        found = Some(s.to_string());
                        break;
                    }
                }
            }
            found.or_else(|| {
                templates
                    .first()
                    .and_then(|t| t.get("template"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
        }
        _ => None,
    };

    // Fall back to chat_template.jinja if not found in config.
    let chat_template = chat_template.or_else(|| try_load_jinja_template(dir));

    // Extract bos_token and eos_token (can be string or object with "content" field).
    let bos_token = extract_token_str(&config, "bos_token");
    let eos_token = extract_token_str(&config, "eos_token");

    (chat_template, bos_token, eos_token)
}

/// Try loading chat_template from a standalone .jinja file.
fn try_load_jinja_template(dir: &PathBuf) -> Option<String> {
    let jinja_path = dir.join("chat_template.jinja");
    if jinja_path.exists() {
        match std::fs::read_to_string(&jinja_path) {
            Ok(s) => {
                tracing::info!(path = ?jinja_path, "loaded chat_template from .jinja file");
                Some(s)
            }
            Err(e) => {
                tracing::warn!(path = ?jinja_path, error = %e, "failed to read chat_template.jinja");
                None
            }
        }
    } else {
        None
    }
}

fn extract_token_str(config: &serde_json::Value, key: &str) -> String {
    match config.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(obj) => obj
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        None => String::new(),
    }
}

/// A minijinja Map object that preserves JSON insertion order.
///
/// minijinja deserializes maps into a `BTreeMap` (`ValueMap` in value/mod.rs),
/// which sorts keys alphabetically — but vLLM (Python dicts) preserves
/// insertion order. To make the gateway's `| tojson` output byte-identical to
/// vLLM's, objects that may be serialized via `| tojson` are wrapped in this
/// type instead of going through `Value::from_serialize`.
///
/// Why it works: `Serialize for Value` serializes a Map object by iterating
/// `try_iter_pairs()`, which yields keys from `enumerate()` and values from
/// `get_value()` in the SAME order the enumerator emits. `Enumerator::Values`
/// emits the keys here verbatim in insertion order, so the resulting JSON keeps
/// insertion order. Field access (`tool.function`) resolves via `get_value`,
/// so templates that do `{{ tool.function | tojson }}` (MiniMax) work too.
#[derive(Debug)]
struct OrderedObject(Vec<(Arc<str>, Value)>);

impl Object for OrderedObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }
    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let keys: Vec<Value> = self.0.iter().map(|(k, _)| Value::from(k.to_string())).collect();
        Enumerator::Values(keys)
    }
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let k = key.as_str()?;
        self.0.iter().find_map(|(name, v)| (&**name == k).then(|| v.clone()))
    }
    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        Some(self.0.len())
    }
}

/// A minijinja Seq object wrapping a fixed `Vec<Value>` without going through
/// serde (which would re-sort any Map elements). Used so the `tools` array can
/// hold `OrderedObject` elements and stay iterable as a sequence.
#[derive(Debug)]
struct OrderedSeq(Vec<Value>);

impl Object for OrderedSeq {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }
    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Seq(self.0.len())
    }
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let idx = key.as_usize()?;
        self.0.get(idx).cloned()
    }
    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        Some(self.0.len())
    }
}

/// Convert a serde_json::Value into a minijinja Value that preserves object
/// insertion order. Objects become `OrderedObject`, arrays become `OrderedSeq`,
/// scalars become plain values. Used for any data a template may emit via
/// `| tojson` (tools, and recursively their nested fields).
fn serde_to_ordered(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::default(),
        serde_json::Value::Bool(b) => Value::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Value::from(u)
            } else if let Some(i) = n.as_i64() {
                Value::from(i)
            } else {
                Value::from(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::from(s),
        serde_json::Value::Array(arr) => {
            Value::from_object(OrderedSeq(arr.iter().map(serde_to_ordered).collect()))
        }
        serde_json::Value::Object(obj) => {
            let pairs = obj
                .iter()
                .map(|(k, val)| {
                    // OpenAI encodes tool-call arguments as a JSON *string*
                    // (`function.arguments = "{\"cmd\":\"...\"}"`), but chat
                    // templates iterate them as a dict (e.g. MiniMax:
                    // `{% for k, v in tool_call.arguments.items() %}`). vLLM/HF
                    // parse the string to a dict before rendering; mirror that
                    // here so `.items()` yields parameters instead of nothing.
                    let converted = if k == "arguments" {
                        if let Some(s) = val.as_str() {
                            serde_json::from_str::<serde_json::Value>(s)
                                .ok()
                                .filter(|p| p.is_object() || p.is_array())
                                .map(|p| serde_to_ordered(&p))
                                .unwrap_or_else(|| serde_to_ordered(val))
                        } else {
                            serde_to_ordered(val)
                        }
                    } else {
                        serde_to_ordered(val)
                    };
                    (Arc::<str>::from(k.as_str()), converted)
                })
                .collect();
            Value::from_object(OrderedObject(pairs))
        }
    }
}

/// Build the template context (`Value`) feeding `messages` through serde
/// (templates access messages field-by-field, so key order is irrelevant) but
/// feeding `tools` as order-preserving `OrderedObject`s (templates serialize
/// tools via `| tojson`, where key order must match vLLM).
///
/// Built with `Value::from_object` rather than the `context!` macro — the
/// macro routes every value through `Value::from_serialize`, which would
/// re-sort the OrderedObjects back into a BTreeMap.
fn build_template_context(
    messages: &[serde_json::Value],
    add_generation_prompt: bool,
    bos_token: &str,
    eos_token: &str,
    tools: Option<&[serde_json::Value]>,
) -> Value {
    let tools_value = match tools {
        Some(t) => Value::from_object(OrderedSeq(t.iter().map(serde_to_ordered).collect())),
        None => Value::default(),
    };
    let ctx = OrderedObject(vec![
        (
            Arc::<str>::from("messages"),
            Value::from_object(OrderedSeq(
                messages.iter().map(|m| serde_to_ordered(m)).collect(),
            )),
        ),
        (
            Arc::<str>::from("add_generation_prompt"),
            Value::from(add_generation_prompt),
        ),
        (Arc::<str>::from("bos_token"), Value::from(bos_token)),
        (Arc::<str>::from("eos_token"), Value::from(eos_token)),
        (Arc::<str>::from("tools"), tools_value),
    ]);
    Value::from_object(ctx)
}

/// Render messages through a Jinja2 chat template (HuggingFace format).
fn render_chat_template(
    template: &str,
    bos_token: &str,
    eos_token: &str,
    messages: &[serde_json::Value],
    add_generation_prompt: bool,
    tools: Option<&[serde_json::Value]>,
) -> Result<String, String> {
    // Preprocess template to replace Python Jinja2 constructs that minijinja
    // doesn't support with equivalent minijinja-compatible syntax.
    let preprocessed = preprocess_template(template);

    let mut env = Environment::new();
    // Match vLLM's Jinja2 configuration: strip first newline after block tags
    // and leading whitespace before block tags on the same line.
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);

    // Jinja2 dicts have a built-in `.items()` method; minijinja does not. Some
    // chat templates call it (e.g. MiniMax renders tool-call arguments via
    // `{% for k, v in tool_call.arguments.items() %}`). Register a callback
    // that routes `.items()` on a Map to minijinja's `|items` filter, matching
    // vLLM/Python behavior.
    env.set_unknown_method_callback(|state, value, method, _args| {
        if value.kind() == minijinja::value::ValueKind::Map && method == "items" {
            state.apply_filter("items", &[value.clone()])
        } else {
            Err(minijinja::Error::new(
                minijinja::ErrorKind::UnknownMethod,
                format!("unknown method {method}"),
            ))
        }
    });

    // Add custom filters for Python str method compatibility.
    // Match vLLM/HuggingFace tojson filter: sort_keys=False + separators=(', ', ': ')
    // so that gateway tokenization produces identical output to vLLM's rendering.
    //
    // Object key order is preserved by feeding tools (and any other tojson'd
    // object) as `OrderedObject`s (see build_template_context). minijinja's
    // `Serialize for Value` walks a Map object via its own `try_iter_pairs`,
    // which follows the object's enumerator order — so an insertion-ordered
    // enumerator yields insertion-ordered JSON. This makes BOTH template styles
    // produce byte-identical output to vLLM:
    //   - whole-object:  `{{ tool | tojson }}`            (Qwen3, GLM, …)
    //   - field-access:  `{{ tool.function | tojson }}`   (MiniMax-M2.7)
    // The legacy pre-serialized-string passthrough below is retained only as a
    // defensive fast path and is no longer how tools are supplied.
    env.add_filter("tojson", |v: Value, _ensure_ascii: Option<bool>| {
        if let Some(s) = v.as_str() {
            let trimmed = s.trim();
            if (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'))
            {
                return s.to_string();
            }
        }
        let compact = serde_json::to_string(&v).unwrap_or_else(|_| v.to_string());
        python_compat_json(&compact)
    });
    env.add_filter("strip", |v: String, chars: Option<String>| -> String {
        match chars.as_deref() {
            Some(c) if !c.is_empty() => v.trim_matches(|ch: char| c.contains(ch)).to_string(),
            _ => v.trim().to_string(),
        }
    });
    env.add_filter("rstrip", |v: String, chars: Option<String>| -> String {
        match chars.as_deref() {
            Some(c) if !c.is_empty() => v.trim_end_matches(|ch: char| c.contains(ch)).to_string(),
            _ => v.trim_end().to_string(),
        }
    });
    env.add_filter("lstrip", |v: String, chars: Option<String>| -> String {
        match chars.as_deref() {
            Some(c) if !c.is_empty() => v.trim_start_matches(|ch: char| c.contains(ch)).to_string(),
            _ => v.trim_start().to_string(),
        }
    });
    env.add_filter("startswith", |v: String, prefix: String| -> bool {
        v.starts_with(&prefix)
    });
    env.add_filter("endswith", |v: String, suffix: String| -> bool {
        v.ends_with(&suffix)
    });
    // nth_split: split string by sep and return the N-th element.
    // Preprocessing converts `expr.split(sep)[N]` → `expr|nth_split(sep, N)`.
    env.add_filter("nth_split", |v: String, sep: String, index: i64| -> String {
        let parts: Vec<&str> = v.split(&sep).collect();
        let idx = if index < 0 {
            (parts.len() as i64 + index).max(0) as usize
        } else {
            index as usize
        };
        parts.get(idx).map_or(String::new(), |s| s.to_string())
    });

    // Add split as a function (not method) — template preprocessing converts
    // `content.split(sep)` to `split(content, sep)`.
    env.add_function("split", |v: String, sep: String| -> Vec<String> {
        v.split(&sep).map(|s| s.to_string()).collect()
    });

    env.add_template("chat", &preprocessed)
        .map_err(|e| format!("invalid template: {e}"))?;

    let tmpl = env
        .get_template("chat")
        .map_err(|e| format!("get template: {e}"))?;

    let ctx = build_template_context(messages, add_generation_prompt, bos_token, eos_token, tools);

    tmpl.render(ctx)
        .map_err(|e| format!("render: {e}"))
}

/// Replace Python Jinja2 constructs with minijinja-compatible equivalents.
///
/// minijinja natively supports: namespace(), slicing (`[1:]`), negative indexing (`[-1]`),
/// `loop.index0`, `is string/iterable/mapping/defined` tests, `in` operator, `reverse` filter.
///
/// What we fix:
/// - `.split(sep)` method → `split(var, sep)` function
/// - `.split(sep)[N]` method+index → `|nth_split(sep, N)` filter
/// - `.strip(sep)` method → `|strip(sep)` filter
/// - `.rstrip(sep)` method → `|rstrip(sep)` filter
/// - `.lstrip(sep)` method → `|lstrip(sep)` filter
/// - `.startswith(prefix)` method → `|startswith(prefix)` filter
/// - `.endswith(suffix)` method → `|endswith(suffix)` filter
/// - `[::-1]` reverse slice → `|reverse` filter (built-in)
/// - `.items()` method → supported natively via unknown_method_callback (NOT stripped)
/// - `tojson(ensure_ascii=False)` → `tojson`
/// - `raise_exception(...)` → `''`
fn preprocess_template(template: &str) -> String {
    use std::sync::LazyLock;

    // Must run before SPLIT_RE: handle .split(X)[N] as a unit.
    static SPLIT_INDEX_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"\.split\(([^)]+)\)\[(-?\d+)\]"#).unwrap());
    static SPLIT_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"(\w+)\.split\(('[^']*'|"[^"]*")\)"#).unwrap());
    static STRIP_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"\.strip\(([^)]*)\)"#).unwrap());
    static RSTRIP_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"\.rstrip\(([^)]*)\)"#).unwrap());
    static LSTRIP_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"\.lstrip\(([^)]*)\)"#).unwrap());
    static STARTSWITH_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"\.startswith\(([^)]+)\)"#).unwrap());
    static ENDSWITH_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"\.endswith\(([^)]+)\)"#).unwrap());
    static REVERSE_SLICE_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\[::-1\]").unwrap());
    static CHAINED_SPLIT_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"\]\.split\(('[^']*'|"[^"]*")\)\[(-?\d+)\]"#).unwrap());
    static ITEMS_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(\w+)\.items\(\)").unwrap());
    static RAISE_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"raise_exception\([^)]*\)").unwrap());
    static TOJSON_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\| tojson\(ensure_ascii=False\)").unwrap());

    let mut result = template.to_string();

    // Order matters: SPLIT_INDEX_RE must run before SPLIT_RE.
    result = SPLIT_INDEX_RE.replace_all(&result, r#"|nth_split($1, $2)"#).into_owned();
    result = SPLIT_RE.replace_all(&result, r#"split($1, $2)"#).into_owned();
    result = STRIP_RE.replace_all(&result, r#"|strip($1)"#).into_owned();
    result = RSTRIP_RE.replace_all(&result, r#"|rstrip($1)"#).into_owned();
    result = LSTRIP_RE.replace_all(&result, r#"|lstrip($1)"#).into_owned();
    result = STARTSWITH_RE.replace_all(&result, r#"|startswith($1)"#).into_owned();
    result = ENDSWITH_RE.replace_all(&result, r#"|endswith($1)"#).into_owned();
    result = REVERSE_SLICE_RE.replace_all(&result, "|reverse").into_owned();
    result = CHAINED_SPLIT_RE.replace_all(&result, "]").into_owned();
    // NOTE: `.items()` is NOT stripped — it is supported at render time via the
    // unknown_method_callback registered in render_chat_template (routed to the
    // `|items` filter). Stripping it here would drop dict iteration used by
    // templates like MiniMax (`{% for k, v in args.items() %}`).
    let _ = &ITEMS_RE; // keep the static defined for reference / potential future use
    result = RAISE_RE.replace_all(&result, "''").into_owned();
    result = TOJSON_RE.replace_all(&result, "| tojson").into_owned();

    result
}

/// Convert compact JSON (serde_json output) to Python json.dumps default format.
///
/// Jinja2's tojson uses separators `(', ', ': ')` by default,
/// producing `{"key": "value", "num": 1}` while serde_json produces
/// `{"key":"value","num":1}`. This function adds spaces after structural
/// `:` and `,` without modifying content inside JSON strings.
fn python_compat_json(compact: &str) -> String {
    let mut out = String::with_capacity(compact.len() + compact.len() / 8);
    let mut in_string = false;
    let mut escape_next = false;

    for ch in compact.chars() {
        if escape_next {
            out.push(ch);
            escape_next = false;
            continue;
        }
        if ch == '\\' && in_string {
            out.push(ch);
            escape_next = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            out.push(ch);
            continue;
        }
        if !in_string {
            match ch {
                ':' => out.push_str(": "),
                ',' => out.push_str(", "),
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn format_openai_chat(messages: &[serde_json::Value]) -> String {
    let mut parts = String::new();
    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
        parts.push_str(&format!("{}: {}\n", role, content));
    }
    parts
}

#[cfg(test)]
mod tojson_tests {
    use super::*;

    /// A tool whose keys (name, description, parameters, ...) are intentionally
    /// NOT in alphabetical order — pins insertion-order preservation.
    fn sample_tool() -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather info",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        })
    }

    #[test]
    fn whole_object_tojson_preserves_insertion_order_qwen_style() {
        // Qwen3 / GLM style: `{{ tool | tojson }}` (whole object).
        let tools = vec![sample_tool()];
        let template = "{% for tool in tools %}{{ tool | tojson }}{% endfor %}";
        let rendered = render_chat_template(template, "", "", &[], true, Some(&tools)).unwrap();
        // type must precede function (insertion order, not alphabetical).
        let type_pos = rendered.find("\"type\"").unwrap();
        let func_pos = rendered.find("\"function\"").unwrap();
        assert!(type_pos < func_pos, "type must precede function: {rendered}");
        let name_pos = rendered.find("\"name\"").unwrap();
        let desc_pos = rendered.find("\"description\"").unwrap();
        let params_pos = rendered.find("\"parameters\"").unwrap();
        assert!(name_pos < desc_pos && desc_pos < params_pos,
            "function keys must keep insertion order name<description<parameters: {rendered}");
    }

    #[test]
    fn field_access_tojson_preserves_insertion_order_minimax_style() {
        // MiniMax style: `{{ tool.function | tojson }}` (field access). A JSON
        // string has no `.function` (would render null) — this pins that tools
        // reach the template as objects with resolvable, ordered sub-fields.
        let tools = vec![sample_tool()];
        let template = "{% for tool in tools %}<tool>{{ tool.function | tojson(ensure_ascii=False) }}</tool>{% endfor %}";
        let rendered = render_chat_template(template, "", "", &[], true, Some(&tools)).unwrap();
        assert!(!rendered.contains("null"), "field access yielded null: {rendered}");
        let name_pos = rendered.find("\"name\"").unwrap();
        let desc_pos = rendered.find("\"description\"").unwrap();
        let params_pos = rendered.find("\"parameters\"").unwrap();
        assert!(name_pos < desc_pos && desc_pos < params_pos,
            "function keys must keep insertion order: {rendered}");
    }

    #[test]
    fn both_styles_render_identical_function_json() {
        // Cross-model compatibility crux: the function object rendered via
        // field access (MiniMax) must be byte-identical to the function object
        // embedded inside the whole-tool rendering (Qwen3) — one mechanism,
        // both template styles.
        let tools = vec![sample_tool()];
        let via_field = render_chat_template(
            "{% for tool in tools %}{{ tool.function | tojson }}{% endfor %}",
            "", "", &[], true, Some(&tools),
        ).unwrap();
        let whole = render_chat_template(
            "{% for tool in tools %}{{ tool | tojson }}{% endfor %}",
            "", "", &[], true, Some(&tools),
        ).unwrap();
        // The field-rendered function JSON must appear verbatim inside the
        // whole-tool rendering (i.e. the same bytes, same key order).
        assert!(
            whole.contains(&via_field),
            "field-access output must be a substring of whole-tool output.\n\
             field: {via_field}\nwhole: {whole}"
        );
    }

    #[test]
    fn assistant_tool_call_arguments_render_as_parameters() {
        // MiniMax renders assistant tool_calls via dict `.items()` on
        // `tool_call.arguments` (a JSON *string* in OpenAI format). vLLM/HF
        // parse the string to a dict first; the gateway must too, or the
        // parameters vanish and the rendered `<invoke>` is empty (diverging
        // from vLLM's cache at every assistant tool call).
        let arguments = r#"{"command": "git clone x", "description": "Clone repo"}"#;
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "bash", "arguments": arguments}
            }]
        })];
        // Mirrors the MiniMax tool-call rendering: iterate arguments.items().
        let template = "{% for m in messages %}{% for tc in m.tool_calls %}{% set fn = tc.function %}<invoke name=\"{{ fn.name }}\">{% for k, v in fn.arguments.items() %}<parameter name=\"{{ k }}\">{{ v }}</parameter>{% endfor %}</invoke>{% endfor %}{% endfor %}";
        let rendered = render_chat_template(template, "", "", &messages, true, None).unwrap();
        assert!(rendered.contains("<parameter name=\"command\">git clone x</parameter>"),
            "arguments not rendered as parameters (string not parsed to dict?): {rendered}");
        assert!(rendered.contains("<parameter name=\"description\">Clone repo</parameter>"),
            "missing description parameter: {rendered}");
    }

    #[test]
    fn test_tojson_matches_vllm() {
        // vLLM/HuggingFace tojson: no key sorting, separators=(', ', ': ')
        let tool = serde_json::json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather info",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "city": {"type": "string"}
                    }
                }
            }
        });
        let compact = serde_json::to_string(&tool).unwrap();
        let result = python_compat_json(&compact);

        // Insertion order preserved, spaces added after : and ,
        let expected = r#"{"type": "function", "function": {"name": "get_weather", "description": "Get weather info", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}"#;
        assert_eq!(result, expected, "Gateway tojson must match vLLM output");
    }

    #[test]
    fn test_python_compat_json_preserves_strings() {
        // Colons and commas inside strings must NOT be modified
        let compact = r#"{"desc":"a: b, c, d","name":"test"}"#;
        // After sort + spaces: {"desc": "a: b, c, d", "name": "test"}
        let result = python_compat_json(compact);
        assert_eq!(result, r#"{"desc": "a: b, c, d", "name": "test"}"#);
    }
}
