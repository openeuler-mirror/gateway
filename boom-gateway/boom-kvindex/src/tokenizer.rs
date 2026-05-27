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
    ) -> PrefixTokens {
        let Some(assets) = self.get_assets(model) else {
            return PrefixTokens { token_ids: Vec::new() };
        };

        let text = if let Some(ref tmpl) = assets.chat_template {
            match render_chat_template(tmpl, &assets.bos_token, &assets.eos_token, messages, true) {
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

        tracing::info!(
            model,
            tokens = token_ids.len(),
            first_8 = ?&token_ids[..token_ids.len().min(8)],
            text_preview = &text[..text.len().min(100)],
            "openai tokenized"
        );
        PrefixTokens { token_ids }
    }

    /// Tokenize Anthropic messages using the model's chat_template.
    pub fn tokenize_anthropic(
        &self,
        model: &str,
        system: Option<&str>,
        messages: &[serde_json::Value],
    ) -> PrefixTokens {
        let Some(assets) = self.get_assets(model) else {
            return PrefixTokens { token_ids: Vec::new() };
        };

        // Build a merged message list: system as a system message + rest.
        let mut all_messages: Vec<serde_json::Value> = Vec::new();
        if let Some(sys) = system {
            all_messages.push(serde_json::json!({
                "role": "system",
                "content": sys
            }));
        }
        all_messages.extend_from_slice(messages);

        let text = if let Some(ref tmpl) = assets.chat_template {
            match render_chat_template(tmpl, &assets.bos_token, &assets.eos_token, &all_messages, true) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(model, error = %e, "chat_template render failed, falling back to simple format");
                    format_anthropic_chat(system, messages)
                }
            }
        } else {
            format_anthropic_chat(system, messages)
        };

        let token_ids = match assets.tokenizer.encode(text.as_str(), false) {
            Ok(enc) => enc.get_ids().to_vec(),
            Err(e) => {
                tracing::warn!(model, "tokenization failed: {}", e);
                return PrefixTokens { token_ids: Vec::new() };
            }
        };

        tracing::info!(
            model,
            tokens = token_ids.len(),
            first_8 = ?&token_ids[..token_ids.len().min(8)],
            "anthropic tokenized"
        );
        PrefixTokens { token_ids }
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

/// Render messages through a Jinja2 chat template (HuggingFace format).
fn render_chat_template(
    template: &str,
    bos_token: &str,
    eos_token: &str,
    messages: &[serde_json::Value],
    add_generation_prompt: bool,
) -> Result<String, String> {
    // Preprocess template to replace Python Jinja2 constructs that minijinja
    // doesn't support with equivalent minijinja-compatible syntax.
    let preprocessed = preprocess_template(template);

    let mut env = Environment::new();
    // Match vLLM's Jinja2 configuration: strip first newline after block tags
    // and leading whitespace before block tags on the same line.
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);

    // Add custom filters for Python compatibility.
    env.add_filter("tojson", |v: Value, _ensure_ascii: Option<bool>| {
        serde_json::to_string(&v).unwrap_or_else(|_| v.to_string())
    });
    env.add_filter("strip", |v: String| v.trim().to_string());

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

    let messages_value = Value::from_serialize(messages);

    let ctx = Value::from_serialize(serde_json::json!({
        "messages": messages_value,
        "add_generation_prompt": add_generation_prompt,
        "bos_token": bos_token,
        "eos_token": eos_token,
    }));

    tmpl.render(ctx)
        .map_err(|e| format!("render: {e}"))
}

/// Replace Python Jinja2 constructs with minijinja-compatible equivalents.
///
/// minijinja natively supports: namespace(), slicing (`[1:]`), negative indexing (`[-1]`),
/// `loop.index0`, `is string/iterable/mapping/defined` tests, `in` operator.
///
/// What we need to fix:
/// - `.split(sep)` method → `split(var, sep)` function
/// - `.strip(sep)` method → remove (close enough for tokenization)
/// - `.items()` method → `[]` (skip iteration)
/// - `tojson(ensure_ascii=False)` → `tojson`
/// - `raise_exception(...)` → `''`
fn preprocess_template(template: &str) -> String {
    use std::sync::LazyLock;

    static SPLIT_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"(\w+)\.split\(('[^']*'|"[^"]*")\)"#).unwrap());
    static STRIP_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"\.strip\((?:'[^']*'|"[^"]*")?\)"#).unwrap());
    static CHAINED_SPLIT_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"\]\.split\(('[^']*'|"[^"]*")\)\[(-?\d+)\]"#).unwrap());
    static ITEMS_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(\w+)\.items\(\)").unwrap());
    static RAISE_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"raise_exception\([^)]*\)").unwrap());
    static TOJSON_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\| tojson\(ensure_ascii=False\)").unwrap());

    let mut result = template.to_string();

    result = SPLIT_RE.replace_all(&result, r#"split($1, $2)"#).into_owned();
    result = STRIP_RE.replace_all(&result, "").into_owned();
    result = CHAINED_SPLIT_RE.replace_all(&result, "]").into_owned();
    result = ITEMS_RE.replace_all(&result, "[]").into_owned();
    result = RAISE_RE.replace_all(&result, "''").into_owned();
    result = TOJSON_RE.replace_all(&result, "| tojson").into_owned();

    result
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

fn format_anthropic_chat(system: Option<&str>, messages: &[serde_json::Value]) -> String {
    let mut parts = String::new();
    if let Some(sys) = system {
        parts.push_str(sys);
        parts.push('\n');
    }
    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
        parts.push_str(&format!("{}: {}\n", role, content));
    }
    parts
}
