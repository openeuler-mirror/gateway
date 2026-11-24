//! DT 用例 — boom-stressmon：1Hz 采样聚合为 10s 峰值点 × 24h ring。
//! 覆盖 StressmonCollector 的 new / observe / snapshot / timeseries
//! 全部 pub API 路径（60952ced 重构后的模型）：桶内取 max、桶边界墙钟
//! 对齐、桶只有在下一桶首样本到达时才闭合可见、window 钳制、wrap 覆盖、
//! uptime 透传、原始 cpu_pct 保留（阈值由前端用 num_workers × 80 对
//! 峰值点推导）。

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

/// 推一个完整的 10s 桶（桶内多条 1Hz 样本），再推下一桶首样本把它闭合。
/// 返回被闭合桶的峰值点。
fn close_bucket(c: &StressmonCollector, bucket_start: i64, cpus: &[f32]) {
    for (i, cpu) in cpus.iter().enumerate() {
        c.observe(sample(bucket_start + i as i64, *cpu));
    }
}

/// DT-STR-01：空 collector 快照为空。
#[tokio::test]
async fn empty_snapshot_is_empty() {
    let c = StressmonCollector::new(4, chrono::Utc::now(), None);
    assert!(c.snapshot(60).is_empty());
    // 单桶未闭合（无下一桶样本）也不可见
    c.observe(sample(5, 1.0));
    assert!(c.snapshot(60).is_empty());
    // trait 路径也走一遍
    let snap = c.timeseries(60).await;
    assert!(snap.samples.is_empty());
    assert_eq!(snap.num_workers, 4);
}

/// DT-STR-02：桶内多条 1Hz 样本折叠为峰值点，ts 墙钟对齐桶起点；
/// 桶只有在下一桶首样本到达时才闭合进入 ring。
#[tokio::test]
async fn bucket_folds_to_max_and_closes_on_next_bucket() {
    let c = StressmonCollector::new(4, chrono::Utc::now(), None);
    close_bucket(&c, 100, &[10.0, 40.0, 20.0]); // 桶 [100,110) 峰值 40
    let closed = c.observe(sample(110, 1.0)); // 闭合桶 100
    assert_eq!(closed.unwrap().ts, 100);
    let out = c.snapshot(60);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].ts, 100);
    assert_eq!(out[0].cpu_pct, 40.0);
}

/// DT-STR-03：多桶按时间正序返回，window 换算为桶数后取最近 N 桶。
#[tokio::test]
async fn snapshot_returns_recent_buckets_in_order() {
    let c = StressmonCollector::new(4, chrono::Utc::now(), None);
    for b in 1..=11 {
        close_bucket(&c, b * 10, &[b as f32]);
    }
    c.observe(sample(120, 0.0)); // 闭合桶 110（最后一桶）
    // window 35s → 4 桶（向上取整），取最近 4 个闭合桶
    let out = c.snapshot(35);
    assert_eq!(out.len(), 4);
    assert_eq!(
        [out[0].ts, out[1].ts, out[2].ts, out[3].ts],
        [80, 90, 100, 110]
    );
}

/// DT-STR-04：window 大于实际样本数时返回全部可用桶。
#[tokio::test]
async fn window_clamps_to_available() {
    let c = StressmonCollector::new(4, chrono::Utc::now(), None);
    for b in 1..=5 {
        close_bucket(&c, b * 10, &[0.0]);
    }
    c.observe(sample(60, 0.0)); // 闭合桶 50
    let out = c.snapshot(10_000);
    assert_eq!(out.len(), 5);
    assert_eq!(out[0].ts, 10);
    assert_eq!(out[4].ts, 50);
}

/// DT-STR-05：填满 24h 容量（CAPACITY=8640 桶）并溢出 1 桶，最旧被覆盖。
#[tokio::test]
async fn wraps_and_overwrites_oldest() {
    let c = StressmonCollector::new(4, chrono::Utc::now(), None);
    for b in 1..=8641 {
        c.observe(sample(b * 10, 0.0));
    }
    c.observe(sample(8642 * 10, 0.0)); // 闭合最后一桶，触发 wrap
    let out = c.snapshot(24 * 60 * 60);
    assert_eq!(out.len(), 8640);
    // 桶 10 已被覆盖，从桶 20 开始
    assert_eq!(out[0].ts, 20);
    assert_eq!(out[8639].ts, 8641 * 10);
}

/// DT-STR-06：峰值点保留原始 cpu_pct，前端可按 num_workers × 80 推导超阈计数。
#[tokio::test]
async fn buckets_preserve_raw_cpu_for_threshold_derivation() {
    let c = StressmonCollector::new(4, chrono::Utc::now(), None); // 阈值 = 4 * 80 = 320
    close_bucket(&c, 100, &[400.0, 320.0]); // 桶峰值 400（超）
    close_bucket(&c, 110, &[320.0, 100.0]); // 桶峰值 320（等于，不超）
    close_bucket(&c, 120, &[321.0, 1.0]); // 桶峰值 321（超）
    c.observe(sample(130, 0.0)); // 闭合桶 120
    let snap = c.timeseries(60).await;
    // 前端式推导：严格大于阈值才计数
    let over = snap
        .samples
        .iter()
        .filter(|s| s.cpu_pct > snap.num_workers as f32 * 80.0)
        .count();
    assert_eq!(over, 2);
    // 桶峰值原样保留（含等于阈值的边界桶）
    let cpus: Vec<f32> = snap.samples.iter().map(|s| s.cpu_pct).collect();
    assert_eq!(cpus, vec![400.0, 320.0, 321.0]);
}

/// DT-STR-07：CPU 全低于阈值时前端推导计数为 0。
#[tokio::test]
async fn cpu_under_threshold_derives_zero() {
    let c = StressmonCollector::new(8, chrono::Utc::now(), None); // 阈值 = 640
    for b in 1..=5 {
        close_bucket(&c, b * 10, &[100.0 * b as f32]);
    }
    c.observe(sample(60, 0.0)); // 闭合桶 50
    let snap = c.timeseries(60).await;
    let over = snap
        .samples
        .iter()
        .filter(|s| s.cpu_pct > snap.num_workers as f32 * 80.0)
        .count();
    assert_eq!(over, 0);
    assert_eq!(snap.samples.len(), 5);
}

/// DT-STR-08：window_secs 为 0/负数时钳制为最小 1 桶。
#[tokio::test]
async fn zero_or_negative_window_clamps_to_one() {
    let c = StressmonCollector::new(4, chrono::Utc::now(), None);
    for b in 1..=3 {
        close_bucket(&c, b * 10, &[0.0]);
    }
    c.observe(sample(40, 0.0)); // 闭合桶 30
    assert_eq!(c.snapshot(0).len(), 1);
    assert_eq!(c.snapshot(-5).len(), 1);
}

/// DT-STR-09：timeseries trait 路径返回完整快照（num_workers + uptime + 样本）。
#[tokio::test]
async fn timeseries_returns_full_snapshot() {
    let started = chrono::Utc::now() - chrono::Duration::seconds(3600);
    let c = StressmonCollector::new(16, started, None);
    close_bucket(&c, 100, &[2000.0]); // 峰值超 16*80=1280
    c.observe(sample(110, 0.0)); // 闭合
    let snap = c.timeseries(60).await;
    assert_eq!(snap.num_workers, 16);
    // uptime 与 /health 同源（started_at 传入），1 小时前启动 → ≥3600s
    assert!(snap.uptime_secs >= 3600);
    assert_eq!(snap.samples.len(), 1);
    assert_eq!(snap.samples[0].ts, 100);
    assert_eq!(snap.samples[0].cpu_pct, 2000.0);
}

/// DT-STR-10：时钟回拨到已开启桶之前的旧桶时样本被丢弃，不影响已聚合状态。
#[tokio::test]
async fn clock_step_back_drops_older_sample() {
    let c = StressmonCollector::new(4, chrono::Utc::now(), None);
    close_bucket(&c, 100, &[50.0]);
    c.observe(sample(110, 10.0)); // 闭合桶 100，开启桶 110
    c.observe(sample(105, 999.0)); // 回拨进更旧的桶 → 丢弃
    c.observe(sample(120, 20.0)); // 闭合桶 110（峰值仍 10）
    let out = c.snapshot(60);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].cpu_pct, 50.0);
    assert_eq!(out[1].cpu_pct, 10.0);
}
