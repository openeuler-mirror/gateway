//! DT 用例 — boom-limiter：滑动窗口限流 + 并发守卫 + PlanStore（非 DB 部分）。
//!
//! 覆盖（crate 内 #[cfg(test)] 在依赖编译下不参与，此处通过 pub API 驱动）：
//! - SlidingWindowLimiter 三段式契约：peek_only（counts 权重感知 /
//!   tokens、costs 历史累计判定 + rejected_kind）、commit_counts、
//!   settle_usage（窗口三维度 + 6 个累计计数器，key/team 两种 scope）
//! - 窗口过期：过期后 peek 归零、commit/settle 复位重建、
//!   snapshot/scan 跳过过期项、cleanup_expired
//! - 累计计数器：peek/reset（返回快照）/ 负值钳 0 / 缺席 0
//! - 仪表盘查询：get_usage(_for_key)/get_all_key_units（跳过 __team__）/
//!   peek_key_windows/peek_team_windows（model 含 ':'、畸形 cache_key 跳过）
//! - 清理：clear_windows / clear_for_key / clear_all_windows
//! - snapshot / restore_counter / restore_cumulative（过期跳过）
//! - decimal_to_micros / micros_to_decimal
//! - RateLimitPlan：effective_limits（rpm/tpm 简写合并到 60s 条目、
//!   schedule 激活/未激活、stale 窗口计算）、validate_schedule_overlap
//!   （相接不重叠 / 重叠 / 跨午夜 / 全天槽 / 畸形 hours 忽略）
//! - PlanStore：plan CRUD、默认 plan、key/team 分配三态解析、类型校验、
//!   快照/恢复/清理
//! - ConcurrencyGuard RAII、try_acquire(_team) 超限拒绝、cleanup_concurrency
//! - GuardedStream：流结束自动释放守卫
//! - migrations：8 个 DDL 函数返回预期表名
//!
//! 跳过（需要 Postgres）：*_db / restore_counters_from_db / sync_counters_to_db /
//! recompute_team_cumulative / row_to_plan 等全部 DB 路径。

use std::str::FromStr;
use std::time::Duration;

use boom_core::types::{LimitDimension, RateLimitKey, WindowLimit};
use boom_limiter::concurrency::GuardedStream;
use boom_limiter::{
    assignment_alter_ddl, assignment_ddl, cumulative_ddl, decimal_to_micros, micros_to_decimal,
    plan_alter_ddl, plan_ddl, rate_limit_state_ddl, state_alter_ddl, team_assignment_ddl,
    CumulativeKind, CumulativeSnapshot, PlanStore, PlanType, QuotaScope, RateLimitPlan,
    ScheduleSlot, SlidingWindowLimiter, WindowKind,
};
use futures::StreamExt;
use rust_decimal::Decimal;

// ───────────────────────── helpers ─────────────────────────

fn rkey(kh: &str, model: &str) -> RateLimitKey {
    RateLimitKey { key_hash: kh.to_string(), model: model.to_string() }
}

fn wl(counts: Option<u64>, tokens: Option<u64>, costs: Option<Decimal>, secs: u64) -> WindowLimit {
    WindowLimit { counts, tokens, costs, window_secs: secs }
}

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn kscope(kh: &str) -> QuotaScope {
    QuotaScope::Key { key_hash: kh.to_string() }
}

fn tscope(tid: &str) -> QuotaScope {
    QuotaScope::Team { team_id: tid.to_string() }
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn plan(name: &str, r#type: PlanType) -> RateLimitPlan {
    RateLimitPlan {
        name: name.to_string(),
        r#type,
        member_plan: None,
        concurrency_limit: None,
        rpm_limit: None,
        tpm_limit: None,
        window_limits: vec![],
        total_token_limit: None,
        total_cost_limit: None,
        schedule: vec![],
    }
}

// ═════════════════════════════════════════════════════════════════
// SlidingWindowLimiter — 三段式：peek → commit_counts → settle_usage
// ═════════════════════════════════════════════════════════════════

/// DT-LM-01：counts 维度 —— 权重感知判定 + 拒绝时报告 Counts/retry_after。
#[tokio::test]
async fn peek_and_commit_counts_dimension() {
    let l = SlidingWindowLimiter::new();
    let key = rkey("kh1", "gpt-4");
    let wins = [wl(Some(2), None, None, 60)];

    // 空 key：0 + weight(2) <= limit(2) → 放行
    let d = l.peek_only(&key, &wins, 2).await;
    assert!(d.allowed);
    assert_eq!(d.remaining, 0);
    assert_eq!(d.retry_after_secs, None);
    assert_eq!(d.rejected_kind, None);

    let d = l.commit_counts(&key, &wins, 2);
    assert!(d.allowed);
    assert_eq!(d.remaining, 0);

    // 已计 2：2 + 1 > 2 → 拒绝，报告 Counts 维度
    let d = l.peek_only(&key, &wins, 1).await;
    assert!(!d.allowed);
    assert_eq!(d.limit, 2);
    assert_eq!(d.rejected_window_secs, Some(60));
    assert!(matches!(d.rejected_kind, Some(LimitDimension::Counts)));
    let retry = d.retry_after_secs.unwrap();
    assert!((58..=60).contains(&retry), "retry_after {retry}");
}

/// DT-LM-02：tokens / costs 维度 —— 历史累计判定（达到限值即拒下一个）。
#[tokio::test]
async fn peek_rejects_tokens_and_costs_dimensions() {
    let l = SlidingWindowLimiter::new();

    // tokens：settle 累计 10 == limit 10 → 拒绝
    let key = rkey("kh-t", "m");
    let tok_wins = [wl(None, Some(10), None, 60)];
    l.settle_usage(&key, &kscope("kh-t"), &tok_wins, 4, 6, 0, 0, 0);
    let d = l.peek_only(&key, &tok_wins, 1).await;
    assert!(!d.allowed);
    assert_eq!(d.limit, 10);
    assert!(matches!(d.rejected_kind, Some(LimitDimension::Tokens)));

    // costs：settle 累计 3 微美元 >= 限值 2 微美元 → 拒绝（limit 以 micros 报告）
    let key2 = rkey("kh-c", "m");
    let cost_wins = [wl(None, None, Some(dec("0.000002")), 60)];
    l.settle_usage(&key2, &kscope("kh-c"), &cost_wins, 0, 0, 1, 1, 1);
    let d = l.peek_only(&key2, &cost_wins, 1).await;
    assert!(!d.allowed);
    assert_eq!(d.limit, 2);
    assert!(matches!(d.rejected_kind, Some(LimitDimension::Costs)));
}

/// DT-LM-03：完整三段式 —— 窗口三维度落位 + 6 个累计计数器（key/team scope）+
/// 全零 settle 早退。
#[tokio::test]
async fn settle_usage_populates_windows_and_cumulative() {
    let l = SlidingWindowLimiter::new();
    let wins = [
        wl(Some(10), None, None, 60),
        wl(None, Some(100), None, 60),
        wl(None, None, Some(dec("0.01")), 3600),
    ];
    let key = rkey("khF", "gpt-4");
    assert!(l.peek_only(&key, &wins, 1).await.allowed);
    assert_eq!(l.commit_counts(&key, &wins, 1).remaining, 9);

    l.settle_usage(&key, &kscope("khF"), &wins, 100, 40, 2, 1, 3);

    let u60 = l.get_usage("khF:gpt-4:60").expect("60s entry");
    assert_eq!((u60.counts, u60.tokens, u60.costs_micros), (1, 140, 6));
    let u3600 = l.get_usage("khF:gpt-4:3600").expect("3600 entry");
    assert_eq!((u3600.counts, u3600.tokens, u3600.costs_micros), (0, 140, 6));

    let s = kscope("khF");
    assert_eq!(l.peek_cumulative(&s, CumulativeKind::TotalInputTokens), 100);
    assert_eq!(l.peek_cumulative(&s, CumulativeKind::TotalOutputTokens), 40);
    assert_eq!(l.peek_cumulative(&s, CumulativeKind::TotalCost), 6);
    assert_eq!(l.peek_cumulative(&s, CumulativeKind::TotalRegularInputCost), 2);
    assert_eq!(l.peek_cumulative(&s, CumulativeKind::TotalCachedInputCost), 1);
    assert_eq!(l.peek_cumulative(&s, CumulativeKind::TotalOutputCost), 3);

    // team scope：窗口落在 __team__ 命名空间，累计落在 tc:{tid}:*
    let tkey = rkey("__team__t9", "gpt-4");
    l.settle_usage(&tkey, &tscope("t9"), &wins, 7, 3, 0, 0, 0);
    assert_eq!(
        l.peek_cumulative(&tscope("t9"), CumulativeKind::TotalInputTokens),
        7
    );
    assert!(l.get_usage("__team__t9:gpt-4:60").is_some());

    // 全零 settle：bump / add_cumulative 早退分支，不 panic
    l.settle_usage(&key, &kscope("khF"), &[], 0, 0, 0, 0, 0);
}

/// DT-LM-04：窗口过期 —— peek 归零、commit/settle 过期复位、
/// snapshot/scan 跳过、cleanup_expired 只删过期项。
#[tokio::test]
async fn window_expiry_resets_counters() {
    let l = SlidingWindowLimiter::new();
    let wins1 = [wl(Some(5), Some(50), Some(dec("1")), 1)];
    let ka = rkey("expA", "m");
    let kb = rkey("expB", "m");
    let kc = rkey("expC", "m");

    l.commit_counts(&ka, &wins1, 1);
    l.settle_usage(&ka, &kscope("expA"), &wins1, 3, 3, 0, 0, 0);
    l.settle_usage(&kb, &kscope("expB"), &wins1, 2, 2, 1, 1, 1); // settle 建项（counts=0）
    l.commit_counts(&kc, &wins1, 1); // 留着过期给 cleanup

    tokio::time::sleep(Duration::from_millis(1200)).await;

    // 过期 → peek 把三维度当 0 → 放行
    assert!(l.peek_only(&ka, &wins1, 1).await.allowed);
    // 列表 / 快照跳过过期项
    assert!(l.peek_key_windows("expA").is_empty());
    assert!(l.snapshot().iter().all(|s| !s.cache_key.starts_with("exp")));

    // 过期后 commit → 复位为 weight（and_modify 过期分支：tokens/costs 清零）
    l.commit_counts(&ka, &wins1, 1);
    let ua = l.get_usage("expA:m:1").unwrap();
    assert_eq!((ua.counts, ua.tokens, ua.costs_micros), (1, 0, 0));

    // 过期后 settle → counts 清零、tokens/costs 重开（and_modify 过期分支）
    l.settle_usage(&kb, &kscope("expB"), &wins1, 1, 1, 2, 0, 0);
    let ub = l.get_usage("expB:m:1").unwrap();
    assert_eq!((ub.counts, ub.tokens, ub.costs_micros), (0, 2, 2));

    // cleanup 只删仍过期的 expC
    assert_eq!(l.cleanup_expired(), 1);
    assert!(l.get_usage("expC:m:1").is_none());
    assert!(l.get_usage("expA:m:1").is_some());
}

/// DT-LM-05：累计计数器 —— reset 返回复位前快照；缺席 scope 一律 0；
/// restore 负值 peek 钳 0。
#[tokio::test]
async fn cumulative_reset_returns_snapshot() {
    let l = SlidingWindowLimiter::new();
    let key = rkey("khR", "m");
    l.settle_usage(&key, &kscope("khR"), &[wl(None, Some(1), None, 60)], 10, 20, 1, 2, 3);

    assert_eq!(l.peek_cumulative(&kscope("other"), CumulativeKind::TotalInputTokens), 0);

    let snap = l.reset_cumulative_local(&kscope("khR"));
    let CumulativeSnapshot {
        input_tokens,
        output_tokens,
        total_cost_micros,
        regular_input_cost_micros,
        cached_input_cost_micros,
        output_cost_micros,
    } = snap;
    assert_eq!(
        (input_tokens, output_tokens, total_cost_micros, regular_input_cost_micros,
         cached_input_cost_micros, output_cost_micros),
        (10, 20, 6, 1, 2, 3)
    );
    assert_eq!(l.peek_cumulative(&kscope("khR"), CumulativeKind::TotalCost), 0);

    // 复位缺席 scope → 全零快照
    assert_eq!(l.reset_cumulative_local(&kscope("never")).input_tokens, 0);

    // 负值（max(0) 钳制）
    l.restore_cumulative("kc:neg:tin".to_string(), -5);
    assert_eq!(l.peek_cumulative(&kscope("neg"), CumulativeKind::TotalInputTokens), 0);
}

/// DT-LM-06：仪表盘查询 —— get_usage / get_usage_for_key /
/// peek_plan_window_usage（__plan__ 命名空间三维度快照，!826c045 dashboard
/// window-usage 列）/ peek_key_windows / peek_team_windows（model 含 ':'
/// 的 cache_key 解析）。
#[tokio::test]
async fn dashboard_listing_queries() {
    let l = SlidingWindowLimiter::new();
    assert!(l.get_usage("none:m:60").is_none());
    assert!(l.get_usage_for_key("nobody").is_empty());
    assert!(l.peek_plan_window_usage("nobody").is_empty());

    let wins = [wl(Some(10), Some(50), Some(dec("0.000003")), 60), wl(Some(100), None, None, 3600)];
    let key = rkey("khL", "gpt:4"); // model 含 ':' → 从右侧解析 window_secs
    l.commit_counts(&key, &wins, 1);
    l.settle_usage(&key, &kscope("khL"), &wins, 5, 5, 1, 1, 1);

    // 每 key 两项窗口
    let mut us = l.get_usage_for_key("khL");
    us.sort_by(|a, b| a.cache_key.cmp(&b.cache_key));
    assert_eq!(us.len(), 2);

    // token/cost 窗口列表：60s 与 3600s 各出 tokens+costs 两条
    let infos = l.peek_key_windows("khL");
    assert_eq!(infos.len(), 4);
    let ti = infos
        .iter()
        .find(|i| matches!(i.kind, WindowKind::Tokens) && i.window_secs == 60)
        .unwrap();
    assert_eq!((ti.model.as_str(), ti.count), ("gpt:4", 10));
    let ci = infos
        .iter()
        .find(|i| matches!(i.kind, WindowKind::CostMicros) && i.window_secs == 3600)
        .unwrap();
    assert_eq!(ci.count, 3);

    // team 命名空间列表
    let tkey = rkey("__team__tL", "m");
    l.settle_usage(&tkey, &tscope("tL"), &[wl(None, Some(9), None, 60)], 4, 5, 0, 0, 0);
    let tinfos = l.peek_team_windows("tL");
    assert_eq!(tinfos.len(), 1);
    assert_eq!(tinfos[0].count, 9);

    // plan 命名空间窗口（dashboard window-usage 列）：__plan__ model 的
    // 计数被 peek_plan_window_usage 快照，counts/tokens/costs 三维度携带
    let pkey = rkey("khP", "__plan__");
    l.commit_counts(&pkey, &[wl(Some(7), None, None, 120)], 2);
    l.settle_usage(&pkey, &kscope("khP"), &[wl(Some(7), Some(80), Some(dec("0.000002")), 120)], 30, 10, 2, 0, 0);
    let plan = l.peek_plan_window_usage("khP");
    assert_eq!(plan.len(), 1);
    let pu = &plan[0];
    assert_eq!((pu.counts, pu.tokens, pu.costs_micros, pu.window_secs), (2, 40, 2, 120));
    assert!(pu.elapsed_secs <= 120, "elapsed {}", pu.elapsed_secs);
    // key 前缀精确匹配：khP2 不命中 khP 的 plan 窗口
    assert!(l.peek_plan_window_usage("khP2").is_empty());
}

/// DT-LM-07：snapshot/restore —— 过期 restore 跳过、有效 restore 落位、
/// 畸形 cache_key（无冒号 / 秒数非数字）被扫描器跳过。
#[tokio::test]
async fn snapshot_restore_and_scan_edge_keys() {
    let l = SlidingWindowLimiter::new();

    // 过期 restore（window_start 太老）→ 跳过
    l.restore_counter("dead:m:60".to_string(), 5, 5, 5, 0, 10);
    assert!(l.get_usage("dead:m:60").is_none());

    // 有效 restore → 落位且进 snapshot
    let ws = now_epoch().saturating_sub(5);
    l.restore_counter("live:m:60".to_string(), 1, 2, 3, ws, 60);
    let u = l.get_usage("live:m:60").unwrap();
    assert_eq!((u.counts, u.tokens, u.costs_micros, u.window_secs), (1, 2, 3, 60));
    assert_eq!(l.snapshot().len(), 1);

    // 畸形 key：tail 无 ':' → rsplit 失败；secs 非数字 → parse 失败
    l.restore_counter("kh1:weird".to_string(), 0, 9, 0, now_epoch(), 60);
    l.restore_counter("kh1:m:abc".to_string(), 0, 0, 7, now_epoch(), 60);
    assert!(l.peek_key_windows("kh1").is_empty());
}

/// DT-LM-08：清理 —— clear_windows（命中/未命中）、clear_for_key 计数、
/// clear_all_windows 只清窗口、**保留累计计量**（!fb9dccd：累计供 dashboard
/// spend 列与 plan total 限额，reset-all-limits 不再清零）。
#[tokio::test]
async fn clear_operations() {
    let l = SlidingWindowLimiter::new();
    let key = rkey("khC", "m");
    let wins = [wl(Some(5), None, None, 60), wl(Some(5), None, None, 300)];
    l.commit_counts(&key, &wins, 1);
    l.settle_usage(&key, &kscope("khC"), &wins, 1, 1, 0, 0, 0);
    let other = rkey("other", "m");
    l.commit_counts(&other, &[wl(Some(5), None, None, 60)], 1);

    l.clear_windows(&key, &[60]); // 命中
    l.clear_windows(&key, &[999]); // 未命中，不 panic
    assert!(l.get_usage("khC:m:60").is_none());
    assert!(l.get_usage("khC:m:300").is_some());

    assert_eq!(l.clear_for_key("khC"), 1);
    assert!(l.get_usage("khC:m:300").is_none());

    assert!(l.clear_all_windows() >= 1);
    assert!(l.snapshot().is_empty());
    // 累计计量保留（settle 的 input_tokens=1 仍在）
    assert_eq!(l.peek_cumulative(&kscope("khC"), CumulativeKind::TotalInputTokens), 1);
}

/// DT-LM-09：Decimal ↔ micros 换算（含负值钳 0）。
#[test]
fn decimal_micros_roundtrip() {
    assert_eq!(decimal_to_micros(dec("1.5")), 1_500_000);
    assert_eq!(micros_to_decimal(2_500_000), dec("2.5"));
    assert_eq!(micros_to_decimal(0), dec("0"));
    // 负数 → to_u64 失败 → 0
    assert_eq!(decimal_to_micros(dec("-1")), 0);
}

// ═════════════════════════════════════════════════════════════════
// RateLimitPlan — effective_limits / validate_schedule_overlap
// ═════════════════════════════════════════════════════════════════

/// DT-LM-10：rpm/tpm 简写合并 —— 用户 60s 条目优先，缺的维度补到同一条件上；
/// 都已覆盖则原样返回；无 60s 条目则追加合成条目。
#[test]
fn effective_limits_merge_shorthand() {
    // 已有 60s counts 条目：counts 用用户的，tpm 补上
    let p = RateLimitPlan {
        rpm_limit: Some(60),
        tpm_limit: Some(1000),
        window_limits: vec![wl(Some(5), None, None, 60)],
        ..plan("a", PlanType::Key)
    };
    let (concur, wins, stale) = p.effective_limits();
    assert_eq!(concur, None);
    assert!(stale.is_empty());
    assert_eq!(wins.len(), 1);
    assert_eq!((wins[0].counts, wins[0].tokens), (Some(5), Some(1000)));

    // 两维度都已被 60s 条目覆盖 → 简写完全忽略
    let p = RateLimitPlan {
        rpm_limit: Some(60),
        tpm_limit: Some(1000),
        window_limits: vec![wl(Some(5), Some(50), None, 60)],
        ..plan("b", PlanType::Key)
    };
    let (_, wins, _) = p.effective_limits();
    assert_eq!(wins.len(), 1);
    assert_eq!((wins[0].counts, wins[0].tokens), (Some(5), Some(50)));

    // 无 60s 条目 → 追加合成 60s counts 条目
    let p = RateLimitPlan {
        rpm_limit: Some(30),
        window_limits: vec![wl(None, Some(10), None, 3600)],
        ..plan("c", PlanType::Key)
    };
    let (concur, wins, _) = p.effective_limits();
    assert_eq!(concur, None);
    assert_eq!(wins.len(), 2);
    let w60 = wins.iter().find(|w| w.window_secs == 60).unwrap();
    assert_eq!((w60.counts, w60.tokens), (Some(30), None));
}

/// DT-LM-11：schedule —— 激活槽覆盖 base 限制并计算 stale 窗口；
/// 无激活槽回退 base，schedule 独有窗口算 stale。
#[test]
fn effective_limits_schedule_slots() {
    // 激活槽（0:00-24:00 恒真）：slot 限制覆盖 base，stale = base 独有秒数
    let p = RateLimitPlan {
        concurrency_limit: Some(4),
        window_limits: vec![wl(Some(9), None, None, 3600), wl(Some(9), None, None, 60)],
        schedule: vec![ScheduleSlot {
            hours: "0:00-24:00".to_string(),
            concurrency_limit: Some(9),
            window_limits: vec![wl(Some(1), None, None, 60)],
            ..Default::default()
        }],
        ..plan("s1", PlanType::Key)
    };
    let (concur, wins, stale) = p.effective_limits();
    assert_eq!(concur, Some(9), "slot overrides base concurrency");
    assert_eq!(stale, vec![3600]);
    assert_eq!(wins.len(), 1);
    assert_eq!((wins[0].counts, wins[0].window_secs), (Some(1), 60));

    // 未激活槽（24:00-24:00 恒假）：用 base；schedule 独有窗口 → stale
    let p = RateLimitPlan {
        window_limits: vec![wl(Some(5), None, None, 60)],
        schedule: vec![ScheduleSlot {
            hours: "24:00-24:00".to_string(),
            window_limits: vec![wl(Some(1), None, None, 300)],
            ..Default::default()
        }],
        ..plan("s2", PlanType::Key)
    };
    let (concur, wins, stale) = p.effective_limits();
    assert_eq!(concur, None);
    assert_eq!(wins.len(), 1);
    assert_eq!(wins[0].window_secs, 60);
    assert_eq!(stale, vec![300]);
}

/// DT-LM-12：validate_schedule_overlap —— 相接不重叠 OK / 重叠 Err /
/// 跨午夜重叠 / 全天槽（start==end）/ 畸形 hours 忽略 / 空表 OK。
#[test]
fn validate_schedule_overlap_matrix() {
    let mk = |hours: Vec<&str>| RateLimitPlan {
        schedule: hours
            .into_iter()
            .map(|h| ScheduleSlot { hours: h.to_string(), ..Default::default() })
            .collect(),
        ..plan("sp", PlanType::Key)
    };

    // 边界相接（9-21 与 21-9 跨午夜）不重叠
    assert!(mk(vec!["9:00-21:00", "21:00-9:00"]).validate_schedule_overlap().is_ok());
    // 普通重叠
    let err = mk(vec!["9:00-21:00", "20:00-23:00"]).validate_schedule_overlap().unwrap_err();
    assert!(err.contains("overlap"), "{err}");
    // 跨午夜槽与白天槽重叠
    assert!(mk(vec!["21:00-9:00", "8:00-10:00"]).validate_schedule_overlap().is_err());
    // 全天槽（start==end 展开为全天）与任何槽重叠
    assert!(mk(vec!["9:00-9:00", "10:00-11:00"]).validate_schedule_overlap().is_err());
    // 畸形 hours 解析失败 → 该槽被过滤，不参与判定
    assert!(mk(vec!["garbage", "10:00-11:00"]).validate_schedule_overlap().is_ok());
    // 空表
    assert!(mk(vec![]).validate_schedule_overlap().is_ok());
}

/// DT-LM-13：is_active_now —— 恒真区间 / 恒假区间 / 畸形串（无 '-' / 非法）。
#[test]
fn schedule_slot_activation_parsing() {
    let slot = |h: &str| ScheduleSlot { hours: h.to_string(), ..Default::default() };
    assert!(slot("0:00-24:00").is_active_now());
    assert!(!slot("24:00-24:00").is_active_now());
    assert!(!slot("not-hours").is_active_now());
    assert!(!slot("9:00").is_active_now());
}

// ═════════════════════════════════════════════════════════════════
// PlanStore — plan CRUD / 分配三态 / 类型校验
// ═════════════════════════════════════════════════════════════════

/// DT-LM-14：plan CRUD + 默认 plan + key 分配三态 + 解析。
#[test]
fn plan_store_crud_and_key_assignments() {
    let ps = PlanStore::new();
    ps.upsert_plan(plan("p1", PlanType::Key));
    assert_eq!(ps.list_plans().len(), 1);
    assert_eq!(ps.get_plan("p1").unwrap().name, "p1");

    // 默认 plan：名字可指向不存在的 plan（get 返回 None）
    assert_eq!(ps.get_default_plan_name(), None);
    ps.set_default_plan(Some("p1".to_string()));
    assert_eq!(ps.get_default_plan_name().as_deref(), Some("p1"));
    assert!(ps.get_default_plan().is_some());
    ps.set_default_plan(Some("missing".to_string()));
    assert_eq!(ps.get_default_plan_name().as_deref(), Some("missing"));
    assert!(ps.get_default_plan().is_none());
    ps.set_default_plan(None);
    assert_eq!(ps.get_default_plan_name(), None);

    // key 分配 + 类型校验
    ps.upsert_plan(plan("pt", PlanType::Team));
    assert!(ps.assign_key("kh1", "p1").is_ok());
    assert!(ps.assign_key("kh2", "nope").is_err(), "plan not found");
    assert!(ps.assign_key("kh3", "pt").is_err(), "team plan cannot go to a key");

    // 三态：无行 / 显式无 plan / 显式指名
    assert_eq!(ps.get_plan_name_explicit("khX"), None);
    ps.assign_key_no_plan("kh4");
    assert_eq!(ps.get_plan_name_explicit("kh4"), Some(None::<String>));
    assert_eq!(ps.get_plan_name("kh4"), None);
    assert_eq!(ps.get_plan_name_explicit("kh1"), Some(Some("p1".to_string())));
    assert_eq!(ps.get_plan_name("kh1").as_deref(), Some("p1"));

    // resolve_plan 三态
    assert!(ps.resolve_plan("kh1").is_some());
    assert!(ps.resolve_plan("kh4").is_none(), "explicit no-plan");
    assert!(ps.resolve_plan("khX").is_none(), "no row");

    // delete_plan：清掉指名引用，保留显式 no-plan
    ps.upsert_plan(plan("dead", PlanType::Key));
    assert!(ps.assign_key("kh5", "dead").is_ok());
    assert!(ps.delete_plan("dead"));
    assert!(!ps.delete_plan("dead"));
    assert!(ps.resolve_plan("kh5").is_none());
    assert_eq!(ps.get_plan_name_explicit("kh4"), Some(None::<String>));

    // unassign / restore / persisted 移除
    assert!(ps.unassign_key("kh1"));
    assert!(!ps.unassign_key("kh1"));
    ps.restore_assignment("kh9", Some("p1"));
    assert_eq!(ps.get_plan_name("kh9").as_deref(), Some("p1"));
    ps.restore_assignment("kh10", None);
    assert_eq!(ps.get_plan_name_explicit("kh10"), Some(None::<String>));
    assert!(ps.remove_assignment_persisted("kh9"));
    assert!(!ps.remove_assignment_persisted("kh9"));
    assert!(!ps.list_assignments().is_empty());
    assert!(!ps.snapshot_assignments().is_empty());

    // clear_plans：清 plan 与默认，保留分配与并发计数
    ps.clear_plans();
    assert!(ps.get_plan("p1").is_none());
    assert_eq!(ps.get_default_plan_name(), None);
    assert!(ps.get_plan_name_explicit("kh4").is_some());

    // cleanup_assignments：清指向缺失 plan 的行，保留显式 no-plan
    ps.upsert_plan(plan("p2", PlanType::Key));
    ps.restore_assignment("kA", Some("p2"));
    ps.restore_assignment("kB", Some("gone"));
    ps.cleanup_assignments();
    assert_eq!(ps.get_plan_name_explicit("kA"), Some(Some("p2".to_string())));
    assert_eq!(ps.get_plan_name_explicit("kB"), None);
    assert_eq!(ps.get_plan_name_explicit("kh4"), Some(None::<String>));
}

/// DT-LM-15：team 分配 + resolve_team_plan 回退默认 + 名称查询。
#[test]
fn plan_store_team_assignments() {
    let ps = PlanStore::new();
    ps.upsert_plan(plan("tk1", PlanType::Team));
    ps.upsert_plan(plan("p1", PlanType::Key));

    assert!(ps.assign_team("t1", "tk1").is_ok());
    assert!(ps.assign_team("t2", "p1").is_err(), "key plan cannot go to a team");
    assert!(ps.assign_team("t3", "nope").is_err());
    assert_eq!(ps.get_team_plan_name("t1").as_deref(), Some("tk1"));

    // 显式分配优先
    assert_eq!(ps.resolve_team_plan("t1").unwrap().name, "tk1");
    // 无分配 → 回退默认 team plan
    ps.set_default_team_plan(Some("tk1".to_string()));
    assert!(ps.resolve_team_plan("no-team").is_some());
    // 分配指向缺失 plan → 也回退默认
    ps.restore_team_assignment("t4", "missing");
    assert_eq!(ps.resolve_team_plan("t4").unwrap().name, "tk1");
    // 无默认 → None
    ps.set_default_team_plan(None);
    assert!(ps.resolve_team_plan("no-team").is_none());

    // effective 名称：显式或默认
    ps.set_default_team_plan(Some("tk1".to_string()));
    assert_eq!(ps.get_team_plan_name_effective("tX").as_deref(), Some("tk1"));
    assert_eq!(ps.get_team_plan_name_effective("t1").as_deref(), Some("tk1"));
    ps.set_default_team_plan(None);
    assert_eq!(ps.get_team_plan_name_effective("tX"), None);
    assert_eq!(ps.get_default_team_plan_name(), None);

    assert!(ps.unassign_team("t1"));
    assert!(!ps.unassign_team("t1"));
    assert!(!ps.list_team_assignments().is_empty());
    assert!(!ps.snapshot_team_assignments().is_empty());
    assert!(ps.remove_team_assignment_persisted("t4"));
    assert!(!ps.remove_team_assignment_persisted("t4"));
}

// ═════════════════════════════════════════════════════════════════
// ConcurrencyGuard / GuardedStream
// ═════════════════════════════════════════════════════════════════

/// DT-LM-16：并发守卫 —— 超限拒绝、Drop 递减、team 通道独立、
/// cleanup_concurrency 清零计数项。
#[test]
fn concurrency_guard_acquire_release() {
    let ps = PlanStore::new();
    assert_eq!(ps.get_concurrency("k"), 0);
    assert_eq!(ps.get_team_concurrency("t"), 0);

    let g1 = ps.try_acquire("k", 2).expect("first");
    let g2 = ps.try_acquire("k", 2).expect("second");
    assert!(ps.try_acquire("k", 2).is_none(), "limit 2 exceeded");
    assert_eq!(ps.get_concurrency("k"), 2);

    drop(g2);
    assert_eq!(ps.get_concurrency("k"), 1);
    assert!(ps.try_acquire("k", 2).is_some());

    let tg = ps.try_acquire_team("t", 1).expect("team slot");
    assert!(ps.try_acquire_team("t", 1).is_none());
    drop(tg);
    drop(g1);
    assert_eq!(ps.get_concurrency("k"), 0);
    assert_eq!(ps.get_team_concurrency("t"), 0);

    // 计数归零的项被清理（k 与 t 两项）
    assert_eq!(ps.cleanup_concurrency(), 2);
    assert_eq!(ps.cleanup_concurrency(), 0);
}

/// DT-LM-17：GuardedStream —— 流结束自动释放守卫；None 守卫不 panic。
#[tokio::test]
async fn guarded_stream_releases_on_end() {
    let ps = PlanStore::new();
    let guard = ps.try_acquire("k", 1).expect("slot");
    let mut s = GuardedStream::new(futures::stream::iter(vec![1u32, 2, 3]), Some(guard));
    assert_eq!(ps.get_concurrency("k"), 1);

    let got: Vec<u32> = s.by_ref().collect().await;
    assert_eq!(got, vec![1, 2, 3]);
    assert_eq!(ps.get_concurrency("k"), 0, "guard released at stream end");

    // None 守卫 + 空流
    let s2 = GuardedStream::new(futures::stream::iter(Vec::<u8>::new()), None);
    assert_eq!(s2.count().await, 0);
}

// ═════════════════════════════════════════════════════════════════
// migrations — DDL 常量
// ═════════════════════════════════════════════════════════════════

/// DT-LM-18：8 个 DDL 函数返回预期表名 / 关键子句。
#[test]
fn migrations_ddl_contents() {
    assert!(rate_limit_state_ddl().contains("boom_rate_limit_state"));
    assert!(state_alter_ddl().contains("ADD COLUMN IF NOT EXISTS tokens"));
    assert!(cumulative_ddl().contains("boom_rate_limit_cumulative"));
    assert!(assignment_ddl().contains("boom_key_plan_assignment"));
    assert!(assignment_alter_ddl().contains("DROP NOT NULL"));
    assert!(team_assignment_ddl().contains("boom_team_plan_assignment"));
    assert!(plan_ddl().contains("boom_rate_limit_plan"));
    assert!(plan_alter_ddl().contains("RENAME COLUMN"));
}
