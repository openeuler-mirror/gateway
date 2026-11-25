//! request_id index: in-memory active-segment index + on-disk sealed `.idx`.
//!
//! Two lives of the same data:
//!
//! - **Active segment**: a `HashMap<id_hash, Vec<ActiveHit>>` recording each
//!   record's phase and absolute offset in the segment's uncompressed
//!   stream. Appended by the single writer, read by queries through the
//!   store's read lock.
//! - **Sealed segment**: the map is converted to a sorted-by-hash array of
//!   `IdxEntry` (with block file positions resolved via the block table) and
//!   persisted as `{seg}.idx` = small header + bloom filter + entries. A
//!   bloom hit prunes segments; a binary search finds candidate entries;
//!   the final check is always content verification (the record's actual
//!   request_id), so hash collisions can never return wrong data.
//!
//! Memory budget at 10 bits/entry bloom: 10M records ≈ 12MB of filters for
//! the whole registry; the sorted arrays stay on disk and are loaded per
//! segment lazily on first query (an LRU caps resident indexes).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::segment::{BlockLoc, IDX_ENTRY_SIZE, IDX_HEADER_SIZE, IDX_MAGIC};

/// xxhash64 of the request_id — the index/bloom key. Content verification
/// after lookup makes collisions harmless (a false positive costs one block
/// read, a missed entry is impossible: every written record is indexed).
pub fn id_hash(request_id: &str) -> u64 {
    twox_hash::XxHash64::oneshot(0, request_id.as_bytes())
}

// ─────────────────────────────────────────────────────────────────
// Bloom filter (double hashing, ~10 bits/entry, k=5 → ~1% fp)
// ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BloomFilter {
    bits: Vec<u64>,
    bit_count: u64, // multiple of 64
}

const BLOOM_HASHES: usize = 5;

impl BloomFilter {
    pub fn with_capacity(expected: usize) -> Self {
        // 10 bits per entry, rounded up to a 64-bit word boundary.
        let words = ((expected as u64).max(1) * 10).div_ceil(64).max(1);
        Self {
            bits: vec![0u64; words as usize],
            bit_count: words * 64,
        }
    }

    pub fn insert(&mut self, hash: u64) {
        let (h1, h2) = split_hash(hash, self.bit_count);
        let mut h = h1;
        for _ in 0..BLOOM_HASHES {
            self.set_bit(h);
            h = (h + h2) % self.bit_count;
        }
    }

    pub fn contains(&self, hash: u64) -> bool {
        let (h1, h2) = split_hash(hash, self.bit_count);
        let mut h = h1;
        for _ in 0..BLOOM_HASHES {
            if self.bits[(h / 64) as usize] >> (h % 64) & 1 == 0 {
                return false;
            }
            h = (h + h2) % self.bit_count;
        }
        true
    }

    fn set_bit(&mut self, pos: u64) {
        self.bits[(pos / 64) as usize] |= 1u64 << (pos % 64);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bits.len() * 8);
        for word in &self.bits {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut bits = Vec::with_capacity(bytes.len() / 8);
        let mut off = 0;
        while off + 8 <= bytes.len() {
            bits.push(u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap()));
            off += 8;
        }
        let bit_count = (bits.len() as u64) * 64;
        Self { bits, bit_count }
    }

    pub fn bit_count(&self) -> u64 {
        self.bit_count
    }
}

fn split_hash(hash: u64, bit_count: u64) -> (u64, u64) {
    // h2 forced odd so the double-hash stride is coprime with the bit space.
    let h1 = hash % bit_count;
    let mut h2 = (hash >> 32) | 1;
    if h2.is_multiple_of(bit_count) {
        h2 = 1;
    }
    (h1, h2)
}

// ─────────────────────────────────────────────────────────────────
// Active-segment index (in-memory)
// ─────────────────────────────────────────────────────────────────

/// One indexed record in the active segment: phase + absolute offset of its
/// frame in the segment's uncompressed stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveHit {
    pub phase: u8,
    pub abs_off: u64,
}

pub type ActiveIndex = HashMap<u64, Vec<ActiveHit>>;

// ─────────────────────────────────────────────────────────────────
// Sealed index (.idx file)
// ─────────────────────────────────────────────────────────────────

/// Entry: where one record lives — block file position + lengths to read
/// the block back, intra-block offset to reach the frame, phase to select
/// between request/response halves. 32 bytes on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdxEntry {
    pub id_hash: u64,
    pub block_file_pos: u64,
    pub comp_len: u32,
    pub uncomp_len: u32,
    pub intra_off: u32,
    pub phase: u8,
}

pub struct SegmentIndex {
    pub entries: Vec<IdxEntry>,
    pub bloom: BloomFilter,
}

impl SegmentIndex {
    /// Convert the active in-memory index into the sealed form. The block
    /// table resolves each record's absolute offset to (block file
    /// position, lengths, intra offset).
    pub fn build(active: &ActiveIndex, blocks: &[BlockLoc], record_count: u64) -> Self {
        let mut entries = Vec::with_capacity(
            active.values().map(|v| v.len()).sum::<usize>().max(record_count as usize),
        );
        let mut bloom = BloomFilter::with_capacity(entries.capacity());
        for (hash, hits) in active {
            for hit in hits {
                let Some(block) = locate_block(blocks, hit.abs_off) else {
                    continue;
                };
                entries.push(IdxEntry {
                    id_hash: *hash,
                    block_file_pos: block.file_pos,
                    comp_len: block.comp_len,
                    uncomp_len: block.uncomp_len,
                    intra_off: (hit.abs_off - block.abs_start) as u32,
                    phase: hit.phase,
                });
                bloom.insert(*hash);
            }
        }
        entries.sort_unstable_by_key(|e| (e.id_hash, e.phase));
        Self { entries, bloom }
    }

    /// Persist as `{seg}.idx` (temp file + atomic rename — a torn .idx is
    /// never visible because the segment is only registered as sealed
    /// after this returns).
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let bloom_bytes = self.bloom.to_bytes();
        let mut buf = Vec::with_capacity(IDX_HEADER_SIZE + bloom_bytes.len() + self.entries.len() * IDX_ENTRY_SIZE);
        buf.extend_from_slice(&IDX_MAGIC.to_le_bytes());
        buf.extend_from_slice(&crate::segment::FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&(IDX_ENTRY_SIZE as u16).to_le_bytes());
        buf.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(bloom_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&bloom_bytes);
        for e in &self.entries {
            buf.extend_from_slice(&e.id_hash.to_le_bytes());
            buf.extend_from_slice(&e.block_file_pos.to_le_bytes());
            buf.extend_from_slice(&e.comp_len.to_le_bytes());
            buf.extend_from_slice(&e.uncomp_len.to_le_bytes());
            buf.extend_from_slice(&e.intra_off.to_le_bytes());
            buf.extend_from_slice(&[e.phase, 0]);
            buf.extend_from_slice(&0u16.to_le_bytes());
        }
        let tmp = path.with_extension("idx.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&buf)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load a `.idx` file. Returns `Ok(None)` for foreign magic / short file
    /// so the caller can decide to rebuild or skip.
    pub fn load(path: &Path) -> io::Result<Option<Self>> {
        let mut f = std::fs::File::open(path)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        if buf.len() < IDX_HEADER_SIZE {
            return Ok(None);
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != IDX_MAGIC {
            return Ok(None);
        }
        let count = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
        let bloom_len = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        let bloom_end = IDX_HEADER_SIZE + bloom_len;
        if buf.len() < bloom_end + count * IDX_ENTRY_SIZE {
            return Ok(None);
        }
        let bloom = BloomFilter::from_bytes(&buf[IDX_HEADER_SIZE..bloom_end]);
        let mut entries = Vec::with_capacity(count);
        let mut off = bloom_end;
        for _ in 0..count {
            let e = IdxEntry {
                id_hash: u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()),
                block_file_pos: u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap()),
                comp_len: u32::from_le_bytes(buf[off + 16..off + 20].try_into().unwrap()),
                uncomp_len: u32::from_le_bytes(buf[off + 20..off + 24].try_into().unwrap()),
                intra_off: u32::from_le_bytes(buf[off + 24..off + 28].try_into().unwrap()),
                phase: buf[off + 28],
            };
            entries.push(e);
            off += IDX_ENTRY_SIZE;
        }
        Ok(Some(Self { entries, bloom }))
    }

    /// All index entries for a hash (empty slice = not in this segment).
    /// Content verification downstream keeps hash collisions harmless.
    pub fn lookup(&self, hash: u64) -> &[IdxEntry] {
        // partition_point, not binary_search: binary_search may return any of
        // the duplicates, and this must find the FIRST entry with this hash.
        let start = self.entries.partition_point(|e| e.id_hash < hash);
        let mut end = start;
        while end < self.entries.len() && self.entries[end].id_hash == hash {
            end += 1;
        }
        &self.entries[start..end]
    }
}

/// Find the block containing absolute stream offset `abs_off` (the last
/// block whose `abs_start <= abs_off`). Shared by the index builder and the
/// store's active-segment query path.
pub fn locate_block(blocks: &[BlockLoc], abs_off: u64) -> Option<&BlockLoc> {
    // Blocks are ordered by abs_start; the last block covers offsets beyond
    // its uncompressed length while it is still open, but every sealed
    // record falls strictly inside one.
    let i = blocks.partition_point(|b| b.abs_start <= abs_off);
    if i == 0 {
        None
    } else {
        Some(&blocks[i - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{prepare_block, BlockLoc};
    use std::path::PathBuf;

    fn block(file_pos: u64, uncomp_len: u32, abs_start: u64) -> BlockLoc {
        BlockLoc {
            file_pos,
            comp_len: 100,
            uncomp_len,
            abs_start,
            count: 1,
        }
    }

    #[test]
    fn bloom_has_no_false_negatives() {
        let mut b = BloomFilter::with_capacity(1000);
        let ids: Vec<u64> = (0..1000).map(|i| id_hash(&format!("req-{i}"))).collect();
        for h in &ids {
            b.insert(*h);
        }
        for h in &ids {
            assert!(b.contains(*h));
        }
        // False positive rate stays ~1% at 10 bits/entry — 100 random probes
        // should almost never produce more than a few hits.
        let fp: usize = (0..1000)
            .filter(|i| b.contains(id_hash(&format!("absent-{i}"))))
            .count();
        assert!(fp < 50, "false positive rate too high: {fp}/1000");
    }

    #[test]
    fn build_resolves_offsets_via_block_table() {
        let mut active: ActiveIndex = HashMap::new();
        let h1 = id_hash("req-a");
        active.insert(
            h1,
            vec![
                ActiveHit { phase: 0, abs_off: 4 },
                ActiveHit { phase: 1, abs_off: 100 },
            ],
        );
        let blocks = vec![
            block(50, 2048, 0),
            block(400, 1024, 2048),
        ];
        let idx = SegmentIndex::build(&active, &blocks, 2);
        assert_eq!(idx.entries.len(), 2);
        // hit at abs 4 → block 0 (file_pos 50, intra 4); hit at abs 100 → block 0 too
        assert_eq!(idx.entries[0].block_file_pos, 50);
        assert_eq!(idx.entries[0].intra_off, 4);
        assert_eq!(idx.entries[1].intra_off, 100);
        // both sorted by phase
        assert_eq!(idx.entries[0].phase, 0);
        assert_eq!(idx.entries[1].phase, 1);

        let hits = idx.lookup(h1);
        assert_eq!(hits.len(), 2);
        assert!(idx.lookup(id_hash("nope")).is_empty());
        assert!(idx.bloom.contains(h1));
    }

    #[test]
    fn idx_write_load_roundtrip() {
        let mut active: ActiveIndex = HashMap::new();
        for i in 0..50 {
            active.insert(id_hash(&format!("id-{i}")), vec![ActiveHit { phase: 0, abs_off: i * 8 }]);
        }
        let blocks = vec![block(10, 4096, 0)];
        let idx = SegmentIndex::build(&active, &blocks, 50);
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().join("t.idx");
        idx.write(&path).unwrap();
        let back = SegmentIndex::load(&path).unwrap().unwrap();
        assert_eq!(back.entries.len(), 50);
        for (a, b) in idx.entries.iter().zip(back.entries.iter()) {
            assert_eq!(a, b);
        }
        assert!(back.bloom.contains(id_hash("id-7")));
    }

    #[test]
    fn idx_load_rejects_foreign_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.idx");
        std::fs::write(&path, b"junk-junk-junk").unwrap();
        assert!(SegmentIndex::load(&path).unwrap().is_none());
    }

    #[test]
    fn prepare_block_smoke_via_index_pipeline() {
        // End-to-end micro-check: encode a record, block it, locate it.
        // All-zero input compresses to almost nothing, so only assert the
        // block carries its header plus a non-empty payload.
        let payload = vec![0u8; 128];
        let bytes = prepare_block(&payload, 1, 1).unwrap();
        assert!(bytes.len() > crate::segment::BLOCK_HEADER_SIZE);
        let blocks = vec![BlockLoc {
            file_pos: 0,
            comp_len: (bytes.len() - 20) as u32,
            uncomp_len: 128,
            abs_start: 0,
            count: 1,
        }];
        assert_eq!(locate_block(&blocks, 127).unwrap().uncomp_len, 128);
        assert!(locate_block(&blocks, 128).is_some());
    }
}
