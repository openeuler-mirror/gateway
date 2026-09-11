//! `AlertManager` — active-alert map + bounded history ring + notifier hook.
//!
//! `raise_alert` is idempotent per key (re-raise refreshes target/message but
//! keeps the original `raised_at` and does not re-fire the notifier);
//! `clear_alert` moves the alert into history with a `cleared_at` timestamp.
//! Unknown-key clears are no-ops, so reconciler passes and event hooks can
//! fire independently without ordering hazards.

use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

use dashmap::DashMap;

use boom_core::alert::{
    now_unix_ms, Alert, AlertApi, AlertKind, AlertNotifier, AlertSnapshot, AlertStatus,
};

const DEFAULT_HISTORY_CAP: usize = 500;

pub struct AlertManager {
    active: DashMap<String, Alert>,
    history: RwLock<VecDeque<Alert>>,
    history_cap: usize,
    notifier: RwLock<Option<Arc<dyn AlertNotifier>>>,
}

impl Default for AlertManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AlertManager {
    pub fn new() -> Self {
        Self::with_history_cap(DEFAULT_HISTORY_CAP)
    }

    pub fn with_history_cap(cap: usize) -> Self {
        Self {
            active: DashMap::new(),
            history: RwLock::new(VecDeque::with_capacity(cap.min(64))),
            history_cap: cap,
            notifier: RwLock::new(None),
        }
    }

    /// Register (or remove with `None`) a push notifier. Called on state
    /// transitions only.
    pub fn set_notifier(&self, notifier: Option<Arc<dyn AlertNotifier>>) {
        *self.notifier.write().expect("alert notifier lock poisoned") = notifier;
    }

    /// Raise (or refresh) an active alert. Idempotent per key: an existing
    /// active alert keeps its original `raised_at`; only target/message are
    /// refreshed. Returns `true` when this call transitioned the alert from
    /// absent to active.
    pub fn raise_alert(
        &self,
        kind: AlertKind,
        key: impl Into<String>,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> bool {
        let key = key.into();
        let mut raised = false;
        let alert = match self.active.get_mut(&key) {
            Some(mut existing) => {
                existing.target = target.into();
                existing.message = message.into();
                existing.clone()
            }
            None => {
                raised = true;
                let alert = Alert {
                    key: key.clone(),
                    kind,
                    target: target.into(),
                    message: message.into(),
                    status: AlertStatus::Active,
                    raised_at: now_unix_ms(),
                    cleared_at: None,
                };
                self.active.insert(key, alert.clone());
                alert
            }
        };
        if raised {
            tracing::warn!(alert_key = %alert.key, target = %alert.target, "alert raised: {}", alert.message);
            self.fire(|n| n.on_raise(&alert));
        }
        raised
    }

    /// Clear an active alert: move it to history with `cleared_at` set.
    /// No-op (returns `None`) when the key has no active alert.
    pub fn clear_alert(&self, key: &str) -> Option<Alert> {
        let (_, mut alert) = self.active.remove(key)?;
        alert.status = AlertStatus::Cleared;
        alert.cleared_at = Some(now_unix_ms());

        {
            let mut history = self.history.write().expect("alert history lock poisoned");
            if history.len() >= self.history_cap {
                history.pop_back();
            }
            history.push_front(alert.clone());
        }

        tracing::info!(alert_key = %alert.key, "alert cleared: {}", alert.message);
        self.fire(|n| n.on_clear(&alert));
        Some(alert)
    }

    /// Active alert keys (for reconciler reverse-check passes).
    pub fn active_keys(&self) -> Vec<String> {
        self.active.iter().map(|e| e.key().clone()).collect()
    }

    fn fire<F: Fn(&Arc<dyn AlertNotifier>)>(&self, f: F) {
        let guard = self.notifier.read().expect("alert notifier lock poisoned");
        if let Some(n) = guard.as_ref() {
            f(n);
        }
    }
}

#[async_trait::async_trait]
impl AlertApi for AlertManager {
    async fn snapshot(&self) -> AlertSnapshot {
        let mut active: Vec<Alert> =
            self.active.iter().map(|e| e.value().clone()).collect();
        active.sort_by(|a, b| b.raised_at.cmp(&a.raised_at));

        let history: Vec<Alert> = {
            let history = self.history.read().expect("alert history lock poisoned");
            history.iter().cloned().collect()
        };

        let active_count = active.len();
        AlertSnapshot {
            healthy: active_count == 0,
            active_count,
            active,
            history,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingNotifier {
        raises: AtomicUsize,
        clears: AtomicUsize,
    }

    impl AlertNotifier for CountingNotifier {
        fn on_raise(&self, _alert: &Alert) {
            self.raises.fetch_add(1, Ordering::SeqCst);
        }
        fn on_clear(&self, _alert: &Alert) {
            self.clears.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn raise_is_idempotent_per_key() {
        let m = AlertManager::new();
        assert!(m.raise_alert(
            AlertKind::DeploymentAutoDisabled,
            "deployment:d1",
            "model-a",
            "down",
        ));
        assert!(!m.raise_alert(
            AlertKind::DeploymentAutoDisabled,
            "deployment:d1",
            "model-a",
            "down again",
        ));
        let snap = futures_block(m.snapshot());
        assert_eq!(snap.active.len(), 1);
        assert_eq!(snap.active[0].message, "down again");
        assert_eq!(snap.active[0].status, AlertStatus::Active);
        assert!(snap.active[0].cleared_at.is_none());
        assert!(!snap.healthy);
    }

    #[test]
    fn clear_moves_alert_to_history() {
        let m = AlertManager::new();
        m.raise_alert(
            AlertKind::OtelExporterOffline,
            "otel:exporter",
            "http://otel:4318",
            "offline",
        );
        let cleared = m.clear_alert("otel:exporter").expect("cleared");
        assert_eq!(cleared.status, AlertStatus::Cleared);
        assert!(cleared.cleared_at.is_some());

        assert!(m.clear_alert("otel:exporter").is_none());

        let snap = futures_block(m.snapshot());
        assert!(snap.healthy);
        assert_eq!(snap.active_count, 0);
        assert_eq!(snap.history.len(), 1);
        assert_eq!(snap.history[0].key, "otel:exporter");
    }

    #[test]
    fn history_ring_is_bounded() {
        let m = AlertManager::with_history_cap(3);
        for i in 0..5 {
            let key = format!("deployment:d{i}");
            m.raise_alert(AlertKind::DeploymentAutoDisabled, key.clone(), "m", "x");
            m.clear_alert(&key);
        }
        let snap = futures_block(m.snapshot());
        assert_eq!(snap.history.len(), 3);
        // newest first
        assert_eq!(snap.history[0].key, "deployment:d4");
        assert_eq!(snap.history[2].key, "deployment:d2");
    }

    #[test]
    fn active_keys_lists_keys() {
        let m = AlertManager::new();
        m.raise_alert(AlertKind::DeploymentAutoDisabled, "deployment:d1", "m", "x");
        m.raise_alert(AlertKind::OtelExporterOffline, "otel:exporter", "ep", "x");
        let mut keys = m.active_keys();
        keys.sort();
        assert_eq!(keys, vec!["deployment:d1".to_string(), "otel:exporter".to_string()]);
    }

    #[test]
    fn notifier_fires_on_transitions_only() {
        let m = AlertManager::new();
        let n = Arc::new(CountingNotifier {
            raises: AtomicUsize::new(0),
            clears: AtomicUsize::new(0),
        });
        m.set_notifier(Some(n.clone()));

        m.raise_alert(AlertKind::DeploymentAutoDisabled, "deployment:d1", "m", "x");
        m.raise_alert(AlertKind::DeploymentAutoDisabled, "deployment:d1", "m", "x"); // idempotent
        m.clear_alert("deployment:d1");
        m.clear_alert("deployment:d1"); // no-op, no fire

        assert_eq!(n.raises.load(Ordering::SeqCst), 1);
        assert_eq!(n.clears.load(Ordering::SeqCst), 1);
    }

    /// `AlertApi::snapshot` is async; the manager itself is sync, so block
    /// briefly in tests.
    fn futures_block(snap: impl std::future::Future<Output = AlertSnapshot>) -> AlertSnapshot {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(snap)
    }
}
