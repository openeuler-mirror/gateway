//! DT 用例 — boom-alert：内存告警管理器。
//! 覆盖 raise/clear 幂等性、history ring 边界、notifier 触发时机、snapshot 排序。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use boom_alert::{Alert, AlertApi, AlertKind, AlertManager, AlertNotifier, AlertStatus};

/// 计数型 notifier — 验证 on_raise/on_clear 只在状态迁移时触发。
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

/// DT-ALR-01：新 raise 返回 true 并置 Active，重复 raise 返回 false 且刷新 message。
#[tokio::test]
async fn raise_is_idempotent_per_key() {
    let m = AlertManager::new();
    assert!(m.raise_alert(
        AlertKind::DeploymentAutoDisabled,
        "deployment:d1",
        "model-a",
        "down",
    ));
    // 重复 raise 同 key：返回 false，但 message 被刷新
    assert!(!m.raise_alert(
        AlertKind::DeploymentAutoDisabled,
        "deployment:d1",
        "model-a",
        "down again",
    ));
    let snap = m.snapshot().await;
    assert_eq!(snap.active.len(), 1);
    assert_eq!(snap.active[0].message, "down again");
    assert_eq!(snap.active[0].status, AlertStatus::Active);
    assert!(snap.active[0].cleared_at.is_none());
    assert!(!snap.healthy);
}

/// DT-ALR-02：clear 命中时移到 history 并置 Cleared，再 clear 同 key 返回 None。
#[tokio::test]
async fn clear_moves_alert_to_history() {
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
    // 再 clear 已不存在的 key — no-op
    assert!(m.clear_alert("otel:exporter").is_none());

    let snap = m.snapshot().await;
    assert!(snap.healthy);
    assert_eq!(snap.active_count, 0);
    assert_eq!(snap.history.len(), 1);
    assert_eq!(snap.history[0].key, "otel:exporter");
}

/// DT-ALR-03：history ring 超容量时丢弃最旧，保持 newest-first。
#[tokio::test]
async fn history_ring_is_bounded() {
    let m = AlertManager::with_history_cap(3);
    for i in 0..5 {
        let key = format!("deployment:d{i}");
        m.raise_alert(AlertKind::DeploymentAutoDisabled, key.clone(), "m", "x");
        m.clear_alert(&key);
    }
    let snap = m.snapshot().await;
    assert_eq!(snap.history.len(), 3);
    assert_eq!(snap.history[0].key, "deployment:d4");
    assert_eq!(snap.history[2].key, "deployment:d2");
}

/// DT-ALR-04：active_keys 列出所有活跃告警 key。
#[tokio::test]
async fn active_keys_lists_keys() {
    let m = AlertManager::new();
    m.raise_alert(AlertKind::DeploymentAutoDisabled, "deployment:d1", "m", "x");
    m.raise_alert(AlertKind::OtelExporterOffline, "otel:exporter", "ep", "x");
    let mut keys = m.active_keys();
    keys.sort();
    assert_eq!(
        keys,
        vec!["deployment:d1".to_string(), "otel:exporter".to_string()]
    );
}

/// DT-ALR-05：notifier 只在状态迁移时触发（raise 1 次 + clear 1 次，重复/空操作不触发）。
#[tokio::test]
async fn notifier_fires_on_transitions_only() {
    let m = AlertManager::new();
    let n = Arc::new(CountingNotifier {
        raises: AtomicUsize::new(0),
        clears: AtomicUsize::new(0),
    });
    m.set_notifier(Some(n.clone()));

    m.raise_alert(AlertKind::DeploymentAutoDisabled, "deployment:d1", "m", "x");
    m.raise_alert(AlertKind::DeploymentAutoDisabled, "deployment:d1", "m", "x"); // idempotent
    m.clear_alert("deployment:d1");
    m.clear_alert("deployment:d1"); // no-op

    assert_eq!(n.raises.load(Ordering::SeqCst), 1);
    assert_eq!(n.clears.load(Ordering::SeqCst), 1);
}

/// DT-ALR-06：set_notifier(None) 后迁移不再触发回调。
#[tokio::test]
async fn notifier_can_be_cleared() {
    let m = AlertManager::new();
    let n = Arc::new(CountingNotifier {
        raises: AtomicUsize::new(0),
        clears: AtomicUsize::new(0),
    });
    m.set_notifier(Some(n.clone()));
    m.raise_alert(AlertKind::OtelExporterOffline, "k1", "t", "x");
    assert_eq!(n.raises.load(Ordering::SeqCst), 1);
    m.set_notifier(None);
    m.clear_alert("k1");
    assert_eq!(n.clears.load(Ordering::SeqCst), 0);
}

/// DT-ALR-07：Default 等价于 new（默认 history cap 500）。
#[tokio::test]
async fn default_equals_new() {
    let m = AlertManager::default();
    for i in 0..10 {
        let k = format!("k{i}");
        m.raise_alert(AlertKind::DeploymentAutoDisabled, k.clone(), "t", "x");
        m.clear_alert(&k);
    }
    let snap = m.snapshot().await;
    assert_eq!(snap.history.len(), 10);
    assert!(snap.healthy);
}

/// DT-ALR-08：snapshot 的 active 按 raised_at 倒序排列。
#[tokio::test]
async fn snapshot_sorts_active_by_raised_at_desc() {
    let m = AlertManager::new();
    m.raise_alert(AlertKind::DeploymentAutoDisabled, "k1", "t", "x");
    // 稍后再 raise 另一个，确保 raised_at 不同
    m.raise_alert(AlertKind::OtelExporterOffline, "k2", "t", "x");
    let snap = m.snapshot().await;
    assert_eq!(snap.active.len(), 2);
    assert!(snap.active[0].raised_at >= snap.active[1].raised_at);
}

/// DT-ALR-09：未注册 notifier 时 raise/clear 不 panic。
#[tokio::test]
async fn no_notifier_does_not_panic() {
    let m = AlertManager::new();
    m.raise_alert(AlertKind::DeploymentAutoDisabled, "k1", "t", "x");
    m.clear_alert("k1");
    let snap = m.snapshot().await;
    assert!(snap.healthy);
}
