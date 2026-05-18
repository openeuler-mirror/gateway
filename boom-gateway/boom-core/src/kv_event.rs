use serde::{Deserialize, Serialize};

/// Storage tier for KV cache blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageTier {
    Gpu,
    Cpu,
    Ssd,
    Remote,
}

impl StorageTier {
    /// Priority score for tier-aware routing (higher = faster access).
    pub fn priority_score(&self) -> f64 {
        match self {
            Self::Gpu => 1.0,
            Self::Cpu => 0.7,
            Self::Ssd => 0.4,
            Self::Remote => 0.2,
        }
    }
}

/// A single KV-cache event reported by a vLLM/Dynamo worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DynamoKvEvent {
    /// A KV-cache block was stored on a worker.
    #[serde(rename = "store")]
    Store {
        worker_id: String,
        /// Sequence-level hash (identifies the full request sequence).
        sequence_hash: String,
        /// Prefix hash (identifies the prefix shared across blocks).
        prefix_hash: String,
        /// Local hash of this specific block.
        local_hash: u64,
        /// Block index within the sequence (0-based).
        block_index: u64,
        /// Storage tier where this block resides.
        storage_tier: StorageTier,
    },

    /// KV-cache blocks were removed from a worker.
    #[serde(rename = "remove")]
    Remove {
        worker_id: String,
        sequence_hash: String,
        /// If provided, only remove blocks from this tier.
        storage_tier: Option<StorageTier>,
    },

    /// Load metrics snapshot from a worker.
    #[serde(rename = "load_metrics")]
    LoadMetrics {
        worker_id: String,
        /// Number of requests waiting in the scheduling queue.
        queue_depth: u64,
        /// KV-cache utilization ratio (0.0–1.0).
        kv_utilization: f64,
        /// Number of currently running requests.
        running_requests: u64,
        /// Per-tier usage: (tier, used_blocks, total_blocks).
        tier_usage: Vec<(StorageTier, u64, u64)>,
    },
}

/// A batch of KV events sent in a single HTTP request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamoEventBatch {
    pub events: Vec<DynamoKvEvent>,
}

/// Result of a KV-cache prefix match lookup.
#[derive(Debug, Clone)]
pub struct KvMatchResult {
    /// Worker ID with the best match.
    pub worker_id: String,
    /// Number of consecutive prefix blocks matched.
    pub match_depth: u64,
    /// Hit ratio: matched_blocks / total_blocks.
    pub hit_ratio: f64,
    /// Best storage tier where the matched prefix resides.
    pub best_tier: StorageTier,
    /// Tier score (0.0–1.0, higher is better).
    pub tier_score: f64,
    /// Load score (0.0–1.0, higher means more available).
    pub load_score: f64,
    /// Combined score (weighted sum of hit, tier, load).
    pub combined_score: f64,
}

/// Load state tracked per worker.
#[derive(Debug, Clone, Default)]
pub struct LoadState {
    pub queue_depth: u64,
    pub kv_utilization: f64,
    pub running_requests: u64,
}
