# 33. Staleness counts from the last write or confirmation, not the last read

**Date:** 13 October 2026 · **Status:** accepted, implemented; the full suite passed,
1842 passed and 0 failed, not merged · **Decided by:** the owner

## Decision

A live row is stale when it was written more than `STALE_DAYS` days ago (365 by default, or the
`days` a caller passes) and nobody has confirmed it within that span. Reads play no part.
`STALE_SQL` in `src/adapters/postgres/memory.rs` drops its `last_accessed_at IS NULL` clause and
keeps the rest: the `created_at` threshold, the confirmation window floored at one day, liveness
and the grant.

Content rows are append-only. A rewrite inserts a new row that supersedes the old one, so
`created_at` is the last write. `last_confirmed_at` is the last confirmation, and three paths set
it: a `confirm` verdict in the review queue, a write that restates an existing open fact, and an
ingest emission that restates one. A write that restates a private fact confirms nothing, because
the exact-match lookup in `write.rs` runs on open rows only, so a private row leaves the queue only
through a `confirm` verdict or an ingest restatement. The rule needs no new column.

`last_accessed_at` and `access_count` stay. Search still bumps them, `/statsz` still reports
`never_retrieved` from them, and the cleanup pass's unread scan still reads
`access_count`. Those measure use, which is what they always measured.

## Context

The stale source listed only rows nobody had ever read. Every `memory_search` hit sets
`last_accessed_at` on the rows it returns. An agent working a review reads page one, searches for
newer facts on each subject, then reads page two. The agent replay behind decision 0031 found that
the search had already pulled the rows it touched out of the stale source, so items the agent had
not yet seen dropped out of the queue mid-review. Decision 0031 restored the 50-item default page,
which hid the symptom on a small queue and left the cause in place for any queue longer than one
page.

The rule also answered the wrong question. A fact turns stale because the world moves on after it
was written. An agent reading it every day does not make it current, and a fact read often is the
one where a wrong answer costs most.

## What lost

**Snapshot the queue at the first read.** The service would freeze the stale ids on page one and
serve later pages from the snapshot. It fixes paging and keeps the meaning, but it adds state the
queue does not have today: a snapshot per caller, an expiry, and a rule for what a verdict does to
a frozen page. A queue that agents read over MCP from several clients makes that state hard to
scope. It also keeps the wrong meaning.

**Skip the access bump during a review.** Search would leave `last_accessed_at` alone while a
review was in progress. The server has no notion of a review session, so it would have to guess
from the caller or take a flag on every search. A search that skips its bump also leaves usage
figures short. It keeps the wrong meaning too.

## What it costs, accepted

- Every user sees a different stale queue after the upgrade. Rows that were read at least once and
  are older than the threshold join the queue. On a store whose agents search often, that is most
  of its old rows, and the first review after the upgrade is longer than the last one before it.
- An old row that agents still read often now shows as stale. That is the intent, and it costs a
  `confirm` per row once a window. A confirmed row returns after one window, so a store full of
  lasting facts asks for each to be confirmed once a year at the default.
- `--days 0` used to list every row nobody had read. It now lists every live row not confirmed in
  the last day.
- The `staleness` block in `/statsz` keeps its name and still counts reads. A reader can
  take its `never_retrieved` figure for the size of the stale queue; it is not.

## What this is not for

It is not a reaper. Nothing expires or deletes a stale row; a person or an agent decides each one.

It does not touch the cleanup pass's unread scan (`UNREAD_SQL`, `access_count = 0`), which proposes
rows nobody ever used and has its own window. That scan is about use, and reads are the right
signal there.

It does not change how search ranks rows or what `access_count` records.

## Reversal condition

Revisit if reviews show agents confirming the same rows in bulk without reading them, so the queue
becomes a chore that trains people to click through. The evidence would be `confirm` verdicts
clustered in time on rows the agent did not search around, read from `tool_calls`. A per-namespace
or per-kind threshold is the first step before restoring any read signal.
