//! Postgres implementation of `EmbeddingMigrationRepository` (decision 0027).
//!
//! Every statement that names a slot exists twice, as an A const and a B const, and a `match slot`
//! picks one. A column name is never built at runtime. The B twin exchanges `embedding` with
//! `embedding_b`, `embedding_model` with `embedding_b_model`, `model_a` with `model_b` and `'a'`
//! with `'b'`, and nothing else; `every_slot_b_statement_differs_from_its_a_twin_only_in_the_slot`
//! holds every pair to that. Exchanging both ways is what keeps the eligibility rule whole: the B
//! twin lists the two vector columns in the other order, and the rule still names both.

use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use crate::adapters::postgres::conflict_wake::CONFLICT_WAKE_CHANNEL;
use crate::domain::embedding_migration::{
    Counts, FlipOutcome, FlipRequest, Intent, IntentChange, PendingRow, Published, UnitState, Verb,
};
use crate::domain::embedding_slot::VectorSlot;
use crate::domain::errors::{DomainError, Result};
use crate::domain::types::Sensitivity;
use crate::ports::embedding_migration::{ChangeOutcome, EmbeddingMigrationRepository};

pub struct PgEmbeddingMigrationRepository {
    pool: PgPool,
}

impl PgEmbeddingMigrationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

// ── seed ────────────────────────────────────────────────────────────────────────────────────────

const SEED_COUNTS_A: &str = "\
SELECT tenant_id, embedding_model AS model, count(*) AS rows
  FROM memory WHERE embedding IS NOT NULL AND embedding_model IS NOT NULL
 GROUP BY tenant_id, embedding_model";

const SEED_COUNTS_B: &str = "\
SELECT tenant_id, embedding_b_model AS model, count(*) AS rows
  FROM memory WHERE embedding_b IS NOT NULL AND embedding_b_model IS NOT NULL
 GROUP BY tenant_id, embedding_b_model";

/// ($1 unit, $2 active_slot, $3 model_a, $4 model_b). A unit that gained a row since the counts
/// keeps it: seeding never overwrites a state.
const SEED_INSERT: &str = "\
INSERT INTO embedding_state (tenant_id, active_slot, model_a, model_b)
VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING";

// ── state ───────────────────────────────────────────────────────────────────────────────────────

const STATES: &str = "\
SELECT tenant_id, active_slot, model_a, model_b, flipped_at
  FROM embedding_state ORDER BY tenant_id";

const STATE: &str = "\
SELECT tenant_id, active_slot, model_a, model_b, flipped_at
  FROM embedding_state WHERE tenant_id = $1";

// ── name ────────────────────────────────────────────────────────────────────────────────────────

/// ($1 unit, $2 model). Names slot A only while it is inactive and empty.
const NAME_SLOT_A: &str = "\
UPDATE embedding_state SET model_a = $2, updated_at = now()
 WHERE tenant_id = $1 AND active_slot = 'b' AND model_a IS NULL
   AND model_b IS DISTINCT FROM $2";

const NAME_SLOT_B: &str = "\
UPDATE embedding_state SET model_b = $2, updated_at = now()
 WHERE tenant_id = $1 AND active_slot = 'a' AND model_b IS NULL
   AND model_a IS DISTINCT FROM $2";

// ── counts ──────────────────────────────────────────────────────────────────────────────────────

/// ($1 unit, $2 active model, $3 target or NULL, $4 retire or NULL), for a unit active on slot A.
/// `failed` is not stored; the sweep tracks it in memory.
const COUNTS_ACTIVE_A: &str = "\
SELECT count(*) FILTER (WHERE sensitivity <> 'sealed'
         AND (content IS NOT NULL OR embedding IS NOT NULL OR embedding_b IS NOT NULL)) AS eligible,
       count(*) FILTER (WHERE $3::text IS NOT NULL AND sensitivity <> 'sealed'
         AND (content IS NOT NULL OR embedding IS NOT NULL OR embedding_b IS NOT NULL)
         AND (embedding_b IS NULL OR embedding_b_model IS DISTINCT FROM $3)) AS other_pending,
       count(*) FILTER (WHERE sensitivity <> 'sealed'
         AND (content IS NOT NULL OR embedding_b IS NOT NULL)
         AND embedding IS NULL) AS active_holes,
       count(*) FILTER (WHERE embedding IS NOT NULL
         AND embedding_model IS DISTINCT FROM $2) AS foreign_model,
       count(*) FILTER (WHERE sensitivity <> 'sealed' AND content IS NULL
         AND embedding IS NULL AND embedding_b IS NULL) AS without_vector,
       count(*) FILTER (WHERE $4::text IS NOT NULL AND embedding_b_model = $4) AS retire_pending
  FROM memory WHERE tenant_id = $1";

const COUNTS_ACTIVE_B: &str = "\
SELECT count(*) FILTER (WHERE sensitivity <> 'sealed'
         AND (content IS NOT NULL OR embedding_b IS NOT NULL OR embedding IS NOT NULL)) AS eligible,
       count(*) FILTER (WHERE $3::text IS NOT NULL AND sensitivity <> 'sealed'
         AND (content IS NOT NULL OR embedding_b IS NOT NULL OR embedding IS NOT NULL)
         AND (embedding IS NULL OR embedding_model IS DISTINCT FROM $3)) AS other_pending,
       count(*) FILTER (WHERE sensitivity <> 'sealed'
         AND (content IS NOT NULL OR embedding IS NOT NULL)
         AND embedding_b IS NULL) AS active_holes,
       count(*) FILTER (WHERE embedding_b IS NOT NULL
         AND embedding_b_model IS DISTINCT FROM $2) AS foreign_model,
       count(*) FILTER (WHERE sensitivity <> 'sealed' AND content IS NULL
         AND embedding_b IS NULL AND embedding IS NULL) AS without_vector,
       count(*) FILTER (WHERE $4::text IS NOT NULL AND embedding_model = $4) AS retire_pending
  FROM memory WHERE tenant_id = $1";

// ── fill ────────────────────────────────────────────────────────────────────────────────────────

/// ($1 unit, $2 model, $3 after or NULL, $4 limit): rows pending in slot A for $2. A private row
/// reports its ciphertext length as `chars`, which overstates its text by the AEAD tag: the
/// request it joins packs a little smaller than it could.
const NEXT_A: &str = "\
SELECT id, sensitivity, content,
       coalesce(length(content), octet_length(content_ct), 0)::int8 AS chars
  FROM memory
 WHERE tenant_id = $1
   AND sensitivity <> 'sealed'
   AND (content IS NOT NULL OR embedding IS NOT NULL OR embedding_b IS NOT NULL)
   AND (embedding IS NULL OR embedding_model IS DISTINCT FROM $2)
   AND ($3::uuid IS NULL OR id > $3)
 ORDER BY id LIMIT $4";

const NEXT_B: &str = "\
SELECT id, sensitivity, content,
       coalesce(length(content), octet_length(content_ct), 0)::int8 AS chars
  FROM memory
 WHERE tenant_id = $1
   AND sensitivity <> 'sealed'
   AND (content IS NOT NULL OR embedding_b IS NOT NULL OR embedding IS NOT NULL)
   AND (embedding_b IS NULL OR embedding_b_model IS DISTINCT FROM $2)
   AND ($3::uuid IS NULL OR id > $3)
 ORDER BY id LIMIT $4";

/// ($1 unit, $2 after or NULL, $3 limit): eligible rows with no vector in slot A. The `steady` fill
/// reads these, so a row whose vector carries another model's id is counted in `foreign_model` and
/// never re-embedded.
const NEXT_HOLES_A: &str = "\
SELECT id, sensitivity, content,
       coalesce(length(content), octet_length(content_ct), 0)::int8 AS chars
  FROM memory
 WHERE tenant_id = $1
   AND sensitivity <> 'sealed'
   AND (content IS NOT NULL OR embedding IS NOT NULL OR embedding_b IS NOT NULL)
   AND embedding IS NULL
   AND ($2::uuid IS NULL OR id > $2)
 ORDER BY id LIMIT $3";

const NEXT_HOLES_B: &str = "\
SELECT id, sensitivity, content,
       coalesce(length(content), octet_length(content_ct), 0)::int8 AS chars
  FROM memory
 WHERE tenant_id = $1
   AND sensitivity <> 'sealed'
   AND (content IS NOT NULL OR embedding_b IS NOT NULL OR embedding IS NOT NULL)
   AND embedding_b IS NULL
   AND ($2::uuid IS NULL OR id > $2)
 ORDER BY id LIMIT $3";

/// ($1 unit, $2 id, $3 vector, $4 model). The eligibility rule repeats here: a revocation that
/// committed after NEXT_A read the row leaves content NULL and both vectors NULL, and this store
/// must land nothing on it. The EXISTS is review M1's guard: a pass that read state before a retire
/// or a rename stores nothing, so no slot ever holds vectors of a model its state row does not name.
const STORE_A: &str = "\
UPDATE memory SET embedding = $3, embedding_model = $4
 WHERE tenant_id = $1 AND id = $2
   AND sensitivity <> 'sealed'
   AND (content IS NOT NULL OR embedding IS NOT NULL OR embedding_b IS NOT NULL)
   AND (embedding IS NULL OR embedding_model IS DISTINCT FROM $4)
   AND EXISTS (SELECT 1 FROM embedding_state s WHERE s.tenant_id = $1 AND s.model_a = $4)";

const STORE_B: &str = "\
UPDATE memory SET embedding_b = $3, embedding_b_model = $4
 WHERE tenant_id = $1 AND id = $2
   AND sensitivity <> 'sealed'
   AND (content IS NOT NULL OR embedding_b IS NOT NULL OR embedding IS NOT NULL)
   AND (embedding_b IS NULL OR embedding_b_model IS DISTINCT FROM $4)
   AND EXISTS (SELECT 1 FROM embedding_state s WHERE s.tenant_id = $1 AND s.model_b = $4)";

// ── flip ────────────────────────────────────────────────────────────────────────────────────────

/// Two seconds bounds how long the flip can hold the state row while it queues behind the conflict
/// key; a write never waits on that row, so only the sweep pays for it.
const FLIP_LOCK_TIMEOUT: &str = "SET LOCAL lock_timeout = '2s'";

const FLIP_LOCK_STATE: &str = "\
SELECT active_slot, model_a, model_b FROM embedding_state WHERE tenant_id = $1 FOR UPDATE";

/// The command never locks a state row, so this second lock cannot close a cycle with it.
const FLIP_GENERATION: &str = "SELECT generation FROM embedding_control WHERE singleton FOR SHARE";

/// ($1 unit, $2 target model). Runs under the transaction's snapshot, so an insert that commits
/// after it and missed its second vector lands with a hole the `flipped` phase fills next pass.
const FLIP_PENDING_A: &str = "\
SELECT count(*) FROM memory
 WHERE tenant_id = $1 AND sensitivity <> 'sealed'
   AND (content IS NOT NULL OR embedding IS NOT NULL OR embedding_b IS NOT NULL)
   AND (embedding IS NULL OR embedding_model IS DISTINCT FROM $2)";

const FLIP_PENDING_B: &str = "\
SELECT count(*) FROM memory
 WHERE tenant_id = $1 AND sensitivity <> 'sealed'
   AND (content IS NOT NULL OR embedding_b IS NOT NULL OR embedding IS NOT NULL)
   AND (embedding_b IS NULL OR embedding_b_model IS DISTINCT FROM $2)";

/// ($1 unit, $2 target slot).
const FLIP_UPDATE: &str = "\
UPDATE embedding_state SET active_slot = $2, flipped_at = now(), updated_at = now()
 WHERE tenant_id = $1";

/// Exclusive, so it waits out conflict scans in flight, as `memory_conflict_forget_row` does.
const FLIP_CONFLICT_KEY: &str =
    "SELECT pg_advisory_xact_lock(hashtextextended('memory_conflict:' || $1, 0))";
const FLIP_CLEAR_PAIRS: &str = "DELETE FROM memory_conflict WHERE tenant_id = $1";
const FLIP_CLEAR_SCANS: &str = "DELETE FROM memory_conflict_scan WHERE tenant_id = $1";
/// ($1 channel, $2 unit). The channel binds `CONFLICT_WAKE_CHANNEL`, the name the sweeper listens on.
const FLIP_NOTIFY: &str = "SELECT pg_notify($1, $2)";

// ── retire ──────────────────────────────────────────────────────────────────────────────────────

/// ($1 unit, $2 model, $3 limit). The EXISTS on `embedding_state` matches only while slot A is
/// inactive, so a batch never clears the slot the unit reads, whatever state a pass read before
/// it ran.
const RETIRE_A: &str = "\
UPDATE memory SET embedding = NULL, embedding_model = NULL
 WHERE tenant_id = $1
   AND EXISTS (SELECT 1 FROM embedding_state s WHERE s.tenant_id = $1 AND s.active_slot = 'b')
   AND id IN (SELECT id FROM memory WHERE tenant_id = $1 AND embedding_model = $2
               ORDER BY id LIMIT $3)";

const RETIRE_B: &str = "\
UPDATE memory SET embedding_b = NULL, embedding_b_model = NULL
 WHERE tenant_id = $1
   AND EXISTS (SELECT 1 FROM embedding_state s WHERE s.tenant_id = $1 AND s.active_slot = 'a')
   AND id IN (SELECT id FROM memory WHERE tenant_id = $1 AND embedding_b_model = $2
               ORDER BY id LIMIT $3)";

/// ($1 unit, $2 model). Matches only when no row still carries the model, so a write that landed
/// a vector of it after the last batch keeps the slot named.
const CLEAR_SLOT_A: &str = "\
UPDATE embedding_state SET model_a = NULL, updated_at = now()
 WHERE tenant_id = $1 AND model_a = $2 AND active_slot = 'b'
   AND NOT EXISTS (SELECT 1 FROM memory WHERE tenant_id = $1 AND embedding_model = $2)";

const CLEAR_SLOT_B: &str = "\
UPDATE embedding_state SET model_b = NULL, updated_at = now()
 WHERE tenant_id = $1 AND model_b = $2 AND active_slot = 'a'
   AND NOT EXISTS (SELECT 1 FROM memory WHERE tenant_id = $1 AND embedding_b_model = $2)";

// ── control ─────────────────────────────────────────────────────────────────────────────────────

const CONTROL_ENSURE: &str =
    "INSERT INTO embedding_control (singleton) VALUES (true) ON CONFLICT DO NOTHING";

macro_rules! control_read {
    () => {
        "\
SELECT generation, target, flip, retire, verb, requested_at,
       applied_generation, server_models, server_status, server_seen_at
  FROM embedding_control WHERE singleton"
    };
}

const CONTROL_READ: &str = control_read!();
const CONTROL_LOCK: &str = concat!(control_read!(), " FOR UPDATE");

/// ($1 target, $2 flip, $3 retire, $4 verb).
const CONTROL_WRITE: &str = "\
UPDATE embedding_control
   SET generation = generation + 1, target = $1, flip = $2, retire = $3, verb = $4,
       requested_at = now()
 WHERE singleton
RETURNING generation, target, flip, retire, verb, requested_at";

const CONTROL_GENERATION: &str = "SELECT generation FROM embedding_control WHERE singleton";

/// ($1 applied_generation, $2 server_models, $3 server_status).
const PUBLISH: &str = "\
UPDATE embedding_control
   SET applied_generation = $1, server_models = $2, server_status = $3, server_seen_at = now()
 WHERE singleton";

// ── slot choice ─────────────────────────────────────────────────────────────────────────────────

const fn name_slot_sql(slot: VectorSlot) -> &'static str {
    match slot {
        VectorSlot::A => NAME_SLOT_A,
        VectorSlot::B => NAME_SLOT_B,
    }
}

const fn counts_sql(active: VectorSlot) -> &'static str {
    match active {
        VectorSlot::A => COUNTS_ACTIVE_A,
        VectorSlot::B => COUNTS_ACTIVE_B,
    }
}

const fn next_sql(slot: VectorSlot) -> &'static str {
    match slot {
        VectorSlot::A => NEXT_A,
        VectorSlot::B => NEXT_B,
    }
}

const fn next_holes_sql(slot: VectorSlot) -> &'static str {
    match slot {
        VectorSlot::A => NEXT_HOLES_A,
        VectorSlot::B => NEXT_HOLES_B,
    }
}

const fn store_sql(slot: VectorSlot) -> &'static str {
    match slot {
        VectorSlot::A => STORE_A,
        VectorSlot::B => STORE_B,
    }
}

const fn flip_pending_sql(slot: VectorSlot) -> &'static str {
    match slot {
        VectorSlot::A => FLIP_PENDING_A,
        VectorSlot::B => FLIP_PENDING_B,
    }
}

const fn retire_sql(slot: VectorSlot) -> &'static str {
    match slot {
        VectorSlot::A => RETIRE_A,
        VectorSlot::B => RETIRE_B,
    }
}

const fn clear_slot_sql(slot: VectorSlot) -> &'static str {
    match slot {
        VectorSlot::A => CLEAR_SLOT_A,
        VectorSlot::B => CLEAR_SLOT_B,
    }
}

// ── pure mapping ────────────────────────────────────────────────────────────────────────────────

/// A CHECK constraint holds `active_slot` to 'a' or 'b', so any other value means it was dropped.
fn slot_from_db(unit: &str, stored: &str) -> Result<VectorSlot> {
    VectorSlot::parse(stored).ok_or_else(|| {
        DomainError::internal(format!(
            "embedding_state for unit {unit} holds active_slot '{stored}', which is neither 'a' \
             nor 'b'"
        ))
    })
}

fn unit_state(
    unit: String,
    active_slot: &str,
    model_a: Option<String>,
    model_b: Option<String>,
    flipped_at: Option<DateTime<Utc>>,
) -> Result<UnitState> {
    let active_slot = slot_from_db(&unit, active_slot)?;
    let state = UnitState { unit, active_slot, model_a, model_b, flipped_at };
    // `UnitState::active_model` panics on this shape, and embedding_state_active_named forbids it.
    if state.model(active_slot).is_none() {
        return Err(DomainError::internal(format!(
            "embedding_state for unit {} names no model in its active slot {}",
            state.unit,
            active_slot.as_str()
        )));
    }
    Ok(state)
}

/// The column's CHECK lists the four verbs; an unknown one means the constraint was dropped or a
/// newer server wrote it. Either way this process cannot act on it, so it says which.
fn verb_from_db(stored: Option<&str>) -> Result<Option<Verb>> {
    let Some(stored) = stored else { return Ok(None) };
    [Verb::Start, Verb::Flip, Verb::Rollback, Verb::Retire]
        .into_iter()
        .find(|v| v.as_str() == stored)
        .map(Some)
        .ok_or_else(|| {
            DomainError::internal(format!("embedding_control holds an unknown verb '{stored}'"))
        })
}

fn sensitivity_from_db(id: uuid::Uuid, stored: &str) -> Result<Sensitivity> {
    Sensitivity::parse(stored).ok_or_else(|| {
        DomainError::internal(format!("memory row {id} holds an unknown sensitivity '{stored}'"))
    })
}

/// One state row per unit from the per-column model counts: the majority id names each slot, a
/// tie going to the lower id so two boots seed the same row. A unit whose two majorities agree
/// keeps the model in slot A only, because `embedding_state_two_models` forbids naming it twice;
/// its slot B rows then count as pending for whatever model slot B is named next.
fn seed_plan(a: &[(String, String, i64)], b: &[(String, String, i64)]) -> Vec<UnitState> {
    use std::collections::BTreeMap;

    fn majorities(counts: &[(String, String, i64)]) -> BTreeMap<&str, (&str, i64)> {
        let mut best: BTreeMap<&str, (&str, i64)> = BTreeMap::new();
        for (unit, model, n) in counts {
            let wins = match best.get(unit.as_str()) {
                None => true,
                Some(&(held, held_n)) => *n > held_n || (*n == held_n && model.as_str() < held),
            };
            if wins {
                best.insert(unit, (model, *n));
            }
        }
        best
    }

    let a = majorities(a);
    let b = majorities(b);
    let mut units: Vec<&str> = a.keys().chain(b.keys()).copied().collect();
    units.sort_unstable();
    units.dedup();
    units
        .into_iter()
        .map(|unit| {
            let model_a = a.get(unit).map(|&(m, _)| m.to_string());
            let model_b =
                b.get(unit).map(|&(m, _)| m.to_string()).filter(|m| Some(m) != model_a.as_ref());
            let active_slot = if model_a.is_some() { VectorSlot::A } else { VectorSlot::B };
            UnitState { unit: unit.to_string(), active_slot, model_a, model_b, flipped_at: None }
        })
        .collect()
}

/// A request whose target slot is already active, or whose slot names another model, would flip a
/// unit onto vectors the pending count never examined. The caller built it wrong; say so.
fn check_flip_request(req: &FlipRequest) -> std::result::Result<(), String> {
    let unit = &req.expect.unit;
    if req.target_slot == req.expect.active_slot {
        return Err(format!(
            "flip for unit {unit} targets slot {}, which is already active",
            req.target_slot.as_str()
        ));
    }
    if req.expect.model(req.target_slot) != Some(req.target_model.as_str()) {
        return Err(format!(
            "flip for unit {unit} targets {} in slot {}, which holds {}",
            req.target_model,
            req.target_slot.as_str(),
            req.expect.model(req.target_slot).unwrap_or("no model")
        ));
    }
    Ok(())
}

/// `Stale` unless the locked row is the one the pass read. A missing row is stale too: the pass
/// read one.
fn stale_state(
    expect: &UnitState,
    locked: Option<(VectorSlot, Option<String>, Option<String>)>,
) -> bool {
    match locked {
        Some((slot, a, b)) => {
            slot != expect.active_slot || a != expect.model_a || b != expect.model_b
        }
        None => true,
    }
}

/// `Stale` when the pass read a generation and the control row now holds another, or is gone.
fn stale_generation(expect: Option<i64>, locked: Option<i64>) -> bool {
    expect.is_some() && expect != locked
}

/// SQLSTATE 55P03, `lock_not_available`, is what `lock_timeout` raises.
fn is_lock_timeout(code: Option<&str>) -> bool {
    code == Some("55P03")
}

/// Any statement of the flip that gives up on a lock becomes `LockTimeout`; the transaction rolled
/// back with it, so nothing was written and the next pass retries.
fn flip_error(e: sqlx::Error) -> Result<FlipOutcome> {
    let code = e.as_database_error().and_then(|d| d.code());
    if is_lock_timeout(code.as_deref()) {
        return Ok(FlipOutcome::LockTimeout);
    }
    Err(DomainError::from(e))
}

// ── row readers ─────────────────────────────────────────────────────────────────────────────────

fn state_from_row(row: &PgRow) -> Result<UnitState> {
    unit_state(
        row.try_get("tenant_id")?,
        row.try_get::<&str, _>("active_slot")?,
        row.try_get("model_a")?,
        row.try_get("model_b")?,
        row.try_get("flipped_at")?,
    )
}

fn intent_from_row(row: &PgRow) -> Result<Intent> {
    Ok(Intent {
        generation: row.try_get("generation")?,
        target: row.try_get("target")?,
        flip: row.try_get("flip")?,
        retire: row.try_get("retire")?,
        verb: verb_from_db(row.try_get::<Option<&str>, _>("verb")?)?,
        requested_at: row.try_get("requested_at")?,
    })
}

fn published_from_row(row: &PgRow) -> Result<Published> {
    Ok(Published {
        applied_generation: row.try_get("applied_generation")?,
        server_models: row.try_get("server_models")?,
        status: row.try_get("server_status")?,
        seen_at: row.try_get("server_seen_at")?,
    })
}

fn pending_from_row(row: &PgRow) -> Result<PendingRow> {
    let id: uuid::Uuid = row.try_get("id")?;
    Ok(PendingRow {
        id,
        sensitivity: sensitivity_from_db(id, row.try_get::<&str, _>("sensitivity")?)?,
        content: row.try_get("content")?,
        chars: row.try_get("chars")?,
    })
}

fn model_counts(rows: &[PgRow]) -> Result<Vec<(String, String, i64)>> {
    rows.iter()
        .map(|r| Ok((r.try_get("tenant_id")?, r.try_get("model")?, r.try_get("rows")?)))
        .collect()
}

#[async_trait]
impl EmbeddingMigrationRepository for PgEmbeddingMigrationRepository {
    async fn seed(&self) -> Result<Vec<UnitState>> {
        let a = sqlx::query(SEED_COUNTS_A).fetch_all(&self.pool).await?;
        let b = sqlx::query(SEED_COUNTS_B).fetch_all(&self.pool).await?;
        let mut created = Vec::new();
        for state in seed_plan(&model_counts(&a)?, &model_counts(&b)?) {
            let done = sqlx::query(SEED_INSERT)
                .bind(&state.unit)
                .bind(state.active_slot.as_str())
                .bind(state.model_a.as_deref())
                .bind(state.model_b.as_deref())
                .execute(&self.pool)
                .await?;
            if done.rows_affected() == 1 {
                created.push(state);
            }
        }
        Ok(created)
    }

    async fn states(&self) -> Result<Vec<UnitState>> {
        let rows = sqlx::query(STATES).fetch_all(&self.pool).await?;
        rows.iter().map(state_from_row).collect()
    }

    async fn state(&self, unit: &str) -> Result<Option<UnitState>> {
        let row = sqlx::query(STATE).bind(unit).fetch_optional(&self.pool).await?;
        row.as_ref().map(state_from_row).transpose()
    }

    async fn name_slot(&self, unit: &str, slot: VectorSlot, model: &str) -> Result<bool> {
        let done =
            sqlx::query(name_slot_sql(slot)).bind(unit).bind(model).execute(&self.pool).await?;
        Ok(done.rows_affected() == 1)
    }

    async fn counts(
        &self,
        unit: &str,
        state: &UnitState,
        target: Option<&str>,
        retire: Option<&str>,
    ) -> Result<Counts> {
        let row = sqlx::query(counts_sql(state.active_slot))
            .bind(unit)
            .bind(state.active_model())
            .bind(target)
            .bind(retire)
            .fetch_one(&self.pool)
            .await?;
        Ok(Counts {
            eligible: row.try_get("eligible")?,
            other_pending: row.try_get("other_pending")?,
            active_holes: row.try_get("active_holes")?,
            foreign_model: row.try_get("foreign_model")?,
            failed: 0,
            without_vector: row.try_get("without_vector")?,
            retire_pending: row.try_get("retire_pending")?,
        })
    }

    async fn next_batch(
        &self,
        unit: &str,
        slot: VectorSlot,
        model: &str,
        after: Option<uuid::Uuid>,
        limit: i64,
    ) -> Result<Vec<PendingRow>> {
        let rows = sqlx::query(next_sql(slot))
            .bind(unit)
            .bind(model)
            .bind(after)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(pending_from_row).collect()
    }

    async fn next_holes(
        &self,
        unit: &str,
        slot: VectorSlot,
        after: Option<uuid::Uuid>,
        limit: i64,
    ) -> Result<Vec<PendingRow>> {
        let rows = sqlx::query(next_holes_sql(slot))
            .bind(unit)
            .bind(after)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(pending_from_row).collect()
    }

    async fn store(
        &self,
        unit: &str,
        id: uuid::Uuid,
        slot: VectorSlot,
        model: &str,
        vector: Vec<f32>,
    ) -> Result<bool> {
        let done = sqlx::query(store_sql(slot))
            .bind(unit)
            .bind(id)
            .bind(pgvector::Vector::from(vector))
            .bind(model)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn flip(&self, req: &FlipRequest) -> Result<FlipOutcome> {
        check_flip_request(req).map_err(DomainError::internal)?;
        let started = Instant::now();
        match self.flip_tx(req, started).await {
            Ok(outcome) => Ok(outcome),
            Err(e) => flip_error(e),
        }
    }

    async fn retire_batch(
        &self,
        unit: &str,
        slot: VectorSlot,
        model: &str,
        limit: i64,
    ) -> Result<u64> {
        let done = sqlx::query(retire_sql(slot))
            .bind(unit)
            .bind(model)
            .bind(limit)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }

    async fn clear_slot(&self, unit: &str, slot: VectorSlot, model: &str) -> Result<bool> {
        let done =
            sqlx::query(clear_slot_sql(slot)).bind(unit).bind(model).execute(&self.pool).await?;
        Ok(done.rows_affected() == 1)
    }

    async fn control(&self) -> Result<(Intent, Published)> {
        let row = sqlx::query(CONTROL_READ).fetch_optional(&self.pool).await?;
        match row {
            Some(row) => Ok((intent_from_row(&row)?, published_from_row(&row)?)),
            None => Ok((Intent::default(), Published::default())),
        }
    }

    async fn generation(&self) -> Result<i64> {
        let g: Option<i64> =
            sqlx::query_scalar(CONTROL_GENERATION).fetch_optional(&self.pool).await?;
        // control() answers the defaults for a missing row, so this answers their generation.
        Ok(g.unwrap_or_default())
    }

    async fn change_intent(
        &self,
        // Spelled higher-ranked: async_trait names every elided lifetime in the signature, and left
        // elided these two would outlive the locals `decide` is called with.
        decide: &(dyn for<'i, 's> Fn(
            &'i Intent,
            &'s [UnitState],
        ) -> std::result::Result<Option<IntentChange>, String>
              + Send
              + Sync),
    ) -> Result<ChangeOutcome> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(CONTROL_ENSURE).execute(&mut *tx).await?;
        let locked = sqlx::query(CONTROL_LOCK).fetch_one(&mut *tx).await?;
        let intent = intent_from_row(&locked)?;
        // Read after the lock (review M7): a command that waited on another's commit decides on the
        // states that commit left, which the sweep may also have moved meanwhile.
        let states = sqlx::query(STATES)
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(state_from_row)
            .collect::<Result<Vec<_>>>()?;

        let change = match decide(&intent, &states) {
            Ok(Some(change)) => change,
            Ok(None) => {
                tx.rollback().await?;
                return Ok(ChangeOutcome::NoOp(intent));
            }
            Err(reason) => {
                tx.rollback().await?;
                return Ok(ChangeOutcome::Refused(reason));
            }
        };

        let written = sqlx::query(CONTROL_WRITE)
            .bind(change.target.as_deref())
            .bind(change.flip)
            .bind(change.retire.as_deref())
            .bind(change.verb.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let intent = intent_from_row(&written)?;
        tx.commit().await?;
        Ok(ChangeOutcome::Written(intent))
    }

    async fn publish(&self, published: &Published) -> Result<()> {
        sqlx::query(PUBLISH)
            .bind(published.applied_generation)
            .bind(&published.server_models)
            .bind(published.status.clone())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

impl PgEmbeddingMigrationRepository {
    /// The flip's transaction in the spec's order. A refusal rolls back and writes nothing; any
    /// error drops `tx`, which rolls back too.
    async fn flip_tx(&self, req: &FlipRequest, started: Instant) -> sqlx::Result<FlipOutcome> {
        let unit = req.expect.unit.as_str();
        let mut tx = self.pool.begin().await?;
        sqlx::query(FLIP_LOCK_TIMEOUT).execute(&mut *tx).await?;

        let locked = sqlx::query(FLIP_LOCK_STATE).bind(unit).fetch_optional(&mut *tx).await?;
        let locked = match locked {
            Some(row) => {
                let stored: &str = row.try_get("active_slot")?;
                // An unknown slot cannot equal the expected one, so it reads as stale here and
                // `states()` names it as an error on the next pass.
                match VectorSlot::parse(stored) {
                    Some(slot) => Some((slot, row.try_get("model_a")?, row.try_get("model_b")?)),
                    None => None,
                }
            }
            None => None,
        };
        if stale_state(&req.expect, locked) {
            tx.rollback().await?;
            return Ok(FlipOutcome::Stale);
        }

        if req.expect_generation.is_some() {
            let g: Option<i64> =
                sqlx::query_scalar(FLIP_GENERATION).fetch_optional(&mut *tx).await?;
            if stale_generation(req.expect_generation, g) {
                tx.rollback().await?;
                return Ok(FlipOutcome::Stale);
            }
        }

        let pending: i64 = sqlx::query_scalar(flip_pending_sql(req.target_slot))
            .bind(unit)
            .bind(&req.target_model)
            .fetch_one(&mut *tx)
            .await?;
        if pending > 0 {
            tx.rollback().await?;
            return Ok(FlipOutcome::Incomplete(pending));
        }

        sqlx::query(FLIP_UPDATE)
            .bind(unit)
            .bind(req.target_slot.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query(FLIP_CONFLICT_KEY).bind(unit).execute(&mut *tx).await?;
        sqlx::query(FLIP_CLEAR_PAIRS).bind(unit).execute(&mut *tx).await?;
        sqlx::query(FLIP_CLEAR_SCANS).bind(unit).execute(&mut *tx).await?;
        sqlx::query(FLIP_NOTIFY).bind(CONFLICT_WAKE_CHANNEL).bind(unit).execute(&mut *tx).await?;
        tx.commit().await?;

        let flip_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(FlipOutcome::Flipped { flip_ms })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exchanges each slot's names with the other's, token by token, so `embedding` never matches
    /// inside `embedding_b` and `'a'` never matches inside a longer literal.
    fn exchange(sql: &str) -> String {
        let mut out = String::with_capacity(sql.len());
        let bytes: Vec<char> = sql.chars().collect();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii_alphanumeric() || c == '_' {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '_') {
                    i += 1;
                }
                let word: String = bytes[start..i].iter().collect();
                out.push_str(match word.as_str() {
                    "embedding" => "embedding_b",
                    "embedding_b" => "embedding",
                    "embedding_model" => "embedding_b_model",
                    "embedding_b_model" => "embedding_model",
                    "model_a" => "model_b",
                    "model_b" => "model_a",
                    other => other,
                });
            } else if c == '\'' {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != '\'' {
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                let literal: String = bytes[start..i].iter().collect();
                out.push_str(match literal.as_str() {
                    "'a'" => "'b'",
                    "'b'" => "'a'",
                    other => other,
                });
            } else {
                out.push(c);
                i += 1;
            }
        }
        out
    }

    const TWINS: &[(&str, &str, &str)] = &[
        ("SEED_COUNTS", SEED_COUNTS_A, SEED_COUNTS_B),
        ("NAME_SLOT", NAME_SLOT_A, NAME_SLOT_B),
        ("COUNTS_ACTIVE", COUNTS_ACTIVE_A, COUNTS_ACTIVE_B),
        ("NEXT", NEXT_A, NEXT_B),
        ("NEXT_HOLES", NEXT_HOLES_A, NEXT_HOLES_B),
        ("STORE", STORE_A, STORE_B),
        ("FLIP_PENDING", FLIP_PENDING_A, FLIP_PENDING_B),
        ("RETIRE", RETIRE_A, RETIRE_B),
        ("CLEAR_SLOT", CLEAR_SLOT_A, CLEAR_SLOT_B),
    ];

    #[test]
    fn every_slot_b_statement_differs_from_its_a_twin_only_in_the_slot() {
        for (name, a, b) in TWINS {
            assert_ne!(a, b, "{name}: the twins are identical, so one names the wrong slot");
            assert_eq!(&exchange(a), b, "{name}_B is not {name}_A with the slots exchanged");
        }
    }

    #[test]
    fn the_exchange_leaves_longer_names_and_literals_alone() {
        assert_eq!(
            exchange("embedding_state.model_a 'sealed' embedding_b_hnsw 'a'"),
            "embedding_state.model_b 'sealed' embedding_b_hnsw 'b'"
        );
    }

    #[test]
    fn each_slot_picks_its_own_twin() {
        for slot in [VectorSlot::A, VectorSlot::B] {
            let col = slot.column();
            let pending = format!("({col} IS NULL OR {col}_model IS DISTINCT FROM $");
            assert!(next_sql(slot).contains(&pending), "next for {slot:?}");
            // The steady fill reads holes only: a foreign id in the slot never qualifies.
            assert!(next_holes_sql(slot).contains(&format!("AND {col} IS NULL\n")));
            assert!(!next_holes_sql(slot).contains("IS DISTINCT FROM"), "holes for {slot:?}");
            assert!(store_sql(slot).contains(&pending), "store for {slot:?}");
            assert!(flip_pending_sql(slot).contains(&pending), "flip_pending for {slot:?}");
            assert!(store_sql(slot).contains(&format!("SET {col} = $3, {col}_model = $4")));
            assert!(retire_sql(slot).contains(&format!("SET {col} = NULL, {col}_model = NULL")));
            assert!(retire_sql(slot).contains(&format!("AND {col}_model = $2")));
            let model_col = format!("model_{}", slot.as_str());
            assert!(name_slot_sql(slot).contains(&format!("SET {model_col} = $2")));
            assert!(clear_slot_sql(slot).contains(&format!("SET {model_col} = NULL")));
            assert!(
                name_slot_sql(slot).contains(&format!("active_slot = '{}'", slot.other().as_str()))
            );
            assert!(clear_slot_sql(slot)
                .contains(&format!("active_slot = '{}'", slot.other().as_str())));
        }
        // counts picks by the active slot: its holes are in that slot.
        assert!(counts_sql(VectorSlot::A).contains("AND embedding IS NULL) AS active_holes"));
        assert!(counts_sql(VectorSlot::B).contains("AND embedding_b IS NULL) AS active_holes"));
    }

    #[test]
    fn the_control_lock_is_the_read_with_a_row_lock() {
        assert_eq!(CONTROL_LOCK, format!("{CONTROL_READ} FOR UPDATE"));
    }

    #[test]
    fn a_store_requires_the_state_row_to_name_the_model_for_its_slot() {
        assert!(STORE_A.contains("s.model_a = $4"));
        assert!(STORE_B.contains("s.model_b = $4"));
    }

    fn state(slot: VectorSlot, a: Option<&str>, b: Option<&str>) -> UnitState {
        UnitState {
            unit: "me".into(),
            active_slot: slot,
            model_a: a.map(Into::into),
            model_b: b.map(Into::into),
            flipped_at: None,
        }
    }

    #[test]
    fn a_state_row_maps_its_slot() {
        let s = unit_state("me".into(), "b", Some("x".into()), Some("y".into()), None).unwrap();
        assert_eq!(s, state(VectorSlot::B, Some("x"), Some("y")));
        let s = unit_state("me".into(), "a", Some("x".into()), None, None).unwrap();
        assert_eq!(s.active_slot, VectorSlot::A);
    }

    #[test]
    fn an_unknown_stored_slot_is_an_internal_error_naming_it() {
        let e = unit_state("me".into(), "c", Some("x".into()), None, None).unwrap_err();
        assert_eq!(e.kind, crate::domain::errors::Kind::Internal);
        assert!(e.log_message().contains("'c'"), "{}", e.log_message());
        assert!(e.log_message().contains("me"), "{}", e.log_message());
    }

    #[test]
    fn an_active_slot_with_no_model_is_an_internal_error() {
        // The CHECK constraint forbids it, and UnitState::active_model would panic on it.
        let e = unit_state("me".into(), "b", Some("x".into()), None, None).unwrap_err();
        assert_eq!(e.kind, crate::domain::errors::Kind::Internal);
    }

    #[test]
    fn every_stored_verb_maps_back_to_its_verb() {
        for verb in [Verb::Start, Verb::Flip, Verb::Rollback, Verb::Retire] {
            assert_eq!(verb_from_db(Some(verb.as_str())).unwrap(), Some(verb));
        }
        assert_eq!(verb_from_db(None).unwrap(), None);
    }

    #[test]
    fn an_unknown_stored_verb_is_an_internal_error_naming_it() {
        let e = verb_from_db(Some("cancel")).unwrap_err();
        assert_eq!(e.kind, crate::domain::errors::Kind::Internal);
        assert!(e.log_message().contains("cancel"), "{}", e.log_message());
    }

    #[test]
    fn a_stored_sensitivity_maps_and_an_unknown_one_errs() {
        let id = uuid::Uuid::nil();
        assert_eq!(sensitivity_from_db(id, "private").unwrap(), Sensitivity::Private);
        let e = sensitivity_from_db(id, "secret").unwrap_err();
        assert!(e.log_message().contains("secret"), "{}", e.log_message());
    }

    fn c(unit: &str, model: &str, n: i64) -> (String, String, i64) {
        (unit.into(), model.into(), n)
    }

    #[test]
    fn seed_names_each_slot_with_its_majority_model() {
        let plan = seed_plan(
            &[c("me", "bge", 90), c("me", "old", 10), c("t2", "bge", 5)],
            &[c("me", "gemma", 3), c("me", "other", 1)],
        );
        assert_eq!(
            plan,
            vec![
                state(VectorSlot::A, Some("bge"), Some("gemma")),
                UnitState { unit: "t2".into(), ..state(VectorSlot::A, Some("bge"), None) },
            ]
        );
    }

    #[test]
    fn seed_activates_slot_b_when_slot_a_holds_nothing() {
        let plan = seed_plan(&[], &[c("me", "gemma", 3)]);
        assert_eq!(plan, vec![state(VectorSlot::B, None, Some("gemma"))]);
    }

    #[test]
    fn seed_breaks_a_tie_toward_the_lower_id() {
        let one = seed_plan(&[c("me", "y", 5), c("me", "x", 5)], &[]);
        let two = seed_plan(&[c("me", "x", 5), c("me", "y", 5)], &[]);
        assert_eq!(one, two);
        assert_eq!(one[0].model_a.as_deref(), Some("x"));
    }

    #[test]
    fn seed_never_names_one_model_in_both_slots() {
        let plan = seed_plan(&[c("me", "bge", 9)], &[c("me", "bge", 4)]);
        assert_eq!(plan, vec![state(VectorSlot::A, Some("bge"), None)]);
    }

    fn request(expect: UnitState, slot: VectorSlot, model: &str) -> FlipRequest {
        FlipRequest {
            expect,
            target_slot: slot,
            target_model: model.into(),
            expect_generation: None,
        }
    }

    #[test]
    fn a_flip_request_must_target_the_inactive_slot_holding_its_model() {
        let s = state(VectorSlot::A, Some("bge"), Some("gemma"));
        assert!(check_flip_request(&request(s.clone(), VectorSlot::B, "gemma")).is_ok());
        assert!(check_flip_request(&request(s.clone(), VectorSlot::A, "bge")).is_err());
        assert!(check_flip_request(&request(s.clone(), VectorSlot::B, "third")).is_err());
        let empty = state(VectorSlot::A, Some("bge"), None);
        assert!(check_flip_request(&request(empty, VectorSlot::B, "gemma")).is_err());
    }

    #[test]
    fn the_flip_is_stale_unless_the_locked_row_matches_what_the_pass_read() {
        let s = state(VectorSlot::A, Some("bge"), Some("gemma"));
        let same = Some((VectorSlot::A, Some("bge".to_string()), Some("gemma".to_string())));
        assert!(!stale_state(&s, same));
        assert!(stale_state(&s, None));
        assert!(stale_state(&s, Some((VectorSlot::B, Some("bge".into()), Some("gemma".into())))));
        assert!(stale_state(&s, Some((VectorSlot::A, Some("bge".into()), None))));
        assert!(stale_state(&s, Some((VectorSlot::A, Some("other".into()), Some("gemma".into())))));
    }

    #[test]
    fn the_flip_is_stale_when_the_generation_moved() {
        assert!(!stale_generation(Some(4), Some(4)));
        assert!(stale_generation(Some(4), Some(5)));
        assert!(stale_generation(Some(4), None));
        // Env mode reads no generation and the flip never asks.
        assert!(!stale_generation(None, None));
    }

    #[test]
    fn lock_not_available_is_the_only_lock_timeout() {
        assert!(is_lock_timeout(Some("55P03")));
        assert!(!is_lock_timeout(Some("40P01")));
        assert!(!is_lock_timeout(Some("57014")));
        assert!(!is_lock_timeout(None));
    }

    #[derive(Debug)]
    struct Coded(&'static str);

    impl std::fmt::Display for Coded {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "coded {}", self.0)
        }
    }

    impl std::error::Error for Coded {}

    impl sqlx::error::DatabaseError for Coded {
        fn message(&self) -> &str {
            "coded"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(self.0.into())
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    #[test]
    fn a_lock_timeout_in_the_flip_maps_to_its_outcome_and_anything_else_errs() {
        let timed_out = sqlx::Error::Database(Box::new(Coded("55P03")));
        assert_eq!(flip_error(timed_out).unwrap(), FlipOutcome::LockTimeout);
        let other = sqlx::Error::Database(Box::new(Coded("23514")));
        assert!(flip_error(other).is_err());
        assert!(flip_error(sqlx::Error::RowNotFound).is_err());
    }
}
