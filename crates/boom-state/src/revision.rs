use std::collections::HashMap;

use sqlx::PgPool;

/// Plan definitions changed (`boom_rate_limit_plan`).
pub const CATEGORY_PLAN: &str = "plan";
/// Key/team → plan assignments changed (`boom_key_plan_assignment`,
/// `boom_team_plan_assignment`).
pub const CATEGORY_ASSIGNMENT: &str = "assignment";
/// Client blocklist rules or the enabled switch changed
/// (`boom_client_block_rule`, `boom_client_blocklist_state`).
pub const CATEGORY_BLOCKLIST: &str = "blocklist";
/// Verification-token rows changed — other instances must drop their auth
/// cache (`boom_verification_token` writes).
pub const CATEGORY_AUTH: &str = "auth";
/// Quota decision-base switch flipped (`boom_config` KV
/// `decision_base_shared`). Covers only the shared/local enforcement view —
/// the counter deltas themselves need no broadcast (they converge through
/// the incremental push protocol).
pub const CATEGORY_QUOTA: &str = "quota";
/// Routing-plane config changed (`boom_model_deployment`, `boom_model_alias`,
/// `boom_workflow_definition`/`boom_workflow_model`, `boom_cost_template`).
/// One category for all four stores: the reconcile is a cheap idempotent
/// diff, and ordering dependencies (cost templates feed deployment cost
/// derivation) make a combined reconcile the safe shape. Runtime health
/// writes (`auto_disable_db`/`auto_enable_db`) deliberately do NOT bump —
/// health is per-instance connectivity state, not config.
pub const CATEGORY_ROUTING: &str = "routing";
/// user_tag header list changed (`boom_user_tag_config` single row).
pub const CATEGORY_USER_TAG: &str = "user_tag";
/// Cluster scheduling policy group changed (`boom_router_policy` single
/// row: schedule_policy / key_affinity_* / rebalance_threshold /
/// kvc router_ttl_secs / kvc max_blocks). Distinct from CATEGORY_ROUTING
/// (model/alias/workflow/cost content): a policy flip also rebuilds the
/// scheduling policy object and — on schedule_policy changes — the KV
/// trie, so it gets its own reconcile arm. Non-flip changes (including
/// max_blocks) preserve the learned trie: the capacity live-resizes via
/// `KvIndexBackend::set_block_capacity`.
pub const CATEGORY_ROUTER_POLICY: &str = "router_policy";
/// Router behavior trio changed (`boom_config` KV `router_behavior`:
/// enable_priority_header / strip_claude_code_attribution /
/// forward_client_headers). Cluster forwarding semantics — instances
/// diverging on these would give clients different header treatment
/// depending on which instance served the request. Re-adopt is a plain
/// inner-config swap (the fields are read per-request); no policy/trie
/// rebuild involved, hence its own category distinct from
/// CATEGORY_ROUTER_POLICY.
pub const CATEGORY_ROUTER_BEHAVIOR: &str = "router_behavior";
/// Trace observability switches changed (`boom_config` KV
/// `trace_metrics_enabled`).
pub const CATEGORY_TRACE: &str = "trace";

/// Atomically increment a category's revision counter.
///
/// Call AFTER the business write completes (sequential await ordering:
/// readers that observe the new revision are guaranteed to see the
/// committed data). GaussDB-compatible upsert; `revision = revision + 1`
/// is atomic at row level so concurrent bumps never lose a count.
///
/// A crash between business write and bump only delays propagation until
/// the next bump of that category (or the next periodic full reconcile,
/// which re-asserts every category regardless of the counters) — the
/// local instance already applied the change in memory, only peers lag.
pub async fn bump(pool: &PgPool, category: &str) -> Result<(), sqlx::Error> {
    boom_core::gaussdb_upsert!(
        pool,
        || sqlx::query(
            r#"UPDATE boom_config_revision
               SET revision = revision + 1, updated_at = NOW()
               WHERE category = $1"#,
        )
        .bind(category),
        || sqlx::query(
            r#"INSERT INTO boom_config_revision (category, revision)
               VALUES ($1, 1)"#,
        )
        .bind(category)
    )
}

/// Current revision counters. Watchers diff this against their last-seen
/// snapshot; any category whose counter moved triggers a reconcile of
/// that category (idempotent — a redundant trigger is harmless).
pub async fn snapshot(pool: &PgPool) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows =
        sqlx::query_as::<_, (String, i64)>(r#"SELECT category, revision FROM boom_config_revision"#)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

/// Bump after a successful business write; log-and-continue on failure.
/// The business write and local memory update already succeeded — a
/// failed bump only delays peer propagation until the next bump.
pub async fn bump_best_effort(pool: &PgPool, category: &str) {
    if let Err(e) = bump(pool, category).await {
        tracing::warn!(error = %e, category, "revision bump failed — peer propagation delayed");
    }
}
