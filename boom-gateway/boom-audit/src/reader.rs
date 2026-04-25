use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{FromRow, PgPool};

/// Query parameters for listing request logs.
#[derive(Debug, Deserialize)]
pub struct ListLogsQuery {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_per_page")]
    pub per_page: i64,
    pub key_hash: Option<String>,
    pub model: Option<String>,
    pub status: Option<String>,
}

fn default_page() -> i64 {
    1
}
fn default_per_page() -> i64 {
    50
}

/// A single log row from boom_request_log.
#[derive(Debug, FromRow)]
pub struct LogRow {
    pub request_id: Option<String>,
    pub key_hash: String,
    pub key_name: Option<String>,
    pub team_id: Option<String>,
    pub model: String,
    pub api_path: String,
    pub is_stream: bool,
    pub status_code: i16,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub duration_ms: Option<i32>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Paginated result for listing logs.
pub struct LogsPage {
    pub logs: Vec<LogRow>,
    pub page: i64,
    pub per_page: i64,
    pub total: i64,
}

/// List request logs with optional filters and pagination.
pub async fn list_logs(pool: &PgPool, query: ListLogsQuery) -> Result<LogsPage, sqlx::Error> {
    let offset = (query.page - 1).max(0) * query.per_page;

    // Build WHERE clause dynamically.
    let mut where_clauses = Vec::new();
    let mut param_idx = 1u32;

    let key_hash_param = if query.key_hash.is_some() {
        let i = param_idx;
        param_idx += 1;
        Some(i)
    } else {
        None
    };
    let model_param = if query.model.is_some() {
        let i = param_idx;
        param_idx += 1;
        Some(i)
    } else {
        None
    };
    let status_param = if query.status.as_deref() == Some("error") {
        let i = param_idx;
        param_idx += 1;
        Some(i)
    } else {
        None
    };

    if query.key_hash.is_some() {
        where_clauses.push(format!("key_hash = ${}", key_hash_param.unwrap()));
    }
    if query.model.is_some() {
        where_clauses.push(format!("model = ${}", model_param.unwrap()));
    }
    if query.status.as_deref() == Some("error") {
        where_clauses.push(format!("status_code != ${}", status_param.unwrap()));
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
        r#"SELECT request_id, key_hash, key_name, team_id, model, api_path,
                  is_stream, status_code, error_type, error_message,
                  input_tokens, output_tokens, duration_ms, created_at
           FROM boom_request_log
           {where_sql}
           ORDER BY created_at DESC
           LIMIT ${limit_idx} OFFSET ${offset_idx}"#,
    );

    let count_sql = format!(r#"SELECT COUNT(*) FROM boom_request_log {where_sql}"#,);

    let mut q = sqlx::query_as::<_, LogRow>(&sql);
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);

    if let Some(ref v) = query.key_hash {
        q = q.bind(v);
        cq = cq.bind(v);
    }
    if let Some(ref v) = query.model {
        q = q.bind(v);
        cq = cq.bind(v);
    }
    if query.status.as_deref() == Some("error") {
        q = q.bind(200i16);
        cq = cq.bind(200i16);
    }

    q = q.bind(query.per_page).bind(offset);

    let rows = q.fetch_all(pool).await?;
    let total = cq.fetch_one(pool).await.unwrap_or(0);

    Ok(LogsPage {
        logs: rows,
        page: query.page,
        per_page: query.per_page,
        total,
    })
}

/// Convert a LogsPage into a JSON response value.
pub fn logs_page_to_json(page: LogsPage) -> Value {
    let logs: Vec<Value> = page
        .logs
        .into_iter()
        .map(|r| {
            json!({
                "request_id": r.request_id,
                "key_hash": r.key_hash,
                "key_name": r.key_name,
                "team_id": r.team_id,
                "model": r.model,
                "api_path": r.api_path,
                "is_stream": r.is_stream,
                "status_code": r.status_code,
                "error_type": r.error_type,
                "error_message": r.error_message,
                "input_tokens": r.input_tokens,
                "output_tokens": r.output_tokens,
                "duration_ms": r.duration_ms,
                "created_at": r.created_at.map(|d| d.to_rfc3339()),
            })
        })
        .collect();

    json!({
        "logs": logs,
        "page": page.page,
        "per_page": page.per_page,
        "total": page.total,
    })
}
