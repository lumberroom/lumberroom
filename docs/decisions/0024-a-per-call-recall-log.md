# 24. A per-call recall log, off by default

**Date:** 6 October 2026 · **Status:** accepted by the owner, implemented on branch
`feat/recall-event-log`, not merged · **Decided by:** the owner, 6 October 2026, after the bootstrap
ranking evaluation of the same day

## Decision

Add `recall_event`, an append-only table with one row per memory per `memory_search` or
`context_bootstrap` call: the call id, tool, client, session, active project, memory id, namespace,
digest section, rank and time. No content, no content digest, no query text. `RECALL_EVENT_LOG`
turns it on and defaults to off. `RECALL_EVENT_RETENTION_DAYS`, default 30, bounds it, and the
cleanup pass enforces the window. Forgetting a memory deletes its events through `ON DELETE CASCADE`.

The rows ride in the statement that already writes `recall_emission`, as a data-modifying CTE, on
the task that statement already runs on. The log adds no round trip, and the caller waits on
nothing.

## Context

`recall_emission` keeps one row per (tenant, content digest, memory, tool), with the first and last
emission time and a count. That is the right shape for the echo check it exists for. It cannot say
which rows one call returned together.

An offline evaluation of digest ranking needs exactly that: which rows a session saw, and which of
them it used next. From the aggregate you can only bound it. The first and last emission times give
a lower bound that misses every emission in between, and an upper bound that credits any row whose
emissions straddle the window. A bootstrap's project has to be inferred from nearby emissions, and
some calls have none. A few weeks of a per-call log replace both bounds with exact labels.

## What it is not for

**It is not analytics.** Nothing in the server reads the table, no screen shows it, and no ranking
uses it. A feature that wants to read it is a new decision.

**It is not a content record.** A row holds ids, positions and times. It adds no plaintext exposure,
and none of the verification-oracle concern migration `20260823000017` removed from
`recall_emission`, because there is no digest to test a guess against.

## Choices inside it

**Private rows are logged.** `recall_emission` skips them because it stores a keyed digest of the
content. This log stores no content, only ids. For a private row it adds what `access_count` and
`last_accessed_at` do not hold: which client read the row, when, and alongside which other rows.
That is metadata about use, covered under the disclosure cost below, and it exposes no content.
Skipping private rows would make the evaluation blind to every private fact.

**A cached digest logs a call.** The cache hit returns rows to a caller, so it is a call with rows.
It still records no emission, as before.

**A row can appear twice in one call.** The digest's recent section can repeat a row from profile or
project. Both appearances are logged with their own section and rank, so the table has an identity
key and no unique (call, memory) key. A unique key would have failed the whole statement, emissions
included, on the first such digest.

**Digest rows come from the structured payload.** The digest logs the rows its payload carries.
The markdown can leave an entry out for budget, and the log does not record that.

**`call_id` is minted in the service and does not join `tool_calls.id`.** Joining would mean
threading an id through `Ctx`, which 15 test files and three server modules build field by field.
`tool_calls` carries the client, the session and the time, which pairs the two closely enough for
an evaluation, and an exact join is a later change if one turns out to need it.

**The by-memory index leads with `memory_id`.** The cascade a forget triggers filters on `memory_id`
alone, and a `(tenant_id, memory_id)` index cannot serve that on Postgres 16 without scanning it
whole. Memory ids are unique across tenants, so `(memory_id, tenant_id)` serves the evaluation's
per-row read as well.

**Retention needs the cleanup pass.** The purge runs on `CLEANUP_INTERVAL_SECS` rather than a
scheduler of its own, so boot refuses `RECALL_EVENT_LOG=true` with `CLEANUP_INTERVAL_SECS=0`. It
deletes 5,000 rows a statement, oldest first, until a batch comes back short. Three limits on the
window follow from that:

- An event lives up to `RECALL_EVENT_RETENTION_DAYS` plus one `CLEANUP_INTERVAL_SECS`. The pass
  skips its first tick after boot, and a restart starts that wait over.
- The purge runs with the log off too, so rows written before the owner turned it off still age
  out. With the log off and `CLEANUP_INTERVAL_SECS=0`, nothing deletes them, and they stay until
  the owner turns the pass back on or deletes them by hand.
- The purge reaches the live database only. A dump from `deploy/backup.sh` keeps the events it
  holds for that dump's own retention, 14 days by default.

**An archive does not carry it,** as it carries neither `recall_emission` nor `tool_calls`. An
import assigns new ids, so the events would point at rows the destination does not have.

## What lost

**Leaving `recall_emission` alone and living with approximations.** The lower and upper bounds it
allows can disagree by a wide margin, and that gap is the question the evaluation exists to
answer.

**A separate statement for the log.** It costs a round trip on every read for nothing the CTE
cannot do.

**On by default.** A self-hoster who never runs an evaluation gains nothing from a table that says
which facts each client used and when.

## What it costs, accepted

- **Shared fate with the emission write.** One statement means one failure: a call whose insert
  fails loses both records, and a log line says so. The event list names rows the emission list
  leaves out, private rows among them, so a forget between the read and the insert could fail the
  foreign key on an event alone. The events insert skips ids with no memory row to close that,
  which leaves only a delete committing inside the statement itself, the window the emission
  insert already had.
- **Disclosure.** The log reveals which facts a tenant used and when. A deployment that turns it on
  should say so in its privacy notice.
- **A cache hit now spawns a task when the log is on.** Measured below.

## Measured

Observed on 6 October 2026 with `./scripts/cargo.sh test -j 1 --test recall_event -- --ignored
--nocapture latency`: a fresh database per run, 400 seeded rows across four namespaces, the hash
embedder, 20 timed runs per line after one warm-up, in the order off, on, on, off. The laptop was
running other cargo builds at the time, which the spread shows.

| tool | log off, median ms (two runs) | log on, median ms (two runs) |
|---|---|---|
| `memory_search` | 10.43, 6.39 | 8.18, 11.69 |
| `context_bootstrap`, built | 5.36, 5.30 | 7.09, 8.41 |
| `context_bootstrap`, cached | 0.39, 0.54 | 0.80, 0.57 |

At 20 runs the search and build numbers show no difference the noise does not swamp. An earlier
off-then-on run of the same test gave search 7.56 against 5.77 and build 6.33 against 3.62, the
other way round. The cache-hit path costs about 0.2 to 0.4 ms more with the log on in both runs,
which is building the record and spawning the task that the off path skips.

Not measured: the database time of the folded statement against the emission insert alone, and
anything on a production-sized store.

## Reversal condition

Drop the table and the two settings once the ranking evaluation it serves has run and nobody has
asked for a second one within a release. If a feature ever needs to read the log at request time,
that is a new decision about access, retention and the privacy notice, not an extension of this one.
