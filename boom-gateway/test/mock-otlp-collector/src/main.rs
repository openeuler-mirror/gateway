//! Mock OTLP/HTTP receiver for the gateway's telemetry push.
//!
//! Decodes `ExportLogsServiceRequest` protobuf from POST `/v1/logs` and
//! pretty-prints each LogRecord. Two modes:
//!
//! - **brief** (default): one line per record. OTel header fields
//!   (trace_id / span_id / severity / body, first 80 chars) only. Good for
//!   live monitoring while you trigger requests against the gateway.
//! - **full** (`--full`): dumps every attribute on every LogRecord, plus
//!   resource attributes and dropped_attributes_count. Good for verifying
//!   field mappings while developing new entry types.
//!
//! The crate is intentionally a separate Cargo project (see `[workspace]`
//! in Cargo.toml) — `cargo build --workspace` from the gateway root does
//! NOT compile this tool. Build it explicitly:
//!   cd boom-gateway/test/mock-otlp-collector && cargo build --release

use std::net::SocketAddr;

use axum::body::Bytes;
use axum::{http::StatusCode, routing::post, Router};
use clap::Parser;
use opentelemetry_proto::tonic::{
    collector::logs::v1::ExportLogsServiceRequest,
    common::v1::any_value::Value,
    logs::v1::SeverityNumber as Sev,
};
use prost::Message;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "mock-otlp-collector",
    about = "Mock OTLP/HTTP receiver — decodes and pretty-prints LogRecord stream"
)]
struct Args {
    /// Bind address (OTLP/HTTP default port is 4318).
    #[arg(long, default_value = "0.0.0.0:4318")]
    bind: String,

    /// Print every attribute on every LogRecord (default: brief one-liner).
    #[arg(long)]
    full: bool,

    /// Truncate body / attribute strings to N characters in brief mode.
    /// In --full mode strings are printed in full.
    #[arg(long, default_value_t = 80)]
    brief_chars: usize,
}

#[derive(Clone)]
struct SinkState {
    full: bool,
    brief_chars: usize,
    /// Counter incremented on each request — used as the per-request prefix
    /// so multiple batched LogRecords are visually grouped together.
    seq: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let addr: SocketAddr = match args.bind.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("invalid --bind '{}': {}", args.bind, e);
            std::process::exit(1);
        }
    };

    let state = SinkState {
        full: args.full,
        brief_chars: args.brief_chars,
        seq: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };

    let app = Router::new()
        .route("/v1/logs", post(handle_logs))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!(
        addr = %addr,
        full_mode = args.full,
        "mock-otlp-collector listening — POST /v1/logs"
    );
    axum::serve(listener, app).await.unwrap();
}

async fn handle_logs(
    axum::extract::State(state): axum::extract::State<SinkState>,
    body: Bytes,
) -> (StatusCode, &'static [u8]) {
    let req_seq = state
        .seq
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let req: ExportLogsServiceRequest = match ExportLogsServiceRequest::decode(&body[..]) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                req_seq,
                body_bytes = body.len(),
                "decode failed: {e}"
            );
            // OTLP retry would just hammer us with the same broken batch —
            // return 200 so the gateway's exporter doesn't retry.
            return (StatusCode::OK, b"");
        }
    };

    let total: usize = req
        .resource_logs
        .iter()
        .flat_map(|rl| rl.scope_logs.iter())
        .map(|sl| sl.log_records.len())
        .sum();

    if state.full {
        print_full(req_seq, &req);
    } else {
        print_brief(req_seq, &req, state.brief_chars, total);
    }

    // Empty ExportLogsServiceResponse (code 0).
    (StatusCode::OK, b"")
}

fn print_brief(req_seq: u64, req: &ExportLogsServiceRequest, brief_chars: usize, total: usize) {
    let now = chrono_now();
    println!("[{now} req#{req_seq}] batch={total} records");
    for rl in &req.resource_logs {
        for sl in &rl.scope_logs {
            for lr in &sl.log_records {
                let trace_id = hex(&lr.trace_id);
                let span_id = hex(&lr.span_id);
                let sev = severity_name(lr.severity_number);
                let body = body_string(&lr.body).unwrap_or_default();
                let body_short = truncate_str(&body, brief_chars);
                let trace_part = if trace_id.is_empty() {
                    String::new()
                } else {
                    format!("trace={trace_id} span={span_id} ")
                };
                println!(
                    "  {trace_part}sev={sev} body=\"{body_short}\""
                );
            }
        }
    }
}

fn print_full(req_seq: u64, req: &ExportLogsServiceRequest) {
    let now = chrono_now();
    let total: usize = req
        .resource_logs
        .iter()
        .flat_map(|rl| rl.scope_logs.iter())
        .map(|sl| sl.log_records.len())
        .sum();
    println!("\n[{now} req#{req_seq}] ExportLogsServiceRequest: {} resource_logs, {} records", req.resource_logs.len(), total);
    for (ri, rl) in req.resource_logs.iter().enumerate() {
        println!("\n-- ResourceLogs[{ri}] --");
        if let Some(res) = &rl.resource {
            for attr in &res.attributes {
                println!("  resource.attr: {} = {}", attr.key, format_any(&attr.value));
            }
        }
        for (si, sl) in rl.scope_logs.iter().enumerate() {
            let scope_name = sl
                .scope
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_default();
            println!(
                "\n  ScopeLogs[{si}] scope.name={scope_name:?} {} records",
                sl.log_records.len()
            );
            for (li, lr) in sl.log_records.iter().enumerate() {
                println!("\n    LogRecord[{li}]:");
                println!("      time_unix_nano: {}", lr.time_unix_nano);
                println!(
                    "      severity: {} ({:?})",
                    severity_name(lr.severity_number),
                    lr.severity_text
                );
                let trace_id = hex(&lr.trace_id);
                let span_id = hex(&lr.span_id);
                if !trace_id.is_empty() {
                    println!("      trace_id={trace_id} span_id={span_id}");
                }
                println!("      body: {}", format_any(&lr.body));
                if lr.dropped_attributes_count > 0 {
                    println!(
                        "      dropped_attributes_count: {}",
                        lr.dropped_attributes_count
                    );
                }
                for attr in &lr.attributes {
                    println!("      attr: {} = {}", attr.key, format_any(&attr.value));
                }
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

fn severity_name(n: i32) -> &'static str {
    // OTel severity numbers are: 1-4 TRACE, 5-8 DEBUG, 9-12 INFO,
    // 13-16 WARN, 17-20 ERROR, 21-24 FATAL. The enum's integer values
    // match this scale.
    match Sev::try_from(n).unwrap_or(Sev::Unspecified) {
        Sev::Trace => "TRACE",
        Sev::Trace2 => "TRACE",
        Sev::Trace3 => "TRACE",
        Sev::Trace4 => "TRACE",
        Sev::Debug => "DEBUG",
        Sev::Debug2 => "DEBUG",
        Sev::Debug3 => "DEBUG",
        Sev::Debug4 => "DEBUG",
        Sev::Info => "INFO",
        Sev::Info2 => "INFO",
        Sev::Info3 => "INFO",
        Sev::Info4 => "INFO",
        Sev::Warn => "WARN",
        Sev::Warn2 => "WARN",
        Sev::Warn3 => "WARN",
        Sev::Warn4 => "WARN",
        Sev::Error => "ERROR",
        Sev::Error2 => "ERROR",
        Sev::Error3 => "ERROR",
        Sev::Error4 => "ERROR",
        Sev::Fatal => "FATAL",
        Sev::Fatal2 => "FATAL",
        Sev::Fatal3 => "FATAL",
        Sev::Fatal4 => "FATAL",
        _ => "?",
    }
}

fn body_string(v: &Option<opentelemetry_proto::tonic::common::v1::AnyValue>) -> Option<String> {
    match v.as_ref()?.value.as_ref()? {
        Value::StringValue(s) => Some(s.clone()),
        _ => Some(format_any(v)),
    }
}

fn format_any(v: &Option<opentelemetry_proto::tonic::common::v1::AnyValue>) -> String {
    use opentelemetry_proto::tonic::common::v1::AnyValue;
    match v {
        Some(AnyValue { value: Some(val), .. }) => match val {
            Value::StringValue(s) => format!("\"{s}\""),
            Value::IntValue(i) => i.to_string(),
            Value::BoolValue(b) => b.to_string(),
            Value::DoubleValue(d) => d.to_string(),
            Value::BytesValue(b) => format!("<{} bytes>", b.len()),
            Value::ArrayValue(a) => {
                let parts: Vec<String> = a
                    .values
                    .iter()
                    .map(|av| format_any(&Some(av.clone())))
                    .collect();
                format!("[{}]", parts.join(", "))
            }
            Value::KvlistValue(k) => {
                let parts: Vec<String> = k
                    .values
                    .iter()
                    .map(|kv| format!("{}={}", kv.key, format_any(&kv.value)))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            Value::StringValueStrindex(_) => "(str-index)".to_string(),
        },
        Some(AnyValue { value: None, .. }) => "(empty)".to_string(),
        None => "(none)".to_string(),
    }
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let h = (secs / 3600 + 8) % 24; // UTC+8 for CN local time
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}
