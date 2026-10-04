# Phase 7. Valid time

Written 20 August 2026. Implementation plan for decision
[`0008-valid-time`](../decisions/0008-valid-time.md), phase 1.

**Since implemented, phase 2 included.** [Decision 0008](../decisions/0008-valid-time.md) now reads
"Accepted, implemented, including the phase 2 this record deferred." Migration
`20260821000015_backfill_occurred_at.sql` and later ones carry the schema this plan specifies, and
`occurred_at` reaches every layer named below. What follows is the plan as it was written, before any
of it had run; read it as the design record, not as the current state of the code.

## Status of the decision this implements

`0008-valid-time.md` is headed "Accepted, phase 1 implemented". In this working tree the string
`occurred_at` appears in one file, that decision record. No migration, no domain type, no port, no
adapter, no service mentions it, and the latest migration is `20260820000010_ingest_capability.sql`.
Two researchers found this independently. Either the work lives somewhere this tree cannot see, or
the header overstates it. This spec assumes the second and is the phase 1 implementation plan. It
also amends 0008 in two places rather than implementing it verbatim; both amendments are named in §3
with the evidence that forced them, and both need a line in the decision record before code lands.

Nothing below has been run. Every claim about current behaviour comes from reading the files named;
every claim about new behaviour is a design target until the gates in §11 report.

## 0.5 What the review changed

A blind reviewer read this document with no access to the repository, and six panel reviewers read
it with full access and in isolation from each other. Seven reviews. The resolutions below override
the sections they name.

**D1, the near-now fence. Adopted, unanimous.** A write path refuses an `occurred_at` inside
`WRITE_MIN_OCCURRED_AGE_SECS` (default 86400) of now. The reasoning that settled it: a near-now
valid time duplicates `created_at` by construction, so refusing it destroys no information at all,
and the writes it refuses are exactly the ones where the column is redundant. Three corrections came
with it, and each one matters more than the fence.

- **The fence sits at the MCP tool boundary only.** The ingest fill in §8 is exempt. `observed_at`
  is a transcript timestamp, and a same-day conversation would trip a naive fence and lose the one
  fill that justifies this phase.
- **The refusal says "omit `occurred_at`", never "use an older date".** Told the second thing, a
  model backdates past the window, which destroys the detectability §11's compliance query depends
  on and converts a visible refusal into silent corruption.
- **It replaces `WRITE_MAX_FUTURE_OCCURRED_SECS` rather than joining it.** One bound, one refusal,
  one setting. §4's separate future-date check and its test are deleted.
- **Amended: the future case has its own message.** The bound and the setting stay
  single. A future `occurred_at` now gets a refusal that says the date is in the future, because
  the near-now wording misdescribed it. Both refusals still say "omit `occurred_at`". The "one
  refusal" sentence above is superseded on that point.

**D2, the registry stop-loss. Adopted, unanimous.** `registry.rs:109-121` upserts with
`DO UPDATE SET value = EXCLUDED.value ... version = registry.version + 1`, so only a counter
survives a change and the previous value is gone. A superseded memory row is hidden and still
present; a replaced registry value is destroyed. The irreversible loss is in the component this
phase was not going to touch.

Ships beside this phase as its own migration: one append-only history table and one INSERT in the
upsert. No query surface, no range type, no exclusion constraint. The full design stays deferred,
which is what §9's argument was actually about. This does not alter the memory migration in §2.

**D3, out-of-order supersession. Split four ways, resolved as two changes.** Two reviewers held the
plan's flag-and-proceed, two held refusal, one proposed accepting the write while refusing the link,
and one proposed splitting by strictness. What reconciled them is a fact from the source rather than
an opinion: flag-and-proceed still writes `superseded_by`, so approving a July proposal after an
August one hides the August row and serves July content as current truth, with a warn log as the
only trace and no un-supersede path anywhere in this plan.

- **`approve_all` orders its batch by `observed_at` before approving.** This removes the trigger for
  almost every case on the 222-row queue, and it dissolves the batch-stalling objection that the
  flag-and-proceed side rested on.
- **A strict inversion is refused.** When `successor.occurred_at < predecessor.occurred_at`, the
  write is refused naming both dates. Equality and the undated case keep the flag path. Refusal is
  slow; proceeding is unrepairable, and this plan ships no repair.

**Four defects the blind reviewer found, all fixed below.**

1. **§7's own example cannot be built.** "Since March" has no RFC 3339 form, so the model must invent
   a day, an hour and an offset that the same description forbids. The rule now: a date-only form
   (`2026-03-01`) is accepted and read as midnight UTC, and a bare month is not representable and
   must be omitted. Say that in the argument description rather than leaving a builder to guess.
2. **Risk 2's mitigation is in no step.** §11 puts the compliance query in `lumberroom stats` and §10's
   steps never build it. It becomes a numbered step, or the mitigation for the second-ranked risk is
   prose.
3. **§3's open question is built as if closed.** D3 above closes it. Step 2 implements the two
   changes named there.
4. **§8 conflates the two clocks one level up, and this is the sharpest finding in the review.**
   `min(observed_at)` is when the owner was observed stating a fact, not when the fact held. A July
   transcript saying "we moved in June" stores July. That is this phase's own error repeated inside
   the fix for it. It is honest as an upper bound and it must be labelled one: §12 gains it as a
   ranked risk, and the ingest fill's comment says plainly that it stores when the fact was said
   rather than when it became true.

**One cut.** `occurred_recency_weight` ships as a validated setting that no SQL reads, so an operator
who sets it sees nothing happen. It lands in the commit that adds the SQL reading it. The formula
stays here as prose.

**Two additions to §12.** The `min(observed_at)` conflation above, and the future-date hole: a
predecessor holding a legal future `occurred_at` against an undated successor falls through S2 to
`created_at` and produces an inverted period the ordering guard cannot see, surfacing as a raw
`memory_valid_period_check` violation at approve time.

## 1. What the prior art settled

Four researchers read prior art independently: R1 on Zep/Graphiti, R2 on bitemporal theory and
Postgres, R3 on agent memory systems, R4 on temporal retrieval benchmarks.

**A caveat covering most of what follows.** All four reached their web sources through WebFetch,
which relays pages through a summarising model. R1 calls its Graphiti quotations "high-confidence
transcription, not bytes I diffed". R3 says the same of mem0. R4 caught the summariser returning two
different numbers for one table cell across two passes. R3's agentmemory findings are the exception,
read from source on this machine. Treat the direction of every relayed finding as sound and any
specific number as needing a second look before it is quoted elsewhere.

### Where they agree

**Two columns beside the transaction clock is the standard shape.** Graphiti's `EntityEdge` carries
nullable `valid_at` and `invalid_at` beside a required `created_at` and an `expired_at` (R1,
`graphiti_core/edges.py`). SQL:2011 expresses valid time as an application-time period over two
ordinary columns, kept separate from system-versioned tables (R2, the SQL:2011 Wikipedia page; R2
did not open the ISO standard). mem0 exposes an add-time `timestamp`/`event_date` separate from its
`created_at` audit field (R3, docs.mem0.ai). Nothing argues for a different shape.

**Nobody has published a number attributable to bi-temporality alone.** Zep's DMR 94.8% and
LongMemEval 71.2% are whole-system figures, and R1 verified there is no ablation isolating the two
timelines. This backs 0008's line that valid time is not a benchmark fix.

**The correction-versus-change gap is industry-wide.** Graphiti routes every contradiction through
`edge.invalid_at = resolved_edge.valid_at`, so a fact that was never true still gets a valid
interval (R1). agentmemory's two supersession paths both resolve on "newer wins" with no correction
concept (R3, read from source). mem0's UPDATE overwrites content, and reconstruction means replaying
a history log (R3, relayed). 0008's escape hatch is more than any of the three ships.

**Retrieval benchmarks cannot see the payoff.** LongMemEval's temporal-reasoning category needs the
dated session surfaced; the date arithmetic happens in the reader, which retrieval metrics never
score (R4, arXiv 2410.10813v2). lumberroom's recall_any@20 is 98.4% already (R4, from `VERIFY.md`).

### Where they conflict

**A standing preference: dated or timeless?** 0008 says a preference has no date, so NULL means
timeless. Graphiti's prompt says the opposite: "If the fact is ongoing (present tense), set
`valid_at` to the timestamp of the episode the fact originates from" (R1,
`prompts/extract_edges.py`). Under Graphiti a preference is true-from-when-heard and stays inside
the temporal machinery; under 0008 it sits outside. Costs nothing in phase 1 and everything at
as-of time. Resolved in §3, rule N2.

**LLM date extraction: refused or fenced?** 0008 rejects it flatly. Graphiti ships a narrower thing:
a small model, one fact at a time, anchored to a caller-supplied `reference_time`, told not to infer
dates from unrelated events, writing NULL on any parse failure (R1). R1 reads 0008's rejection as
stated more broadly than the evidence supports. R4 supplies the counterweight: LongMemEval's own
time-range extraction helps recall by 6.8% at session level, and the paper warns that a weak
extractor hallucinates ranges, which for a hard filter removes the right answer rather than
mis-ranking it. Resolved in §7: no extraction in phase 1, and the line drawn is between inventing a
date from content and carrying one that arrived as metadata.

**Whether an undated successor should end its predecessor's validity.** 0008 says yes, Graphiti says
no, and the reason each is right for its own system is rule S3 in §3.

### What lumberroom should copy

**The ordering guard, from Graphiti.** Invalidation fires only when both facts are dated and
`old.valid_at < new.valid_at` (R1, `utils/maintenance/edge_operations.py`). A late-arriving old fact
cannot retire a newer one. lumberroom's 222 queued proposals are exactly this case: July facts arriving
after August ones in one backfill. agentmemory shipped the opposite rule, "older `createdAt` loses",
which R3 read from source. Rule S4 in §3.

**Half-open intervals, from SQL:2011 and Postgres.** Start inclusive, end exclusive, so consecutive
periods sharing an instant tile the timeline once with no gap and no double cover (R2, the PG16
range-types page for the `[)` canonical form; the reasoning is R2's own). Rule I1 in §3.

**The nullable pair rather than a range type, from R2's verdict.** Defended in §2.

**Nothing from mem0's `reference_date`, yet.** R3 reads it as a precedent for shipping as-of as an
additive search parameter rather than a storage overhaul. Phase 2 evidence, relayed rather than
byte-verified, recorded in §9 and not acted on. **One recommendation is already satisfied**: R1
suggests adding a transaction-time end column now rather than retrofitting it, and migration
`20260819000005` already added `superseded_at`. Prior art confirms an existing choice.

## 2. The schema

One migration, additive, nullable, no backfill.

```sql
-- migrations/20260821000011_valid_time.sql
--
-- Valid time beside transaction time. `created_at` says when this store learned the fact;
-- these two say when the fact held in the world. They are never conflated again.
--
-- Nullable with no default and no backfill. Filling these from created_at would write the
-- conflation this migration exists to end into every row that predates it, and a NULL that
-- says "unknown" is worth more than a date that is wrong.

ALTER TABLE memory ADD COLUMN IF NOT EXISTS occurred_at    timestamptz;
ALTER TABLE memory ADD COLUMN IF NOT EXISTS occurred_until  timestamptz;

-- The one invariant a scalar pair cannot get for free. A range constructor rejects an inverted
-- period at write time; two columns accept it silently, and every as-of predicate downstream
-- would then be reading a period that ends before it starts.
ALTER TABLE memory DROP CONSTRAINT IF EXISTS memory_valid_period_check;
ALTER TABLE memory ADD CONSTRAINT memory_valid_period_check
  CHECK (occurred_at IS NULL OR occurred_until IS NULL OR occurred_at <= occurred_until);

-- Live rows in one namespace by when the fact became true: the index a range filter or an
-- occurred_at tiebreak would use. Partial on the same predicate as `memory_live` from
-- migration 005 so the two stay consistent. NULLS LAST so a dated fact beats an undated one
-- under the "newest fact first" ordering.
CREATE INDEX IF NOT EXISTS memory_occurred_at
  ON memory (tenant_id, namespace, occurred_at DESC NULLS LAST)
  WHERE superseded_by IS NULL;

-- "Rows with no known start", which the ingest fill, the supersession backfill and `lumberroom
-- review` all ask for. Partial, so it stays the size of the gap rather than the table.
CREATE INDEX IF NOT EXISTS memory_no_occurred_at
  ON memory (tenant_id, created_at)
  WHERE occurred_at IS NULL AND superseded_by IS NULL;
```

Column by column:

| column | type | null | default | meaning |
| --- | --- | --- | --- | --- |
| `occurred_at` | `timestamptz` | yes | none | Inclusive start of the period the fact held. NULL means no known start. |
| `occurred_until` | `timestamptz` | yes | none | Exclusive end of that period. NULL means still holding, or no known end. |

### Pair of scalars, not `tstzrange`

R2's verdict is the pair for the memory table, so this plan adopts it with R2's reasons rather than
against them. There is nothing to defend against: R2 reserves range types for a future registry
history and reaches the same conclusion here. The argument, restated so an implementer does not have
to go back to the research:

- The pair forecloses nothing. `tstzrange(occurred_at, occurred_until, '[)')` is a pure expression
  over the two columns, so a GiST expression index, `&&`, `@>`, `-|-` and eventually PG18's
  `WITHOUT OVERLAPS` are reachable later with no data migration (R2, from the PG16 range-types and
  btree_gist pages; PG16 is what `docker-compose.yml:30` runs). It also matches SQL:2011's
  application-time period, which is two ordinary columns.
- The one thing a range type buys here is an exclusion constraint, which needs a key naming the
  thing that cannot be two values at once. `memory` has `namespace` and free-text `content` and no
  such key; unscoped, `EXCLUDE USING GIST (during WITH &&)` would forbid the owner from holding two
  facts at once. No class of bug it could catch exists on this table. R2 makes this the load-bearing
  half of the verdict and it is convincing.
- The registry is where a range type earns its keep, because `UNIQUE (tenant_id, namespace, kind,
  key)` is the key an exclusion constraint wants. Out of scope; see §9.

What the pair costs: no range operators without a constructor at every reference, and a four-branch
NULL-aware as-of predicate somebody will get wrong once. The `CHECK` buys back the invariant.

### What the migration does to existing data

**The 6 existing memory rows.** Both columns land NULL. Those rows came from hand writes and the
acceptance gates, and `created_at` genuinely is the only clock anybody knows for them; writing it
into `occurred_at` would assert a fact about the world from a fact about the store. Under N1 a NULL
start is admitted by every live read, so the six behave exactly as they do today. The row count
comes from the task brief and 0008; this plan is barred from querying the database.

**The 222 queued ingest proposals.** Untouched by the migration. They are rows in `ingest_proposal`,
not in `memory`, and they become memory rows only when the owner approves one and
`services::ingest::approve` calls `services::write::run`. They gain dates at approve time from
`ingest_proposal_source.observed_at`, already populated and already carried end to end
(`src/ports/ingest.rs:73`, `src/adapters/postgres/ingest.rs:72,273,337`). §8 has the mechanism.
Proposals approved before this ships land with NULL and need a manual pass, so ship the code before
approving the queue.

**Forward-only migrations.** `sqlx` embeds migrations at compile time and applies them at boot, so
once a binary carrying this migration has run against the store, an older image cannot boot against
it. Three consequences:

1. Schema and code ship in one deploy. No window where the columns exist and no binary reads them,
   and none where a rolled-back binary meets a migrated store.
2. Rollback is forward-only too. If the columns turn out wrong, the fix is migration 012.
3. `ADD COLUMN` with no default and no `NOT NULL` returns immediately in PG11 and later. The two
   `CREATE INDEX` statements are not `CONCURRENTLY` and block writes for their duration, which at
   six rows is microseconds. Revisit against a store with real volume.

## 3. The semantics

Rules an implementer cannot misread, numbered so a comment or a test name can cite one.

**I1. Half-open, `[occurred_at, occurred_until)`.** The start instant is inside the period, the end
instant outside it. A fact with `occurred_at = T` held at `T`. A fact with `occurred_until = T` did
not hold at `T`.

The reason is contiguity. Predecessor ending at `T` and successor starting at `T` tile the timeline
once. Under closed intervals `T` belongs to two rows, a point query at `T` returns two contradictory
answers, and nothing errors. Ending the predecessor one tick early needs a successor function, and
`timestamptz` is continuous, so any tick bakes a resolution into the data.

Every as-of predicate written in this codebase, now or in phase 2, has one shape:

```sql
(occurred_at    IS NULL OR occurred_at    <= $t)
AND (occurred_until IS NULL OR occurred_until >  $t)
```

`<=` on the left, `>` on the right. Writing `>=` on the right is the bug this rule prevents, and it
is silent.

**N1. `occurred_at IS NULL` means no known start, and a live read treats the fact as having always
held.** One meaning, decided here, because one nullable column cannot carry two.

R2 is right that NULL will do two jobs in practice: 0008 intends "timeless" for a preference, and
ingest or a manual write will also produce "the date exists in the world and this store does not
know it". They want different treatment in an as-of query, where a timeless fact should match every
`t` and an unknown-start fact arguably should not. The resolution is not a second column. Guarantee
the ingest path always fills `occurred_at` (§8), so "unknown" stops occurring on the rows the
feature was built for, and accept that the residual NULLs are preferences and hand-written facts
where "always held" is the reading the owner wants. Phase 2 inherits this. If it turns out wrong,
the fix is a third state and a migration, not a reinterpretation.

**N2. A standing preference keeps a NULL `occurred_at`.** 0008's position, kept against Graphiti's
opposite rule, because the alternative asks the write path to invent a date from the arrival clock
for a fact that has none, and its only payoff arrives in phase 2. Record in 0008 that this is a live
disagreement with the closest prior art and that the cost lands at as-of time.

**N3. `occurred_until IS NULL` means the fact still holds, or its end is unknown.** Also not
distinguished, and it matters less: both mean "do not tell anybody this fact has stopped". A live
read does not filter on `occurred_until` at all in phase 1 (§5).

**S1. Supersession sets the predecessor's `occurred_until`, never the predecessor's
`occurred_at`.** A change ends a period; it does not move its start. Rewriting a predecessor's start
is a correction, and corrections are S5.

**S2. The value written is `COALESCE(successor.occurred_at, successor.created_at)`.** This amends
0008, which says a successor with no `occurred_at` sets its predecessor's
`occurred_until` to the successor's `created_at`, full stop. R1 and R2 independently found the same
defect: on a backfill the successor's `created_at` is the day the backfill ran while the fact
changed in July, so the rule writes transaction time into a valid-time column, on exactly the rows
that motivated the decision. The `COALESCE` puts valid time first.

The second arm is an admission of ignorance, not a measurement, and goes in the decision record and
in a comment beside the SQL in those words. What makes it rare is §8: ingest fills `occurred_at`
before a proposal reaches the write path, so an ingested successor is never the undated case.

**S3. Contiguity is a consequence of the live filter, and that dependency is recorded.** lumberroom hides
retired rows behind `superseded_by IS NULL`, so a retired row with a NULL `occurred_until` would be
invisible *and* unbounded. Graphiti leaves the same field NULL and is right to, because its default
read returns retired edges with their dates and lets the reader reconcile (R1). If lumberroom's live
filter ever changes, S2 becomes the wrong rule. Say so in 0008.

**S4. The ordering guard. A successor whose `occurred_at` precedes its predecessor's `occurred_at`
does not set `occurred_until`, and the write is flagged.**

Copied from Graphiti, which fires invalidation only when both facts are dated and `old.valid_at <
new.valid_at` (R1). The case is live: 222 proposals spanning a week with Codex sessions reaching
back to July, approved in whatever order the owner works the queue. A July fact approved after an
August one would otherwise retire the August truth and stamp July into its `occurred_until`,
producing a period that ends before it starts. The `CHECK` in §2 would turn that into a constraint
violation, which is better than storing it and worse than not attempting it.

Four cases:

- Both dated, `successor.occurred_at > predecessor.occurred_at`: normal change, write
  `occurred_until = successor.occurred_at`.
- Both dated, `successor.occurred_at <= predecessor.occurred_at`: the successor states an older or
  simultaneous fact. Write the `superseded_by` link as usual, leave `occurred_until` NULL, log at
  `warn` with both ids and dates, tell the caller. The link is correct; only the valid-time end is
  unknowable from arrival order.
- Successor undated: S2's `COALESCE` falls to `created_at` and the guard cannot fire.
- Predecessor undated, successor dated: write `occurred_until = successor.occurred_at`. The
  predecessor gains an end and keeps its unknown start.

**Open question, named rather than decided.** Whether the second case should refuse the write
instead of proceeding with a NULL end. Refusing is safer for the timeline and worse for an owner
working a 222-row queue, where it stops a batch on a row that is out of order rather than wrong.
This plan proceeds and flags. Revisit after the first real backfill; §12 risk 1.

**S5. Correction stays a caller-supplied override, and phase 1 ships no way to invoke it.** 0008
makes change the default and correction the known limitation, with `occurred_until` settable at
supersede time, which is more than Graphiti, agentmemory or mem0 offer (R1, R3). Phase 1 implements
the storage and the default and adds no `correction: true` argument, for §7's reason: a model
asserting "this was never true" is a stronger claim than "this changed", and no evidence says a
model gets it right. The escape hatch that does exist: an owner who knows a fact was never true sets
`occurred_until = occurred_at` on the predecessor by hand, which under I1 is an empty period.
Document that idiom in the runbook. Do not build a tool for it yet.

**S6. A fact whose valid time ends with no successor is legal and stays live.** `occurred_until`
set, `superseded_by` NULL. The contract expired, the trial ended, nothing replaced the fact. Phase 1
stores it, returns it from every live read, and does not filter on it. The alternative is a fact
that vanishes from search on a date, which is silent forgetting, and this system's worst failure is
answering "nothing is known" about something it holds. Rendering surfaces may print "until 3 March
2026" beside the content; the retrieval path does not act on it.

**S7. `superseded_at` stays transaction time.** It records when the store learned the fact had been
replaced, it is set with `now()`, and nothing in this phase may read it as a valid-time end.
Migration 005 plus the `superseded_by IS NULL` current-row marker already amount to a hand-rolled
system-time period, so this phase adds the application-time half and the result is a bitemporal
table in SQL:2011's sense (R2). Worth one sentence in 0008: it reframes phase 2's policy question as
the same question system-versioned tables answer with current-row constraints.

## 4. The write path

`services::write::run` has an ordered sequence of checks, labelled (a0), (a) through (h) in the
file. Valid time enters at exactly one place and interacts with two of them.

### Signature

```rust
pub async fn run(
    ctx: &Ctx,
    content: &str,
    namespace: &str,
    tags: Option<Vec<String>>,
    supersedes: Option<&str>,
    sensitivity: Option<&str>,
    occurred_at: Option<DateTime<Utc>>,     // new
) -> Result<WriteOutcome>
```

One new parameter, not two. A write declaring its own end is a fact that arrives already expired,
and there is no use case in phase 1, so `occurred_until` is written by supersession alone. Adding it
later is additive.

Callers to update: `src/mcp/mod.rs`, `src/services/ingest.rs:347`, the console and CLI write paths,
and every test calling `write::run` positionally, `tests/integration.rs:592` among them.

### Where it enters, and what it does not touch

- **(a0) credentials refusal, (a) level, (b) ceiling, (c) grant, (d) tripwire.** Untouched. A date
  is not content, does not move a classification, cannot match a tripwire pattern, and is not passed
  to `tripwire::scan`.
- **Validation, new, placed after (d) and before (h).** Refuse an `occurred_at` more than
  `WRITE_MAX_FUTURE_OCCURRED_SECS` in the future. Bitemporal theory allows future valid time, but a
  write path receiving a future date is far more often clock skew or a parse error than a claim
  about the future, and a future-dated row sorts ahead of every real fact under any `occurred_at
  DESC` ordering. Default 86400 seconds: absorbs timezone mistakes, refuses year-2087. No lower
  bound; a fact from 1994 is legitimate. A setting, because every setting is in `config.rs`.
- **(h) `supersedes` validation.** Unchanged in what it validates. Guard S4 needs the predecessor's
  `occurred_at`, and `validate_supersedes` already fetches the target with `find_by_id`, so the
  value is in hand once `Memory` carries the column. Return the target `Memory` rather than its id.
- **(e) exact-duplicate collapse, (f) dedupe bands.** Untouched. Decisions, not omissions; below.
- **(g) encryption.** Untouched. Both timestamps are metadata and stay in plaintext columns for a
  private row, as `created_at`, `tags` and `namespace` already do. A date is not content. Say so in
  a comment, because the seam in that file is defended hard and a reviewer will ask.
- **Insert.** `NewMemory` gains `occurred_at`. The `INSERT` at `memory.rs:711` gains one column and
  one bind. `occurred_until` is not in the insert; a new row starts unbounded.
- **Supersession.** `supersede(&self, tenant, old, new, until: Option<DateTime<Utc>>)`, where
  `until` is what S2 computed in the service. Computing it in the service keeps the rule in one
  readable place and the adapter free of policy. The `UPDATE` at `memory.rs:1111` becomes `SET
  superseded_by = $3, superseded_at = now(), occurred_until = COALESCE(occurred_until, $4)`. The
  `COALESCE` on the column matters: a predecessor that already carried an end keeps it. The same
  change lands on the mirrored `UPDATE` inside `insert` at `memory.rs:754`.

### Should dedupe and the conflict bands consider `occurred_at`?

No, for both, and the reasoning is worth writing down because both are tempting.

**Exact-duplicate collapse (e)** matches on `(tenant, namespace, content)` against live rows and
confirms rather than inserting, so today a second write of identical content at a different date
collapses and loses its date. Adding `occurred_at` to the match key would store both rows, which is
worse: the common producer of identical content at two dates is an agent restating a standing fact
across sessions, the repetition-is-confirmation case this branch serves, and splitting it by date
refills the digest with the same sentence. The rarer episodic case is better served by content that
names the time, which the `memory_write` description already asks for.

One adjustment lands: on a collapse where the incoming write carries an *earlier* `occurred_at`,
move the stored row's date earlier, because the fact held from at least that point. A one-line
`LEAST` in the `confirm` statement, turning a lossy collapse into a conservative one. A stored NULL
stays NULL: N1 already means "has always held", earlier than any date.

**The dedupe and conflict bands (f)** run over embeddings. `occurred_at` is not in the embedding and
should not be. They are a similarity question and stay one.

One temptation belongs to a later phase: `possible_conflicts` carrying each candidate's
`occurred_at`, so a model deciding whether to supersede sees which fact is older in world terms.
Good idea, changes the published `ConflictCandidate` type, not in phase 1.

## 5. The read path

The most constrained section, because three mechanisms here are pinned by tests that read SQL as
text.

### What changes

**The `select_memory!` macro** (`memory.rs:73-99`) gains both columns. It is the column list every
read of `memory` selects, and `memory_from_row` reads columns by name and panics on one a query
forgot, so both go into the macro and into `memory_from_row`.

**Six hand-written column lists that mirror that macro** gain the same two columns. Missing one is a
runtime panic, not a compile error: `search_sql!`'s final `SELECT` (`memory.rs:184-186`),
`recent_sql!` (`:307-309`), `DIGEST_SQL`'s profile (`:392-394`), project_context (`:406-408`) and
recent (`:420-422`) subqueries, and `supersession_head` (`:1155-1157`). Those six cannot use the
macro because they sit inside a larger statement. A unit test asserting each one mentions
`occurred_at` is cheap and belongs in the same change.

**The types.** `domain::types::Memory` gains two `Option<DateTime<Utc>>` fields, both
`#[serde(skip_serializing_if = "Option::is_none")]`. `ports::NewMemory` gains `occurred_at`.
`MemoryRepository::supersede` gains the `until` parameter. `services::search::Hit` and
`services::bootstrap::Fact` each gain two `Option<String>` fields, skipped when None, RFC 3339 like
`created_at` already is.

The digest carrying them is deliberate: it is where a model reads standing facts, and "true since
March" is more useful there than in a search result. The cache is untouched, since `cache_key`
(`bootstrap.rs:235`) keys on client, project, ceilings and budget. Entries built before the deploy
expire on `BOOTSTRAP_CACHE_MS` like any other, and `write::run` already calls `clear_cache()`.

### What does not change

**The `SEARCH_LIVE` predicate and the four compiled search variants.** No new predicate, no new bind
parameter, no `$15`. This is the decision that most shapes phase 1 and it needs its argument stated.

0008's phase 1 scope includes "range filters over live rows", which would mean `memory_search`
gaining `occurred_before` and `occurred_after` compiled into the search SQL. Three things argue
against it landing now:

1. `the_linear_blend_gains_no_window_function_and_no_new_parameter` (`memory.rs:1618`) pins the
   shipped statement at thirteen parameters and asserts `$14` does not appear. The test is not the
   obstacle; it is doing its job. The obstacle is what it protects: the comment above `search_sql!`
   explains that the vector arm's plan is what pgvector's iterative scan depends on, and migration
   003 exists because a filtered vector search silently returned zero rows. Two more filters inside
   the vector arm, ahead of its `LIMIT`, is the shape of the thing that broke.
2. The rank-fusion experiment is in flight. A search-SQL change beside it makes both harder to read.
3. R4's evidence points the other way. The only time-aware retrieval intervention LongMemEval's
   authors measured is a hard time-range filter built by an LLM parsing the query, and the paper
   warns a hallucinated range removes the correct candidate rather than mis-ranking it. lumberroom will
   not ship LLM query parsing (§7), so the caller supplies the range by hand, and no evidence says
   a model would.

Phase 1 is storage, write path, supersession semantics, ingest fill and display. Range filters move
to phase 2 alongside as-of: both need the same NULL-aware predicate and the same policy answer.

**The digest's seven grant-filtered subqueries.** Three gain two columns in their select lists. None
gains or loses a `JOIN reachable rg`. The test at `memory.rs:1535` counts seven and still counts
seven. Say so in the commit message; a reviewer seeing `DIGEST_SQL` change will check that number.

**The bootstrap cache.** Untouched, per above.

**`find_exact`, `neighbours`, `conflicts`, `stale`, `list_for_export`.** `find_exact` and the export
go through `select_memory!` and gain the columns for free. `neighbours` and `conflicts` return
`ConflictCandidate` and `ConflictPair`, unchanged in phase 1.

**The `hnsw.iterative_scan` settings from migration 003.** Untouched. §2's new partial index does
not compete with the HNSW index; they serve different queries, and phase 1 runs nothing on a hot
path that uses the new one. It ships now because adding an index later is the expensive direction.

### The wire contract

`tests/integration.rs:588`, `published_payloads_keep_their_field_names`, pins the exact key set of a
`memory_write` outcome and of a search hit. Both new fields are `skip_serializing_if =
"Option::is_none"`, so an undated row publishes exactly today's keys and the existing assertions
pass unchanged. The same change adds a second case: write a row *with* an `occurred_at`, search for
it, assert the key set gains `occurred_at` and nothing else. Both keys are snake_case.

`crates/lumberroom/tests/wire.rs` reads the same payloads from the client side. Extend its key-set
assertions rather than relaxing them.

## 6. Ranking

**No recency term in phase 1. The weight exists and is zero.**

`SearchConfig` gains `occurred_recency_weight: f64` from `SEARCH_OCCURRED_RECENCY_WEIGHT`, default
`0.0`, validated at boot. No SQL reads it. It exists so the experiment is a config change rather
than a code change, and so `config.rs` stays the one place a setting is defined.

The formula, recorded now so the experiment starts from a stated hypothesis:

```
occurred_boost = exp(-ln(2) * age_days / HALF_LIFE_DAYS)   where age_days = (now - occurred_at)/1 day
                 0                                          where occurred_at IS NULL
```

Additive under the linear blend, in the usage boost's bracket and multiplied by the weight, so the
term is bounded by the weight. Multiplicative under RRF, for the reason the existing comment gives:
an RRF score is about 1/61 and an additive 0.05 would sort results by the boost. `HALF_LIFE_DAYS`
starts at 180 and is a guess.

The NULL arm scoring zero is the arguable choice. Under N1 a NULL fact has always held, so scoring
it maximally stale contradicts the rule, and scoring it maximally fresh puts every preference above
every dated fact. Zero makes the term reranking-only among dated rows, the smallest intervention.
The experiment has to settle both this and the half-life.

**Why not now.** Four independent reasons, any one of which would do.

*Confounding.* The rank-fusion experiment is running concurrently. `SEARCH_FUSION` selects between
two blends with different score scales, and `VERIFY.md` traces the NDCG@10 and MRR gap of -4.9 to a
calibration bug in the linear blend where a lexical match worth `ts_rank` 0.259 arrives as 0.091
against a cosine near 0.7. A second uncontrolled variable makes both unreadable.

*No headroom.* `recall_any@20` is 98.4% against a published 99.4% (R4, from `VERIFY.md`). A ranking
term only reshuffles rows already in the candidate set, so the ceiling here is that 4.9-point
ordering gap, whose diagnosed cause is not temporal.

*No prior art.* R4 found no decay-style recency ranking in LongMemEval; the only time-aware
intervention the paper measures is a hard filter. R4 flags its general IR knowledge about half-life
constants as belief rather than a fetched source, and this plan does not lean on it. R1 found no
ablation isolating Graphiti's temporal model. mem0 walls decay off from validity by policy, keying
it on access recency and calling it "never a filter" (R3, relayed).

*No data.* On the day this ships the six existing rows carry NULL, and a new row carries a date only
if ingest or the owner supplied one. A recency term over a mostly-NULL column measures the fill
rate, not the ranking.

**What would justify turning it on.** A store where most live rows carry `occurred_at`, the fusion
experiment concluded and its winner shipped, and a LongMemEval run with `--dates-in-text` showing
that a temporal signal in the text moves `temporal-reasoning` at all (§11). In that order.

## 7. The tool surface

### `memory_write` gains one argument

```rust
/// When this fact became true in the world, RFC 3339. Set it ONLY when the user stated the
/// time: "since March", "we moved to Postgres 16 in June", "as of last Tuesday". Never infer
/// a date from context and never use today's date because today is when you heard it: the
/// store already records that separately. Omit it whenever the user did not say.
#[serde(default)]
pub occurred_at: Option<String>,
```

Parsed with `DateTime::parse_from_rfc3339`. A parse failure is a refusal naming the format, never a
silent NULL, which would leave a model believing it recorded something it did not.

### Should a model ever set it?

Yes, under that description, and the line needs stating because 0008's rejection of date extraction
reads broadly enough to forbid it.

The pattern 0008 refuses is an extractor deriving a date from content, the way the tripwire refuses
content-derived classification: a model reading "we switched last year" and writing a timestamp is a
guess stored as a fact. That refusal stands, and no model call is added anywhere in the write path.

What this argument carries is different. It is the user's own statement, relayed. A model in a
conversation where the owner said "since March" is transcribing, not inferring, and the alternative
is losing the one clock that matters. Graphiti ships the fenced version of exactly this: a small
model, one fact at a time, anchored to a caller-supplied reference time, told not to infer dates
from unrelated events, writing NULL on any parse failure (R1). R1 judges 0008's blanket rejection
stated more broadly than the evidence supports. This plan agrees on the narrow case, not the broad.

The fence is the description text, so it is written as an instruction with a named failure mode
rather than as a field label, and three of its four sentences are prohibitions. Whether models
comply is measurable; §11 says how, and §12 risk 2 says what non-compliance looks like.

The residual risk is real: a model stamping today's date on every write turns a diagnostic column
into noise, and the noise is indistinguishable from signal after the fact. That is why the write
path records `source_client` alongside, why §11 has a query separating model-supplied dates from
ingest-supplied ones, and why 0008's reversal condition should gain a second arm: a client found
stamping today loses the argument, rather than everybody losing it.

### `memory_search` gains nothing

No `as_of`, no `occurred_before`, no `occurred_after`, for §5's reasons. The description is
unchanged. One display change reaches the model without a new argument: search hits and digest facts
carry `occurred_at` when set, so a model asking "what is the Postgres port" sees the answer and,
where known, when it became true. That is the whole model-visible payoff in phase 1, and it is small.

### `context_bootstrap`, `registry_get`, `memory_forget`

Unchanged. `registry_get` is where the concept diverges: `Provenance.valid_from` is a `String`
filled with `Utc::now()` (`types.rs:137`, `services/registry.rs:222`), named for valid time and set
to transaction time, inside a jsonb blob nothing can index or compare without parsing. The exact
conflation 0008 exists to end, in the same repo. Not fixed in this phase; see §9.

## 8. Ingestion

`observed_at` already exists on `ingest_proposal_source` and already travels end to end. It stops at
the proposal. This is the shortest and highest-value part of phase 1.

### The path

One proposal can have many sources: `ingest_proposal_source` is keyed `(proposal_id, source_key)`,
so the same fact stated in eight transcripts produces eight rows each with its own nullable
`observed_at`. The fill needs an aggregation, and the aggregation is `min(observed_at)`.

Earliest observation, because `occurred_at` answers when the fact became true, and the earliest
moment the owner was observed stating it is the tightest upper bound this store has. `max` would say
"when it was last restated", which is confirmation and already has `last_confirmed_at`.

`IngestRepository::proposal` (or a companion) returns `observed_at` as `min(s.observed_at)` over
the proposal's sources; the source join at `adapters/postgres/ingest.rs:337` already orders by
`observed_at NULLS LAST`, so this is a `MIN` in the proposal query or a fold over what `show` loads.
`services::ingest::approve` (`ingest.rs:311`) passes it into `write::run`. Nothing else changes: the
refusal path, `mark_error`, the `deduplicated` path and `approve_all` batching are untouched.

**The pattern to avoid.** `ingest.rs:655` reads `fact.source.observed_at.unwrap_or_else(Utc::now)`
for the emission probe, and that default is right there: an emission after this moment cannot have
caused this span. Do not copy it. A proposal whose every source has a NULL `observed_at` gets
`occurred_at = None`, not `now()`. Defaulting to the approval clock writes transaction time into a
valid-time column, on the exact rows this phase exists for.

### The 222 already queued

Untouched by the migration and needing nothing special, provided the code ships before they are
approved. Each carries at least one source row, the transcript reader populated `observed_at` on
those rows, and approval after this change fills `occurred_at` from it.

Two checks before approving. Count proposals whose sources are all NULL, since those approve to
NULL and the owner should know how many beforehand. Read the min and max of `min(observed_at)`
across the queue: if the spread is not roughly a week with a July tail, the transcript reader is not
recording what the decision record says.

**If any were approved before this ships**, their memory rows carry NULL and nothing repairs them
automatically, because nothing re-runs the write path over an existing row. A one-off `UPDATE memory
SET occurred_at = (SELECT min(s.observed_at) ...) WHERE occurred_at IS NULL AND id IN (SELECT
memory_id FROM ingest_proposal WHERE state = 'written')` would do it. Write it only if needed.

### The extractor

Unchanged, and stated because this is where a date-guessing model would most naturally be added. The
extractor proposes content, namespace, tags and a supersession target. It does not propose a date
and this phase does not ask it to. `observed_at` is metadata from the transcript file, not a model's
reading of the text, which is why it is trustworthy enough for a valid-time column.

## 9. Out of scope

### The as-of query, deferred by 0008

`memory_search(as_of: t)` answering what held at `t` needs rows that `superseded_by IS NULL` hides.
0008 names four collisions and they are all real in this tree:

1. **The `SEARCH_LIVE` predicate is compiled in.** Four variants exist as `const &str` from macros,
   and the comment above `search_sql!` says why the predicate is a literal rather than a bound
   boolean: `superseded_by IS NULL` as written matches migration 005's `memory_live` partial index,
   and `($n OR superseded_by IS NULL)` leaves the planner unable to prove the index predicate under
   a generic plan. An as-of variant is a fifth and sixth compiled statement, not a parameter.
2. **The digest's seven grant-filtered subqueries**, whose join count `memory.rs:1535` pins. An
   as-of digest is a second digest, or seven more subqueries inside one statement whose
   single-round-trip latency budget is the reason it is not decomposed.
3. **The bootstrap cache**, keyed on client, project, ceilings and budget. An as-of digest adds `t`
   to that key, and the cache is a policy boundary (`bootstrap.rs:229-235`), so touching the key
   needs the same care as touching a grant.
4. **Dedupe compares against live rows only.** `find_exact` filters `superseded_by IS NULL` because
   collapsing a new write into a retired row would revive the fact that retirement corrected. As-of
   reads must not reach that path.

**What phase 2 needs, beyond the four.**

- **A named axis.** 0008 says `as_of` should answer "what held at `t`", which is valid time, then
  describes the mechanism as reading rows the live filter hides, which is transaction time. R2 is
  right that these are two questions, and Fowler's Sally salary case is where they come apart: what
  was the salary on 25 February, versus what did the store believe it was (R2,
  martinfowler.com/articles/bitemporal-history.html, relayed). Pick one meaning and reserve the
  other name first. Suggested: `as_of` reads the occurred pair, `believed_at` reads
  `created_at`/`superseded_at`.
- **A policy answer.** A client granted read over live rows has not obviously been granted the
  history behind them, and a retired fact can be more revealing than its replacement. That belongs
  with the two-axis grant model and has not been decided. R3 notes mem0 has no analog because it is
  single-tenant, so there is no prior art to copy.
- **A shape decision.** Graphiti gets point-in-time nearly free because it never filtered history
  out: its default read returns every edge with its dates and the reader reconciles (R1). lumberroom has
  to pay for what Graphiti got by not building. The cheap intermediate is an as-of read returning
  retired rows *with* their periods attached, letting the caller reason, rather than resolving the
  timeline server-side.
- **The NULL question from N1**, which phase 2 is the first thing to feel.

### Range filters on `memory_search`

Moved from 0008's phase 1 to phase 2, with the argument in §5.

### The registry

Out of scope, and the item most likely to be the wrong call. R2's finding:
`adapters/postgres/registry.rs:111-120` upserts with `ON CONFLICT ... DO UPDATE SET value =
EXCLUDED.value, version = registry.version + 1`. The previous value is overwritten in place, only a
counter survives, and no migration creates a registry history table. "What was the Postgres port
before I changed it" is one of 0008's three motivating questions, registry-shaped for a port held in
the registry, and for that row the old value is *gone* rather than hidden. R2's conclusion: the
registry gap is destructive while the memory gap only obscures, reversing the usual priority.

A strong argument, not acted on here, because it is different work. A registry history needs a
versioned table, and R2 is right that this is where `tstzrange` plus `btree_gist` earns its keep,
since `UNIQUE (tenant_id, namespace, kind, key)` is the key an exclusion constraint needs:

```sql
EXCLUDE USING GIST (tenant_id WITH =, namespace WITH =, kind WITH =, key WITH =, validity WITH &&)
```

That makes "one value per key at any instant" a database invariant instead of an application
convention, a class of bug the scalar pair on `memory` cannot catch and does not need to. Its own
decision record and its own migration. Bundling it here would double the surface and put a new table
under an exclusion constraint into the same deploy as a column addition.

**Recorded as owed, with a date.** Open a follow-up before this phase closes, and put the priority
argument in it in R2's words, so the next reader sees that the sequencing was chosen rather than
overlooked.

`Provenance.valid_from` also stays as it is. Renaming a `String` field named for valid time and set
to `Utc::now()` costs nothing on its own and belongs in the registry work, where the replacement is
a real column rather than a better name for a string in a jsonb blob.

### Everything else

No decay changes. No automatic contradiction detection. No `correction: true` argument. No LLM date
extraction anywhere. No as-of for the export or the Obsidian vault.

## 10. The order of work

The adapter first, then the write service, then ingest and display, then the CLI, then the
documentation and the decision record. Each step compiles on its own and the suite runs once at
the end; the gates in section 11 are what settle it.

## 11. How it will be verified

Nothing in this plan has run. What follows is what would settle each claim.

### The gates that must not regress

All four acceptance gates, against a live server, unchanged. `./scripts/correction-test.sh` bears
directly: 13 PASS today, covering that a correction does not resurface and that two texts differing
by one digit stay two rows. Both behaviours sit on the path this phase changes.

`./scripts/cargo.sh test -j 1` and `-p lumberroom`. 626 tests passed on 20 August. Check the split,
not the total: integration tests skip rather than fail with no database, so a low count is not a
pass.

### New tests, and what each one proves

`tests/integration.rs`:

1. Undated write stores NULL and searches exactly as today. Behaviour-neutral for the six rows.
2. Past-dated write round-trips through the write path and both read paths.
3. Two days future refused, one day accepted. The bound, at the value config says.
4. B supersedes A, both dated, B later: A's `occurred_until` equals B's `occurred_at`. Rule S2.
5. B supersedes A, B undated: A's `occurred_until` equals B's `created_at`. S2's second arm, and the
   test that catches a regression back to 0008's original wording.
6. B supersedes A, both dated, B *earlier*: A's `occurred_until` stays NULL, the link is written, a
   warning is logged. Rule S4. This is the 222-proposal case and the most important test here.
7. A predecessor already carrying an `occurred_until` keeps it. The `COALESCE` on the column.
8. An inverted period is refused by the database. The `CHECK` is on and spelled right.
9. `published_payloads_keep_their_field_names` gains the dated case from §5.
10. Duplicate collapse with an earlier incoming date moves the stored date earlier; a later one
    leaves it. The `LEAST`.

`tests/ingest.rs`:

11. A proposal with three sources at three timestamps approves to the earliest. The `min`.
12. A proposal whose sources all carry NULL `observed_at` approves to NULL, not the approval clock.
    Proves the `unwrap_or_else(Utc::now)` pattern was not copied.

`src/adapters/postgres/memory.rs`, unit:

13. All six hand-written column lists contain both columns.
14. The three existing pinned tests still assert what they assert today. No edits.

### The end-to-end check, on real data

The honest definition of done, and not automatable, because the test suite has no transcripts. Run
it by hand after the code ships and before the queue is approved. Approve ten proposals from the
222, spanning the range, including at least one July Codex session. Their `occurred_at` should
reproduce the transcript dates and their `created_at` should all be today. If two rows from
different weeks share an `occurred_at`, the fill is wrong and nothing else here matters.

### LongMemEval

Two runs on the session-as-document protocol that produced the numbers in `VERIFY.md`:

```bash
./scripts/eval-longmemeval.sh --type temporal-reasoning
./scripts/eval-longmemeval.sh --type temporal-reasoning --dates-in-text
```

`--dates-in-text` prepends `date: <haystack_date>` to each session before writing it
(`crates/lumberroom/src/eval/corpus.rs:216`), putting the temporal signal in the text where the
lexical and vector arms reach it. That is the only place a retrieval system can act on a date
without a filter or a ranking term.

**What the pair proves.** The delta is the ceiling on what any temporal signal can do for retrieval
here, measured before anybody migrates a schema for one. Near zero means `occurred_at` has no
retrieval headroom on this benchmark and §6's weight stays at zero indefinitely. Material means an
argument for phase 2's range filters and for revisiting §6.

**What neither run proves.** Neither measures `occurred_at`: the harness writes sessions through
`memory_write` and would have to be changed to pass a date, which is not in this phase. The
`corpus.rs` comment is explicit that prepending the date is an advantage over the published protocol
rather than a match for it, so the number is not comparable to the published 95.5%.

**Expect it to barely move**, and say so before running it. `recall_any@20` is 98.4% and the
ordering gap traces to a lexical calibration bug with nothing to do with time (R4, `VERIFY.md`). A
retrieval-only metric cannot see reader-stage date arithmetic at all (R4, LongMemEval §5.1). What
justifies the work in that case is the end-to-end check above: a store that tells the owner a fact
he stated in July and a fact he stated this morning are the same age is wrong, in a way no retrieval
metric measures. 0008 already says this and it is the argument that carries the phase.

### Whether models comply with the `occurred_at` fence

Not verifiable at ship time. After two weeks of use, one query settles it: per `source_client`,
count rows where `occurred_at IS NOT NULL` against rows where `occurred_at::date =
created_at::date`. A client whose second count approaches its first is stamping today rather than
transcribing a stated date, which is the failure §7 predicts. `lumberroom stats` is its natural home.

## 12. The risks, ranked

**1. The ordering guard is wrong or missing, and the backfill writes inverted periods.** Highest
probability, because the triggering data is already queued: 222 proposals spanning a week with a
July tail, approved in queue order rather than chronological order. In production it looks like an
approval returning a `memory_valid_period_check` violation with nothing explaining it, or worse, an
August fact's `occurred_until` silently set to a July date so a later as-of query reports the
current Postgres port stopped being true a month before it started. agentmemory shipped the
equivalent defect and R3 read it from source. Mitigated by S4 and test 6. The residual is §3's open
question about whether S4 should refuse rather than flag.

**2. Models stamp today's date on every write and the column becomes noise.** The fence is prose in
a tool description, the same mechanism `possible_conflicts` relies on, and a date is easier to
produce than a supersession decision. In production: three months in, 90% of live rows carry an
`occurred_at` equal to their `created_at`, nobody notices because both are plausible, and the
distinction the phase was built for is gone. Worse than never shipping the column, since the noise
is indistinguishable from signal afterwards. Detected by §11's query, run on a schedule rather than
once. Mitigated by revoking the argument per client.

**3. A read path is missed and panics at runtime.** `memory_from_row` reads columns by name and
panics on one a query forgot. Six hand-written lists mirror `select_memory!`, and a missed one is a
panic on a request path, not a compile error. In production: `memory_search` works and
`context_bootstrap` panics, or the reverse, depending which list was missed. Mitigated by test 13,
a string assertion that costs nothing. Low probability, high blast radius, trivially preventable.

**4. The search SQL gets touched anyway.** Step 1's agent sees `SEARCH_LIVE` and adds a filter
because 0008 says phase 1 includes range filters. The pinned tests catch the parameter count and the
`superseded_by` match count, so this surfaces as a failing test rather than an incident. Its cost is
a wasted track and a confounded fusion experiment. Mitigated by step 1's explicit instruction and by
putting §5's argument in the agent's prompt rather than only in this document.

**5. NULL turns out to mean two things after all.** N1 commits to one meaning and §8 removes the
main producer of the other. In production: an as-of query for March returns every preference the
owner has ever expressed, including ones stated in August, and he reads that as the store making
things up. Deferred rather than mitigated. The fix would be a third state and a migration.

**6. The registry keeps destroying history while memory gains it.** "What was the Postgres port
before I changed it" is one of 0008's three motivating questions, and for a port held in the
registry this phase does not answer it. In production: the owner tries the question the record
promised, gets nothing, and concludes the feature does not work. Mitigated only by saying so, in the
follow-up issue and in 0008. This plan sequences against R2's priority argument deliberately.

**7. The migration blocks a real store.** Two non-concurrent `CREATE INDEX` statements at boot. At
six rows this is nothing; against real volume it holds a write lock for the build. Migrations are
forward-only and applied at boot, so the first time it matters is a production boot that outlasts a
health check.

**8. The half-open convention gets written as closed somewhere.** `>=` instead of `>` on the
`occurred_until` side of an as-of predicate. Phase 1 writes no such predicate, so this is a phase 2
risk recorded early. In production: a point query at the changeover instant returns both the old
fact and the new one, with no error. Mitigated by I1 stating the predicate once, in one shape.
