//! vLLM ZMQ KV-cache event types and parser.
//!
//! vLLM publishes KV-cache events over ZMQ PUB/SUB using msgpack encoded
//! by Python's `msgspec` library with `tag=True, array_like=True, omit_defaults=True`.
//! This produces tagged arrays where the first element is a string type tag,
//! which is NOT compatible with standard serde enum representations.
//!
//! We use `rmpv::Value` for dynamic parsing and manual tag dispatch.

use boom_core::kv_event::{GatewayKvEvent, StorageTier};

/// A single KV event as produced by vLLM's msgspec encoder.
#[derive(Debug, Clone)]
pub enum VllmKvEvent {
    BlockStored {
        /// Cumulative/chained block hashes (signed i64, ExternalBlockHash).
        block_hashes: Vec<i64>,
        /// Parent block hash (None for the first block in a sequence).
        parent_block_hash: Option<i64>,
        /// Token IDs in this block.
        token_ids: Vec<u32>,
        /// Number of tokens per block (typically 16).
        block_size: u32,
        /// LoRA adapter name.
        lora_name: Option<String>,
        /// Storage medium: None/"gpu" = GPU, "cpu" = CPU, "disk" = SSD.
        medium: Option<String>,
    },
    BlockRemoved {
        /// Hashes of evicted blocks.
        block_hashes: Vec<i64>,
        /// Storage medium.
        medium: Option<String>,
    },
    /// All blocks for this worker have been cleared.
    AllBlocksCleared,
}

/// Top-level vLLM event batch: `[timestamp, [events...], data_parallel_rank?]`.
#[derive(Debug, Clone)]
pub struct VllmEventBatch {
    /// Unix timestamp (seconds, float64).
    pub ts: f64,
    /// Events in this batch.
    pub events: Vec<VllmKvEvent>,
    /// Data-parallel rank (None if DP not enabled).
    pub data_parallel_rank: Option<u32>,
}

/// Parse a vLLM msgpack payload into an event batch.
pub fn parse_vllm_batch(payload: &[u8]) -> Result<VllmEventBatch, String> {
    let value: rmpv::Value =
        rmpv::decode::read_value(&mut &payload[..]).map_err(|e| format!("msgpack decode: {e}"))?;

    let arr = value
        .as_array()
        .ok_or_else(|| "expected msgpack array at top level".to_string())?;

    if arr.len() < 2 {
        return Err("expected at least 2 elements [ts, events]".to_string());
    }

    let ts = arr[0]
        .as_f64()
        .ok_or_else(|| "timestamp is not a number".to_string())?;

    let events_arr = arr[1]
        .as_array()
        .ok_or_else(|| "events field is not an array".to_string())?;

    let mut events = Vec::with_capacity(events_arr.len());
    for raw_event in events_arr {
        let event = parse_vllm_event(raw_event)?;
        events.push(event);
    }

    let data_parallel_rank = if arr.len() > 2 {
        arr[2].as_u64().map(|v| v as u32)
    } else {
        None
    };

    Ok(VllmEventBatch {
        ts,
        events,
        data_parallel_rank,
    })
}

/// Parse a single tagged event from msgspec's `tag=True` encoding.
fn parse_vllm_event(value: &rmpv::Value) -> Result<VllmKvEvent, String> {
    let arr = value
        .as_array()
        .ok_or_else(|| "event is not an array".to_string())?;

    if arr.is_empty() {
        return Err("empty event array".to_string());
    }

    let tag = arr[0]
        .as_str()
        .ok_or_else(|| "event tag is not a string".to_string())?;

    match tag {
        "BlockStored" => parse_block_stored(&arr[1..]),
        "BlockRemoved" => parse_block_removed(&arr[1..]),
        "AllBlocksCleared" => Ok(VllmKvEvent::AllBlocksCleared),
        other => {
            // Unknown event type — skip gracefully.
            tracing::debug!(tag = other, "ignoring unknown vLLM event type");
            Err(format!("unknown event tag: {other}"))
        }
    }
}

/// Parse BlockStored fields.
///
/// msgspec field order with `omit_defaults=True`:
/// `[block_hashes, parent_block_hash?, token_ids, block_size, lora_id?, medium?, lora_name?, extra_keys?, group_idx?, kv_cache_spec_kind?, kv_cache_spec_sliding_window?]`
fn parse_block_stored(fields: &[rmpv::Value]) -> Result<VllmKvEvent, String> {
    if fields.len() < 4 {
        return Err(format!(
            "BlockStored: expected at least 4 fields, got {}",
            fields.len()
        ));
    }

    let block_hashes = parse_i64_array(&fields[0], "block_hashes")?;
    let parent_block_hash = fields[1].as_i64();
    let token_ids = parse_u32_array(&fields[2], "token_ids")?;
    let block_size = fields[3]
        .as_u64()
        .ok_or_else(|| "block_size is not a number".to_string())? as u32;

    // Optional fields: lora_id (idx 4), medium (idx 5), lora_name (idx 6)
    let medium = fields.get(5).and_then(|v| v.as_str()).map(|s| s.to_string());
    let lora_name = fields.get(6).and_then(|v| v.as_str()).map(|s| s.to_string());

    Ok(VllmKvEvent::BlockStored {
        block_hashes,
        parent_block_hash,
        token_ids,
        block_size,
        lora_name,
        medium,
    })
}

/// Parse BlockRemoved fields.
///
/// msgspec field order: `[block_hashes, medium?, group_idx?]`
fn parse_block_removed(fields: &[rmpv::Value]) -> Result<VllmKvEvent, String> {
    if fields.is_empty() {
        return Err("BlockRemoved: expected at least 1 field".into());
    }

    let block_hashes = parse_i64_array(&fields[0], "block_hashes")?;
    let medium = fields.get(1).and_then(|v| v.as_str()).map(|s| s.to_string());

    Ok(VllmKvEvent::BlockRemoved {
        block_hashes,
        medium,
    })
}

fn parse_i64_array(value: &rmpv::Value, name: &str) -> Result<Vec<i64>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("{name} is not an array"))
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_i64())
                .collect()
        })
}

fn parse_u32_array(value: &rmpv::Value, name: &str) -> Result<Vec<u32>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("{name} is not an array"))
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u32))
                .collect()
        })
}

/// Parse a ZMQ topic string in llm-d format: `kv@{worker_id}@{model_name}`.
pub fn parse_topic(topic: &str) -> Option<(&str, &str)> {
    let mut parts = topic.splitn(3, '@');
    let prefix = parts.next()?;
    let worker_id = parts.next()?;
    let model = parts.next()?;
    if prefix == "kv" && !worker_id.is_empty() && !model.is_empty() {
        Some((worker_id, model))
    } else {
        None
    }
}

/// Map vLLM's `medium` field to our `StorageTier`.
pub fn medium_to_tier(medium: &Option<String>) -> StorageTier {
    match medium {
        None => StorageTier::Gpu,
        Some(s) => match s.to_lowercase().as_str() {
            "gpu" => StorageTier::Gpu,
            "cpu" => StorageTier::Cpu,
            "disk" | "ssd" => StorageTier::Ssd,
            _ => StorageTier::Remote,
        },
    }
}

/// Convert a parsed vLLM batch into internal `GatewayKvEvent` list.
///
/// Each `BlockStored` may contain multiple block hashes, which are fanned out
/// into individual `Store` events. `BlockRemoved` is similarly expanded.
/// `AllBlocksCleared` produces a single `Remove` event (the caller should
/// additionally call `kv_index.remove_worker()` for complete cleanup).
pub fn vllm_batch_to_gateway_events(
    batch: &VllmEventBatch,
    worker_id: &str,
    model: &str,
) -> Vec<GatewayKvEvent> {
    let mut out = Vec::new();

    for event in &batch.events {
        match event {
            VllmKvEvent::BlockStored {
                block_hashes,
                token_ids,
                block_size,
                medium,
                ..
            } => {
                let tier = medium_to_tier(medium);
                let bs = *block_size as usize;
                for (idx, &hash) in block_hashes.iter().enumerate() {
                    let block_index = if bs > 0 && !token_ids.is_empty() {
                        (token_ids.len() / bs).saturating_sub(
                            block_hashes.len() - 1 - idx,
                        ) as u64
                    } else {
                        idx as u64
                    };

                    // Extract per-block token_ids.
                    let block_tokens = if bs > 0 {
                        let start = idx * bs;
                        let end = std::cmp::min(start + bs, token_ids.len());
                        if start < token_ids.len() {
                            token_ids[start..end].to_vec()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    };

                    out.push(GatewayKvEvent::Store {
                        model: model.to_string(),
                        worker_id: worker_id.to_string(),
                        sequence_hash: String::new(),
                        prefix_hash: String::new(),
                        local_hash: hash as u64,
                        block_index,
                        token_ids: block_tokens,
                        block_size: *block_size,
                        storage_tier: tier,
                    });
                }
            }
            VllmKvEvent::BlockRemoved { block_hashes, .. } => {
                // Per-block eviction: only remove the specific block hashes.
                let hashes: Vec<u64> = block_hashes.iter().map(|&h| h as u64).collect();
                out.push(GatewayKvEvent::EvictBlocks {
                    model: model.to_string(),
                    worker_id: worker_id.to_string(),
                    block_hashes: hashes,
                });
            }
            VllmKvEvent::AllBlocksCleared => {
                out.push(GatewayKvEvent::Remove {
                    worker_id: worker_id.to_string(),
                    sequence_hash: String::new(),
                    storage_tier: None,
                });
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_topic() {
        assert_eq!(
            parse_topic("kv@worker-0@Qwen/Qwen3-0.6B"),
            Some(("worker-0", "Qwen/Qwen3-0.6B"))
        );
        assert_eq!(parse_topic("kv@w@m"), Some(("w", "m")));
        assert_eq!(parse_topic("kv@w@"), None); // empty model
        assert_eq!(parse_topic("kv@@m"), None); // empty worker
        assert_eq!(parse_topic("other@w@m"), None); // wrong prefix
        assert_eq!(parse_topic("kv@w"), None); // no model
        assert_eq!(parse_topic(""), None);
    }

    #[test]
    fn test_medium_to_tier() {
        assert_eq!(medium_to_tier(&None), StorageTier::Gpu);
        assert_eq!(medium_to_tier(&Some("gpu".into())), StorageTier::Gpu);
        assert_eq!(medium_to_tier(&Some("GPU".into())), StorageTier::Gpu);
        assert_eq!(medium_to_tier(&Some("cpu".into())), StorageTier::Cpu);
        assert_eq!(medium_to_tier(&Some("CPU".into())), StorageTier::Cpu);
        assert_eq!(medium_to_tier(&Some("disk".into())), StorageTier::Ssd);
        assert_eq!(medium_to_tier(&Some("ssd".into())), StorageTier::Ssd);
        assert_eq!(medium_to_tier(&Some("remote".into())), StorageTier::Remote);
    }

    #[test]
    fn test_parse_block_stored_msgpack() {
        // Manually construct msgpack for:
        // [ts, [["BlockStored", [100, 200], nil, [1,2,3,4], 16, nil, "gpu"]], nil]
        use rmpv::encode::write_value;

        let mut buf = Vec::new();
        // Top-level array of 3 elements
        write_value(&mut buf, &rmpv::Value::Array(vec![
            rmpv::Value::from(1234567890.0_f64), // ts
            rmpv::Value::Array(vec![              // events array
                rmpv::Value::Array(vec![          // BlockStored event
                    rmpv::Value::from("BlockStored"),
                    rmpv::Value::Array(vec![rmpv::Value::from(100i64), rmpv::Value::from(200i64)]), // block_hashes
                    rmpv::Value::Nil,             // parent_block_hash
                    rmpv::Value::Array(vec![rmpv::Value::from(1u64), rmpv::Value::from(2u64), rmpv::Value::from(3u64), rmpv::Value::from(4u64)]), // token_ids
                    rmpv::Value::from(16u64),     // block_size
                    rmpv::Value::Nil,             // lora_id
                    rmpv::Value::from("gpu"),     // medium
                ]),
            ]),
            rmpv::Value::Nil, // data_parallel_rank
        ])).unwrap();

        let batch = parse_vllm_batch(&buf).unwrap();
        assert!((batch.ts - 1234567890.0).abs() < 0.01);
        assert_eq!(batch.events.len(), 1);

        match &batch.events[0] {
            VllmKvEvent::BlockStored { block_hashes, token_ids, block_size, medium, .. } => {
                assert_eq!(*block_hashes, vec![100, 200]);
                assert_eq!(*token_ids, vec![1, 2, 3, 4]);
                assert_eq!(*block_size, 16);
                assert_eq!(medium.as_deref(), Some("gpu"));
            }
            other => panic!("expected BlockStored, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_block_removed_msgpack() {
        use rmpv::encode::write_value;

        let mut buf = Vec::new();
        write_value(&mut buf, &rmpv::Value::Array(vec![
            rmpv::Value::from(1.0_f64),
            rmpv::Value::Array(vec![
                rmpv::Value::Array(vec![
                    rmpv::Value::from("BlockRemoved"),
                    rmpv::Value::Array(vec![rmpv::Value::from(42i64)]),
                    rmpv::Value::from("cpu"),
                ]),
            ]),
            rmpv::Value::Nil,
        ])).unwrap();

        let batch = parse_vllm_batch(&buf).unwrap();
        assert_eq!(batch.events.len(), 1);

        match &batch.events[0] {
            VllmKvEvent::BlockRemoved { block_hashes, medium } => {
                assert_eq!(*block_hashes, vec![42]);
                assert_eq!(medium.as_deref(), Some("cpu"));
            }
            other => panic!("expected BlockRemoved, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_all_blocks_cleared_msgpack() {
        use rmpv::encode::write_value;

        let mut buf = Vec::new();
        write_value(&mut buf, &rmpv::Value::Array(vec![
            rmpv::Value::from(1.0_f64),
            rmpv::Value::Array(vec![
                rmpv::Value::Array(vec![
                    rmpv::Value::from("AllBlocksCleared"),
                ]),
            ]),
        ])).unwrap();

        let batch = parse_vllm_batch(&buf).unwrap();
        assert!(matches!(batch.events[0], VllmKvEvent::AllBlocksCleared));
    }

    #[test]
    fn test_vllm_batch_to_gateway_events_store() {
        let batch = VllmEventBatch {
            ts: 1.0,
            events: vec![VllmKvEvent::BlockStored {
                block_hashes: vec![100, 200],
                parent_block_hash: None,
                token_ids: vec![1, 2, 3, 4, 5, 6, 7, 8],
                block_size: 4,
                lora_name: None,
                medium: Some("gpu".into()),
            }],
            data_parallel_rank: None,
        };

        let events = vllm_batch_to_gateway_events(&batch, "worker-0", "test-model");
        assert_eq!(events.len(), 2);

        // First block
        match &events[0] {
            GatewayKvEvent::Store { model, worker_id, local_hash, token_ids, block_size, storage_tier, .. } => {
                assert_eq!(model, "test-model");
                assert_eq!(worker_id, "worker-0");
                assert_eq!(*local_hash, 100i64 as u64);
                assert_eq!(*token_ids, vec![1, 2, 3, 4]);
                assert_eq!(*block_size, 4);
                assert_eq!(*storage_tier, StorageTier::Gpu);
            }
            other => panic!("expected Store, got {other:?}"),
        }

        // Second block
        match &events[1] {
            GatewayKvEvent::Store { local_hash, token_ids, .. } => {
                assert_eq!(*local_hash, 200i64 as u64);
                assert_eq!(*token_ids, vec![5, 6, 7, 8]);
            }
            other => panic!("expected Store, got {other:?}"),
        }
    }

    #[test]
    fn test_vllm_batch_to_gateway_events_cleared() {
        let batch = VllmEventBatch {
            ts: 1.0,
            events: vec![VllmKvEvent::AllBlocksCleared],
            data_parallel_rank: None,
        };

        let events = vllm_batch_to_gateway_events(&batch, "worker-0", "model");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], GatewayKvEvent::Remove { worker_id, .. } if worker_id == "worker-0"));
    }

    #[test]
    fn test_i64_to_u64_bit_reinterpret() {
        // Negative i64 should be reinterpreted as u64 (bit pattern preserved).
        let hash: i64 = -1;
        let as_u64: u64 = hash as u64;
        assert_eq!(as_u64, u64::MAX);
    }
}
