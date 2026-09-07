use dashmap::DashMap;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_ENTRIES_PER_KEY: usize = 3;

/// In-memory store for debug error recordings.
///
/// When enabled (via admin API), upstream errors are captured with full
/// request bodies for diagnosis. Each key retains at most 3 entries (FIFO).
#[derive(Debug)]
pub struct DebugErrorStore {
    enabled: AtomicBool,
    /// request_id → entry
    entries: DashMap<String, DebugErrorEntry>,
    /// key_hash → ordered list of request_ids (newest last)
    key_index: DashMap<String, VecDeque<String>>,
}

/// A single captured debug error with full context.
#[derive(Debug, Clone, Serialize)]
pub struct DebugErrorEntry {
    pub request_id: String,
    pub key_hash: String,
    pub key_alias: Option<String>,
    pub model: String,
    pub api_path: String,
    pub is_stream: bool,
    pub created_at: String,
    pub status_code: u16,
    pub error_type: String,
    pub error_message: String,
    pub upstream_status: Option<u16>,
    pub upstream_body: Option<String>,
    pub request_body: Option<String>,
}

impl DebugErrorStore {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            entries: DashMap::new(),
            key_index: DashMap::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        if !enabled {
            self.entries.clear();
            self.key_index.clear();
        }
    }

    /// Record a debug error entry. Evicts oldest entries per key if over limit.
    pub fn record(&self, entry: DebugErrorEntry) {
        if !self.is_enabled() {
            return;
        }

        let request_id = entry.request_id.clone();
        let key_hash = entry.key_hash.clone();

        // Insert into main store.
        self.entries.insert(request_id.clone(), entry);

        // Update key index.
        let mut ids = self
            .key_index
            .entry(key_hash.clone())
            .or_insert_with(VecDeque::new);

        ids.push_back(request_id.clone());

        // Evict oldest entries if over limit.
        while ids.len() > MAX_ENTRIES_PER_KEY {
            if let Some(old_id) = ids.pop_front() {
                self.entries.remove(&old_id);
            }
        }
    }

    /// Look up a single entry by request_id.
    pub fn get(&self, request_id: &str) -> Option<DebugErrorEntry> {
        self.entries.get(request_id).map(|r| r.value().clone())
    }

    /// List all entries for a given key.
    pub fn list_for_key(&self, key_hash: &str) -> Vec<DebugErrorEntry> {
        self.key_index
            .get(key_hash)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.entries.get(id).map(|r| r.value().clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Clear all entries.
    pub fn clear(&self) {
        self.entries.clear();
        self.key_index.clear();
    }

    /// Total number of stored entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Default for DebugErrorStore {
    fn default() -> Self {
        Self::new()
    }
}
