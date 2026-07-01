use serde::{Deserialize, Serialize};
use std::collections::HashSet;

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

/// A single KV-cache event reported by an inference engine worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GatewayKvEvent {
    /// A KV-cache block was stored on a worker.
    #[serde(rename = "store")]
    Store {
        /// Model name this block belongs to.
        model: String,
        worker_id: String,
        /// Sequence-level hash (identifies the full request sequence).
        sequence_hash: String,
        /// Prefix hash (identifies the prefix shared across blocks).
        prefix_hash: String,
        /// Block hash from vLLM (used for EvictBlocks removal lookup).
        local_hash: u64,
        /// Parent block hash. `None` for root blocks (first block of a new sequence),
        /// `Some(hash)` for continuation blocks that chain to a parent.
        #[serde(default)]
        parent_hash: Option<u64>,
        /// Block index within the sequence (0-based).
        block_index: u64,
        /// Token IDs in this block.
        token_ids: Vec<u32>,
        /// Tokens per block (typically 16).
        block_size: u32,
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

    /// Specific KV-cache blocks were evicted from a worker (per-block granularity).
    #[serde(rename = "evict_blocks")]
    EvictBlocks {
        model: String,
        worker_id: String,
        /// Block hashes that were evicted.
        block_hashes: Vec<u64>,
        /// The storage tier from which blocks were removed.
        /// When present, only removes the worker if the tier matches,
        /// preventing incorrect removal during GPU↔CPU swap.
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

/// Result of a KV-cache prefix match lookup.
#[derive(Debug, Clone)]
pub struct KvMatchResult {
    /// Worker ID with the best match.
    pub worker_id: String,
    /// Number of consecutive prefix blocks matched (hit blocks).
    pub match_depth: u64,
    /// Total prefix blocks in the request (input blocks). hit_ratio =
    /// match_depth / total_blocks. Stored raw so the dashboard can derive
    /// the ratio without re-tokenizing.
    pub total_blocks: u64,
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

/// Backend interface for the KV-cache prefix index.
///
/// Implemented by TokenPrefixIndex (in boom-kvindex) and consumed by KvcAwarePolicy
/// (in boom-routing). Defined in boom-core so both crates can access it
/// without creating a circular dependency.
pub trait KvIndexBackend: Send + Sync {
    /// Apply a single KV event to the index.
    fn apply_event(&self, event: &GatewayKvEvent);

    /// Find workers with the best KV-cache prefix match for the given token IDs.
    fn find_matches(
        &self,
        model: &str,
        token_ids: &[u32],
        candidate_worker_ids: &[String],
    ) -> Vec<KvMatchResult>;

    /// Remove all index entries for a worker.
    fn remove_worker(&self, worker_id: &str);

    /// Total number of indexed blocks.
    fn block_count(&self) -> usize;

    /// Number of prefix blocks the request's token_ids hash into (the input
    /// block count for DFX). Same as request_hashes.len() inside find_matches;
    /// exposed so callers can record it even when no worker matched (empty
    /// find_matches result). Default 0 for backends without a block notion.
    fn prefix_block_count(&self, _token_ids: &[u32]) -> u64 {
        0
    }

    /// Return all model names currently tracked.
    fn model_names(&self) -> HashSet<String>;

    /// Dump index state for debugging: (model, trie_key, workers, tier, depth).
    fn debug_dump(&self) -> Vec<(String, u64, Vec<String>, StorageTier, u64)>;

    /// Latest reported scheduling queue depth for a worker (from LoadMetrics
    /// events). Used to load-balance auxiliary calls (e.g. `/tokenize`) across
    /// vLLM endpoints. Returns None when no metrics have arrived yet.
    fn queue_depth_for(&self, _worker_id: &str) -> Option<u64> {
        None
    }

    /// Maximum number of blocks the index can hold (LRU capacity). 0 if the
    /// backend doesn't enforce a limit. Reported in the selection log so
    /// `block_count / block_capacity` shows how close the trie is to evicting.
    fn block_capacity(&self) -> usize {
        0
    }
}
