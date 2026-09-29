//! DT 用例 — boom-kvindex：token 前缀 trie KV-cache 亲和索引。
//! 覆盖 Store/StoreBatch/Remove/EvictBlocks 事件、find_matches 命中/未命中、
//! LRU 驱逐、TTL prune、record_request_prefix、计数器。

use boom_core::kv_event::{BatchBlock, GatewayKvEvent, KvIndexBackend, StorageTier};
use boom_kvindex::TokenPrefixIndex;

const BLOCK: usize = 4;

fn store_event(model: &str, worker: &str, parent_hash: Option<u64>, bytes: Vec<u8>) -> GatewayKvEvent {
    GatewayKvEvent::Store {
        model: model.to_string(),
        worker_id: worker.to_string(),
        sequence_hash: String::new(),
        prefix_hash: String::new(),
        local_hash: 0,
        parent_hash,
        block_index: 0,
        block_bytes: bytes,
        block_size: BLOCK as u32,
        storage_tier: StorageTier::Gpu,
    }
}

fn store_batch_event(model: &str, worker: &str, chunks: Vec<Vec<u8>>) -> GatewayKvEvent {
    GatewayKvEvent::StoreBatch {
        model: model.to_string(),
        worker_id: worker.to_string(),
        blocks: chunks
            .into_iter()
            .map(|b| BatchBlock {
                local_hash: 0,
                parent_hash: None,
                block_bytes: b,
                block_size: BLOCK as u32,
                storage_tier: StorageTier::Gpu,
            })
            .collect(),
    }
}

/// DT-KV-01：单 root block 存入后 find_matches 命中同前缀。
#[test]
fn single_root_block_match() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_event("m", "w0", None, vec![1, 2, 3, 4]));
    let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].worker_id, "w0");
    assert_eq!(matches[0].match_depth, 1);
    assert!((matches[0].hit_ratio - 1.0).abs() < 1e-9);
}

/// DT-KV-02：多块链式前缀，find_matches 命中深度等于共享块数。
#[test]
fn chained_prefix_partial_match() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    // 存 3 个链式块：b0(root) -> b1 -> b2
    idx.apply_event(&store_event("m", "w0", None, vec![1, 2, 3, 4]));
    idx.apply_event(&store_event("m", "w0", Some(hash_of(&[1, 2, 3, 4])), vec![5, 6, 7, 8]));
    idx.apply_event(&store_event(
        "m",
        "w0",
        Some(hash_of(&[5, 6, 7, 8])),
        vec![9, 10, 11, 12],
    ));
    // 查询前 2 块 → 命中深度 2，total_blocks=2 → ratio=1.0
    let matches = idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8], &["w0".to_string()]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].match_depth, 2);
    assert!((matches[0].hit_ratio - 1.0).abs() < 1e-9);
}

/// DT-KV-03：StoreBatch 一次存多块，等价于多次 Store。
#[test]
fn store_batch_equivalent_to_stores() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_batch_event(
        "m",
        "w0",
        vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8], vec![9, 10, 11, 12]],
    ));
    let matches = idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8], &["w0".to_string()]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].match_depth, 2);
    // block_count 应为 3
    assert_eq!(idx.block_count(), 3);
}

/// DT-KV-04：未注册模型的 find_matches 返回空。
#[test]
fn find_matches_unknown_model_returns_empty() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_event("m", "w0", None, vec![1, 2, 3, 4]));
    let matches = idx.find_matches("other", &[1, 2, 3, 4], &["w0".to_string()]);
    assert!(matches.is_empty());
}

/// DT-KV-05：前缀不匹配的 worker 不出现在结果中。
#[test]
fn find_matches_excludes_non_matching_worker() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_event("m", "w0", None, vec![1, 2, 3, 4]));
    idx.apply_event(&store_event("m", "w1", None, vec![9, 9, 9, 9]));
    let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string(), "w1".to_string()]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].worker_id, "w0");
}

/// DT-KV-06：Remove 事件清空 worker 全部 claim，之后 find_matches 不命中。
#[test]
fn remove_worker_clears_claims() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_event("m", "w0", None, vec![1, 2, 3, 4]));
    assert_eq!(idx.block_count(), 1);
    idx.apply_event(&GatewayKvEvent::Remove {
        worker_id: "w0".to_string(),
        sequence_hash: String::new(),
        storage_tier: None,
    });
    assert_eq!(idx.block_count(), 0);
    let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
    assert!(matches.is_empty());
}

/// DT-KV-07：EvictBlocks 按 hash 精确驱逐单个块。
#[test]
fn evict_blocks_removes_specific_hash() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_batch_event(
        "m",
        "w0",
        vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]],
    ));
    assert_eq!(idx.block_count(), 2);
    // 驱逐第二个块（effective_hash = hash_of([5,6,7,8])，因为 local_hash=0）
    idx.apply_event(&GatewayKvEvent::EvictBlocks {
        model: "m".to_string(),
        worker_id: "w0".to_string(),
        block_hashes: vec![hash_of(&[5, 6, 7, 8])],
        storage_tier: None,
    });
    assert_eq!(idx.block_count(), 1);
    // 第一个块仍可命中
    let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
    assert_eq!(matches.len(), 1);
}

/// DT-KV-08：LRU 容量满时驱逐最旧块。
#[test]
fn lru_evicts_oldest_when_full() {
    // max_blocks = 2
    let idx = TokenPrefixIndex::new(BLOCK, 2);
    idx.apply_event(&store_event("m", "w0", None, vec![1, 2, 3, 4]));
    idx.apply_event(&store_event("m", "w0", None, vec![5, 6, 7, 8]));
    assert_eq!(idx.block_count(), 2);
    // 第三块触发 LRU 驱逐最旧
    idx.apply_event(&store_event("m", "w0", None, vec![9, 10, 11, 12]));
    assert_eq!(idx.block_count(), 2);
    // 第一块被驱逐，find_matches 不命中
    let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string()]);
    assert!(matches.is_empty());
}

/// DT-KV-09：record_request_prefix 直接记录前缀（不经事件）。
#[test]
fn record_request_prefix_stores_blocks() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8], StorageTier::Gpu);
    assert_eq!(idx.block_count(), 2);
    let matches = idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8], &["w0".to_string()]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].match_depth, 2);
}

/// DT-KV-10：prefix_block_count 按 block_size 切分计数。
#[test]
fn prefix_block_count_chunks_by_block_size() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    assert_eq!(idx.prefix_block_count(&[1, 2, 3, 4, 5, 6, 7, 8, 9]), 2); // 9 bytes / 4 = 2 full
    assert_eq!(idx.prefix_block_count(&[]), 0);
    assert_eq!(idx.prefix_block_count(&[1, 2]), 0);
}

/// DT-KV-11：空输入/空候选 find_matches 返回空。
#[test]
fn find_matches_empty_inputs_return_empty() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_event("m", "w0", None, vec![1, 2, 3, 4]));
    assert!(idx.find_matches("m", &[], &["w0".to_string()]).is_empty());
    assert!(idx.find_matches("m", &[1, 2, 3, 4], &[]).is_empty());
}

/// DT-KV-12：block_bytes 为空的 Store 事件被忽略。
#[test]
fn empty_block_bytes_store_is_ignored() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_event("m", "w0", None, vec![]));
    assert_eq!(idx.block_count(), 0);
    assert_eq!(idx.node_count(), 0);
}

/// DT-KV-13：node_count 跟踪 trie 节点数（root + 内部 + 叶子）。
#[test]
fn node_count_tracks_trie_nodes() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_batch_event(
        "m",
        "w0",
        vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]],
    ));
    // root(1) + b0(1) + b1(1) = 3
    assert_eq!(idx.node_count(), 3);
}

/// DT-KV-14：model_names 返回所有注册过的模型。
#[test]
fn model_names_lists_registered_models() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_event("m1", "w0", None, vec![1, 2, 3, 4]));
    idx.apply_event(&store_event("m2", "w0", None, vec![1, 2, 3, 4]));
    let names = idx.model_names();
    assert_eq!(names.len(), 2);
    assert!(names.contains("m1"));
    assert!(names.contains("m2"));
}

/// DT-KV-15：block_capacity 返回 LRU 容量配置。
#[test]
fn block_capacity_returns_configured_max() {
    let idx = TokenPrefixIndex::new(BLOCK, 42);
    assert_eq!(idx.block_capacity(), 42);
}

/// DT-KV-16：同一前缀多 worker 命中时按 combined_score 降序排列。
#[test]
fn multiple_workers_sorted_by_score() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    // w0 有 3 块链，w1 只有 1 块
    idx.apply_event(&store_batch_event(
        "m",
        "w0",
        vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8], vec![9, 10, 11, 12]],
    ));
    idx.apply_event(&store_event("m", "w1", None, vec![1, 2, 3, 4]));
    // 查询 3 块：w0 命中深度 3，w1 命中深度 1
    let matches = idx.find_matches(
        "m",
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        &["w0".to_string(), "w1".to_string()],
    );
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].worker_id, "w0"); // 高分在前
    assert_eq!(matches[0].match_depth, 3);
    assert_eq!(matches[1].worker_id, "w1");
    assert_eq!(matches[1].match_depth, 1);
}

/// DT-KV-17：TTL prune 过期块后不再命中。
#[test]
fn prune_expired_removes_old_blocks() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_event("m", "w0", None, vec![1, 2, 3, 4]));
    assert_eq!(idx.block_count(), 1);
    // TTL = 0 → 立即过期
    idx.prune_expired(std::time::Duration::from_secs(0));
    assert_eq!(idx.block_count(), 0);
}

/// DT-KV-18：debug_dump 返回带 worker claim 的节点信息。
#[test]
fn debug_dump_returns_populated_nodes() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_event("m", "w0", None, vec![1, 2, 3, 4]));
    let dump = idx.debug_dump();
    assert_eq!(dump.len(), 1);
    assert_eq!(dump[0].0, "m");
    assert!(dump[0].2.contains(&"w0".to_string()));
}

fn hash_of(bytes: &[u8]) -> u64 {
    twox_hash::xxhash3_64::Hasher::oneshot(bytes)
}
