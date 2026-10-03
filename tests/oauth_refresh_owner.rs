//! `refresh_owner` against the real Postgres query. The unit tests in `src/authserver` use an
//! in-memory store that hard-codes the liveness rule, so none of them can fail if the SQL loses a
//! clause. This one can.
//!
//! The invariant: a spent, revoked or expired refresh token answers `None`. `refresh_caller`
//! treats `None` as "fall through to `rotate_refresh`", whose replay verdict revokes the family.
//! If a spent token answered `Some(owner)` instead, a replay presented under the wrong client_id
//! would be refused before the family kill and the kill could be dodged.
//!
//! Skipped when no database is reachable, in the shape the other integration binaries use.

use lumberroom_server::adapters::postgres;
use lumberroom_server::ports::oauth::{NewOauthClient, NewRefreshToken};
use lumberroom_server::ports::OauthStore;
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const CLIENT: &str = "client-owner-test";

/// Returns None when no database is reachable.
async fn setup() -> Option<(PgPool, common::DbGuard)> {
    let admin_url = std::env::var("DATABASE_URL").ok()?;
    let base_url = admin_url.rsplit_once('/')?.0.to_string();
    let admin = PgPool::connect(&admin_url).await.ok()?;
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(TEST_DB)
        .fetch_optional(&admin)
        .await
        .ok()?;
    if exists.is_none() {
        // Audited: TEST_DB is a compile-time constant with no external input.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {TEST_DB}")))
            .execute(&admin)
            .await
            .ok()?;
    }
    admin.close().await;

    let url = format!("{base_url}/{TEST_DB}");
    let guard = common::lock_database(&url).await?;
    let pool = postgres::connect(&url).await.ok()?;
    postgres::migrate(&pool).await.ok()?;
    // Cascades to oauth_refresh, oauth_token and oauth_code through their foreign keys.
    sqlx::query("TRUNCATE oauth_client CASCADE").execute(&pool).await.ok()?;
    Some((pool, guard))
}

async fn seed_client(store: &postgres::PgOauthStore) {
    store
        .register_client(NewOauthClient {
            client_id: CLIENT.into(),
            secret_hash: None,
            client_name: "owner test".into(),
            redirect_uris: vec!["https://tool.example/cb".into()],
            grant_types: vec!["authorization_code".into(), "refresh_token".into()],
            software_id: None,
            software_version: None,
            registered_via: "dcr".into(),
        })
        .await
        .unwrap();
}

async fn seed_refresh(store: &postgres::PgOauthStore, hash: &str, ttl_secs: i64) {
    store
        .insert_refresh(NewRefreshToken {
            token_hash: hash.into(),
            client_id: CLIENT.into(),
            family_id: uuid::Uuid::new_v4(),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(ttl_secs),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn refresh_owner_answers_only_for_a_live_token() {
    let Some((pool, _guard)) = setup().await else {
        eprintln!("skipping: no database reachable");
        return;
    };
    let store = postgres::PgOauthStore::new(pool.clone());
    seed_client(&store).await;

    seed_refresh(&store, "live", 3600).await;
    seed_refresh(&store, "consumed", 3600).await;
    seed_refresh(&store, "revoked", 3600).await;
    seed_refresh(&store, "expired", -3600).await;
    sqlx::query("UPDATE oauth_refresh SET consumed_at = now() WHERE token_hash = 'consumed'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE oauth_refresh SET revoked_at = now() WHERE token_hash = 'revoked'")
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(store.refresh_owner("live").await.unwrap().as_deref(), Some(CLIENT));
    assert_eq!(store.refresh_owner("consumed").await.unwrap(), None, "spent token");
    assert_eq!(store.refresh_owner("revoked").await.unwrap(), None, "revoked token");
    assert_eq!(store.refresh_owner("expired").await.unwrap(), None, "expired token");
    assert_eq!(store.refresh_owner("never-issued").await.unwrap(), None, "unknown token");
}

#[tokio::test]
async fn a_token_spent_by_rotate_refresh_stops_answering_for_its_owner() {
    let Some((pool, _guard)) = setup().await else {
        eprintln!("skipping: no database reachable");
        return;
    };
    let store = postgres::PgOauthStore::new(pool);
    seed_client(&store).await;
    seed_refresh(&store, "rotating", 3600).await;

    assert_eq!(store.refresh_owner("rotating").await.unwrap().as_deref(), Some(CLIENT));
    store.rotate_refresh("rotating").await.unwrap();
    assert_eq!(store.refresh_owner("rotating").await.unwrap(), None);
}
