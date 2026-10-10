//! One lock the whole test suite shares.
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

use sqlx::Connection;

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
pub async fn lock_database(url: &str) -> Option<DbGuard> {
    let mut conn = sqlx::PgConnection::connect(url).await.ok()?;
    sqlx::query("SELECT pg_advisory_lock($1)").bind(SUITE_LOCK).execute(&mut conn).await.ok()?;
    Some(DbGuard { conn: Some(conn) })
}

/// The embedder set a test Ctx uses: the engine registry resolved for `embedder`'s id with the
/// config's overrides and old single variables, so a test that sets `conflict_threshold` still
/// moves the floor. It mirrors the boot path in `src/main.rs` without its refusals, because a test
/// that wants a refusal asserts on the registry directly.
#[allow(dead_code)]
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
