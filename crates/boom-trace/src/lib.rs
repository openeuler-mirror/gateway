//! # boom-trace — 请求级链路与延迟分布的 trace 数据中枢
//!
//! ## 这个模块管什么
//!
//! **以 trace 为核心抽象**：一条客户端请求从入站到响应出站的全过程，
//! 被记录为一条 `RequestSpan`（父 span）+ 若干子 span（每个上游 provider
//! 调用、fusion 子调用、flow control 排队等阶段一个）。基于这条 trace
//! 数据，本模块衍生出多种应用：
//!
//! - **链路跟踪**：父/子 span 树，定位慢请求瓶颈在哪个阶段
//! - **延迟分布**：按 (model, deployment_id, is_stream) 维度的 HDR 直方图
//!   — TTFT / E2E / queue_wait / upstream_time 的 p50/p95/p99/p999
//! - **慢请求 ring buffer**：最近 N 条慢请求（duration > 阈值）的 trace
//!   快照，供 dashboard 直接拉列表
//! - **可视化 snapshot**：dashboard 通过 `Arc<dyn TraceApi>` 读取聚合数据
//!   不依赖本 crate 的具体类型
//! - **OTLP traces 导出**：把 span 树导出为 OpenTelemetry traces
//!   （区别于 boom-promptlog 已有的 OTLP logs 导出）
//!
//! `trace` 是数据/能力集合名词，不是工具名 — 与 `boom-audit`/`boom-fusion`
//! 命名风格一致。本模块不限定 trace 的应用方式：可观测是 trace 的一种
//! 应用，调试、回放、合规留痕、SLA 报表都是基于 trace 的应用。
//!
//! ## 职责边界（CLAUDE.md §2）
//!
//! **管：**
//! - `RequestSpan` 数据结构 + 父子关系
//! - HDR 延迟直方图（in-memory，跨 reload 存活）
//! - 慢请求 ring buffer（固定容量，参考 boom-stressmon）
//! - `TraceApi` trait + snapshot 类型（impl 在本 crate，trait 放 boom-core）
//! - OTLP traces 导出器（feature-gated）
//!
//! **不管：**
//! - 不写 DB 表 — 持久化数据由 boom-audit 的 `boom_request_log` 担任；
//!   本模块只在启动时从该表回填历史 N 天到 in-memory 直方图（只读）
//! - 不记录 prompt 内容 — 那是 boom-promptlog 的职责；本模块的 span
//!   只记时间维度 + 元数据，不记请求/响应 body
//! - 不做路由决策 — 不感知 Provider 具体实现
//! - 不做 HTTP 转发 — 不在 handler 里写业务逻辑
//!
//! ## 模块依赖方向（CLAUDE.md §1）
//!
//! ```text
//! boom-core ← boom-trace
//! boom-main → boom-trace（组装层，注入 span 调用点）
//! boom-dashboard → boom-core（只读 Arc<dyn TraceApi>，不依赖本 crate）
//! ```
//!
//! - **boom-trace 是 leaf crate**：只依赖 boom-core，不依赖 boom-provider /
//!   boom-routing / boom-config / boom-dashboard / boom-promptlog，避免循环依赖
//! - **trait 放 boom-core**：`TraceApi` + `RequestSpan` 等共享类型定义在
//!   `boom-core::trace`，让 boom-dashboard 可以 `Arc<dyn TraceApi>` 消费
//!   而不依赖本 crate（与 `StressmonApi` 同款 §5 模式）
//! - **`OtlpConfig` 也放 boom-core**：让 logs 和 traces 两个通道共用同一份
//!   配置类型，boom-promptlog 现在只 re-export boom-core 的版本
//! - **boom-main 是组装层**：在 routes.rs 的关键阶段（auth 完成、provider
//!   调用前后、stream Drop）调 `trace.record_*`，把 span 数据喂进本模块
//!
//! ## 状态生命周期（CLAUDE.md §4）
//!
//! 本模块的 `TraceRegistry` 放 `AppState` 顶层 Arc，**跨 reload 存活**：
//!
//! ```ignore
//! AppState
//!   └─ trace: Arc<boom_trace::TraceRegistry>  // 跨 reload 存活
//! ```
//!
//! - 活跃 span 表、慢请求 ring buffer 是运行时累积的状态，不受 SIGHUP 热加载
//!   影响（与 inflight / stressmon / agent_stats 同款生命周期）
//! - OTLP exporter 通过 `Arc<ArcSwap<Option<Arc<TraceExporter>>>>` 持有，
//!   热加载时 `replace_otlp` abort 旧 flush task → best-effort flush →
//!   构造新 exporter → store → spawn 新 flush task（与 promptlog 同款）
//! - 配置项（OTLP endpoint、ring 容量、慢请求阈值）走 boom-config 的
//!   `TraceConfig`，热加载只重建 exporter / 调阈值；活跃 span 不丢
//!
//! ## 与周边模块的关系
//!
//! | 模块 | 关系 |
//! |------|------|
//! | boom-core | 持有 `TraceApi` trait + `RequestSpan` + `OtlpConfig` 等共享类型 |
//! | boom-audit | 本模块在启动时只读 `boom_request_log` 回填历史百分位；不写表 |
//! | boom-promptlog | 互补：promptlog 管 prompt 内容 + OTLP logs 导出；本模块管 trace 链路 + OTLP traces 导出。body 通过 `Arc<serde_json::Value>` 共享单份内存 |
//! | boom-stressmon | 互补：stressmon 是机器压力（1Hz 采样）；本模块是请求级延迟（per-request span） |
//! | boom-provider / boom-fusion | 不直接依赖；gateway span 通过 `gateway_headers` 注入 `traceparent` 头，provider 已有 header 注入循环（openai.rs:89 / anthropic.rs:539）零改动应用 |
//! | boom-dashboard | 通过 `Arc<dyn TraceApi>` 只读 snapshot；不依赖本 crate |
//! | boom-main | 组装层，在 routes.rs 关键阶段调 `start_request` / `with_span_mut` / `finalize_*` |

pub mod context;
pub mod guard;
#[cfg(feature = "otlp")]
pub mod otlp_export;
pub mod registry;
pub mod span;

pub use context::{parse_traceparent, build_child_traceparent, W3cContext};
pub use guard::{trace_filter_matches, TraceGuard};
#[cfg(feature = "otlp")]
pub use otlp_export::{convert_span_to_resource_spans, ping_endpoint, TraceExporter};
pub use registry::TraceRegistry;
pub use span::RequestSpan;

// Re-export boom-core types consumers expect from this crate.
pub use boom_core::trace::{
    ExporterStatusSnapshot, ProbeResult, SpanStatus, TraceApi, TraceSnapshot,
};
pub use boom_core::OtlpConfig;
