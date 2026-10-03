# Write-time conflict pairs: implementation plan

> **Draft.** Design only. Nothing is built yet, and this document changes as the build proceeds.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Conflict reads list and count stored pairs; a trigger on every insert wakes a sweeper that
records the row's pairs after it commits, and the write pays no scan.

**Architecture:** Two tables, three scan functions and three triggers on `memory` in one migration
(two invalidate and wake, one only wakes); one per-tenant advisory lock key shared by scans and the
invalidating triggers; two port methods; a listener adapter on `LISTEN memory_conflict`; a sweeper
loop that a timer and the listener both wake; a new body for `CONFLICTS_SQL`. The write path does
not change. Two stacked PRs: E1 records pairs and changes no read; E2 switches the read.

**Tech stack:** Rust, sqlx 0.9 without macros (`PgListener` from the `postgres` feature already on),
Postgres 16 with pgvector, plpgsql.

**Spec:** [`write-time-conflicts.md`](write-time-conflicts.md). Decision:
[0022](../decisions/0022-conflict-pairs-are-recorded-when-a-row-is-written.md).

## Global constraints

- The SQL in spec sections 4.1 to 4.5 and 4.9 is the contract. Paste it; do not paraphrase it.
- Only the sweeper calls `memory_conflict_record`. No trigger function on `memory` calls it or
  inserts into `memory_conflict`. `src/services/write.rs` does not change.
- One scan per statement. The sweep never runs two scans in one transaction.
- Scans take `hashtextextended('memory_conflict:' || tenant, 0)` shared; both invalidating trigger
  functions take it exclusive, two keys in `least` then `greatest` order. Any edit to those functions
  keeps the lock. `memory_conflict_record` keeps `SET lock_timeout = '200ms'`.
- Notification channel `memory_conflict`, payload the tenant id and nothing else.
- No `embedding IS NOT NULL` in the read. Liveness and the grant on both halves, inside the read.
- New settings live in `src/config.rs` and are validated at boot. No new dependency.
- `sqlx::query`/`query_as` with `.bind()`; statements as `const` so the statement-scan tests see them.
- Migrations forward-only. Never edit `20260922000025` or anything before it.
- Prose: no em dashes, no AI attribution anywhere, comments say why. Grep `\x{2014}` before handing back.
- Subagents run no git and no `cargo test`. `./scripts/cargo.sh check --all-targets` only, and expect
  errors in files other tasks own while they are in flight. The lead runs the suite.

## Review focus

1. Two writes into one namespace at the same moment: after one sweep both rows are marked and their
   pair is stored (E1-T4 test 1).
2. A bulk insert of 300 rows in one transaction: one notification for the tenant at commit, none
   before, and no scan or conflict lock inside the transaction (E1-T4 tests 2 and 17).
3. A row revived by `unexpire` after newer rows were written: the revive wakes the sweeper and the
   pair appears without a timer tick (E1-T4 test 4).
4. A reader whose grant covers one half of a stored pair at its level and not the other: the pair is
   absent and `conflicts_pending` counts only rows the reader may see (E2-T4 test 11).
5. The listener is gone: the next timer sweep still clears pending rows (E1-T4 test 18).

## Merge order

1. **PR E1**: the migration, the port additions, the adapter's two methods, the listener adapter,
   the sweeper, its wake set and loop, its config, tests. No reader changes and no write-path
   change. Mergeable on its own: reads stay on the self-join and the tables fill.
2. **PR E2**: the stored-pair read, `conflicts_pending` in the queue, MCP render and CLI, the
   `CONFLICT_SCAN_MAX` deprecation (read, ignored, one boot warning; removed the release after), the
   0018 supersession marker. Depends on E1.

## Task table

| id | purpose | files owned | model | depends on | output contract | gate |
|---|---|---|---|---|---|---|
| E1-L | lock interfaces | `migrations/<E1 date>000026_conflict_pairs.sql`, `src/ports/memory.rs`, `src/config.rs`, `src/authserver/routes.rs`, `src/adapters/auth/metadata.rs` | lead (opus) | none | the migration takes the date E1 is opened and keeps that name for good: once merged it is listed in downstream manifests and never renamed; contents = spec 4.1 to 4.5 verbatim; `ConflictSweep` and the two trait methods from spec 4.6 (no `record_conflicts`); `QualityConfig.conflict_sweep_secs: u64`, `conflict_sweep_budget_ms: u64` with env `CONFLICT_SWEEP_SECS` (60) and `CONFLICT_SWEEP_BUDGET_MS` (5000, refused below 100 or above 25000); the two full `QualityConfig` literals at `src/authserver/routes.rs:2209` and `src/adapters/auth/metadata.rs:197`, which carry no `..Default`, gain both fields | `./scripts/cargo.sh check --all-targets` fails only on the missing adapter methods; migration applies to an empty scratch database |
| E1-T1 | adapter methods | `src/adapters/postgres/memory.rs` | opus | E1-L | `RECORD_CONFLICTS_SQL`, `NEXT_CONFLICT_SCANS_SQL`, `CONFLICT_BACKLOG_SQL`, `CONFLICTS_PENDING_SQL` constants; `sweep_conflicts` as the per-row loop in spec 4.3 and `conflicts_pending`; unit tests that scan the constants | `cargo check`; constants pass the existing live-classification test |
| E1-T2 | listener adapter | `src/adapters/postgres/conflict_wake.rs` (new) | sonnet | E1-L | `CONFLICT_WAKE_CHANNEL` and `listen` exactly as below, on `sqlx::postgres::PgListener::connect_with(pool)`; `recv` in a loop inside the spawned task; an error from `recv` logs one warning and the loop goes on; wire-in note for `pub mod conflict_wake;` in `src/adapters/postgres/mod.rs` | `cargo check`; copied into a scratch crate and run against a scratch Postgres: a `NOTIFY memory_conflict, 't1'` reaches the callback |
| E1-T3 | sweeper service | `src/services/conflicts.rs` (new) | sonnet | E1-L | `SWEEP_BATCH`, `SweepReport`, `sweep`, `Wakes` and `run_loop` exactly as below; unit tests for `Wakes` (two wakes for one tenant before `wait` return it once; a wake with no waiter leaves a permit the next `wait` takes at once); wire-in note for `src/services/mod.rs` and `src/main.rs` | `cargo check`; `Wakes` tests run in a scratch crate; E1-T4 tests 5, 16, 18 and 19 |
| E1-T4 | integration tests | `tests/conflict_pairs.rs` (new) | opus | E1-T1, E1-T2, E1-T3 | tests 1 to 8 and 12 to 19 below | lead runs `./scripts/cargo.sh test -j 1 --test conflict_pairs`; every test fails once with its fix reverted |
| E1-W | wiring and suite | `src/main.rs`, `src/services/mod.rs`, `src/adapters/postgres/mod.rs` | lead | E1-T1 to E1-T4 | `spawn_conflict_sweep` as below, `pub mod conflicts;`, `pub mod conflict_wake;` | full suite `./scripts/cargo.sh test -j 1`, count reported |
| E1-T5 | docs | `docs/managing.md`, `docs/traps.md`, `docs/architecture.md` | sonnet | E1-W | the two settings in the settings table, with the line that `CONFLICT_SWEEP_SECS=0` stops all pair recording; a trap entry "a conflict scan inside a trigger on memory, or inside the inserting transaction, loses pairs" with spec section 6 as the evidence; a trap entry "a pooler in transaction mode drops conflict wakes"; `src/services/` list gains `conflicts`, `src/adapters/postgres/` gains `conflict_wake` | em dash grep empty |
| E1-G3 | scan cost on real data | none | lead with the owner | E1-W | `EXPLAIN (ANALYZE, BUFFERS) SELECT memory_conflict_record(...)` on a copy of a real store's largest namespace, p50 of 20 runs, plan text; the same for `memory_conflict_next(..., 50)` and `memory_conflict_backlog` | written into decision 0022 as measured |
| E2-L | lock read shape | `src/ports/memory.rs`, `src/config.rs` | lead (opus) | E1 merged | `live_embedded_counts` leaves the trait; `conflict_scan_max` stays in `QualityConfig` and is still parsed, its range check goes, and boot logs the deprecation warning in spec 4.8 once when `CONFLICT_SCAN_MAX` is set; the two test fixtures in `src/authserver/routes.rs` and `src/adapters/auth/metadata.rs` stay as they are; removal is the following release | `cargo check` fails only where E2-T1 and E2-T2 own the callers; a config unit test asserts the warning fires when the variable is set and not otherwise |
| E2-T1 | stored-pair read | `src/adapters/postgres/memory.rs` | opus | E2-L | `CONFLICTS_SQL` = spec 4.9 verbatim; `LIVE_EMBEDDED_COUNTS_SQL` and its method removed; `conflicts` doc comment rewritten | existing `CONFLICTS_SQL` scan tests still pass; `cargo check` |
| E2-T2 | queue service | `src/services/review_queue.rs`, `tests/review_queue.rs` | opus | E2-L | refusal and `codes::NAMESPACE_TOO_LARGE` removed; `Queue.conflicts_pending: i64`; render line; tests 1 and 2 below | lead runs `--test review_queue` and `--test review_queue_mcp` |
| E2-T3 | CLI | `crates/lumberroom/src/wire.rs`, `crates/lumberroom/src/review.rs`, `crates/lumberroom/tests/wire.rs`, `crates/lumberroom/tests/fixtures/review_queue.json` | sonnet | E2-L | `#[serde(default)] conflicts_pending: i64`; the printed line; fixture carries `"conflicts_pending": 3` and a test reads it, and a fixture without the field parses to 0 | lead runs `-p lumberroom` tests |
| E2-T4 | parity tests and records | `tests/conflict_pairs.rs`, `docs/decisions/0018-one-review-queue.md`, `docs/decisions/README.md`, `docs/managing.md`, `docs/specs/write-time-conflicts.md` | opus | E2-T1, E2-T2 | tests 9 to 11 below; 0018 status line names the superseded parts and links 0022; README rows for 0022 and 0018; docs mark `CONFLICT_SCAN_MAX` deprecated and ignored; spec status line updated with what ran | lead runs the full suite; em dash grep empty |
| E2-G2 | read cost on real data | none | lead with the owner | E2-T1 | `EXPLAIN (ANALYZE, BUFFERS)` of the new `CONFLICTS_SQL` on the same copy | written into decision 0022 as measured |

### Interfaces locked in E1-L

```rust
// src/ports/memory.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictSweep {
    pub scanned: i64,
    pub pending: i64,
}

// appended to trait MemoryRepository
async fn sweep_conflicts(&self, tenant: &str, floor: f64, limit: i64) -> Result<ConflictSweep>;
async fn conflicts_pending(
    &self,
    tenant: &str,
    floor: f64,
    reader: &[NamespaceGrant],
) -> Result<i64>;
```

```rust
// src/adapters/postgres/conflict_wake.rs, E1-T2
pub const CONFLICT_WAKE_CHANNEL: &str = "memory_conflict";

pub async fn listen<F>(
    pool: &sqlx::PgPool,
    on_wake: F,
) -> crate::domain::errors::Result<tokio::task::JoinHandle<()>>
where
    F: Fn(&str) + Send + Sync + 'static;
```

```rust
// src/services/conflicts.rs, E1-T3
pub const SWEEP_BATCH: i64 = 50;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub scanned: i64,
    pub pending: i64,
}

pub async fn sweep(
    repo: &dyn crate::ports::MemoryRepository,
    tenant: &str,
    floor: f64,
    budget: std::time::Duration,
) -> crate::domain::errors::Result<SweepReport>;

#[derive(Default)]
pub struct Wakes {
    tenants: std::sync::Mutex<std::collections::BTreeSet<String>>,
    notify: tokio::sync::Notify,
}

impl Wakes {
    pub fn wake(&self, tenant: &str);
    pub async fn wait(&self) -> Vec<String>;
}

pub async fn run_loop(
    repo: std::sync::Arc<dyn crate::ports::MemoryRepository>,
    tenant: String,
    floor: f64,
    budget: std::time::Duration,
    interval: std::time::Duration,
    wakes: std::sync::Arc<Wakes>,
);
```

`sweep` loops `sweep_conflicts(tenant, floor, SWEEP_BATCH)`, adds `scanned`, keeps the last
`pending`, and stops when `pending == 0`, a call returns `scanned == 0`, or `budget` has elapsed
since entry. `scanned` counts committed scans only, so a batch of rows that fail every time ends the
sweep instead of re-running the failing scans until the budget runs out.

`Wakes::wake` inserts the tenant under the mutex, drops the guard, then calls `notify_one`.
`Wakes::wait` awaits `notified()`, then takes the set with `std::mem::take` under the mutex and
returns it as a `Vec`. It never holds the mutex across an `.await`.

`run_loop` builds `tokio::time::interval(interval)`, skips the first tick, then loops on
`tokio::select!` over `tick.tick()` and `wakes.wait()`. A tick runs `sweep` for `tenant`; a wake runs
it when the returned list contains `tenant` and does nothing otherwise. It logs at info when
`scanned > 0` and at warn when `sweep` fails, and never returns.

`spawn_conflict_sweep` (E1-W), in `src/main.rs`:

```rust
fn spawn_conflict_sweep(
    cfg: Arc<config::Config>,
    repo: Arc<dyn ports::MemoryRepository>,
    pool: sqlx::PgPool,
);
```

With `conflict_sweep_secs == 0` it logs once at info that no conflict pairs will be recorded and
returns. Otherwise it builds `Arc<Wakes>`, spawns a task that awaits
`pg::conflict_wake::listen(&pool, move |t| wakes.wake(t))` (one warning on error, then nothing
more), and spawns `run_loop` with `cfg.tenant_id`, floor `cfg.quality.conflict_threshold`, budget
`Duration::from_millis(cfg.quality.conflict_sweep_budget_ms)` and interval
`Duration::from_secs(cfg.quality.conflict_sweep_secs)`.

### Statements E1-T1 writes

```rust
const RECORD_CONFLICTS_SQL: &str = "SELECT memory_conflict_record($1, $2, $3)";
const NEXT_CONFLICT_SCANS_SQL: &str = "SELECT id FROM memory_conflict_next($1, $2, $3::int4) AS id";
const CONFLICT_BACKLOG_SQL: &str = "SELECT memory_conflict_backlog($1, $2)";
const CONFLICTS_PENDING_SQL: &str = concat!(
    "SELECT count(*) FROM memory m
      WHERE m.tenant_id = $1
        AND ",
    live!(),
    "
        AND NOT EXISTS (SELECT 1 FROM memory_conflict_scan s
                         WHERE s.memory_id = m.id AND s.floor <= $2)
        AND EXISTS (
              SELECT 1
                FROM unnest($3::text[], $4::bool[], $5::text[]) AS g(prefix, exact, max)
               WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                          ELSE left(m.namespace, length(g.prefix)) = g.prefix END
                 AND sensitivity_rank(g.max) >= sensitivity_rank(m.sensitivity)
            )"
);
```

`conflicts_pending` returns 0 without a query when `reader` is empty, as `conflicts` does.

`sweep_conflicts` fetches ids with `NEXT_CONFLICT_SCANS_SQL`, runs `RECORD_CONFLICTS_SQL` once per id
on the pool (one autocommitted statement each, never inside a transaction), counts the calls that
returned without error as `scanned`, then reads `pending` with `CONFLICT_BACKLOG_SQL`. A record that
fails is logged with the row id, does not count, and the loop goes on to the next id.

### JSON shape E2 adds

`GET /admin/review/queue` and the MCP queue envelope gain one field beside `dismissed`:

```json
{ "items": [], "dismissed": 0, "conflicts_pending": 12, "has_more": false }
```

It is present whenever the conflict source ran, and 0 when nothing is pending.

## Tests

`tests/conflict_pairs.rs` keeps today's self-join as a test constant, `SELF_JOIN_SQL` (the body of
`CONFLICTS_SQL` at `src/adapters/postgres/memory.rs:1169-1214` before E2), and each event test ends
with `assert_parity(&pool, tenant, floor)`: the stored read and `SELF_JOIN_SQL` return the same
`(older_id, newer_id, similarity)` rows in the same order, after one `sweep` with a 10 s budget.

Tests that need the wake open their own `sqlx::postgres::PgListener` on `memory_conflict` before the
write and read with `tokio::time::timeout(Duration::from_secs(2), listener.recv())`. Tests 16 and 18
run `run_loop` in a spawned task and abort it at the end.

1. `two_concurrent_writes_in_one_namespace_both_find_their_pair`: 20 rounds of two `write::run`
   calls joined with `tokio::join!` on near-identical content (hash embedder, similarity above 0.90),
   then one `sweep`; every round's pair is stored and both rows are marked.
2. `an_insert_notifies_its_tenant_at_commit_and_not_on_rollback`: in one transaction insert 300 rows
   for tenant T; no notification arrives before commit; after commit exactly one notification with
   payload `T` arrives. A second transaction inserts one row and rolls back; no notification arrives
   within 500 ms.
3. `a_restored_row_is_pending_until_the_sweeper_scans_it`: `restore_row`, then
   `conflicts_pending` counts it, then a sweep clears it and parity holds.
4. `a_revived_row_wakes_and_pairs_with_rows_written_while_it_was_retired`: write A, expire A, write
   B similar to A, unexpire A; a notification for the tenant arrives; A is pending; after a sweep
   the pair is stored.
5. `a_sweep_stops_on_its_budget_and_resumes`: 300 unmarked rows, a 1 ms budget scans at least one
   batch and leaves pending above 0; a second sweep with 10 s reaches 0.
6. `lowering_the_threshold_turns_every_row_pending`: marks at 0.90, then `conflicts_pending` at 0.85
   counts every live row and a sweep at 0.85 adds the pairs between 0.85 and 0.90.
7. `a_changed_vector_drops_the_row_s_pairs_and_wakes`: `UPDATE memory SET embedding = ...` on one
   half; its pairs and mark are gone, the other row's pairs with third rows stay, and a notification
   arrives.
8. `forgetting_a_row_cascades_its_pairs_and_mark`: `forget::by_id`; no row in either table names it.
12. `a_vector_change_waits_for_a_scan_in_flight`: one connection opens a transaction, takes the
    shared advisory key and inserts a pair for row A as a scan would; a second connection updates A's
    vector and blocks; after the first commits, the second's trigger deletes the pair. No pair naming
    A survives.
13. `a_sweep_and_concurrent_writes_in_one_namespace_do_not_deadlock`: 200 unmarked similar rows, then
    a sweep runs while 20 concurrent `write::run` calls land in the same namespace; no
    `deadlock detected` error, and parity holds after a final sweep.
14. `a_revive_behind_a_locked_row_commits_while_a_scan_waits`: transaction T locks row D with
    `SELECT ... FOR UPDATE`; a second connection starts `memory_conflict_record` on a row similar to D,
    which blocks on D's row lock in its foreign-key check; T then revives a retired row and commits.
    Assert T commits, the scan fails with `lock_not_available` (55P03) within 1 s, the scanned row
    has no mark, and the next sweep marks it. Run it on both shapes: the engine delete path
    (`SELECT supersedes ... FOR UPDATE`, then revive) and a bare `FOR UPDATE` plus `unexpire`.
15. `no_trigger_on_memory_scans_or_writes_pairs`: for every non-internal trigger on `memory`
    (`pg_trigger` joined to `pg_proc` on `tgfoid`), `prosrc` does not contain
    `memory_conflict_record` and does not match `INSERT\s+INTO\s+(public\.)?memory_conflict\y`
    through `~*`. The match runs in SQL, and the Rust literal is a raw string. A Postgres regex reads
    `\b` as a backspace, not a word boundary, so the pattern ends in `\y`; with `\b` it matched no
    function on a scratch database (measured in review). As a self-check the same pattern over all
    of `pg_proc` must match `memory_conflict_record`. Fails naming the trigger. Its mutation gate is
    mandatory: a planted trigger on `memory` whose function inserts into `memory_conflict` makes it
    fail.
16. `a_wake_records_a_write_s_pair_without_a_tick`: `listen` feeding a `Wakes`, `run_loop` with a
    3,600 s interval; write A, then B similar to A; within 2 s the pair is stored.
17. `a_bulk_insert_runs_no_scan_inside_its_transaction`: insert 300 similar rows in one transaction;
    before commit, from a second connection, `memory_conflict` and `memory_conflict_scan` hold
    nothing for them and `pg_locks` shows no advisory lock held by the inserting backend.
18. `a_lost_wake_is_swept_on_the_next_tick`: `run_loop` with a 1 s interval and no listener; write
    two similar rows; within 3 s the pair is stored.
19. `a_row_that_fails_every_scan_ends_the_sweep_early`: transaction T locks row D with
    `SELECT ... FOR UPDATE` and stays open; row R, similar to D and oldest, and 10 rows similar to
    nothing are unmarked. One `sweep` with a 10 s budget returns within 2 s with `scanned == 10`;
    the 10 rows are marked and R is pending. After T commits, the next sweep marks R.
9. `parity_with_the_self_join_at_three_grants_and_two_floors` (E2-T4): a fixture of 60 rows over three
   namespaces at mixed levels; grants `*` at sealed, `project:*` at open, one namespace at private;
   floors 0.90 and 0.95.
10. `a_dismissed_pair_stays_out_of_the_stored_read` (E2-T4).
11. `pending_counts_only_rows_the_reader_may_see` (E2-T4): an unmarked private row is pending for a
    private reader and not for an open one.

`tests/review_queue.rs` (E2-T2):

1. `the_envelope_carries_conflicts_pending`.
2. `a_namespace_past_two_thousand_rows_gets_its_conflicts`: replaces the test at
   `tests/review_queue.rs:966-990` that asserted `namespace_too_large`.

## Self-review

Every spec section has a task: 4.1 to 4.5 E1-L, with tests 12 to 15 and 17 for the lock, the
per-row commit, the timeout and the trigger shape; 4.6 E1-L, E1-T1 and E1-T2; 4.7 no task, since the
write path does not change, and tests 1 and 17 pin it; 4.8 E1-T3 and E1-W with tests 16, 18 and 19; 4.9
E2-T1 to E2-T3; section 5 E1-T4 tests 2 to 8; section 6 tests 1 and 15; section 7 E1-G3 and E2-G2;
section 8 test 5; section 9 tests 9 to 11 and E2-T2 test 2; section 12 every gate.

Execution: subagent-driven. Most tasks write against interfaces E1-L locks, and a wrong scan is a
silent stale pair rather than a compile error, so each task gets its own reviewer before the next.
