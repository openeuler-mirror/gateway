//! ZMQ SUB subscriber for vLLM KV-cache events.
//!
//! Connects to one or more vLLM PUB sockets, parses the 3-frame multipart
//! messages (topic, seq, msgpack payload), and feeds events into the
//! `KvIndexBackend`.

use boom_core::kv_event::KvIndexBackend;
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::vllm_event::{self, VllmEventBatch};

/// Configuration for the ZMQ KV event subscriber.
pub struct KvSubscriberConfig {
    /// ZMQ PUB endpoints to subscribe to (e.g., `["tcp://worker-0:5557"]`).
    pub endpoints: Vec<String>,
    /// Topic prefix for subscription filtering (e.g., `"kv@"`).
    pub topic_prefix: String,
}

/// Spawn the ZMQ subscriber as a background tokio task.
///
/// Returns a `JoinHandle` for graceful shutdown. The task exits when:
/// - A message is received on the `shutdown` channel, OR
/// - All ZMQ streams end (all publishers disconnected).
pub fn spawn_kv_subscriber(
    config: KvSubscriberConfig,
    kv_index: Arc<dyn KvIndexBackend>,
    mut shutdown: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_subscriber(config, kv_index, &mut shutdown).await {
            tracing::error!("ZMQ KV subscriber exited with error: {e}");
        }
    })
}

async fn run_subscriber(
    config: KvSubscriberConfig,
    kv_index: Arc<dyn KvIndexBackend>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<(), String> {
    let context = tmq::Context::new();

    // Build one SUB stream per endpoint, then merge with select_all.
    let mut streams = Vec::new();
    for endpoint in &config.endpoints {
        let sub = tmq::subscribe(&context)
            .connect(endpoint)
            .map_err(|e| format!("connect to {endpoint}: {e}"))?
            .subscribe(config.topic_prefix.as_bytes())
            .map_err(|e| format!("subscribe {endpoint}: {e}"))?;
        streams.push(sub);
        tracing::info!(endpoint, "ZMQ SUB connected");
    }

    if streams.is_empty() {
        return Err("no ZMQ endpoints configured".into());
    }

    let mut merged = futures::stream::select_all(streams);

    tracing::info!(
        endpoints = config.endpoints.len(),
        topic_prefix = %config.topic_prefix,
        "ZMQ KV event subscriber started"
    );

    loop {
        tokio::select! {
            msg = merged.next() => {
                match msg {
                    Some(Ok(multipart)) => {
                        if let Err(e) = handle_message(&multipart, &kv_index) {
                            tracing::warn!(error = %e, "failed to process ZMQ message");
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "ZMQ receive error");
                    }
                    None => {
                        tracing::info!("all ZMQ streams ended, subscriber exiting");
                        return Ok(());
                    }
                }
            }
            _ = shutdown.recv() => {
                tracing::debug!("ZMQ KV subscriber shutting down");
                return Ok(());
            }
        }
    }
}

/// Process a single ZMQ multipart message: `[topic, seq, msgpack_payload]`.
fn handle_message(
    multipart: &tmq::Multipart,
    kv_index: &Arc<dyn KvIndexBackend>,
) -> Result<(), String> {
    if multipart.len() < 3 {
        return Err(format!("expected 3 frames, got {}", multipart.len()));
    }

    // Frame 0: topic (UTF-8)
    let topic = String::from_utf8_lossy(&multipart[0]);
    let (worker_id, model) = vllm_event::parse_topic(&topic)
        .ok_or_else(|| format!("invalid topic format: {topic}"))?;

    // Frame 1: sequence number (8 bytes, big-endian u64) — informational only.
    if multipart[1].len() >= 8 {
        let seq = u64::from_be_bytes(
            multipart[1][..8].try_into().unwrap_or([0u8; 8]),
        );
        tracing::trace!(seq, worker_id, model, "ZMQ KV event received");
    }

    // Frame 2: msgpack payload
    let payload = &multipart[2];
    let batch: VllmEventBatch = vllm_event::parse_vllm_batch(payload)?;

    // Log per-event details.
    let mut stored_count = 0usize;
    let mut removed_count = 0usize;
    let mut cleared_count = 0usize;
    for ev in &batch.events {
        match ev {
            vllm_event::VllmKvEvent::BlockStored { block_hashes, medium, .. } => {
                stored_count += block_hashes.len();
                tracing::trace!(
                    worker_id, model,
                    hashes = block_hashes.len(),
                    medium = ?medium.as_deref().unwrap_or("gpu"),
                    "BlockStored"
                );
            }
            vllm_event::VllmKvEvent::BlockRemoved { block_hashes, .. } => {
                removed_count += block_hashes.len();
                tracing::trace!(
                    worker_id, model,
                    hashes = block_hashes.len(),
                    "BlockRemoved"
                );
            }
            vllm_event::VllmKvEvent::AllBlocksCleared => {
                cleared_count += 1;
                tracing::trace!(worker_id, model, "AllBlocksCleared");
            }
        }
    }
    if stored_count > 0 || removed_count > 0 || cleared_count > 0 {
        tracing::debug!(
            worker_id, model,
            stored = stored_count,
            removed = removed_count,
            cleared = cleared_count,
            "KV event batch processed"
        );
    }

    // Convert to internal events and apply.
    let gateway_events = vllm_event::vllm_batch_to_gateway_events(&batch, worker_id, model);
    let count = gateway_events.len();

    let has_clear = batch.events.iter().any(|e| {
        matches!(e, vllm_event::VllmKvEvent::AllBlocksCleared)
    });

    // Apply events sequentially. AllBlocksCleared maps to a Remove event,
    // which already calls remove_worker() internally — no need for a second call.
    for event in &gateway_events {
        kv_index.apply_event(event);
    }

    tracing::debug!(
        worker_id,
        model,
        events = count,
        cleared = has_clear,
        "KV events applied"
    );

    Ok(())
}
