# 0017. A fact can stop being true without being replaced

15 September 2026. Accepted, implemented.

What was run, and where: the maintainer ran the whole engine suite on a clean
`lumberroom_rust_test` and it returned 993 passed across 17 binaries with no "skipping" line
anywhere, which is the run this record stands on. Alongside it, `./scripts/cargo.sh check
--all-targets` is clean apart from a warning that predates this work, and
`./scripts/cargo.sh test -j 1 --lib` returns 781 passed and 0 failed.

The skip is worth knowing about: the suite reports every test as ok when the database it finds
carries migrations it does not recognise, so a pass count alone settles nothing and the absence of
"skipping" lines is the part to read.

Nothing is deployed and no store has been migrated by a binary carrying this.

## The decision

The live-row predicate gains a second clause:

```sql
m.superseded_by IS NULL AND (m.occurred_until IS NULL OR m.occurred_until > now())
```

It is written once, as the `live!()` macro in `src/adapters/postgres/mod.rs`, and every reader that
means "what the store holds now" takes it.

Taking it: `SEARCH_LIVE` and `SEARCH_RRF_LIVE`, `RECENT_LIVE`, the digest's five memory subqueries,
`GRAPH_NEIGHBOURS_SQL` inside its history arm, the dedupe probe in `neighbours`, both halves of
`conflicts`, `UNDATED_SQL`, the four live counters in `staleness`, `find_exact`, `stale`,
`list_for_export`, the `live` and `above_open` counts in `NAMESPACE_SUMMARY_SQL`, and all four
candidate statements in the cleanup adapter (`EXACT_DUPLICATES_SQL`, `SIMILAR_PAIRS_SQL`,
`NEWEST_SQL`, `TAGGED_DATED_SQL`, `UNREAD_SQL`).

Left with the link test alone, each for its own reason:

- `SEARCH_ALL` and `SEARCH_RRF_ALL` read history by request, so they filter on neither clock.
- `SEARCH_AS_OF` and `SEARCH_RRF_AS_OF` ask about an instant. `now()` would answer a different
  question and would hide exactly the row the read exists to find.
- `RETIRED_SQL` gained an arm instead. An expired row is on that page by definition, and `expired`
  says which of the two happened.
- `retired_since` is that statement, and both chain walks (`CHAIN_IDS_SQL`,
  `SUPERSESSION_HEAD_SQL`) follow links through rows that no longer hold.
- `occurred_at_compliance` counts dating discipline, which an expiry does not change.
- `SAMPLE_CONTENT_SQL` samples what search could reach at all, for the recall monitor.
- `namespace_counts` is discovery. A namespace does not stop existing because a fact in it
  expired, and that count never reaches a response as it stands.
- `SEED_ALIAS_SQL` and `SEED_TAG_SQL` build structural edges that the walk's own grant severs.
- The `retired` counter in `staleness` and `PAIR_COUNTS_SQL` count supersessions, which is the other
  axis.

A test enforces the split rather than a convention: `every_link_test_in_this_file_is_classified_live_or_history`
reads `memory.rs`'s own source and fails on any `superseded_by IS NULL` that neither carries the
period test nor appears in a named allow list with a reason. It caught `namespace_counts` the first
time it ran.

Two statements write the column with no successor. `expire` closes a live row's period and returns
the instant it wrote. `unexpire` reopens a row, guarded on that instant. `review::expire` and
`review::unexpire` run `writable_row` the way `supersede` does and clear the digest cache after.

Neither reaches the MCP tool surface or the CLI in this change. That is a later decision.

## The context that forced it

One mechanism retired a row and it needed a successor. A fact that describes a situation the world
has moved past has no successor to name: the sprint ended, the offer expired, the plan for that week
happened. The store held three answers and none of them fit.

`superseded_by` needs a replacement row, and writing one to say "this no longer holds" puts a fact in
the store whose only content is the absence of another.

`CleanupKind::Stale` deletes, "only because there is nothing for its rows to supersede into"
(`src/domain/cleanup.rs`). A delete takes the text, the chain and every as-of read with it, which is
why 0011 made it a proposal a person applies rather than an act.

`occurred_until` already carried the end of a fact's validity and no live read touched it, so a row
with a closed period and no successor answered `memory_search` and refused
`memory_search(as_of: now)`. Two reads of one store disagreed about one row.

## What lost, and why

**A new `expired_at` column.** It needs a migration and it puts a second state beside a column that
already means this. Two columns for one end date is the shape where one gets written and the other
read.

**Deleting.** It is what `CleanupKind::Stale` does and nothing brings the row back. A retirement a
person can undo in one statement is a different act from one they cannot.

**Self-supersession.** `superseded_by = id` drops the row from every live read with no code change
and makes a one-row cycle. Both chain walks recurse on `m.id = c.superseded_by` and would return
sixty-four copies with `depth_capped` set, and `services::forget` refuses two-row cycles for the
same reason. It is a corruption wearing a flag.

## What it costs, accepted

**A live read carries one more boolean term.** The conjunct is stronger than the partial index
predicate from migration 005, so the planner can still prove a partial index applies and the period
test becomes a filter above the scan. Observed on the integration database, with `enable_seqscan`
off so the planner had to choose an index:

```
 Index Scan using memory_occurred_at on memory m  (cost=0.14..5.92 rows=1 width=16)
   Index Cond: ((tenant_id = 'default'::text) AND (namespace = 'global'::text))
   Filter: ((occurred_until IS NULL) OR (occurred_until > now()))
```

`memory_occurred_at` carries the same `WHERE superseded_by IS NULL` predicate as `memory_live`, and
the planner picked it over `memory_live` on a one-row table. What the plan settles is the claim that
matters: a partial index on that predicate is still usable under the conjunct, and the period test
does not enter the index condition. No timing was measured and none is claimed.

The opposite shape is the trap the adapter already records. `($n OR superseded_by IS NULL)` is
weaker than the index predicate, so the planner cannot prove the index applies and loses it on a
store where history outweighs live rows.

**The vector arm now carries a range comparison ahead of its LIMIT**, which is the shape migration
003 exists for: a filter that discards candidates inside the arm makes an ordered index scan return
fewer rows than the limit asks for, and `hnsw.iterative_scan = strict_order` is what makes pgvector
resume the scan instead of handing back a short answer. The as-of pair has run under that setting
since 0008 with a strictly harder filter, two comparisons against a bound instant rather than one
against `now()` over a column that is NULL on nearly every row.

**The column has two writers now, and one consequence followed.** `RETIRE_PREDECESSOR_SQL` writes
`occurred_until` with `superseded_by` in one UPDATE; `EXPIRE_SQL` writes it alone. `forget`'s revive
clears the column on every row it brings back, on the premise that a supersession put it there, and
a second writer would have broken that premise: expire a row, supersede it, delete the successor
with a revive, and the row comes back with its expiry wiped. `write::validate_supersedes_target`
now refuses a target whose period is already closed, which restores the premise and makes the
refusal the honest one anyway. A closed fact takes no successor; the owner brings it back first or
writes the new fact on its own.

**One existing shape of row changes its answer, and the change is right.** A restore that could not
relink a successor leaves a row carrying `superseded_at` and `occurred_until` with a NULL link. It
was already absent from the as-of reads, which read the period; now it is absent from the live reads
too, which is the two halves of the store agreeing about a fact that ended. What it must not become
is "expired", because it was replaced and the replacement is what went missing. That is why the
state is defined as `superseded_at IS NULL AND occurred_until IS NOT NULL AND occurred_until <=
now()` and spelled that way in all three places that name it: `RETIRED_SQL`, `console::data::Entry`
and `write::validate_supersedes_target`. The orphan keeps its replace form, takes a supersession,
and lists as a retirement whose successor is gone.

An imported archive carrying `occurred_until` with no supersession at all reads as expired, which is
the correct answer to a period somebody closed.

**An expired row is neither live nor retired, and every surface says so in its own words.**
`expire` writes no `superseded_at`, because that column means a successor retired this row and
writing it made the console print "replaced by a row that has since been deleted" about a fact
nothing had replaced. `staleness` counts the row in neither half. The rail's per-namespace line
keeps two numbers and counts it in neither, which the SQL comment states: a third number would cost
more width than the state is worth, and the retired page lists those rows by name. The fact page
reads "Expired", strikes the claim through, dates the closure and offers no replace form. The
retired page carries the row with "expired" and the day.

**`Memory::is_live` answers both clocks now.** It used to test the link alone, which let an expired
row be named as the successor in a supersession: a live fact retired into one no live read returns.
The state the console prints and this predicate are different questions, and the code says so. A row
a supersession retired does not hold now and is retired rather than expired.
`RETIRE_PREDECESSOR_SQL` carries the same refusal inside the statement as defence in depth, spelled
as the state test rather than as `occurred_until IS NULL`, so the orphan above is still replaceable.

**A recorded ruling is reversed.** `valid_time_reaches_a_search_result_without_touching_the_search`
asserted that no live statement carries a period predicate. The test is split: the live half now
asserts the conjunct and the as-of half keeps every assertion it had.

## What it is not for

**It is not a delete.** The row keeps its text, its chain, its history and every as-of read inside
its period. `forget` is still the only path that removes a row.

**It is not a deprecation of `superseded_by`.** A fact that a later fact replaced is a supersession
and goes on retiring through that path. This is the case where nothing replaced it.

**It is not a capability.** Expiring takes the write grant at the row's own level and no flag.
`may_delete` guards loss nothing brings back, and an expire reverses in one statement.

## The reversal condition

If a live read is measured slower on a store where the term does not apply, the readers split into
two statement variants the way `SEARCH_LIVE` and `SEARCH_ALL` already do, and the period test moves
to the variant that needs it.
