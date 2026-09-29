//! DT 用例 — boom-routing：路由决策中枢纯内存逻辑。
//!
//! 覆盖（全部纯内存，跳过 *_db 方法——无 Postgres）：
//! - AliasStore：set_alias/remove/resolve/is_hidden/all_aliases/visible_names/clear/len
//! - DeploymentStore：set/add/exclusive/select(RR)/get_providers/contains/count/len/
//!   find_model_by_deployment_id/remove_deployment_by_deployment_id/quota_ratio/
//!   cost_rate/visibility/team_can_access/clear
//! - ModelCostRate：compute_cost（含 cached 折扣分支）/is_zero
//! - visibility_from_db / parse_allowed_teams / parse_visibility_str
//! - InFlightTracker + InFlightGuard（RAII，model + deployment 双层）
//! - RebalanceCounter（ring buffer 60 桶 + advance 清理）/ RebalanceMoveTracker
//! - RequestRateTracker（record/snapshot_all/remove + _total 聚合）
//! - MlServiceStats（attempt/success/failure/fallback + min/max CAS + tier 分布）
//! - AutoRouter / TierClassifier / StrategyRegistry（内容分类）
//! - Router：resolve_model_name/candidates_for/select_with_candidates/visible_model_names/
//!   team_can_access/is_*_model/policy_name/set_policy
//! - Policy：RoundRobinPolicy / ShufflePolicy / KeyAffinityPolicy / KvcAwarePolicy
//! - load_helpers：should_rebalance / deployment_load / min_load_candidate
//! - migrations：DDL 字符串包含关键列

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use boom_core::kv_event::{GatewayKvEvent, KvIndexBackend, StorageTier};
use boom_core::provider::{DeploymentQueueInfo, Provider};
use boom_core::types::{
    ChatCompletionRequest, ChatCompletionResponse, ChatStream, Message, MessageContent,
    MessageRole, Tool,
};
use boom_core::GatewayError;
use boom_kvindex::TokenPrefixIndex;
use boom_routing::{
    AliasStore, AutoRouter, DeploymentStore, InFlightGuard, InFlightTracker, KeyAffinityPolicy,
    KvcAwarePolicy, ModelCostRate, MlServiceStats, RequestRateTracker, RebalanceCounter,
    RebalanceMoveTracker, Router, RoundRobinPolicy, SchedulePolicy,
    ShufflePolicy, TierClassifier, VisibilityState,
};
use boom_routing::auto_router::{ClassifyRequest, ClassificationStrategy, StrategyRegistry};
use boom_routing::deployment_store::{
    parse_allowed_teams, visibility_from_db,
};
use rust_decimal::Decimal;

// ───────────────────────── mock provider ─────────────────────────

/// 可配置的 mock provider：name / deployment_id / kv_worker_id / models。
/// chat/chat_stream 不会被路由测试调用，返回 Err。
struct MockProvider {
    name: String,
    deployment_id: Option<String>,
    kv_worker_id: Option<String>,
    models: Vec<String>,
}

impl MockProvider {
    fn new(name: &str, deployment_id: &str) -> Self {
        Self {
            name: name.to_string(),
            deployment_id: Some(deployment_id.to_string()),
            kv_worker_id: Some(deployment_id.to_string()),
            models: vec![],
        }
    }
    fn no_kv(name: &str, deployment_id: &str) -> Self {
        Self {
            name: name.to_string(),
            deployment_id: Some(deployment_id.to_string()),
            kv_worker_id: None,
            models: vec![],
        }
    }
}

impl Provider for MockProvider {
    fn name(&self) -> &str { &self.name }
    fn models(&self) -> &[String] { &self.models }
    fn deployment_id(&self) -> Option<&str> { self.deployment_id.as_deref() }
    fn kv_worker_id(&self) -> Option<&str> { self.kv_worker_id.as_deref() }

    fn chat<'life0, 'async_trait>(
        &'life0 self,
        _request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send + 'async_trait>>
    where
        Self: 'async_trait,
        'life0: 'async_trait,
    {
        Box::pin(async { Err(GatewayError::ConfigError("mock".into())) })
    }
    fn chat_stream<'life0, 'async_trait>(
        &'life0 self,
        _request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChatStream, GatewayError>> + Send + 'async_trait>>
    where
        Self: 'async_trait,
        'life0: 'async_trait,
    {
        Box::pin(async { Err(GatewayError::ConfigError("mock".into())) })
    }
}

fn p(name: &str, did: &str) -> Arc<dyn Provider> {
    Arc::new(MockProvider::new(name, did))
}
fn p_no_kv(name: &str, did: &str) -> Arc<dyn Provider> {
    Arc::new(MockProvider::no_kv(name, did))
}

/// mock DeploymentQueueInfo：total_load / max_capacity 用内部 DashMap 模拟。
struct MockQueueInfo {
    loads: std::sync::Mutex<std::collections::HashMap<String, (u64, u32)>>,
}
impl MockQueueInfo {
    fn new() -> Self { Self { loads: std::sync::Mutex::new(std::collections::HashMap::new()) } }
    fn set(&self, dep_id: &str, load: u64, cap: u32) {
        self.loads.lock().unwrap().insert(dep_id.to_string(), (load, cap));
    }
}
impl DeploymentQueueInfo for MockQueueInfo {
    fn total_load(&self, deployment_id: &str) -> u64 {
        self.loads.lock().unwrap().get(deployment_id).map(|(l, _)| *l).unwrap_or(0)
    }
    fn max_capacity(&self, deployment_id: &str) -> u32 {
        self.loads.lock().unwrap().get(deployment_id).map(|(_, c)| *c).unwrap_or(0)
    }
}

// ───────────────────────── KV event helper ─────────────────────────

const BLOCK: usize = 4;

fn store_event(model: &str, worker: &str, parent_hash: Option<u64>, bytes: Vec<u8>) -> GatewayKvEvent {
    GatewayKvEvent::Store {
        model: model.to_string(),
        worker_id: worker.to_string(),
        sequence_hash: String::new(),
        prefix_hash: String::new(),
        local_hash: 0,
        parent_hash,
        block_index: 0,
        block_bytes: bytes,
        block_size: BLOCK as u32,
        storage_tier: StorageTier::Gpu,
    }
}

fn fresh_index() -> Arc<TokenPrefixIndex> {
    Arc::new(TokenPrefixIndex::new(BLOCK, 500_000))
}

// ═════════════════════════════════════════════════════════════════
// AliasStore
// ═════════════════════════════════════════════════════════════════

/// DT-RT-01：new/default 空存储；resolve 未知名返回 None。
#[test]
fn alias_store_new_empty() {
    let s = AliasStore::new();
    assert_eq!(s.len(), 0);
    assert_eq!(s.hidden_count(), 0);
    assert!(s.resolve("nope").is_none());
    assert!(s.all_aliases().is_empty());
    assert!(s.visible_names().is_empty());
    let def: AliasStore = AliasStore::default();
    assert_eq!(def.len(), 0);
}

/// DT-RT-02：set_alias 非 hidden → resolve 命中、visible_names 含之、is_hidden=false。
#[test]
fn alias_set_visible() {
    let s = AliasStore::new();
    s.set_alias("gpt4-mini".into(), "gpt-4o-mini".into(), false);
    assert_eq!(s.resolve("gpt4-mini").as_deref(), Some("gpt-4o-mini"));
    assert!(!s.is_hidden("gpt4-mini"));
    assert!(s.visible_names().contains(&"gpt4-mini".to_string()));
    assert_eq!(s.len(), 1);
}

/// DT-RT-03：set_alias hidden=true → is_hidden=true、visible_names 不含、但 resolve 仍命中。
#[test]
fn alias_set_hidden_excluded_from_visible() {
    let s = AliasStore::new();
    s.set_alias("secret".into(), "real-model".into(), true);
    assert!(s.is_hidden("secret"));
    assert_eq!(s.resolve("secret").as_deref(), Some("real-model"));
    assert!(!s.visible_names().contains(&"secret".to_string()));
    assert_eq!(s.hidden_count(), 1);
}

/// DT-RT-04：set_alias 把已 hidden 的别名重新设为 visible → hidden 集合移除。
#[test]
fn alias_unhide_via_set() {
    let s = AliasStore::new();
    s.set_alias("a".into(), "t".into(), true);
    assert_eq!(s.hidden_count(), 1);
    s.set_alias("a".into(), "t2".into(), false);
    assert_eq!(s.hidden_count(), 0);
    assert!(!s.is_hidden("a"));
    assert_eq!(s.resolve("a").as_deref(), Some("t2"));
}

/// DT-RT-05：remove_alias 返回 true 并清除；不存在返回 false。
#[test]
fn alias_remove() {
    let s = AliasStore::new();
    s.set_alias("a".into(), "t".into(), true);
    assert!(s.remove_alias("a"));
    assert!(s.resolve("a").is_none());
    assert!(!s.is_hidden("a"));
    assert!(!s.remove_alias("ghost"));
}

/// DT-RT-06：all_aliases 返回全部（含 hidden）；clear 清空全部。
#[test]
fn alias_all_and_clear() {
    let s = AliasStore::new();
    s.set_alias("a".into(), "t1".into(), false);
    s.set_alias("b".into(), "t2".into(), true);
    let all = s.all_aliases();
    assert_eq!(all.len(), 2);
    s.clear();
    assert_eq!(s.len(), 0);
    assert_eq!(s.hidden_count(), 0);
}

// ═════════════════════════════════════════════════════════════════
// DeploymentStore — 纯状态方法
// ═════════════════════════════════════════════════════════════════

/// DT-RT-07：new/default 空存储；contains/len/total_deployments 全 0。
#[test]
fn store_new_empty() {
    let s = DeploymentStore::new();
    assert_eq!(s.len(), 0);
    assert_eq!(s.total_deployments(), 0);
    assert!(!s.contains("m"));
    assert_eq!(s.deployment_count("m"), 0);
    assert!(s.model_names().is_empty());
    let def: DeploymentStore = DeploymentStore::default();
    assert_eq!(def.len(), 0);
}

/// DT-RT-08：set_deployments 替换整组，deployment_count/contains/total 反映。
#[test]
fn store_set_deployments() {
    let s = DeploymentStore::new();
    assert!(s.set_deployments("m".into(), vec![p("a", "d1"), p("a", "d2")]));
    assert!(s.contains("m"));
    assert_eq!(s.deployment_count("m"), 2);
    assert_eq!(s.total_deployments(), 2);
    assert_eq!(s.len(), 1);
    assert!(s.model_names().contains(&"m".to_string()));
    // 替换（覆盖）
    assert!(s.set_deployments("m".into(), vec![p("a", "d3")]));
    assert_eq!(s.deployment_count("m"), 1);
}

/// DT-RT-09：set_deployments 对空 provider 列表 → contains=true 但 count=0（空组保留以抑制通配回退）。
#[test]
fn store_set_empty_list_keeps_key() {
    let s = DeploymentStore::new();
    assert!(s.set_deployments("m".into(), vec![]));
    assert!(s.contains("m"));
    assert_eq!(s.deployment_count("m"), 0);
}

/// DT-RT-10：add_deployment 增量追加到现有组或新建组。
#[test]
fn store_add_deployment() {
    let s = DeploymentStore::new();
    assert!(s.add_deployment("m", p("a", "d1")));
    assert!(s.add_deployment("m", p("a", "d2")));
    assert_eq!(s.deployment_count("m"), 2);
    // 新组
    assert!(s.add_deployment("n", p("a", "d3")));
    assert_eq!(s.len(), 2);
}

/// DT-RT-11：set_exclusive_deployment 注册独占模型；再次 set/add/remove 被拒。
#[test]
fn store_exclusive_deployment() {
    let s = DeploymentStore::new();
    s.set_exclusive_deployment("vip".into(), p("a", "d1")).unwrap();
    assert!(s.is_exclusive_model("vip"));
    assert_eq!(s.deployment_count("vip"), 1);
    // 冲突：同名再注册 → Err
    assert!(s.set_exclusive_deployment("vip".into(), p("a", "d2")).is_err());
    // 已有 deployment 的名字注册独占 → Err
    s.set_deployments("normal".into(), vec![p("a", "d9")]);
    assert!(s.set_exclusive_deployment("normal".into(), p("a", "dx")).is_err());
    // 对独占模型 set_deployments → 被拒返回 false
    assert!(!s.set_deployments("vip".into(), vec![p("a", "d2")]));
    // 对独占模型 add_deployment → 被拒
    assert!(!s.add_deployment("vip", p("a", "d2")));
    // 对独占模型 remove_deployments → 被拒
    assert!(!s.remove_deployments("vip"));
}

/// DT-RT-12：remove_deployments 普通模型 → true 并清除；不存在 → false。
#[test]
fn store_remove_deployments() {
    let s = DeploymentStore::new();
    s.set_deployments("m".into(), vec![p("a", "d1")]);
    assert!(s.remove_deployments("m"));
    assert!(!s.contains("m"));
    assert!(!s.remove_deployments("ghost"));
}

/// DT-RT-13：find_model_by_deployment_id 反查；remove_deployment_by_deployment_id 跨组清除。
#[test]
fn store_find_and_remove_by_deployment_id() {
    let s = DeploymentStore::new();
    s.set_deployments("m1".into(), vec![p("a", "d1"), p("a", "d2")]);
    s.set_deployments("m2".into(), vec![p("a", "d2")]); // d2 同时在两组
    assert_eq!(s.find_model_by_deployment_id("d1").as_deref(), Some("m1"));
    // d2 在两组都出现，find 返回首个
    assert!(matches!(s.find_model_by_deployment_id("d2").as_deref(), Some("m1") | Some("m2")));
    assert!(s.find_model_by_deployment_id("ghost").is_none());

    s.remove_deployment_by_deployment_id("d2");
    assert_eq!(s.deployment_count("m1"), 1); // d2 从 m1 移除
    assert_eq!(s.deployment_count("m2"), 0); // d2 从 m2 移除（空组保留）
}

/// DT-RT-14：select 空组返回 None；单 provider 直接返回；多 provider 轮询。
#[test]
fn store_select_round_robin() {
    let s = DeploymentStore::new();
    assert!(s.select("ghost").is_none());

    s.set_deployments("single".into(), vec![p("a", "d1")]);
    let r0 = s.select("single").unwrap();
    assert_eq!(r0.deployment_id(), Some("d1"));

    s.set_deployments("multi".into(), vec![p("a", "d1"), p("a", "d2"), p("a", "d3")]);
    // 轮询：连续 select 应依次走 d1,d2,d3,d1...
    let ids: Vec<String> = (0..4).filter_map(|_| s.select("multi").and_then(|x| x.deployment_id().map(|s| s.to_string()))).collect();
    assert_eq!(ids, vec!["d1", "d2", "d3", "d1"]);
}

/// DT-RT-15：get_providers 返回克隆的 Vec；model_names 列出全部 key。
#[test]
fn store_get_providers_and_names() {
    let s = DeploymentStore::new();
    s.set_deployments("m".into(), vec![p("a", "d1")]);
    s.set_deployments("n".into(), vec![p("a", "d2")]);
    assert_eq!(s.get_providers("m").unwrap().len(), 1);
    assert!(s.get_providers("ghost").is_none());
    let names = s.model_names();
    assert_eq!(names.len(), 2);
}

// ═════════════════════════════════════════════════════════════════
// DeploymentStore — quota / cost / visibility
// ═════════════════════════════════════════════════════════════════

/// DT-RT-16：quota_ratio 默认 1；set_quota_ratio 覆盖。
#[test]
fn store_quota_ratio() {
    let s = DeploymentStore::new();
    assert_eq!(s.get_quota_ratio("m"), 1);
    s.set_quota_ratio("m", 5);
    assert_eq!(s.get_quota_ratio("m"), 5);
}

/// DT-RT-17：cost_rate 默认全零；set_cost_rate 存非零；set 零费率 → 移除条目。
#[test]
fn store_cost_rate() {
    let s = DeploymentStore::new();
    assert!(s.get_cost_rate("m").is_zero());
    s.set_cost_rate("m", ModelCostRate::new(Decimal::from(2), Decimal::from(6)));
    let r = s.get_cost_rate("m");
    assert!(!r.is_zero());
    assert_eq!(r.input_cost_per_token, Decimal::from(2));
    // set 零费率 → 移除
    s.set_cost_rate("m", ModelCostRate::default());
    assert!(s.get_cost_rate("m").is_zero());
}

/// DT-RT-18：set_visibility Normal 清除条目；Public/Private 设置；visibility 读取。
#[test]
fn store_visibility_states() {
    let s = DeploymentStore::new();
    // 默认 Normal
    assert_eq!(s.visibility("m"), VisibilityState::Normal);
    assert!(!s.is_public_model("m"));
    assert!(!s.is_private_model("m"));

    s.set_visibility("m", VisibilityState::Public);
    assert!(s.is_public_model("m"));
    assert!(!s.is_private_model("m"));

    s.set_visibility("m", VisibilityState::Private(vec!["team-a".into()]));
    assert!(s.is_private_model("m"));
    assert!(!s.is_public_model("m"));

    // Normal 清除
    s.set_visibility("m", VisibilityState::Normal);
    assert_eq!(s.visibility("m"), VisibilityState::Normal);
}

/// DT-RT-19：team_can_access：Public/Normal 恒 true；Private 看 ACL。
#[test]
fn store_team_can_access() {
    let s = DeploymentStore::new();
    s.set_visibility("pub", VisibilityState::Public);
    assert!(s.team_can_access("pub", None));
    assert!(s.team_can_access("pub", Some("any")));

    s.set_visibility("priv", VisibilityState::Private(vec!["t1".into(), "t2".into()]));
    assert!(!s.team_can_access("priv", None)); // 无 team → 拒
    assert!(s.team_can_access("priv", Some("t1")));
    assert!(!s.team_can_access("priv", Some("t3")));

    // Private 空列表 → 全锁
    s.set_visibility("locked", VisibilityState::Private(vec![]));
    assert!(!s.team_can_access("locked", Some("any")));

    // Normal → 恒 true
    assert!(s.team_can_access("normal", Some("any")));
}

/// DT-RT-20：remove_deployments 清除 visibility；clear 清全部状态。
#[test]
fn store_remove_clears_visibility() {
    let s = DeploymentStore::new();
    s.set_deployments("m".into(), vec![p("a", "d1")]);
    s.set_visibility("m", VisibilityState::Public);
    s.set_cost_rate("m", ModelCostRate::new(Decimal::from(1), Decimal::from(2)));
    s.set_quota_ratio("m", 3);
    s.remove_deployments("m");
    // visibility 随模型移除
    assert_eq!(s.visibility("m"), VisibilityState::Normal);
    assert!(!s.contains("m"));
}

/// DT-RT-21：clear 清空 deployments/quota/cost/exclusive/visibility（rr_counters 也清）。
#[test]
fn store_clear_all() {
    let s = DeploymentStore::new();
    s.set_deployments("m".into(), vec![p("a", "d1"), p("a", "d2")]);
    s.set_visibility("m", VisibilityState::Public);
    s.set_quota_ratio("m", 5);
    s.set_cost_rate("m", ModelCostRate::new(Decimal::from(1), Decimal::from(2)));
    let _ = s.select("m"); // 走一下 rr_counter
    s.clear();
    assert_eq!(s.len(), 0);
    assert_eq!(s.get_quota_ratio("m"), 1);
    assert!(s.get_cost_rate("m").is_zero());
    assert_eq!(s.visibility("m"), VisibilityState::Normal);
    // 清空后重新 set 同名 RR 计数器从 0 开始
    s.set_deployments("m".into(), vec![p("a", "d1"), p("a", "d2")]);
    let first = s.select("m").and_then(|x| x.deployment_id().map(|s| s.to_string())).unwrap();
    assert_eq!(first, "d1");
}

// ═════════════════════════════════════════════════════════════════
// ModelCostRate
// ═════════════════════════════════════════════════════════════════

/// DT-RT-22：compute_cost 无 cached 折扣 → 全部按 input 费率。
#[test]
fn cost_rate_no_cached_pricing() {
    let r = ModelCostRate::new(Decimal::from(2), Decimal::from(5));
    // 100 input, 0 cached, 20 output → 100*2 + 0 + 20*5 = 300
    assert_eq!(r.compute_cost(100, 0, 20), Decimal::from(300));
    // cached_tokens > 0 但 cached_input_cost 为零 → cached 按普通 input 价
    assert_eq!(r.compute_cost(100, 30, 20), Decimal::from(300)); // 70*2 + 30*2 + 20*5
}

/// DT-RT-23：compute_cost 有 cached 折扣 → cached 部分按折扣价。
#[test]
fn cost_rate_with_cached_pricing() {
    let r = ModelCostRate::with_cached(Decimal::from(2), Decimal::from(1), Decimal::from(5));
    // 100 input, 30 cached, 20 output → 70*2 + 30*1 + 20*5 = 140+30+100 = 270
    let (reg, cached, out) = r.compute_cost_breakdown(100, 30, 20);
    assert_eq!(reg, Decimal::from(140));
    assert_eq!(cached, Decimal::from(30));
    assert_eq!(out, Decimal::from(100));
    assert_eq!(r.compute_cost(100, 30, 20), Decimal::from(270));
}

/// DT-RT-24：cached_tokens 超过 input_tokens → 截断为 input_tokens。
#[test]
fn cost_rate_cached_saturates_to_input() {
    let r = ModelCostRate::with_cached(Decimal::from(2), Decimal::from(1), Decimal::from(5));
    // input=50, cached=100 (>50) → cached=50, non_cached=0 → 0 + 50*1 + 20*5 = 150
    assert_eq!(r.compute_cost(50, 100, 20), Decimal::from(150));
}

/// DT-RT-25：is_zero：default 全零 true；new(非零) false。
#[test]
fn cost_rate_is_zero() {
    assert!(ModelCostRate::default().is_zero());
    assert!(ModelCostRate::new(Decimal::ZERO, Decimal::ZERO).is_zero());
    assert!(!ModelCostRate::new(Decimal::from(1), Decimal::ZERO).is_zero());
    assert!(!ModelCostRate::with_cached(Decimal::ZERO, Decimal::from(1), Decimal::ZERO).is_zero());
}

// ═════════════════════════════════════════════════════════════════
// visibility_from_db / parse_allowed_teams
// ═════════════════════════════════════════════════════════════════

/// DT-RT-26：visibility_from_db 各组合（normal/public/private+ACL/private 无 ACL=全锁/垃圾值兜底 normal）。
#[test]
fn visibility_from_db_combinations() {
    assert_eq!(visibility_from_db(&Some("normal".into()), &None), VisibilityState::Normal);
    assert_eq!(visibility_from_db(&Some("public".into()), &None), VisibilityState::Public);
    assert_eq!(
        visibility_from_db(&Some("private".into()), &Some(serde_json::json!(["t1", "t2"]))),
        VisibilityState::Private(vec!["t1".into(), "t2".into()])
    );
    // private 无 ACL → 全锁（空列表）
    assert_eq!(visibility_from_db(&Some("private".into()), &None), VisibilityState::Private(vec![]));
    // normal 上有残留 ACL → 不重新私有化
    assert_eq!(visibility_from_db(&Some("normal".into()), &Some(serde_json::json!(["t1"]))), VisibilityState::Normal);
    // 垃圾 visibility 值 → 兜底 normal
    assert_eq!(visibility_from_db(&Some("garbage".into()), &None), VisibilityState::Normal);
    assert_eq!(visibility_from_db(&None, &None), VisibilityState::Normal);
}

/// DT-RT-27：parse_allowed_teams 各形态（None/Null/Array/空数组/非数组垃圾→空列表=全锁）。
#[test]
fn parse_allowed_teams_forms() {
    assert_eq!(parse_allowed_teams(&None), None);
    assert_eq!(parse_allowed_teams(&Some(serde_json::Value::Null)), None);
    assert_eq!(
        parse_allowed_teams(&Some(serde_json::json!(["a", "b"]))),
        Some(vec!["a".into(), "b".into()])
    );
    assert_eq!(parse_allowed_teams(&Some(serde_json::json!([]))), Some(vec![]));
    // 非数组垃圾 → Some(空) = 私有但全锁
    assert_eq!(parse_allowed_teams(&Some(serde_json::json!("oops"))), Some(vec![]));
    assert_eq!(parse_allowed_teams(&Some(serde_json::json!(42))), Some(vec![]));
}

// ═════════════════════════════════════════════════════════════════
// InFlightTracker + InFlightGuard
// ═════════════════════════════════════════════════════════════════

/// DT-RT-28：new 默认空；get_stats 过滤 request_count=0 的项。
#[test]
fn inflight_new_empty() {
    let t = Arc::new(InFlightTracker::new());
    assert!(t.get_stats().is_empty());
    assert!(t.get_deployment_stats().is_empty());
    assert_eq!(t.get_model_input_chars("m"), 0);
    assert_eq!(t.get_deployment_count("m", "d1"), 0);
    let def: InFlightTracker = InFlightTracker::default();
    assert!(def.get_stats().is_empty());
}

/// DT-RT-29：InFlightGuard model-level：acquire 后计数+chars 增加；Drop 后回落。
#[test]
fn inflight_guard_model_level() {
    let t = Arc::new(InFlightTracker::new());
    {
        let _g = InFlightGuard::new(t.clone(), "m", 500);
        assert_eq!(t.get_model_input_chars("m"), 500);
        let stats = t.get_stats();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].model, "m");
        assert_eq!(stats[0].inflight_requests, 1);
        assert_eq!(stats[0].inflight_input_chars, 500);
    }
    // Drop 后计数清零 → get_stats 过滤掉
    assert!(t.get_stats().is_empty());
    assert_eq!(t.get_model_input_chars("m"), 0);
}

/// DT-RT-30：InFlightGuard deployment-level：双层计数；get_deployment_count / get_deployment_stats。
#[test]
fn inflight_guard_deployment_level() {
    let t = Arc::new(InFlightTracker::new());
    {
        let _g = InFlightGuard::new_for_deployment(t.clone(), "m", "d1", 300);
        assert_eq!(t.get_deployment_count("m", "d1"), 1);
        assert_eq!(t.get_model_input_chars("m"), 300);
        let dstats = t.get_deployment_stats();
        assert_eq!(dstats.len(), 1);
        assert_eq!(dstats[0].model, "m");
        assert_eq!(dstats[0].deployment_id, "d1");
        assert_eq!(dstats[0].inflight_requests, 1);
        assert_eq!(dstats[0].inflight_input_chars, 300);
    }
    assert_eq!(t.get_deployment_count("m", "d1"), 0);
    assert!(t.get_deployment_stats().is_empty());
}

/// DT-RT-31：多个并发 guard 累加；逐个 Drop 递减。
#[test]
fn inflight_multiple_guards_accumulate() {
    let t = Arc::new(InFlightTracker::new());
    let g1 = InFlightGuard::new_for_deployment(t.clone(), "m", "d1", 100);
    let g2 = InFlightGuard::new_for_deployment(t.clone(), "m", "d1", 200);
    let g3 = InFlightGuard::new_for_deployment(t.clone(), "m", "d2", 50);
    assert_eq!(t.get_deployment_count("m", "d1"), 2);
    assert_eq!(t.get_deployment_count("m", "d2"), 1);
    assert_eq!(t.get_model_input_chars("m"), 350);
    drop(g1);
    assert_eq!(t.get_deployment_count("m", "d1"), 1);
    drop(g2);
    drop(g3);
    assert_eq!(t.get_model_input_chars("m"), 0);
}

// ═════════════════════════════════════════════════════════════════
// RebalanceCounter + RebalanceMoveTracker
// ═════════════════════════════════════════════════════════════════

/// DT-RT-32：RebalanceCounter record 累加；snapshot 60 桶，最后一桶=now。
#[test]
fn rebalance_counter_record_snapshot() {
    let c = RebalanceCounter::new();
    c.record();
    c.record();
    c.record();
    let snap = c.snapshot();
    assert_eq!(snap.len(), 60);
    assert_eq!(snap[0].0, "-59m");
    assert_eq!(snap[59].0, "now");
    assert_eq!(snap[59].1, 3);
}

/// DT-RT-33：RebalanceMoveTracker record_move(from,to)；None/空 id 跳过；snapshot 排序。
#[test]
fn rebalance_move_tracker_basic() {
    let t = RebalanceMoveTracker::new();
    t.record_move(Some("a"), Some("b"));
    t.record_move(Some("a"), Some("b"));
    t.record_move(Some("c"), Some("a"));
    let s = t.snapshot();
    let names: Vec<&str> = s.iter().map(|m| m.deployment_id.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
    let a = s.iter().find(|m| m.deployment_id == "a").unwrap();
    assert_eq!(a.in_count, 1); // c→a
    assert_eq!(a.out_count, 2); // a→b ×2
    let b = s.iter().find(|m| m.deployment_id == "b").unwrap();
    assert_eq!(b.in_count, 2);
    assert_eq!(b.out_count, 0);
}

/// DT-RT-34：RebalanceMoveTracker None/空 id 各方向跳过；both None → 空。
#[test]
fn rebalance_move_tracker_none_and_empty() {
    let t = RebalanceMoveTracker::new();
    t.record_move(Some("a"), None); // 只记 out
    t.record_move(None, Some("b")); // 只记 in
    t.record_move(None, None); // 全跳
    t.record_move(Some(""), Some("")); // 空 id 跳
    let s = t.snapshot();
    assert_eq!(s.len(), 2);
    let a = s.iter().find(|m| m.deployment_id == "a").unwrap();
    assert_eq!((a.in_count, a.out_count), (0, 1));
    let b = s.iter().find(|m| m.deployment_id == "b").unwrap();
    assert_eq!((b.in_count, b.out_count), (1, 0));
}

/// DT-RT-35：RebalanceMoveTracker same id → in/out 各+1。
#[test]
fn rebalance_move_tracker_same_id() {
    let t = RebalanceMoveTracker::new();
    t.record_move(Some("a"), Some("a"));
    let s = t.snapshot();
    let a = &s[0];
    assert_eq!((a.in_count, a.out_count), (1, 1));
}

// ═════════════════════════════════════════════════════════════════
// RequestRateTracker
// ═════════════════════════════════════════════════════════════════

/// DT-RT-36：new 默认只有 _total（count=0）；record 后 _total + 该 deployment 各+1；snapshot_all 首项 _total。
#[test]
fn request_rate_record_and_snapshot() {
    let t = RequestRateTracker::new();
    t.record("dep-1", "model-a");
    t.record("dep-1", "model-a");
    t.record("dep-2", "model-b");
    let all = t.snapshot_all();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].0, "_total");
    // 其余按 deployment_id 排序
    assert_eq!(all[1].0, "dep-1");
    assert_eq!(all[1].1, "model-a");
    assert_eq!(all[2].0, "dep-2");
    assert_eq!(all[2].1, "model-b");
}

/// DT-RT-37：rename：同名 deployment 再次 record 新 model → 标签更新。
#[test]
fn request_rate_rename_updates_label() {
    let t = RequestRateTracker::new();
    t.record("dep-1", "old");
    t.record("dep-1", "new");
    let all = t.snapshot_all();
    assert_eq!(all[1].1, "new");
}

/// DT-RT-38：remove 删除 deployment 系列；_total 保留。
#[test]
fn request_rate_remove() {
    let t = RequestRateTracker::new();
    t.record("dep-1", "m");
    t.remove("dep-1");
    let all = t.snapshot_all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].0, "_total");
}

// ═════════════════════════════════════════════════════════════════
// MlServiceStats
// ═════════════════════════════════════════════════════════════════

/// DT-RT-39：counters 记录正确（attempt/success/failure/fallback + tier 分布）。
#[test]
fn mlstats_counters() {
    let s = MlServiceStats::new();
    let _ = s.record_attempt();
    s.record_success("small", std::time::Duration::from_millis(10));
    let _ = s.record_attempt();
    s.record_failure();
    s.record_fallback("small");
    assert_eq!(s.attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(s.successes.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(s.failures.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(s.fallbacks.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        s.tier_distribution.get("small").unwrap().value().load(std::sync::atomic::Ordering::Relaxed),
        2, // success + fallback 都 bump tier
    );
}

/// DT-RT-40：min/max latency 仅成功调用更新（CAS 取最大/最小）。
#[test]
fn mlstats_min_max_latency() {
    let s = MlServiceStats::new();
    s.record_success("a", std::time::Duration::from_millis(10));
    s.record_success("b", std::time::Duration::from_millis(50));
    s.record_success("c", std::time::Duration::from_millis(5));
    s.record_failure(); // 不影响 min/max
    let max_ns = s.max_latency_ns.load(std::sync::atomic::Ordering::Relaxed);
    let min_ns = s.min_latency_ns.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(max_ns / 1_000_000, 50);
    assert_eq!(min_ns / 1_000_000, 5);
}

/// DT-RT-41：无成功调用时 min_latency 保持 NO_LATENCY 哨兵。
#[test]
fn mlstats_min_sentinel_when_no_success() {
    let s = MlServiceStats::new();
    let _ = s.record_attempt();
    s.record_failure();
    // NO_LATENCY = u64::MAX
    assert_eq!(s.min_latency_ns.load(std::sync::atomic::Ordering::Relaxed), u64::MAX);
}

/// DT-RT-42：maybe_emit_summary 首次 true（last=0）；窗口内第二次 false。
#[test]
fn mlstats_maybe_emit_summary_window() {
    let s = MlServiceStats::new();
    assert!(s.maybe_emit_summary("http://x"));
    assert!(!s.maybe_emit_summary("http://x"));
}

// ═════════════════════════════════════════════════════════════════
// AutoRouter / TierClassifier / StrategyRegistry
// ═════════════════════════════════════════════════════════════════

fn msg(role: &str, text: &str) -> Message {
    Message {
        role: match role {
            "user" => MessageRole::User,
            "system" => MessageRole::System,
            "assistant" => MessageRole::Assistant,
            _ => MessageRole::User,
        },
        content: MessageContent::Text(text.to_string()),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
    }
}

fn make_router() -> AutoRouter {
    let mut tiers = std::collections::HashMap::new();
    tiers.insert("small".into(), "small-cup".into());
    tiers.insert("medium".into(), "medium-cup".into());
    tiers.insert("large".into(), "large-cup".into());
    AutoRouter::new("auto".into(), Arc::new(TierClassifier), "medium".into(), tiers)
}

/// DT-RT-43：非匹配 model → classify 返回 None（透传）。
#[tokio::test]
async fn auto_router_non_matching_model() {
    let r = make_router();
    let msgs = vec![msg("user", "hi")];
    assert!(r.classify("gpt-4o", &msgs, &None).await.is_none());
    assert_eq!(r.model_name(), "auto");
}

/// DT-RT-44：短问候 → small；debug 关键词 → medium；代码块+推理关键词 → large。
/// （打分：code 0.8 + reasoning 0.8 = 1.6 ≥ 1.5 才到 large；上游内部测试
/// `code_request_routes_to_large` 用例只到 1.3，本身是失败的——此处按真实打分断言。）
#[tokio::test]
async fn auto_router_tier_classification() {
    let r = make_router();
    let small = r.classify("auto", &[msg("user", "hi")], &None).await;
    assert_eq!(small.as_deref(), Some("small-cup"));

    let medium = r.classify("auto", &[msg("user", "debug this code:\n```python\nprint(1)\n```")], &None).await;
    assert_eq!(medium.as_deref(), Some("medium-cup"));

    let large = r.classify("auto", &[msg("user", "prove the formula:\n```python\nprint(1)\n```")], &None).await;
    assert_eq!(large.as_deref(), Some("large-cup"));
}

/// DT-RT-45：tools 存在 → 至少 medium。
#[tokio::test]
async fn auto_router_tool_request_medium_plus() {
    let r = make_router();
    let tools = vec![Tool {
        tool_type: "function".into(),
        function: boom_core::types::ToolFunction {
            name: "get_weather".into(),
            description: Some("w".into()),
            parameters: serde_json::json!({}),
        },
    }];
    let res = r.classify("auto", &[msg("user", "weather?")], &Some(tools)).await;
    assert!(res == Some("medium-cup".into()) || res == Some("large-cup".into()));
}

/// DT-RT-46：未知 tier → 回退 default_tier；default 也缺失 → 回退 model_name。
#[tokio::test]
async fn auto_router_unknown_tier_falls_back() {
    // strategy 返回未知 tier
    struct Unknown;
    #[async_trait::async_trait]
    impl ClassificationStrategy for Unknown {
        fn name(&self) -> &str { "unknown" }
        async fn classify(&self, _req: &ClassifyRequest<'_>) -> String { "x-large".into() }
    }
    let mut tiers = std::collections::HashMap::new();
    tiers.insert("medium".into(), "medium-cup".into()); // 只有 default
    let r = AutoRouter::new("auto".into(), Arc::new(Unknown), "medium".into(), tiers);
    let res = r.classify("auto", &[msg("user", "x")], &None).await;
    assert_eq!(res.as_deref(), Some("medium-cup"));

    // default 也缺失 → 回退 model_name
    let r2 = AutoRouter::new("auto".into(), Arc::new(Unknown), "missing".into(), std::collections::HashMap::new());
    let res2 = r2.classify("auto", &[msg("user", "x")], &None).await;
    assert_eq!(res2.as_deref(), Some("auto"));
}

/// DT-RT-47：StrategyRegistry register + get（命中/未命中）。
#[tokio::test]
async fn strategy_registry_register_lookup() {
    let mut reg = StrategyRegistry::new();
    reg.register(Arc::new(TierClassifier));
    assert!(reg.get("tier_classifier").is_some());
    assert!(reg.get("nonexistent").is_none());
    let def: StrategyRegistry = StrategyRegistry::default();
    assert!(def.get("any").is_none());
}

// ═════════════════════════════════════════════════════════════════
// RoundRobinPolicy / ShufflePolicy
// ═════════════════════════════════════════════════════════════════

/// DT-RT-48：RoundRobinPolicy 空候选→None；单候选直返；多候选轮询。
#[test]
fn round_robin_policy() {
    let pol = RoundRobinPolicy::new();
    assert_eq!(pol.name(), "round_robin");
    assert!(pol.select("m", &[], None, 0).is_none());
    let single = vec![p("a", "d1")];
    assert!(pol.select("m", &single, None, 0).is_some());
    let cands = vec![p("a", "d1"), p("a", "d2"), p("a", "d3")];
    let ids: Vec<String> = (0..4).filter_map(|_| {
        pol.select("m", &cands, None, 0).and_then(|x| x.deployment_id().map(|s| s.to_string()))
    }).collect();
    assert_eq!(ids, vec!["d1", "d2", "d3", "d1"]);
    // select_with_context 默认委托 select，kv_hit_ratio=0
    let sel = pol.select_with_context("m", &cands, None, 0, &[1, 2]).unwrap();
    assert_eq!(sel.kv_hit_ratio, 0.0);
    assert!(!sel.kv_match_attempted);
}

/// DT-RT-49：ShufflePolicy 空候选→None；单候选直返；多候选均匀分布（统计容忍）。
#[test]
fn shuffle_policy_distribution() {
    let pol = ShufflePolicy::new();
    assert_eq!(pol.name(), "shuffle");
    assert!(pol.select("m", &[], None, 0).is_none());
    let single = vec![p("a", "d1")];
    assert!(pol.select("m", &single, None, 0).is_some());
    let cands = vec![p("a", "d1"), p("a", "d2"), p("a", "d3")];
    let mut counts = [0u32; 3];
    for _ in 0..3000 {
        let picked = pol.select("m", &cands, None, 0).unwrap();
        let idx = cands.iter().position(|c| Arc::ptr_eq(c, &picked)).unwrap();
        counts[idx] += 1;
    }
    // 3 × 3000 → ~1000 each；容忍 ±15%
    for c in counts.iter() {
        assert!((850..=1150).contains(c), "shuffle skewed: {c}");
    }
}

// ═════════════════════════════════════════════════════════════════
// KeyAffinityPolicy
// ═════════════════════════════════════════════════════════════════

/// DT-RT-50：KeyAffinityPolicy 空候选→None；单候选直返。
#[test]
fn key_affinity_empty_and_single() {
    let tracker = Arc::new(InFlightTracker::new());
    let pol = KeyAffinityPolicy::new(tracker, 0, 100, None);
    assert_eq!(pol.name(), "key_affinity");
    assert!(pol.select("m", &[], None, 0).is_none());
    let single = vec![p("a", "d1")];
    assert_eq!(pol.select("m", &single, Some("k1"), 0).unwrap().deployment_id(), Some("d1"));
}

/// DT-RT-51：无 key_hash → 回退 lowest-load（首候选，所有 load 相同时）。
#[test]
fn key_affinity_no_key_falls_back_to_lowest_load() {
    let tracker = Arc::new(InFlightTracker::new());
    let pol = KeyAffinityPolicy::new(tracker.clone(), 0, 100, None);
    let cands = vec![p("a", "d1"), p("a", "d2")];
    let r = pol.select("m", &cands, None, 0).unwrap();
    // 所有 load=0 → min_load_candidate 返回首候选
    assert_eq!(r.deployment_id(), Some("d1"));
}

/// DT-RT-52：首次带 key → 最低负载派发并记录 affinity；同 key 再来 → 命中 affinity。
#[test]
fn key_affinity_initial_assignment_and_hit() {
    let tracker = Arc::new(InFlightTracker::new());
    let pol = KeyAffinityPolicy::new(tracker, 0, 100, None);
    let cands = vec![p("a", "d1"), p("a", "d2")];
    // 首次：最低负载 → d1，记录 affinity k1→d1
    let r1 = pol.select("m", &cands, Some("k1"), 0).unwrap();
    assert_eq!(r1.deployment_id(), Some("d1"));
    // 第二次同 key：affinity 命中 d1（load 相同不触发 rebalance）
    let r2 = pol.select("m", &cands, Some("k1"), 0).unwrap();
    assert_eq!(r2.deployment_id(), Some("d1"));
}

/// DT-RT-53：warm-up：context_threshold>0 且 total_input < threshold → 最低负载（不走 affinity）。
#[test]
fn key_affinity_warmup_below_threshold() {
    let tracker = Arc::new(InFlightTracker::new());
    let pol = KeyAffinityPolicy::new(tracker, 10_000, 100, None);
    let cands = vec![p("a", "d1"), p("a", "d2")];
    // tracker 无 inflight → total_input=0 < 10000 → warm-up 走最低负载
    let r = pol.select("m", &cands, Some("k1"), 0).unwrap();
    assert_eq!(r.deployment_id(), Some("d1")); // 首候选
}

/// DT-RT-54：rebalance：preferred 过载超阈值 → 迁移到 least_loaded；move_tracker 记录。
#[test]
fn key_affinity_rebalance_migrates() {
    let tracker = Arc::new(InFlightTracker::new());
    let move_t = Arc::new(RebalanceMoveTracker::new());
    let mut pol = KeyAffinityPolicy::new(tracker.clone(), 0, 10, Some(move_t.clone()));
    let qi = Arc::new(MockQueueInfo::new());
    pol.set_queue_info(qi.clone());
    let cands = vec![p("a", "d1"), p("a", "d2")];
    // 先建立 affinity k1→d1（此时无 queue 负载，d1 为最低负载首候选）
    let first = pol.select("m", &cands, Some("k1"), 0).unwrap();
    assert_eq!(first.deployment_id(), Some("d1"));
    // 注入 d1 load=80% cap=100, d2 load=0% → 80 > 0 + threshold=10 → rebalance 到 d2
    qi.set("d1", 80, 100);
    qi.set("d2", 0, 100);
    let r = pol.select("m", &cands, Some("k1"), 0).unwrap();
    assert_eq!(r.deployment_id(), Some("d2"));
    // move_tracker 应记录 d1→d2
    let moves = move_t.snapshot();
    assert!(moves.iter().any(|m| m.deployment_id == "d1" && m.out_count > 0));
    assert!(moves.iter().any(|m| m.deployment_id == "d2" && m.in_count > 0));
}

// ═════════════════════════════════════════════════════════════════
// KvcAwarePolicy
// ═════════════════════════════════════════════════════════════════

/// DT-RT-55：KvcAwarePolicy 空候选→None；单候选→直返（skip KV lookup）。
#[test]
fn kvc_aware_empty_and_single() {
    let idx = fresh_index();
    let tracker = Arc::new(InFlightTracker::new());
    let pol = KvcAwarePolicy::new(idx, tracker, None);
    assert_eq!(pol.name(), "kvc_aware");
    assert!(pol.select_with_context("m", &[], None, 0, &[1, 2]).is_none());
    let single = vec![p("a", "d1")];
    let sel = pol.select_with_context("m", &single, None, 0, &[1, 2]).unwrap();
    assert!(!sel.kv_match_attempted);
    assert_eq!(sel.kv_hit_ratio, 0.0);
}

/// DT-RT-56：空 prefix → 最低负载（degraded=true）。
#[test]
fn kvc_aware_empty_prefix_degraded() {
    let idx = fresh_index();
    let tracker = Arc::new(InFlightTracker::new());
    let pol = KvcAwarePolicy::new(idx, tracker, None);
    let cands = vec![p("a", "d1"), p("a", "d2")];
    let sel = pol.select_with_context("m", &cands, None, 0, &[]).unwrap();
    assert!(sel.degraded);
    assert!(!sel.kv_match_attempted);
}

/// DT-RT-57：候选无 kv_worker_id → 最低负载（degraded=false, match_attempted=false）。
#[test]
fn kvc_aware_no_kv_worker_id_falls_back() {
    let idx = fresh_index();
    let tracker = Arc::new(InFlightTracker::new());
    let pol = KvcAwarePolicy::new(idx, tracker, None);
    let cands = vec![p_no_kv("a", "d1"), p_no_kv("a", "d2")];
    let sel = pol.select_with_context("m", &cands, None, 0, &[1, 2, 3, 4]).unwrap();
    assert!(!sel.kv_match_attempted);
    assert!(!sel.degraded);
    assert_eq!(sel.kv_hit_ratio, 0.0);
}

/// DT-RT-58：冷启动（trie 无命中）→ 全候选 score=0 平手 → round-robin；match_attempted=true。
#[test]
fn kvc_aware_cold_round_robin() {
    let idx = fresh_index();
    let tracker = Arc::new(InFlightTracker::new());
    let pol = KvcAwarePolicy::new(idx, tracker, None);
    let cands = vec![p("a", "d1"), p("a", "d2")];
    // trie 为空 → 全 0 分 → round-robin 在 d1/d2 间轮换
    let s1 = pol.select_with_context("m", &cands, None, 0, &[1, 2, 3, 4]).unwrap();
    let s2 = pol.select_with_context("m", &cands, None, 0, &[1, 2, 3, 4]).unwrap();
    assert!(s1.kv_match_attempted);
    assert_eq!(s1.kv_hit_ratio, 0.0);
    assert_ne!(
        s1.provider.deployment_id(), s2.provider.deployment_id(),
        "cold round-robin should alternate"
    );
}

/// DT-RT-59：trie 命中前缀 → affinity 派发到命中的 worker；hit_ratio>0。
#[test]
fn kvc_aware_affinity_hit() {
    let idx = fresh_index();
    // 在 d1 的 trie 下记录前缀 [1,2,3,4]
    idx.apply_event(&store_event("m", "d1", None, vec![1, 2, 3, 4]));
    let tracker = Arc::new(InFlightTracker::new());
    let pol = KvcAwarePolicy::new(idx, tracker, None);
    let cands = vec![p("a", "d1"), p("a", "d2")];
    let sel = pol.select_with_context("m", &cands, None, 0, &[1, 2, 3, 4]).unwrap();
    assert!(sel.kv_match_attempted);
    assert!(sel.kv_hit_ratio > 0.0);
    assert_eq!(sel.provider.deployment_id(), Some("d1"));
    assert_eq!(sel.kv_hit_blocks, 1);
}

/// DT-RT-60：rebalance_threshold<100 + winner 过载 → 迁移到 least_loaded。
#[test]
fn kvc_aware_rebalance_when_winner_overloaded() {
    let idx = fresh_index();
    idx.apply_event(&store_event("m", "d1", None, vec![1, 2, 3, 4])); // d1 有命中
    let tracker = Arc::new(InFlightTracker::new());
    let move_t = Arc::new(RebalanceMoveTracker::new());
    let mut pol = KvcAwarePolicy::new(idx, tracker, Some(move_t.clone()));
    pol.set_rebalance_threshold(10);
    let qi = Arc::new(MockQueueInfo::new());
    qi.set("d1", 80, 100); // winner d1 load=80%
    qi.set("d2", 0, 100);
    pol.set_queue_info(qi);
    let cands = vec![p("a", "d1"), p("a", "d2")];
    // d1 命中（winner）但 load=80% >> d2 load=0% + 10 → 迁移到 d2
    let sel = pol.select_with_context("m", &cands, None, 0, &[1, 2, 3, 4]).unwrap();
    assert_eq!(sel.provider.deployment_id(), Some("d2"));
    assert!(move_t.snapshot().iter().any(|m| m.deployment_id == "d1" && m.out_count > 0));
}

// ═════════════════════════════════════════════════════════════════
// load_helpers
// ═════════════════════════════════════════════════════════════════

/// DT-RT-61：should_rebalance：preferred > min + threshold → true；否则 false。
#[test]
fn load_helpers_should_rebalance() {
    use boom_routing::load_helpers::should_rebalance;
    assert!(should_rebalance(80, 10, 50));   // 80 > 10+50
    assert!(!should_rebalance(50, 10, 50));  // 50 == 60? no, 50 > 60 false
    assert!(!should_rebalance(10, 10, 50));  // 10 > 60 false
    assert!(should_rebalance(100, 0, 99));
}

/// DT-RT-62：deployment_load 无 queue_info → raw inflight；无 deployment_id → 0。
#[test]
fn load_helpers_deployment_load() {
    use boom_routing::load_helpers::deployment_load;
    let tracker = Arc::new(InFlightTracker::new());
    let qi: Option<Arc<dyn DeploymentQueueInfo>> = None;
    // 无 deployment_id 的 provider → 0
    struct NoId;
    impl Provider for NoId {
        fn name(&self) -> &str { "n" }
        fn models(&self) -> &[String] { &[] }
        fn chat<'l, 'a>(&'l self, _r: ChatCompletionRequest) -> Pin<Box<dyn Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send + 'a>> where Self: 'a, 'l: 'a { Box::pin(async { Err(GatewayError::ConfigError("x".into())) }) }
        fn chat_stream<'l, 'a>(&'l self, _r: ChatCompletionRequest) -> Pin<Box<dyn Future<Output = Result<ChatStream, GatewayError>> + Send + 'a>> where Self: 'a, 'l: 'a { Box::pin(async { Err(GatewayError::ConfigError("x".into())) }) }
    }
    assert_eq!(deployment_load(&tracker, &qi, "m", &NoId), 0);
    // 有 deployment_id 但无 inflight → 0
    let prov = p("a", "d1");
    assert_eq!(deployment_load(&tracker, &qi, "m", prov.as_ref()), 0);
    // 有 inflight（1 请求）→ raw_load=1
    let _g = InFlightGuard::new_for_deployment(tracker.clone(), "m", "d1", 100);
    assert_eq!(deployment_load(&tracker, &qi, "m", prov.as_ref()), 1);
}

/// DT-RT-63：min_load_candidate 返回最低 load 的候选（无 inflight 时首候选）。
#[test]
fn load_helpers_min_load_candidate() {
    use boom_routing::load_helpers::min_load_candidate;
    let tracker = Arc::new(InFlightTracker::new());
    let qi: Option<Arc<dyn DeploymentQueueInfo>> = None;
    let cands = vec![p("a", "d1"), p("a", "d2"), p("a", "d3")];
    let (load, prov) = min_load_candidate(&tracker, &qi, "m", &cands);
    assert_eq!(load, 0);
    assert_eq!(prov.deployment_id(), Some("d1"));
    // d2 有 inflight → d1/d3 load=0 仍最低，首候选 d1
    let _g = InFlightGuard::new_for_deployment(tracker.clone(), "m", "d2", 100);
    let (load2, _) = min_load_candidate(&tracker, &qi, "m", &cands);
    assert_eq!(load2, 0);
}

// ═════════════════════════════════════════════════════════════════
// Router
// ═════════════════════════════════════════════════════════════════

fn router_with(store: Arc<DeploymentStore>, aliases: Arc<AliasStore>) -> Router {
    Router::new(store, aliases, Arc::new(RoundRobinPolicy::new()))
}

/// DT-RT-64：resolve_model_name：直接 deployment → 原名；alias → target；都不是 → 原名。
#[test]
fn router_resolve_model_name() {
    let store = Arc::new(DeploymentStore::new());
    store.set_deployments("gpt-4o".into(), vec![p("a", "d1")]);
    let aliases = Arc::new(AliasStore::new());
    aliases.set_alias("gpt4".into(), "gpt-4o".into(), false);
    let r = router_with(store, aliases);
    // 直接 deployment
    assert_eq!(r.resolve_model_name("gpt-4o"), "gpt-4o");
    // alias
    assert_eq!(r.resolve_model_name("gpt4"), "gpt-4o");
    // 都不是 → 原名
    assert_eq!(r.resolve_model_name("unknown"), "unknown");
}

/// DT-RT-65：resolve_model / resolve_candidates：精确匹配 → alias → "*" fallback。
#[test]
fn router_resolve_candidates_cascade() {
    let store = Arc::new(DeploymentStore::new());
    store.set_deployments("real".into(), vec![p("a", "d1")]);
    store.set_deployments("*".into(), vec![p("a", "wild")]); // 兜底
    let aliases = Arc::new(AliasStore::new());
    aliases.set_alias("alias".into(), "real".into(), false);
    let r = router_with(store, aliases);
    // 精确
    assert_eq!(r.candidates_for("real").unwrap().len(), 1);
    // alias → real
    assert_eq!(r.candidates_for("alias").unwrap().len(), 1);
    // 未知 → "*" fallback
    assert_eq!(r.candidates_for("ghost-model").unwrap().len(), 1);
    // resolve_model 返回 alias target
    assert_eq!(r.resolve_model("alias").as_deref(), Some("real"));
    assert!(r.resolve_model("real").is_none()); // 非 alias → None
}

/// DT-RT-66：resolve_candidates 空组（configured 但无 provider）→ None（抑制 fallback）。
#[test]
fn router_empty_group_suppresses_fallback() {
    let store = Arc::new(DeploymentStore::new());
    store.set_deployments("m".into(), vec![]); // 空组
    store.set_deployments("*".into(), vec![p("a", "w")]);
    let r = router_with(store, Arc::new(AliasStore::new()));
    // 空组 → None（不 fallback 到 *）
    assert!(r.candidates_for("m").is_none());
}

/// DT-RT-67：select_with_candidates 空列表→None；非空→委托 policy。
#[test]
fn router_select_with_candidates() {
    let store = Arc::new(DeploymentStore::new());
    let r = router_with(store, Arc::new(AliasStore::new()));
    let cands = vec![p("a", "d1"), p("a", "d2")];
    assert!(r.select_with_candidates("m", &[], None, 0, &[]).is_none());
    let sel = r.select_with_candidates("m", &cands, None, 0, &[]).unwrap();
    // RoundRobinPolicy select_with_context 默认 kv_hit_ratio=0
    assert_eq!(sel.kv_hit_ratio, 0.0);
}

/// DT-RT-68：select_provider_with_prefix：有候选→Selection；无候选→None。
#[test]
fn router_select_provider_with_prefix() {
    let store = Arc::new(DeploymentStore::new());
    store.set_deployments("m".into(), vec![p("a", "d1"), p("a", "d2")]);
    let r = router_with(store, Arc::new(AliasStore::new()));
    let sel = r.select_provider_with_prefix("m", None, 0, &[1, 2]).unwrap();
    assert_eq!(sel.kv_hit_ratio, 0.0); // RR policy 无 KV 上下文
    assert!(r.select_provider_with_prefix("ghost", None, 0, &[1, 2]).is_none());
}

/// DT-RT-69：is_model_configured / is_public_model / is_private_model / team_can_access 透传 store。
#[test]
fn router_access_checks() {
    let store = Arc::new(DeploymentStore::new());
    store.set_deployments("m".into(), vec![p("a", "d1")]);
    store.set_visibility("m", VisibilityState::Private(vec!["t1".into()]));
    let r = router_with(store, Arc::new(AliasStore::new()));
    assert!(r.is_model_configured("m"));
    assert!(!r.is_model_configured("ghost"));
    assert!(r.is_private_model("m"));
    assert!(!r.is_public_model("m"));
    assert!(r.team_can_access("m", Some("t1")));
    assert!(!r.team_can_access("m", Some("t2")));
}

/// DT-RT-70：visible_model_names 过滤空组 + "*" + 追加可见 alias。
#[test]
fn router_visible_model_names() {
    let store = Arc::new(DeploymentStore::new());
    store.set_deployments("a".into(), vec![p("p", "d1")]);
    store.set_deployments("b".into(), vec![]); // 空组 → 过滤
    store.set_deployments("*".into(), vec![p("p", "w")]); // 通配 → 过滤
    let aliases = Arc::new(AliasStore::new());
    aliases.set_alias("alias-x".into(), "a".into(), false);
    aliases.set_alias("hidden-y".into(), "a".into(), true); // hidden → 过滤
    let r = router_with(store, aliases);
    let names = r.visible_model_names();
    assert!(names.contains(&"a".to_string()));
    assert!(!names.contains(&"b".to_string())); // 空组
    assert!(!names.contains(&"*".to_string())); // 通配
    assert!(names.contains(&"alias-x".to_string())); // 可见 alias
    assert!(!names.contains(&"hidden-y".to_string())); // hidden alias
}

/// DT-RT-71：policy_name / set_policy 热替换。
#[test]
fn router_policy_name_and_swap() {
    let store = Arc::new(DeploymentStore::new());
    let r = router_with(store, Arc::new(AliasStore::new()));
    assert_eq!(r.policy_name(), "round_robin");
    r.set_policy(Arc::new(ShufflePolicy::new()));
    assert_eq!(r.policy_name(), "shuffle");
}

/// DT-RT-72：with_classifier + is_auto_virtual_model + resolve_request_model。
#[tokio::test]
async fn router_with_classifier() {
    let store = Arc::new(DeploymentStore::new());
    store.set_deployments("small-cup".into(), vec![p("a", "d1")]);
    let classifier = Arc::new(make_router());
    let r = Router::with_classifier(
        store,
        Arc::new(AliasStore::new()),
        Arc::new(RoundRobinPolicy::new()),
        Some(classifier),
    );
    assert!(r.is_auto_virtual_model("auto"));
    assert!(!r.is_auto_virtual_model("other"));
    // resolve_request_model：匹配 virtual → 分类
    let res = r.resolve_request_model("auto", &[msg("user", "hi")], &None).await;
    assert_eq!(res, "small-cup");
    // 非匹配 → 走 resolve_model_name
    let res2 = r.resolve_request_model("small-cup", &[msg("user", "hi")], &None).await;
    assert_eq!(res2, "small-cup");
    // set_classifier 清空
    r.set_classifier(None);
    assert!(!r.is_auto_virtual_model("auto"));
}

// ═════════════════════════════════════════════════════════════════
// migrations — DDL 字符串包含关键列/表
// ═════════════════════════════════════════════════════════════════

/// DT-RT-73：deployment_ddl 含关键列；migration_add_* 各加对应列；alias_ddl 含表名。
#[test]
fn migrations_ddl_content() {
    use boom_routing::migrations::*;
    let ddl = deployment_ddl();
    assert!(ddl.contains("boom_model_deployment"));
    assert!(ddl.contains("model_name"));
    assert!(ddl.contains("litellm_model"));
    assert!(ddl.contains("deployment_id"));

    let ad = migration_add_auto_disabled();
    assert!(ad.contains("auto_disabled"));

    let at = migration_add_allowed_teams();
    assert!(at.contains("allowed_teams"));

    let av = migration_add_visibility();
    assert!(av.contains("visibility"));
    assert!(av.contains("private")); // 旧数据归一化

    let al = alias_ddl();
    assert!(al.contains("boom_model_alias"));
    assert!(al.contains("alias_name"));
    assert!(al.contains("target_model"));
}
