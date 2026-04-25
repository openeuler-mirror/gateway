use serde::Deserialize;

/// Prompt logging configuration.
///
/// ```yaml
/// prompt_log:
///   enabled: false
///   dir: "/data/prompt_logs"
///   max_file_size_mb: 50
///   excluded_keys: []
///   excluded_teams: []
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct PromptLogConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_dir")]
    pub dir: String,
    #[serde(default = "default_max_file_size")]
    pub max_file_size_mb: u64,
    #[serde(default)]
    pub excluded_keys: Vec<String>,
    #[serde(default)]
    pub excluded_teams: Vec<String>,
}

impl Default for PromptLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_dir(),
            max_file_size_mb: default_max_file_size(),
            excluded_keys: Vec::new(),
            excluded_teams: Vec::new(),
        }
    }
}

fn default_dir() -> String {
    "/data/prompt_logs".to_string()
}

fn default_max_file_size() -> u64 {
    50
}

impl PromptLogConfig {
    /// Check if a request from this key/team should be logged.
    pub fn should_capture(&self, key_hash: &str, team_id: Option<&str>) -> bool {
        if !self.enabled {
            return false;
        }
        if self.excluded_keys.iter().any(|k| k == key_hash) {
            return false;
        }
        if let Some(tid) = team_id {
            if self.excluded_teams.iter().any(|t| t == tid) {
                return false;
            }
        }
        true
    }

    /// Builder: return a copy with the global enabled flag changed.
    pub fn with_enabled(&self, enabled: bool) -> Self {
        let mut c = self.clone();
        c.enabled = enabled;
        c
    }

    /// Builder: return a copy with a key added to or removed from the exclusion list.
    pub fn with_key_excluded(&self, key_hash: &str, excluded: bool) -> Self {
        let mut c = self.clone();
        if excluded {
            if !c.excluded_keys.iter().any(|k| k == key_hash) {
                c.excluded_keys.push(key_hash.to_string());
            }
        } else {
            c.excluded_keys.retain(|k| k != key_hash);
        }
        c
    }

    /// Builder: return a copy with a team added to or removed from the exclusion list.
    pub fn with_team_excluded(&self, team_id: &str, excluded: bool) -> Self {
        let mut c = self.clone();
        if excluded {
            if !c.excluded_teams.iter().any(|t| t == team_id) {
                c.excluded_teams.push(team_id.to_string());
            }
        } else {
            c.excluded_teams.retain(|t| t != team_id);
        }
        c
    }
}
