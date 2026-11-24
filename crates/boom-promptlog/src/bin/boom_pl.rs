//! boom-pl — offline reader for the prompt log segment store.
//!
//! Pure reads over the format layer: no store open, no recovery, no writes —
//! safe to run while the gateway is live, including against the active (not
//! yet sealed) segment.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use boom_promptlog::entry::LogPhase;
use boom_promptlog::segment::{
    decode_record, for_each_record, read_block_at, read_header, read_sealed_meta, scan_blocks,
    SEG_EXT,
};
use boom_promptlog::EntryPair;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "boom-pl",
    about = "Offline reader for the boom prompt log segment store",
    version
)]
struct Cli {
    /// Prompt log root directory (the one holding `segments/`)
    #[arg(long, short)]
    dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List segments: record counts, sizes, time ranges, seal state
    List,
    /// Dump records as JSONL — every segment, or one named segment
    Dump {
        /// A segment name (file name or stem) from `list`
        segment: Option<String>,
    },
    /// Look up one request's entries by request_id (newest segments first)
    Get { request_id: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::List => list(&cli.dir),
        Cmd::Dump { segment } => dump(&cli.dir, segment.as_deref()),
        Cmd::Get { request_id } => get(&cli.dir, &request_id),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("boom-pl: {e}");
            ExitCode::FAILURE
        }
    }
}

// ────────────────────────────────────────────────────────────────

fn segments_dir(root: &Path) -> PathBuf {
    root.join("segments")
}

/// All `.seg` files sorted by name — for one slug that equals chronological
/// order (start_ms and zero-padded seq sort lexicographically).
fn segment_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(segments_dir(root))?;
    for e in rd {
        let e = e?;
        let path = e.path();
        if path.is_file() && path.extension().is_some_and(|x| x == SEG_EXT) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn human(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

fn ts(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "-".into())
}

fn list(root: &Path) -> io::Result<ExitCode> {
    let files = segment_files(root)?;
    if files.is_empty() {
        println!("no segments under {}", segments_dir(root).display());
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<44} {:>9} {:>7} {:>9} {:>8}  FIRST  LAST",
        "SEGMENT", "RECORDS", "BLOCKS", "UNCOMP", "FILE"
    );
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy();
        let len = path.metadata()?.len();
        match read_sealed_meta(&path)? {
            Some((_header, footer, _)) => println!(
                "{:<44} {:>9} {:>7} {:>9} {:>8}  {}  {}",
                name,
                footer.record_count,
                footer.block_count,
                human(footer.uncomp_bytes),
                human(len),
                ts(footer.first_ts_ms),
                ts(footer.last_ts_ms),
            ),
            None => println!(
                "{:<44} {:>9} {:>7} {:>9} {:>8}  unsalvaged (active or crashed)",
                name, "-", "-", "-", human(len)
            ),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn dump(root: &Path, wanted: Option<&str>) -> io::Result<ExitCode> {
    let files = segment_files(root)?;
    let files: Vec<PathBuf> = match wanted {
        None => files,
        Some(w) => {
            let matches: Vec<_> = files
                .into_iter()
                .filter(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(w.trim_end_matches(&format!(".{SEG_EXT}")))
                })
                .collect();
            if matches.is_empty() {
                eprintln!("boom-pl: no segment matches `{w}`");
                return Ok(ExitCode::FAILURE);
            }
            matches
        }
    };
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for path in &files {
        dump_segment(path, &mut out)?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn dump_segment(path: &Path, out: &mut impl Write) -> io::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut reader = &file;
    let Some(header) = read_header(&mut reader)? else {
        return Ok(()); // not a segment file; `list` already flagged it
    };
    let (blocks, _) = scan_blocks(&file, header.data_offset)?;
    for b in &blocks {
        let payload = read_block_at(&file, b.file_pos)?;
        for_each_record(&payload, |frame, _| {
            out.write_all(frame)?;
            out.write_all(b"\n")?;
            Ok(())
        })?;
    }
    Ok(())
}

fn get(root: &Path, request_id: &str) -> io::Result<ExitCode> {
    let mut pair = EntryPair::default();
    'segments: for path in segment_files(root)?.into_iter().rev() {
        let file = std::fs::File::open(&path)?;
        let mut reader = &file;
        let Some(header) = read_header(&mut reader)? else {
            continue;
        };
        let (blocks, _) = scan_blocks(&file, header.data_offset)?;
        for b in &blocks {
            let payload = read_block_at(&file, b.file_pos)?;
            for_each_record(&payload, |frame, _| {
                if let Ok(entry) = decode_record(frame) {
                    if entry.request_id == request_id {
                        match entry.phase {
                            LogPhase::Request => pair.request = Some(entry),
                            LogPhase::Response => pair.response = Some(entry),
                        }
                    }
                }
                Ok(())
            })?;
            if pair.complete() {
                break 'segments;
            }
        }
    }
    if pair.request.is_none() && pair.response.is_none() {
        eprintln!(
            "boom-pl: request `{request_id}` not found under {}",
            segments_dir(root).display()
        );
        return Ok(ExitCode::FAILURE);
    }
    println!(
        "{}",
        serde_json::json!({
            "request": pair.request,
            "response": pair.response,
        })
    );
    Ok(ExitCode::SUCCESS)
}
