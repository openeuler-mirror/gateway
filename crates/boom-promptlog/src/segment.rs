//! Segment file format — the pure format layer, no policy.
//!
//! Layout of a `.seg` file (all integers little-endian):
//!
//! ```text
//! SegmentHeader:
//!   [u32 magic "BLP1"][u16 version][u16 header_len]
//!   [u64 start_ts_ms][u8 slug_len][slug bytes]          (payload = header_len)
//! Block × N (appended by the writer as blocks fill):
//!   [u32 magic "BLK1"][u32 comp_len][u32 uncomp_len][u32 crc32][u32 count]
//!   [comp_len bytes zstd(uncomp payload)]
//! SegmentFooter (last FTR_SIZE bytes of the file):
//!   [u32 magic "FTR1"][u64 block_count][u64 record_count][u64 first_ts_ms]
//!   [u64 last_ts_ms][u64 uncomp_bytes][u32 footer_crc][u32 footer_len]
//!   (footer_crc covers the 40 bytes between the magic and footer_crc)
//! ```
//!
//! Uncompressed block payload = concatenation of record frames:
//! `[u32 len][len bytes of JSON]` — one `PromptLogEntry` per frame. The same
//! frame encoding is reused by the spill file.
//!
//! Design invariants this layer serves:
//! - **Per-block compression**: offsets in the uncompressed stream are fixed
//!   at write time forever, so a record's location never moves and sealed
//!   segments need zero re-writing (sealing = append footer).
//! - **Self-describing files**: magic + per-block CRC + footer CRC let any
//!   process (the age enforcer, the offline CLI, a future importer) validate
//!   a segment without external state — the manifest is a cache, never a
//!   source of truth.
//! - **Tail-truncatable recovery**: a crash mid-write leaves a stream of
//!   CRC-valid blocks with no footer; salvage = scan blocks, append footer.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::entry::LogPhase;

pub const SEG_MAGIC: u32 = u32::from_le_bytes(*b"BLP1");
pub const BLK_MAGIC: u32 = u32::from_le_bytes(*b"BLK1");
pub const FTR_MAGIC: u32 = u32::from_le_bytes(*b"FTR1");
pub const IDX_MAGIC: u32 = u32::from_le_bytes(*b"PID1");

pub const FORMAT_VERSION: u16 = 1;

/// Fixed sizes used by readers to position themselves.
pub const SEG_HEADER_PREFIX: u64 = 8; // magic + version + header_len
pub const BLOCK_HEADER_SIZE: usize = 20;
pub const FTR_SIZE: usize = 52;
pub const IDX_HEADER_SIZE: usize = 16;
pub const IDX_ENTRY_SIZE: usize = 32;

pub const SEG_EXT: &str = "seg";
pub const IDX_EXT: &str = "idx";

// Compile-time tripwire: readers below position by these sizes; layout drift
// fails the build instead of misreading at runtime.
const _: () = assert!(SEG_HEADER_PREFIX as usize == 8);
const _: () = assert!(BLOCK_HEADER_SIZE == 20);
const _: () = assert!(FTR_SIZE == 52);
const _: () = assert!(IDX_HEADER_SIZE == 16);
const _: () = assert!(IDX_ENTRY_SIZE == 32);

// ─────────────────────────────────────────────────────────────────
// Naming
// ─────────────────────────────────────────────────────────────────

/// Filename-safe instance slug: `gateway-host:4000` → `gateway-host-4000`.
/// The slug is part of every segment filename so a shared directory (a
/// misconfiguration, but a survivable one) never confuses the age enforcer
/// into deleting another instance's segments.
pub fn instance_slug(instance_id: &str) -> String {
    instance_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `{slug}-{start_ms}-{seq:06}.seg` — the two numeric fields parse from the
/// right, so the slug itself may contain dashes.
pub fn segment_file_name(slug: &str, start_ms: i64, seq: u64) -> String {
    format!("{}-{}-{:06}.{}", slug, start_ms, seq, SEG_EXT)
}

pub fn parse_segment_file_name(name: &str) -> Option<SegmentName> {
    let stem = name.strip_suffix(&format!(".{}", SEG_EXT))?;
    let (slug, start_ms, seq) = parse_segment_stem(stem)?;
    Some(SegmentName {
        slug,
        start_ms,
        seq,
    })
}

/// Shared by `.seg` and `.idx` (the idx file uses the same stem).
pub fn parse_segment_stem(stem: &str) -> Option<(String, i64, u64)> {
    let mut parts = stem.rsplit('-');
    let seq: u64 = parts.next()?.parse().ok()?;
    let start_ms: i64 = parts.next()?.parse().ok()?;
    // rsplit yields the remaining slug pieces right-to-left.
    let mut slug_parts: Vec<&str> = parts.collect();
    slug_parts.reverse();
    let slug = slug_parts.join("-");
    if slug.is_empty() {
        return None;
    }
    Some((slug, start_ms, seq))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentName {
    pub slug: String,
    pub start_ms: i64,
    pub seq: u64,
}

// ─────────────────────────────────────────────────────────────────
// Segment header
// ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SegmentHeader {
    pub start_ts_ms: i64,
    pub slug: String,
    /// Byte offset where the first block starts.
    pub data_offset: u64,
}

pub fn write_header<W: Write>(w: &mut W, slug: &str, start_ts_ms: i64) -> io::Result<()> {
    let slug = slug.as_bytes();
    if slug.len() > 255 {
        return Err(io::Error::other("segment slug longer than 255 bytes"));
    }
    let payload_len = 8 + 1 + slug.len();
    w.write_all(&SEG_MAGIC.to_le_bytes())?;
    w.write_all(&FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&(payload_len as u16).to_le_bytes())?;
    w.write_all(&start_ts_ms.to_le_bytes())?;
    w.write_all(&[slug.len() as u8])?;
    w.write_all(slug)?;
    Ok(())
}

/// Read and validate a segment header. `Ok(None)` = not a segment file
/// (short file or foreign magic) — callers treat that as salvage-or-skip.
pub fn read_header<R: Read + Seek>(r: &mut R) -> io::Result<Option<SegmentHeader>> {
    let mut prefix = [0u8; SEG_HEADER_PREFIX as usize];
    if r.read(&mut prefix)? != prefix.len() {
        return Ok(None);
    }
    let magic = u32::from_le_bytes(prefix[0..4].try_into().unwrap());
    if magic != SEG_MAGIC {
        return Ok(None);
    }
    let version = u16::from_le_bytes(prefix[4..6].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(io::Error::other(format!(
            "unsupported segment format version {version}"
        )));
    }
    let header_len = u16::from_le_bytes(prefix[6..8].try_into().unwrap()) as usize;
    if header_len < 9 {
        return Ok(None);
    }
    let mut payload = vec![0u8; header_len];
    if r.read_exact(&mut payload).is_err() {
        return Ok(None);
    }
    let start_ts_ms = i64::from_le_bytes(payload[0..8].try_into().unwrap());
    let slug_len = payload[8] as usize;
    if 9 + slug_len > payload.len() {
        return Ok(None);
    }
    let slug = String::from_utf8_lossy(&payload[9..9 + slug_len]).into_owned();
    let data_offset = SEG_HEADER_PREFIX + header_len as u64;
    Ok(Some(SegmentHeader {
        start_ts_ms,
        slug,
        data_offset,
    }))
}

// ─────────────────────────────────────────────────────────────────
// Record codec (shared by segments, spill, import)
// ─────────────────────────────────────────────────────────────────

use crate::entry::PromptLogEntry;

/// Serialize one entry into its record frame bytes (`[u32 len][json]`).
/// Returns the frame so the uncompressed stream can append it verbatim and
/// index its absolute offset.
pub fn encode_record_frame(entry: &PromptLogEntry) -> io::Result<Vec<u8>> {
    let json = serde_json::to_vec(entry)
        .map_err(|e| io::Error::other(format!("serialize prompt log entry: {e}")))?;
    let mut frame = Vec::with_capacity(4 + json.len());
    frame.extend_from_slice(&(json.len() as u32).to_le_bytes());
    frame.extend_from_slice(&json);
    Ok(frame)
}

pub fn decode_record(frame: &[u8]) -> io::Result<PromptLogEntry> {
    serde_json::from_slice(frame)
        .map_err(|e| io::Error::other(format!("decode prompt log record: {e}")))
}

/// Walk a stream of record frames, invoking `f(frame_bytes, frame_start_offset)`.
/// Stops cleanly at EOF; tolerates a torn tail (truncated frame) by stopping
/// before it — used by spill drain and salvage scans.
pub fn for_each_record<F>(data: &[u8], mut f: F) -> io::Result<()>
where
    F: FnMut(&[u8], u64) -> io::Result<()>,
{
    let mut off = 0usize;
    while off + 4 <= data.len() {
        let len =
            u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        if len == 0 || off + 4 + len > data.len() {
            break; // torn tail — stop at the last complete frame
        }
        f(&data[off + 4..off + 4 + len], off as u64)?;
        off += 4 + len;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────
// Blocks
// ─────────────────────────────────────────────────────────────────

/// Where a flushed block lives: file position of its header, compressed and
/// uncompressed lengths, and the absolute offset (in the segment's
/// uncompressed stream) where the block's payload starts.
#[derive(Debug, Clone)]
pub struct BlockLoc {
    pub file_pos: u64,
    pub comp_len: u32,
    pub uncomp_len: u32,
    pub abs_start: u64,
    pub count: u32,
}

/// Compress one block and assemble its on-disk bytes (header + payload).
/// CPU-bound — callers run this via `spawn_blocking`. `count` = number of
/// record frames the payload contains.
pub fn prepare_block(uncomp: &[u8], count: u32, level: i32) -> io::Result<Vec<u8>> {
    let comp = zstd::bulk::compress(uncomp, level)
        .map_err(|e| io::Error::other(format!("zstd compress block: {e}")))?;
    let crc = crc32fast::hash(uncomp);
    let mut out = Vec::with_capacity(BLOCK_HEADER_SIZE + comp.len());
    out.extend_from_slice(&BLK_MAGIC.to_le_bytes());
    out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
    out.extend_from_slice(&(uncomp.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&comp);
    Ok(out)
}

/// Read one block back: header at `file_pos`, verify magic + lengths + CRC,
/// return the uncompressed payload. Sync — callers wrap in `spawn_blocking`.
pub fn read_block_at(file: &std::fs::File, file_pos: u64) -> io::Result<Vec<u8>> {
    let mut f = file;
    f.seek(SeekFrom::Start(file_pos))?;
    let mut header = [0u8; BLOCK_HEADER_SIZE];
    f.read_exact(&mut header)?;
    if u32::from_le_bytes(header[0..4].try_into().unwrap()) != BLK_MAGIC {
        return Err(io::Error::other("block magic mismatch"));
    }
    let comp_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let uncomp_len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    let crc = u32::from_le_bytes(header[12..16].try_into().unwrap());
    // 128MB guards a corrupt comp_len from ballooning the read.
    if comp_len > 128 * 1024 * 1024 || uncomp_len > 128 * 1024 * 1024 {
        return Err(io::Error::other("block length out of range"));
    }
    let mut comp = vec![0u8; comp_len];
    f.read_exact(&mut comp)?;
    let uncomp = zstd::bulk::decompress(&comp, uncomp_len)
        .map_err(|e| io::Error::other(format!("zstd decompress block: {e}")))?;
    if crc32fast::hash(&uncomp) != crc {
        return Err(io::Error::other("block crc mismatch"));
    }
    Ok(uncomp)
}

/// Scan a file from `data_offset`, validating each block (magic + lengths +
/// CRC via full decompress). Returns the block table and the byte offset
/// just past the last valid block — the salvage truncation point. Stops at
/// the first invalid/torn block.
pub fn scan_blocks(file: &std::fs::File, data_offset: u64) -> io::Result<(Vec<BlockLoc>, u64)> {
    let file_len = file.metadata()?.len();
    let mut blocks = Vec::new();
    let mut pos = data_offset;
    let mut abs_start = 0u64;
    while pos + BLOCK_HEADER_SIZE as u64 <= file_len {
        let mut header = [0u8; BLOCK_HEADER_SIZE];
        let n = {
            let mut f = file;
            f.seek(SeekFrom::Start(pos))?;
            f.read(&mut header)?
        };
        if n < BLOCK_HEADER_SIZE {
            break;
        }
        if u32::from_le_bytes(header[0..4].try_into().unwrap()) != BLK_MAGIC {
            break;
        }
        let comp_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let uncomp_len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        let count = u32::from_le_bytes(header[16..20].try_into().unwrap());
        if pos + (BLOCK_HEADER_SIZE as u64) + comp_len as u64 > file_len
            || uncomp_len > 128 * 1024 * 1024
        {
            break;
        }
        // Full-CRC validation: salvage runs rarely (post-crash), correctness
        // beats speed here.
        read_block_at(file, pos)?;
        blocks.push(BlockLoc {
            file_pos: pos,
            comp_len: comp_len as u32,
            uncomp_len: uncomp_len as u32,
            abs_start,
            count,
        });
        abs_start += uncomp_len as u64;
        pos += BLOCK_HEADER_SIZE as u64 + comp_len as u64;
    }
    Ok((blocks, pos))
}

// ─────────────────────────────────────────────────────────────────
// Footer
// ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentFooter {
    pub block_count: u64,
    pub record_count: u64,
    pub first_ts_ms: i64,
    pub last_ts_ms: i64,
    pub uncomp_bytes: u64,
}

pub fn write_footer<W: Write>(w: &mut W, footer: &SegmentFooter) -> io::Result<()> {
    let mut body = Vec::with_capacity(FTR_SIZE);
    body.extend_from_slice(&FTR_MAGIC.to_le_bytes());
    body.extend_from_slice(&footer.block_count.to_le_bytes());
    body.extend_from_slice(&footer.record_count.to_le_bytes());
    body.extend_from_slice(&footer.first_ts_ms.to_le_bytes());
    body.extend_from_slice(&footer.last_ts_ms.to_le_bytes());
    body.extend_from_slice(&footer.uncomp_bytes.to_le_bytes());
    let crc = crc32fast::hash(&body[4..]);
    body.extend_from_slice(&crc.to_le_bytes());
    body.extend_from_slice(&(FTR_SIZE as u32).to_le_bytes());
    debug_assert_eq!(body.len(), FTR_SIZE);
    w.write_all(&body)
}

/// Read the footer from the end of a file. `Ok(None)` = file too short /
/// magic mismatch / CRC mismatch — i.e. "not a cleanly sealed segment".
pub fn read_footer_at<R: Read + Seek>(r: &mut R) -> io::Result<Option<SegmentFooter>> {
    let file_len = r.seek(SeekFrom::End(0))?;
    if file_len < FTR_SIZE as u64 {
        return Ok(None);
    }
    r.seek(SeekFrom::Start(file_len - FTR_SIZE as u64))?;
    let mut buf = [0u8; FTR_SIZE];
    if r.read_exact(&mut buf).is_err() {
        return Ok(None);
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != FTR_MAGIC {
        return Ok(None);
    }
    let stored_crc = u32::from_le_bytes(buf[44..48].try_into().unwrap());
    if crc32fast::hash(&buf[4..44]) != stored_crc {
        return Ok(None);
    }
    Ok(Some(SegmentFooter {
        block_count: u64::from_le_bytes(buf[4..12].try_into().unwrap()),
        record_count: u64::from_le_bytes(buf[12..20].try_into().unwrap()),
        first_ts_ms: i64::from_le_bytes(buf[20..28].try_into().unwrap()),
        last_ts_ms: i64::from_le_bytes(buf[28..36].try_into().unwrap()),
        uncomp_bytes: u64::from_le_bytes(buf[36..44].try_into().unwrap()),
    }))
}

/// Read header + footer in one pass; `Ok(None)` when the file is not a
/// sealed, CRC-valid segment (the age enforcer's validation chain step).
pub fn read_sealed_meta(path: &Path) -> io::Result<Option<(SegmentHeader, SegmentFooter, u64)>> {
    let mut file = std::fs::File::open(path)?;
    let Some(header) = read_header(&mut file)? else {
        return Ok(None);
    };
    let Some(footer) = read_footer_at(&mut file)? else {
        return Ok(None);
    };
    let len = file.metadata()?.len();
    Ok(Some((header, footer, len)))
}

/// A record's phase, packed into idx entries as u8.
pub fn phase_to_u8(phase: LogPhase) -> u8 {
    match phase {
        LogPhase::Request => 0,
        LogPhase::Response => 1,
    }
}

pub fn phase_from_u8(v: u8) -> Option<LogPhase> {
    match v {
        0 => Some(LogPhase::Request),
        1 => Some(LogPhase::Response),
        _ => None,
    }
}

/// Parse an entry's RFC3339 timestamp into epoch millis. Entry timestamps
/// are self-generated (`Utc::now().to_rfc3339()`), so parse failure means a
/// foreign/corrupt record — `None` excludes it from footer time ranges.
pub fn entry_ts_ms(entry: &PromptLogEntry) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
        .ok()
        .map(|t| t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sample_entry(id: &str) -> PromptLogEntry {
        let mut e = PromptLogEntry::new_request(
            id,
            None,
            "kh",
            None,
            None,
            "m",
            "/p",
            false,
            std::sync::Arc::new(serde_json::json!({"q": id})),
            None,
            None,
        );
        e.timestamp = "2026-11-24T12:00:00+00:00".to_string();
        e
    }

    #[test]
    fn file_name_roundtrip() {
        let name = segment_file_name("gw-host-4000", 1732449600000, 7);
        assert!(name.starts_with("gw-host-4000-"));
        let parsed = parse_segment_file_name(&name).unwrap();
        assert_eq!(parsed.slug, "gw-host-4000");
        assert_eq!(parsed.start_ms, 1732449600000);
        assert_eq!(parsed.seq, 7);
        // idx files share the stem
        let stem = name.strip_suffix(".seg").unwrap();
        assert_eq!(
            parse_segment_stem(stem),
            Some(("gw-host-4000".into(), 1732449600000, 7))
        );
    }

    #[test]
    fn slug_sanitizes_colons() {
        assert_eq!(instance_slug("host:4000"), "host-4000");
        assert_eq!(instance_slug("a.b_c"), "a.b_c");
    }

    #[test]
    fn header_roundtrip() {
        let mut buf = Vec::new();
        write_header(&mut buf, "gw-1", 123).unwrap();
        let mut cur = io::Cursor::new(buf);
        let h = read_header(&mut cur).unwrap().unwrap();
        assert_eq!(h.slug, "gw-1");
        assert_eq!(h.start_ts_ms, 123);
        assert_eq!(h.data_offset, cur.position());
    }

    #[test]
    fn header_rejects_foreign_magic() {
        let mut buf = vec![0u8; 64];
        buf[0..8].copy_from_slice(b"NOTBLOM1");
        assert!(read_header(&mut io::Cursor::new(buf)).unwrap().is_none());
    }

    #[test]
    fn footer_roundtrip_and_crc_gate() {
        let footer = SegmentFooter {
            block_count: 3,
            record_count: 100,
            first_ts_ms: 1,
            last_ts_ms: 2,
            uncomp_bytes: 4096,
        };
        let mut buf = Vec::new();
        write_footer(&mut buf, &footer).unwrap();
        assert_eq!(buf.len(), FTR_SIZE);
        let back = read_footer_at(&mut io::Cursor::new(&buf)).unwrap().unwrap();
        assert_eq!(back, footer);

        // corrupt one byte → CRC mismatch → None (treated as unsealed)
        let mut torn = buf.clone();
        torn[12] ^= 0xff;
        assert!(read_footer_at(&mut io::Cursor::new(&torn)).unwrap().is_none());
    }

    #[test]
    fn block_roundtrip_and_frame_walk() {
        let mut payload = Vec::new();
        let mut count = 0;
        for i in 0..3 {
            let frame = encode_record_frame(&sample_entry(&format!("r{i}"))).unwrap();
            payload.extend_from_slice(&frame);
            count += 1;
        }
        let bytes = prepare_block(&payload, count, 1).unwrap();
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&bytes).unwrap();
        file.flush().unwrap();

        let uncomp = read_block_at(file.as_file(), 0).unwrap();
        assert_eq!(uncomp, payload);

        let mut seen = Vec::new();
        for_each_record(&uncomp, |frame, off| {
            let entry = decode_record(frame)?;
            seen.push((entry.request_id, off));
            Ok(())
        })
        .unwrap();
        // Offsets are cumulative frame starts in the uncompressed stream —
        // derive them from the same encoded frames instead of hardcoding sizes.
        let mut expected = Vec::new();
        let mut off = 0u64;
        for i in 0..3 {
            let frame = encode_record_frame(&sample_entry(&format!("r{i}"))).unwrap();
            expected.push((format!("r{i}"), off));
            off += frame.len() as u64;
        }
        assert_eq!(seen, expected);

        // corrupted magic → read error
        let mut bad = bytes.clone();
        bad[0] ^= 0xff;
        let mut f2 = tempfile::NamedTempFile::new().unwrap();
        f2.write_all(&bad).unwrap();
        f2.flush().unwrap();
        assert!(read_block_at(f2.as_file(), 0).is_err());
    }

    #[test]
    fn for_each_record_stops_at_torn_tail() {
        let frame = encode_record_frame(&sample_entry("x")).unwrap();
        let mut data = frame.clone();
        data.extend_from_slice(&frame[..frame.len() - 3]); // torn second frame
        let mut n = 0;
        for_each_record(&data, |_, _| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn scan_blocks_rebuilds_table_and_stops_at_bad_tail() {
        let mut payload = Vec::new();
        let frame = encode_record_frame(&sample_entry("a")).unwrap();
        payload.extend_from_slice(&frame);
        let b1 = prepare_block(&payload, 1, 1).unwrap();
        let payload2 = payload.clone();
        let b2 = prepare_block(&payload2, 1, 1).unwrap();

        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_header(file.as_file_mut(), "gw", 1).unwrap();
        let data_offset = file.as_file().metadata().unwrap().len();
        file.write_all(&b1).unwrap();
        file.write_all(&b2).unwrap();
        // torn tail: half a block header
        file.write_all(&b1[..10]).unwrap();
        file.flush().unwrap();

        let (blocks, end) = scan_blocks(file.as_file(), data_offset).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].abs_start, blocks[0].uncomp_len as u64);
        assert_eq!(end, data_offset + b1.len() as u64 + b2.len() as u64);
    }

    #[test]
    fn phase_packing_roundtrip() {
        assert_eq!(phase_from_u8(phase_to_u8(LogPhase::Request)), Some(LogPhase::Request));
        assert_eq!(phase_from_u8(phase_to_u8(LogPhase::Response)), Some(LogPhase::Response));
        assert!(phase_from_u8(9).is_none());
    }

    #[test]
    fn entry_ts_ms_parses() {
        let e = sample_entry("t");
        assert_eq!(entry_ts_ms(&e), Some(chrono::DateTime::parse_from_rfc3339("2026-11-24T12:00:00+00:00").unwrap().timestamp_millis()));
    }
}
