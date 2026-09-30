//! A failed migration must not wedge the next one.
//!
//! sqlx holds a session-level advisory lock across `Migrator::run`, and several error paths return
//! before the unlock. Run on a pooled connection, a failure sends that connection back to the pool
//! still holding the lock, and every later attempt blocks rather than reporting the original error.

use sqlx::{AssertSqlSafe, PgPool};

fn base() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set; this test must not skip")
}

#[tokio::test]
async fn a_failed_migration_leaves_no_advisory_lock_behind() {
    let b = base();
    let cut = b.rfind('/').unwrap();
    let admin = PgPool::connect(&format!("{}/postgres", &b[..cut])).await.expect("admin");
    // The pid keeps two checkouts on one cluster apart: `DROP ... WITH (FORCE)` on a shared name
    // kills the other run's probe connection mid-test.
    let probe = format!("migration_lock_probe_{}", std::process::id());
    let _ = sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {probe} WITH (FORCE)")))
        .execute(&admin)
        .await;
    sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {probe}")))
        .execute(&admin)
        .await
        .expect("create");

    let url = format!("{}/{probe}", &b[..cut]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1) // one connection, so a leaked lock is observable
        .connect(&url)
        .await
        .expect("connect");

    // Poison it: a recorded migration this build does not have. That is the shape of a merge that
    // dropped a file, and it fails after the lock is taken.
    lumberroom_server::adapters::postgres::migrate(&pool).await.expect("first migrate");
    sqlx::raw_sql(AssertSqlSafe(
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time)
         VALUES (99999999999999, 'a migration this build does not have', now(), true, '\\x00', 0)"))
        .execute(&pool).await.expect("poison");

    let failed = lumberroom_server::adapters::postgres::migrate(&pool).await;
    assert!(failed.is_err(), "the poisoned migration set was accepted");

    // This database's locks only. `pg_locks` lists the whole cluster, so another test binary's suite
    // lock, or another checkout's suite on the same cluster, counted here as a leak. sqlx takes its
    // migration lock inside the database it migrates, so a real leak still shows up here.
    let held: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks
          WHERE locktype = 'advisory'
            AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .fetch_one(&pool)
    .await
    .expect("lock count");
    assert_eq!(held, 0, "a failed migration left {held} advisory lock(s) held");

    // And the real proof: a second attempt returns its error instead of hanging.
    let again = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        lumberroom_server::adapters::postgres::migrate(&pool),
    )
    .await;
    assert!(again.is_ok(), "the second migrate blocked on the leaked lock");
    assert!(again.unwrap().is_err(), "the second migrate should still report the same problem");

    pool.close().await;
    let _ = sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {probe} WITH (FORCE)")))
        .execute(&admin)
        .await;
}
