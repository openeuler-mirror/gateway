//! Offline replay tool: read JSONL prompt logs from disk and push to an
//! OTLP/HTTP collector. Behaviour is identical to the runtime exporter —
//! they share `OtelExporter` and `convert_entry_to_log_records` from
//! `boom_promptlog::otlp`, so "what was stored locally = what gets pushed".
//!
//! Usage:
//!   cargo run --example replay -- --dir /data/prompt_logs --otlp-endpoint http://collector:4318
//!   cargo run --example replay -- --dir /data/prompt_logs/_no_team/kh --otlp-endpoint http://collector:4318

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use boom_promptlog::{
    OtelReplayer, PromptLogConfig,
    config::OtlpConfig,
};

/// Replay JSONL prompt logs into an OTLP collector.
#[derive(Parser, Debug)]
#[command(name = "replay", about = "Replay JSONL prompt logs to OTLP")]
struct Args {
    /// Path to the prompt_log dir (same as `prompt_log.dir` in YAML) or a
    /// subdirectory like `dir/_no_team/kh`. If a subdirectory is given, only
    /// that key's logs are replayed.
    #[arg(long)]
    dir: PathBuf,

    /// OTLP/HTTP base URL, e.g. `http://otel-collector:4318`. The exporter
    /// appends `/v1/logs` itself.
    #[arg(long)]
    otlp_endpoint: String,

    /// Service name reported in the OTLP Resource (default: boom-gateway).
    #[arg(long, default_value = "boom-gateway")]
    service_name: String,

    /// Override service version (defaults to the gateway's CARGO_PKG_VERSION).
    #[arg(long)]
    service_version: Option<String>,

    /// Batch size (entries per flush). Default 512.
    #[arg(long, default_value_t = 512)]
    batch_size: usize,

    /// Flush interval in seconds. Default 5.
    #[arg(long, default_value_t = 5)]
    flush_interval_secs: u64,

    /// Per-attribute byte budget for large bodies (request/response/raw).
    /// Default 4096.
    #[arg(long, default_value_t = 4096)]
    max_attribute_bytes: usize,

    /// In-memory queue cap. When full, oldest entries are dropped with a
    /// warn. Default 10000.
    #[arg(long, default_value_t = 10000)]
    max_queue_size: usize,

    /// HTTP timeout in seconds. Default 10.
    #[arg(long, default_value_t = 10)]
    timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    tracing_subscriber::fmt::init();

    let otlp_cfg = OtlpConfig {
        enabled: true,
        endpoint: args.otlp_endpoint.clone(),
        service_name: args.service_name.clone(),
        service_version: args.service_version.clone(),
        timeout_secs: args.timeout_secs,
        batch_size: args.batch_size,
        flush_interval_secs: args.flush_interval_secs,
        max_attribute_bytes: args.max_attribute_bytes,
        headers: std::collections::HashMap::new(),
        max_queue_size: args.max_queue_size,
    };

    // We don't need the writer — only the exporter. The PromptLogConfig we
    // synthesize here is unused by the replayer; OtelReplayer only reads the
    // OtlpConfig sub-struct. The full struct is built so that future
    // additions to required fields don't break this binary.
    let _unused_writer_cfg = PromptLogConfig {
        otlp: otlp_cfg.clone(),
        ..PromptLogConfig::default()
    };

    let replayer = Arc::new(OtelReplayer::new(&otlp_cfg));
    let flush_handle = replayer.spawn_flush();

    let started = std::time::Instant::now();
    let total = replayer.replay_dir(&args.dir).await?;
    tracing::info!(entries = total, elapsed_ms = %started.elapsed().as_millis(), "Replay queue populated, draining");

    // Force a final flush, then abort the periodic task (it loops forever
    // by design — keeping it alive for the lifetime of the runtime). Without
    // abort, `flush_handle.await` hangs.
    replayer.flush().await;
    drop(replayer);
    flush_handle.abort();
    let _ = flush_handle.await;

    tracing::info!("Replay complete: {} entries pushed to {}", total, args.otlp_endpoint);
    Ok(())
}
