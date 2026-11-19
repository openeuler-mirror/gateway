//! 计量同步域用例（MI-001~008、MI-017）。台账见 cases/multi-instance-metering/。
//! 期望值锚点 = 客户端观测响应 usage 之和；DB 等待一律 poll_scalar。

use crate::{CaseEntry, EnvGroup};
use e2e_envs::fixture::KEY_MAIN;
use e2e_envs::TestEnv;
use e2e_tools::keys::{key_cumulative_cache_key, CumSuffix};
use e2e_tools::{Bind, Evidence};
use futures::future::BoxFuture;
use std::time::Duration;

pub const ALL: &[CaseEntry] = &[
    CaseEntry {
        id: "MI-001",
        title: "双实例计量收敛：cumulative == Σ usage",
        env_group: EnvGroup::GBase,
        severity: "P0",
        decision_base_shared: false,
        run: mi_001,
    },
    CaseEntry {
        id: "MI-002",
        title: "shared 判定：A 耗尽预算后 B 跨实例 429",
        env_group: EnvGroup::GBase,
        severity: "P0",
        decision_base_shared: true,
        run: mi_002,
    },
    CaseEntry {
        id: "MI-019",
        title: "shared 首触盲区：B 未服务过的 key 全局基数缺失",
        env_group: EnvGroup::GBase,
        severity: "P0",
        decision_base_shared: true,
        run: mi_019,
    },
];

/// 用例级 seed：SQL 工件落 runs/<id>/configs/seed-<label>.sql（取证），psql 执行。
fn seed_extra(env: &TestEnv, label: &str, sql: String) -> anyhow::Result<()> {
    let p = env
        .root
        .join("configs")
        .join(format!("seed-{label}.sql"));
    std::fs::write(&p, &sql)?;
    env.pg.psql_file(&p)?;
    Ok(())
}

/// 实例当前最大水位 seq；无行 → -1（首推 seq 从 0 起，boot hydrate = MAX+1）。
async fn max_seq(env: &TestEnv, instance_id: &str) -> anyhow::Result<i64> {
    let v = env
        .db
        .scalar_opt(
            "SELECT MAX(seq) FROM boom_quota_delta WHERE instance_id = $1",
            &[Bind::S(instance_id.to_string())],
        )
        .await?;
    Ok(v.and_then(|x| x.as_i64()).unwrap_or(-1))
}

/// 累计行值；行不存在 → 错误（计量断言下行不存在即是丢数）。
async fn cumulative(env: &TestEnv, raw_key: &str, suffix: CumSuffix) -> anyhow::Result<i64> {
    env.db
        .scalar_i64(
            "SELECT value FROM boom_rate_limit_cumulative WHERE cache_key = $1",
            &[Bind::S(key_cumulative_cache_key(raw_key, suffix))],
        )
        .await
}

fn mi_001<'a>(env: &'a TestEnv) -> BoxFuture<'a, anyhow::Result<Vec<String>>> {
    Box::pin(async move {
        let mut ev = Evidence::default();
        let model = env.model_name.clone();

        // baseline 水位：请求前 seq，之后等 seq > baseline（该水位行必含本实例全部本地增量）
        let base_a = max_seq(env, "gw-a").await?;
        let base_b = max_seq(env, "gw-b").await?;

        // 1. gw-a ×10、gw-b ×5（同 key 同 model），逐条记录 usage
        let mut expected_total: i64 = 0;
        let mut expected_prompt: i64 = 0;
        let mut expected_completion: i64 = 0;
        for (gw, n) in [("gw-a", 10u32), ("gw-b", 5u32)] {
            let client = env.gateway(gw).client_with(KEY_MAIN);
            for i in 0..n {
                let out = client.chat(&model, &format!("MI-001 {gw} #{i}")).await?;
                anyhow::ensure!(
                    out.status == 200,
                    "{gw} #{i}: status={} body={}",
                    out.status,
                    out.body_snippet
                );
                let u = out
                    .usage
                    .ok_or_else(|| anyhow::anyhow!("{gw} #{i}: 响应缺 usage"))?;
                expected_prompt += u.prompt_tokens;
                expected_completion += u.completion_tokens;
                expected_total += u.total_tokens;
            }
        }
        ev.note(format!(
            "请求 15/15 成功：Σ prompt={expected_prompt} completion={expected_completion} total={expected_total}"
        ));

        // 2. poll 两实例新水位行（≤95s = 3×interval + 余量）
        let timeout = Duration::from_secs(95);
        let step = Duration::from_secs(2);
        for (inst, base) in [("gw-a", base_a), ("gw-b", base_b)] {
            let seq = env
                .db
                .poll_scalar(
                    timeout,
                    step,
                    "SELECT COALESCE(MAX(seq), -1) FROM boom_quota_delta WHERE instance_id = $1",
                    vec![Bind::S(inst.to_string())],
                    move |v| v.as_i64().filter(|s| *s > base),
                )
                .await?;
            ev.note(format!("{inst}: 水位 seq {base} → {seq}"));
        }

        // 3. cumulative 收敛断言（精确相等）
        let tin = cumulative(env, KEY_MAIN, CumSuffix::TotalInputTokens).await?;
        let tout = cumulative(env, KEY_MAIN, CumSuffix::TotalOutputTokens).await?;
        ev.expect_eq("cumulative tin", tin, expected_prompt)?;
        ev.expect_eq("cumulative tout", tout, expected_completion)?;
        ev.expect_eq("cumulative tin+tout", tin + tout, expected_total)?;

        Ok(ev.lines)
    })
}

// ── MI-002 ──
// 准入语义（代码核实）：total_token_limit 判定 = 当前计数（g_* 基数 + 本地未推增量）≥ limit，
// 不做在途请求前瞻。剧本：limit=2000，A 发 31 请求（31×66=2046，逐步准入时 ≤1980<2000 全放行），
// B 在 A 增量 commit 且自身拉取之后发 1 请求 → 当前计数 2046 ≥ 2000 → 429。
// mock 定长 240 chars → 每请求 completion=60；prompt 24~26 chars → 6 tokens；total=66/请求。
const KEY_MI002: &str = "sk-e2e-key-mi002";
const PLAN_MI002: &str = "e2e-mi002-ttl2000";
const TOK_PER_REQ: i64 = 66;

fn mi_002<'a>(env: &'a TestEnv) -> BoxFuture<'a, anyhow::Result<Vec<String>>> {
    Box::pin(async move {
        let mut ev = Evidence::default();
        let model = env.model_name.clone();

        // 0. 用例级 fixture：plan(total_token_limit=2000) + key + assignment + bump
        let kh = e2e_tools::keys::key_hash(KEY_MI002);
        seed_extra(
            env,
            "mi002",
            format!(
                "INSERT INTO boom_verification_token (token, key_name, user_id, models, spend)\n\
                 VALUES ('{kh}', 'e2e-mi002', 'e2e-user', '{{}}', 0.0)\n\
                 ON CONFLICT (token) DO NOTHING;\n\n\
                 INSERT INTO boom_rate_limit_plan (name, type, total_token_limit, window_limits, schedule, is_default, source)\n\
                 VALUES ('{PLAN_MI002}', 'key', 2000, '[]'::jsonb, '[]'::jsonb, false, 'db')\n\
                 ON CONFLICT (name) DO NOTHING;\n\n\
                 INSERT INTO boom_key_plan_assignment (key_hash, plan_name)\n\
                 VALUES ('{kh}', '{PLAN_MI002}')\n\
                 ON CONFLICT (key_hash) DO NOTHING;\n\n"
            ) + &e2e_envs::fixture::bump_categories_sql(&["plan", "assignment"]),
        )?;

        // plan 传播就绪探测：warmup key（ttl=1，已 assignment）请求必 429；
        // 未就绪期间请求会 200（无限制），poll 直到 429 出现。用 gw-b 探（判定方就是它）。
        let warm = env.gateway("gw-b").client_with(e2e_envs::fixture::KEY_WARMUP);
        let warm_ready = e2e_tools::poll::poll_until(
            Duration::from_secs(30),
            Duration::from_secs(2),
            || {
                let warm = warm.clone();
                let model = model.clone();
                Box::pin(async move {
                    let out = warm.chat(&model, "MI-002 warmup").await?;
                    println!(
                        "[mi002] warmup 传播探测: status={} body={:?}",
                        out.status, out.body_snippet
                    );
                    Ok(if out.status == 429 { Some(()) } else { None })
                })
            },
        )
        .await;
        ev.note(format!(
            "plan 传播就绪（gw-b warmup 429）：{}",
            if warm_ready.is_ok() { "yes" } else { "NO（30s 未现 429）" }
        ));
        warm_ready?;

        let base_a = max_seq(env, "gw-a").await?;

        // 1. B 预热 1 请求：shared 拉取只刷新本地已跟踪的 cumulative key——
        //    B 必须先服务过该 key，后续周期才会拉到它的全局基数（首触盲区 = MI-019 靶子）
        let client_b = env.gateway("gw-b").client_with(KEY_MI002);
        let out = client_b.chat(&model, "MI-002 gw-b prewarm").await?;
        anyhow::ensure!(
            out.status == 200,
            "gw-b prewarm: status={} body={}",
            out.status,
            out.body_snippet
        );
        let prewarm = out
            .usage
            .ok_or_else(|| anyhow::anyhow!("gw-b prewarm: 缺 usage"))?
            .total_tokens;
        ev.note(format!("gw-b prewarm usage={prewarm}（key 进入 B 的跟踪集）"));

        // 2. gw-a 发 31 请求，全部放行（逐步准入 ≤1980 < 2000；
        //    burst 须在 gw-a 首个同步周期（boot+90s）前完成，否则基数拉取会改变准入算术）
        let client_a = env.gateway("gw-a").client_with(KEY_MI002);
        let mut sum_a: i64 = 0;
        for i in 0..31 {
            let out = client_a.chat(&model, &format!("MI-002 gw-a #{i}")).await?;
            anyhow::ensure!(
                out.status == 200,
                "gw-a #{i}: 意外 {} body={}",
                out.status,
                out.body_snippet
            );
            sum_a += out
                .usage
                .ok_or_else(|| anyhow::anyhow!("gw-a #{i}: 缺 usage"))?
                .total_tokens;
        }
        ev.expect_eq("Σ gw-a usage（预期 31×66）", sum_a, 31 * TOK_PER_REQ)?;

        // 3. poll gw-a 水位（2046 commit 落地）→ 读 b_mark → poll gw-b 水位 > b_mark。
        //    b_mark 在观测到 A commit 之后读取，故"水位 > b_mark"的 B 周期必然始于
        //    A commit 之后——其同事务读回的全局基数已含 2046+prewarm。
        let timeout = Duration::from_secs(95);
        let step = Duration::from_secs(2);
        let seq_a = env
            .db
            .poll_scalar(
                timeout,
                step,
                "SELECT COALESCE(MAX(seq), -1) FROM boom_quota_delta WHERE instance_id = $1",
                vec![Bind::S("gw-a".to_string())],
                move |v| v.as_i64().filter(|s| *s > base_a),
            )
            .await?;
        let b_mark = max_seq(env, "gw-b").await?;
        let seq_b = env
            .db
            .poll_scalar(
                timeout,
                step,
                "SELECT COALESCE(MAX(seq), -1) FROM boom_quota_delta WHERE instance_id = $1",
                vec![Bind::S("gw-b".to_string())],
                move |v| v.as_i64().filter(|s| *s > b_mark),
            )
            .await?;
        ev.note(format!(
            "水位：gw-a {base_a}→{seq_a}，gw-b mark {b_mark}→{seq_b}（B 基数已含 A 增量）"
        ));

        // 4. gw-b 发 1 请求 → 429（计数 2046+prewarm ≥ 2000）
        let out = client_b.chat(&model, "MI-002 gw-b overflow").await?;
        ev.expect_eq("gw-b overflow 请求 status", out.status, 429)?;
        ev.expect_true(
            "429 body 含 key_total_token（限流类型正确）",
            out.body_snippet.contains("key_total_token"),
        )?;

        // 5. DB 总量 == A 消耗 + B prewarm（被拒请求不入库）
        let tin = cumulative(env, KEY_MI002, CumSuffix::TotalInputTokens).await?;
        let tout = cumulative(env, KEY_MI002, CumSuffix::TotalOutputTokens).await?;
        ev.expect_eq(
            "cumulative tin+tout == Σ A usage + prewarm",
            tin + tout,
            sum_a + prewarm,
        )?;

        Ok(ev.lines)
    })
}

// ── MI-019 ──
// 首触盲区（代码核实 sliding_window.rs sync_delta_cycle）：shared 拉取范围 =
// self.cumulative 中本地已跟踪的 key。实例从未服务过的 key 无条目 → 拉取永不含它
// → 判定计数从 0 起步，全局已超限仍放行至本地计数 ~limit——N 实例可放大 key 预算 ~N 倍。
const KEY_MI019: &str = "sk-e2e-key-mi019";
const PLAN_MI019: &str = "e2e-mi019-ttl1000";
const TOK_PER_REQ_MI019: i64 = 66; // 同 MI-002：mock 定长 240 chars

fn mi_019<'a>(env: &'a TestEnv) -> BoxFuture<'a, anyhow::Result<Vec<String>>> {
    Box::pin(async move {
        let mut ev = Evidence::default();
        let model = env.model_name.clone();

        // 0. 用例级 fixture：plan(total_token_limit=1000) + key + assignment + bump
        let kh = e2e_tools::keys::key_hash(KEY_MI019);
        seed_extra(
            env,
            "mi019",
            format!(
                "INSERT INTO boom_verification_token (token, key_name, user_id, models, spend)\n\
                 VALUES ('{kh}', 'e2e-mi019', 'e2e-user', '{{}}', 0.0)\n\
                 ON CONFLICT (token) DO NOTHING;\n\n\
                 INSERT INTO boom_rate_limit_plan (name, type, total_token_limit, window_limits, schedule, is_default, source)\n\
                 VALUES ('{PLAN_MI019}', 'key', 1000, '[]'::jsonb, '[]'::jsonb, false, 'db')\n\
                 ON CONFLICT (name) DO NOTHING;\n\n\
                 INSERT INTO boom_key_plan_assignment (key_hash, plan_name)\n\
                 VALUES ('{kh}', '{PLAN_MI019}')\n\
                 ON CONFLICT (key_hash) DO NOTHING;\n\n"
            ) + &e2e_envs::fixture::bump_categories_sql(&["plan", "assignment"]),
        )?;

        // 无 warmup 门控：本用例的红（B 首触放行）与 plan 派发是否命中 MI-018 竞态无关
        // ——B 侧无论 plan 生效与否，首触计数都是 0，观察结果同为放行。
        //
        // B 侧观察标记：给 gw-b 打一笔非靶 key（KEY_MAIN）流量。零流量的 B 走周期
        // early-return（无 delta 无跟踪 key）从不写水位行，"B 已完成周期"将不可观察；
        // 有任一跟踪 key 后 shared 模式每周期都写水位。与靶 key 的首触盲区正交。
        let marker = env.gateway("gw-b").client_with(KEY_MAIN);
        let out = marker.chat(&model, "MI-019 gw-b marker").await?;
        anyhow::ensure!(
            out.status == 200,
            "gw-b marker: status={} body={}",
            out.status,
            out.body_snippet
        );

        let base_a = max_seq(env, "gw-a").await?;

        // 1. gw-a 发 16 请求全放行（16×66=1056 ≥ 1000；plan 生效时第 16 个前置计数
        //    990<1000 仍放行；assignment 被 MI-018 抹掉时同样全放行——两条路径 Σ 一致）
        let client_a = env.gateway("gw-a").client_with(KEY_MI019);
        let mut sum_a: i64 = 0;
        for i in 0..16 {
            let out = client_a.chat(&model, &format!("MI-019 gw-a #{i}")).await?;
            anyhow::ensure!(
                out.status == 200,
                "gw-a #{i}: 意外 {} body={}",
                out.status,
                out.body_snippet
            );
            sum_a += out
                .usage
                .ok_or_else(|| anyhow::anyhow!("gw-a #{i}: 缺 usage"))?
                .total_tokens;
        }
        ev.expect_eq(
            "Σ gw-a usage（预期 16×66）",
            sum_a,
            16 * TOK_PER_REQ_MI019,
        )?;

        // 2. 等 A 上推落地，再等 B 在 A commit 之后完成同步周期（该周期拉全局——
        //    但 B 从未服务过该 key，无 cumulative 条目，拉取范围不含它：这正是靶子）
        let timeout = Duration::from_secs(95);
        let step = Duration::from_secs(2);
        let seq_a = env
            .db
            .poll_scalar(
                timeout,
                step,
                "SELECT COALESCE(MAX(seq), -1) FROM boom_quota_delta WHERE instance_id = $1",
                vec![Bind::S("gw-a".to_string())],
                move |v| v.as_i64().filter(|s| *s > base_a),
            )
            .await?;
        let b_mark = max_seq(env, "gw-b").await?;
        let seq_b = env
            .db
            .poll_scalar(
                timeout,
                step,
                "SELECT COALESCE(MAX(seq), -1) FROM boom_quota_delta WHERE instance_id = $1",
                vec![Bind::S("gw-b".to_string())],
                move |v| v.as_i64().filter(|s| *s > b_mark),
            )
            .await?;
        ev.note(format!(
            "水位：gw-a {base_a}→{seq_a}，gw-b mark {b_mark}→{seq_b}（B 已完成 A commit 后周期）"
        ));

        // 3. gw-b 首触该 key：口径声明（全局基数+本地增量）应 429。
        //    现状 = 200（拉取不含未跟踪 key → 计数 0 起步）——跑红即 bug 证据。
        let client_b = env.gateway("gw-b").client_with(KEY_MI019);
        let out = client_b.chat(&model, "MI-019 gw-b first-touch").await?;
        if out.status != 429 {
            anyhow::bail!(
                "gw-b 首触 status={}（body 含 key_total_token: {}）——全局已耗 {sum_a} ≥ 1000 \
                 仍放行：shared 拉取范围只含本地已跟踪 key，B 对未服务过的 key 无全局基数\
                 （MI-019 bug 证据；修复后此处应为 429）",
                out.status,
                out.body_snippet.contains("key_total_token")
            );
        }
        ev.note("gw-b 首触 429：首触基数已含全局（盲区已修复？）——复核后转 automated");

        // 修复后的完整性断言：被拒不入库
        let tin = cumulative(env, KEY_MI019, CumSuffix::TotalInputTokens).await?;
        let tout = cumulative(env, KEY_MI019, CumSuffix::TotalOutputTokens).await?;
        ev.expect_eq("cumulative tin+tout == Σ A usage", tin + tout, sum_a)?;

        Ok(ev.lines)
    })
}
