//! The sweep that moves each unit of the store from one embedding model to another (decision 0027).
//!
//! Each pass derives the configured view, walks every unit, fills the slot the phase rules name,
//! flips a unit that is ready, and retires a slot whose rollback window has passed. Live writes
//! never wait on any of it: the fill keeps one request in flight, sleeps to hold its duty cycle,
//! and stops under the disk floor. Only `ports` reach the store, so the sweep runs unchanged on any
//! repository that implements `EmbeddingMigrationRepository`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::embedders::EmbedderSet;
use super::row_opener::RowOpener;
use crate::config::Config;
use crate::domain::embedding_command::configured_from_intent;
use crate::domain::embedding_migration::{
    Blocked, Configured, Counts, DiskStatus, FlipOutcome, FlipRequest, PendingRow, Phase,
    Published, Rollback, UnitState, UnitStatus,
};
use crate::domain::embedding_phase::{fill_sleep, pack, phase, retire_after, rollback, target};
use crate::domain::embedding_slot::VectorSlot;
use crate::domain::errors::DomainError;
use crate::ports::{Embedder, EmbeddingMigrationRepository, FreeSpace};

/// Rows `next_batch` reads per page.
pub const PAGE: i64 = 64;
/// Rows one retire statement clears.
pub const RETIRE_BATCH: i64 = 500;
/// Longest a pass fills one unit before it moves on. A design target in the shape of the conflict
/// sweeper's budget (`services/conflicts.rs`): without it one pass held a 1.4 to 2.1 hour fill and
/// published nothing until the end (review M8).
pub const FILL_BUDGET: Duration = Duration::from_secs(60);
/// Refusals before a row counts as failed and blocks the flip.
const FAILURES_TO_FAIL: u32 = 3;
/// The text a probe embeds. Short, so it costs one small request.
const PROBE_TEXT: &str = "lumberroom embedding probe";
/// Failed ids the status names, so the console can list them.
const FAILED_IDS_SHOWN: usize = 20;

/// Where a pass gets its configured view.
pub enum Steer {
    /// Env mode: fixed at boot from `.env`.
    Env(Configured),
    /// Command mode: derived on every pass from `embedding_control` through
    /// `domain::embedding_command::configured_from_intent`.
    Command {
        /// `EMBED_*`'s id, then `EMBED_PREVIOUS_*`'s when set.
        blocks: Vec<String>,
        rollback_days: i64,
        guessed_acting: BTreeMap<String, Vec<String>>,
    },
}

/// The settings a pass reads, copied out of `Config` once so a test builds a sweep without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Knobs {
    pub fill_chars: usize,
    pub fill_duty: u8,
    /// 0 when no floor is set; the sweep then holds no `FreeSpace` either.
    pub floor_bytes: u64,
    pub fill_budget: Duration,
}

impl Knobs {
    pub fn from_config(cfg: &Config) -> Self {
        Knobs {
            fill_chars: cfg.embed.migrate.fill_chars,
            fill_duty: cfg.embed.migrate.fill_duty,
            floor_bytes: cfg.embed.disk.floor_mb.saturating_mul(1_048_576),
            fill_budget: FILL_BUDGET,
        }
    }
}

pub struct Sweep {
    pub repo: Arc<dyn EmbeddingMigrationRepository>,
    pub embedders: Arc<EmbedderSet>,
    pub opener: Arc<dyn RowOpener>,
    pub knobs: Knobs,
    pub steer: Steer,
    pub kek_verified: bool,
    /// None when EMBED_DISK_FLOOR_MB is 0.
    pub disk: Option<Arc<dyn FreeSpace>>,
    pub status: Arc<RwLock<SweepStatus>>,
    book: Mutex<Book>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SweepStatus {
    pub units: Vec<UnitStatus>,
    pub disk: Option<DiskStatus>,
    pub rollback: Option<Rollback>,
    pub rate_rows_per_min: f64,
    pub last_pass_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

/// Units walked this pass that are not blocked, with their active model. The fork refills its
/// system map for these.
pub struct PassReport {
    pub ready: Vec<(String, String)>,
}

/// The units this pass walks before the ones that hold a state row. The engine passes
/// `vec![cfg.tenant_id]`; the fork its active tenants.
#[async_trait]
pub trait Units: Send + Sync {
    async fn list(&self) -> Vec<String>;
}

pub struct SingleUnit(pub String);

#[async_trait]
impl Units for SingleUnit {
    async fn list(&self) -> Vec<String> {
        vec![self.0.clone()]
    }
}

/// What the sweep remembers between passes. In memory only: a restart forgets every refusal, and a
/// row that still fails counts back up to three.
#[derive(Default)]
struct Book {
    /// Refusals per row, keyed by unit, slot and id.
    failures: HashMap<(String, &'static str, Uuid), u32>,
    /// Units whose last fill skipped a private row for want of a verified KEK.
    kek_skipped: HashSet<String>,
    disk_paused: bool,
    /// The blocked reason last logged per unit, so each change logs once.
    blocked: HashMap<String, Option<Blocked>>,
}

/// Why a pass stopped before its last unit.
enum Stop {
    Disk,
    Generation,
    Store(String),
}

impl From<DomainError> for Stop {
    fn from(e: DomainError) -> Self {
        Stop::Store(e.log_message())
    }
}

/// How one fill ended. Every end but `Done` leaves rows for the next pass.
#[derive(Debug, PartialEq, Eq)]
enum FillEnd {
    Done,
    Budget,
    Dropped,
    Outage,
    NoEmbedder,
}

/// One pass's record of embedding calls, kept per model id: a refusal counts against a row only
/// when the same model answered some other request in the pass.
#[derive(Default)]
struct Tally {
    sent: HashMap<String, u32>,
    answered: HashMap<String, u32>,
    last_error: HashMap<String, String>,
    /// Rows a single-row request refused: (unit, slot, id, model).
    refused_alone: Vec<(String, &'static str, Uuid, String)>,
    /// Models a probe found down this pass. Every later fill on them ends at once.
    down: HashSet<String>,
    stored: u64,
    errors: Vec<String>,
}

impl Tally {
    fn answered(&self, model: &str) -> bool {
        self.answered.get(model).copied().unwrap_or(0) > 0
    }
}

/// Rows one request carried, and how many of them landed.
struct Sent {
    attempted: usize,
    landed: usize,
}

impl Sweep {
    pub fn new(
        repo: Arc<dyn EmbeddingMigrationRepository>,
        embedders: Arc<EmbedderSet>,
        opener: Arc<dyn RowOpener>,
        knobs: Knobs,
        steer: Steer,
        kek_verified: bool,
        disk: Option<Arc<dyn FreeSpace>>,
    ) -> Self {
        Sweep {
            repo,
            embedders,
            opener,
            knobs,
            steer,
            kek_verified,
            disk,
            status: Arc::new(RwLock::new(SweepStatus::default())),
            book: Mutex::new(Book::default()),
        }
    }

    /// Pass after pass, `interval` apart. Returns at once when `interval` is zero, which is
    /// `EMBED_MIGRATE_SECS=0`: the sweep is off.
    pub async fn run_loop(self: Arc<Self>, units: Arc<dyn Units>, interval: Duration) {
        if interval.is_zero() {
            return;
        }
        loop {
            self.pass(units.as_ref()).await;
            tokio::time::sleep(interval).await;
        }
    }

    pub async fn pass(&self, units: &dyn Units) -> PassReport {
        let started = Instant::now();
        let mut tally = Tally::default();
        let mut report = PassReport { ready: Vec::new() };
        let listed = units.list().await;

        let view = match self.view(&listed).await {
            Ok(view) => view,
            Err((text, generation)) => {
                tracing::error!(error = %text, "embedding pass has no configured view");
                tally.errors.push(text);
                self.finish(None, generation, &[], false, started, &mut tally).await;
                return report;
            }
        };
        let generation = view.generation;
        self.embedders.set_configured(Arc::new(view.clone()));

        if self.disk_paused() {
            self.finish(Some(&view), generation, &[], false, started, &mut tally).await;
            return report;
        }

        let walk = match self.units_to_walk(&listed).await {
            Ok(walk) => walk,
            Err(e) => {
                tally.errors.push(format!("embedding states: {}", e.log_message()));
                self.finish(Some(&view), generation, &[], false, started, &mut tally).await;
                return report;
            }
        };

        for unit in &walk {
            match self.walk_unit(unit, &view, &mut tally, &mut report).await {
                Ok(()) => {}
                Err(stop) => {
                    match stop {
                        Stop::Disk => {}
                        Stop::Generation => tracing::info!(
                            unit = %unit,
                            "embedding pass ended: the control row moved since the pass read it"
                        ),
                        Stop::Store(text) => tally.errors.push(text),
                    }
                    self.finish(Some(&view), generation, &walk, false, started, &mut tally).await;
                    return report;
                }
            }
        }
        self.finish(Some(&view), generation, &walk, true, started, &mut tally).await;
        report
    }

    /// Step 0: the view this pass runs on, and in command mode the generation it came from. Err
    /// carries the text for the status and the generation to publish it under, when one was read.
    async fn view(&self, listed: &[String]) -> Result<Configured, (String, Option<i64>)> {
        match &self.steer {
            Steer::Env(view) => Ok(view.clone()),
            Steer::Command { blocks, rollback_days, guessed_acting } => {
                let (intent, _) =
                    self.repo.control().await.map_err(|e| {
                        (format!("embedding control row: {}", e.log_message()), None)
                    })?;
                let default_active = match listed.first() {
                    Some(unit) => self
                        .repo
                        .state(unit)
                        .await
                        .map_err(|e| (e.log_message(), Some(intent.generation)))?
                        .map(|s| s.active_model().to_string()),
                    None => None,
                };
                configured_from_intent(
                    &intent,
                    blocks,
                    default_active.as_deref(),
                    *rollback_days,
                    guessed_acting,
                )
                .map_err(|text| (text, Some(intent.generation)))
            }
        }
    }

    /// Step 2: the listed units in their order, then every other unit with a state row, sorted.
    /// Seeds once when a walked unit has no state row, and reloads the request path's state map.
    async fn units_to_walk(&self, listed: &[String]) -> crate::domain::errors::Result<Vec<String>> {
        let mut states = self.repo.states().await?;
        let mut walk: Vec<String> = Vec::new();
        for unit in listed {
            if !walk.contains(unit) {
                walk.push(unit.clone());
            }
        }
        let mut rest: Vec<String> =
            states.iter().map(|s| s.unit.clone()).filter(|u| !walk.contains(u)).collect();
        rest.sort();
        walk.extend(rest);
        if walk.iter().any(|u| !states.iter().any(|s| &s.unit == u)) {
            self.repo.seed().await?;
            states = self.repo.states().await?;
        }
        self.embedders.set_states(states);
        Ok(walk)
    }

    async fn walk_unit(
        &self,
        unit: &str,
        view: &Configured,
        tally: &mut Tally,
        report: &mut PassReport,
    ) -> Result<(), Stop> {
        let Some(mut state) = self.repo.state(unit).await? else {
            // No vector yet, so nothing to move. Writes land on the model `for_unit` names, and
            // the pass after the first one seeds the unit.
            if let Ok(u) = self.embedders.for_unit(unit) {
                report.ready.push((unit.to_string(), u.embedder.id()));
            }
            return Ok(());
        };
        let unit_started = Instant::now();
        let deadline = unit_started + self.knobs.fill_budget;

        if let Some(t) = target(&state, view) {
            if state.other_model().is_none()
                && self.repo.name_slot(unit, state.active_slot.other(), &t).await?
            {
                state = self.reload(unit, state).await?;
            }
        }

        let goal = target(&state, view);
        let mut counts = self.counts(unit, &state, view, goal.as_deref()).await?;
        // A slot with nothing pending holds no failed row: the row was filled, forgotten or
        // deleted, and no fill will run to notice.
        if counts.other_pending == 0 {
            self.forget_unseen(unit, state.active_slot.other(), &HashSet::new());
        }
        if counts.active_holes + counts.foreign_model == 0 {
            self.forget_unseen(unit, state.active_slot, &HashSet::new());
        }
        let (beneath, beneath_reason) = phase(&state, view, &counts, None);
        let blocked_by_service = self.service_reason(unit);
        let active = state.active_model().to_string();
        let (active_slot, inactive) = (state.active_slot, state.active_slot.other());
        let stored_before = tally.stored;
        let mut ends: Vec<FillEnd> = Vec::new();
        let mut skipped_private = false;

        let fill_other = matches!(beneath, Phase::Filling | Phase::Held | Phase::Flipped)
            || (beneath == Phase::Ready && blocked_by_service.is_some())
            || matches!(beneath_reason, Some(Blocked::Guessed(_)));
        let fill_active = matches!(beneath, Phase::Steady | Phase::Flipped | Phase::Retiring);

        // Active holes first: in `flipped` a row without its active vector is invisible to search.
        if fill_active && counts.active_holes + counts.foreign_model > 0 {
            let end = self
                .fill(
                    unit,
                    active_slot,
                    &active,
                    view.generation,
                    deadline,
                    tally,
                    &mut skipped_private,
                )
                .await?;
            ends.push(end);
        }
        // Only the budget stops the second fill: one model's outage says nothing about the other.
        if fill_other && counts.other_pending > 0 && !ends.contains(&FillEnd::Budget) {
            if let Some(goal) = goal.as_deref() {
                let end = self
                    .fill(
                        unit,
                        inactive,
                        goal,
                        view.generation,
                        deadline,
                        tally,
                        &mut skipped_private,
                    )
                    .await?;
                ends.push(end);
            }
        }
        self.note_kek(unit, skipped_private, &ends);

        let mut retire_wait = None;
        if beneath == Phase::Ready && blocked_by_service.is_none() {
            if let Some(goal) = goal.clone() {
                state = self.flip(unit, state, inactive, goal, view.generation).await?;
            }
        } else if beneath == Phase::Retiring && ends.iter().all(|e| *e == FillEnd::Done) {
            counts = self.counts(unit, &state, view, goal.as_deref()).await?;
            retire_wait = retire_after(&state, view).filter(|at| *at > Utc::now());
            if counts.active_holes == 0 && retire_wait.is_none() {
                state = self.retire(unit, state, view).await?;
            }
        }

        // Refusals count before the status is built, so a third one blocks the unit this pass.
        self.settle_refusals(tally, false);
        let goal = target(&state, view);
        let worked =
            tally.stored > stored_before || !ends.is_empty() || state.active_slot != active_slot;
        if worked {
            counts = self.counts(unit, &state, view, goal.as_deref()).await?;
        }
        counts.failed = self.failed_count(unit);
        let (shown, reason) = phase(&state, view, &counts, self.service_reason(unit));
        if shown == Phase::Retiring && retire_wait.is_none() {
            retire_wait = retire_after(&state, view).filter(|at| *at > Utc::now());
        }
        self.log_blocked(unit, &reason);
        if worked {
            tracing::info!(
                unit,
                phase = ?shown,
                filled = tally.stored - stored_before,
                pending = counts.other_pending + counts.active_holes,
                ms = unit_started.elapsed().as_millis() as u64,
                "embedding pass"
            );
        }
        if shown != Phase::Blocked {
            report.ready.push((unit.to_string(), state.active_model().to_string()));
        }
        self.put_unit_status(UnitStatus {
            unit: unit.to_string(),
            phase: shown,
            active_slot: state.active_slot,
            active: state.active_model().to_string(),
            other: state.other_model().map(str::to_string),
            counts,
            failed_ids: self.failed_ids(unit),
            blocked: reason,
            flipped_at: state.flipped_at,
            retire_after: if shown == Phase::Retiring { retire_wait } else { None },
        });
        Ok(())
    }

    async fn reload(&self, unit: &str, fallback: UnitState) -> Result<UnitState, Stop> {
        let state = self.repo.state(unit).await?.unwrap_or(fallback);
        self.embedders.put_state(state.clone());
        Ok(state)
    }

    async fn counts(
        &self,
        unit: &str,
        state: &UnitState,
        view: &Configured,
        goal: Option<&str>,
    ) -> Result<Counts, Stop> {
        let mut counts = self.repo.counts(unit, state, goal, view.retire.as_deref()).await?;
        counts.failed = self.failed_count(unit);
        Ok(counts)
    }

    /// Fills `slot` with `model` for every row pending there, a page at a time from the lowest id.
    /// The cursor lives for this call only: a row written mid-pass below it waits for the next
    /// pass.
    #[allow(clippy::too_many_arguments)]
    async fn fill(
        &self,
        unit: &str,
        slot: VectorSlot,
        model: &str,
        generation: Option<i64>,
        deadline: Instant,
        tally: &mut Tally,
        skipped_private: &mut bool,
    ) -> Result<FillEnd, Stop> {
        let Some(embedder) = self.embedders.by_id(model) else {
            tally.errors.push(format!("no embedder is built for {model}, which unit {unit} needs"));
            return Ok(FillEnd::NoEmbedder);
        };
        let mut after: Option<Uuid> = None;
        let mut seen: HashSet<Uuid> = HashSet::new();
        loop {
            if tally.down.contains(model) {
                return Ok(FillEnd::Outage);
            }
            if Instant::now() >= deadline {
                return Ok(FillEnd::Budget);
            }
            self.gate(generation).await?;
            let rows = self.repo.next_batch(unit, slot, model, after, PAGE).await?;
            let Some(last) = rows.last() else {
                self.forget_unseen(unit, slot, &seen);
                return Ok(FillEnd::Done);
            };
            after = Some(last.id);
            seen.extend(rows.iter().map(|r| r.id));

            let rows = self.open_private(unit, slot, rows, skipped_private).await;
            let (mut attempted, mut landed) = (0usize, 0usize);
            for request in pack(rows, self.knobs.fill_chars) {
                if Instant::now() >= deadline {
                    return Ok(FillEnd::Budget);
                }
                match self.send(unit, slot, model, embedder.as_ref(), request, tally).await? {
                    Some(sent) => {
                        attempted += sent.attempted;
                        landed += sent.landed;
                    }
                    None => return Ok(FillEnd::Outage),
                }
            }
            if attempted > 0 && landed == 0 {
                tracing::warn!(
                    unit,
                    slot = slot.as_str(),
                    model,
                    rows = attempted,
                    "embedding store dropped"
                );
                return Ok(FillEnd::Dropped);
            }
        }
    }

    /// Plaintext for the private rows of a page. Without a verified KEK they stay out of the fill
    /// and the unit reports `blocked: kek`; a row the opener leaves out counts one failure.
    async fn open_private(
        &self,
        unit: &str,
        slot: VectorSlot,
        rows: Vec<PendingRow>,
        skipped_private: &mut bool,
    ) -> Vec<PendingRow> {
        let (mut ready, private): (Vec<PendingRow>, Vec<PendingRow>) =
            rows.into_iter().partition(|r| r.content.is_some());
        if private.is_empty() {
            return ready;
        }
        if !self.kek_verified {
            *skipped_private = true;
            return ready;
        }
        let ids: Vec<Uuid> = private.iter().map(|r| r.id).collect();
        let mut opened = match self.opener.open(unit, &ids).await {
            Ok(map) => map,
            Err(e) => {
                // No row is to blame for a key that will not load; the next pass tries again.
                tracing::error!(
                    unit,
                    error = %e.log_message(),
                    "embedding sweep could not open private rows"
                );
                return ready;
            }
        };
        for mut row in private {
            match opened.remove(&row.id) {
                Some(text) => {
                    row.chars = text.chars().count() as i64;
                    row.content = Some(text);
                    ready.push(row);
                }
                None => self.count_failure(unit, slot, row.id),
            }
        }
        ready.sort_by_key(|r| r.id);
        ready
    }

    /// One request, then one `store` per vector, then the duty-cycle sleep. A refused request of
    /// several rows goes again one row at a time to find the row at fault. None means the model is
    /// down: the fill stops rather than send every row alone into an outage.
    async fn send(
        &self,
        unit: &str,
        slot: VectorSlot,
        model: &str,
        embedder: &dyn Embedder,
        rows: Vec<PendingRow>,
        tally: &mut Tally,
    ) -> Result<Option<Sent>, Stop> {
        match self.request(unit, slot, model, embedder, &rows, tally).await? {
            Ok(sent) => return Ok(Some(sent)),
            Err(()) if rows.len() == 1 => {
                tally.refused_alone.push((
                    unit.to_string(),
                    slot.as_str(),
                    rows[0].id,
                    model.to_string(),
                ));
                return Ok(if self.alive(model, embedder, tally).await {
                    Some(Sent { attempted: 1, landed: 1 })
                } else {
                    None
                });
            }
            Err(()) => {}
        }
        let mut total = Sent { attempted: 0, landed: 0 };
        for row in rows {
            let id = row.id;
            match self
                .request(unit, slot, model, embedder, std::slice::from_ref(&row), tally)
                .await?
            {
                Ok(sent) => {
                    total.attempted += sent.attempted;
                    total.landed += sent.landed;
                }
                Err(()) => {
                    tally.refused_alone.push((
                        unit.to_string(),
                        slot.as_str(),
                        id,
                        model.to_string(),
                    ));
                    if !self.alive(model, embedder, tally).await {
                        return Ok(None);
                    }
                }
            }
        }
        // A refused row is not a dropped page: the page's other rows decide that.
        if total.attempted == 0 {
            total = Sent { attempted: 1, landed: 1 };
        }
        Ok(Some(total))
    }

    /// Whether `model` answers at all this pass. A lone refusal before any answer could be the row
    /// or an outage, and without telling them apart a bad row at the lowest id would stop its
    /// unit's fill on every pass. One short probe per model per pass settles it: an answer makes
    /// the refusal the row's, a refusal marks the model down for the rest of the pass.
    async fn alive(&self, model: &str, embedder: &dyn Embedder, tally: &mut Tally) -> bool {
        if tally.answered(model) {
            return true;
        }
        if tally.down.contains(model) {
            return false;
        }
        *tally.sent.entry(model.to_string()).or_default() += 1;
        let started = Instant::now();
        let answer = embedder.embed_documents(vec![PROBE_TEXT.to_string()]).await;
        tokio::time::sleep(fill_sleep(started.elapsed(), self.knobs.fill_duty)).await;
        match answer {
            Ok(_) => {
                *tally.answered.entry(model.to_string()).or_default() += 1;
                true
            }
            Err(e) => {
                tally.last_error.insert(model.to_string(), e.log_message());
                tally.down.insert(model.to_string());
                false
            }
        }
    }

    /// Err(()) when the embedder refused the request. Store errors end the pass.
    async fn request(
        &self,
        unit: &str,
        slot: VectorSlot,
        model: &str,
        embedder: &dyn Embedder,
        rows: &[PendingRow],
        tally: &mut Tally,
    ) -> Result<Result<Sent, ()>, Stop> {
        let texts: Vec<String> =
            rows.iter().map(|r| r.content.clone().unwrap_or_default()).collect();
        *tally.sent.entry(model.to_string()).or_default() += 1;
        let started = Instant::now();
        let answer = embedder.embed_documents(texts).await;
        let took = started.elapsed();
        let outcome = match answer {
            Ok(vectors) if vectors.len() == rows.len() => {
                *tally.answered.entry(model.to_string()).or_default() += 1;
                let mut landed = 0usize;
                for (row, vector) in rows.iter().zip(vectors) {
                    // True or false, the row is no longer pending, so it leaves the failed set.
                    if self.repo.store(unit, row.id, slot, model, vector).await? {
                        landed += 1;
                        tally.stored += 1;
                    }
                    self.clear_failure(unit, slot, row.id);
                }
                Ok(Sent { attempted: rows.len(), landed })
            }
            Ok(vectors) => {
                let text =
                    format!("{model} returned {} vectors for {} texts", vectors.len(), rows.len());
                tally.last_error.insert(model.to_string(), text);
                Err(())
            }
            Err(e) => {
                tally.last_error.insert(model.to_string(), e.log_message());
                Err(())
            }
        };
        tokio::time::sleep(fill_sleep(took, self.knobs.fill_duty)).await;
        Ok(outcome)
    }

    async fn flip(
        &self,
        unit: &str,
        state: UnitState,
        inactive: VectorSlot,
        goal: String,
        generation: Option<i64>,
    ) -> Result<UnitState, Stop> {
        let from = state.active_model().to_string();
        let req = FlipRequest {
            expect: state.clone(),
            target_slot: inactive,
            target_model: goal.clone(),
            expect_generation: generation,
        };
        match self.repo.flip(&req).await? {
            FlipOutcome::Flipped { flip_ms } => {
                tracing::info!(
                    unit,
                    from = %from,
                    to = %goal,
                    slot = inactive.as_str(),
                    flip_ms,
                    "embedding flip"
                );
                self.reload(unit, state).await
            }
            outcome => {
                tracing::warn!(unit, outcome = ?outcome, "embedding flip refused");
                Ok(state)
            }
        }
    }

    /// Deletes the retire model's vectors from the inactive slot in batches, then clears the
    /// slot's model. The caller has checked that no active hole remains and the window has passed.
    async fn retire(
        &self,
        unit: &str,
        state: UnitState,
        view: &Configured,
    ) -> Result<UnitState, Stop> {
        let Some(model) = view.retire.as_deref() else { return Ok(state) };
        let inactive = state.active_slot.other();
        let mut cleared = 0u64;
        loop {
            self.gate(view.generation).await?;
            let n = self.repo.retire_batch(unit, inactive, model, RETIRE_BATCH).await?;
            if n == 0 {
                break;
            }
            cleared += n;
        }
        if self.repo.clear_slot(unit, inactive, model).await? {
            tracing::info!(
                unit,
                model,
                slot = inactive.as_str(),
                rows = cleared,
                "embedding retire"
            );
            return self.reload(unit, state).await;
        }
        // A write that resolved its slots before this pass landed one more vector; the next pass
        // deletes it and clears the slot then.
        Ok(state)
    }

    /// Checked before every fill page and retire batch: a command that moved the control row ends
    /// the pass, and so does free space below the floor.
    async fn gate(&self, generation: Option<i64>) -> Result<(), Stop> {
        if let Some(g) = generation {
            if self.repo.generation().await? != g {
                return Err(Stop::Generation);
            }
        }
        if self.disk_paused() {
            return Err(Stop::Disk);
        }
        Ok(())
    }

    /// Reads free space, records it in the status, and logs each change between paused and not.
    /// A failed read pauses: nothing about an unreadable disk says there is room.
    fn disk_paused(&self) -> bool {
        let Some(space) = &self.disk else { return false };
        let floor = self.knobs.floor_bytes;
        let (free, paused) = match space.free_bytes() {
            Ok(free) => (free, free < floor),
            Err(e) => {
                tracing::error!(error = %e.log_message(), "embedding sweep cannot read free space");
                (0, true)
            }
        };
        self.status.write().expect("status lock poisoned").disk =
            Some(DiskStatus { free_bytes: free, floor_bytes: floor, paused });
        let mut book = self.book.lock().expect("book lock poisoned");
        if book.disk_paused != paused {
            book.disk_paused = paused;
            if paused {
                tracing::warn!(free, floor, "embedding paused: disk");
            } else {
                tracing::info!(free, floor, "embedding resumed: disk");
            }
        }
        paused
    }

    /// Publishes the pass: status fields, the rollback summary, and in command mode the control
    /// row. Runs on every exit, so a pass that stops early still tells the command where it is.
    async fn finish(
        &self,
        view: Option<&Configured>,
        generation: Option<i64>,
        walked: &[String],
        complete: bool,
        started: Instant,
        tally: &mut Tally,
    ) {
        self.settle_refusals(tally, true);
        let minutes = started.elapsed().as_secs_f64() / 60.0;
        let snapshot = {
            let mut status = self.status.write().expect("status lock poisoned");
            if complete {
                status.units.retain(|u| walked.contains(&u.unit));
            }
            if let Some(view) = view {
                status.rollback = Some(rollback(&status.units, view));
            }
            status.rate_rows_per_min =
                if tally.stored > 0 && minutes > 0.0 { tally.stored as f64 / minutes } else { 0.0 };
            status.last_pass_at = Some(Utc::now());
            status.error =
                if tally.errors.is_empty() { None } else { Some(tally.errors.join("; ")) };
            status.clone()
        };
        if let (Steer::Command { blocks, .. }, Some(applied)) = (&self.steer, generation) {
            let published = Published {
                applied_generation: applied,
                server_models: blocks.clone(),
                status: Some(super::embedding_status::published(
                    &snapshot,
                    self.embedders.all_thresholds(),
                )),
                seen_at: None,
            };
            if let Err(e) = self.repo.publish(&published).await {
                tracing::error!(
                    error = %e.log_message(),
                    "embedding sweep could not publish its status"
                );
            }
        }
    }

    /// Counts each lone refusal against its row once its model has answered another request this
    /// pass. At the end of the pass the rest belong to models that answered nothing: an outage,
    /// which counts against no row and names the model in the status.
    fn settle_refusals(&self, tally: &mut Tally, end_of_pass: bool) {
        let refused = std::mem::take(&mut tally.refused_alone);
        let mut outage: Vec<String> = Vec::new();
        for (unit, slot, id, model) in refused {
            if tally.answered(&model) {
                let mut book = self.book.lock().expect("book lock poisoned");
                *book.failures.entry((unit, slot, id)).or_default() += 1;
            } else if end_of_pass {
                if !outage.contains(&model) {
                    outage.push(model);
                }
            } else {
                tally.refused_alone.push((unit, slot, id, model));
            }
        }
        if end_of_pass {
            for (model, sent) in &tally.sent {
                if *sent > 0 && !tally.answered(model) && !outage.contains(model) {
                    outage.push(model.clone());
                }
            }
            outage.sort();
            for model in outage {
                let last = tally.last_error.get(&model).cloned().unwrap_or_default();
                tally.errors.push(format!("{model} failed every request this pass: {last}"));
            }
        }
    }

    fn slot_key(slot: VectorSlot) -> &'static str {
        slot.as_str()
    }

    fn count_failure(&self, unit: &str, slot: VectorSlot, id: Uuid) {
        let mut book = self.book.lock().expect("book lock poisoned");
        *book.failures.entry((unit.to_string(), Self::slot_key(slot), id)).or_default() += 1;
    }

    fn clear_failure(&self, unit: &str, slot: VectorSlot, id: Uuid) {
        let mut book = self.book.lock().expect("book lock poisoned");
        book.failures.remove(&(unit.to_string(), Self::slot_key(slot), id));
    }

    /// After a fill reached the end of its rows: a failed row that `next_batch` no longer returns
    /// is no longer pending, so it leaves the set.
    fn forget_unseen(&self, unit: &str, slot: VectorSlot, seen: &HashSet<Uuid>) {
        let key = Self::slot_key(slot);
        let mut book = self.book.lock().expect("book lock poisoned");
        book.failures.retain(|(u, s, id), _| !(u == unit && *s == key && !seen.contains(id)));
    }

    fn failed_count(&self, unit: &str) -> i64 {
        let book = self.book.lock().expect("book lock poisoned");
        book.failures.iter().filter(|((u, _, _), n)| u == unit && **n >= FAILURES_TO_FAIL).count()
            as i64
    }

    fn failed_ids(&self, unit: &str) -> Vec<Uuid> {
        let book = self.book.lock().expect("book lock poisoned");
        let mut ids: Vec<Uuid> = book
            .failures
            .iter()
            .filter(|((u, _, _), n)| u == unit && **n >= FAILURES_TO_FAIL)
            .map(|((_, _, id), _)| *id)
            .collect();
        ids.sort();
        ids.dedup();
        ids.truncate(FAILED_IDS_SHOWN);
        ids
    }

    /// The reasons only the service knows. A KEK problem comes first: it is the operator's to fix
    /// and it explains rows that would otherwise look stuck.
    fn service_reason(&self, unit: &str) -> Option<Blocked> {
        let kek = self.book.lock().expect("book lock poisoned").kek_skipped.contains(unit);
        if kek {
            return Some(Blocked::Kek);
        }
        match self.failed_count(unit) {
            0 => None,
            n => Some(Blocked::Failed(n)),
        }
    }

    /// A fill that skipped a private row marks the unit; a pass whose fills all reached the end
    /// without skipping one clears the mark. A fill cut short says nothing either way.
    fn note_kek(&self, unit: &str, skipped: bool, ends: &[FillEnd]) {
        let mut book = self.book.lock().expect("book lock poisoned");
        if skipped {
            book.kek_skipped.insert(unit.to_string());
        } else if ends.iter().all(|e| *e == FillEnd::Done) {
            book.kek_skipped.remove(unit);
        }
    }

    fn log_blocked(&self, unit: &str, reason: &Option<Blocked>) {
        let mut book = self.book.lock().expect("book lock poisoned");
        let last = book.blocked.insert(unit.to_string(), reason.clone());
        if last.as_ref() != Some(reason) {
            match reason {
                Some(reason) => tracing::warn!(unit, reason = ?reason, "embedding unit blocked"),
                None if last.flatten().is_some() => {
                    tracing::info!(unit, "embedding unit unblocked")
                }
                None => {}
            }
        }
    }

    /// Publishes one unit's status at once, so the console stays current through a long fill.
    fn put_unit_status(&self, unit: UnitStatus) {
        let mut status = self.status.write().expect("status lock poisoned");
        match status.units.iter_mut().find(|u| u.unit == unit.unit) {
            Some(slot) => *slot = unit,
            None => status.units.push(unit),
        }
    }

    #[cfg(test)]
    fn failures(&self, unit: &str, slot: VectorSlot, id: Uuid) -> u32 {
        let book = self.book.lock().expect("book lock poisoned");
        book.failures.get(&(unit.to_string(), Self::slot_key(slot), id)).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use uuid::Uuid;

    use super::*;
    use crate::domain::embedding_migration::{
        Blocked, Counts, FlipOutcome, FlipRequest, FlipScope, Intent, IntentChange, PendingRow,
        Phase, Published, UnitState,
    };
    use crate::domain::embedding_slot::VectorSlot;
    use crate::domain::errors::{DomainError, Result};
    use crate::domain::similarity::SimilarityThresholds;
    use crate::domain::types::Sensitivity;
    use crate::ports::{ChangeOutcome, Embedder};

    const P: &str = "Xenova/bge-base-en-v1.5@q8";
    const C: &str = "openai:google/embeddinggemma-2";
    const X: &str = "openai:third-model";

    // ── the store ─────────────────────────────────────────────────────────────────────────────

    #[derive(Debug, Clone)]
    struct Row {
        unit: String,
        id: Uuid,
        sensitivity: Sensitivity,
        /// None for a private row, whose plaintext only the opener holds.
        content: Option<String>,
        a: Option<String>,
        b: Option<String>,
    }

    impl Row {
        fn slot(&self, slot: VectorSlot) -> &Option<String> {
            match slot {
                VectorSlot::A => &self.a,
                VectorSlot::B => &self.b,
            }
        }
        fn slot_mut(&mut self, slot: VectorSlot) -> &mut Option<String> {
            match slot {
                VectorSlot::A => &mut self.a,
                VectorSlot::B => &mut self.b,
            }
        }
        fn eligible(&self) -> bool {
            self.sensitivity != Sensitivity::Sealed
                && (self.content.is_some() || self.a.is_some() || self.b.is_some())
        }
        fn pending(&self, slot: VectorSlot, model: &str) -> bool {
            self.eligible() && self.slot(slot).as_deref() != Some(model)
        }
    }

    fn row(unit: &str, n: u128, a: Option<&str>, b: Option<&str>) -> Row {
        Row {
            unit: unit.into(),
            id: Uuid::from_u128(n),
            sensitivity: Sensitivity::Open,
            content: Some(format!("row {n}")),
            a: a.map(str::to_string),
            b: b.map(str::to_string),
        }
    }

    fn state(unit: &str, active: VectorSlot, a: Option<&str>, b: Option<&str>) -> UnitState {
        UnitState {
            unit: unit.into(),
            active_slot: active,
            model_a: a.map(str::to_string),
            model_b: b.map(str::to_string),
            flipped_at: None,
        }
    }

    #[derive(Default)]
    struct Store {
        rows: Vec<Row>,
        states: BTreeMap<String, UnitState>,
        intent: Intent,
        published: Vec<Published>,
        /// Every control-row call, by name.
        control_calls: Vec<&'static str>,
        flips: Vec<FlipRequest>,
        /// The `after` of every next_batch call, per pass reset by the test.
        afters: Vec<Option<Uuid>>,
        /// Bumps the control generation after this many stores, to stand in for a command that
        /// commits mid-pass.
        bump_after_stores: Option<usize>,
        stores: usize,
    }

    #[derive(Default)]
    struct FakeRepo(Mutex<Store>);

    impl FakeRepo {
        fn with(rows: Vec<Row>, states: Vec<UnitState>) -> Arc<Self> {
            let repo = FakeRepo::default();
            {
                let mut s = repo.0.lock().unwrap();
                s.rows = rows;
                s.states = states.into_iter().map(|st| (st.unit.clone(), st)).collect();
            }
            Arc::new(repo)
        }
        fn store(&self) -> std::sync::MutexGuard<'_, Store> {
            self.0.lock().unwrap()
        }
        fn row(&self, n: u128) -> Row {
            self.store().rows.iter().find(|r| r.id == Uuid::from_u128(n)).cloned().unwrap()
        }
        fn state_of(&self, unit: &str) -> UnitState {
            self.store().states[unit].clone()
        }
    }

    #[async_trait]
    impl EmbeddingMigrationRepository for FakeRepo {
        async fn seed(&self) -> Result<Vec<UnitState>> {
            let mut s = self.store();
            let units: Vec<String> = s.rows.iter().map(|r| r.unit.clone()).collect();
            let mut made = vec![];
            for unit in units {
                if s.states.contains_key(&unit) {
                    continue;
                }
                let majority = |slot: VectorSlot| -> Option<String> {
                    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
                    for r in s.rows.iter().filter(|r| r.unit == unit) {
                        if let Some(m) = r.slot(slot) {
                            *tally.entry(m.clone()).or_default() += 1;
                        }
                    }
                    tally.into_iter().max_by_key(|(_, n)| *n).map(|(m, _)| m)
                };
                let (a, b) = (majority(VectorSlot::A), majority(VectorSlot::B));
                if a.is_none() && b.is_none() {
                    continue;
                }
                let active = if a.is_some() { VectorSlot::A } else { VectorSlot::B };
                let st = UnitState {
                    unit: unit.clone(),
                    active_slot: active,
                    model_a: a,
                    model_b: b,
                    flipped_at: None,
                };
                s.states.insert(unit, st.clone());
                made.push(st);
            }
            Ok(made)
        }
        async fn states(&self) -> Result<Vec<UnitState>> {
            Ok(self.store().states.values().cloned().collect())
        }
        async fn state(&self, unit: &str) -> Result<Option<UnitState>> {
            Ok(self.store().states.get(unit).cloned())
        }
        async fn name_slot(&self, unit: &str, slot: VectorSlot, model: &str) -> Result<bool> {
            let mut s = self.store();
            let Some(st) = s.states.get_mut(unit) else { return Ok(false) };
            if st.active_slot == slot
                || st.model(slot).is_some()
                || st.model(slot.other()) == Some(model)
            {
                return Ok(false);
            }
            match slot {
                VectorSlot::A => st.model_a = Some(model.into()),
                VectorSlot::B => st.model_b = Some(model.into()),
            }
            Ok(true)
        }
        async fn counts(
            &self,
            unit: &str,
            state: &UnitState,
            target: Option<&str>,
            retire: Option<&str>,
        ) -> Result<Counts> {
            let s = self.store();
            let (act, oth) = (state.active_slot, state.active_slot.other());
            let rows: Vec<&Row> = s.rows.iter().filter(|r| r.unit == unit).collect();
            let count = |f: &dyn Fn(&Row) -> bool| rows.iter().filter(|r| f(r)).count() as i64;
            Ok(Counts {
                eligible: count(&|r| r.eligible()),
                other_pending: target.map_or(0, |t| count(&|r| r.pending(oth, t))),
                active_holes: count(&|r| {
                    r.sensitivity != Sensitivity::Sealed
                        && (r.content.is_some() || r.slot(oth).is_some())
                        && r.slot(act).is_none()
                }),
                foreign_model: count(&|r| {
                    r.slot(act).as_deref().is_some_and(|m| m != state.active_model())
                }),
                failed: 0,
                without_vector: count(&|r| {
                    r.sensitivity != Sensitivity::Sealed
                        && r.content.is_none()
                        && r.a.is_none()
                        && r.b.is_none()
                }),
                retire_pending: retire.map_or(0, |m| count(&|r| r.slot(oth).as_deref() == Some(m))),
            })
        }
        async fn next_batch(
            &self,
            unit: &str,
            slot: VectorSlot,
            model: &str,
            after: Option<Uuid>,
            limit: i64,
        ) -> Result<Vec<PendingRow>> {
            let mut s = self.store();
            s.afters.push(after);
            let mut rows: Vec<&Row> = s
                .rows
                .iter()
                .filter(|r| {
                    r.unit == unit && r.pending(slot, model) && after.is_none_or(|a| r.id > a)
                })
                .collect();
            rows.sort_by_key(|r| r.id);
            Ok(rows
                .into_iter()
                .take(limit as usize)
                .map(|r| PendingRow {
                    id: r.id,
                    sensitivity: r.sensitivity,
                    content: r.content.clone(),
                    chars: r.content.as_ref().map_or(40, |c| c.len() as i64),
                })
                .collect())
        }
        async fn store(
            &self,
            unit: &str,
            id: Uuid,
            slot: VectorSlot,
            model: &str,
            _vector: Vec<f32>,
        ) -> Result<bool> {
            let mut s = self.store();
            let names = s.states.get(unit).is_some_and(|st| st.model(slot) == Some(model));
            s.stores += 1;
            if s.bump_after_stores == Some(s.stores) {
                s.intent.generation += 1;
            }
            let Some(r) = s.rows.iter_mut().find(|r| r.unit == unit && r.id == id) else {
                return Ok(false);
            };
            if !names || !r.pending(slot, model) {
                return Ok(false);
            }
            *r.slot_mut(slot) = Some(model.into());
            Ok(true)
        }
        async fn flip(&self, req: &FlipRequest) -> Result<FlipOutcome> {
            let mut s = self.store();
            s.flips.push(req.clone());
            let unit = req.expect.unit.clone();
            if s.states.get(&unit) != Some(&req.expect) {
                return Ok(FlipOutcome::Stale);
            }
            if req.expect_generation.is_some_and(|g| g != s.intent.generation) {
                return Ok(FlipOutcome::Stale);
            }
            let n = s
                .rows
                .iter()
                .filter(|r| r.unit == unit && r.pending(req.target_slot, &req.target_model))
                .count() as i64;
            if n > 0 {
                return Ok(FlipOutcome::Incomplete(n));
            }
            let st = s.states.get_mut(&unit).unwrap();
            st.active_slot = req.target_slot;
            st.flipped_at = Some(Utc::now());
            Ok(FlipOutcome::Flipped { flip_ms: 1 })
        }
        async fn retire_batch(
            &self,
            unit: &str,
            slot: VectorSlot,
            model: &str,
            limit: i64,
        ) -> Result<u64> {
            let mut s = self.store();
            let mut n = 0u64;
            for r in s.rows.iter_mut().filter(|r| r.unit == unit) {
                if n as i64 >= limit {
                    break;
                }
                if r.slot(slot).as_deref() == Some(model) {
                    *r.slot_mut(slot) = None;
                    n += 1;
                }
            }
            Ok(n)
        }
        async fn clear_slot(&self, unit: &str, slot: VectorSlot, model: &str) -> Result<bool> {
            let mut s = self.store();
            if s.rows.iter().any(|r| r.unit == unit && r.slot(slot).as_deref() == Some(model)) {
                return Ok(false);
            }
            let Some(st) = s.states.get_mut(unit) else { return Ok(false) };
            if st.active_slot == slot || st.model(slot) != Some(model) {
                return Ok(false);
            }
            match slot {
                VectorSlot::A => st.model_a = None,
                VectorSlot::B => st.model_b = None,
            }
            Ok(true)
        }
        async fn control(&self) -> Result<(Intent, Published)> {
            let mut s = self.store();
            s.control_calls.push("control");
            Ok((s.intent.clone(), Published::default()))
        }
        async fn generation(&self) -> Result<i64> {
            let mut s = self.store();
            s.control_calls.push("generation");
            Ok(s.intent.generation)
        }
        async fn change_intent(
            &self,
            _decide: &(dyn for<'i, 's> Fn(
                &'i Intent,
                &'s [UnitState],
            ) -> std::result::Result<Option<IntentChange>, String>
                  + Send
                  + Sync),
        ) -> Result<ChangeOutcome> {
            unreachable!("the sweep never writes intent")
        }
        async fn publish(&self, published: &Published) -> Result<()> {
            let mut s = self.store();
            s.control_calls.push("publish");
            s.published.push(published.clone());
            Ok(())
        }
    }

    // ── embedders, opener, disk ───────────────────────────────────────────────────────────────

    struct FakeEmbedder {
        id: String,
        calls: AtomicUsize,
        down: AtomicBool,
        /// A request holding any of these texts is refused whole, as llama-server refuses one
        /// over-long input.
        refuse: Mutex<HashSet<String>>,
        delay: Duration,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl FakeEmbedder {
        fn new(id: &str, log: &Arc<Mutex<Vec<String>>>) -> Arc<Self> {
            Arc::new(FakeEmbedder {
                id: id.into(),
                calls: AtomicUsize::new(0),
                down: AtomicBool::new(false),
                refuse: Mutex::new(HashSet::new()),
                delay: Duration::ZERO,
                log: Arc::clone(log),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
        fn refuse(&self, n: u128) {
            self.refuse.lock().unwrap().insert(format!("row {n}"));
        }
        fn accept_all(&self) {
            self.refuse.lock().unwrap().clear();
        }
    }

    #[async_trait]
    impl Embedder for FakeEmbedder {
        fn id(&self) -> String {
            self.id.clone()
        }
        fn dim(&self) -> usize {
            4
        }
        async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.log.lock().unwrap().push(format!("{} {}", self.id, texts.join("|")));
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if self.down.load(Ordering::SeqCst) {
                return Err(DomainError::unavailable("embedder unreachable"));
            }
            if texts.iter().any(|t| self.refuse.lock().unwrap().contains(t)) {
                return Err(DomainError::unavailable("input exceeds the context window"));
            }
            Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
        }
        async fn embed_query(&self, _text: &str) -> Result<Vec<f32>> {
            unreachable!("the sweep embeds documents only")
        }
    }

    #[derive(Default)]
    struct FakeOpener(HashMap<Uuid, String>);

    #[async_trait]
    impl RowOpener for FakeOpener {
        async fn open(&self, _unit: &str, ids: &[Uuid]) -> Result<HashMap<Uuid, String>> {
            Ok(ids.iter().filter_map(|id| self.0.get(id).map(|t| (*id, t.clone()))).collect())
        }
    }

    struct FakeDisk(AtomicU64);

    impl FreeSpace for FakeDisk {
        fn free_bytes(&self) -> Result<u64> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }

    struct Listed(Vec<String>);

    #[async_trait]
    impl Units for Listed {
        async fn list(&self) -> Vec<String> {
            self.0.clone()
        }
    }

    // ── harness ───────────────────────────────────────────────────────────────────────────────

    fn knobs() -> Knobs {
        Knobs { fill_chars: 4000, fill_duty: 100, floor_bytes: 0, fill_budget: FILL_BUDGET }
    }

    fn view(previous: Option<&str>) -> Configured {
        Configured {
            current: C.into(),
            previous: previous.map(str::to_string),
            retire: None,
            flip: FlipScope::All,
            rollback_days: 7,
            guessed_acting: BTreeMap::new(),
            generation: None,
        }
    }

    fn command() -> Steer {
        Steer::Command {
            blocks: vec![C.into(), P.into()],
            rollback_days: 7,
            guessed_acting: BTreeMap::new(),
        }
    }

    struct Rig {
        repo: Arc<FakeRepo>,
        p: Arc<FakeEmbedder>,
        c: Arc<FakeEmbedder>,
        log: Arc<Mutex<Vec<String>>>,
    }

    fn rig(rows: Vec<Row>, states: Vec<UnitState>) -> Rig {
        let log = Arc::new(Mutex::new(vec![]));
        Rig {
            repo: FakeRepo::with(rows, states),
            p: FakeEmbedder::new(P, &log),
            c: FakeEmbedder::new(C, &log),
            log,
        }
    }

    impl Rig {
        fn sweep(&self, steer: Steer, knobs: Knobs) -> Sweep {
            self.sweep_with(steer, knobs, Arc::new(FakeOpener::default()), true, None)
        }
        fn sweep_with(
            &self,
            steer: Steer,
            knobs: Knobs,
            opener: Arc<dyn RowOpener>,
            kek_verified: bool,
            disk: Option<Arc<dyn FreeSpace>>,
        ) -> Sweep {
            let initial = match &steer {
                Steer::Env(v) => v.clone(),
                Steer::Command { .. } => view(None),
            };
            let thresholds: HashMap<String, Arc<SimilarityThresholds>> = [P, C]
                .iter()
                .map(|id| (id.to_string(), Arc::new(SimilarityThresholds::default())))
                .collect();
            let built: Vec<Arc<dyn Embedder>> =
                vec![self.c.clone() as Arc<dyn Embedder>, self.p.clone() as Arc<dyn Embedder>];
            let embedders = Arc::new(EmbedderSet::new(built, initial, thresholds));
            Sweep::new(self.repo.clone(), embedders, opener, knobs, steer, kek_verified, disk)
        }
    }

    fn me() -> SingleUnit {
        SingleUnit("me".into())
    }

    fn unit_status(sweep: &Sweep, unit: &str) -> UnitStatus {
        sweep.status.read().unwrap().units.iter().find(|u| u.unit == unit).cloned().unwrap()
    }

    /// A filling unit: active on P in slot A, slot B named C, `n` rows holding only P.
    fn filling(n: u128) -> Rig {
        let rows = (1..=n).map(|i| row("me", i, Some(P), None)).collect();
        rig(rows, vec![state("me", VectorSlot::A, Some(P), Some(C))])
    }

    // ── steady ────────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_steady_unit_with_no_hole_gets_no_embedding_call() {
        let r =
            rig(vec![row("me", 1, Some(C), None)], vec![state("me", VectorSlot::A, Some(C), None)]);
        let sweep = r.sweep(Steer::Env(view(None)), knobs());
        sweep.pass(&me()).await;
        assert_eq!(r.c.calls() + r.p.calls(), 0);
        assert_eq!(unit_status(&sweep, "me").phase, Phase::Steady);
    }

    #[tokio::test]
    async fn a_steady_unit_fills_its_active_holes() {
        let r = rig(
            vec![row("me", 1, Some(C), None), row("me", 2, None, None)],
            vec![state("me", VectorSlot::A, Some(C), None)],
        );
        let sweep = r.sweep(Steer::Env(view(None)), knobs());
        sweep.pass(&me()).await;
        assert_eq!(r.repo.row(2).a.as_deref(), Some(C));
        assert_eq!(r.c.calls(), 1);
        assert_eq!(r.p.calls(), 0);
    }

    #[tokio::test]
    async fn a_third_model_in_the_other_slot_gets_no_embedding_call() {
        let r = rig(
            vec![row("me", 1, Some(C), Some(X)), row("me", 2, None, Some(X))],
            vec![state("me", VectorSlot::A, Some(C), Some(X))],
        );
        let sweep = r.sweep(Steer::Env(view(None)), knobs());
        let report = sweep.pass(&me()).await;
        assert_eq!(r.c.calls(), 0);
        assert_eq!(unit_status(&sweep, "me").blocked, Some(Blocked::OtherHoldsThird(X.into())));
        assert!(report.ready.is_empty(), "a blocked unit is not ready");
    }

    // ── filling, held, ready ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_sweep_names_the_other_slot_before_it_fills() {
        let r =
            rig(vec![row("me", 1, Some(P), None)], vec![state("me", VectorSlot::A, Some(P), None)]);
        let sweep =
            r.sweep(Steer::Env(Configured { flip: FlipScope::None, ..view(Some(P)) }), knobs());
        sweep.pass(&me()).await;
        assert_eq!(r.repo.state_of("me").model_b.as_deref(), Some(C));
        assert_eq!(r.repo.row(1).b.as_deref(), Some(C));
        assert_eq!(sweep.embedders.for_unit("me").unwrap().slot, VectorSlot::A);
    }

    #[tokio::test]
    async fn flip_none_never_flips() {
        let r = filling(3);
        let sweep =
            r.sweep(Steer::Env(Configured { flip: FlipScope::None, ..view(Some(P)) }), knobs());
        sweep.pass(&me()).await;
        sweep.pass(&me()).await;
        assert!(r.repo.store().flips.is_empty());
        assert_eq!(r.repo.state_of("me").active_slot, VectorSlot::A);
        assert_eq!(unit_status(&sweep, "me").phase, Phase::Held);
    }

    #[tokio::test]
    async fn a_listed_unit_flips_and_an_unlisted_one_holds() {
        let r = rig(
            vec![row("me", 1, Some(P), Some(C)), row("acme", 2, Some(P), Some(C))],
            vec![
                state("me", VectorSlot::A, Some(P), Some(C)),
                state("acme", VectorSlot::A, Some(P), Some(C)),
            ],
        );
        let flip = FlipScope::Units(vec!["me".into()]);
        let sweep = r.sweep(Steer::Env(Configured { flip, ..view(Some(P)) }), knobs());
        sweep.pass(&Listed(vec!["me".into(), "acme".into()])).await;
        assert_eq!(r.repo.state_of("me").active_slot, VectorSlot::B);
        assert_eq!(r.repo.state_of("acme").active_slot, VectorSlot::A);
        assert_eq!(unit_status(&sweep, "acme").phase, Phase::Held);
        assert_eq!(
            sweep.embedders.for_unit("me").unwrap().slot,
            VectorSlot::B,
            "the map follows the flip"
        );
    }

    #[tokio::test]
    async fn a_unit_with_a_state_row_outside_the_list_is_walked() {
        let r = rig(
            vec![
                row("me", 1, Some(C), None),
                row("orphan", 2, None, None),
                row("orphan", 3, Some(C), None),
            ],
            vec![
                state("me", VectorSlot::A, Some(C), None),
                state("orphan", VectorSlot::A, Some(C), None),
            ],
        );
        let sweep = r.sweep(Steer::Env(view(None)), knobs());
        sweep.pass(&me()).await;
        assert_eq!(r.repo.row(2).a.as_deref(), Some(C));
        assert_eq!(unit_status(&sweep, "orphan").phase, Phase::Steady);
    }

    #[tokio::test]
    async fn a_unit_with_no_state_row_is_seeded_before_its_fill() {
        let r = rig(vec![row("me", 1, Some(P), None)], vec![]);
        let sweep =
            r.sweep(Steer::Env(Configured { flip: FlipScope::None, ..view(Some(P)) }), knobs());
        sweep.pass(&me()).await;
        assert_eq!(r.repo.state_of("me").model_a.as_deref(), Some(P));
        assert_eq!(r.repo.row(1).b.as_deref(), Some(C));
    }

    // ── flipped and retiring ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_flipped_phase_fills_an_active_hole_before_the_other_slot() {
        let r = rig(
            vec![row("me", 1, Some(P), None), row("me", 2, None, Some(C))],
            vec![state("me", VectorSlot::B, Some(P), Some(C))],
        );
        let sweep = r.sweep(Steer::Env(view(Some(P))), knobs());
        sweep.pass(&me()).await;
        let log = r.log.lock().unwrap().clone();
        assert_eq!(log, vec![format!("{C} row 1"), format!("{P} row 2")]);
        assert_eq!(r.repo.row(1).b.as_deref(), Some(C));
        assert_eq!(r.repo.row(2).a.as_deref(), Some(P));
    }

    fn retiring(flipped_days_ago: i64) -> Rig {
        let mut st = state("me", VectorSlot::B, Some(P), Some(C));
        st.flipped_at = Some(Utc::now() - chrono::Duration::days(flipped_days_ago));
        rig(vec![row("me", 1, Some(P), None), row("me", 2, Some(P), Some(C))], vec![st])
    }

    fn retire_view() -> Configured {
        Configured { retire: Some(P.into()), ..view(None) }
    }

    #[tokio::test]
    async fn retiring_fills_holes_before_deleting() {
        let r = retiring(30);
        r.c.refuse(1);
        let sweep = r.sweep(Steer::Env(retire_view()), knobs());
        sweep.pass(&me()).await;
        assert_eq!(r.repo.row(2).a.as_deref(), Some(P), "nothing deleted while a hole remains");
        assert_eq!(r.repo.state_of("me").model_a.as_deref(), Some(P));

        r.c.accept_all();
        sweep.pass(&me()).await;
        assert_eq!(r.repo.row(1).b.as_deref(), Some(C));
        assert!(r.repo.row(1).a.is_none() && r.repo.row(2).a.is_none());
        assert_eq!(r.repo.state_of("me").model_a, None, "the slot is cleared");
    }

    #[tokio::test]
    async fn retire_waits_for_the_rollback_window() {
        let r = retiring(2);
        let sweep = r.sweep(Steer::Env(retire_view()), knobs());
        sweep.pass(&me()).await;
        assert_eq!(r.repo.row(2).a.as_deref(), Some(P));
        let status = unit_status(&sweep, "me");
        assert_eq!(status.phase, Phase::Retiring);
        assert!(status.retire_after.is_some_and(|at| at > Utc::now()));
    }

    // ── failures ──────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_lone_refused_row_counts_as_failed() {
        // Row 1 is over the request cap, so it goes alone; row 2 travels in its own request.
        let mut long = row("me", 1, Some(P), None);
        long.content = Some("x".repeat(5000));
        let r = rig(
            vec![long, row("me", 2, Some(P), None)],
            vec![state("me", VectorSlot::A, Some(P), Some(C))],
        );
        r.c.refuse.lock().unwrap().insert("x".repeat(5000));
        let sweep =
            r.sweep(Steer::Env(Configured { flip: FlipScope::None, ..view(Some(P)) }), knobs());
        sweep.pass(&me()).await;
        assert_eq!(sweep.failures("me", VectorSlot::B, Uuid::from_u128(1)), 1);
        assert_eq!(r.repo.row(2).b.as_deref(), Some(C));
    }

    #[tokio::test]
    async fn a_refused_row_inside_a_batch_is_found_alone() {
        let r = filling(3);
        r.c.refuse(2);
        let sweep = r.sweep(Steer::Env(view(Some(P))), knobs());
        sweep.pass(&me()).await;
        assert_eq!(sweep.failures("me", VectorSlot::B, Uuid::from_u128(2)), 1);
        assert_eq!(sweep.failures("me", VectorSlot::B, Uuid::from_u128(1)), 0);
        assert_eq!(r.repo.row(1).b.as_deref(), Some(C));
        assert_eq!(r.repo.row(3).b.as_deref(), Some(C));
    }

    /// Three passes with a fresh row each, so the embedder answers something every pass.
    async fn fail_row_one_three_times(r: &Rig, sweep: &Sweep) {
        r.c.refuse(1);
        for n in 10..13 {
            r.repo.store().rows.push(row("me", n, Some(P), None));
            sweep.pass(&me()).await;
        }
    }

    #[tokio::test]
    async fn a_failing_row_blocks_the_flip() {
        let r = filling(1);
        let sweep = r.sweep(Steer::Env(view(Some(P))), knobs());
        fail_row_one_three_times(&r, &sweep).await;
        let status = unit_status(&sweep, "me");
        assert_eq!(status.blocked, Some(Blocked::Failed(1)));
        assert_eq!(status.failed_ids, vec![Uuid::from_u128(1)]);
        assert_eq!(status.counts.failed, 1);
        assert!(r.repo.store().flips.is_empty());
        assert_eq!(r.repo.state_of("me").active_slot, VectorSlot::A);
    }

    #[tokio::test]
    async fn a_failed_unit_keeps_filling_and_retries() {
        let r = filling(1);
        let sweep = r.sweep(Steer::Env(view(Some(P))), knobs());
        fail_row_one_three_times(&r, &sweep).await;
        r.repo.store().rows.push(row("me", 20, Some(P), None));
        let before = r.log.lock().unwrap().len();
        sweep.pass(&me()).await;
        let sent: Vec<String> = r.log.lock().unwrap()[before..].to_vec();
        assert!(sent.iter().any(|s| s.contains("row 1")), "the failed row is retried: {sent:?}");
        assert_eq!(r.repo.row(20).b.as_deref(), Some(C), "the fill carries on");
    }

    #[tokio::test]
    async fn a_recovered_embedder_unblocks_the_unit() {
        let r = filling(1);
        let sweep =
            r.sweep(Steer::Env(Configured { flip: FlipScope::None, ..view(Some(P)) }), knobs());
        fail_row_one_three_times(&r, &sweep).await;
        assert_eq!(unit_status(&sweep, "me").phase, Phase::Blocked);
        r.c.accept_all();
        sweep.pass(&me()).await;
        let status = unit_status(&sweep, "me");
        assert_eq!(status.blocked, None);
        assert_eq!(status.phase, Phase::Held);
        assert!(status.failed_ids.is_empty());
    }

    #[tokio::test]
    async fn a_failed_row_that_leaves_the_store_unblocks_the_unit() {
        let r = filling(1);
        let sweep =
            r.sweep(Steer::Env(Configured { flip: FlipScope::None, ..view(Some(P)) }), knobs());
        fail_row_one_three_times(&r, &sweep).await;
        assert_eq!(unit_status(&sweep, "me").blocked, Some(Blocked::Failed(1)));
        r.repo.store().rows.retain(|row| row.id != Uuid::from_u128(1));
        sweep.pass(&me()).await;
        assert_eq!(unit_status(&sweep, "me").blocked, None);
        assert_eq!(unit_status(&sweep, "me").phase, Phase::Held);
    }

    #[tokio::test]
    async fn an_outage_batch_counts_against_no_row() {
        let r = filling(3);
        r.c.down.store(true, Ordering::SeqCst);
        let sweep = r.sweep(Steer::Env(view(Some(P))), knobs());
        sweep.pass(&me()).await;
        for n in 1..=3 {
            assert_eq!(sweep.failures("me", VectorSlot::B, Uuid::from_u128(n)), 0);
        }
        let error = sweep.status.read().unwrap().error.clone();
        assert!(error.as_deref().is_some_and(|e| e.contains(C)), "{error:?}");
        // The batch, its first row alone, then the probe that finds the model down.
        assert_eq!(r.c.calls(), 3, "an outage stops the fill");
    }

    #[tokio::test]
    async fn an_unopenable_private_row_counts_as_failed() {
        let mut private = row("me", 1, Some(P), None);
        private.sensitivity = Sensitivity::Private;
        private.content = None;
        let mut opens = row("me", 2, Some(P), None);
        opens.sensitivity = Sensitivity::Private;
        opens.content = None;
        let r = rig(vec![private, opens], vec![state("me", VectorSlot::A, Some(P), Some(C))]);
        let opener = FakeOpener(HashMap::from([(Uuid::from_u128(2), "opened text".to_string())]));
        let sweep = r.sweep_with(Steer::Env(view(Some(P))), knobs(), Arc::new(opener), true, None);
        sweep.pass(&me()).await;
        assert_eq!(sweep.failures("me", VectorSlot::B, Uuid::from_u128(1)), 1);
        assert_eq!(r.repo.row(2).b.as_deref(), Some(C));
        assert!(r.log.lock().unwrap().iter().any(|s| s.contains("opened text")));
    }

    #[tokio::test]
    async fn an_unverified_kek_skips_private_rows_and_reports_kek() {
        let mut private = row("me", 1, Some(P), None);
        private.sensitivity = Sensitivity::Private;
        private.content = None;
        let r = rig(
            vec![private, row("me", 2, Some(P), None)],
            vec![state("me", VectorSlot::A, Some(P), Some(C))],
        );
        let opener = FakeOpener(HashMap::from([(Uuid::from_u128(1), "secret".to_string())]));
        let sweep = r.sweep_with(Steer::Env(view(Some(P))), knobs(), Arc::new(opener), false, None);
        sweep.pass(&me()).await;
        assert!(r.repo.row(1).b.is_none());
        assert_eq!(r.repo.row(2).b.as_deref(), Some(C), "open rows still fill");
        assert_eq!(unit_status(&sweep, "me").blocked, Some(Blocked::Kek));
        assert_eq!(sweep.failures("me", VectorSlot::B, Uuid::from_u128(1)), 0);
    }

    // ── cursor and budget ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_cursor_restarts_every_pass() {
        let r = rig(
            (100..170).map(|i| row("me", i, Some(P), None)).collect(),
            vec![state("me", VectorSlot::A, Some(P), Some(C))],
        );
        let sweep =
            r.sweep(Steer::Env(Configured { flip: FlipScope::None, ..view(Some(P)) }), knobs());
        sweep.pass(&me()).await;
        let first = std::mem::take(&mut r.repo.store().afters);
        assert_eq!(first.first(), Some(&None));
        assert!(first.len() >= 2, "70 rows take two pages: {first:?}");

        // A row written mid-migration can sort below where the last cursor stopped.
        r.repo.store().rows.push(row("me", 1, Some(P), None));
        sweep.pass(&me()).await;
        assert_eq!(r.repo.store().afters.first(), Some(&None));
        assert_eq!(r.repo.row(1).b.as_deref(), Some(C));
    }

    #[tokio::test]
    async fn the_fill_stops_at_its_budget_and_publishes() {
        let rows: Vec<Row> = (1..=20)
            .map(|i| Row {
                content: Some(format!("row {i} {}", "y".repeat(3000))),
                ..row("me", i, Some(P), None)
            })
            .collect();
        let mut rows = rows;
        rows.push(row("other", 99, Some(P), None));
        let log = Arc::new(Mutex::new(vec![]));
        let slow = Arc::new(FakeEmbedder {
            delay: Duration::from_millis(30),
            ..Arc::try_unwrap(FakeEmbedder::new(C, &log)).ok().unwrap()
        });
        let r = Rig {
            repo: FakeRepo::with(
                rows,
                vec![
                    state("me", VectorSlot::A, Some(P), Some(C)),
                    state("other", VectorSlot::A, Some(P), Some(C)),
                ],
            ),
            p: FakeEmbedder::new(P, &log),
            c: slow,
            log,
        };
        r.repo.store().intent =
            Intent { target: Some(C.into()), generation: 3, ..Intent::default() };
        let sweep = r.sweep(command(), Knobs { fill_budget: Duration::from_millis(80), ..knobs() });
        sweep.pass(&Listed(vec!["me".into(), "other".into()])).await;
        let filled = (1..=20).filter(|i| r.repo.row(*i).b.is_some()).count();
        assert!(filled > 0 && filled < 20, "the budget ends the unit's fill: {filled} filled");
        assert_eq!(r.repo.row(99).b.as_deref(), Some(C), "the pass moves on to the next unit");
        assert_eq!(r.repo.store().published.last().map(|p| p.applied_generation), Some(3));
    }

    // ── disk ──────────────────────────────────────────────────────────────────────────────────

    fn disk(free: u64) -> Option<Arc<dyn FreeSpace>> {
        Some(Arc::new(FakeDisk(AtomicU64::new(free))))
    }

    #[tokio::test]
    async fn the_sweep_pauses_below_the_disk_floor() {
        let r = filling(2);
        let k = Knobs { floor_bytes: 1000, ..knobs() };
        let sweep = r.sweep_with(
            Steer::Env(view(Some(P))),
            k,
            Arc::new(FakeOpener::default()),
            true,
            disk(999),
        );
        sweep.pass(&me()).await;
        assert_eq!(r.c.calls(), 0);
        assert!(r.repo.row(1).b.is_none());
        let d = sweep.status.read().unwrap().disk.unwrap();
        assert_eq!((d.free_bytes, d.floor_bytes, d.paused), (999, 1000, true));
    }

    #[tokio::test]
    async fn the_sweep_resumes_at_the_floor() {
        let r = filling(2);
        let k = Knobs { floor_bytes: 1000, ..knobs() };
        let space = Arc::new(FakeDisk(AtomicU64::new(10)));
        let sweep = r.sweep_with(
            Steer::Env(view(Some(P))),
            k,
            Arc::new(FakeOpener::default()),
            true,
            Some(space.clone() as Arc<dyn FreeSpace>),
        );
        sweep.pass(&me()).await;
        assert!(r.repo.row(1).b.is_none());
        space.0.store(1000, Ordering::SeqCst);
        sweep.pass(&me()).await;
        assert_eq!(r.repo.row(1).b.as_deref(), Some(C));
        assert!(!sweep.status.read().unwrap().disk.unwrap().paused);
    }

    /// Free space that reads from a script, one value per call, then the last value forever.
    struct Falling(Mutex<Vec<u64>>);

    impl FreeSpace for Falling {
        fn free_bytes(&self) -> Result<u64> {
            let mut left = self.0.lock().unwrap();
            Ok(if left.len() > 1 { left.remove(0) } else { left[0] })
        }
    }

    #[tokio::test]
    async fn the_fill_stops_at_the_next_page_when_the_disk_falls_below_the_floor() {
        let r = rig(
            (100..170).map(|i| row("me", i, Some(P), None)).collect(),
            vec![state("me", VectorSlot::A, Some(P), Some(C))],
        );
        // The pass start and the first page read room; the second page does not.
        let space = Arc::new(Falling(Mutex::new(vec![5000, 5000, 10])));
        let k = Knobs { floor_bytes: 1000, ..knobs() };
        let sweep = r.sweep_with(
            Steer::Env(view(Some(P))),
            k,
            Arc::new(FakeOpener::default()),
            true,
            Some(space),
        );
        sweep.pass(&me()).await;
        let filled = (100..170).filter(|i| r.repo.row(*i).b.is_some()).count();
        assert_eq!(filled, PAGE as usize);
        assert!(sweep.status.read().unwrap().disk.unwrap().paused);
    }

    #[tokio::test]
    async fn a_disk_pause_publishes() {
        let r = filling(2);
        r.repo.store().intent =
            Intent { target: Some(C.into()), generation: 4, ..Intent::default() };
        let k = Knobs { floor_bytes: 1000, ..knobs() };
        let sweep = r.sweep_with(command(), k, Arc::new(FakeOpener::default()), true, disk(1));
        sweep.pass(&me()).await;
        let published = r.repo.store().published.clone();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].applied_generation, 4);
        assert_eq!(r.c.calls(), 0);
    }

    // ── steering ──────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn command_mode_reads_the_control_row_each_pass() {
        let r =
            rig(vec![row("me", 1, Some(P), None)], vec![state("me", VectorSlot::A, Some(P), None)]);
        let sweep = r.sweep(command(), knobs());
        sweep.pass(&me()).await;
        assert_eq!(r.repo.state_of("me").model_b, None, "no target, nothing named");
        assert_eq!(sweep.embedders.configured().previous, None);

        r.repo.store().intent =
            Intent { target: Some(C.into()), generation: 1, ..Intent::default() };
        sweep.pass(&me()).await;
        assert_eq!(r.repo.state_of("me").model_b.as_deref(), Some(C));
        assert_eq!(r.repo.row(1).b.as_deref(), Some(C));
        assert_eq!(
            sweep.embedders.configured().previous.as_deref(),
            Some(P),
            "writes now embed with both"
        );
        let reads = r.repo.store().control_calls.iter().filter(|c| **c == "control").count();
        assert_eq!(reads, 2);
    }

    #[tokio::test]
    async fn a_moved_generation_ends_the_fill() {
        let r = rig(
            (100..170).map(|i| row("me", i, Some(P), None)).collect(),
            vec![state("me", VectorSlot::A, Some(P), Some(C))],
        );
        {
            let mut s = r.repo.store();
            s.intent = Intent { target: Some(C.into()), generation: 7, ..Intent::default() };
            s.bump_after_stores = Some(1);
        }
        let sweep = r.sweep(command(), knobs());
        sweep.pass(&me()).await;
        let filled = (100..170).filter(|i| r.repo.row(*i).b.is_some()).count();
        assert_eq!(filled, PAGE as usize, "the page in hand finishes and no further page starts");
        assert_eq!(r.repo.store().published.last().map(|p| p.applied_generation), Some(7));
    }

    #[tokio::test]
    async fn the_flip_carries_the_pass_generation() {
        let r = rig(
            vec![row("me", 1, Some(P), Some(C))],
            vec![state("me", VectorSlot::A, Some(P), Some(C))],
        );
        r.repo.store().intent =
            Intent { target: Some(C.into()), flip: true, generation: 5, ..Intent::default() };
        let sweep = r.sweep(command(), knobs());
        sweep.pass(&me()).await;
        let flips = r.repo.store().flips.clone();
        assert_eq!(flips.len(), 1);
        assert_eq!(flips[0].expect_generation, Some(5));
        assert_eq!(flips[0].target_slot, VectorSlot::B);
        assert_eq!(r.repo.state_of("me").active_slot, VectorSlot::B);
    }

    #[tokio::test]
    async fn command_mode_publishes_each_pass() {
        let r =
            rig(vec![row("me", 1, Some(P), None)], vec![state("me", VectorSlot::A, Some(P), None)]);
        r.repo.store().intent.generation = 2;
        let sweep = r.sweep(command(), knobs());
        sweep.pass(&me()).await;
        sweep.pass(&me()).await;
        let published = r.repo.store().published.clone();
        assert_eq!(published.len(), 2);
        assert!(published.iter().all(|p| p.applied_generation == 2));
        assert_eq!(published[0].server_models, vec![C.to_string(), P.to_string()]);
        assert!(published[0].status.is_some());
    }

    #[tokio::test]
    async fn env_mode_never_touches_the_control_row() {
        let r = rig(
            vec![row("me", 1, Some(P), None)],
            vec![state("me", VectorSlot::A, Some(P), Some(C))],
        );
        let sweep = r.sweep(Steer::Env(view(Some(P))), knobs());
        sweep.pass(&me()).await;
        sweep.pass(&me()).await;
        assert_eq!(
            r.repo.state_of("me").active_slot,
            VectorSlot::B,
            "the env view filled and flipped"
        );
        assert!(r.repo.store().control_calls.is_empty(), "{:?}", r.repo.store().control_calls);
        assert_eq!(r.repo.store().flips[0].expect_generation, None);
    }
}
