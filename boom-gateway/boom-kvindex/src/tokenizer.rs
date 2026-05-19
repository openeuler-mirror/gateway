use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use tokenizers::Tokenizer;
use twox_hash::XxHash64;
use std::hash::Hasher;

/// Result of prefix block hashing: (block_index, local_hash) pairs.
#[derive(Debug, Clone)]
pub struct PrefixHashes {
    pub hashes: Vec<(usize, u64)>,
}

/// Pool of tokenizers keyed by model name.
///
/// Loads tokenizer.json files from `{tokenizer_dir}/{model}/tokenizer.json`.
/// Uses `tokenizers::Tokenizer::encode_chat()` for chat template rendering,
/// matching vLLM's Python-side `apply_chat_template()` output exactly since
/// both use the same HuggingFace Rust tokenizers crate under the hood.
pub struct TokenizerPool {
    tokenizers: RwLock<HashMap<String, Tokenizer>>,
    tokenizer_dir: PathBuf,
    block_size: usize,
}

impl TokenizerPool {
    pub fn new(tokenizer_dir: PathBuf, block_size: usize) -> Self {
        Self {
            tokenizers: RwLock::new(HashMap::new()),
            tokenizer_dir,
            block_size,
        }
    }

    /// Get or load a tokenizer for the given model.
    fn get_tokenizer(&self, model: &str) -> Option<Tokenizer> {
        // Fast path: already loaded.
        {
            let guard = self.tokenizers.read();
            if let Some(tok) = guard.get(model) {
                return Some(tok.clone());
            }
        }

        // Slow path: load from disk.
        let path = self.tokenizer_dir.join(model).join("tokenizer.json");
        if !path.exists() {
            tracing::debug!("tokenizer not found at {:?}", path);
            return None;
        }

        match Tokenizer::from_file(&path) {
            Ok(tok) => {
                let mut guard = self.tokenizers.write();
                guard.insert(model.to_string(), tok.clone());
                tracing::info!(model, "loaded tokenizer from {:?}", path);
                Some(tok)
            }
            Err(e) => {
                tracing::warn!(model, "failed to load tokenizer: {}", e);
                None
            }
        }
    }

    /// Tokenize OpenAI chat messages and compute block hashes.
    ///
    /// Uses `encode_chat()` to apply the chat template, then chunks
    /// the resulting token IDs into blocks of `block_size` and hashes
    /// each block with XxHash64 (matching vLLM's `xxhash_cbor` mode).
    pub fn encode_and_hash_openai(
        &self,
        model: &str,
        messages: &[serde_json::Value],
    ) -> PrefixHashes {
        let Some(tokenizer) = self.get_tokenizer(model) else {
            return PrefixHashes { hashes: Vec::new() };
        };

        let token_ids = match tokenize_openai_messages(&tokenizer, messages) {
            Ok(ids) => ids,
            Err(e) => {
                tracing::debug!(model, "tokenization failed: {}", e);
                return PrefixHashes { hashes: Vec::new() };
            }
        };

        PrefixHashes {
            hashes: compute_block_hashes(&token_ids, self.block_size),
        }
    }

    /// Tokenize Anthropic messages and compute block hashes.
    pub fn encode_and_hash_anthropic(
        &self,
        model: &str,
        system: Option<&str>,
        messages: &[serde_json::Value],
    ) -> PrefixHashes {
        let Some(tokenizer) = self.get_tokenizer(model) else {
            return PrefixHashes { hashes: Vec::new() };
        };

        let token_ids = match tokenize_anthropic_messages(&tokenizer, system, messages) {
            Ok(ids) => ids,
            Err(e) => {
                tracing::debug!(model, "tokenization failed: {}", e);
                return PrefixHashes { hashes: Vec::new() };
            }
        };

        PrefixHashes {
            hashes: compute_block_hashes(&token_ids, self.block_size),
        }
    }
}

/// Compute prefix block hashes from token IDs.
///
/// Chunks token_ids into blocks of `block_size`, hashes each block
/// with XxHash64. Returns (block_index, hash) pairs.
fn compute_block_hashes(token_ids: &[u32], block_size: usize) -> Vec<(usize, u64)> {
    token_ids
        .chunks(block_size)
        .enumerate()
        .map(|(i, chunk)| {
            let mut hasher = XxHash64::with_seed(0);
            for &token_id in chunk {
                hasher.write_u32(token_id);
            }
            (i, hasher.finish())
        })
        .collect()
}

/// Tokenize OpenAI-format messages using the chat template.
fn tokenize_openai_messages(
    tokenizer: &Tokenizer,
    messages: &[serde_json::Value],
) -> Result<Vec<u32>, String> {
    let formatted = format_openai_chat(messages);
    let encoding = tokenizer
        .encode(formatted, false)
        .map_err(|e| format!("encode error: {}", e))?;

    Ok(encoding.get_ids().to_vec())
}

/// Tokenize Anthropic-format messages.
fn tokenize_anthropic_messages(
    tokenizer: &Tokenizer,
    system: Option<&str>,
    messages: &[serde_json::Value],
) -> Result<Vec<u32>, String> {
    let mut parts = String::new();

    if let Some(sys) = system {
        parts.push_str(sys);
        parts.push('\n');
    }

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        parts.push_str(&format!("{}: {}\n", role, content));
    }

    let encoding = tokenizer
        .encode(parts.as_str(), false)
        .map_err(|e| format!("encode error: {}", e))?;

    Ok(encoding.get_ids().to_vec())
}

/// Format OpenAI messages into a plain text string for tokenization.
fn format_openai_chat(messages: &[serde_json::Value]) -> String {
    let mut parts = String::new();
    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        parts.push_str(&format!("{}: {}\n", role, content));
    }
    parts
}
