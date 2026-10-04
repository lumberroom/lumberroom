# 0022. Conflict pairs are recorded when a row is written

> **Draft.** PR E1 (the pair tables, triggers and sweeper) merged as #106 (47d7a55). PR E2 (the readers) is implemented on branch `feat/write-time-readers` and not merged. Gate E2-G2 (the read's cost on real data) has not run. This document changes as the build proceeds.

3 October 2026. Proposed, design; nothing built, nothing run. Revised the same day (revision 1,
below): a trigger on `memory` wakes the sweeper, and the write path's call to the scan is withdrawn.
Spec: [`docs/specs/write-time-conflicts.md`](../specs/write-time-conflicts.md). On acceptance this
record supersedes two parts of [0018](0018-one-review-queue.md): "a stored queue table for
conflicts" in its "What lost", and the `CONFLICT_SCAN_MAX` refusal. The setting itself is read,
ignored and warned about for one release, then removed (owner ruling, 3 October 2026).

4 October 2026. Status: proposed; E1 merged in #106, E2 implemented and not merged. The readers
list and count stored pairs on the E2 branch. Gate E2-G2 (the read's cost on a real store) has not
run; until it does, the read's 17.6 ms under "Costs accepted" stays a scratch-copy figure.

## The decision

The store keeps every live conflict pair in a table, `memory_conflict`, and the conflicts read lists
and counts from it. The read does no vector arithmetic.

A row's pairs are found by one exact scan of its own namespace, `memory_conflict_record`. A second
table, `memory_conflict_scan`, marks each row whose pairs are on record and the floor they were
found at. A live row with no current mark is pending. A sweeper scans pending rows, which makes
every insert path, a revival, a lowered threshold and the first backfill one mechanism. The sweeper
commits one row's scan at a time.

The original text continued: "which the write path calls after the insert has committed, as a
statement of its own". Revision 1 withdraws that call. The sweeper is now the only caller, and a
trigger wakes it (below).

Two triggers on `memory` keep the marks honest: a changed vector, namespace, tenant or model deletes
a row's pairs and mark, and a row that comes back to life loses its mark. Scans hold a per-tenant
advisory lock shared and both triggers take it exclusive, so no scan that read an old vector can
insert its pair after the trigger cleared the row. Retirement, sensitivity and the grant are applied
at read time on both halves, inside the statement, as today.

Readers report `conflicts_pending`, the readable rows not yet scanned, so a short list says it is
short.

## Revision 1, 3 October 2026: the trigger wakes, the sweeper scans

**Decision.** An `AFTER INSERT` row trigger on `memory`, `memory_conflict_wake`, calls
`pg_notify('memory_conflict', tenant_id)`. The two invalidating triggers notify the same way after
they clear a row. A listener (`sqlx` `PgListener`) hands each tenant to the sweeper, which scans the
tenant's pending rows after the transaction committed. The timer stays as the fallback for a lost
wake. `write::run_inner` calls nothing new, and the port loses `record_conflicts`.

**Context.** The owner asked for the detection to move into a trigger in the migration, for write
performance. The previous revision made every write wait for its scan after commit: p50 10.2 to
11.4 ms and p95 16.2 to 18.7 ms at 1,691 rows (scratch), linear in the namespace. It also rested on
a rule the code could not enforce, that the call never runs inside the inserting transaction.

**What lost, and why.**

- *A scan in an `AFTER INSERT` trigger.* It runs inside the inserting transaction, so two concurrent
  writes into one namespace each miss the other unless a per-namespace exclusive lock serialises
  them to commit. The user waits for the scan. A lock timeout aborts the write unless caught in an
  `EXCEPTION` block, and one block per row overflows Postgres's 64-entry subtransaction cache on any
  bulk insert. A multi-row transaction pays one scan per row and holds the tenant's shared key and
  every pair key it inserted to commit, the shape that deadlocked the batched sweep on a scratch copy.
- *A deferred constraint trigger that scans at commit.* Still inside the transaction, so the same
  visibility hole and the same serialising lock; the user still waits; a failure aborts the commit;
  row level only, so bulk inserts pay one scan per row at commit; `SET CONSTRAINTS ALL IMMEDIATE`
  turns it back into the previous shape.
- *Keeping the write path's call.* Correct, and slower for every write than a notification, with
  its ordering rule pinned only by a probabilistic test.
- *A queue table written by the trigger, an in-process wake from the write path, a short polling
  timer, row ids in the payload.* Spec section 11 gives each reason: an extra write per insert, a
  convention every insert path must remember, a poll that never stops, and a payload that names rows
  to any listener.

**Costs accepted.**

- The write returns before its pairs exist. The gap is the wake's delivery plus one scan, tens of
  milliseconds (estimate); `conflicts_pending` covers it, and nothing in the engine reads pairs in
  the same request as the write.
- On Postgres 16 every transaction that notifies takes a database-wide lock at commit, held through
  the commit's flush, so notifying commits run one at a time. Fixed upstream for Postgres 19, in no
  production release (as reported). At this store's write rate it should not show (estimate).
- A lost wake waits for the timer, `CONFLICT_SWEEP_SECS` (60 by default). Behind a pooler in
  transaction mode every wake is lost.
- The notification queue (8 GB default) fills only if a listening session sits in an open
  transaction for a long time; when full, every notifying transaction fails at commit, writes
  included.
- `CONFLICT_SWEEP_SECS=0` now stops all pair recording, where before it left the inline call running.
- A new row's scan waits behind any backlog the sweep is draining, oldest first.
- A row whose scan fails every time stays pending and costs one failed scan per batch on every
  sweep. Fifty or more such rows fill every batch and starve the rows behind them.

**What became moot.** The rule that `record_conflicts` never runs inside the inserting transaction
and the probabilistic 20-round test that guarded it give way to a catalog test: no trigger function
on `memory` may call `memory_conflict_record` or insert into `memory_conflict`. The cost "an inline
write behind a re-embed or a shred gives up the same way" no longer applies to writes; the
`lock_timeout` stays for the sweeper's own deadlock with lock-then-revive transactions. The per-row
commit in the sweep stays, now because a batch would hold the tenant's shared key across 50 scans
and one failure would roll back 49, not because inline writes could deadlock with it.

**Reversal condition.** Return to the write path's after-commit call (the previous revision) if
production shows `memory_write` latency rising after the deploy with waits on the notify commit lock
(`Lock` waits on object 0 of class 1262 in `pg_stat_activity` or the lock log), or if wake-to-pair
p95 on production passes 1 s outside a backfill.

## The context that forced it

On 3 October 2026 the self-join behind the conflicts read took about 6.4 s per run on a real store
of about 3,870 live memories whose largest namespace held 1,691 rows, and about 74% of that went to
detoasting vectors (measured with `EXPLAIN ANALYZE`; the spec's section 1 carries the numbers). The
cost is quadratic in the largest namespace and every read paid it again. Decision 0018's answer, a
refusal past `CONFLICT_SCAN_MAX`, kept the server up by giving the largest stores no conflict review
at all. The owner ruled the same day that conflicts move to write time.

## What lost, and why

**Caching the self-join.** A miss still costs seconds, the first read after any write misses, and a
cache keyed by grant multiplies the work.

**Recording pairs inside the insert's transaction, or from an `AFTER INSERT` trigger.** Two
concurrent writes would each scan before the other committed, both would carry marks, and their pair
would never be found. A trigger would also put a namespace scan inside every bulk insert. Revision 1
keeps this verdict for a trigger that scans and adopts a trigger that only notifies.

**A pgvector HNSW query per write.** A pair set needs every neighbour above the floor and HNSW
returns the nearest k; dropping the limit turns the scan into one that runs to its tuple cap. The
planner also declined the index for a tenant and namespace filter in the measurement that took
48.8 s. The exact scan took a p95 of 16.2 to 18.7 ms at 1,691 rows on a scratch copy.

**Detection on the timer alone.** Simpler, and every new pair would wait a sweep interval. The
owner asked for detection at write time. Revision 1 keeps the timer as a fallback and lets the
trigger's notification start the sweep at commit.

**The whole sweep batch in one function call, or a procedure that commits per row.** One call holds
every pair key it inserted and the tenant's shared key until it commits; on a scratch copy it
deadlocked with a writer in the same namespace. A committing procedure cannot run as
`SECURITY DEFINER`, which a deployment under row security needs.

**Storing only the pairs the writer could read.** The stored set would depend on who wrote second.

**The scan mark as a column on `memory`.** Every scan would update a `memory` row, and a non-HOT
update writes into every index on the table, the HNSW index included.

## Costs accepted

Revision 1 withdraws the first and the ninth items below. Each keeps its original words in quotes,
so the record shows what was accepted before.

- Withdrawn by revision 1: "A write pays one exact scan of its namespace after the insert, linear in
  the namespace." The scan now costs sweeper time: on a scratch copy at 1,691 rows (5 vCPU laptop,
  warm cache) p50 10.2 to 11.4 ms, p95 16.2 to 18.7 ms. Production is unmeasured until gate E1-G3.
- An update that changes a vector, namespace, model or liveness waits for the tenant's scans in
  flight, about one scan's time.
- A scan carries `lock_timeout = '200ms'`. A transaction that holds a `memory` row lock and then
  revives a row would otherwise deadlock with a scan whose foreign-key check waits on that row, and
  Postgres could kill the user's transaction. With the timeout the scan gives up and its row waits
  for the next sweep.
- Two transactions that fire the invalidating triggers on rows of one supersession chain can still
  deadlock each other. The timeout does not reach that cycle, and Postgres ends one of them.
- The read is a bounded join, not an index walk: 17.6 ms on a scratch copy of 3,874 rows, most of
  it a sequential scan of `memory`.
- Two scans of the namespace per new row, since the pre-insert neighbour query stays for the dedupe
  collapse. Only the pre-insert one is on the write path.
- A pair can be missing for up to one sweep interval after a lost wake, a lowered threshold or a
  failed scan. Readers say so through `conflicts_pending`.
- The scan lives in SQL functions in a migration, outside the adapter's statement-scan tests, and
  changes to it ship as forward migrations. A second storage backend implements it in its adapter.
  A deployment that replaces a function must re-apply the replacement after each such migration,
  because `CREATE OR REPLACE` resets its security and `search_path`.
- Withdrawn by revision 1: "The rule that `record_conflicts` never runs inside the inserting
  transaction is a convention the code cannot enforce." No such call exists; a catalog test pins
  the trigger shape.
- Lowering `CONFLICT_THRESHOLD` rescans the store and grows the table several times over at 0.80.
  The owner ruled on 3 October 2026 to document that cost and refuse nothing at boot.

## What this is not for

Deciding a conflict, changing a verdict or the dismissed-pair ledger. The cleanup pass's own
near-duplicate join. The `possible_conflicts` a write returns, which stay with the pre-insert query.

## Reversal condition

If the scan's measured p95 passes 50 ms on any store's largest namespace (scaled from the scratch
p95, an estimated 4,500 to 5,000 rows), the scan moves to an indexed neighbour search with a recall
check against the exact scan. Since revision 1 a slow scan lengthens the wait between a write and
its pairs and slows the backfill; it no longer slows the write. If the parity gate (E2-G1) ever
finds a pair the self-join returns and the stored read does not, readers go back to the self-join
until the gap is explained. Revision 1 carries its own reversal condition above.
