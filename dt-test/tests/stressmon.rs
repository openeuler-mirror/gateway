//! DT 用例 — boom-stressmon：实时压力指标 ring buffer。
//! 覆盖 StressmonCollector 的 new / record_sample / snapshot / timeseries
//! 全部 pub API 路径：时序顺序、window 钳制、wrap 覆盖、CPU 阈值计数。

use boom_core::{StressmonApi, StressmonSample};
use boom_stressmon::StressmonCollector;

fn sample(ts: i64, cpu_pct: f32) -> StressmonSample {
    StressmonSample {
        ts,
        cpu_pct,
        rss_bytes: 0,
        worker_queue_depth: 0,
        blocking_tasks_queued: 0,
        inflight: 0,
    }
}

/// DT-STR-01：空 collector 快照为空。
#[tokio::test]
async fn empty_snapshot_is_empty() {
    let c = StressmonCollector::new(4);
    assert!(c.snapshot(60).is_empty());
    // trait 路径也走一遍
    let snap = c.timeseries(60).await;
    assert!(snap.samples.is_empty());
    assert_eq!(snap.num_workers, 4);
    assert_eq!(snap.cpu_over_80_count, 0);
}

/// DT-STR-02：记录后快照按时间正序返回最近 N 条。
#[tokio::test]
async fn snapshot_returns_recent_in_chronological_order() {
    let c = StressmonCollector::new(4);
    for t in 1..=10 {
        c.record_sample(sample(t, 0.0));
    }
    let out = c.snapshot(3);
    assert_eq!(out.len(), 3);
    assert_eq!([out[0].ts, out[1].ts, out[2].ts], [8, 9, 10]);
}

/// DT-STR-03：window 大于实际样本数时返回全部可用样本。
#[tokio::test]
async fn window_clamps_to_available() {
    let c = StressmonCollector::new(4);
    for t in 1..=5 {
        c.record_sample(sample(t, 0.0));
    }
    let out = c.snapshot(100);
    assert_eq!(out.len(), 5);
    assert_eq!(out[0].ts, 1);
    assert_eq!(out[4].ts, 5);
}

/// DT-STR-04：window 超过 CAPACITY(3600) 时钳制到容量上限。
#[tokio::test]
async fn window_clamps_to_capacity() {
    let c = StressmonCollector::new(4);
    for t in 1..=10 {
        c.record_sample(sample(t, 0.0));
    }
    // 请求 10000s — 钳制到 3600，但只有 10 条样本
    let out = c.snapshot(10000);
    assert_eq!(out.len(), 10);
    assert_eq!(out[0].ts, 1);
}

/// DT-STR-05：填满容量并溢出 1 条，最旧样本被覆盖，时序连续。
#[tokio::test]
async fn wraps_and_overwrites_oldest() {
    let c = StressmonCollector::new(4);
    // CAPACITY=3600，填到 CAPACITY+1 触发 wrap
    for t in 1..=3601 {
        c.record_sample(sample(t, 0.0));
    }
    let out = c.snapshot(3600);
    assert_eq!(out.len(), 3600);
    // ts=1 已被覆盖，从 ts=2 开始
    assert_eq!(out[0].ts, 2);
    assert_eq!(out[3599].ts, 3601);
}

/// DT-STR-06：CPU 超过 worker 池 80% 阈值时累计计数。
#[tokio::test]
async fn cpu_over_threshold_increments_counter() {
    let c = StressmonCollector::new(4); // 阈值 = 4 * 80 = 320
    c.record_sample(sample(1, 400.0)); // 超
    c.record_sample(sample(2, 320.0)); // 等于（不超，严格 >）
    c.record_sample(sample(3, 321.0)); // 超
    let snap = c.timeseries(60).await;
    assert_eq!(snap.cpu_over_80_count, 2);
}

/// DT-STR-07：CPU 全低于阈值时计数为 0。
#[tokio::test]
async fn cpu_under_threshold_keeps_zero_counter() {
    let c = StressmonCollector::new(8); // 阈值 = 640
    for t in 1..=5 {
        c.record_sample(sample(t, 100.0 * t as f32));
    }
    let snap = c.timeseries(60).await;
    assert_eq!(snap.cpu_over_80_count, 0);
    assert_eq!(snap.samples.len(), 5);
}

/// DT-STR-08：window_secs 为 0/负数时钳制为最小 1。
#[tokio::test]
async fn zero_or_negative_window_clamps_to_one() {
    let c = StressmonCollector::new(4);
    for t in 1..=3 {
        c.record_sample(sample(t, 0.0));
    }
    assert_eq!(c.snapshot(0).len(), 1);
    assert_eq!(c.snapshot(-5).len(), 1);
}

/// DT-STR-09：timeseries trait 路径返回完整快照（num_workers + 计数 + 样本）。
#[tokio::test]
async fn timeseries_returns_full_snapshot() {
    let c = StressmonCollector::new(16);
    c.record_sample(sample(1, 2000.0)); // 超 16*80=1280
    let snap = c.timeseries(60).await;
    assert_eq!(snap.num_workers, 16);
    assert_eq!(snap.cpu_over_80_count, 1);
    assert_eq!(snap.samples.len(), 1);
    assert_eq!(snap.samples[0].ts, 1);
}
