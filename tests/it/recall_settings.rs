//! The recall settings live in a cluster catalog that a single-database `pg_dump` does not carry,
//! so a restored database comes back with the migration recorded as applied and the settings gone.
//! Boot re-applies them. These tests prove both halves against a real database.

use sqlx::{AssertSqlSafe, PgPool};

fn base() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set; this test must not skip")
}

async fn scratch(name: &str) -> (PgPool, PgPool) {
    let b = base();
    let cut = b.rfind('/').unwrap();
    let admin = PgPool::connect(&format!("{}/postgres", &b[..cut])).await.expect("admin");
    let _ = sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")))
        .execute(&admin)
        .await;
    sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create");
    let db = PgPool::connect(&format!("{}/{name}", &b[..cut])).await.expect("connect");
    (db, admin)
}

async fn settings(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT unnest(s.setconfig) FROM pg_db_role_setting s
           JOIN pg_database d ON d.oid = s.setdatabase
          WHERE d.datname = current_database() AND s.setrole = 0",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

#[tokio::test]
async fn boot_reapplies_settings_a_restore_dropped() {
    let (db, admin) = scratch("recall_settings_probe").await;

    // A restored database: the schema is there, the cluster catalog entry is not.
    assert!(settings(&db).await.is_empty(), "the scratch database started with settings");

    lumberroom_server::adapters::postgres::ensure_recall_settings(&db).await.unwrap();

    let after = settings(&db).await;
    assert!(
        after.iter().any(|s| s == "hnsw.iterative_scan=strict_order"),
        "iterative_scan was not restored: {after:?}"
    );
    assert!(
        after.iter().any(|s| s == "hnsw.ef_search=100"),
        "ef_search was not restored: {after:?}"
    );

    // Idempotent: a second boot changes nothing and must not error.
    lumberroom_server::adapters::postgres::ensure_recall_settings(&db).await.unwrap();
    assert_eq!(settings(&db).await.len(), after.len());

    db.close().await;
    let _ =
        sqlx::raw_sql(AssertSqlSafe("DROP DATABASE IF EXISTS recall_settings_probe WITH (FORCE)"))
            .execute(&admin)
            .await;
}

#[tokio::test]
async fn a_database_name_with_a_quote_is_still_handled() {
    // ALTER DATABASE names an identifier, so the name is quoted rather than bound. The name comes
    // from the server, not a caller, and this pins the quoting anyway.
    let (db, admin) = scratch("recall_probe_quoted").await;
    lumberroom_server::adapters::postgres::ensure_recall_settings(&db).await.unwrap();
    assert!(settings(&db).await.iter().any(|s| s.starts_with("hnsw.iterative_scan")));
    db.close().await;
    let _ =
        sqlx::raw_sql(AssertSqlSafe("DROP DATABASE IF EXISTS recall_probe_quoted WITH (FORCE)"))
            .execute(&admin)
            .await;
}
