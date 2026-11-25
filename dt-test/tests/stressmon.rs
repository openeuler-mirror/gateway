//! Public integration coverage for the stress collector's closed 10-second buckets.

use boom_core::{StressmonApi, StressmonSample};
use boom_stressmon::{StressmonCollector, PERIOD_SECS};

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

#[tokio::test]
async fn empty_collector_has_no_snapshot_or_database_history() {
    let collector = StressmonCollector::new(4, chrono::Utc::now(), None);
    assert!(collector.snapshot(60).is_empty());
    let snapshot = collector.timeseries(60).await;
    assert!(snapshot.samples.is_empty());
    assert_eq!(snapshot.num_workers, 4);
    assert!(collector.history(0).await.is_empty());
}

#[test]
fn only_closed_buckets_are_visible_in_chronological_order() {
    let collector = StressmonCollector::new(4, chrono::Utc::now(), None);
    for index in 1..=11 {
        collector.observe(sample(index * PERIOD_SECS, 0.0));
    }

    let newest = collector.snapshot(3 * PERIOD_SECS);
    let timestamps: Vec<_> = newest.iter().map(|point| point.ts).collect();
    assert_eq!(
        timestamps,
        [8 * PERIOD_SECS, 9 * PERIOD_SECS, 10 * PERIOD_SECS]
    );
}

#[test]
fn snapshot_rounds_up_and_clamps_to_available_buckets() {
    let collector = StressmonCollector::new(4, chrono::Utc::now(), None);
    for index in 1..=6 {
        collector.observe(sample(index * PERIOD_SECS, 0.0));
    }

    assert_eq!(collector.snapshot(PERIOD_SECS + 1).len(), 2);
    let all = collector.snapshot(100_000);
    assert_eq!(all.len(), 5);
    assert_eq!(all[0].ts, PERIOD_SECS);
    assert_eq!(all[4].ts, 5 * PERIOD_SECS);
}

#[test]
fn full_ring_keeps_the_newest_day_of_closed_buckets() {
    let collector = StressmonCollector::new(4, chrono::Utc::now(), None);
    let capacity = 24 * 60 * 60 / PERIOD_SECS;
    for index in 1..=capacity + 2 {
        collector.observe(sample(index * PERIOD_SECS, 0.0));
    }

    let all = collector.snapshot(2 * 24 * 60 * 60);
    assert_eq!(all.len(), capacity as usize);
    assert_eq!(all.first().unwrap().ts, 2 * PERIOD_SECS);
    assert_eq!(all.last().unwrap().ts, (capacity + 1) * PERIOD_SECS);
}

#[tokio::test]
async fn bucket_peaks_preserve_cpu_for_threshold_derivation() {
    let collector = StressmonCollector::new(4, chrono::Utc::now(), None);
    for (index, peak) in [400.0, 320.0, 321.0].into_iter().enumerate() {
        let start = (index as i64 + 1) * PERIOD_SECS;
        collector.observe(sample(start, 20.0));
        collector.observe(sample(start + 1, peak));
        collector.observe(sample(start + 2, 30.0));
    }
    assert_eq!(
        collector
            .observe(sample(4 * PERIOD_SECS, 0.0))
            .unwrap()
            .cpu_pct,
        321.0
    );

    let snapshot = collector.timeseries(60).await;
    let cpu: Vec<_> = snapshot.samples.iter().map(|point| point.cpu_pct).collect();
    assert_eq!(cpu, [400.0, 320.0, 321.0]);
    assert_eq!(
        cpu.iter()
            .filter(|&&peak| peak > snapshot.num_workers as f32 * 80.0)
            .count(),
        2
    );
}

#[tokio::test]
async fn cpu_under_threshold_derives_zero_overloads() {
    let collector = StressmonCollector::new(8, chrono::Utc::now(), None);
    for index in 1..=6 {
        collector.observe(sample(index * PERIOD_SECS, 100.0 * index as f32));
    }
    let snapshot = collector.timeseries(60).await;
    assert_eq!(snapshot.samples.len(), 5);
    assert!(snapshot
        .samples
        .iter()
        .all(|point| point.cpu_pct <= snapshot.num_workers as f32 * 80.0));
}

#[test]
fn nonpositive_window_returns_the_latest_closed_bucket() {
    let collector = StressmonCollector::new(4, chrono::Utc::now(), None);
    for index in 1..=3 {
        collector.observe(sample(index * PERIOD_SECS, 0.0));
    }
    for window in [0, -5] {
        let snapshot = collector.snapshot(window);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].ts, 2 * PERIOD_SECS);
    }
}

#[tokio::test]
async fn timeseries_returns_process_uptime_worker_count_and_points() {
    let started_at = chrono::Utc::now() - chrono::Duration::hours(1);
    let collector = StressmonCollector::new(16, started_at, None);
    collector.observe(sample(PERIOD_SECS + 1, 2000.0));
    collector.observe(sample(2 * PERIOD_SECS, 0.0));

    let snapshot = collector.timeseries(60).await;
    assert_eq!(snapshot.num_workers, 16);
    assert!(snapshot.uptime_secs >= 3600);
    assert_eq!(snapshot.samples.len(), 1);
    assert_eq!(snapshot.samples[0].ts, PERIOD_SECS);
    assert_eq!(snapshot.samples[0].cpu_pct, 2000.0);
}
