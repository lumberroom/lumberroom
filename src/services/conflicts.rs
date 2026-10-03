//! The sweeper that records conflict pairs.
//!
//! A pair is recorded after the write that created it has committed, never inside that write. A scan
//! inside the inserting transaction, or inside a trigger on `memory`, cannot see the concurrent
//! writer's uncommitted row, so two simultaneous writes each miss the other and the pair is lost for
//! good. The sweeper runs in its own statements, one autocommit per row, so every scan sees every
//! committed row. See `docs/specs/write-time-conflicts.md` section 6.
//!
//! Only this module reaches `memory_conflict_record`, through `MemoryRepository::sweep_conflicts`.
//!
//! The wake is a hint and the interval is the guarantee. A `NOTIFY` lost to a dropped connection, or
//! sent through a pooler in transaction mode, delays a pair by one interval and no longer.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::domain::errors::Result;
use crate::ports::MemoryRepository;

/// Rows scanned per `sweep_conflicts` call. One call is one short pass; the budget bounds how many
/// passes a sweep makes.
pub const SWEEP_BATCH: i64 = 50;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub scanned: i64,
    pub pending: i64,
}

/// Scan batches until nothing is pending, a batch commits no scan, or `budget` has passed.
///
/// `scanned` counts committed scans only. A batch whose rows all fail returns `scanned == 0`, and
/// that ends the sweep. Without the check the loop would re-run the same failing scans until the
/// budget ran out, every interval.
pub async fn sweep(
    repo: &dyn MemoryRepository,
    tenant: &str,
    floor: f64,
    budget: Duration,
) -> Result<SweepReport> {
    let start = Instant::now();
    let mut report = SweepReport::default();
    loop {
        let batch = repo.sweep_conflicts(tenant, floor, SWEEP_BATCH).await?;
        report.scanned += batch.scanned;
        report.pending = batch.pending;
        if batch.pending == 0 || batch.scanned == 0 || start.elapsed() >= budget {
            return Ok(report);
        }
    }
}

/// Tenants that have been woken since the last `wait`.
///
/// A set plus a `Notify` permit: any number of wakes before a `wait` collapse into one return of the
/// distinct tenants, and a wake with no waiter leaves a permit, so the next `wait` returns at once
/// instead of sleeping through a write that already happened.
#[derive(Default)]
pub struct Wakes {
    tenants: Mutex<BTreeSet<String>>,
    notify: Notify,
}

impl Wakes {
    pub fn wake(&self, tenant: &str) {
        // The guard drops before notify_one so a waiter that wakes at once never blocks on it.
        {
            let mut set = self.tenants.lock().unwrap_or_else(|p| p.into_inner());
            set.insert(tenant.to_string());
        }
        self.notify.notify_one();
    }

    /// Never holds the mutex across an await: the set is taken after the permit arrives.
    pub async fn wait(&self) -> Vec<String> {
        self.notify.notified().await;
        let taken = {
            let mut set = self.tenants.lock().unwrap_or_else(|p| p.into_inner());
            std::mem::take(&mut *set)
        };
        taken.into_iter().collect()
    }
}

/// Sweep on every tick and on every wake that names `tenant`. Never returns.
pub async fn run_loop(
    repo: Arc<dyn MemoryRepository>,
    tenant: String,
    floor: f64,
    budget: Duration,
    interval: Duration,
    wakes: Arc<Wakes>,
) {
    let mut tick = tokio::time::interval(interval);
    // A sweep that overruns the interval must not queue a burst of catch-up ticks behind it.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick of an interval completes at once. Boot does not need a sweep before the
    // first write or the first interval.
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            woken = wakes.wait() => {
                if !woken.iter().any(|t| t == &tenant) {
                    continue;
                }
            }
        }
        match sweep(repo.as_ref(), &tenant, floor, budget).await {
            Ok(r) if r.scanned > 0 => {
                tracing::info!(scanned = r.scanned, pending = r.pending, "conflict sweep");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e.log_message(), "conflict sweep failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn two_wakes_for_one_tenant_return_it_once() {
        let w = Wakes::default();
        w.wake("t1");
        w.wake("t1");
        assert_eq!(w.wait().await, vec!["t1".to_string()]);
        // The second permit does not exist: notify_one stores at most one.
        let again = tokio::time::timeout(Duration::from_millis(50), w.wait()).await;
        assert!(again.is_err(), "a second wait must block");
    }

    #[tokio::test]
    async fn a_wake_with_no_waiter_leaves_a_permit() {
        let w = Wakes::default();
        w.wake("t1");
        let got = tokio::time::timeout(Duration::from_millis(50), w.wait())
            .await
            .expect("wait returns at once");
        assert_eq!(got, vec!["t1".to_string()]);
    }

    #[tokio::test]
    async fn distinct_tenants_return_together() {
        let w = Wakes::default();
        w.wake("b");
        w.wake("a");
        assert_eq!(w.wait().await, vec!["a".to_string(), "b".to_string()]);
    }
}
