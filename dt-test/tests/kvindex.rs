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

/// 链式块 key：hash(parent_effective ‖ content_hash)，与 crate 内
/// chain_block_hash 一致（第二块及以后的 EvictBlocks 需要它）。
fn chain_hash(parent: u64, content: u64) -> u64 {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&parent.to_le_bytes());
    buf[8..].copy_from_slice(&content.to_le_bytes());
    hash_of(&buf)
}

fn evict_event(model: &str, worker: &str, hashes: Vec<u64>) -> GatewayKvEvent {
    GatewayKvEvent::EvictBlocks {
        model: model.to_string(),
        worker_id: worker.to_string(),
        block_hashes: hashes,
        storage_tier: None,
    }
}

/// DT-KV-19：LRU 驱逐留下空壳节点，sweep_stale 自底向上回收。
/// max_blocks=1 时记录第二个独立块 → 第一个块被 LRU 驱逐，
/// 其节点变成无 worker 无子节点的 shell；sweep 后 node_count 回落。
#[test]
fn sweep_stale_reclaims_lru_shell() {
    let idx = TokenPrefixIndex::new(BLOCK, 1);
    idx.record_request_prefix("m", "w0", &[1, 2, 3, 4], StorageTier::Gpu);
    assert_eq!(idx.block_count(), 1);
    // root + 1 块节点
    assert_eq!(idx.node_count(), 2);
    // 记录第二个不同内容的块 → LRU 溢出驱逐第一个（claims=1，nodes=3：root+shell+新块）
    idx.record_request_prefix("m", "w0", &[5, 6, 7, 8], StorageTier::Gpu);
    assert_eq!(idx.block_count(), 1);
    assert_eq!(idx.node_count(), 3);
    // 首次 sweep：last_sweep_ms 为 sentinel → 立即执行，shell 被回收
    idx.sweep_stale();
    assert_eq!(idx.node_count(), 2, "LRU shell must be swept");
    assert_eq!(idx.block_count(), 1, "live claim untouched");
}

/// DT-KV-20：sweep 门控——无驱逐事件时 sweep 直接跳过（不遍历），
/// 驱逐后只跑一次，再次调用被 SWEEP_INTERVAL_MS 节流早退。
#[test]
fn sweep_stale_gate_and_throttle() {
    let idx = TokenPrefixIndex::new(BLOCK, 1_000_000);
    idx.record_request_prefix("m", "w0", &[1, 2, 3, 4], StorageTier::Gpu);
    // 无驱逐 → 门控关闭，sweep 不动节点（覆盖 gate 早退分支）
    idx.sweep_stale();
    assert_eq!(idx.node_count(), 2);
    // 驱逐 arm 门控（EvictBlocks 走 evict_single_block）
    let h = hash_of(&[1, 2, 3, 4]);
    idx.apply_event(&evict_event("m", "w0", vec![h]));
    assert_eq!(idx.block_count(), 0);
    // 第一次真实 sweep 运行并回收
    idx.sweep_stale();
    assert_eq!(idx.node_count(), 1);
    // 紧接着再触发：节流窗口内 → 早退（覆盖 throttle 分支）
    idx.sweep_stale();
    assert_eq!(idx.node_count(), 1);
}

/// DT-KV-21：remove_worker 清空全部 claim 后，sweep 整链回收（自底向上）。
#[test]
fn sweep_stale_after_remove_worker_unwinds_chain() {
    let idx = TokenPrefixIndex::new(BLOCK, 1_000_000);
    idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8], StorageTier::Gpu);
    assert_eq!(idx.block_count(), 2);
    assert_eq!(idx.node_count(), 3);
    idx.remove_worker("w0");
    assert_eq!(idx.block_count(), 0);
    // 链上所有节点都是空壳（父有子 → deepest-first 逐层回收）
    idx.sweep_stale();
    assert_eq!(idx.node_count(), 1, "whole chain swept, only root remains");
}

/// DT-KV-22：TTL prune 全量过期 + sweep 回收（wholesale expiry 路径）。
#[test]
fn sweep_stale_after_ttl_prune() {
    let idx = TokenPrefixIndex::new(BLOCK, 1_000_000);
    idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8], StorageTier::Gpu);
    // 空索引 prune：expired 为空 → 早退分支
    let empty = TokenPrefixIndex::new(BLOCK, 10);
    empty.prune_expired(std::time::Duration::from_secs(60));
    // TTL=0 → 全部过期（覆盖 PruneTimers::pop_expired 的到期循环）
    idx.prune_expired(std::time::Duration::ZERO);
    assert_eq!(idx.block_count(), 0);
    idx.sweep_stale();
    assert_eq!(idx.node_count(), 1);
}

/// DT-KV-23：EvictBlocks 带 hash=0（跳过）与未知 hash（block_lookup 未命中早退）；
/// 链上第二块驱逐用 chain-scoped key。
#[test]
fn evict_blocks_zero_and_unknown_hash_noop() {
    let idx = TokenPrefixIndex::new(BLOCK, 1_000_000);
    idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8], StorageTier::Gpu);
    // hash=0 → continue（两个循环各一次）；未知 hash → evict_single_block 早退
    idx.apply_event(&evict_event("m", "w0", vec![0, 999_999]));
    assert_eq!(idx.block_count(), 2, "no-op evictions must not drop claims");
    // 第二块（链式 key）精确驱逐
    let h1 = hash_of(&[1, 2, 3, 4]);
    let h2 = chain_hash(h1, hash_of(&[5, 6, 7, 8]));
    idx.apply_event(&evict_event("m", "w0", vec![h2]));
    assert_eq!(idx.block_count(), 1);
    // lru_evict_block 的 hash==0 早退
    idx.apply_event(&evict_event("m", "w0", vec![0]));
    assert_eq!(idx.block_count(), 1);
}

/// DT-KV-24：record 路径的边界——空前缀、小于一个 block 的前缀、
/// find_matches 短前缀均安全返回。
#[test]
fn record_and_find_edge_inputs() {
    let idx = TokenPrefixIndex::new(BLOCK, 1_000_000);
    // 空前缀 → record 早退
    idx.record_request_prefix("m", "w0", &[], StorageTier::Gpu);
    assert_eq!(idx.block_count(), 0);
    // 小于 block_size 的非空前缀 → n_full=0 → apply_prepared(空)
    idx.record_request_prefix("m", "w0", &[1, 2], StorageTier::Gpu);
    assert_eq!(idx.block_count(), 0);
    // find_matches 短前缀 → 空
    assert!(idx.find_matches("m", &[1], &["w0".to_string()]).is_empty());
    // StoreBatch 空 blocks → 早退
    idx.apply_event(&GatewayKvEvent::StoreBatch {
        model: "m".to_string(),
        worker_id: "w0".to_string(),
        blocks: vec![],
    });
    assert_eq!(idx.block_count(), 0);
}

/// DT-KV-25：record 路径 LRU 溢出（apply_prepared 内的驱逐处理）——
/// 记录超过 max_blocks 的链，旧块被驱逐且 node_count 被 sweep 收敛。
#[test]
fn record_path_lru_overflow_evicts_oldest() {
    let idx = TokenPrefixIndex::new(BLOCK, 2);
    // 4 块链，容量 2 → 记录过程中前两块被驱逐
    idx.record_request_prefix("m", "w0", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16], StorageTier::Gpu);
    assert_eq!(idx.block_count(), 2);
    // 命中只可能在后两块（前缀前两块已被驱逐 → 链在头部断开）
    assert!(idx.find_matches("m", &[1, 2, 3, 4, 5, 6, 7, 8], &["w0".to_string()]).is_empty());
    idx.sweep_stale();
    // root + 未回收的链节点中，被驱逐块的后代若仍有 claim 则保留；
    // 这里后两块有 claim → 至少 root+2 存活
    assert!(idx.node_count() >= 3);
}

/// DT-KV-26：候选 worker 无命中（depth=0 continue）时结果只含有命中的 worker。
#[test]
fn find_matches_skips_zero_depth_workers() {
    let idx = TokenPrefixIndex::new(BLOCK, 500_000);
    idx.apply_event(&store_event("m", "w0", None, vec![1, 2, 3, 4]));
    // w1 没有任何块 → depth 0 → 被 continue 跳过
    let matches = idx.find_matches("m", &[1, 2, 3, 4], &["w0".to_string(), "w1".to_string()]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].worker_id, "w0");
}
