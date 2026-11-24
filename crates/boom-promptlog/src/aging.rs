//! Retention / aging enforcement for sealed segments.
//!
//! Gated by `retention.enabled` (default **false**): when off, no policy is
//! evaluated, nothing is ever deleted — the store grows unboundedly, matching
//! the pre-refactor JSONL behavior. Operators opt in per fleet via the
//! DB-authoritative retention KV.
//!
//! Policy (all three, evaluated in this order, only when enabled):
//! 1. `max_age_days`: delete when a segment's newest record (footer
//!    `last_ts_ms` — data time, not mtime; a restored backup or a copied
//!    tree ages by its content) is older than the cutoff.
//! 2. `max_total_gb`: delete oldest-first until the sealed total fits.
//! 3. `disk_pct_watermark`: when the store's filesystem is fuller than the
//!    watermark, delete oldest-first until below it (or nothing remains).
//!
//! Anti-misdeletion: deletion candidates come **only** from the store's
//! in-memory registry, which recovery built from validated scans. Before
//! each unlink the candidate is re-validated through a chain — parent
//! directory, exact filename pattern with matching fields, this
//! instance's slug prefix, and content self-verification (magic + footer
//! CRC via `read_sealed_meta`). A file that fails any step is reported and
//! skipped: 宁漏勿错 — leaving an orphan behind is recoverable, deleting
//! someone else's file is not. The active segment is never a candidate
//! (it is not in the sealed registry).

use std::io;

use crate::config::RetentionConfig;
use crate::segment::{parse_segment_file_name, read_sealed_meta};
use crate::store::{SealedMeta, SegmentStore};

#[derive(Debug, Default, Clone)]
pub struct AgingReport {
    pub deleted_segments: u64,
    pub freed_bytes: u64,
}

/// One enforcement pass. Sync (disk validation + unlink); the writer calls
/// it from `spawn_blocking` after each seal and on its retention timer.
/// A no-op when the retention master switch is off.
pub fn enforce_once(store: &SegmentStore, cfg: &RetentionConfig) -> io::Result<AgingReport> {
    let mut report = AgingReport::default();
    if !cfg.enabled {
        return Ok(report);
    }

    if cfg.max_age_days > 0 {
        let cutoff =
            chrono::Utc::now().timestamp_millis() - cfg.max_age_days as i64 * 86_400_000;
        for meta in store.sealed_metas() {
            if meta.last_ts_ms < cutoff {
                delete_one(store, &meta, &mut report, "aged out");
            }
        }
    }

    if cfg.max_total_gb > 0 {
        let cap = cfg.max_total_gb * 1024 * 1024 * 1024;
        loop {
            let metas = store.sealed_metas();
            let total: u64 = metas.iter().map(|m| m.bytes).sum();
            if total <= cap || metas.is_empty() {
                break;
            }
            delete_one(store, &metas[0], &mut report, "total-size cap");
        }
    }

    if cfg.disk_pct_watermark > 0 {
        let dir = store.current_dir();
        loop {
            let metas = store.sealed_metas();
            if metas.is_empty() {
                break;
            }
            let Ok(avail) = fs2::available_space(&dir) else {
                break;
            };
            let Ok(total) = fs2::total_space(&dir) else {
                break;
            };
            if total == 0 {
                break;
            }
            let used_pct = (total - avail) * 100 / total;
            if used_pct < cfg.disk_pct_watermark as u64 {
                break;
            }
            delete_one(store, &metas[0], &mut report, "disk watermark");
        }
    }

    Ok(report)
}

fn delete_one(
    store: &SegmentStore,
    meta: &SealedMeta,
    report: &mut AgingReport,
    reason: &str,
) {
    if let Err(why) = validate_candidate(store, meta) {
        tracing::warn!(
            path = ?meta.path,
            %why,
            "retention candidate rejected by the validation chain; skipping"
        );
        return;
    }
    match store.remove_sealed(meta) {
        Ok(()) => {
            report.deleted_segments += 1;
            report.freed_bytes += meta.bytes;
            tracing::info!(
                path = ?meta.path,
                bytes = meta.bytes,
                records = meta.record_count,
                last_ts_ms = meta.last_ts_ms,
                reason,
                "deleted sealed segment (retention)"
            );
        }
        Err(e) => {
            tracing::warn!(path = ?meta.path, error = %e, "retention delete failed; will retry next pass");
        }
    }
}

/// The anti-misdeletion chain. Every step must pass; failure returns why.
fn validate_candidate(store: &SegmentStore, meta: &SealedMeta) -> Result<(), String> {
    // 1. The candidate must live in our segments directory.
    if meta.path.parent() != Some(store.current_dir().as_path()) {
        return Err("path is not inside the store's segments directory".into());
    }
    // 2. Exact filename pattern, fields matching the registry entry.
    let name = meta
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "path has no valid file name".to_string())?;
    let parsed = parse_segment_file_name(name)
        .ok_or_else(|| "file name does not match the segment pattern".to_string())?;
    if parsed.slug != meta.slug || parsed.start_ms != meta.start_ms || parsed.seq != meta.seq {
        return Err("file name fields disagree with the registry entry".into());
    }
    // 3. Our instance's slug prefix.
    if parsed.slug != store.instance_slug() {
        return Err("segment belongs to another instance".into());
    }
    // 4. Content self-verification: segment magic + footer CRC.
    match read_sealed_meta(&meta.path) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("file is not a CRC-valid sealed segment".into()),
        Err(e) => Err(format!("segment verification failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::PromptLogEntry;
    use crate::store::SegmentStore;
    use arc_swap::ArcSwap;
    use std::sync::Arc;

    fn test_store(dir: &std::path::Path) -> Arc<SegmentStore> {
        let c = crate::config::PromptLogConfig {
            dir: dir.to_string_lossy().into_owned(),
            ..Default::default()
        };
        SegmentStore::open_sync(Arc::new(ArcSwap::from_pointee(c)), "gw-test").unwrap()
    }

    fn entry(id: &str) -> PromptLogEntry {
        PromptLogEntry::new_request(
            id,
            None,
            "kh",
            None,
            None,
            "m",
            "/p",
            false,
            Arc::new(serde_json::json!({"q": id})),
            None,
            None,
        )
    }

    #[tokio::test]
    async fn disabled_retention_never_deletes_even_when_policies_would_fire() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // Entry stamped 30 days ago — every policy would fire if armed.
        let mut e = entry("r1");
        e.timestamp = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        store.append_entry(&e).unwrap();
        store.seal_active().await.unwrap();

        // Default config: enabled=false, master switch off.
        let cfg = crate::config::RetentionConfig {
            max_age_days: 7,
            max_total_gb: 1,
            disk_pct_watermark: 1,
            ..Default::default()
        };
        let report = enforce_once(&store, &cfg).unwrap();
        assert_eq!(report.deleted_segments, 0);
        assert_eq!(store.stats().sealed_segments, 1);
    }

    #[tokio::test]
    async fn fresh_segments_survive_all_policies_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        store.append_entry(&entry("r1")).unwrap();
        store.seal_active().await.unwrap();

        // Fresh data + plenty of disk: every policy armed, nothing fires.
        let cfg = crate::config::RetentionConfig {
            enabled: true,
            max_age_days: 7,
            max_total_gb: 1,
            disk_pct_watermark: 90,
            ..Default::default()
        };
        let report = enforce_once(&store, &cfg).unwrap();
        assert_eq!(report.deleted_segments, 0);
        assert_eq!(store.stats().sealed_segments, 1);
    }

    #[tokio::test]
    async fn aged_out_segment_is_deleted_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut e = entry("r1");
        e.timestamp = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        store.append_entry(&e).unwrap();
        store.seal_active().await.unwrap();

        let cfg = crate::config::RetentionConfig {
            enabled: true,
            max_age_days: 7,
            ..Default::default()
        };
        let report = enforce_once(&store, &cfg).unwrap();
        assert_eq!(report.deleted_segments, 1);
        assert_eq!(store.stats().sealed_segments, 0);
    }

    #[test]
    fn validation_chain_rejects_forgeries() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let sealed_dir = store.current_dir();

        // A registry-style meta pointing outside the segments dir.
        let outside = SealedMeta {
            path: std::env::temp_dir().join("somewhere-else-000000.seg"),
            idx_path: std::env::temp_dir().join("somewhere-else.idx"),
            slug: "gw-test".into(),
            start_ms: 1,
            seq: 1,
            first_ts_ms: 1,
            last_ts_ms: 1,
            record_count: 1,
            uncomp_bytes: 1,
            bytes: 1,
        };
        assert!(validate_candidate(&store, &outside).is_err());

        // Right directory, but the file is not a segment at all.
        let bogus = sealed_dir.join("gw-test-1-000001.seg");
        std::fs::write(&bogus, b"not a segment").unwrap();
        let meta = SealedMeta {
            path: bogus.clone(),
            idx_path: sealed_dir.join("gw-test-1-000001.idx"),
            slug: "gw-test".into(),
            start_ms: 1,
            seq: 1,
            first_ts_ms: 1,
            last_ts_ms: 1,
            record_count: 1,
            uncomp_bytes: 1,
            bytes: 15,
        };
        assert!(validate_candidate(&store, &meta).is_err());

        // A foreign instance's name in our directory.
        let foreign = SealedMeta {
            path: sealed_dir.join("other-1-000001.seg"),
            idx_path: sealed_dir.join("other-1-000001.idx"),
            slug: "other".into(),
            start_ms: 1,
            seq: 1,
            first_ts_ms: 1,
            last_ts_ms: 1,
            record_count: 1,
            uncomp_bytes: 1,
            bytes: 1,
        };
        assert!(validate_candidate(&store, &foreign).is_err());
    }
}
