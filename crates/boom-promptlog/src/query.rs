//! Read-only query API for the prompt log.
//!
//! Dashboard consumes this trait — it must not hold the `PromptLogWriter`
//! handle directly (that handle is owned by boom-main, and dashboard
//! shouldn't depend on the writer's type or its internals). The
//! segment-store implementation lives here because the block/index layout
//! is this crate's private detail.
//!
//! `find_pair` does real (bounded) disk I/O — bloom-pruned, at most a few
//! block reads — so async callers wrap it in `spawn_blocking`.

use crate::config::PromptLogConfig;
use crate::store::{EntryPair, SegmentStore};
use std::sync::Arc;

/// Snapshot of the parts of `PromptLogConfig` that the dashboard needs to
/// render its admin UI. We don't hand back the whole config to keep the
/// schema narrow and explicit — new private fields won't silently leak into
/// the dashboard.
#[derive(Debug, Clone)]
pub struct PromptLogConfigSnapshot {
    pub enabled: bool,
    pub dir: String,
    pub capture_raw_upstream: bool,
    pub excluded_keys: Vec<String>,
    pub excluded_teams: Vec<String>,
    pub record_headers: Vec<String>,
    /// Effective retention policy — the DB-adopted global value in DB mode,
    /// the YAML value in no-DB mode. The dashboard's retention form renders
    /// this.
    pub retention: crate::config::RetentionConfig,
    pub otlp_enabled: bool,
    pub otlp_endpoint: String,
}

impl From<&PromptLogConfig> for PromptLogConfigSnapshot {
    fn from(c: &PromptLogConfig) -> Self {
        Self {
            enabled: c.enabled,
            dir: c.dir.clone(),
            capture_raw_upstream: c.capture_raw_upstream,
            excluded_keys: c.excluded_keys.clone(),
            excluded_teams: c.excluded_teams.clone(),
            record_headers: c.record_headers.clone(),
            retention: c.retention.clone(),
            otlp_enabled: c.otlp.enabled,
            otlp_endpoint: c.otlp.endpoint.clone(),
        }
    }
}

/// Read-only window into the prompt log store. request_id is globally unique
/// (the gateway generates one per request), so it is the only lookup key —
/// no key_hash/team scoping is needed, and the id index prunes segments.
pub trait PromptLogQueryApi: Send + Sync {
    /// Look up both phases of one request by its request_id. An entry that
    /// never landed (still in flight, or dropped under pressure) is simply
    /// `None` on its half.
    fn find_pair(&self, request_id: &str) -> EntryPair;

    /// Snapshot of the live config at the moment of the call. Used by the
    /// dashboard to render the admin page (excluded lists, record_headers,
    /// otlp toggle) without holding the writer handle.
    fn config_snapshot(&self) -> PromptLogConfigSnapshot;

    /// Full live config clone. Used by the dashboard's toggle handlers —
    /// they read-modify-write via `PromptLogConfig::with_*` builders, then
    /// push the result back through `AdminCommand::UpdatePromptLogConfig`.
    /// The narrow `config_snapshot()` is for rendering; this is for mutation.
    fn full_config(&self) -> PromptLogConfig;
}

/// Segment-store-backed `PromptLogQueryApi`. Holds the store the writer
/// built plus the config handle (shared with the writer) so hot-reloads are
/// visible without rebuilding the trait object.
pub struct SegmentPromptLogQuery {
    store: Arc<SegmentStore>,
    config: Arc<arc_swap::ArcSwap<PromptLogConfig>>,
}

impl SegmentPromptLogQuery {
    pub fn new(store: Arc<SegmentStore>, config: Arc<arc_swap::ArcSwap<PromptLogConfig>>) -> Self {
        Self { store, config }
    }
}

impl PromptLogQueryApi for SegmentPromptLogQuery {
    fn find_pair(&self, request_id: &str) -> EntryPair {
        self.store.find_pair_sync(request_id)
    }

    fn config_snapshot(&self) -> PromptLogConfigSnapshot {
        PromptLogConfigSnapshot::from(self.config.load().as_ref())
    }

    fn full_config(&self) -> PromptLogConfig {
        self.config.load().as_ref().clone()
    }
}

/// Query handle for the degraded boot (the store failed to open — bad `dir`,
/// full disk, unreadable recovery): no entry ever landed anywhere, so
/// lookups return empty pairs. The config snapshot still works so the
/// dashboard renders the committed YAML state and shows the retention form.
pub struct DisabledPromptLogQuery {
    config: Arc<arc_swap::ArcSwap<PromptLogConfig>>,
}

impl DisabledPromptLogQuery {
    pub fn new(config: Arc<arc_swap::ArcSwap<PromptLogConfig>>) -> Self {
        Self { config }
    }
}

impl PromptLogQueryApi for DisabledPromptLogQuery {
    fn find_pair(&self, _request_id: &str) -> EntryPair {
        EntryPair::default()
    }

    fn config_snapshot(&self) -> PromptLogConfigSnapshot {
        PromptLogConfigSnapshot::from(self.config.load().as_ref())
    }

    fn full_config(&self) -> PromptLogConfig {
        self.config.load().as_ref().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::LogPhase;
    use arc_swap::ArcSwap;

    fn test_config(dir: &std::path::Path) -> Arc<ArcSwap<PromptLogConfig>> {
        let c = PromptLogConfig {
            enabled: true,
            dir: dir.to_string_lossy().into_owned(),
            segment: crate::config::SegmentConfig {
                max_segment_mb: 16,
                ..Default::default()
            },
            ..Default::default()
        };
        Arc::new(ArcSwap::from_pointee(c))
    }

    #[tokio::test]
    async fn find_pair_returns_both_phases() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let store = SegmentStore::open_sync(cfg.clone(), "gw-test").unwrap();

        let req = crate::entry::PromptLogEntry::new_request(
            "rid-1", None, "kh", None, None, "m", "/p", false,
            Arc::new(serde_json::json!({})), None, None,
        );
        store.append_entry(&req).unwrap();
        let resp = crate::entry::PromptLogEntry::new_response_from(&req);
        store.append_entry(&resp).unwrap();
        store.flush_active_block().await.unwrap();

        let q = SegmentPromptLogQuery::new(store, cfg);
        let pair = q.find_pair("rid-1");
        assert!(pair.complete());
        assert_eq!(pair.request.unwrap().phase, LogPhase::Request);
        assert_eq!(pair.response.unwrap().phase, LogPhase::Response);

        let absent = q.find_pair("nope");
        assert!(absent.request.is_none() && absent.response.is_none());
    }

    #[test]
    fn config_snapshot_reflects_live_state() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let store = SegmentStore::open_sync(cfg.clone(), "gw-test").unwrap();
        let q = SegmentPromptLogQuery::new(store, cfg.clone());

        let snap = q.config_snapshot();
        assert!(snap.enabled);
        assert_eq!(snap.dir, dir.path().to_string_lossy());
        assert!(snap.otlp_endpoint.is_empty());

        // Hot-reload: store a new config, snapshot reflects it immediately.
        let new_cfg = PromptLogConfig {
            enabled: false,
            excluded_keys: vec!["deadbeef".to_string()],
            ..Default::default()
        };
        cfg.store(Arc::new(new_cfg));
        let snap2 = q.config_snapshot();
        assert!(!snap2.enabled);
        assert_eq!(snap2.excluded_keys, vec!["deadbeef".to_string()]);
        assert_eq!(q.full_config().excluded_keys, vec!["deadbeef".to_string()]);
    }
}
