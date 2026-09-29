//! DT 用例 — boom-trace::otlp_export：OTLP/HTTP traces 导出器。
//!
//! 覆盖（boom-trace 作为依赖编译时其 #[cfg(test)] 不参与，本文件通过
//! pub API 驱动同一份代码）：
//! - TraceExporter::new：初始 Online、status_snapshot 各字段
//! - enqueue：Online 入队 / Offline 立即丢弃（dropped_count +
//!   total_dropped_during_offline）/ 队列满弹最旧 / 攒满 batch_size
//!   触发后台 flush（flush_permit）
//! - flush：空批早退 / 成功 POST {endpoint}/v1/traces（自定义 header +
//!   Content-Type protobuf）/ 失败重试 3 次后转 Offline（episode +
//!   dropped_in_batch）
//! - probe：成功 → 转 Online（last_recovery_ts、consecutive 归零）/
//!   失败 → consecutive_probe_failures 递增（不影响 Online 状态）
//! - spawn_flush_task / spawn_flush_task_to_handle：定时 tick，
//!   Online → flush；Offline → run_probe_cycle（探测失败记次、
//!   恢复成功回 Online 并补 flush）
//! - convert_span_to_resource_spans：trace/span/parent id 映射、
//!   status(Unset/Ok/Error) + message、deployment_id/trace_state 属性、
//!   attribute bag 四种类型、重复 key 覆盖、scope/version 元数据
//! - ping_endpoint：空 endpoint / 成功延迟 / HTTP 错误码 / 连接失败

use std::sync::Arc;
use std::time::Duration;

use boom_core::OtlpConfig;
use boom_core::trace::{ProbeResult, SpanAttributeValue};
use boom_trace::otlp_export::TraceExporter;
use boom_trace::otlp_export::{convert_span_to_resource_spans, ping_endpoint};
use boom_trace::{parse_traceparent, RequestSpan};
use opentelemetry_proto::tonic::common::v1::any_value::Value as OtlpAnyValue;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

// ───────────────────────── helpers ─────────────────────────

const TP: &str = "00-0af76598164860bd9a43d7c1a31725ab-00f067aa0ba902b7-01";
const TP_ZERO_PARENT: &str = "00-0af76598164860bd9a43d7c1a31725ab-0000000000000000-01";

/// 带自定义 header / 小批量参数的配置。
fn config(endpoint: &str) -> OtlpConfig {
    let mut headers = std::collections::HashMap::new();
    headers.insert("X-Otlp-Key".to_string(), "dt-secret".to_string());
    OtlpConfig {
        endpoint: endpoint.to_string(),
        enabled: true,
        service_name: "boom-dt".to_string(),
        service_version: Some("v-dt".to_string()),
        timeout_secs: 2,
        batch_size: 64,
        flush_interval_secs: 1,
        max_queue_size: 1024,
        headers,
        ..OtlpConfig::default()
    }
}

fn make_span(req_id: &str) -> RequestSpan {
    let w3c = parse_traceparent(TP).unwrap();
    let mut span = RequestSpan::new(
        req_id.to_string(),
        &w3c,
        "gpt-4".to_string(),
        "/v1/chat/completions".to_string(),
        false,
        1_000_000_000,
    );
    span.finalize_ok(2_000_000_000);
    span
}

fn traces_ok() -> Mock {
    Mock::given(method("POST")).and(path("/v1/traces")).respond_with(ResponseTemplate::new(200))
}

fn traces_500() -> Mock {
    Mock::given(method("POST")).and(path("/v1/traces")).respond_with(ResponseTemplate::new(500))
}

/// 轮询直到 server 收到 ≥ n 个请求（超时 false）。
async fn wait_for_requests(server: &MockServer, n: usize, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if server.received_requests().await.unwrap().len() >= n {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.received_requests().await.unwrap().len() >= n
}

fn has_header(req: &wiremock::Request, name: &str) -> bool {
    req.headers.keys().any(|k| k.as_str().eq_ignore_ascii_case(name))
}

/// 可切换状态的响应器：fail=true → 500，false → 200（模拟端点故障/恢复，
/// 规避 wiremock 多 mock 优先级问题）。
struct ToggleRespond {
    fail: Arc<std::sync::atomic::AtomicBool>,
}

impl Respond for ToggleRespond {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            ResponseTemplate::new(500)
        } else {
            ResponseTemplate::new(200)
        }
    }
}

// ═════════════════════════════════════════════════════════════════
// 初始状态 / status_snapshot
// ═════════════════════════════════════════════════════════════════

/// DT-OTLP-01：new 初始 Online；snapshot 全字段（endpoint/None 时间戳/0 计数）。
#[test]
fn exporter_starts_online() {
    let exp = TraceExporter::new(&config("http://otlp.example"));
    assert!(!exp.is_offline());
    assert_eq!(exp.dropped_count_handle().load(std::sync::atomic::Ordering::Relaxed), 0);
    let snap = exp.status_snapshot();
    assert_eq!(snap.status, "online");
    assert_eq!(snap.endpoint, "http://otlp.example");
    assert_eq!(snap.last_failure_ts, None);
    assert_eq!(snap.last_recovery_ts, None);
    assert_eq!(snap.consecutive_probe_failures, 0);
    assert_eq!(snap.total_offline_episodes, 0);
    assert_eq!(snap.total_dropped_during_offline, 0);
    assert_eq!(snap.dropped_count, 0);
}

// ═════════════════════════════════════════════════════════════════
// flush：成功推送 / 空批早退 / header
// ═════════════════════════════════════════════════════════════════

/// DT-OTLP-02：enqueue 2 条 + flush → 一次 POST（自定义 header、protobuf body）；
/// 再 flush 空批 → 早退不多发。
#[tokio::test]
async fn flush_pushes_batch_with_headers() {
    let server = MockServer::start().await;
    traces_ok().mount(&server).await;
    let exp = TraceExporter::new(&config(&server.uri()));

    exp.enqueue(make_span("r-1")).await;
    exp.enqueue(make_span("r-2")).await;
    exp.flush().await;
    assert!(!exp.is_offline());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "one POST carries the whole batch");
    assert!(has_header(&reqs[0], "x-otlp-key"), "custom OTLP header attached");
    assert!(has_header(&reqs[0], "content-type"));
    assert!(!reqs[0].body.is_empty(), "protobuf body non-empty");

    // 空批 flush 早退（不产生新请求）
    exp.flush().await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// DT-OTLP-03：flush 连续失败（HTTP 500 ×3 重试）→ Offline；
/// episodes=1、批内 dropped；此后 enqueue 立即丢弃。
#[tokio::test]
async fn flush_failure_goes_offline_and_drops_later_enqueues() {
    let server = MockServer::start().await;
    traces_500().mount(&server).await;
    let exp = TraceExporter::new(&config(&server.uri()));

    exp.enqueue(make_span("r-1")).await;
    exp.flush().await;
    assert!(exp.is_offline(), "3 failed attempts must flip offline");

    let snap = exp.status_snapshot();
    assert_eq!(snap.status, "offline");
    assert_eq!(snap.total_offline_episodes, 1);
    assert_eq!(snap.dropped_count, 1, "drained batch counted as dropped");
    assert!(snap.last_failure_ts.is_some());
    assert_eq!(snap.last_recovery_ts, None);

    // Offline 期间 enqueue → 立即丢弃，不进队列
    exp.enqueue(make_span("r-2")).await;
    exp.enqueue(make_span("r-3")).await;
    let snap = exp.status_snapshot();
    assert_eq!(snap.total_dropped_during_offline, 2);
    assert_eq!(snap.dropped_count, 3);
    assert_eq!(exp.dropped_count_handle().load(std::sync::atomic::Ordering::Relaxed), 3);
}

// ═════════════════════════════════════════════════════════════════
// probe：恢复 / 失败计数
// ═════════════════════════════════════════════════════════════════

/// DT-OTLP-04：Offline 后端点恢复 → probe Ok → 回 Online；
/// 恢复后 enqueue/flush 恢复推送；episodes 不再增长。
#[tokio::test]
async fn probe_recovers_from_offline() {
    let server = MockServer::start().await;
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    Mock::given(method("POST")).and(path("/v1/traces"))
        .respond_with(ToggleRespond { fail: fail.clone() })
        .mount(&server).await;
    let exp = TraceExporter::new(&config(&server.uri()));

    exp.enqueue(make_span("r-1")).await;
    exp.flush().await;
    assert!(exp.is_offline());

    // 端点恢复 → probe 成功
    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    match exp.probe().await {
        ProbeResult::Ok { latency_ms } => assert!(latency_ms < 5_000),
        ProbeResult::Fail { error } => panic!("probe should succeed: {error}"),
    }
    assert!(!exp.is_offline());
    let snap = exp.status_snapshot();
    assert_eq!(snap.status, "online");
    assert_eq!(snap.total_offline_episodes, 1, "episode count is sticky");
    assert!(snap.last_recovery_ts.is_some());
    assert_eq!(snap.consecutive_probe_failures, 0, "recovery resets the streak");

    // Online 恢复后 enqueue/flush 正常
    let before = server.received_requests().await.unwrap().len();
    exp.enqueue(make_span("r-2")).await;
    exp.flush().await;
    assert!(wait_for_requests(&server, before + 1, 3_000).await);
}

/// DT-OTLP-05：probe 失败（连接拒绝）→ consecutive_probe_failures+1，
/// 但 Online 状态不变（probe 失败不触发 offline 转移）。
#[tokio::test]
async fn probe_failure_counts_streak_without_offline() {
    // 端口 1 无监听 → 立即 connection refused
    let exp = TraceExporter::new(&config("http://127.0.0.1:1"));
    match exp.probe().await {
        ProbeResult::Fail { error } => assert!(!error.is_empty()),
        ProbeResult::Ok { .. } => panic!("dead endpoint must fail"),
    }
    assert!(!exp.is_offline(), "probe failure alone stays online");
    let snap = exp.status_snapshot();
    assert_eq!(snap.consecutive_probe_failures, 1);
    assert!(snap.last_failure_ts.is_some());
}

// ═════════════════════════════════════════════════════════════════
// enqueue：队列满弹最旧 / 攒批触发后台 flush
// ═════════════════════════════════════════════════════════════════

/// DT-OTLP-06：max_queue_size=3、enqueue 5 条 → 弹掉最旧 2 条（dropped=2），
/// flush 只推一批。
#[tokio::test]
async fn queue_overflow_drops_oldest() {
    let server = MockServer::start().await;
    traces_ok().mount(&server).await;
    let cfg = OtlpConfig {
        batch_size: 1000, // 不触发攒批自动 flush
        max_queue_size: 3,
        ..config(&server.uri())
    };
    let exp = TraceExporter::new(&cfg);
    for i in 0..5 {
        exp.enqueue(make_span(&format!("r-{i}"))).await;
    }
    assert_eq!(exp.dropped_count_handle().load(std::sync::atomic::Ordering::Relaxed), 2);
    assert!(!exp.is_offline());

    exp.flush().await;
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "surviving 3 spans go out in one batch");
    assert!(!reqs[0].body.is_empty());
}

/// DT-OTLP-07：batch_size=2 → 第 2 条 enqueue 攒满批，后台任务自动 flush。
#[tokio::test]
async fn batch_full_triggers_background_flush() {
    let server = MockServer::start().await;
    traces_ok().mount(&server).await;
    let cfg = OtlpConfig {
        batch_size: 2,
        max_queue_size: 1024,
        ..config(&server.uri())
    };
    let exp = TraceExporter::new(&cfg);
    exp.enqueue(make_span("r-1")).await;
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "below batch size nothing is pushed"
    );
    exp.enqueue(make_span("r-2")).await;
    assert!(wait_for_requests(&server, 1, 3_000).await, "auto flush fires at batch boundary");
    assert_eq!(exp.dropped_count_handle().load(std::sync::atomic::Ordering::Relaxed), 0);
}

// ═════════════════════════════════════════════════════════════════
// 后台 tick 任务
// ═════════════════════════════════════════════════════════════════

/// DT-OTLP-08：spawn_flush_task 定时 flush 队列内容。
#[tokio::test]
async fn spawn_flush_task_flushes_on_tick() {
    let server = MockServer::start().await;
    traces_ok().mount(&server).await;
    let exp = TraceExporter::new(&config(&server.uri()));
    exp.enqueue(make_span("r-1")).await;

    let task = exp.spawn_flush_task();
    assert!(wait_for_requests(&server, 1, 3_500).await, "ticker must flush the queue");
    task.abort();
}

/// DT-OTLP-09：spawn_flush_task_to_handle —— Offline 态 tick 走探测：
/// 探测仍 500 → 记 failure；端点恢复后下一 tick 探测成功 → 回 Online。
#[tokio::test]
async fn spawn_flush_task_to_handle_probes_and_recovers() {
    let server = MockServer::start().await;
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    Mock::given(method("POST")).and(path("/v1/traces"))
        .respond_with(ToggleRespond { fail: fail.clone() })
        .mount(&server).await;
    let exp = TraceExporter::new(&config(&server.uri()));

    // 先转 Offline（drained 批计 dropped）
    exp.enqueue(make_span("r-1")).await;
    exp.flush().await;
    assert!(exp.is_offline());

    let handle = Arc::new(std::sync::Mutex::new(None));
    exp.spawn_flush_task_to_handle(handle.clone());

    // 首 tick 立即探测 → 仍 500 → consecutive_probe_failures ≥ 1 且保持 Offline
    let deadline = std::time::Instant::now() + Duration::from_millis(2_500);
    while std::time::Instant::now() < deadline
        && exp.status_snapshot().consecutive_probe_failures == 0
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(exp.status_snapshot().consecutive_probe_failures >= 1, "failed probe counted");
    assert!(exp.is_offline());

    // 端点恢复 → 下一 tick 探测成功回 Online
    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    let deadline = std::time::Instant::now() + Duration::from_millis(3_500);
    while std::time::Instant::now() < deadline && exp.is_offline() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!exp.is_offline(), "probe cycle must bring the exporter back");
    let snap = exp.status_snapshot();
    assert_eq!(snap.consecutive_probe_failures, 0);
    assert_eq!(snap.total_offline_episodes, 1);
    assert!(snap.last_recovery_ts.is_some());

    let h = handle.lock().unwrap().take();
    if let Some(h) = h {
        h.abort();
    }
}

// ═════════════════════════════════════════════════════════════════
// convert_span_to_resource_spans — 纯映射函数
// ═════════════════════════════════════════════════════════════════

fn attr<'a>(rs: &'a opentelemetry_proto::tonic::trace::v1::ResourceSpans, key: &str) -> Option<&'a OtlpAnyValue> {
    rs.scope_spans[0].spans[0]
        .attributes
        .iter()
        .find(|a| a.key == key)
        .and_then(|a| a.value.as_ref())
        .and_then(|v| v.value.as_ref())
}

fn resource_attr(rs: &opentelemetry_proto::tonic::trace::v1::ResourceSpans, key: &str) -> Option<String> {
    rs.resource
        .as_ref()?
        .attributes
        .iter()
        .find(|a| a.key == key)
        .and_then(|a| a.value.as_ref())
        .and_then(|v| v.value.as_ref())
        .map(|v| match v {
            OtlpAnyValue::StringValue(s) => s.clone(),
            _ => String::new(),
        })
}

fn empty_resource() -> opentelemetry_proto::tonic::resource::v1::Resource {
    Default::default()
}

/// DT-OTLP-10：Ok span → id/时间/属性全映射；Unset/Error 状态码；
/// 零 parent → 空 parent_span_id。
#[test]
fn convert_span_maps_ids_status_and_attributes() {
    let w3c = parse_traceparent(TP).unwrap();
    let mut span = RequestSpan::new(
        "r-1".into(),
        &w3c,
        "gpt-4".into(),
        "/v1/chat/completions".into(),
        true,
        1_000_000_000,
    );
    span.set_deployment_id("dep-7".into());
    span.trace_state = "vendor=1".to_string();
    span.set_attribute("k.str", SpanAttributeValue::String("v".into()));
    span.set_attribute("k.int", SpanAttributeValue::Int(7));
    span.set_attribute("k.bool", SpanAttributeValue::Bool(true));
    span.set_attribute("k.dup", SpanAttributeValue::Int(1));
    span.set_attribute("k.dup", SpanAttributeValue::Int(2)); // 覆盖旧值
    span.set_llm_request(Arc::new(serde_json::json!({"messages": [1]})));
    span.finalize_ok(2_000_000_000);

    let rs = convert_span_to_resource_spans(&span, &empty_resource());
    let scope = &rs.scope_spans[0];
    assert_eq!(scope.scope.as_ref().unwrap().name, "boom-trace");
    let s = &scope.spans[0];
    assert_eq!(s.name, "boom-gateway.request");
    assert_eq!(s.kind, 2); // SERVER
    assert_eq!(s.flags, 1); // sampled
    assert_eq!(s.trace_id.len(), 16);
    assert_eq!(s.trace_id[0], 0x0a);
    assert_eq!(s.span_id.len(), 8);
    assert_eq!(s.parent_span_id.len(), 8);
    assert_eq!(s.trace_state, "vendor=1");
    assert_eq!(s.start_time_unix_nano, 1_000_000_000);
    assert_eq!(s.end_time_unix_nano, 2_000_000_000);
    assert_eq!(s.status.as_ref().unwrap().code, 1); // Ok

    // 标准属性
    match attr(&rs, "url.path") {
        Some(OtlpAnyValue::StringValue(v)) => assert_eq!(v, "/v1/chat/completions"),
        other => panic!("url.path wrong: {other:?}"),
    }
    match attr(&rs, "boom-gateway.request.id") {
        Some(OtlpAnyValue::StringValue(v)) => assert_eq!(v, "r-1"),
        other => panic!("request.id wrong: {other:?}"),
    }
    match attr(&rs, "boom-gateway.model") {
        Some(OtlpAnyValue::StringValue(v)) => assert_eq!(v, "gpt-4"),
        other => panic!("model wrong: {other:?}"),
    }
    assert!(matches!(attr(&rs, "boom-gateway.is_stream"), Some(OtlpAnyValue::BoolValue(true))));
    assert!(matches!(attr(&rs, "boom-gateway.deployment_id"),
        Some(OtlpAnyValue::StringValue(v)) if v == "dep-7"));
    assert!(matches!(attr(&rs, "boom-gateway.trace_state"),
        Some(OtlpAnyValue::StringValue(v)) if v == "vendor=1"));

    // attribute bag 四类 + 覆盖语义
    assert!(matches!(attr(&rs, "k.str"), Some(OtlpAnyValue::StringValue(v)) if v == "v"));
    assert!(matches!(attr(&rs, "k.int"), Some(OtlpAnyValue::IntValue(7))));
    assert!(matches!(attr(&rs, "k.bool"), Some(OtlpAnyValue::BoolValue(true))));
    assert!(matches!(attr(&rs, "k.dup"), Some(OtlpAnyValue::IntValue(2))));
    match attr(&rs, "boom-gateway.llm_request") {
        Some(OtlpAnyValue::StringValue(v)) => assert!(v.contains("messages"), "json body as string: {v}"),
        other => panic!("llm_request wrong: {other:?}"),
    }

    // 零 parent → 空 parent_span_id
    let w3c_zero = parse_traceparent(TP_ZERO_PARENT).unwrap();
    let span_zero = RequestSpan::new("r-z".into(), &w3c_zero, "m".into(), "/p".into(), false, 0);
    let rs_zero = convert_span_to_resource_spans(&span_zero, &empty_resource());
    assert!(rs_zero.scope_spans[0].spans[0].parent_span_id.is_empty());
    // Unset 状态 + 无 deployment/trace_state 属性
    assert_eq!(rs_zero.scope_spans[0].spans[0].status.as_ref().unwrap().code, 0);
    assert!(attr(&rs_zero, "boom-gateway.deployment_id").is_none());
    assert!(attr(&rs_zero, "boom-gateway.trace_state").is_none());

    // Error 状态 → code 2 + message
    let mut span_err = RequestSpan::new("r-e".into(), &w3c, "m".into(), "/p".into(), false, 0);
    span_err.finalize_error(9, "boom".into());
    let rs_err = convert_span_to_resource_spans(&span_err, &empty_resource());
    let st = rs_err.scope_spans[0].spans[0].status.as_ref().unwrap();
    assert_eq!(st.code, 2);
    assert_eq!(st.message, "boom");
}

/// DT-OTLP-11：resource 挂载 service.name / service.version（Some/None 两态）。
#[test]
fn convert_span_carries_resource_metadata() {
    let w3c = parse_traceparent(TP).unwrap();
    let span = RequestSpan::new("r".into(), &w3c, "m".into(), "/p".into(), false, 0);

    let mut res = empty_resource();
    res.attributes.push(opentelemetry_proto::tonic::common::v1::KeyValue {
        key: "service.name".into(),
        value: Some(opentelemetry_proto::tonic::common::v1::AnyValue {
            value: Some(OtlpAnyValue::StringValue("boom-dt".into())),
        }),
        ..Default::default()
    });
    let rs = convert_span_to_resource_spans(&span, &res);
    assert_eq!(resource_attr(&rs, "service.name").as_deref(), Some("boom-dt"));
    assert!(rs.resource.is_some());
}

// ═════════════════════════════════════════════════════════════════
// ping_endpoint
// ═════════════════════════════════════════════════════════════════

/// DT-OTLP-12：ping_endpoint —— 空 endpoint / 成功 / HTTP 500 / 连接失败。
#[tokio::test]
async fn ping_endpoint_matrix() {
    // 空 endpoint
    let err = ping_endpoint(&config("")).await.err().expect("empty endpoint");
    assert!(err.contains("endpoint not configured"), "{err}");

    // 成功 → 延迟毫秒
    let server = MockServer::start().await;
    traces_ok().mount(&server).await;
    let ms = ping_endpoint(&config(&server.uri())).await.expect("ping ok");
    assert!(ms < 5_000);

    // HTTP 500
    let bad = MockServer::start().await;
    traces_500().mount(&bad).await;
    let err = ping_endpoint(&config(&bad.uri())).await.err().expect("500 fails");
    assert!(err.contains("HTTP 500"), "{err}");

    // 连接失败
    let err = ping_endpoint(&config("http://127.0.0.1:1")).await.err().expect("refused");
    assert!(!err.is_empty());
}
