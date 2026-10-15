//! Every trigger must survive `pg_dump` and `pg_restore`.
//!
//! `pg_dump` writes each trigger as `pg_get_triggerdef` prints it with `search_path` empty, and
//! `pg_restore` replays that text with `search_path` still empty. A WHEN clause that only resolves
//! through `public` restores as an error. `memory_conflict_moved` shipped in 0.5.0 comparing two
//! vectors with `IS DISTINCT FROM`, which prints with no schema on its operator, so every dump
//! taken from 0.5.0 stopped `pg_restore --exit-on-error` at that trigger.
//!
//! The check replays the same text the same way, for every trigger the migrations leave in
//! `public`, so a trigger a later migration or a fork adds is covered without naming it here.

use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};

fn base() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set; this test must not skip")
}

#[tokio::test]
async fn every_trigger_recreates_from_its_dumped_definition() {
    let b = base();
    let cut = b.rfind('/').unwrap();
    let admin = PgPool::connect(&format!("{}/postgres", &b[..cut])).await.expect("admin");
    // A database of its own, named by pid: dropping a trigger takes an exclusive lock on its table,
    // which would stall every other binary reading the shared test database, and the shared one may
    // carry another branch's migrations.
    let probe = format!("trigger_restore_probe_{}", std::process::id());
    let _ = sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {probe} WITH (FORCE)")))
        .execute(&admin)
        .await;
    sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {probe}")))
        .execute(&admin)
        .await
        .expect("create");

    let url = format!("{}/{probe}", &b[..cut]);
    {
        let pool = PgPool::connect(&url).await.expect("connect");
        lumberroom_server::adapters::postgres::migrate(&pool).await.expect("migrate");
        pool.close().await;
    }

    let mut conn = PgConnection::connect(&url).await.expect("connect");
    let mut tx = conn.begin().await.expect("begin");
    sqlx::raw_sql(AssertSqlSafe("SET LOCAL search_path = ''")).execute(&mut *tx).await.unwrap();

    // Read under the empty path, as pg_dump does: the text names every object it cannot reach
    // unqualified, so `tgrelid::regclass` prints `public.memory` and DROP finds it.
    let triggers: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT t.tgname::text, t.tgrelid::regclass::text, pg_get_triggerdef(t.oid)
           FROM pg_catalog.pg_trigger t
           JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'public' AND NOT t.tgisinternal
          ORDER BY 2, 1",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("catalog read");
    assert!(
        triggers.iter().any(|(name, _, _)| name == "memory_conflict_moved"),
        "the catalog read found none of the triggers it exists to check: {triggers:?}"
    );

    let mut broken = Vec::new();
    for (name, table, def) in &triggers {
        // Generated DDL: the trigger and table names come from the catalog, quoted by quote_ident
        // or regclass output, and the definition is Postgres's own text. A savepoint per trigger so
        // one failure leaves the transaction usable for the rest.
        sqlx::raw_sql(AssertSqlSafe("SAVEPOINT replay")).execute(&mut *tx).await.unwrap();
        let ident: String = sqlx::query_scalar("SELECT pg_catalog.quote_ident($1)")
            .bind(name)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let replay = format!("DROP TRIGGER {ident} ON {table}; {def}");
        if let Err(e) = sqlx::raw_sql(AssertSqlSafe(replay)).execute(&mut *tx).await {
            broken.push(format!("{name} on {table}: {e}\n    {def}"));
        }
        sqlx::raw_sql(AssertSqlSafe("ROLLBACK TO SAVEPOINT replay"))
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.rollback().await.expect("rollback");
    conn.close().await.expect("close");

    let _ = sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {probe} WITH (FORCE)")))
        .execute(&admin)
        .await;
    assert!(
        broken.is_empty(),
        "these triggers do not recreate from their pg_dump text, so a restore fails on them:\n{}",
        broken.join("\n")
    );
}
