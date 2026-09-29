//! DT 用例 — boom-trace：请求级 trace 链路 + 延迟分布中枢。
//! 覆盖 W3C traceparent 解析/构造、trace_filter_matches 全分支、
//! RequestSpan 属性/finalize、TraceRegistry start/finalize/snapshot、
//! TraceGuard RAII（Drop finalize、mark_error、属性写入、child span id）。
//!
//! 关键：用 `replace_otlp(&disabled_config, true)` 在不创建 exporter 的前提下
//! 打开 registry.enabled 标志——`enabled && config.enabled` 为 false 故不构造
//! TraceExporter，finalize 路径的 OTLP enqueue 因 exporter=None 静默跳过，全程无网络。

use boom_core::trace::{SpanAttributeValue, SpanStatus, TraceApi};
use boom_core::OtlpConfig;
use boom_trace::{
    build_child_traceparent, parse_traceparent, trace_filter_matches, RequestSpan, TraceGuard,
    TraceRegistry, W3cContext,
};

fn w3c() -> W3cContext {
    let mut ctx = parse_traceparent("00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01")
        .expect("well-formed traceparent");
    ctx.trace_state = "opencode_user_id=alice,vendor=acme".to_string();
    ctx
}

/// 启用 registry 但不创建 exporter：config.enabled=false 使 replace_otlp
/// 的 `enabled && config.enabled` 为 false → 不构造 TraceExporter、不联网。
async fn enabled_registry() -> std::sync::Arc<TraceRegistry> {
    let reg = TraceRegistry::new();
    let cfg = OtlpConfig {
        enabled: false,
        ..OtlpConfig::default()
    };
    reg.replace_otlp(&cfg, true).await;
    reg
}

// ── W3C context parse / build ──

/// DT-TRC-01：parse_traceparent 解析合法 header，sampled 标志正确。
#[test]
fn parse_well_formed_traceparent() {
    let ctx = parse_traceparent("00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01")
        .expect("parse");
    assert_eq!(ctx.trace_id[0], 0x0a);
    assert_eq!(ctx.trace_id[1], 0xf7);
    assert_eq!(ctx.parent_span_id[0], 0x00);
    assert_eq!(ctx.parent_span_id[1], 0xf0);
    assert!(ctx.sampled);
    assert_eq!(ctx.trace_id_hex(), "0af76598164860bd9a43d7c1a31725ab");
    assert_eq!(ctx.parent_span_id_hex(), "00f067aa0ba902b7");
}

/// DT-TRC-02：flags=00 时 sampled=false。
#[test]
fn parse_unsampled_traceparent() {
    let ctx = parse_traceparent("00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-00")
        .expect("parse");
    assert!(!ctx.sampled);
}

/// DT-TRC-03：各种畸形输入被拒（节数、长度、非 hex）。
#[test]
fn parse_rejects_malformed() {
    assert!(parse_traceparent("garbage").is_none());
    assert!(parse_traceparent("00-short").is_none());
    // 节数不足
    assert!(parse_traceparent("00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7").is_none());
    // 非 hex 字符
    assert!(parse_traceparent("00-xx-00f067aa0ba902b7-01").is_none());
}

/// DT-TRC-04：build_child_traceparent 与 parse 互为逆运算，sampled/unsampled 两种 flags。
#[test]
fn build_child_round_trips() {
    let trace_id = [0x0a; 16];
    let span_id = [0x42; 8];
    let tp = build_child_traceparent(trace_id, span_id, true);
    let ctx = parse_traceparent(&tp).expect("child parses");
    assert_eq!(ctx.trace_id, trace_id);
    assert_eq!(ctx.parent_span_id, span_id);
    assert!(ctx.sampled);

    let tp2 = build_child_traceparent(trace_id, span_id, false);
    let ctx2 = parse_traceparent(&tp2).expect("child parses");
    assert!(!ctx2.sampled);
}

/// DT-TRC-05：tracestate_value 命中/未命中/空 state。
#[test]
fn tracestate_value_lookup() {
    let ctx = w3c();
    assert_eq!(ctx.tracestate_value("opencode_user_id"), Some("alice".to_string()));
    assert_eq!(ctx.tracestate_value("vendor"), Some("acme".to_string()));
    assert_eq!(ctx.tracestate_value("missing"), None);

    let empty = parse_traceparent("00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01").unwrap();
    assert_eq!(empty.tracestate_value("any"), None);
}

// ── trace_filter_matches ──

/// DT-TRC-06：keys 与 regex 都为空时匹配所有请求。
#[test]
fn filter_matches_when_both_empty() {
    let ctx = w3c();
    assert!(trace_filter_matches(&[], &None, &ctx));
}

/// DT-TRC-07：tracestate key 命中时 true。
#[test]
fn filter_matches_on_tracestate_key() {
    let ctx = w3c();
    assert!(trace_filter_matches(&["opencode_user_id".to_string()], &None, &ctx));
    assert!(!trace_filter_matches(&["nonexistent".to_string()], &None, &ctx));
}

/// DT-TRC-08：trace_id regex 前缀匹配（^）与包含匹配。
#[test]
fn filter_matches_on_trace_id_regex() {
    let ctx = w3c();
    // 前缀匹配
    assert!(trace_filter_matches(&[], &Some("0af7".to_string()), &ctx));
    assert!(trace_filter_matches(&[], &Some("^0af7".to_string()), &ctx));
    // 不匹配
    assert!(!trace_filter_matches(&[], &Some("deadbeef".to_string()), &ctx));
    assert!(!trace_filter_matches(&[], &Some("^deadbeef".to_string()), &ctx));
}

// ── RequestSpan ──

/// DT-TRC-09：RequestSpan::new 生成与 parent 不同的 span_id，字段初始化正确。
#[test]
fn span_new_has_distinct_id_and_defaults() {
    let w = w3c();
    let span = RequestSpan::new(
        "req-1".to_string(),
        &w,
        "gpt-4".to_string(),
        "/v1/chat/completions".to_string(),
        false,
        1_000_000_000,
    );
    assert_ne!(span.span_id, w.parent_span_id);
    assert_eq!(span.trace_id, w.trace_id);
    assert_eq!(span.parent_span_id, w.parent_span_id);
    assert_eq!(span.status, SpanStatus::Unset);
    assert_eq!(span.end_time_unix_nano, 0);
    assert!(span.attributes.is_empty());
    assert_eq!(span.deployment_id, None);
}

/// DT-TRC-10：set_attribute 覆盖已有 key，set_llm_request/response 写 Json 属性。
#[test]
fn span_set_attribute_replaces_and_body_setters() {
    let w = w3c();
    let mut span = RequestSpan::new("r".to_string(), &w, "m".to_string(), "/p".to_string(), false, 0);
    span.set_attribute("k", SpanAttributeValue::Int(1));
    span.set_attribute("k", SpanAttributeValue::Int(2));
    assert_eq!(span.attributes.len(), 1);
    span.set_attribute("other", SpanAttributeValue::String("v".to_string()));
    assert_eq!(span.attributes.len(), 2);

    let body = std::sync::Arc::new(serde_json::json!({"messages": []}));
    span.set_llm_request(body.clone());
    span.set_llm_response(body.clone());
    assert_eq!(span.attributes.len(), 4); // k, other, llm_request, llm_response
    span.set_deployment_id("dep-1".to_string());
    assert_eq!(span.deployment_id.as_deref(), Some("dep-1"));
}

/// DT-TRC-11：finalize_ok / finalize_error 设置 end_time 与 status。
#[test]
fn span_finalize_ok_and_error() {
    let w = w3c();
    let mut span = RequestSpan::new("r".to_string(), &w, "m".to_string(), "/p".to_string(), false, 1_000);
    span.finalize_ok(2_000);
    assert_eq!(span.end_time_unix_nano, 2_000);
    assert_eq!(span.status, SpanStatus::Ok);

    let mut span2 = RequestSpan::new("r2".to_string(), &w, "m".to_string(), "/p".to_string(), false, 1_000);
    span2.finalize_error(2_000, "boom".to_string());
    assert_eq!(span2.status, SpanStatus::Error);
    assert_eq!(span2.error_message.as_deref(), Some("boom"));
}

/// DT-TRC-12：to_snapshot 转成 boom_core::trace::RequestSpan（hex 编码 id）。
#[test]
fn span_to_snapshot_encodes_ids_as_hex() {
    let w = w3c();
    let span = RequestSpan::new("r".to_string(), &w, "m".to_string(), "/p".to_string(), true, 1_000);
    let snap = span.to_snapshot();
    assert_eq!(snap.request_id, "r");
    assert_eq!(snap.trace_id, w.trace_id_hex());
    assert_eq!(snap.parent_span_id, w.parent_span_id_hex());
    assert_eq!(snap.span_id.len(), 16); // 8 bytes → 16 hex chars
    assert_ne!(snap.span_id, snap.parent_span_id); // 与 parent 不同
    assert!(snap.is_stream);
    assert_eq!(snap.start_time_unix_nano, 1_000);
}

// ── TraceRegistry ──

/// DT-TRC-13：new 默认 enabled=false，otlp_status/probe_otlp 返回 None。
#[tokio::test]
async fn registry_new_has_no_otlp() {
    let reg = TraceRegistry::new();
    assert!(!reg.enabled());
    assert_eq!(reg.active_count(), 0);
    assert!(reg.otlp_status().await.is_none());
    assert!(reg.probe_otlp().await.is_none());
}

/// DT-TRC-14：start_request + finalize_ok 计入 recent ring，active 清空。
#[tokio::test]
async fn registry_start_finalize_ok_lands_in_recent() {
    let reg = enabled_registry().await;
    assert!(reg.enabled());
    let w = w3c();
    let _span = reg.start_request(
        "req-1".to_string(),
        &w,
        "gpt-4".to_string(),
        "/p".to_string(),
        false,
        1_000_000_000,
    );
    assert_eq!(reg.active_count(), 1);
    reg.finalize_ok("req-1", 2_000_000_000);
    assert_eq!(reg.active_count(), 0);

    let snap = reg.snapshot().await;
    assert_eq!(snap.total_spans_started, 1);
    assert_eq!(snap.total_spans_finalized, 1);
    assert_eq!(snap.total_spans_error, 0);
    assert_eq!(snap.recent.len(), 1);
    assert_eq!(snap.recent[0].request_id, "req-1");
    assert!(snap.active.is_empty());
}

/// DT-TRC-15：finalize_error 计入 error 计数与 recent ring，status=Error。
#[tokio::test]
async fn registry_finalize_error_bumps_counter() {
    let reg = enabled_registry().await;
    let w = w3c();
    let _span = reg.start_request("req-2".to_string(), &w, "m".to_string(), "/p".to_string(), false, 1_000);
    reg.finalize_error("req-2", 2_000, "boom".to_string());
    let snap = reg.snapshot().await;
    assert_eq!(snap.total_spans_error, 1);
    assert_eq!(snap.recent.len(), 1);
    assert_eq!(snap.recent[0].status, SpanStatus::Error);
}

/// DT-TRC-16：finalize 未知 request_id 是 no-op（不 panic、不增计数）。
#[tokio::test]
async fn registry_finalize_unknown_is_noop() {
    let reg = enabled_registry().await;
    reg.finalize_ok("ghost", 1_000);
    reg.finalize_error("ghost2", 1_000, "x".to_string());
    let snap = reg.snapshot().await;
    assert_eq!(snap.total_spans_started, 0);
    assert_eq!(snap.total_spans_finalized, 0);
    assert_eq!(snap.total_spans_error, 0);
}

/// DT-TRC-17：with_span_mut 命中时应用闭包，未命中返回 None。
#[tokio::test]
async fn registry_with_span_mut() {
    let reg = enabled_registry().await;
    let w = w3c();
    let _span = reg.start_request("r".to_string(), &w, "m".to_string(), "/p".to_string(), false, 100);
    let set = reg.with_span_mut("r", |s| {
        s.set_attribute("k", SpanAttributeValue::Int(42));
        s.deployment_id = Some("d1".to_string());
    });
    assert!(set.is_some());
    let read = reg.with_span_mut("r", |s| s.deployment_id.clone());
    assert_eq!(read, Some(Some("d1".to_string())));
    let miss = reg.with_span_mut("missing", |s| s.request_id.clone());
    assert!(miss.is_none());
}

// ── TraceGuard ──

/// DT-TRC-18：registry 未启用时 TraceGuard::start 返回 None。
#[tokio::test]
async fn guard_none_when_disabled() {
    let reg = TraceRegistry::new(); // enabled=false
    let w = w3c();
    let g = TraceGuard::start(
        reg.clone(),
        "req-1".to_string(),
        &w,
        "m".to_string(),
        "/p".to_string(),
        false,
        1_000,
        true, // should_trace=true 但 registry.enabled()=false
    );
    assert!(g.is_none());
    assert_eq!(reg.active_count(), 0);
}

/// DT-TRC-19：guard Drop 时 finalize_ok，recent ring 收到 span。
#[tokio::test]
async fn guard_drop_finalizes_ok() {
    let reg = enabled_registry().await;
    let w = w3c();
    {
        let _g = TraceGuard::start(
            reg.clone(),
            "req-2".to_string(),
            &w,
            "m".to_string(),
            "/p".to_string(),
            false,
            1_000,
            true,
        );
        assert_eq!(reg.active_count(), 1);
    }
    assert_eq!(reg.active_count(), 0);
    let snap = reg.snapshot().await;
    assert_eq!(snap.total_spans_started, 1);
    assert_eq!(snap.total_spans_finalized, 1);
    assert_eq!(snap.total_spans_error, 0);
}

/// DT-TRC-20：mark_error 使 Drop 走 finalize_error 路径。
#[tokio::test]
async fn guard_mark_error_routes_to_finalize_error() {
    let reg = enabled_registry().await;
    let w = w3c();
    {
        let mut g = TraceGuard::start(
            reg.clone(),
            "req-3".to_string(),
            &w,
            "m".to_string(),
            "/p".to_string(),
            false,
            1_000,
            true,
        )
        .expect("started");
        g.mark_error("upstream 500".to_string());
    }
    let snap = reg.snapshot().await;
    assert_eq!(snap.total_spans_error, 1);
    assert_eq!(snap.total_spans_finalized, 1);
}

/// DT-TRC-21：guard 的属性写入、llm_request/response、child_parent_span_id、request_id。
#[tokio::test]
async fn guard_attribute_setters_and_child_span_id() {
    let reg = enabled_registry().await;
    let w = w3c();
    let body = std::sync::Arc::new(serde_json::json!({"messages": []}));
    let g = TraceGuard::start(
        reg.clone(),
        "req-4".to_string(),
        &w,
        "m".to_string(),
        "/p".to_string(),
        false,
        1_000,
        true,
    )
    .expect("started");
    assert_eq!(g.request_id(), "req-4");
    g.set_attribute("custom", SpanAttributeValue::String("v".to_string()));
    g.set_llm_request(body.clone());
    g.set_llm_response(body.clone());
    let child_id = g.child_parent_span_id();
    assert_ne!(child_id, [0u8; 8]); // 非零
    assert_ne!(child_id, w.parent_span_id); // 与 parent 不同
    let has_llm = reg
        .with_span_mut("req-4", |s| {
            s.attributes.iter().any(|(k, _)| k == "boom-gateway.llm_request")
        })
        .unwrap_or(false);
    assert!(has_llm);
}
