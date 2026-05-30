/// GaussDB-compatible upsert pattern.
///
/// GaussDB does not support `ON CONFLICT DO UPDATE`, so we emulate it with:
/// UPDATE → if no rows affected → INSERT → if INSERT fails (concurrent race) → UPDATE again.
///
/// Usage:
/// ```ignore
/// gaussdb_upsert(pool,
///     || sqlx::query("UPDATE t SET v=$2 WHERE id=$1").bind(id).bind(&val),
///     || sqlx::query("INSERT INTO t (id,v) VALUES ($1,$2)").bind(id).bind(&val),
/// ).await?;
/// ```
#[macro_export]
macro_rules! gaussdb_upsert {
    ($pool:expr, $update_fn:expr, $insert_fn:expr) => {{
        let updated = ($update_fn)().execute($pool).await?;
        if updated.rows_affected() == 0 {
            if ($insert_fn)().execute($pool).await.is_err() {
                // Concurrent insert race — retry update.
                ($update_fn)().execute($pool).await?;
            }
        }
        Ok::<(), sqlx::Error>(())
    }};
}
