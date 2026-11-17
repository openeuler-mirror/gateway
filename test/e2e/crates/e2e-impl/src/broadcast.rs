//! 广播域用例（MI-018…）。台账见 cases/multi-instance-broadcast/。
//! 观察面：请求行为 + 实例日志佐证；不直接读 watcher 内部状态。

use crate::{CaseEntry, EnvGroup};
use e2e_envs::fixture::KEY_WARMUP;
use e2e_envs::TestEnv;
use e2e_tools::Evidence;
use futures::future::BoxFuture;
use std::time::Duration;

pub const ALL: &[CaseEntry] = &[CaseEntry {
    id: "MI-018",
    title: "revision 首轮派发：plan+assignment 同轮变更后两实例均生效",
    env_group: EnvGroup::GBase,
    severity: "P0",
    decision_base_shared: false,
    run: mi_018,
}];

/// poll 单实例 KEY_WARMUP（ttl=1 plan）直到 429；返回首现耗时。
/// 前置：环境启动时 fixture 已单事务 bump（plan/assignment/quota），
/// 本函数不再 bump——追加 bump 会给竞态愈合机会，污染观察。
async fn await_plan_429(env: &TestEnv, gw: &str) -> anyhow::Result<f64> {
    let client = env.gateway(gw).client_with(KEY_WARMUP);
    let model = env.model_name.clone();
    let t0 = std::time::Instant::now();
    e2e_tools::poll::poll_until(Duration::from_secs(95), Duration::from_secs(2), || {
        let client = client.clone();
        let model = model.clone();
        Box::pin(async move {
            let out = client.chat(&model, "MI-018 plan probe").await?;
            Ok(if out.status == 429 { Some(()) } else { None })
        })
    })
    .await?;
    Ok(t0.elapsed().as_secs_f64())
}

fn mi_018<'a>(env: &'a TestEnv) -> BoxFuture<'a, anyhow::Result<Vec<String>>> {
    Box::pin(async move {
        // 逐实例探测（顺序执行：受竞态影响的实例 95s 超时，正常实例秒级返回）
        let mut results: Vec<(&str, anyhow::Result<f64>)> = Vec::new();
        for gw in ["gw-a", "gw-b"] {
            results.push((gw, await_plan_429(env, gw).await));
        }
        let detail = results
            .iter()
            .map(|(g, r)| match r {
                Ok(s) => format!("{g}: 429 @ {s:.1}s"),
                Err(_) => format!("{g}: 95s 无 429（plan=-）"),
            })
            .collect::<Vec<_>>()
            .join("；");

        let reds: Vec<&str> = results
            .iter()
            .filter(|(_, r)| r.is_err())
            .map(|(g, _)| *g)
            .collect();
        if !reds.is_empty() {
            // 派发成功却未生效 + 无 ERROR 日志 = assignment 被 cleanup 抹掉、信号已消费。
            anyhow::bail!(
                "revision 首轮派发后 plan 未生效 [{detail}]；受影响实例：{reds:?}\
                 ——reconcile_assignments_from_db 末尾 cleanup_assignments 在 plan 装载前清空绑定（MI-018 bug 证据）"
            );
        }

        let mut ev = Evidence::default();
        ev.note(format!("两实例 plan 均生效：{detail}"));
        Ok(ev.lines)
    })
}
