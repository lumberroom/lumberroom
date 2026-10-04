//! The wake channel for the conflict sweeper.
//!
//! The sweeper is the only caller of `memory_conflict_record`; this module only tells it when to
//! look. A writer's transaction issues `NOTIFY memory_conflict, '<tenant>'` and the callback here
//! hands the tenant to the sweeper's `Wakes`. A notification is a hint, never the work: Postgres
//! drops them while no connection listens, and the sweeper's interval pass covers every gap.
//!
//! The listener owns a dedicated connection. `PgListener` takes one from the pool and holds it for
//! the life of the task, so size the pool with that connection in mind.

use sqlx::postgres::PgListener;
use sqlx::PgPool;

use crate::domain::errors::{DomainError, Result};

pub const CONFLICT_WAKE_CHANNEL: &str = "memory_conflict";

/// Start listening and return the task that drives the callback.
///
/// `connect_with` and `listen` run before the task spawns, so a bad pool or a refused LISTEN fails
/// the caller at boot instead of leaving a task that never hears anything.
///
/// A `recv` error logs one warning and the loop goes on. `PgListener` reconnects and re-issues
/// LISTEN by itself on the next `recv`, and a notification sent while it was down is lost. That
/// loss is acceptable for the reason in the module comment.
pub async fn listen<F>(pool: &PgPool, on_wake: F) -> Result<tokio::task::JoinHandle<()>>
where
    F: Fn(&str) + Send + Sync + 'static,
{
    let mut listener = PgListener::connect_with(pool).await.map_err(|e| {
        DomainError::internal(format!("conflict wake listener connect failed: {e}"))
    })?;
    listener
        .listen(CONFLICT_WAKE_CHANNEL)
        .await
        .map_err(|e| DomainError::internal(format!("conflict wake LISTEN failed: {e}")))?;

    Ok(tokio::spawn(async move {
        loop {
            match listener.recv().await {
                Ok(note) => on_wake(note.payload()),
                Err(e) => {
                    tracing::warn!(error = %e, "conflict wake listener lost its connection, retrying");
                    // A refused reconnect returns at once; without a pause a down database spins
                    // this loop and floods the log.
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // Needs the compose database. A NOTIFY writes no table, so the shared database is safe here.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL"]
    async fn notify_reaches_the_callback_with_its_payload() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let pool = PgPool::connect(&url).await.unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let handle =
            listen(&pool, move |t| sink.lock().unwrap().push(t.to_string())).await.expect("listen");

        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(CONFLICT_WAKE_CHANNEL)
            .bind("t1-wake-test")
            .execute(&pool)
            .await
            .unwrap();

        let mut got = false;
        for _ in 0..50 {
            if seen.lock().unwrap().iter().any(|t| t == "t1-wake-test") {
                got = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        handle.abort();
        assert!(got, "callback never saw the notification: {:?}", seen.lock().unwrap());
    }
}
