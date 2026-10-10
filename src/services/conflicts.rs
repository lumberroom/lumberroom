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

use async_trait::async_trait;

use super::embedders::EmbedderSet;
use crate::domain::errors::Result;
use crate::domain::similarity::CONFLICT;
use crate::ports::memory::ConflictSweep;
use crate::ports::MemoryRepository;

/// The one repository call a sweep makes. A seam so a test can record the floor each pass scans
/// at; `MemoryRepository` has forty-odd methods and no fake.
#[async_trait]
pub trait ConflictScan: Send + Sync {
    async fn scan(&self, tenant: &str, floor: f64, limit: i64) -> Result<ConflictSweep>;
}

#[async_trait]
impl<T: MemoryRepository + ?Sized> ConflictScan for T {
    async fn scan(&self, tenant: &str, floor: f64, limit: i64) -> Result<ConflictSweep> {
        self.sweep_conflicts(tenant, floor, limit).await
    }
}

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
pub async fn sweep<R: ConflictScan + ?Sized>(
    repo: &R,
    tenant: &str,
    floor: f64,
    budget: Duration,
) -> Result<SweepReport> {
    let start = Instant::now();
    let mut report = SweepReport::default();
    loop {
        let batch = repo.scan(tenant, floor, SWEEP_BATCH).await?;
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

/// One pass: the unit's `conflict` value, read now, then a sweep at that floor. A unit whose model
/// has no resolved thresholds fails the pass before any scan runs.
async fn pass<R: ConflictScan + ?Sized>(
    repo: &R,
    tenant: &str,
    embedders: &EmbedderSet,
    budget: Duration,
) -> Result<SweepReport> {
    let floor = embedders.for_unit(tenant)?.thresholds.get(CONFLICT);
    sweep(repo, tenant, floor, budget).await
}

/// Sweep on every tick and on every wake that names `tenant`. Never returns.
///
/// The floor is read from the unit's thresholds at the top of each pass. A model switch that lands
/// between two passes changes the floor of the next one with no restart.
pub async fn run_loop<R: ConflictScan + ?Sized + 'static>(
    repo: Arc<R>,
    tenant: String,
    embedders: Arc<EmbedderSet>,
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
        match pass(repo.as_ref(), &tenant, &embedders, budget).await {
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

    use crate::domain::similarity::{Resolved, SimilarityThresholds, Source};
    use crate::ports::Embedder;

    struct FakeEmbedder(&'static str);

    #[async_trait]
    impl Embedder for FakeEmbedder {
        fn id(&self) -> String {
            self.0.to_string()
        }
        fn dim(&self) -> usize {
            4
        }
        async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.0; 4]).collect())
        }
        async fn embed_query(&self, _: &str) -> Result<Vec<f32>> {
            Ok(vec![0.0; 4])
        }
    }

    /// An embedder set whose only model resolved `conflict` to `floor`, or resolved nothing.
    fn set(id: &'static str, resolved: Option<f64>) -> Arc<EmbedderSet> {
        let mut thresholds = std::collections::HashMap::new();
        if let Some(value) = resolved {
            thresholds.insert(
                id.to_string(),
                Arc::new(SimilarityThresholds {
                    model: id.to_string(),
                    family: None,
                    values: [(CONFLICT.to_string(), Resolved { value, source: Source::Override })]
                        .into(),
                }),
            );
        }
        Arc::new(EmbedderSet::single(Arc::new(FakeEmbedder(id)), thresholds))
    }

    /// Records the floor of every scan. Reports one scanned row and none pending, so a sweep is one
    /// call.
    #[derive(Default)]
    struct Recorder {
        floors: Mutex<Vec<f64>>,
    }

    #[async_trait]
    impl ConflictScan for Recorder {
        async fn scan(&self, _: &str, floor: f64, _: i64) -> Result<ConflictSweep> {
            self.floors.lock().unwrap().push(floor);
            Ok(ConflictSweep { scanned: 1, pending: 0 })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_conflict_floor_is_read_each_pass() {
        let repo = Arc::new(Recorder::default());
        let wakes = Arc::new(Wakes::default());
        let looped = tokio::spawn(run_loop(
            Arc::clone(&repo),
            "t1".to_string(),
            set("embeddinggemma-2", Some(0.91)),
            Duration::from_secs(1),
            Duration::from_secs(60),
            Arc::clone(&wakes),
        ));
        // Three passes: two timer ticks and one wake. Paused time advances only while every task
        // waits, so each sleep below lets exactly the pending pass finish.
        tokio::time::sleep(Duration::from_secs(61)).await;
        wakes.wake("t1");
        tokio::time::sleep(Duration::from_secs(1)).await;
        tokio::time::sleep(Duration::from_secs(60)).await;
        looped.abort();
        let floors = repo.floors.lock().unwrap().clone();
        assert_eq!(floors, vec![0.91, 0.91, 0.91], "one scan per pass, at the unit's own value");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pass_with_no_resolved_thresholds_scans_nothing_and_the_loop_survives() {
        // The lookup sits inside the pass. Read once before the loop it would have failed boot or
        // been cached; read per pass it fails the pass, logs, and tries again at the next tick.
        let repo = Arc::new(Recorder::default());
        let wakes = Arc::new(Wakes::default());
        let looped = tokio::spawn(run_loop(
            Arc::clone(&repo),
            "t1".to_string(),
            set("embeddinggemma-2", None),
            Duration::from_secs(1),
            Duration::from_secs(60),
            wakes,
        ));
        tokio::time::sleep(Duration::from_secs(121)).await;
        assert!(!looped.is_finished(), "a failed pass must not end the loop");
        looped.abort();
        assert!(repo.floors.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sweep_takes_the_floor_it_is_given() {
        let repo = Recorder::default();
        sweep(&repo, "t1", 0.42, Duration::from_secs(1)).await.unwrap();
        assert_eq!(repo.floors.lock().unwrap().as_slice(), &[0.42]);
    }
}
