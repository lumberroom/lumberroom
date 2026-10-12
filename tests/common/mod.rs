//! What every database test shares: one lock, and the rule for when a test may skip.
//!
//! Every integration binary here truncates `lumberroom_rust_test`, and each carries its own
//! `static SERIAL` mutex. A mutex serialises threads inside one process, and these are six
//! processes, so nothing stopped `tests/console_cleanup` truncating the table `tests/cleanup` was
//! part way through asserting against.
//!
//! It fails the way shared state always does: every file passes on its own and two of them fail in
//! a full run, with an assertion that reads like a logic bug. The two that failed were a proposal
//! whose members had gone and a survivor chosen from a cluster that had changed underneath, both of
//! which are exactly what a truncate from another process looks like from the inside.
//!
//! A Postgres advisory lock is held by a session rather than by a process, so every binary queues
//! on the same one. It is released when the connection closes, which is what dropping the guard
//! does.
//!
//! It also owns the rule for when a database test may skip. Only a database that never answered
//! is a reason to skip, and `LUMBERROOM_TEST_REQUIRE_DB` takes even that away. Every step after
//! the first answer panics: a skip there reports ok with nothing asserted, which is how a test
//! database holding another checkout's migrations hid a failing mutation on 24 September and
//! 12 October 2026.

// Each test binary compiles its own copy of this module and uses a different part of it.
#![allow(dead_code)]

use sqlx::{Connection, PgPool};

/// A setup step that ran after the database answered. Its failure panics with the real error.
///
/// Every test file used to carry its own copy that printed "skipping" and returned `None`, which
/// turned a broken setup into a skip. A binary reaches this one through `#[macro_use] mod common;`.
macro_rules! step {
    ($what:expr, $result:expr) => {
        match $result {
            Ok(v) => v,
            Err(e) => panic!("{} failed: {e:?}", $what),
        }
    };
}

/// Whether a test with no database must fail rather than skip. A laptop with Docker stopped may
/// still skip; `scripts/cargo.sh test`, `test-fast` and CI attach a database and set this.
pub fn db_required() -> bool {
    std::env::var("LUMBERROOM_TEST_REQUIRE_DB").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Panics when `required`, otherwise prints the reason and returns `None` so the caller skips.
pub fn skip_or_fail<T>(required: bool, what: &str, why: impl std::fmt::Debug) -> Option<T> {
    assert!(
        !required,
        "{what} failed: {why:?}\nLUMBERROOM_TEST_REQUIRE_DB is set, so this test must not skip"
    );
    eprintln!("skipping: {what} failed: {why:?}");
    None
}

/// The admin pool `DATABASE_URL` names and the URL with its database part cut off, or `None` on a
/// machine with no database.
///
/// A refused or timed-out connection may skip. A connection the server answered with an error,
/// such as a wrong password or a missing database, panics: something is listening, so the run
/// is broken rather than absent.
pub async fn connect_admin() -> Option<(PgPool, String)> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(e) => return skip_or_fail(db_required(), "reading DATABASE_URL", e),
    };
    // The URL carries a password, so it stays out of the message.
    let Some((base, _)) = url.rsplit_once('/') else {
        panic!("DATABASE_URL has no database part");
    };
    match PgPool::connect(&url).await {
        Ok(admin) => Some((admin, base.to_string())),
        Err(e @ sqlx::Error::Database(_)) => {
            panic!("connecting to the admin database failed: {e:?}")
        }
        Err(e) => skip_or_fail(db_required(), "connecting to the admin database", e),
    }
}

/// Creates `name` unless it already exists.
///
/// `scripts/cargo.sh test-fast` runs several binaries at once, and two of them can both find the
/// database missing and both create it. The loser gets 42P04, or 23505 when the two catalog inserts
/// collide. Either code means the database now exists, which is all the caller asked for. Call this
/// before `lock_database`, which connects to the database and so needs it to exist; the migrate
/// that follows runs under that lock.
pub async fn create_database_if_missing(admin: &PgPool, name: &str) {
    let exists: Option<i32> = step!(
        format!("looking for {name}"),
        sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
            .bind(name)
            .fetch_optional(admin)
            .await
    );
    if exists.is_some() {
        return;
    }
    // DDL takes no bind parameter. Audited: every caller passes a compile-time constant.
    let created =
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(admin).await;
    match created {
        Ok(_) => {}
        Err(e) if lost_creation_race(e.as_database_error().and_then(|d| d.code()).as_deref()) => {}
        Err(e) => panic!("creating {name} failed: {e:?}"),
    }
}

/// Whether a `CREATE DATABASE` failed only because another process created the database first.
pub fn lost_creation_race(sqlstate: Option<&str>) -> bool {
    matches!(sqlstate, Some("42P04" | "23505"))
}

/// Applies the migrations to `database`, and panics with the way out when they do not apply.
///
/// The usual cause is a database another checkout migrated first: a branch with a newer
/// migration, or a fork whose own migrations sqlx cannot find here. sqlx calls that
/// `VersionMissing`, or `VersionMismatch` when a shared version's file changed.
pub async fn migrate(pool: &PgPool, database: &str) {
    if let Err(e) = lumberroom_server::adapters::postgres::migrate(pool).await {
        panic!(
            "migrating {database} failed: {e:?}\n\
             If that names VersionMissing or VersionMismatch, {database} holds migrations from \
             another checkout. It is a throwaway test database, so drop it and the next run \
             recreates it:\n  \
             docker exec lumberroom-testdb-1 psql -U lumberroom -d postgres \
             -c \"DROP DATABASE {database} WITH (FORCE)\""
        );
    }
}

/// Any constant, as long as every binary uses this one. Derived from nothing: it just has to be a
/// number no other advisory lock in this database picks.
const SUITE_LOCK: i64 = 0x5574_7254_6573_74;

/// Held for the length of one test. Dropping it closes the connection, which releases the lock.
pub struct DbGuard {
    conn: Option<sqlx::PgConnection>,
}

impl Drop for DbGuard {
    fn drop(&mut self) {
        // The connection closes when it drops and Postgres releases every advisory lock the session
        // held. Explicit unlock would need an await, and Drop cannot have one.
        drop(self.conn.take());
    }
}

/// Blocks until every other test binary has finished with the database.
///
/// A dedicated connection rather than one from the pool: a pooled connection goes back to be reused
/// while still holding the lock, and the next test to borrow it would inherit a lock it never took
/// and never releases.
pub async fn lock_database(url: &str) -> DbGuard {
    let mut conn = step!("connecting for the suite lock", sqlx::PgConnection::connect(url).await);
    step!(
        "taking the suite lock",
        sqlx::query("SELECT pg_advisory_lock($1)").bind(SUITE_LOCK).execute(&mut conn).await
    );
    DbGuard { conn: Some(conn) }
}

/// The embedder set a test Ctx uses: the engine registry resolved for `embedder`'s id with the
/// config's overrides and old single variables, so a test that sets `conflict_threshold` still
/// moves the floor. It mirrors the boot path in `src/main.rs` without its refusals, because a test
/// that wants a refusal asserts on the registry directly.
pub fn test_embedders(
    embedder: std::sync::Arc<dyn lumberroom_server::ports::Embedder>,
    cfg: &lumberroom_server::config::Config,
) -> std::sync::Arc<lumberroom_server::services::embedders::EmbedderSet> {
    use lumberroom_server::domain::similarity::Registry;
    let t =
        Registry::engine().resolve(&embedder.id(), &cfg.embed.thresholds, &cfg.legacy_thresholds());
    std::sync::Arc::new(lumberroom_server::services::embedders::EmbedderSet::single(
        std::sync::Arc::clone(&embedder),
        std::collections::HashMap::from([(embedder.id(), std::sync::Arc::new(t))]),
    ))
}
