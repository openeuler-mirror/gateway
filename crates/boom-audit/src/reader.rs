//! boom_request_log 读取 API —— DDL 所有权(§3):本 crate 独占
//! boom_request_log 的全部 SQL。Dashboard 的日志/DFX 页通过本模块查询,
//! 不再手写他人表的 SQL。

use sqlx::PgPool;

/// 上限:有界精确计数的截断值。计数子查询取到该值即止(成本 O(cap)),
/// 总数达到上限时返回 total_capped,UI 显示"1000+ pages"。
pub const LOGS_COUNT_CAP: i64 = 50_000;

#[derive(Debug, Clone)]
pub struct LogsQuery {
    pub page: i64,
    pub per_page: i64,
    pub range_from: Option<chrono::DateTime<chrono::Utc>>,
    pub key_hash: Option<String>,
    pub model: Option<String>,
    pub status: Option<String>,
    pub request_id: Option<String>,
    pub key_alias: Option<String>,
    pub api_path: Option<String>,
    pub status_code: Option<i16>,
    pub stream: Option<String>,
    pub error: Option<String>,
    pub team_alias: Option<String>,
    pub policy: Option<String>,
    pub instance_id: Option<String>,
    pub client_ip: Option<String>,
}

pub struct LogsPage {
    pub logs: Vec<LogRow>,
    pub page: i64,
    pub per_page: i64,
    pub total: i64,
    pub total_capped: bool,
}

/// 全量行(日志/DFX 展示所需的全部列)。公开字段,消费方直接读。
#[derive(Debug, sqlx::FromRow)]
pub struct LogRow {
    pub request_id: Option<String>,
    pub key_hash: String,
    pub key_name: Option<String>,
    pub key_alias: Option<String>,
    pub team_id: Option<String>,
    pub team_alias: Option<String>,
    pub model: String,
    pub api_path: String,
    pub is_stream: bool,
    pub status_code: i16,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub duration_ms: Option<i32>,
    pub ttft_ms: Option<i32>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deployment_id: Option<String>,
    pub client_ip: Option<String>,
    pub cached_tokens: Option<i64>,
    pub schedule_policy: Option<String>,
    pub kv_hit_blocks: Option<i64>,
    pub kv_input_blocks: Option<i64>,
    pub trie_blocks: Option<i64>,
    pub trie_max_blocks: Option<i64>,
    pub request_tokens: Option<i64>,
    pub queue_wait_ms: Option<i32>,
    /// Instance that served (and prompt-logged) this request. The prompt-log
    /// segments are instance-local, so the dashboard uses this to decide
    /// whether to serve the entry locally or proxy to the owning instance.
    pub instance_id: Option<String>,
    pub user_tag1: Option<String>,
    pub user_tag2: Option<String>,
    pub user_tag3: Option<String>,
}

fn normalize_pagination(page: i64, per_page: i64) -> (i64, i64) {
    (page.max(1), per_page.clamp(1, 1000))
}

/// Escape LIKE/ILIKE metacharacters so a literal "%" or "_" in a filter
/// value can't widen the match to unrelated rows.
fn escape_like(v: &str) -> String {
    v.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

async fn begin_with_timeout(pool: &PgPool) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// 日志查询(带列筛选 + 分页 + 有界计数)。
///
/// 行为契约(与原 dashboard 实现逐字一致,迁移后语义不变):
/// - 列筛选:ILIKE 子串(值经 escape_like),stream 是静态 WHERE
/// - 取 per_page+1 探测 has_next;计数子查询截断在 LOGS_COUNT_CAP
/// - 主/计数双查询跑在同一事务内,10s statement_timeout 防全表扫挂死
pub async fn list_logs(pool: &PgPool, query: LogsQuery) -> Result<LogsPage, sqlx::Error> {
    let (page, per_page) = normalize_pagination(query.page, query.per_page);
    let offset = (page - 1) * per_page;

    let mut where_clauses = Vec::new();
    let mut param_idx = 1u32;
    macro_rules! slot {
        ($field:expr) => {
            if $field.is_some() { let i = param_idx; param_idx += 1; Some(i) } else { None }
        };
    }
    let range_param = if query.range_from.is_some() { let i = param_idx; param_idx += 1; Some(i) } else { None };
    let key_hash_param   = slot!(query.key_hash);
    let model_param      = slot!(query.model);
    let status_param     = if query.status.as_deref() == Some("error") { let i = param_idx; param_idx += 1; Some(i) } else { None };
    let request_id_param = slot!(query.request_id);
    let key_alias_param  = slot!(query.key_alias);
    let api_path_param   = slot!(query.api_path);
    let status_code_param= slot!(query.status_code);
    let error_param      = slot!(query.error);
    let team_alias_param = slot!(query.team_alias);
    let client_ip_param  = slot!(query.client_ip);
    let policy_param     = slot!(query.policy);
    let instance_param   = slot!(query.instance_id);

    if let Some(i) = range_param {
        where_clauses.push(format!("rl.created_at >= ${i}"));
    }
    if query.key_hash.is_some() {
        where_clauses.push(format!("rl.key_hash = ${}", key_hash_param.unwrap()));
    }
    if query.model.is_some() {
        where_clauses.push(format!("rl.model ILIKE ${}", model_param.unwrap()));
    }
    if query.status.as_deref() == Some("error") {
        where_clauses.push(format!("rl.status_code != ${}", status_param.unwrap()));
    }
    if query.request_id.is_some() {
        where_clauses.push(format!("rl.request_id ILIKE ${}", request_id_param.unwrap()));
    }
    if query.key_alias.is_some() {
        where_clauses.push(format!("(rl.key_alias ILIKE ${0} OR rl.key_name ILIKE ${0})", key_alias_param.unwrap()));
    }
    if query.api_path.is_some() {
        where_clauses.push(format!("rl.api_path ILIKE ${}", api_path_param.unwrap()));
    }
    if query.status_code.is_some() {
        where_clauses.push(format!("rl.status_code = ${}", status_code_param.unwrap()));
    }
    if query.stream.is_some() {
        let s = query.stream.as_deref().unwrap().to_lowercase();
        if s == "yes" || s == "true" || s == "1" {
            where_clauses.push("rl.is_stream = true".to_string());
        } else if s == "no" || s == "false" || s == "0" {
            where_clauses.push("rl.is_stream = false".to_string());
        }
    }
    if query.error.is_some() {
        where_clauses.push(format!("rl.error_type ILIKE ${}", error_param.unwrap()));
    }
    if query.team_alias.is_some() {
        where_clauses.push(format!("bt.team_alias ILIKE ${}", team_alias_param.unwrap()));
    }
    if query.policy.is_some() {
        where_clauses.push(format!("rl.schedule_policy ILIKE ${}", policy_param.unwrap()));
    }
    if query.instance_id.is_some() {
        where_clauses.push(format!("rl.instance_id ILIKE ${}", instance_param.unwrap()));
    }
    if query.client_ip.is_some() {
        where_clauses.push(format!("rl.client_ip ILIKE ${}", client_ip_param.unwrap()));
    }
    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };
    let limit_idx = param_idx;
    param_idx += 1;
    let offset_idx = param_idx;

    let sql = format!(
        r#"SELECT rl.request_id, rl.key_hash, rl.key_name, rl.key_alias, rl.team_id,
                  bt.team_alias,
                  rl.model, rl.api_path,
                  rl.is_stream, rl.status_code, rl.error_type, rl.error_message,
                  rl.input_tokens, rl.output_tokens, rl.duration_ms, rl.ttft_ms, rl.created_at,
                  rl.deployment_id, rl.client_ip, rl.cached_tokens,
                  rl.schedule_policy, rl.kv_hit_blocks, rl.kv_input_blocks,
                  rl.trie_blocks, rl.trie_max_blocks, rl.request_tokens,
                  rl.queue_wait_ms, rl.instance_id, rl.user_tag1, rl.user_tag2, rl.user_tag3
           FROM boom_request_log rl
           LEFT JOIN boom_team_table bt ON rl.team_id = bt.team_id
           {where_sql}
           ORDER BY rl.created_at DESC
           LIMIT ${limit_idx} OFFSET ${offset_idx}"#,
    );
    let mut q = sqlx::query_as::<_, LogRow>(&sql);

    let model_pattern      = query.model.as_ref().map(|v| format!("%{}%", escape_like(v)));
    let request_id_pattern = query.request_id.as_ref().map(|v| format!("%{}%", escape_like(v)));
    let key_alias_pattern  = query.key_alias.as_ref().map(|v| format!("%{}%", escape_like(v)));
    let api_path_pattern   = query.api_path.as_ref().map(|v| format!("%{}%", escape_like(v)));
    let error_pattern      = query.error.as_ref().map(|v| format!("%{}%", escape_like(v)));
    let team_alias_pattern = query.team_alias.as_ref().map(|v| format!("%{}%", escape_like(v)));
    let client_ip_pattern  = query.client_ip.as_ref().map(|v| format!("%{}%", escape_like(v)));
    let policy_pattern     = query.policy.as_ref().map(|v| format!("%{}%", escape_like(v)));
    let instance_pattern   = query.instance_id.as_ref().map(|v| format!("%{}%", escape_like(v)));

    if let Some(from) = query.range_from {
        q = q.bind(from);
    }
    if let Some(ref v) = query.key_hash {
        q = q.bind(v.clone());
    }
    if let Some(ref p) = model_pattern {
        q = q.bind(p.clone());
    }
    if query.status.as_deref() == Some("error") {
        q = q.bind(200i16);
    }
    if let Some(ref p) = request_id_pattern {
        q = q.bind(p.clone());
    }
    if let Some(ref p) = key_alias_pattern {
        q = q.bind(p.clone());
    }
    if let Some(ref p) = api_path_pattern {
        q = q.bind(p.clone());
    }
    if let Some(v) = query.status_code {
        q = q.bind(v);
    }
    if let Some(ref p) = error_pattern {
        q = q.bind(p.clone());
    }
    if let Some(ref p) = team_alias_pattern {
        q = q.bind(p.clone());
    }
    if let Some(ref p) = client_ip_pattern {
        q = q.bind(p.clone());
    }
    if let Some(ref p) = policy_pattern {
        q = q.bind(p.clone());
    }
    if let Some(ref p) = instance_pattern {
        q = q.bind(p.clone());
    }
    q = q.bind(per_page + 1).bind(offset);

    let count_sql = format!(
        r#"SELECT COUNT(*) FROM (
               SELECT 1 FROM boom_request_log rl
               LEFT JOIN boom_team_table bt ON rl.team_id = bt.team_id
               {where_sql}
               LIMIT ${limit_idx}
           ) t"#,
    );
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    if let Some(from) = query.range_from {
        cq = cq.bind(from);
    }
    if let Some(ref v) = query.key_hash {
        cq = cq.bind(v.clone());
    }
    if let Some(ref p) = model_pattern {
        cq = cq.bind(p.clone());
    }
    if query.status.as_deref() == Some("error") {
        cq = cq.bind(200i16);
    }
    if let Some(ref p) = request_id_pattern {
        cq = cq.bind(p.clone());
    }
    if let Some(ref p) = key_alias_pattern {
        cq = cq.bind(p.clone());
    }
    if let Some(ref p) = api_path_pattern {
        cq = cq.bind(p.clone());
    }
    if let Some(v) = query.status_code {
        cq = cq.bind(v);
    }
    if let Some(ref p) = error_pattern {
        cq = cq.bind(p.clone());
    }
    if let Some(ref p) = team_alias_pattern {
        cq = cq.bind(p.clone());
    }
    if let Some(ref p) = client_ip_pattern {
        cq = cq.bind(p.clone());
    }
    if let Some(ref p) = policy_pattern {
        cq = cq.bind(p.clone());
    }
    if let Some(ref p) = instance_pattern {
        cq = cq.bind(p.clone());
    }
    cq = cq.bind(LOGS_COUNT_CAP);

    let (rows, total): (Vec<LogRow>, i64) = match async {
        let mut tx = begin_with_timeout(pool).await?;
        let rows = q.fetch_all(&mut *tx).await?;
        let total = cq.fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>((rows, total))
    }
    .await
    {
        Ok(r) => r,
        Err(e) => return Err(e),
    };
    let total_capped = total >= LOGS_COUNT_CAP;
    Ok(LogsPage { logs: rows, page, per_page, total, total_capped })
}

/// has_next 判定:主查询取 per_page+1 探测;计数达到截断值时,
/// total >= offset+per_page+1 同样视为有下一页。
pub fn has_next(page: &LogsPage) -> bool {
    let offset = (page.page - 1) * page.per_page;
    page.logs.len() > page.per_page as usize || page.total >= offset + page.per_page + 1
}

/// 用户密钥日志(用户面板"我的请求"页):单 key 分页 + 有界计数。
/// 返回的 LogRow 只填用户面板需要的列,其余为默认 None/0。
pub async fn list_logs_for_key(
    pool: &PgPool,
    key_hash: &str,
    page: i64,
    per_page: i64,
) -> Result<(Vec<LogRow>, i64), sqlx::Error> {
    let (page, per_page) = normalize_pagination(page, per_page);
    let offset = (page - 1) * per_page;
    let (rows, total): (Vec<LogRow>, i64) = match async {
        let mut tx = begin_with_timeout(pool).await?;
        let rows = sqlx::query_as(
            r#"SELECT request_id, key_hash, key_name, model, api_path,
                      is_stream, status_code, error_type, error_message,
                      input_tokens, output_tokens, duration_ms, created_at,
                      client_ip, cached_tokens, queue_wait_ms,
                      NULL::text AS key_alias, NULL::text AS team_id,
                      NULL::text AS team_alias, 0 AS ttft_ms,
                      NULL::text AS deployment_id, NULL::text AS schedule_policy,
                      NULL::bigint AS kv_hit_blocks, NULL::bigint AS kv_input_blocks,
                      NULL::bigint AS trie_blocks, NULL::bigint AS trie_max_blocks,
                      NULL::bigint AS request_tokens, NULL::text AS instance_id,
                      NULL::text AS user_tag1, NULL::text AS user_tag2, NULL::text AS user_tag3
               FROM boom_request_log
               WHERE key_hash = $1
               ORDER BY created_at DESC
               LIMIT $2 OFFSET $3"#,
        )
        .bind(key_hash)
        .bind(per_page + 1)
        .bind(offset)
        .fetch_all(&mut *tx)
        .await?;
        let total = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM (
                   SELECT 1 FROM boom_request_log WHERE key_hash = $1 LIMIT $2
               ) t"#,
        )
        .bind(key_hash)
        .bind(LOGS_COUNT_CAP)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>((rows, total))
    }
    .await
    {
        Ok(r) => r,
        Err(e) => return Err(e),
    };
    Ok((rows, total))
}
