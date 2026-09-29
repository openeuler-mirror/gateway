//! DT 用例 — boom-ctxaware：客户端类型分类 + 60 分钟 agent 统计 ring。
//! 覆盖 classifier 全分支 + AgentStatsTracker record/record_tokens/snapshot 聚合与 ratio。

use boom_ctxaware::{
    classify, is_anthropic_path, AgentStatsTracker, ClientKind, CLIENT_TYPE_HEADER,
};

/// DT-CTX-01：is_anthropic_path 精确匹配 /v1/messages 及其子路径。
#[test]
fn anthropic_path_exact_and_subpath() {
    assert!(is_anthropic_path("/v1/messages"));
    assert!(is_anthropic_path("/v1/messages/count_tokens"));
    assert!(is_anthropic_path("/v1/messages/"));
    assert!(!is_anthropic_path("/v1/chat/completions"));
    assert!(!is_anthropic_path("/v1/completions"));
    assert!(!is_anthropic_path("/v1/message"));
    assert!(!is_anthropic_path("/anything"));
}

/// DT-CTX-02：classify 按 api_path 分流 Anthropic / Other。
#[test]
fn classify_routes_messages_to_anthropic() {
    assert_eq!(classify("/v1/messages"), ClientKind::Anthropic);
    assert_eq!(classify("/v1/messages/count_tokens"), ClientKind::Anthropic);
    assert_eq!(classify("/v1/chat/completions"), ClientKind::Other);
    assert_eq!(classify("/v1/completions"), ClientKind::Other);
    assert_eq!(classify("/anything-else"), ClientKind::Other);
}

/// DT-CTX-03：wire_label 输出稳定 header 值，CLIENT_TYPE_HEADER 常量正确。
#[test]
fn wire_label_and_header_constant() {
    assert_eq!(ClientKind::Anthropic.wire_label(), "anthropic");
    assert_eq!(ClientKind::Other.wire_label(), "anonymous");
    assert_eq!(CLIENT_TYPE_HEADER, "X-BooM-Client-Type");
}

/// DT-CTX-04：record 按 path 分桶，snapshot 汇总请求计数与 anthropic 占比。
#[test]
fn record_buckets_by_path_and_aggregates_summary() {
    let tracker = AgentStatsTracker::new();
    tracker.record("/v1/messages");
    tracker.record("/v1/messages");
    tracker.record("/v1/chat/completions");
    tracker.record("/v1/completions");

    let snap = tracker.snapshot();
    assert_eq!(snap.events.len(), 60);
    let last = snap.events.last().unwrap();
    assert_eq!(last.total, 4);
    assert_eq!(last.anthropic, 2);
    assert_eq!(snap.summary.total, 4);
    assert_eq!(snap.summary.anthropic, 2);
    assert!((snap.summary.ratio - 0.5).abs() < 1e-9);
}

/// DT-CTX-05：冷启动（无 record）summary 全零，ratio 为 0.0。
#[test]
fn cold_start_has_zero_ratio() {
    let tracker = AgentStatsTracker::new();
    let snap = tracker.snapshot();
    assert_eq!(snap.summary.total, 0);
    assert_eq!(snap.summary.anthropic, 0);
    assert_eq!(snap.summary.ratio, 0.0);
    assert_eq!(snap.summary.input_token_ratio, 0.0);
    assert_eq!(snap.summary.output_token_ratio, 0.0);
}

/// DT-CTX-06：record_tokens 按 path 分流 total/anthropic 子合计，ratio 正确。
#[test]
fn record_tokens_accumulates_by_path() {
    let tracker = AgentStatsTracker::new();
    tracker.record_tokens("/v1/messages", 100, 200);
    tracker.record_tokens("/v1/messages", 50, 80);
    tracker.record_tokens("/v1/chat/completions", 500, 60);

    let snap = tracker.snapshot();
    assert_eq!(snap.summary.input_tokens_total, 650);
    assert_eq!(snap.summary.input_tokens_anthropic, 150);
    assert_eq!(snap.summary.output_tokens_total, 340);
    assert_eq!(snap.summary.output_tokens_anthropic, 280);
    assert!((snap.summary.input_token_ratio - (150.0 / 650.0)).abs() < 1e-9);
    assert!((snap.summary.output_token_ratio - (280.0 / 340.0)).abs() < 1e-9);
}

/// DT-CTX-07：record_tokens 在无 record 时不影响请求计数（两计数器独立）。
#[test]
fn record_tokens_without_record_bumps_tokens_only() {
    let tracker = AgentStatsTracker::new();
    tracker.record_tokens("/v1/messages", 100, 200);
    let snap = tracker.snapshot();
    assert_eq!(snap.summary.total, 0);
    assert_eq!(snap.summary.input_tokens_anthropic, 100);
    assert_eq!(snap.summary.output_tokens_anthropic, 200);
}

/// DT-CTX-08：Default 等价于 new，事件序列首尾标签正确。
#[test]
fn default_equals_new_and_event_labels() {
    let tracker = AgentStatsTracker::default();
    tracker.record("/v1/messages");
    let snap = tracker.snapshot();
    assert_eq!(snap.events.len(), 60);
    // 最旧是 -59m，最新是 now
    assert_eq!(snap.events.first().unwrap().minute, "-59m");
    assert_eq!(snap.events.last().unwrap().minute, "now");
    assert_eq!(snap.events.last().unwrap().total, 1);
}

/// DT-CTX-09：混合 record 与 record_tokens，summary 同时反映两者。
#[test]
fn mixed_record_and_tokens_both_counted() {
    let tracker = AgentStatsTracker::new();
    tracker.record("/v1/messages");
    tracker.record("/v1/chat/completions");
    tracker.record_tokens("/v1/messages", 300, 100);
    tracker.record_tokens("/v1/chat/completions", 200, 50);

    let snap = tracker.snapshot();
    assert_eq!(snap.summary.total, 2);
    assert_eq!(snap.summary.anthropic, 1);
    assert!((snap.summary.ratio - 0.5).abs() < 1e-9);
    assert_eq!(snap.summary.input_tokens_total, 500);
    assert_eq!(snap.summary.input_tokens_anthropic, 300);
    assert_eq!(snap.summary.output_tokens_total, 150);
    assert_eq!(snap.summary.output_tokens_anthropic, 100);
}
