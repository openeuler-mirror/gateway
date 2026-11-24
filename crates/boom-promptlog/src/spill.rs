//! Overflow spill file — the middle tier of the no-loss backpressure
//! chain (memory channel → **spill disk** → drop).
//!
//! When the in-memory channel is over its byte budget (writer stalled,
//! transient disk pressure), producers append record frames directly to
//! `{dir}/spill.log` — sequential I/O, tens of µs per record, bounded by
//! a built-in budget. The background writer drains the spill back into
//! the segment store whenever the channel goes idle. Only when the spill
//! itself cannot take more (disk full / IO error / budget exceeded) does
//! an entry drop, with a counter + warn.
//!
//! Frame format is the segment record codec (`[u32 len][json]`), so the
//! drain path reuses `for_each_record` semantics: torn tails (a crash
//! mid-append) are discarded by the drain's EOF handling.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct SpillStore {
    /// Current `{dir}/spill.log`. Swapped by the writer after a config
    /// `dir` change (the old file is drained to zero first).
    path: Mutex<PathBuf>,
    inner: Mutex<SpillInner>,
    max_bytes: std::sync::atomic::AtomicU64,
}

struct SpillInner {
    file: Option<File>,
    bytes: u64,
}

impl SpillInner {
    fn closed() -> Self {
        Self {
            file: None,
            bytes: 0,
        }
    }
}

impl SpillStore {
    pub fn new(dir: &Path, max_bytes: u64) -> Self {
        Self {
            path: Mutex::new(spill_path(dir)),
            inner: Mutex::new(SpillInner::closed()),
            max_bytes: std::sync::atomic::AtomicU64::new(max_bytes),
        }
    }

    /// Point the spill at a new directory after the old one was drained.
    pub fn update_dir(&self, dir: &Path) {
        let new_path = spill_path(dir);
        let mut path = self.path.lock().unwrap();
        if *path != new_path {
            *path = new_path;
            let mut inner = self.inner.lock().unwrap();
            // The writer only rotates the dir after a full drain, so any
            // residue here is already lost content — start clean.
            *inner = SpillInner::closed();
        }
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Append one record frame (already `[u32 len][json]`-encoded). Fails
    /// with a plain io::Error when the spill budget is exhausted — the
    /// producer turns that into the drop+count path.
    ///
    /// Called synchronously from request handlers under contention only:
    /// the lock is held for one append (µs), and the spill is touched at
    /// all only when the writer is already stalled.
    pub fn try_write(&self, frame: &[u8]) -> io::Result<()> {
        let path = self.path.lock().unwrap().clone();
        let mut inner = self.inner.lock().unwrap();
        let max_bytes = self.max_bytes.load(std::sync::atomic::Ordering::Relaxed);
        if inner.bytes + frame.len() as u64 > max_bytes {
            return Err(io::Error::other("spill budget exhausted"));
        }
        if inner.file.is_none() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            inner.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| io::Error::other(format!("open spill {path:?}: {e}")))?,
            );
        }
        let file = inner.file.as_mut().unwrap();
        if let Err(e) = file.write_all(frame) {
            // A partial append leaves a torn frame the drain discards.
            inner.file = None;
            return Err(e);
        }
        inner.bytes += frame.len() as u64;
        Ok(())
    }

    /// Replay the spill through `f` (frame → store append). On a clean end
    /// (EOF or a torn tail) the file is truncated to zero and the next
    /// `try_write` starts fresh. When `f` fails mid-file the file is left
    /// intact and the next drain replays from the start — already-appended
    /// frames re-append and become duplicates, which is harmless (queries
    /// verify by request_id content) and far simpler than a persistent
    /// progress cursor for a path that only runs when the store itself is
    /// failing.
    ///
    /// The lock is held for the whole drain: producers wanting to spill
    /// during a drain wait (they are already in the degraded path), while
    /// the healthy path (`try_write` on an empty spill) never runs.
    pub fn drain<F>(&self, mut f: F) -> io::Result<u64>
    where
        F: FnMut(&[u8]) -> io::Result<()>,
    {
        let path = self.path.lock().unwrap().clone();
        let mut inner = self.inner.lock().unwrap();
        if inner.bytes == 0 {
            return Ok(0);
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| io::Error::other(format!("open spill {path:?}: {e}")))?;
        let mut consumed: u64 = 0;
        let replayed = replay_frames(&mut file, &mut consumed, &mut f);
        match replayed {
            Ok(n) => {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                // Drop the write handle too: it was opened without append,
                // and the next try_write must re-open in append mode.
                inner.file = None;
                inner.bytes = 0;
                Ok(n)
            }
            Err(e) => Err(e),
        }
    }

    /// Bytes currently in the spill (advisory, for diagnostics).
    pub fn bytes(&self) -> u64 {
        self.inner.lock().unwrap().bytes
    }
}

/// Read frames from the current position, invoking `f` on each record body
/// and advancing `consumed`. Stops at EOF (clean or torn) or the first `f`
/// error, returning the number of frames replayed.
fn replay_frames<F>(
    file: &mut File,
    consumed: &mut u64,
    f: &mut F,
) -> io::Result<u64>
where
    F: FnMut(&[u8]) -> io::Result<()>,
{
    let mut replayed = 0u64;
    loop {
        let mut len_buf = [0u8; 4];
        match file.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(replayed),
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        // A zero length or absurd size means a torn/corrupt frame — stop.
        if len == 0 || len > 256 * 1024 * 1024 {
            return Ok(replayed);
        }
        let mut body = vec![0u8; len];
        if let Err(e) = file.read_exact(&mut body) {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                return Ok(replayed); // torn tail — discard
            }
            return Err(e);
        }
        f(&body)?;
        *consumed += 4 + len as u64;
        replayed += 1;
    }
}

fn spill_path(dir: &Path) -> PathBuf {
    dir.join("spill.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(body: &str) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&(body.len() as u32).to_le_bytes());
        f.extend_from_slice(body.as_bytes());
        f
    }

    #[test]
    fn write_drain_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillStore::new(dir.path(), 1024);
        spill.try_write(&frame("one")).unwrap();
        spill.try_write(&frame("two")).unwrap();
        assert_eq!(spill.bytes(), frame("one").len() as u64 + frame("two").len() as u64);

        let mut seen = Vec::new();
        let n = spill
            .drain(|body| {
                seen.push(String::from_utf8_lossy(body).into_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(seen, vec!["one", "two"]);
        // File truncated back to zero.
        assert_eq!(std::fs::metadata(dir.path().join("spill.log")).unwrap().len(), 0);
        assert_eq!(spill.bytes(), 0);
    }

    #[test]
    fn drain_empty_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillStore::new(dir.path(), 1024);
        assert_eq!(spill.drain(|_| Ok(())).unwrap(), 0);
        assert!(!dir.path().join("spill.log").exists());
    }

    #[test]
    fn failed_frame_replays_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillStore::new(dir.path(), 1024);
        spill.try_write(&frame("keep")).unwrap();
        spill.try_write(&frame("fail")).unwrap();
        spill.try_write(&frame("after")).unwrap();

        let first = spill
            .drain(|body| {
                if body == b"fail" {
                    Err(io::Error::other("store append failed"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert!(first.to_string().contains("store append failed"));
        assert_eq!(spill.bytes(), 3 * 4 + 4 + 4 + 5); // file left intact

        // Next drain replays everything: "keep" duplicates (harmless by
        // design), "fail" and "after" append this time.
        let mut seen = Vec::new();
        let second = spill
            .drain(|body| {
                seen.push(String::from_utf8_lossy(body).into_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(second, 3);
        assert_eq!(seen, vec!["keep", "fail", "after"]);
        assert_eq!(spill.bytes(), 0);
    }

    #[test]
    fn torn_tail_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillStore::new(dir.path(), 1024);
        spill.try_write(&frame("ok")).unwrap();
        // Simulate a crash mid-append: raw partial frame bytes.
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.path().join("spill.log"))
            .unwrap();
        f.write_all(&[0x40, 0x00]).unwrap();
        drop(f);

        let mut seen = Vec::new();
        let n = spill
            .drain(|body| {
                seen.push(String::from_utf8_lossy(body).into_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(seen, vec!["ok"]);
        assert_eq!(std::fs::metadata(dir.path().join("spill.log")).unwrap().len(), 0);
    }

    #[test]
    fn budget_exhaustion_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillStore::new(dir.path(), 8);
        // frame = 4B length + body → 10 bytes against an 8-byte budget.
        assert!(spill.try_write(&frame("123456")).is_err());
        assert_eq!(spill.bytes(), 0);
    }
}
