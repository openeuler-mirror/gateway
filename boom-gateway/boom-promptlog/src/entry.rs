use serde::{Deserialize, Serialize};

/// A single prompt log entry — one line in a JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptLogEntry {
    pub request_id: String,
    pub timestamp: String,
    pub key_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_alias: Option<String>,
    pub model: String,
    pub api_path: String,
    pub is_stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Original request body (OpenAI or Anthropic format, stored as-is).
    pub request: serde_json::Value,
    /// Full response body (non-stream) or accumulated content (stream).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<serde_json::Value>,
}

impl PromptLogEntry {
    pub fn new(
        request_id: &str,
        key_hash: &str,
        team_alias: Option<&str>,
        model: &str,
        api_path: &str,
        is_stream: bool,
        request_body: serde_json::Value,
    ) -> Self {
        Self {
            request_id: request_id.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            key_hash: key_hash.to_string(),
            team_alias: team_alias.map(|s| s.to_string()),
            model: model.to_string(),
            api_path: api_path.to_string(),
            is_stream,
            status_code: None,
            duration_ms: None,
            error_message: None,
            request: request_body,
            response: None,
        }
    }

    pub fn set_response(&mut self, response: serde_json::Value) {
        self.response = Some(response);
    }

    pub fn set_status(&mut self, status_code: i32, duration_ms: u64) {
        self.status_code = Some(status_code);
        self.duration_ms = Some(duration_ms);
    }

    pub fn set_error(&mut self, error_message: String) {
        self.error_message = Some(error_message);
    }
}
