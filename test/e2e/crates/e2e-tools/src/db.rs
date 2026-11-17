//! DB 探针：对共享 Postgres 的只读取证查询 + 基于 poll_until 的等待。

use std::time::Duration;

use serde_json::Value;
use sqlx::{Column, PgPool, Row, TypeInfo};

#[derive(Clone)]
pub struct DbProbe {
    pool: PgPool,
}

/// 绑定参数（sqlx 的 String bind 会声明 TEXT 类型，与 BIGINT 列比较会炸，故显式分型）
#[derive(Clone, Debug)]
pub enum Bind {
    I(i64),
    F(f64),
    S(String),
    B(bool),
}

impl DbProbe {
    pub async fn connect(dsn: &str) -> anyhow::Result<Self> {
        Ok(Self {
            pool: PgPool::connect(dsn).await?,
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 单行单列 → JSON Value（SQL 无行 → 错误；NULL → Value::Null）
    pub async fn scalar(&self, sql: &str, binds: &[Bind]) -> anyhow::Result<Value> {
        let row = self.query_opt(sql, binds).await?;
        let row = row.ok_or_else(|| anyhow::anyhow!("scalar query returned no rows"))?;
        Ok(cell_to_value(&row, 0))
    }

    /// 单行单列，NULL 时返回 None
    pub async fn scalar_opt(&self, sql: &str, binds: &[Bind]) -> anyhow::Result<Option<Value>> {
        Ok(self
            .query_opt(sql, binds)
            .await?
            .map(|row| cell_to_value(&row, 0)))
    }

    pub async fn scalar_i64(&self, sql: &str, binds: &[Bind]) -> anyhow::Result<i64> {
        match self.scalar_opt(sql, binds).await? {
            Some(Value::Number(n)) => Ok(n
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("scalar_i64: not an i64: {n}"))?),
            Some(v) => anyhow::bail!("scalar_i64: not a number: {v}"),
            None => anyhow::bail!("scalar_i64: NULL"),
        }
    }

    /// 通用行读取：每行一个 JSON object（key = 列名）。TIMESTAMP 序列化为 epoch 秒。
    pub async fn rows(&self, sql: &str, binds: &[Bind]) -> anyhow::Result<Vec<Value>> {
        let rows = self.query_all(sql, binds).await?;
        Ok(rows
            .iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                for (i, col) in row.columns().iter().enumerate() {
                    obj.insert(col.name().to_string(), cell_to_value(row, i));
                }
                Value::Object(obj)
            })
            .collect())
    }

    /// poll 版：反复执行 scalar，谓词满足返回携带值。禁止固定 sleep 的 DB 等待统一走这里。
    pub async fn poll_scalar<T, P>(
        &self,
        timeout: Duration,
        step: Duration,
        sql: &str,
        binds: Vec<Bind>,
        pred: P,
    ) -> anyhow::Result<T>
    where
        T: Clone + std::fmt::Debug + 'static,
        P: Fn(&Value) -> Option<T> + Send + Sync + Clone + 'static,
    {
        crate::poll::poll_until(timeout, step, move || {
            let db = self.clone();
            let sql = sql.to_string();
            let binds = binds.clone();
            let pred = pred.clone();
            Box::pin(async move {
                let v = db.scalar(&sql, &binds).await?;
                Ok(pred(&v))
            })
        })
        .await
    }

    async fn query_opt(
        &self,
        sql: &str,
        binds: &[Bind],
    ) -> anyhow::Result<Option<sqlx::postgres::PgRow>> {
        let mut q = sqlx::query(sql);
        for b in binds {
            q = bind_one(q, b);
        }
        Ok(q.fetch_optional(&self.pool).await?)
    }

    async fn query_all(
        &self,
        sql: &str,
        binds: &[Bind],
    ) -> anyhow::Result<Vec<sqlx::postgres::PgRow>> {
        let mut q = sqlx::query(sql);
        for b in binds {
            q = bind_one(q, b);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }
}

fn bind_one<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    b: &Bind,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match b {
        Bind::I(v) => q.bind(*v),
        Bind::F(v) => q.bind(*v),
        Bind::S(v) => q.bind(v.clone()),
        Bind::B(v) => q.bind(*v),
    }
}

/// PG 列 → JSON Value。NULL 全部走 Option 先判；不认识的类型降级为字符串。
fn cell_to_value(row: &sqlx::postgres::PgRow, idx: usize) -> Value {
    let ty = row.columns()[idx].type_info().name().to_string();
    match ty.as_str() {
        "INT2" => row
            .try_get::<Option<i16>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v)),
        "INT4" => row
            .try_get::<Option<i32>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v)),
        "INT8" => row
            .try_get::<Option<i64>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v)),
        "FLOAT4" => row
            .try_get::<Option<f32>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v)),
        "FLOAT8" => row
            .try_get::<Option<f64>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v)),
        "BOOL" => row
            .try_get::<Option<bool>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v)),
        "NUMERIC" => row
            .try_get::<Option<String>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v)),
        "JSON" | "JSONB" => row
            .try_get::<Option<sqlx::types::Json<Value>>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v.0)),
        "TIMESTAMPTZ" => row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v.timestamp())),
        "TIMESTAMP" => row
            .try_get::<Option<chrono::NaiveDateTime>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v.and_utc().timestamp())),
        // TEXT / VARCHAR / NAME / BPCHAR / 其它
        _ => row
            .try_get::<Option<String>, _>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::json!(v)),
    }
    .unwrap_or(Value::Null)
}
