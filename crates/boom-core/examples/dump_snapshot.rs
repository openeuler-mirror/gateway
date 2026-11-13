//! One-off: drive RollbackStats through its composite event methods (the
//! ONLY public recording surface) for a coherent set of requests, then dump
//! the snapshot JSON. Both invariants must hold:
//!   ① identify_total = miss + inert + no_target + selected
//!   ② selected = retry_success + retry_error + guard_error
//! Output is the fixture for scripts/smoke_frontend.js — regenerate there
//! after any change:
//!   cargo run -p boom-core --example dump_snapshot
fn main() {
    use boom_core::GatewayError;
    let stats = boom_core::context_error::RollbackStats::new();
    let m = "glm-5.3";

    // req1: context error NOT identified → miss
    let e_miss = GatewayError::UpstreamError {
        status: 400,
        message: r#"{"error":{"code":"WeirdModelError","message":"odd"}}"#.into(),
    };
    stats.record_miss_identified(m, &e_miss);

    // req2: identified but pool not capable → inert (routes.rs else branch;
    // bumps identify_total + pool_inert + per-model + code internally)
    let e_ctx = GatewayError::UpstreamError {
        status: 400,
        message: r#"{"error":{"code":"InputTokenExceeded","message":"too long"}}"#.into(),
    };
    stats.record_pool_inert_identified(m, &e_ctx);

    // req3: capable but no satisfying tier → no_target
    stats.record_no_target_identified(m, &e_ctx);

    // req4: rollback attempted and succeeded (fixed the e_ctx error)
    stats.record_selected(m);
    stats.record_retry_success_fixed(m, &e_ctx, 42);

    // req5: rollback attempted, retry call itself failed
    let e_retry = GatewayError::UpstreamError {
        status: 500,
        message: "internal".into(),
    };
    stats.record_selected(m);
    stats.record_retry_error(m, &e_retry);

    // req6: rollback attempted, guard swap failed (terminal — no retry
    // call, no retry_error double-count, invariant ②)
    let e_guard = GatewayError::FlowControlQueueTimeout {
        deployment_id: "d1".into(),
        waiters: 3,
        message: "queue full".into(),
    };
    stats.record_selected(m);
    stats.record_guard_error(m, &e_guard);

    // req7: 502→400 passthrough (finalize.rs; outside both invariants)
    stats.record_no_fallback_return(Some(m), &e_ctx);

    // first-try direct hit (success metric, outside both invariants)
    stats.record_affinity_direct(m);

    println!(
        "{}",
        serde_json::to_string_pretty(&stats.snapshot()).unwrap()
    );
}
