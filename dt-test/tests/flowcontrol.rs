//! DT 用例 — boom-flowcontrol：per-deployment 流控槽位 + VIP/普通双队列 + RAII。
//!
//! 全部纯内存（tokio oneshot 驱动，无网络无 DB）。覆盖：
//! - FlowController ensure_slot/remove_slot/retain_slots（含 max=0 移除、reconfigure）
//! - acquire：NoSlot / ContextExceeded / 立即派发 / 超时 Timeout{waiters,waited} / 成功
//! - VIP 先于普通派发、periodic_dispatch
//! - get_stats / get_queued_waiters / get_dispatched_keys / get_key_request_status
//!   （含 Waiting{ahead} / Processing{parallel_count} 两态 + stale index 清理）
//! - DeploymentQueueInfo trait（total_load / max_capacity）
//! - FlowControlGuard RAII（Drop 释放槽位并重派发、wait_duration）
//! - FlowControlledStream（passthrough + 流结束 take guard）

use std::time::Duration;

use boom_core::DeploymentQueueInfo;
use boom_flowcontrol::{
    FlowControlConfig, FlowControlError, FlowController, FlowControlledStream,
    UserRequestStage,
};
use futures::{Stream, StreamExt};

fn cfg(max_inflight: u32, max_context: u64) -> FlowControlConfig {
    FlowControlConfig { max_inflight, max_context }
}

/// poll 一次 future 使其入队但不 await（用 noop waker）。
fn poll_once<F: std::future::Future>(fut: std::pin::Pin<&mut F>) {
    let w = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&w);
    let _ = fut.poll(&mut cx);
}

// ───────────────────────── ensure_slot / remove / retain ─────────────────────────

/// DT-FC-01：new/default 空控制器，get_stats 为空。
#[tokio::test]
async fn new_controller_has_no_slots() {
    let fc = FlowController::new();
    assert!(fc.get_stats().is_empty());
    let def: FlowController = FlowController::default();
    assert!(def.get_stats().is_empty());
}

/// DT-FC-02：ensure_slot 创建槽位，get_stats 反映配置。
#[tokio::test]
async fn ensure_slot_creates_slot() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(5, 1000));
    let stats = fc.get_stats();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].deployment_id, "dep1");
    assert_eq!(stats[0].max_inflight, 5);
    assert_eq!(stats[0].max_context, 1000);
    assert_eq!(stats[0].current_inflight, 0);
    assert_eq!(stats[0].waiters, 0);
}

/// DT-FC-03：ensure_slot 在 max_inflight=0 & max_context=0 时移除已存在槽位（pass-through）。
#[tokio::test]
async fn ensure_slot_zero_removes_existing() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(5, 1000));
    assert_eq!(fc.get_stats().len(), 1);
    fc.ensure_slot("dep1", &cfg(0, 0));
    assert!(fc.get_stats().is_empty());
}

/// DT-FC-04：ensure_slot 对已存在槽位更新配置（reconfigure max_inflight/max_context）。
#[tokio::test]
async fn ensure_slot_updates_existing_config() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(5, 1000));
    fc.ensure_slot("dep1", &cfg(10, 2000));
    let stats = fc.get_stats();
    assert_eq!(stats[0].max_inflight, 10);
    assert_eq!(stats[0].max_context, 2000);
}

/// DT-FC-05：remove_slot 显式移除槽位。
#[tokio::test]
async fn remove_slot_drops_slot() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(5, 0));
    fc.ensure_slot("dep2", &cfg(5, 0));
    fc.remove_slot("dep1");
    let stats = fc.get_stats();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].deployment_id, "dep2");
}

/// DT-FC-06：retain_slots 保留列表中的槽位，移除其余。
#[tokio::test]
async fn retain_slots_keeps_listed() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(5, 0));
    fc.ensure_slot("dep2", &cfg(5, 0));
    fc.ensure_slot("dep3", &cfg(5, 0));
    fc.retain_slots(&["dep1".to_string(), "dep3".to_string()]);
    let ids: Vec<_> = fc.get_stats().iter().map(|s| s.deployment_id.clone()).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"dep1".to_string()));
    assert!(ids.contains(&"dep3".to_string()));
}

// ───────────────────────── acquire ─────────────────────────

/// DT-FC-07：未配置槽位的 deployment 调 acquire 返回 NoSlot。
#[tokio::test]
async fn acquire_no_slot_returns_no_slot() {
    let fc = FlowController::new();
    let r = fc.acquire("ghost", 100, Duration::from_secs(1), false, None, None, None).await;
    assert!(matches!(r, Err(FlowControlError::NoSlot)));
}

/// DT-FC-08：请求 context 单独超过 max_context 返回 ContextExceeded（永不可派发）。
#[tokio::test]
async fn acquire_context_exceeded() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 500));
    let r = fc.acquire("dep1", 1000, Duration::from_secs(1), false, None, None, None).await;
    match r {
        Err(FlowControlError::ContextExceeded { deployment_id, context_chars, max_context }) => {
            assert_eq!(deployment_id, "dep1");
            assert_eq!(context_chars, 1000);
            assert_eq!(max_context, 500);
        }
        other => panic!("expected ContextExceeded, got: {:?}", other.err()),
    }
}

/// DT-FC-09：max_inflight>=1 时立即派发，返回 guard，wait_duration 极小。
#[tokio::test]
async fn acquire_immediate_dispatch() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(2, 0));
    let g = fc.acquire("dep1", 100, Duration::from_secs(5), false, None, None, None)
        .await
        .expect("immediate");
    // dispatched_at 紧随 enqueued_at（同一次 dispatch 内设值），间隔仅纳秒~微秒级
    assert!(g.wait_duration() < Duration::from_millis(50));
    // 派发后 current_inflight=1
    let stats = fc.get_stats();
    assert_eq!(stats[0].current_inflight, 1);
    assert_eq!(stats[0].current_context, 100);
}

/// DT-FC-10：max_inflight=1 时第二请求超时返回 Timeout{waiters, waited}，并从队列移除。
#[tokio::test]
async fn acquire_timeout_removes_from_queue() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));
    let _g1 = fc.acquire("dep1", 100, Duration::from_secs(60), false, None, None, None)
        .await
        .unwrap();

    let r = fc.acquire("dep1", 100, Duration::from_millis(50), false, None, None, None).await;
    match r {
        Err(FlowControlError::Timeout { deployment_id, waiters, waited }) => {
            assert_eq!(deployment_id, "dep1");
            assert!(waiters <= 1, "waiters should be <=1 after self-removal: {waiters}");
            assert!(waited > Duration::ZERO);
        }
        other => panic!("expected Timeout, got: {:?}", other.err()),
    }
    // 超时后请求已从队列移除
    let stats = fc.get_stats();
    assert_eq!(stats[0].waiters, 0);
}

/// DT-FC-11：guard Drop 释放槽位，后续 acquire 立即成功。
#[tokio::test]
async fn guard_drop_frees_slot() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));
    {
        let _g = fc.acquire("dep1", 100, Duration::from_secs(5), false, None, None, None).await.unwrap();
        assert_eq!(fc.get_stats()[0].current_inflight, 1);
    }
    // Drop 后槽位空，立即可再派发
    let _g2 = fc.acquire("dep1", 100, Duration::from_secs(5), false, None, None, None).await.unwrap();
    assert_eq!(fc.get_stats()[0].current_inflight, 1);
}

/// DT-FC-12：VIP 队列优先于普通队列派发（释放槽位 + periodic_dispatch 后 VIP 先派）。
#[tokio::test]
async fn vip_dispatched_before_normal() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));
    let g1 = fc.acquire("dep1", 100, Duration::from_secs(60), false, None, None, None).await.unwrap();

    // 同时入队一个普通 + 一个 VIP（用长 timeout 的 future，poll 入队但不 await 完成）
    let mut normal_fut = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), false, None, None, None));
    let mut vip_fut = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), true, None, None, None));
    poll_once(normal_fut.as_mut());
    poll_once(vip_fut.as_mut());

    let stats = fc.get_stats();
    assert_eq!(stats[0].waiters, 1);
    assert_eq!(stats[0].vip_waiters, 1);

    // 释放槽位 + 触发派发 → VIP 先派
    drop(g1);
    fc.periodic_dispatch();
    let stats = fc.get_stats();
    assert_eq!(stats[0].vip_waiters, 0, "VIP dispatched first");
    assert_eq!(stats[0].waiters, 1, "normal still waiting");

    // 唤醒 VIP future 取走其 guard 并释放
    poll_once(vip_fut.as_mut());
    drop(normal_fut);
    drop(vip_fut);
}

/// DT-FC-13：periodic_dispatch 在多 deployment 上触发派发。
#[tokio::test]
async fn periodic_dispatch_across_deployments() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));
    fc.ensure_slot("dep2", &cfg(1, 0));
    // 无等待者时 periodic_dispatch 是 no-op，不 panic
    fc.periodic_dispatch();
    let _g1 = fc.acquire("dep1", 0, Duration::from_secs(5), false, None, None, None).await.unwrap();
    let _g2 = fc.acquire("dep2", 0, Duration::from_secs(5), false, None, None, None).await.unwrap();
    fc.periodic_dispatch();
}

// ───────────────────────── stats / queued / dispatched / key status ─────────────────────────

/// DT-FC-14：get_queued_waiters 列出等待者 key_alias/is_vip。
#[tokio::test]
async fn get_queued_waiters_lists_entries() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));
    let _g1 = fc.acquire("dep1", 100, Duration::from_secs(60), false, Some("alice".into()), None, None).await.unwrap();

    let mut normal_fut = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), false, Some("bob".into()), None, None));
    let mut vip_fut = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), true, Some("vip".into()), None, None));
    poll_once(normal_fut.as_mut());
    poll_once(vip_fut.as_mut());

    let q = fc.get_queued_waiters();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].deployment_id, "dep1");
    // 1 vip + 1 normal 等待
    assert_eq!(q[0].waiters.len(), 2);
    assert!(q[0].waiters.iter().any(|w| w.is_vip && w.key_alias.as_deref() == Some("vip")));
    assert!(q[0].waiters.iter().any(|w| !w.is_vip && w.key_alias.as_deref() == Some("bob")));

    drop(normal_fut);
    drop(vip_fut);
}

/// DT-FC-15：get_dispatched_keys 列出在飞请求的 key_alias/is_vip。
#[tokio::test]
async fn get_dispatched_keys_lists_inflight() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(2, 0));
    let _g1 = fc.acquire("dep1", 100, Duration::from_secs(60), false, Some("alice".into()), None, None).await.unwrap();
    let _g2 = fc.acquire("dep1", 100, Duration::from_secs(60), true, Some("vip".into()), None, None).await.unwrap();

    let d = fc.get_dispatched_keys();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].deployment_id, "dep1");
    assert_eq!(d[0].keys.len(), 2);
    assert!(d[0].keys.iter().any(|k| k.is_vip && k.key_alias == "vip"));
    assert!(d[0].keys.iter().any(|k| !k.is_vip && k.key_alias == "alice"));
}

/// DT-FC-16：get_key_request_status 返回 Waiting{ahead}（排队）与 Processing（在飞）；
/// `ahead` 仅计非派发（在飞不算 ahead）；请求结束后 reverse-index 自动清理。
#[tokio::test]
async fn get_key_request_status_waiting_and_processing() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));

    // 在飞请求：带 key_hash kh1（占满 max_inflight=1）
    let _g1 = fc.acquire("dep1", 100, Duration::from_secs(60), false, Some("a".into()), Some("kh1".into()), Some("m1".into())).await.unwrap();

    // 两个排队请求（容量满 → 均等待）。w1 在 w2 之前
    let mut w1 = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), false, Some("b".into()), Some("kh2".into()), Some("m2".into())));
    let mut w2 = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), false, Some("c".into()), Some("kh3".into()), Some("m3".into())));
    poll_once(w1.as_mut());
    poll_once(w2.as_mut());

    // kh1 → Processing{parallel_count}
    let s1 = fc.get_key_request_status("kh1");
    assert_eq!(s1.len(), 1);
    assert_eq!(s1[0].model, "m1");
    assert!(matches!(s1[0].status, UserRequestStage::Processing { .. }));

    // kh2(w1) → Waiting{ahead:0}：g1 已在飞（dispatched），不计入 ahead
    let s2 = fc.get_key_request_status("kh2");
    assert_eq!(s2.len(), 1);
    assert!(matches!(s2[0].status, UserRequestStage::Waiting { ahead: 0 }));

    // kh3(w2) → Waiting{ahead:1}：前面有 w1 在等待
    let s3 = fc.get_key_request_status("kh3");
    assert_eq!(s3.len(), 1);
    assert!(matches!(s3[0].status, UserRequestStage::Waiting { ahead: 1 }));

    // 未知 key_hash → 空
    assert!(fc.get_key_request_status("unknown").is_empty());

    // 释放 in-flight + drop 等待 future → AcquireCleanup 移除队列项 + stale 清理
    drop(_g1);
    drop(w1);
    drop(w2);
    // kh1 请求已离开，下次查询触发 stale 清理
    let _ = fc.get_key_request_status("kh1");
    assert!(fc.get_key_request_status("kh1").is_empty());
}

/// DT-FC-17：DeploymentQueueInfo trait：total_load（队列含在飞+等待）、max_capacity。
/// 用 max_inflight=1 占满容量，poll_once 才会真正排队（否则有空闲会被立即派发）。
#[tokio::test]
async fn deployment_queue_info_total_load_and_capacity() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));
    // 无槽位 → 0
    assert_eq!(fc.total_load("ghost"), 0);
    assert_eq!(fc.max_capacity("ghost"), 0);

    let _g1 = fc.acquire("dep1", 100, Duration::from_secs(60), false, None, None, None).await.unwrap();
    // g1 在飞（dispatched=true 但仍在队列，guard 未 drop）
    assert_eq!(fc.total_load("dep1"), 1);
    assert_eq!(fc.max_capacity("dep1"), 1);

    // 容量满 → poll_once 入队但不派发，队列多一项（仍在队列）
    let mut w = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), false, None, None, None));
    poll_once(w.as_mut());
    assert_eq!(fc.total_load("dep1"), 2); // g1(在飞) + w(等待) 均在队列
    drop(w);
    // w 的 AcquireCleanup::drop 移除其队列项 → total_load 回落
    assert_eq!(fc.total_load("dep1"), 1);
}

/// DT-FC-18：FlowControlGuard.wait_duration——成功派发返回非零/零；超时 race None→ZERO。
#[tokio::test]
async fn guard_wait_duration() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));
    let g = fc.acquire("dep1", 100, Duration::from_secs(5), false, None, None, None).await.unwrap();
    // 立即派发：dispatched_at 紧随 enqueued_at，wait_duration 应为 0 或极小
    assert!(g.wait_duration() < Duration::from_millis(100));
}

// ───────────────────────── FlowControlledStream ─────────────────────────

/// 简单 vec-backed stream（无需额外依赖即可测 FlowControlledStream）。
struct VecStream {
    items: Vec<&'static str>,
    idx: usize,
}
impl Stream for VecStream {
    type Item = &'static str;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.idx < self.items.len() {
            let v = self.items[self.idx];
            self.idx += 1;
            std::task::Poll::Ready(Some(v))
        } else {
            std::task::Poll::Ready(None)
        }
    }
}

/// DT-FC-19：FlowControlledStream::passthrough 无 guard；流结束后 guard 释放槽位。
#[tokio::test]
async fn flow_controlled_stream_passthrough_and_release() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));
    let g = fc.acquire("dep1", 100, Duration::from_secs(5), false, None, None, None).await.unwrap();
    assert_eq!(fc.total_load("dep1"), 1);

    let mut s = FlowControlledStream::new(VecStream { items: vec!["a", "b"], idx: 0 }, g);

    // 拉取两个 item
    assert_eq!(s.next().await, Some("a"));
    assert_eq!(fc.total_load("dep1"), 1); // 还在飞
    assert_eq!(s.next().await, Some("b"));
    assert_eq!(fc.total_load("dep1"), 1); // 仍在飞（未到 None）
    // 流结束（None）→ guard.take() → Drop 释放槽位
    assert_eq!(s.next().await, None);
    assert_eq!(fc.total_load("dep1"), 0, "stream end releases guard");
}

/// DT-FC-20：FlowControlledStream::passthrough（无 guard）正常转发 item 且 None 不 panic。
#[tokio::test]
async fn flow_controlled_stream_passthrough_no_guard() {
    let mut s = FlowControlledStream::passthrough(VecStream { items: vec!["x"], idx: 0 });
    assert_eq!(s.next().await, Some("x"));
    assert_eq!(s.next().await, None);
}

/// DT-FC-21：FlowControlConfig::default() 全零（pass-through 语义），
/// 用 default 配置 ensure_slot 等价于移除槽位。
#[tokio::test]
async fn flowcontrol_config_default_is_passthrough() {
    let d = FlowControlConfig::default();
    assert_eq!(d.max_inflight, 0);
    assert_eq!(d.max_context, 0);

    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(5, 1000));
    fc.ensure_slot("dep1", &d);
    assert!(fc.get_stats().is_empty(), "default config removes the slot");
}

/// DT-FC-22：等待中的请求在槽位被移除后被唤醒为 NoSlot
/// （oneshot sender 随槽位一起 drop → grant_rx 收到 Err）。
#[tokio::test]
async fn parked_waiter_wakes_no_slot_after_remove() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));
    let g1 = fc.acquire("dep1", 100, Duration::from_secs(60), false, None, None, None).await.unwrap();

    let mut w = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), false, None, None, None));
    poll_once(w.as_mut());
    assert_eq!(fc.get_stats()[0].waiters, 1);

    // 槽位移除 → 队列（含 grant sender）整体 drop → 等待者被唤醒为 NoSlot
    fc.remove_slot("dep1");
    let res = futures::FutureExt::now_or_never(w.as_mut());
    assert!(
        matches!(res, Some(Err(FlowControlError::NoSlot))),
        "expected Err(NoSlot) after slot removal"
    );
    drop(g1);
}

/// DT-FC-23：上下文预算跳过派发（inflight 有余量但 used_ctx+req 超 max_context
/// → dispatch 循环 continue 跳过）；槽位释放后重派发成功。
/// 同测 remove_slot 后的 stale reverse-index 清理与 AcquireCleanup 的槽位缺失分支。
#[tokio::test]
async fn context_skip_dispatch_and_stale_index_cleanup() {
    // Part A：context skip
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(2, 100));
    let g1 = fc.acquire("dep1", 60, Duration::from_secs(60), false, None, None, None).await.unwrap();
    assert_eq!(fc.get_stats()[0].current_inflight, 1);

    // inflight 1/2 有余量，但 60+50 > 100 → 被上下文预算挡住，保持等待
    let mut w = Box::pin(fc.acquire("dep1", 50, Duration::from_secs(60), false, None, None, None));
    poll_once(w.as_mut());
    let stats = fc.get_stats();
    assert_eq!(stats[0].waiters, 1, "context budget blocks dispatch despite free inflight slot");
    assert_eq!(stats[0].current_context, 60);

    // 释放 g1 → used_ctx 归零 → w 可派发
    drop(g1);
    let guard = futures::FutureExt::now_or_never(w.as_mut());
    assert!(guard.is_some(), "waiter dispatched after context freed");
    drop(w);

    // Part B：remove_slot 后 reverse-index 残留 → 查询触发 stale 清理；
    // 之后 drop 等待 future 走 AcquireCleanup 的槽位缺失分支。
    fc.ensure_slot("dep2", &cfg(1, 100));
    let g2 = fc.acquire("dep2", 60, Duration::from_secs(60), false, None, None, None).await.unwrap();
    let mut w2 = Box::pin(
        fc.acquire("dep2", 50, Duration::from_secs(60), false, Some("u".into()), Some("kh9".into()), None),
    );
    poll_once(w2.as_mut());

    fc.remove_slot("dep2");
    // kh9 的 index 项指向已删除的槽位 → stale 清理，返回空
    assert!(fc.get_key_request_status("kh9").is_empty());
    assert!(fc.get_key_request_status("kh9").is_empty(), "index entry removed after cleanup");
    // drop 等待 future → AcquireCleanup::drop 发现槽位不存在 → 直接返回（不 panic）
    drop(w2);
    drop(g2);
}

/// DT-FC-24：position_for VIP 分支 —— 排队 VIP 的 ahead 只统计其前面的
/// 未派发 VIP；普通请求的 ahead = 全部未派发 VIP + 前面未派发普通。
#[tokio::test]
async fn vip_position_counts_only_vip_ahead() {
    let fc = FlowController::new();
    fc.ensure_slot("dep1", &cfg(1, 0));
    let g1 = fc.acquire("dep1", 100, Duration::from_secs(60), false, None, None, None).await.unwrap();

    let mut vip1 = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), true, Some("v1".into()), Some("kv1".into()), Some("m".into())));
    let mut vip2 = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), true, Some("v2".into()), Some("kv2".into()), None));
    let mut n = Box::pin(fc.acquire("dep1", 100, Duration::from_secs(60), false, Some("n".into()), Some("kn".into()), None));
    poll_once(vip1.as_mut());
    poll_once(vip2.as_mut());
    poll_once(n.as_mut());

    // kv2（第 2 个 VIP）：前面只有 vip1 未派发 → ahead=1
    let s2 = fc.get_key_request_status("kv2");
    assert_eq!(s2.len(), 1);
    assert!(s2[0].is_vip);
    assert!(matches!(s2[0].status, UserRequestStage::Waiting { ahead: 1 }));

    // kn（普通）：2 个未派发 VIP 全部在前 → ahead=2
    let sn = fc.get_key_request_status("kn");
    assert_eq!(sn.len(), 1);
    assert!(!sn[0].is_vip);
    assert!(matches!(sn[0].status, UserRequestStage::Waiting { ahead: 2 }));

    drop(vip1);
    drop(vip2);
    drop(n);
    drop(g1);
}
