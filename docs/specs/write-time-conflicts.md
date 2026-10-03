# Write-time conflict pairs

> **Draft.** PR E1 (the pair tables, triggers and sweeper) is built on branch `feat/write-time-conflicts`. Readers (PR E2) are not built yet, and this document changes as the build proceeds.

3 October 2026. Status: design, nothing built. Revised the same day: a trigger on `memory` now
wakes the sweeper, and the write path no longer calls the scan (section 11 records the shapes that
lost). Decision record:
[0022](../decisions/0022-conflict-pairs-are-recorded-when-a-row-is-written.md). Plan:
[`write-time-conflicts-plan.md`](write-time-conflicts-plan.md).

## 1. Why

The conflicts read is a self-join on vector distance. `CONFLICTS_SQL`
(`src/adapters/postgres/memory.rs:1169-1214`) joins `memory` to `memory` inside a namespace on
`(a.created_at, a.id) < (b.created_at, b.id)`, keeps pairs at or above `conflict_threshold` (0.90)
and sorts them by similarity. Its cost is O(n squared) in the largest namespace, and every read pays
it again.

Measured on 3 October 2026 on a real store of about 3,870 live memories whose largest namespace
held 1,691 rows, with `EXPLAIN ANALYZE` and the server's slow-statement log:

- one run took about 6.4 s and evaluated about 1.93 million pairs;
- about 74% of that time went to detoasting `vector(768)` values, which sit in external storage:
  the same join took 11.7 s run serially and 3.1 s when each vector was unpacked once;
- a per-row nearest-neighbour statement (`ORDER BY embedding <=> ... LIMIT 10` under `LATERAL`,
  filtered on tenant and namespace) did not use `memory_embedding_hnsw`. The planner took the
  btree, sorted the namespace once per row, and the statement took 48.8 s with 48.6 million buffer
  hits.

Decision 0018 measured the same curve on a dev container: 2.48 s at 1,400 rows in one namespace,
13.33 s at 3,000 and 34.96 s at 5,000. It bounded the read with `CONFLICT_SCAN_MAX` and refused past
it. That bound protects the server and leaves the largest stores with no conflict review at all.

The owner's direction, 3 October 2026: detect conflicts when a memory is written, store the pairs,
and have readers count and list from the stored pairs. The owner then asked for the detection to
start from a database trigger defined in the migration rather than from the write path, for write
performance.

## 2. What success means

1. A conflicts read joins stored pairs to their two rows and does no vector arithmetic. It is a
   bounded read, not necessarily an index read: on a scratch copy the planner chose a sequential
   scan of `memory` feeding a hash join (section 7), and gate E2-G2 records the plan it takes.
2. After a store's backfill, every reader returns exactly what `CONFLICTS_SQL` returns today for the
   same grant, threshold, limit and offset. Section 9 names each place the answer differs and why.
3. A write pays no scan. Its insert fires a trigger that queues one notification; the notification
   reaches the sweeper only when the write commits, and the sweeper scans the row then. Nothing the
   scan does can fail or slow the write.
4. No path in this repository can leave a stored pair that a reader returns wrongly. Section 5
   lists every event and the three ways outside the code that still can.
5. A reader always says how many readable rows are still waiting for their scan, so a short list is
   never presented as the whole answer.

## 3. What exists today

| Piece | Where | Relevance |
|---|---|---|
| Conflicts read | `src/adapters/postgres/memory.rs:1169-1214`, `:3014-3058` | the statement this design replaces |
| Port | `src/ports/memory.rs:785-792` | its doc comment argues against a stored table; this design reverses that argument |
| The only caller | `src/services/review_queue.rs:443-500` | runs `live_embedded_counts` first and refuses past `conflict_scan_max` |
| Write-time neighbours | `src/services/write.rs:449-469`, `src/adapters/postgres/memory.rs:2209-2246` | one HNSW-shaped query per write, `LIMIT conflict_limit` (3), under the writer's read ceiling, before the insert |
| The insert | `src/services/write.rs:327-358` | a pooled statement with no transaction around it, autocommitted |
| Dismissed-pair ledger | `migrations/20260922000025_review_dismissed_pairs.sql` | `(lo_id, hi_id)` in uuid order, `ON DELETE CASCADE` on both; the reader anti-joins it |
| Thresholds | `src/config.rs:916-919`, `:1096` | `DEDUPE_THRESHOLD` 0.97, `CONFLICT_THRESHOLD` 0.90, `CONFLICT_LIMIT` 3, `CONFLICT_SCAN_MAX` 2,000 |
| Background timer | `src/main.rs:347-389` | the hourly cleanup pass on `cfg.tenant_id`; the sweeper copies its shape |
| Pool session setup | `src/adapters/postgres/mod.rs:77` | every pooled connection runs `SET statement_timeout = '30s'` |
| Listener | `sqlx::postgres::PgListener`, in the `postgres` feature already enabled in `Cargo.toml:41` | no new dependency; it reconnects on its own and drops notifications sent while it was down |

The write-time neighbour query cannot record pairs as it stands. It runs before the insert, so the
new row has no id to pair with. It returns at most three rows. It filters on the writer's read
ceiling, so a write-only client would record nothing and a private row would never pair with a row
above the writer's level.

## 4. Design

Two tables, three scan functions, three triggers on `memory`, two port methods, one sweeper that a
timer and a database notification both wake, and a new body for the conflicts read. Nothing on the
write path changes.

Every scan and the two invalidating trigger functions share one per-tenant advisory lock key,
`hashtextextended('memory_conflict:' || tenant_id, 0)`: scans take it shared, the invalidating
triggers take it exclusive (4.4 says why). The wake trigger takes no lock.

### 4.1 Tables

```sql
-- One row per live pair at or above the floor it was scanned at. Older and newer by
-- (created_at, id), the order CONFLICTS_SQL reports them in. created_at never changes in place, so
-- the order a scan writes stays true for the life of both rows.
CREATE TABLE IF NOT EXISTS memory_conflict (
  tenant_id  text             NOT NULL,
  older_id   uuid             NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  newer_id   uuid             NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  similarity double precision NOT NULL,
  PRIMARY KEY (older_id, newer_id),
  CHECK (older_id <> newer_id)
);
-- The read: one tenant, most similar first.
CREATE INDEX IF NOT EXISTS memory_conflict_by_similarity
  ON memory_conflict (tenant_id, similarity DESC);
-- The cascade and the invalidation trigger probe newer_id alone; the primary key serves older_id.
CREATE INDEX IF NOT EXISTS memory_conflict_newer ON memory_conflict (newer_id);

-- One row per memory whose pairs are on record, and the floor they were found at. A live row with
-- no mark at or below the configured threshold is pending: the sweeper scans it, and readers count
-- it.
CREATE TABLE IF NOT EXISTS memory_conflict_scan (
  memory_id  uuid             PRIMARY KEY REFERENCES memory(id) ON DELETE CASCADE,
  tenant_id  text             NOT NULL,
  floor      double precision NOT NULL,
  scanned_at timestamptz      NOT NULL DEFAULT now()
);
```

No `namespace` column on either table. A pair lives inside one namespace by construction, the read
joins both rows anyway, and a copy of the namespace would go stale the day a row moved. The trigger
in 4.4 deletes a row's pairs when its namespace changes, and the read repeats `a.namespace =
b.namespace` as a guard that costs nothing.

The scan mark is a table of its own rather than a column on `memory`. A column would turn every
scan into an update of `memory`. An update that is not HOT writes an entry into every index on the
table, `memory_embedding_hnsw` included, and issue #72 found the store's `touch_accessed` updates
never HOT.

No queue table. A live row with no current mark is already the queue entry: `memory_conflict_next`
finds it. A queue row written by the insert trigger would add a write to every insert and a second
record of the same fact.

### 4.2 Recording one row's pairs

```sql
-- Every live pair p_id forms in its own namespace at or above p_floor, then the mark. Returns
-- nothing: a count of pairs that includes rows the caller cannot read is itself a disclosure.
--
-- No ORDER BY and no LIMIT: this must find every pair above the floor, which a top-k index scan
-- cannot promise. `near` is MATERIALIZED so each candidate's vector is detoasted once:
-- referenced in both the select list and the WHERE clause, it would be unpacked twice.
--
-- TRAP: only the sweeper calls this, as a statement of its own, after the anchor row committed.
-- Never call it from a trigger on memory or inside a transaction that inserted the anchor. Section
-- 6 of docs/specs/write-time-conflicts.md is the reason.
--
-- The shared advisory lock comes first and holds to commit. memory_conflict_forget_row and
-- memory_conflict_rescan_row take the same key exclusive, so a trigger that clears a row's pairs
-- waits for every scan in flight, and a scan that started on an old vector commits before the clear
-- rather than after it.
--
-- lock_timeout bounds every wait in here, the advisory lock and the row locks the pair insert's
-- foreign-key checks take on both memory rows. A transaction that holds a memory row FOR UPDATE and
-- then revives a row waits for this scan's shared key while this scan's FK check waits for its row
-- lock; the timeout ends the scan, the other transaction commits, and the anchor stays unmarked for
-- the next sweep.
CREATE OR REPLACE FUNCTION memory_conflict_record(
    p_tenant text, p_id uuid, p_floor double precision)
RETURNS void LANGUAGE plpgsql SET lock_timeout = '200ms' AS $$
BEGIN
  PERFORM pg_advisory_xact_lock_shared(hashtextextended('memory_conflict:' || p_tenant, 0));
  WITH anchor AS MATERIALIZED (
    SELECT id, namespace, created_at, embedding
      FROM memory
     WHERE tenant_id = p_tenant AND id = p_id AND embedding IS NOT NULL
       AND superseded_by IS NULL AND (occurred_until IS NULL OR occurred_until > now())
  ), near AS MATERIALIZED (
    SELECT m.id, m.created_at, a.id AS anchor_id, a.created_at AS anchor_created_at,
           (1 - (m.embedding <=> a.embedding))::float8 AS similarity
      FROM anchor a
      JOIN memory m
        ON m.tenant_id = p_tenant AND m.namespace = a.namespace AND m.id <> a.id
     WHERE m.superseded_by IS NULL
       AND (m.occurred_until IS NULL OR m.occurred_until > now())
       AND m.embedding IS NOT NULL
  )
  INSERT INTO memory_conflict (tenant_id, older_id, newer_id, similarity)
  SELECT p_tenant,
           CASE WHEN (n.created_at, n.id) < (n.anchor_created_at, n.anchor_id)
                THEN n.id ELSE n.anchor_id END,
           CASE WHEN (n.created_at, n.id) < (n.anchor_created_at, n.anchor_id)
                THEN n.anchor_id ELSE n.id END,
           n.similarity
    FROM near n
   WHERE n.similarity >= p_floor
  ON CONFLICT (older_id, newer_id) DO UPDATE SET similarity = EXCLUDED.similarity;

  -- Marked whether or not it was live or embedded: a mark means "scanned", and a row that becomes
  -- live again loses its mark through the trigger in 4.4.
  INSERT INTO memory_conflict_scan (memory_id, tenant_id, floor)
  SELECT id, tenant_id, p_floor FROM memory WHERE tenant_id = p_tenant AND id = p_id
  ON CONFLICT (memory_id) DO UPDATE SET floor = EXCLUDED.floor, scanned_at = now();
END
$$;
```

The scan does not filter on sensitivity or on any grant. A stored pair is a fact about two rows,
and whether a caller sees it is decided at read time against each half's stored level, inside the
read statement. No row reaches the process: the function returns nothing.

The similarity is the expression `CONFLICTS_SQL` computes, `(1 - (a.embedding <=> b.embedding))::float8`.
pgvector's cosine distance multiplies and sums the same terms in either argument order, so the stored
value should equal the value today's join computes for the same two vectors, and ties should order
the same way. That is a reading of the arithmetic, not an observation; gate E2-G1 settles it.

### 4.3 The sweep

```sql
-- Up to p_limit live rows with no current mark, oldest first. The caller records each one in its
-- own statement, so each scan commits on its own (see below).
CREATE OR REPLACE FUNCTION memory_conflict_next(
    p_tenant text, p_floor double precision, p_limit integer)
RETURNS SETOF uuid LANGUAGE sql STABLE AS $$
  SELECT m.id FROM memory m
   WHERE m.tenant_id = p_tenant
     AND m.superseded_by IS NULL
     AND (m.occurred_until IS NULL OR m.occurred_until > now())
     AND NOT EXISTS (SELECT 1 FROM memory_conflict_scan s
                      WHERE s.memory_id = m.id AND s.floor <= p_floor)
   ORDER BY m.created_at, m.id
   LIMIT p_limit
$$;

-- Every live row in the tenant with no current mark, whoever may read it. For background passes
-- only: section 4.6.
CREATE OR REPLACE FUNCTION memory_conflict_backlog(
    p_tenant text, p_floor double precision)
RETURNS bigint LANGUAGE sql STABLE AS $$
  SELECT count(*) FROM memory m
   WHERE m.tenant_id = p_tenant
     AND m.superseded_by IS NULL
     AND (m.occurred_until IS NULL OR m.occurred_until > now())
     AND NOT EXISTS (SELECT 1 FROM memory_conflict_scan s
                      WHERE s.memory_id = m.id AND s.floor <= p_floor)
$$;
```

The adapter's `sweep_conflicts` calls `memory_conflict_next`, then
`SELECT memory_conflict_record($1, $2, $3)` once per id as a pooled statement, then
`memory_conflict_backlog`. One commit per row is the point. A loop of 50 scans inside one function
call holds the tenant's shared advisory key and every pair key it inserted until the batch commits:
every re-embed, shred or revival in the tenant waits for the whole batch (554 ms for 50 rows on a
scratch copy), and one failing row rolls back the other 49. On a scratch copy a batch in one call
also deadlocked with a writer that held a pair key the batch needed. Two single-row scans cannot
deadlock each other: every pair `record(A)` inserts contains A and every pair `record(B)` inserts
contains B, so they share at most one key, the pair A and B, and a deadlock needs two keys taken in
opposite orders.

A procedure that commits per row would keep the ids inside the database, and lost because a
deployment that has to run the sweep as `SECURITY DEFINER` cannot: Postgres refuses transaction
control inside a definer procedure. The ids that reach the process belong to a background job that
returns them to no one.

A mark at a floor above the configured threshold counts as missing, so lowering
`CONFLICT_THRESHOLD` turns every row pending and the sweeper rescans the store. Raising it changes
nothing stored: the read filters on the threshold, and the pairs below it stay unread.

### 4.4 Two invalidating triggers on `memory`

```sql
-- A row whose vector, namespace, tenant or model changed: its pairs no longer describe it. The
-- exclusive lock waits out every scan in flight in the tenant (4.2), so none of them can insert a
-- pair computed on the old vector after this DELETE and leave it there. The notification wakes the
-- sweeper to rescan the row once this transaction commits.
CREATE OR REPLACE FUNCTION memory_conflict_forget_row() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  -- Both keys in one fixed order when the tenant changed, so two moves between the same two
  -- tenants cannot each hold one key and wait for the other.
  PERFORM pg_advisory_xact_lock(
    hashtextextended('memory_conflict:' || least(OLD.tenant_id, NEW.tenant_id), 0));
  IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id THEN
    PERFORM pg_advisory_xact_lock(
      hashtextextended('memory_conflict:' || greatest(OLD.tenant_id, NEW.tenant_id), 0));
  END IF;
  DELETE FROM memory_conflict WHERE older_id = NEW.id OR newer_id = NEW.id;
  DELETE FROM memory_conflict_scan WHERE memory_id = NEW.id;
  PERFORM pg_notify('memory_conflict', NEW.tenant_id);
  RETURN NULL;
END
$$;
CREATE TRIGGER memory_conflict_moved
  AFTER UPDATE OF tenant_id, namespace, embedding, embedding_model ON memory
  FOR EACH ROW
  WHEN (OLD.tenant_id IS DISTINCT FROM NEW.tenant_id
     OR OLD.namespace IS DISTINCT FROM NEW.namespace
     OR OLD.embedding IS DISTINCT FROM NEW.embedding
     OR OLD.embedding_model IS DISTINCT FROM NEW.embedding_model)
  EXECUTE FUNCTION memory_conflict_forget_row();

-- A row that was not live and is live again: rows written while it was retired never paired with
-- it. Its old pairs stay; they are still true. The lock keeps a scan of this row that began while
-- it was retired from writing its mark after this DELETE.
CREATE OR REPLACE FUNCTION memory_conflict_rescan_row() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  PERFORM pg_advisory_xact_lock(hashtextextended('memory_conflict:' || NEW.tenant_id, 0));
  DELETE FROM memory_conflict_scan WHERE memory_id = NEW.id;
  PERFORM pg_notify('memory_conflict', NEW.tenant_id);
  RETURN NULL;
END
$$;
CREATE TRIGGER memory_conflict_revived
  AFTER UPDATE OF superseded_by, occurred_until ON memory
  FOR EACH ROW
  WHEN ((OLD.superseded_by IS NOT NULL
         OR (OLD.occurred_until IS NOT NULL AND OLD.occurred_until <= now()))
    AND NEW.superseded_by IS NULL
    AND (NEW.occurred_until IS NULL OR NEW.occurred_until > now()))
  EXECUTE FUNCTION memory_conflict_rescan_row();
```

Two triggers rather than one, so a supersession never evaluates a vector comparison: `UPDATE OF`
fires only when the statement names one of the listed columns, and the `embedding` comparison sits
on the trigger that only a re-embed reaches. Retirement needs no trigger at all, because the read
tests both halves for liveness.

The exclusive lock costs an update that fires either trigger a wait for the tenant's scans in flight,
each about 10 ms at 1,691 rows (section 7). A bulk re-embed takes the lock once per transaction,
since an advisory lock a transaction already holds is granted again at once. With one sweeper per
process, at most one scan per tenant is ever in flight from this server.

A cycle can form, and `lock_timeout` on the scan is what breaks it. A scan holds the shared key and
then waits on row locks: its pair insert runs foreign-key checks that take `FOR KEY SHARE` on both
`memory` rows. A transaction that holds a `memory` row `FOR UPDATE` and then revives or re-embeds a
row fires a trigger that waits for the exclusive key. If the scan's candidate is the locked row, each
waits for the other. Two shapes in this repository reach it: the delete that locks a row with
`SELECT supersedes ... FOR UPDATE` and then revives its predecessor
(`src/adapters/postgres/memory.rs:2750-2780`), and any deployment path that locks a row before a
revive. Postgres's deadlock detector waits `deadlock_timeout` (1 s by default) and then may kill
either side, the user's transaction included. With `lock_timeout = '200ms'` the scan gives up
first, its row stays unmarked for the next sweep, and the user's transaction commits. On a scratch
copy both shapes deadlocked and the user's transaction was killed; with the timeout the revive
committed and the scan failed with a lock timeout.

One cycle the timeout does not reach: two transactions that both fire these triggers on rows of one
supersession chain, each holding a `memory` row lock the other wants before asking for the
exclusive key. Neither runs a scan, so neither carries the timeout, and Postgres's detector ends one
of them. Section 12 records it as an open risk.

### 4.5 The wake trigger

```sql
-- Wakes the sweeper for this row's tenant. Sends the tenant id only: a listener learns that some
-- row in the tenant waits for a scan, never which row or what it says.
--
-- Postgres delivers a notification only when the transaction commits, drops it on rollback, and
-- folds identical notifications inside one transaction into one. A bulk insert in one transaction
-- sends one per tenant; a rolled-back insert sends none; the sweeper never hears of a row before
-- the row is visible to it.
--
-- TRAP: never scan here and never write memory_conflict here. A scan inside this trigger runs
-- inside the inserting transaction, where two concurrent writes miss each other (section 6), and a
-- bulk insert pays a namespace scan per row inside the caller's statement. The catalog test in
-- tests/conflict_pairs.rs fails if any trigger function on memory calls memory_conflict_record or
-- inserts into memory_conflict.
CREATE OR REPLACE FUNCTION memory_conflict_wake() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  PERFORM pg_notify('memory_conflict', NEW.tenant_id);
  RETURN NULL;
END
$$;
CREATE TRIGGER memory_conflict_wake
  AFTER INSERT ON memory
  FOR EACH ROW
  WHEN (NEW.superseded_by IS NULL)
  EXECUTE FUNCTION memory_conflict_wake();
```

Row level rather than statement level with a transition table. A transition table would hold every
inserted row, 768-dimension vector included, in a tuplestore for the statement, and Postgres refuses
transition tables on triggers with an `UPDATE OF` column list, so the invalidating triggers could
not share the shape. Duplicate folding inside a transaction gives the statement-level saving
without the tuplestore.

The `WHEN` clause skips rows inserted already retired, which an archive restore of history writes.
They are not live, so `memory_conflict_next` would never pick them.

`pg_notify` needs no privilege and reads only `NEW`, so the trigger runs unchanged under any role
setup, row security included. It takes no advisory lock and no row lock.

### 4.6 Port and adapter

Appended to `MemoryRepository` in `src/ports/memory.rs`:

```rust
/// What one sweep call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictSweep {
    /// Rows whose scan committed in this call. A record call that failed does not count.
    pub scanned: i64,
    /// Live rows in the tenant still without a mark at or below the floor.
    pub pending: i64,
}

/// Scan up to `limit` live rows with no current mark, oldest first, one autocommitted statement per
/// row. `pending` counts every live row in the tenant, whoever may read it, so only a background
/// pass calls this; a request handler that returned it would tell a caller how many rows sit outside
/// their grant.
async fn sweep_conflicts(&self, tenant: &str, floor: f64, limit: i64) -> Result<ConflictSweep>;

/// Live rows this reader may see that carry no mark at or below `floor`. Grant applied inside the
/// query on the row's namespace and stored level.
async fn conflicts_pending(
    &self,
    tenant: &str,
    floor: f64,
    reader: &[NamespaceGrant],
) -> Result<i64>;
```

The port carries no single-row `record_conflicts`. Only the sweep scans, and a method that recorded
one row on demand would invite a caller to run it inside a write's transaction.

`conflicts` keeps its signature and its `ConflictPair` result. Its new body reads stored pairs
(4.9). `live_embedded_counts` loses its only caller and leaves the port.

The adapter calls the functions through three constant statements,
`SELECT memory_conflict_record($1, $2, $3)`,
`SELECT id FROM memory_conflict_next($1, $2, $3::int4) AS id` and
`SELECT memory_conflict_backlog($1, $2)`, so the adapter's statement-scan tests can see them. A
`SETOF uuid` function names its output column after itself, so `SELECT id` resolves only through
the alias.
`sweep_conflicts` is the loop in 4.3, in the adapter.

The listener is an adapter of its own, `src/adapters/postgres/conflict_wake.rs`:

```rust
/// The channel the three conflict triggers notify on. The payload is a tenant id.
pub const CONFLICT_WAKE_CHANNEL: &str = "memory_conflict";

/// LISTEN on the conflict channel on a connection of its own and hand every payload to `on_wake`.
/// Fails only when the first LISTEN fails. After that, sqlx's PgListener reconnects on its own, and
/// a wake sent while it was down is lost; the timer sweep picks the row up.
pub async fn listen<F>(pool: &sqlx::PgPool, on_wake: F) -> Result<tokio::task::JoinHandle<()>>
where
    F: Fn(&str) + Send + Sync + 'static;
```

A callback rather than a service type in the signature keeps the adapter free of any import from
`services`.

Why SQL functions rather than statements in the adapter: one definition of the scan, and a
deployment that enforces row security can replace the functions with `SECURITY DEFINER` versions of
the same signatures without editing the adapter. The cost: the scan lives outside the adapter's
statement-scan unit tests, so the integration suite carries its liveness and revival tests (plan
task E1-T4), and a change to the scan ships as a forward migration with `CREATE OR REPLACE`. A
deployment that replaced a function must re-apply its replacement after any such migration, because
`CREATE OR REPLACE` resets `SECURITY DEFINER` and `SET search_path` to the new definition's.

### 4.7 The write path

Unchanged. `services::write::run_inner` inserts and supersedes as it does today and calls nothing
new. The insert fires `memory_conflict_wake`, which costs one `pg_notify` call inside the insert and
one notification at commit (section 7).

The existing pre-insert `neighbours` query stays as it is: it decides the dedupe collapse and fills
`possible_conflicts`, and neither job can wait for a scan after commit.

Every other path that inserts a row (archive restore in either mode, ingest approval through
`run_observed`, a merge through the review queue, anything a deployment adds) fires the same
trigger. No insert path has to remember to call anything, and no insert path can call the scan.

### 4.8 The sweeper

`src/services/conflicts.rs` (new):

```rust
/// Rows one `sweep_conflicts` call takes. Each is its own statement, so the batch bounds how often
/// the budget is checked, not how long a transaction lasts: 50 rows took 554 ms on a scratch copy
/// at 1,691 rows a namespace.
pub const SWEEP_BATCH: i64 = 50;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub scanned: i64,
    pub pending: i64,
}

/// Sweep `tenant` until nothing is pending, a call commits no scan, or `budget` has passed. A row
/// that fails every time comes back in every batch, so the stop on a call that commits nothing is
/// what ends a sweep left with only failing rows.
pub async fn sweep(
    repo: &dyn MemoryRepository,
    tenant: &str,
    floor: f64,
    budget: std::time::Duration,
) -> Result<SweepReport>;

/// Tenants named by a wake since the sweeper last took them. A thousand wakes for one tenant before
/// the sweeper looks cost one sweep.
#[derive(Default)]
pub struct Wakes {
    tenants: std::sync::Mutex<std::collections::BTreeSet<String>>,
    notify: tokio::sync::Notify,
}

impl Wakes {
    /// Record a wake for `tenant` and release one waiter.
    pub fn wake(&self, tenant: &str);
    /// Wait for at least one wake, then return every tenant woken since the last call and clear the
    /// set.
    pub async fn wait(&self) -> Vec<String>;
}

/// The sweeper for one tenant, forever: a tick of `interval` (the first skipped) or a wake naming
/// `tenant` runs `sweep`; wakes for other tenants are dropped. One loop means one scan at a time
/// from this process.
pub async fn run_loop(
    repo: std::sync::Arc<dyn MemoryRepository>,
    tenant: String,
    floor: f64,
    budget: std::time::Duration,
    interval: std::time::Duration,
    wakes: std::sync::Arc<Wakes>,
);
```

`tokio::sync::Notify` keeps one permit, so wakes that land while a sweep runs leave one permit and a
set of tenants, and the loop sweeps once more for them.

`src/main.rs` gains `spawn_conflict_sweep`, shaped like `spawn_cleanup`. With `CONFLICT_SWEEP_SECS=0`
it starts nothing and logs once that no conflict pairs will be recorded and every new row stays
pending. Otherwise it builds an `Arc<Wakes>`, calls `conflict_wake::listen` with a closure that
calls `wakes.wake` (on failure, one warning, and the loop runs on the timer alone), and spawns
`run_loop` for `cfg.tenant_id`. A log line goes out only when a sweep scanned something or failed.

Two settings in `src/config.rs`, validated at boot:

| Variable | Default | Meaning |
|---|---|---|
| `CONFLICT_SWEEP_SECS` | `60` | seconds between timer sweeps, the fallback for a lost wake; `0` turns the sweeper and its listener off |
| `CONFLICT_SWEEP_BUDGET_MS` | `5000` | time one sweep may spend per tenant; refused below 100 or above 25,000 |

No setting turns the listener off on its own. Behind a connection pooler in transaction mode
`LISTEN` does not hold, wakes go missing, and the timer still sweeps every interval.

`CONFLICT_SCAN_MAX` stays readable for one release and does nothing (owner ruling, 3 October
2026). In E2, `config.rs` still parses it, drops its range check, and logs one deprecation warning
at boot when the variable is set: `CONFLICT_SCAN_MAX is ignored since conflicts are read from stored
pairs, and the next release removes it`. The refusal it fed goes in E2. The field and the variable
leave in the release after.

### 4.9 Readers

`CONFLICTS_SQL` keeps its name, its seven binds and its output columns, so every caller and any
downstream merge sees one changed body:

```sql
SELECT a.id AS older_id, a.namespace AS older_namespace,
       COALESCE(a.content, '') AS older_content,
       b.id AS newer_id, b.namespace AS newer_namespace,
       COALESCE(b.content, '') AS newer_content,
       c.similarity
  FROM memory_conflict c
  JOIN memory a ON a.tenant_id = c.tenant_id AND a.id = c.older_id
  JOIN memory b ON b.tenant_id = c.tenant_id AND b.id = c.newer_id
 WHERE c.tenant_id = $1
   AND c.similarity >= $2
   AND a.namespace = b.namespace
   -- `live!()` on both halves, under this statement's own aliases. Retirement writes nothing to
   -- this table; this is where it takes effect.
   AND a.superseded_by IS NULL
   AND (a.occurred_until IS NULL OR a.occurred_until > now())
   AND b.superseded_by IS NULL
   AND (b.occurred_until IS NULL OR b.occurred_until > now())
   AND EXISTS (
         SELECT 1
           FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
          WHERE CASE WHEN g.exact THEN a.namespace = g.prefix
                     ELSE left(a.namespace, length(g.prefix)) = g.prefix END
            AND sensitivity_rank(g.max) >= sensitivity_rank(a.sensitivity)
       )
   AND EXISTS (
         SELECT 1
           FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
          WHERE CASE WHEN g.exact THEN b.namespace = g.prefix
                     ELSE left(b.namespace, length(g.prefix)) = g.prefix END
            AND sensitivity_rank(g.max) >= sensitivity_rank(b.sensitivity)
       )
   AND NOT EXISTS (
         SELECT 1 FROM memory_pair_dismissed d
          WHERE d.tenant_id = c.tenant_id
            AND d.lo_id = least(a.id, b.id)
            AND d.hi_id = greatest(a.id, b.id)
       )
 ORDER BY c.similarity DESC, a.created_at, a.id, b.id
 LIMIT $3 OFFSET $4
```

The read does not test `embedding IS NOT NULL`. A deployment that keeps vectors in a second column
would drop every pair if it did, and a row that loses its vector loses its pairs through the
trigger in 4.4.

`CONFLICTS_PENDING_SQL` counts live rows under the reader's grant with no mark at or below `$2`,
with the same `unnest` block on the row.

`review_queue::conflict_items` drops the `live_embedded_counts` refusal. `Queue` gains
`conflicts_pending: i64`, filled whenever the conflict source answers. `review_queue::render` prints
one line when it is above zero: `N readable memories have not been checked for conflicts yet, so this
list may be short.` The CLI's `crates/lumberroom/src/wire.rs` reads the field with
`#[serde(default)]`, so it still parses an older server, and `review.rs` prints the same line.

## 5. Every event that touches a pair

"Wake" below means: a trigger notifies at commit, and the sweeper scans the row after the
transaction committed. "Pending" means the row counts in `conflicts_pending` until then.

| Event | Mechanism | Stale window | What a reader sees in the window |
|---|---|---|---|
| Insert on any path (MCP, CLI, console, merge, ingest approval, archive restore or merge, a direct insert a deployment adds) | `memory_conflict_wake`; the sweeper scans after commit | the wake's delivery plus one scan, tens of milliseconds (estimate); one sweep interval if the wake was lost | pending |
| Supersede, `expire`, valid time passing | none; the read tests both halves for liveness | none | nothing |
| Revive: `unexpire`, a delete splicing a chain back (0013), a supersession undone | `memory_conflict_revived` drops the mark and wakes | as for an insert | pending; the row's earlier pairs show again at once |
| Forget, delete, purge, erasure | `ON DELETE CASCADE` on both ids and on the mark | none | nothing |
| Re-embed in place, a model change written to `embedding_model` | `memory_conflict_moved` drops pairs and mark and wakes | as for an insert, behind the tenant's backlog | pending |
| Namespace or tenant changed in place | `memory_conflict_moved` | as for an insert | pending |
| Sensitivity changed in place | none; the read tests each half's stored level | none | nothing |
| Re-encryption of content | none; the vector does not change | none | nothing |
| `CONFLICT_THRESHOLD` lowered | marks above the new floor count as missing; no wake, so the first timer sweep after boot starts the rescan | until the sweeper has rescanned the store | pending, rising then falling |
| `CONFLICT_THRESHOLD` raised | none; the read filters on the threshold | none | nothing |
| `DEDUPE_THRESHOLD` changed | none; no reader in this repository bands on it | none | nothing |
| A pair dismissed or restored | the existing ledger, anti-joined at read | none | nothing |
| A scan that errors or times out | no mark written for that row | until a later sweep succeeds | pending |
| A wake lost (listener reconnecting, a pooler in transaction mode, the server down at commit) | the next timer sweep | up to `CONFLICT_SWEEP_SECS` | pending |

Under the advisory lock in 4.2 and 4.4, every path in this repository that changes a vector,
namespace, tenant, model or liveness either waits for the scans in flight or is waited for by them.
Three things still leave a stored pair wrong: a statement that rewrites `embedding` while the
triggers are disabled, an `UPDATE` that changes a vector through a column the triggers do not
list, and a deployment that replaces a trigger function and drops the lock. The parity check in
section 12 catches all three after the fact; nothing prevents them.

## 6. Why every scan runs after its row commits

Under `READ COMMITTED` each statement reads a snapshot taken when it starts. Take two rows A and B
written at the same moment into one namespace, each committed (at `ta` and `tb`) before its own scan
starts (at `sa > ta` and `sb > tb`). If B's scan started before A committed, then `sb < ta < sa`, so
`tb < sb < sa`: B had committed before A's scan began, and A's scan finds B. Otherwise B's scan
started after A committed and finds A. One side always sees the other.

A scan inside the inserting transaction loses that guarantee: two such transactions can each scan
before the other commits, both rows get marks, and the pair is never found. That rules out a scan
in an `AFTER INSERT` trigger and in a deferred constraint trigger alike (section 11).

This design meets the rule by construction. The only caller of `memory_conflict_record` is the
sweeper. A wake reaches it only after the inserting transaction commits, because Postgres holds a
notification until commit. A timer sweep reads only committed rows. Neither route can start a scan
before its anchor is visible. The catalog test in plan task E1-T4 (test 15) fails if any trigger
function on `memory` calls `memory_conflict_record` or inserts into `memory_conflict`, which is the
one edit that would break it.

A mark committed late changes nothing above: marks decide which rows get scanned, and the argument
is about which rows a scan sees.

## 7. Cost

Figures marked "scratch" were measured on 3 October 2026 on a scratch Postgres 16 with
pgvector, built from this schema and loaded with a copy of the store in section 1, on a 5 vCPU
laptop with a warm cache. Production has not run any of this. Everything else below is an estimate.

**Per write.** The write waits for no scan. It pays the wake trigger: one `pg_notify` call inside the
insert and one queue entry at commit. That is under a millisecond (estimate, not measured). Before
this revision the write waited for a scan after commit: p50 10.2 to 11.4 ms, p95 16.2 to 18.7 ms at
1,691 rows (scratch), growing linearly with the namespace.

**The notify commit lock.** On Postgres 16 a transaction that has queued a notification takes one
database-wide lock while it commits and holds it through the commit's flush to disk, so commits
that notify run one at a time. Recall.ai traced production outages to this lock under many
concurrent writers in March 2025; a fix is committed for Postgres 19 and is in no production release
(both as reported, not measured here). A store of this size writes a few rows a minute (estimate),
far from contention. Deploy step F1-5 in the fork compares `memory_write` latency before and after;
section 12 carries it as an open risk.

**Wake to stored pair, estimate.** Delivery to the listener in milliseconds, then
`memory_conflict_next`, one scan (scratch p50 10.2 to 11.4 ms at 1,691 rows) and
`memory_conflict_backlog`. The two anti-join statements are unmeasured; gate E1-G3 times them.

**Per scan, estimate.** The self-join's 6.4 s over 1.93 million pair evaluations is about 3.3
microseconds a pair, and the serial 11.7 s run is about 6 microseconds. A row's scan evaluates one
pair per live row in its namespace and detoasts each candidate once, so at 1,691 rows it should cost
about 5 to 10 ms. The scratch p50 of 10 to 11 ms sits at the top of that range, so plan on the
scratch figures. The scan grows linearly with the namespace: an estimated 60 to 110 ms p95 at 10,000
rows, scaled from the scratch p95. Off the write path that is sweeper time, not user time. Gate
E1-G3 measures the scan on a restored copy before anyone deploys it.

**Per read, scratch.** On 3,874 rows and 195 stored pairs, run as the serving role, the read took
17.6 ms. 13.4 ms of it was a sequential scan of `memory` feeding a hash join, and the planner left
`memory_conflict_by_similarity` unused. The read is bounded and does no vector arithmetic; it is not
an index walk. Gate E2-G2 records the plan and time on a copy of the real store; an index that lets
the planner start from the pairs is follow-up work if that plan matters. The store in section 1 held
439 pairs at or above 0.90 (measured on the store itself).

**Backfill.** Every row scans its own namespace once, so a store costs about the sum over namespaces
of the square of each size, times 3 to 6 microseconds: an estimated 12 to 23 s of database time for
the store in section 1, spread across sweeps of at most 5 s each. Scratch: a full backfill of 3,783
rows took 18.2 s, and one 50-row sweep took 554 ms.

**Bulk inserts.** An archive restore inserts one autocommitted row per statement, so it sends one
notification per row and the sweeper drains them at its budget, off the restore's path. The restore
itself pays no scan.

**Storage.** One pair row is about 70 bytes plus index entries. 439 pairs is under 100 kB. The
threshold drives the size: on a real store of 1,040 live rows, 68 rows had a same-namespace
neighbour at 0.90 or above and 467 at 0.80 (measured 15 September 2026). An operator who lowers
`CONFLICT_THRESHOLD` to 0.80 should expect several times the pairs and a full rescan.

## 8. Backfill

The migration creates empty tables, so every live row starts pending. The sweeper is the backfill:
oldest rows first, `SWEEP_BATCH` rows a call, at most `CONFLICT_SWEEP_BUDGET_MS` a tenant a sweep,
started by the first timer tick or the first wake. It resumes by construction, because the marks are
rows: a restart, a crash or `CONFLICT_SWEEP_SECS=0` leaves the marks already written, and the next
sweep starts at the oldest unmarked row. No separate job, no lock on `memory`, no window. Progress is
`SELECT memory_conflict_backlog(<tenant>, <threshold>)`, which counts what is left.

While the backlog drains, a new row's scan waits behind it, since the sweep takes rows oldest first.

Archives carry rows and no pairs, so a restored store starts every row pending and the sweeper
rebuilds its pairs the same way.

An operator who wants readers to switch only once the store is complete deploys plan E1 first and
E2 after `pending` reads zero. A self-hosted user who takes both at once sees the pending line in
`lumberroom review` until the sweeper finishes.

## 9. Read semantics

**The same as today.** The pair set at a given threshold once nothing is pending; both halves live;
both halves inside the grant at their stored level, tested in the query; the dismissed ledger
anti-joined; one row per pair, older first; similarity values to the bit; order and paging.

**Different, each on purpose.**

1. **A pending row's pairs are missing until it is scanned.** Right after a write that is tens of
   milliseconds (estimate). The read reports `conflicts_pending`, and the queue and CLI say the list
   may be short. Today the read is never short, and it is also never fast.
2. **A namespace past `CONFLICT_SCAN_MAX` gets an answer.** Today it gets 400
   `namespace_too_large`. The refusal existed only to protect the self-join. The setting itself is
   read and ignored for one release (4.8).
3. **A floor below `CONFLICT_THRESHOLD` cannot be answered below the stored floor.**
   `review_queue::queue` already clamps the requested floor up to the threshold
   (`src/services/review_queue.rs:590-596`), so this repository's readers return the same set. A
   reader elsewhere that passes a lower floor gets the pairs from the threshold up.
4. **A row with no vector.** Today the join drops it by testing `embedding IS NOT NULL`. Stored
   pairs drop it because the trigger deletes its pairs when the vector goes. The outcome is the
   same; the mechanism moved.

## 10. Why not a pgvector HNSW query per write

Three reasons, the first one sufficient.

1. **A pair set needs every neighbour above the floor, and HNSW returns the nearest k.** A row with
   more than k neighbours above 0.90 would lose pairs, and no k is safe for a namespace full of
   restatements. Dropping the `LIMIT` and filtering on distance alone turns the index scan into the
   iterative scan running to `hnsw.max_scan_tuples` whenever few rows qualify, which is the common
   case.
2. **The planner does not pick the index for this filter.** The `LATERAL` statement in section 1
   skipped `memory_embedding_hnsw`, took the `memory_occurred_at` btree (which leads with tenant and
   namespace) and sorted, in 48.8 s. A scale probe on 23 September 2026 found the planner choosing
   the tenant and namespace btree over HNSW for every tenant-scoped search arm.
3. **The exact scan is cheap at today's sizes.** Section 7 has a scratch p95 of 16.2 to 18.7 ms at
   1,691 rows. With no `ORDER BY` on distance the planner has no reason to consider HNSW; which
   btree it takes is unverified, and gate E1-G3 records the plan.

The reversal condition in decision 0022 names when this flips.

## 11. Options that lost

Four shapes for moving the work into the database were weighed on 3 October 2026. The chosen shape
is C: the trigger only wakes, and the sweeper scans after commit.

**A. An `AFTER INSERT` row trigger that runs the scan.** Lost on four counts. (1) The scan runs
inside the inserting transaction, so two concurrent writes into one namespace each miss the other
(section 6). Fixing that needs a per-namespace exclusive lock held to commit, which serialises every
write into the namespace for the rest of its transaction. (2) The user waits for the scan, 10 to
19 ms at 1,691 rows (scratch), linear in the namespace, which is the latency the owner asked to
remove. (3) A scan that hits `lock_timeout` raises inside the user's statement and aborts the write,
unless the trigger catches it in an `EXCEPTION` block. Each such block opens a subtransaction, and
past 64 subtransactions in one transaction Postgres overflows its per-backend subtransaction cache,
a known slowdown for every session on the server; a bulk insert reaches 64 at its 65th row. (4) Bulk
paths multiply it: a multi-row transaction pays one namespace scan per row, holds the tenant's shared
advisory key to commit (every re-embed, shred and revive in the tenant waits for the whole
transaction), and holds every pair key it inserted, the shape that deadlocked the batched sweep on a
scratch copy. In the fork the trigger also fires inside promotion's move transaction
(`MOVE_INSERT_SQL`), which already holds a candidate `FOR UPDATE`, and the 30 s `statement_timeout`
on every pooled connection caps the statement.

**B. A `DEFERRABLE INITIALLY DEFERRED` constraint trigger that scans at commit.** It runs before
the commit becomes visible, so it keeps A's visibility hole: two transactions can both reach their
deferred scans before either commits. Closing it needs the same serialising lock. The user still
waits for the scan, inside commit. A failure at commit aborts the whole transaction, so it needs the
same per-row `EXCEPTION` block. Constraint triggers are row level only, so a bulk insert queues one
deferred scan per row and pays them all at commit. `SET CONSTRAINTS ALL IMMEDIATE` in any session
turns it back into A.

**D. Keep the write path's call after commit, with only the invalidating triggers in the database.**
Correct, and it was the previous revision. Lost on performance and on enforcement: the user waits
for the scan (scratch p95 16.2 to 18.7 ms at 1,691 rows, growing linearly), and the rule that the
call never runs inside the inserting transaction was a convention the code could not enforce, pinned
only by a test that caught a violation with some probability per round.

**C with a queue table.** A trigger that writes a queue row per insert adds a write to every insert,
a table to secure and a second record of what an absent mark already says.

**An in-process wake from `write::run_inner`.** No commit lock, but every insert path has to
remember to send it, the same kind of unenforced convention that sank D. A trigger covers every
path, a direct `psql` insert included.

**A short timer with no wake.** Freshness costs a poll per tenant per interval forever, and each
poll runs the anti-join in `memory_conflict_next` over the tenant's live rows whether anything
changed or not.

**Row ids in the notification.** A bulk restore would queue one distinct payload per row, and
anyone who can `LISTEN` on the database would see which rows changed. The tenant id is enough: the
sweep finds the rows.

**Keep the self-join and cache its result per request or per minute.** Every cache miss still pays
seconds, the first reader after any write pays it, and a cache keyed by grant multiplies the work by
the number of distinct grants.

**Reuse the pre-insert `neighbours` query.** It has no id to pair with, returns three rows, and
filters on the writer's ceiling (section 3).

**Store only pairs the writer could read.** The stored set would depend on who wrote second, and a
reader with a wider grant would miss pairs today's join shows.

**A scan mark as a column on `memory`.** Every scan would update a `memory` row, and an update that
is not HOT adds an entry to every index on the table, the HNSW index included.

**A per-tenant HNSW partial index first.** It answers the wrong question (point 1 of section 10) and
adds an index per tenant to maintain.

## 12. Gates

| Gate | What it settles | Where |
|---|---|---|
| E1-G1 | an insert notifies once per tenant at commit and never on rollback; two concurrent writes in one namespace, then a sweep, yield their pair; a write's pair appears after its wake with no tick | integration tests 1, 2 and 16 |
| E1-G2 | each event in section 5 leaves the stored set equal to a fresh self-join | integration test, one case per row |
| E1-G3 | `memory_conflict_record` on a 1,691-row namespace: a btree on tenant and namespace, no Sort, no HNSW, p50 over 20 runs; `memory_conflict_next` and `memory_conflict_backlog` timed on the same tenant | `EXPLAIN (ANALYZE, BUFFERS)` on a restored copy |
| E1-G4 | a sweep resumes after a restart and reaches `pending = 0` | integration test |
| E1-G5 | no trigger function on `memory` calls `memory_conflict_record` or inserts into `memory_conflict` | catalog test 15 |
| E2-G1 | parity: for a fixture store and for a restored real store after backfill, the new read equals the old `CONFLICTS_SQL` row for row at three grants and two floors | integration test plus a one-off script against the copy |
| E2-G2 | the new read's plan and time on the restored copy | `EXPLAIN (ANALYZE, BUFFERS)` |
| E2-G3 | the queue envelope and CLI wire fixture carry `conflicts_pending` | wire test |

Nothing in this document has run in production. Section 7 separates scratch measurements from
estimates; E1-G3 and E2-G2 settle both on a copy of a real store.

Open risks, recorded and not fixed:

- On Postgres 16 every transaction that notifies takes a database-wide lock at commit, so notifying
  commits run one at a time. The store's write rate is far below where that bites (estimate); a
  store with hundreds of concurrent writers would feel it.
- A lost wake leaves a row pending for up to one timer interval. Behind a pooler in transaction mode
  every wake is lost and freshness falls to the timer.
- If the notification queue (8 GB by default) fills, every transaction that notifies fails at
  commit, writes included. It fills only when some session runs `LISTEN` and then sits in an open
  transaction for a long time; `PgListener` opens no transaction. Postgres logs a warning naming the
  session at half full.
- A row whose scan fails the same way every time stays pending, and `pending` never reaches 0. The
  rows behind it still drain: the failing row takes one slot in each batch, and the sweep stops once
  a batch commits no scan. Each tick and each wake then costs one failed scan per batch, up to
  242 ms when it fails on the lock timeout (scratch, measured in review). The warning names the row,
  and nothing skips it. Fifty or more such rows fill every batch and starve the rows behind them.
  A new row's scan also waits behind any backlog, oldest first.
- Two transactions that fire the invalidating triggers on rows of one supersession chain can
  deadlock each other (section 4.4); the scan's `lock_timeout` does not cover it, and Postgres kills
  one of them.
- A scan that times out behind a re-embed, a shred or a locked row leaves its row pending until the
  next sweep.

## 13. Not for

The hourly cleanup pass's own near-duplicate join (`SIMILAR_PAIRS_SQL` in
`src/adapters/postgres/cleanup.rs:145-185`) stays as it is: it anchors on rows written since its
watermark, runs off the request path at its own floor, and this design changes no cleanup
behaviour. The `possible_conflicts` a write returns stay computed by the pre-insert query. Nothing
here decides a conflict or touches the review verdicts.
