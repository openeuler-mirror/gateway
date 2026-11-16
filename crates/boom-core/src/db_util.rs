/// GaussDB-compatible upsert pattern.
///
/// GaussDB does not support `ON CONFLICT DO UPDATE`, so we emulate it with:
/// UPDATE → if no rows affected → INSERT → if INSERT fails (concurrent race) → UPDATE again.
///
/// **Autocommit only.** The INSERT-failure fallback relies on the failed
/// statement not poisoning anything else, which holds when executing on a
/// `Pool` (each statement is its own transaction). Inside an explicit
/// transaction a failed INSERT aborts the whole transaction in the
/// PG family (GaussDB included) and the blind UPDATE retry would itself
/// fail with "current transaction is aborted" — use
/// [`gaussdb_upsert_tx!`](crate::gaussdb_upsert_tx) there.
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

/// Transaction-scoped variant of [`gaussdb_upsert!`](crate::gaussdb_upsert).
///
/// A failed INSERT aborts the surrounding transaction (PG-family
/// semantics), so the pool-level macro's blind UPDATE retry cannot recover
/// there. This variant wraps the INSERT in a SAVEPOINT: on a concurrent
/// first-insert race it rolls back to the savepoint — restoring the
/// transaction — and re-runs the UPDATE, which now sees the winner's
/// committed row. If the row is still absent after the retry the INSERT
/// failed for a reason other than the race and the original error
/// propagates.
///
/// Usage:
/// ```ignore
/// gaussdb_upsert_tx!(tx,
///     || sqlx::query("UPDATE t SET v=v+$2 WHERE id=$1").bind(id).bind(&delta),
///     || sqlx::query("INSERT INTO t (id,v) VALUES ($1,$2)").bind(id).bind(&delta),
/// )?;
/// ```
#[macro_export]
macro_rules! gaussdb_upsert_tx {
    ($tx:expr, $update_fn:expr, $insert_fn:expr) => {{
        let updated = ($update_fn)().execute(&mut *$tx).await?;
        if updated.rows_affected() == 0 {
            sqlx::query("SAVEPOINT boom_upsert").execute(&mut *$tx).await?;
            let outcome: Result<(), sqlx::Error> = match ($insert_fn)()
                .execute(&mut *$tx)
                .await
            {
                Ok(_) => sqlx::query("RELEASE SAVEPOINT boom_upsert")
                    .execute(&mut *$tx)
                    .await
                    .map(|_| ()),
                Err(insert_err) => {
                    // Concurrent insert race (unique violation) or a real
                    // failure — either way restore the transaction, then let
                    // the UPDATE decide: row present ⇒ race, delta semantics
                    // land correctly; row absent ⇒ surface the INSERT error.
                    sqlx::query("ROLLBACK TO SAVEPOINT boom_upsert")
                        .execute(&mut *$tx)
                        .await?;
                    let res = ($update_fn)().execute(&mut *$tx).await?;
                    if res.rows_affected() == 0 {
                        Err(insert_err)
                    } else {
                        Ok(())
                    }
                }
            };
            outcome?;
        }
        Ok::<(), sqlx::Error>(())
    }};
}
