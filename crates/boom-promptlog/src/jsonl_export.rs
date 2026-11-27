//! Instance-local rolling JSONL output for file collectors.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config::PromptLogConfig;
use crate::segment::parse_segment_stem;

const BUFFER_BYTES: usize = 512 * 1024;
const RETRY_DELAY: Duration = Duration::from_secs(5);
const MIB: u64 = 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
struct Settings {
    root: PathBuf,
    dir: PathBuf,
    file_bytes: u64,
    file_age: Option<Duration>,
    files: usize,
}

impl Settings {
    fn from_config(config: &PromptLogConfig) -> Option<Self> {
        let export = &config.jsonl_export;
        if !export.enabled {
            return None;
        }
        let root = PathBuf::from(&config.dir);
        let dir = if export.dir.trim().is_empty() {
            root.join("export")
        } else {
            PathBuf::from(&export.dir)
        };
        Some(Self {
            root,
            dir,
            file_bytes: export.max_file_mb.clamp(16, 2048) * MIB,
            file_age: (export.max_file_age_secs > 0)
                .then(|| Duration::from_secs(export.max_file_age_secs.clamp(60, 86400))),
            files: export.max_files.clamp(2, 500),
        })
    }

    fn total_bytes(&self) -> u64 {
        self.file_bytes * self.files as u64
    }
}

#[derive(Default)]
pub(crate) struct JsonlExport {
    settings: Option<Settings>,
    ring: Option<Ring>,
    failed_at: Option<Instant>,
    oversized_at: Option<Instant>,
}

impl JsonlExport {
    pub(crate) fn maintain(&mut self, config: &PromptLogConfig, slug: &str) {
        let settings = Settings::from_config(config);
        if self.settings != settings {
            self.close();
            self.settings = settings;
            self.failed_at = None;
            self.oversized_at = None;
        }
        let Some(settings) = &self.settings else {
            return;
        };
        if let Some(ring) = &mut self.ring {
            if let Err(error) = ring.tick(settings) {
                self.fail(error);
            }
        } else if self.failed_at.is_none_or(|at| at.elapsed() >= RETRY_DELAY) {
            match Ring::open(settings, slug) {
                Ok(ring) => self.ring = Some(ring),
                Err(error) => self.fail(error),
            }
        }
    }

    pub(crate) fn append(&mut self, json: &[u8]) {
        let (Some(settings), Some(ring)) = (&self.settings, &mut self.ring) else {
            return;
        };
        let line_bytes = json.len() as u64 + 1;
        if line_bytes > settings.total_bytes() {
            if self
                .oversized_at
                .is_none_or(|at| at.elapsed() >= RETRY_DELAY)
            {
                tracing::warn!(
                    line_bytes,
                    limit_bytes = settings.total_bytes(),
                    "prompt JSONL record exceeds the export capacity; skipping export"
                );
                self.oversized_at = Some(Instant::now());
            }
            return;
        }
        if let Err(error) = ring.append(settings, json, line_bytes) {
            self.fail(error);
        }
    }

    pub(crate) fn close(&mut self) {
        if let Some(ring) = &mut self.ring {
            if let Err(error) = ring.flush() {
                self.fail(error);
            }
        }
        self.ring = None;
    }

    fn fail(&mut self, error: io::Error) {
        if let Some(ring) = self.ring.take() {
            if let Some(active) = ring.active {
                // Never append further lines to a file after a partial write.
                let _ = active.writer.into_parts();
            }
        }
        let dir = self.settings.as_ref().map(|settings| &settings.dir);
        tracing::warn!(?dir, %error, "prompt JSONL export failed; will retry");
        self.failed_at = Some(Instant::now());
    }
}

struct ClosedFile {
    path: PathBuf,
    bytes: u64,
}

struct ActiveFile {
    file: ClosedFile,
    writer: BufWriter<File>,
    opened: Instant,
}

struct Ring {
    dir: PathBuf,
    slug: String,
    sequence: u64,
    closed: VecDeque<ClosedFile>,
    closed_bytes: u128,
    active: Option<ActiveFile>,
    // Serialize owners of the same slug, including overlapping restarts.
    _lock: File,
}

impl Ring {
    fn open(settings: &Settings, slug: &str) -> io::Result<Self> {
        fs::create_dir_all(&settings.root)?;
        fs::create_dir_all(&settings.dir)?;
        let dir = settings.dir.canonicalize()?;
        let root = settings.root.canonicalize()?;
        let segments = root.join("segments");
        let segments = segments.canonicalize().unwrap_or(segments);
        if dir == root || dir.starts_with(segments) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prompt JSONL export needs a directory separate from the Prompt log root and segments",
            ));
        }

        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!(".prompt-{slug}.lock")))?;
        lock.try_lock()?;
        let mut files = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some((owner, timestamp, sequence)) = name
                .to_str()
                .and_then(|name| name.strip_prefix("prompt-"))
                .and_then(|name| name.strip_suffix(".jsonl"))
                .and_then(parse_segment_stem)
            else {
                continue;
            };
            if owner == slug {
                let file = ClosedFile {
                    path: entry.path(),
                    bytes: entry.metadata()?.len(),
                };
                files.push((sequence, timestamp, file));
            }
        }
        // Sort numerically by sequence so clock regressions and seven-digit
        // sequence numbers cannot evict newer files first.
        files.sort_unstable_by_key(|(sequence, timestamp, _)| (*sequence, *timestamp));
        let sequence = files.last().map_or(0, |(sequence, _, _)| *sequence);
        let closed: VecDeque<_> = files.into_iter().map(|(_, _, file)| file).collect();
        let closed_bytes = closed.iter().map(|file| u128::from(file.bytes)).sum();
        let mut ring = Self {
            dir,
            slug: slug.to_owned(),
            sequence,
            closed,
            closed_bytes,
            active: None,
            _lock: lock,
        };
        ring.prune(settings.files, settings.total_bytes())?;
        Ok(ring)
    }

    fn append(&mut self, settings: &Settings, json: &[u8], line_bytes: u64) -> io::Result<()> {
        if self.active.as_ref().is_some_and(|active| {
            active.file.bytes.saturating_add(line_bytes) > settings.file_bytes
        }) {
            self.close_active()?;
        }
        if self.active.is_none() {
            let sequence = self
                .sequence
                .checked_add(1)
                .ok_or_else(|| io::Error::other("prompt JSONL file sequence exhausted"))?;
            let timestamp = chrono::Utc::now().timestamp_millis();
            let path = self.dir.join(format!(
                "prompt-{}-{timestamp}-{sequence:06}.jsonl",
                self.slug
            ));
            self.prune(settings.files - 1, settings.total_bytes() - line_bytes)?;
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            self.sequence = sequence;
            self.active = Some(ActiveFile {
                file: ClosedFile { path, bytes: 0 },
                writer: BufWriter::with_capacity(BUFFER_BYTES, file),
                opened: Instant::now(),
            });
        } else {
            // Keep the active file and evict only as much history as the
            // complete next line needs.
            let active_bytes = self.active.as_ref().unwrap().file.bytes;
            self.prune(
                settings.files - 1,
                settings.total_bytes() - active_bytes - line_bytes,
            )?;
        }
        let active = self.active.as_mut().expect("export file is open");
        active.writer.write_all(json)?;
        active.writer.write_all(b"\n")?;
        active.file.bytes += line_bytes;
        Ok(())
    }

    fn tick(&mut self, settings: &Settings) -> io::Result<()> {
        if self.active.as_ref().is_some_and(|active| {
            settings
                .file_age
                .is_some_and(|age| active.opened.elapsed() >= age)
        }) {
            return self.close_active();
        }
        self.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(active) = &mut self.active {
            active.writer.flush()?;
        }
        Ok(())
    }

    fn close_active(&mut self) -> io::Result<()> {
        self.flush()?;
        if let Some(active) = self.active.take() {
            self.closed_bytes += u128::from(active.file.bytes);
            self.closed.push_back(active.file);
        }
        Ok(())
    }

    fn prune(&mut self, max_files: usize, max_bytes: u64) -> io::Result<()> {
        while self.closed.len() > max_files || self.closed_bytes > u128::from(max_bytes) {
            let oldest = self.closed.front().expect("export exceeds capacity");
            match fs::remove_file(&oldest.path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            self.closed_bytes -= u128::from(oldest.bytes);
            self.closed.pop_front();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn settings(root: &Path, file_bytes: u64, files: usize) -> Settings {
        Settings {
            root: root.to_owned(),
            dir: root.join("export"),
            file_bytes,
            file_age: None,
            files,
        }
    }

    fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect()
    }

    #[test]
    fn rotation_keeps_all_lines_whole_when_capacity_is_available() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path(), 1024, 50);
        let mut ring = Ring::open(&settings, "gw-4000").unwrap();
        for id in 0..1000 {
            let json = format!(r#"{{"id":{id},"message":"hello"}}"#);
            ring.append(&settings, json.as_bytes(), json.len() as u64 + 1)
                .unwrap();
        }
        ring.flush().unwrap();
        let mut ids = Vec::new();
        for path in jsonl_files(&settings.dir) {
            let text = fs::read_to_string(path).unwrap();
            assert!(text.ends_with('\n'));
            assert!(text.len() <= 1024);
            ids.extend(text.lines().map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["id"]
                    .as_u64()
                    .unwrap()
            }));
        }
        ids.sort_unstable();
        assert_eq!(ids, (0..1000).collect::<Vec<_>>());
    }

    #[test]
    fn ring_restarts_keeps_the_newest_files_and_does_not_touch_other_instances() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path(), 3, 3);
        fs::create_dir(&settings.dir).unwrap();
        // A lower timestamp with a newer sequence simulates clock rollback.
        for name in [
            "prompt-gw-4000-999-999998.jsonl",
            "prompt-gw-4000-998-999999.jsonl",
            "prompt-gw-4000-997-1000000.jsonl",
            "prompt-gw-4000-extra-999-999999.jsonl",
        ] {
            fs::write(settings.dir.join(name), b"0\n").unwrap();
        }
        let mut ring = Ring::open(&settings, "gw-4000").unwrap();
        assert!(Ring::open(&settings, "gw-4000").is_err());
        for _ in 0..10 {
            ring.append(&settings, b"1", 2).unwrap();
        }
        ring.flush().unwrap();
        assert_eq!(ring.sequence, 1000010);
        assert_eq!(ring.closed.len(), 2);
        assert_eq!(jsonl_files(&settings.dir).len(), 4);
        assert!(
            settings
                .dir
                .join("prompt-gw-4000-extra-999-999999.jsonl")
                .exists()
        );
        drop(ring);
        let mut ring = Ring::open(&settings, "gw-4000").unwrap();
        ring.append(&settings, b"2", 2).unwrap();
        assert_eq!(ring.sequence, 1000011);
        assert_eq!(ring.closed.len(), 2);
        assert!(
            ring.closed
                .iter()
                .all(|file| fs::read(&file.path).unwrap() == b"1\n")
        );
    }

    #[test]
    fn oversize_line_stays_whole_and_uses_only_the_total_budget() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path(), 6, 3);
        let mut ring = Ring::open(&settings, "gw").unwrap();
        ring.append(&settings, b"12345", 6).unwrap();
        ring.append(&settings, b"12345", 6).unwrap();
        ring.append(&settings, b"123456789", 10).unwrap();
        ring.flush().unwrap();
        let files = jsonl_files(&settings.dir);
        assert_eq!(files.len(), 2);
        assert!(
            files
                .iter()
                .any(|file| fs::read(file).unwrap() == b"123456789\n")
        );
        assert_eq!(
            files
                .iter()
                .map(|file| fs::metadata(file).unwrap().len())
                .sum::<u64>(),
            16
        );
        ring.append(&settings, b"12345", 6).unwrap();
        assert_eq!(ring.closed_bytes, 10);

        let mut export = JsonlExport {
            settings: Some(settings),
            ring: Some(ring),
            ..JsonlExport::default()
        };
        let sequence = export.ring.as_ref().unwrap().sequence;
        export.append(b"123456789012345678");
        assert!(export.oversized_at.is_some());
        assert_eq!(export.ring.as_ref().unwrap().sequence, sequence);
        export.append(b"1");
        assert_eq!(export.ring.as_ref().unwrap().sequence, sequence + 1);
    }

    #[test]
    fn age_rotation_closes_the_file_and_does_not_create_an_empty_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = settings(dir.path(), 1024, 3);
        settings.file_age = Some(Duration::ZERO);
        let mut ring = Ring::open(&settings, "gw").unwrap();
        ring.tick(&settings).unwrap();
        assert!(jsonl_files(&settings.dir).is_empty());
        ring.append(&settings, b"1", 2).unwrap();
        ring.tick(&settings).unwrap();
        assert!(ring.active.is_none());
        ring.tick(&settings).unwrap();
        assert_eq!(jsonl_files(&settings.dir).len(), 1);
        ring.append(&settings, b"2", 2).unwrap();
        assert_eq!(ring.sequence, 2);
    }

    #[test]
    fn failed_prune_and_exhausted_sequences_cannot_overwrite_or_expand_the_ring() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings(dir.path(), 2, 2);
        let mut ring = Ring::open(&settings, "gw").unwrap();
        ring.append(&settings, b"1", 2).unwrap();
        ring.append(&settings, b"2", 2).unwrap();
        let oldest = &ring.closed.front().unwrap().path;
        fs::remove_file(oldest).unwrap();
        fs::create_dir(oldest).unwrap();
        assert!(ring.append(&settings, b"3", 2).is_err());
        assert_eq!(ring.sequence, 2);
        assert!(ring.active.is_none());
        ring.sequence = u64::MAX;
        assert!(ring.append(&settings, b"3", 2).is_err());
        assert_eq!(ring.sequence, u64::MAX);
    }
}
