# Phase 8. The review queue and its decide path

Written 22 September 2026. Design for one queue shape, one decide route, a remembered "both are
fine", the interactive `lumberroom review` loop over it, and the same two operations as MCP tools.
The plan sits beside this file at [`phase-8-review-queue-plan.md`](phase-8-review-queue-plan.md).

Nothing below is built. Every claim about current behaviour names the file it was read from; every
claim about new behaviour is a design target until the gates in §10 report.

## 0. Ground truth

Read on 22 September 2026 at engine `84e9ab5`.

**Three review reads, five actions, no loop, and two copies of one read.**
`services::review::queue` (`src/services/review.rs:109`) builds conflicts, stale rows and registry
entries due; nothing in `src/` calls it and only `tests/integration.rs:1671`, `:1703` and `:1716`
do. `admin_review_stale` and `admin_review_conflicts` (`src/http/mod.rs:1125-1147`, `:1157-1187`)
build the same read inline again, down to the same comment about two round trips per pair. A third
copy would be three places to keep the grant rule and the live-pair rule right, so T0 deletes
`review::queue` and its helpers and T3 puts both handlers on `review_queue::queue`. Their wire
shapes hold, so an installed CLI keeps working.

The CLI's `lumberroom review` (`crates/lumberroom/src/commands.rs:554`) calls four GET routes
(`/admin/review/{stale,conflicts,registry,dates}`, `src/http/mod.rs:113-116`) and prints. Acting
means copying an id into `lumberroom supersede`, `fill-date` or `forget`. The service holds
`confirm` (`review.rs:178`), `supersede` (`:187`), `expire` (`:291`), `unexpire` (`:325`),
`fill_date` (`:348`) and `delete` (`:383`, through `forget::by_id`). Every one of them resolves its
row through `writable_row` (`:406`), which needs read and write at the row's stored level.

**Confirming a stale row does not clear it.** `stale` (`src/adapters/postgres/memory.rs:2555`)
selects `last_accessed_at IS NULL AND created_at < now() - days`. `confirm` (`:2398`) writes
`last_confirmed_at` and nothing else. §5 fixes this. `write::run` also calls `confirm` on both
duplicate-collapse paths (`src/services/write.rs:244`, `:283`), so a client restating a fact
confirms it; §5 keeps that.

**The conflicts query takes no grant.** `MemoryRepository::conflicts(tenant, min_similarity,
limit)` (`src/ports/memory.rs:738`) runs over the whole tenant and the service re-fetches each half
through `can_read` (`review.rs:116-131`). The conflicts and stale statements are inline strings
(`memory.rs:2651`, `:2563`), unlike the nineteen `const ... SQL` statements the file's unit tests
scan. §4 adds the grant; T0 extracts both statements into constants so the new clauses get a scan.

**The conflicts read costs more than its page.** Its doc comment (`memory.rs:2644-2650`) calls the
statement a per-namespace self-join on vector distance with no index able to help, says the `LIMIT`
bounds the output and not the work, and says "it runs from `lumberroom review` by hand, never on a
request path, which is what makes the trade acceptable today". This phase puts it on a route and on
a tool, so that precondition stops holding and §4 replaces it with a bound. Two measurements, both
22 September 2026. On the owner's store, about 1,390 live rows over six namespaces,
`lumberroom review --conflicts --limit 200` returned 39 pairs at the 0.90 threshold in 4.2 s. On a
scratch database in the dev container (pgvector/pgvector:pg16, 768-dimension random vectors,
`min_similarity` 0.9), the statement took 2.48 s at 1,400 rows in one namespace, 13.33 s at 3,000
and 34.96 s at 5,000, and `LIMIT 51` cost the same as `LIMIT 2201`. Container hardware, so the
seconds do not carry to a server; the exponent does.

**Neither review order is total.** `ORDER BY similarity DESC` on conflicts (`memory.rs:2679`), with
`similarity` rounded to four places by `round4` (`:2691`), and `ORDER BY m.created_at ASC` on stale
(`:2580`). So offset paging over either repeats and skips rows with no concurrent write needed. §4
adds the tiebreak.

**A merge write never collapses.** `write::run` skips the exact and near-duplicate collapse when
`supersedes` is set (`write.rs:234`, `:273`).

**Nothing remembers a pair the owner has read.** No table or column in `migrations/` names a
dismissed pair.

**The MCP surface has no review tool.** `TOOL_CAPABILITIES` (`src/mcp/capability.rs:51-71`) lists
ten tools. `tests/mcp_capability.rs:60` pins the open set as five names. `review::expire` reaches no
route, tool or CLI command; decision 0017:61 says so and leaves it for a later decision, and this
design leaves it there. A grep for `expire` over `src/http`, `src/mcp` and `crates` does not settle
it: 63 hits, every one about OAuth token expiry or a skipped ingest file.

**`DomainError` carries no code.** `src/domain/errors.rs` has `kind`, `message` and `source`. A
transport picks the wire code per route, so a service refusal that needs its own code has no way to
say so. §2.3 adds one.

**The cleanup queue is a second proposal queue.** `cleanup_proposal` (migration 016) keeps
`lumberroom cleanup list|show|apply|reject|resolve|unreject`, and it stays there: §8 says why, and
open question 1 says what would reopen it.

**A downstream proposal source feeds this shape** through the `ProposalSource` seam in §7. The seam
is designed here; the implementation is documented where it lives.

## 1. The queue

One route, one shape, three sources.

### 1.1 The route

`GET /admin/review/queue`

| Query | Default | Range | Meaning |
|---|---|---|---|
| `source` | every source the server fills | comma list of `conflict`, `stale`, `proposal` | Which sources to read |
| `limit` | 50 | 1 to 200 | Items per source per page |
| `offset` | 0 | 0 to 2000, refused above | Items to skip, per source |
| `days` | `QUALITY.stale_days` | 0 to 36500 | The stale window |
| `min_similarity` | `QUALITY.conflict_threshold` | that threshold to 1 | The conflict floor |

Defaults come from the server's config, which ends the disagreement where the CLI asks for 90 days
and `STALE_DAYS` says 365 (`commands.rs:604`, `http/mod.rs:1139`). The response says which values
it used. A page holds up to `limit` items per filled source.

`limit` and `days` clamp. `min_similarity` floors at `QUALITY.conflict_threshold` and cannot go
below it, so a caller cannot ask the self-join to sort every pair in the namespace. `offset` past
2000 answers `400 page_too_deep` rather than silently serving page 1: a client paging a shifting
list deserves to hear that its bookmark is gone.

Paging is by offset because none of the three reads can page by keyset, and `offset` binds into the
statement rather than fetching `offset + limit + 1` and discarding rows in Rust. A decision shifts
later pages; §6.1 says how the loop copes.

### 1.2 The envelope

```json
{
  "items": [ ... ],
  "sources": { "conflict": true, "stale": true, "proposal": [] },
  "refused": { "conflict": "namespace_too_large" },
  "dismissed": 12,
  "stale_days": 365,
  "min_similarity": 0.9,
  "limit": 50,
  "offset": 0,
  "has_more": false
}
```

`sources.proposal` lists the origins this server fills; the engine answers `[]`. A client that asks
for `source=proposal` on a server answering `[]` gets `400 source_not_filled`, never an empty list:
an empty list reads as "nothing to review" and that is a different fact.

`refused` names the sources asked for that did not answer, one code each, and is absent when every
source answered. Conflicts is the expensive source and the one most likely to refuse, so a single
`Result` over all three would let it take the stale list and every proposal down with it. A refused
source is not an empty source, which is why the code travels.

`dismissed` counts the pairs the ledger hides from this caller, over the join §3.3 already writes.
Without it a blinded queue reads as a clean queue, and a `keep_both` the owner regrets is findable
only through a separate command.

`has_more` is true when any source held a row past this page. Each read asks for `limit + 1` at the
bound offset and keeps `limit`, which is also the convention §7.2 puts on a proposal source.

### 1.3 The item

```rust
// src/services/review_queue.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source { Conflict, Stale, Proposal }

/// One row as the queue shows it. Whole content, because the person deciding has to read it.
#[derive(Debug, Clone, Serialize)]
pub struct QueueRow {
    pub id: String,
    pub namespace: String,
    pub sensitivity: Sensitivity,
    pub content: String,
    /// False when this id came back in `services::decrypt`'s list of rows it could not open. From
    /// that list, never from an empty `content`: a row whose text is empty is readable.
    pub opened: bool,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    pub access_count: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_accessed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_confirmed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueItem {
    /// `conflict:<id>:<id>` in either order, `stale:<id>`, `proposal:<origin>:<id>`.
    pub key: String,
    pub source: Source,
    pub namespace: String,
    /// Conflict: `[older, newer]` by `(created_at, id)`. Stale: one row. Proposal: the members.
    pub rows: Vec<QueueRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProposalItem>,
    /// What this item takes for this caller. The CLI draws its key line from this and nothing
    /// else. Empty when the caller may not change the rows or a row did not open.
    pub verdicts: Vec<Verdict>,
}

/// One source-supplied field, in the order the source wants it read.
#[derive(Debug, Clone, Serialize)]
pub struct ProposalField {
    pub label: String,
    pub value: String,
}

/// What a proposal source says about one proposal. Every field is the source's and the engine
/// interprets none of it, so the vocabulary stays a list rather than columns this engine would
/// carry forever for one producer.
#[derive(Debug, Clone, Serialize)]
pub struct ProposalItem {
    pub id: String,
    pub origin: String,
    pub kind: String,
    /// The text an `apply` would write. Named, because it is the one field the person has to read
    /// word for word before answering `a`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed_content: Option<String>,
    /// Everything else the person needs: why the source offers this, why it offers no act, a
    /// registry entry, a second model's view. Ordered, print order being the only cost.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<ProposalField>,
    pub created_at: String,
    /// The source says which of `apply` and `dismiss` this proposal takes. Some kinds take
    /// neither act and exist to be read.
    pub verdicts: Vec<Verdict>,
}
```

Which fields each source fills, and which verdicts:

| Field | conflict | stale | proposal |
|---|---|---|---|
| `rows` | older, newer | the row | members |
| `similarity` | yes | no | no |
| `age_days` | no | yes | no |
| `proposal` | no | no | yes |
| `verdicts` | supersede, merge, keep_both, delete | confirm, merge, delete | what `proposal.verdicts` says |

`verdicts` is computed per item. For conflict and stale: empty unless the caller may read and write
every row at its stored level and every row opened; `delete` only with `may_delete`. For a proposal:
copied from `proposal.verdicts`, then emptied under the same row rule. The CLI never offers a key the
server will refuse on grant or on shape; it can still be refused on state (§2.3).

A proposal's act may write outside the member rows' namespaces and the engine cannot see where: it
holds the members and no act. So that promise rests on the source, and §7.2 makes it contractual.

Ordering: conflicts by similarity descending, stale by age descending, proposals as the source
returns them, each with the total tiebreak §4 names. Conflicts first, then stale, then proposals.

## 2. The decide path

`POST /admin/review/decide`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict { Supersede, Merge, KeepBoth, Delete, Confirm, Apply, Dismiss }

#[derive(Debug, Deserialize)]
pub struct Decision {
    pub key: String,
    pub verdict: Verdict,
    /// supersede: the row that survives. Default is the newer row by `(created_at, id)`.
    #[serde(default)]
    pub keep: Option<String>,
    /// delete: which row. Required when the item holds more than one.
    #[serde(default)]
    pub id: Option<String>,
    /// merge: the text the caller wrote. Required. Nothing in the engine writes it.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// merge: the period of the merged fact. §2.1 says what absent defaults to.
    #[serde(default)]
    pub occurred_at: Option<DateTime<Utc>>,
    /// delete: recorded on the deletion. Default "deleted from the review queue".
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decided {
    pub key: String,
    pub verdict: Verdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    /// Rows retired by this call. A proposal source fills it from its own act.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub superseded: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub end_left_open: bool,
    /// merge: rows the new row was meant to retire and did not. The write landed; retry each
    /// with `supersede` against `written`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unfinished: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub already_dismissed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_state: Option<String>,
}
```

The key is an address. The service parses the source and the ids out of it, fetches the rows and
orders a conflict pair by `(created_at, id)`, the comparison the conflicts SQL uses
(`memory.rs:2669`), so the key's spelling never decides which row is older. It does not re-run the
conflict query. A `keep_both` on two ids that no longer pair costs one ledger row, which the grant
rule in §4 keeps to callers who could supersede the pair anyway.

### 2.1 What each verdict does

| Verdict | Sources | Store effect | Reuses | New |
|---|---|---|---|---|
| `supersede` | conflict | the other id retired into `keep` | `review::supersede` | the default of newer |
| `merge` | conflict, stale | `write::run(content, namespace, tags, supersedes = newest id, sensitivity = highest of the sources, occurred_at)`, then `review::supersede(each other id, written)` | `write::run`, `review::supersede` | the two-step orchestration and `unfinished` |
| `keep_both` | conflict | one row in `memory_pair_dismissed`, carrying the client and the token fingerprint | `writable_row` on both | `MemoryRepository::dismiss_pair` |
| `delete` | conflict, stale | one row deleted, chain spliced | `review::delete`, so `forget::by_id`, so `may_delete` | nothing |
| `confirm` | stale | `last_confirmed_at = now()` | `review::confirm` | the stale predicate in §5 |
| `apply` | proposal | whatever the source does | `ProposalSource::decide` | the trait |
| `dismiss` | proposal | whatever the source does | `ProposalSource::decide` | the trait |

`skip` is not a verdict. It changes nothing in the store, so it never reaches the server; the CLI
keeps a per-run set of skipped keys. `expire` is not a verdict either: nothing reaches
`review::expire` today and this phase does not settle the decision 0017 left open.

`merge` on a conflict: `supersedes` names the newer row, so the new row's mirror points at the fact
it most directly replaces, and a second statement retires the older row. `memory.supersedes` holds
one value (`memory.rs:2086-2090`), so it records the newer source alone while both sources carry
`superseded_by = written`. That is a fan-in, not a three-link chain, and `SUBJECT_HISTORY_SQL`'s
`backward` CTE returns both sources at the same depth, which is what §10.4 expects. Every row goes
through `writable_row` before the write, so a grant refusal writes nothing. If the write lands and
the second retirement fails, the response carries `written` and names the row in `unfinished`. A
leftover that is still live reappears as a conflict against the merged row; one that failed because
it had expired (`write::validate_supersedes`, `write.rs:618`) does not, because the conflicts
statement requires an open period on both halves (`memory.rs:2673-2676`), so the CLI prints every
`unfinished` id and the person sees it once.

`occurred_at` on a merge passes `write::run`'s near-now fence like any write, and the default is
picked so it never trips it: absent, it takes the newest source's value when that clears
`POLICY.write_min_occurred_age_secs` (86,400 by default, `config.rs:112`), and otherwise takes
none. Nothing guarantees the sources cleared that fence. `run_ingest` writes under `Fence::Bypass`
(`write.rs:70-88`) and `review::fill_date` refuses only a future date (`review.rs:348-356`), so
same-day rows exist, and a blind default would refuse the merge with a message telling the caller
to omit a field they never sent, on the one verdict with no other route. An explicit value inside
the window is still refused, and that refusal names something the caller chose.

`merge` on a stale row: the same call with one source. This is the verdict for "the fact changed,
here is what it says now".

A verdict outside the item's `verdicts` list answers `400 verdict_not_for_source`, naming the source
and the list.

### 2.2 The ledger's two other routes

`GET /admin/review/dismissed?limit=` lists dismissed pairs whose two rows the caller may both read,
newest first, each as `{ "lo_id", "hi_id", "dismissed_by", "dismissed_token", "dismissed_at",
"rows": [QueueRow, QueueRow] }`. The grant runs inside the query on both rows (§3.4); a pair with
an unreadable half is absent, not partial.

`DELETE /admin/review/dismissed/{a}/{b}` removes one. Both rows go through `writable_row` first;
a missing row, an unreadable row and a missing dismissal all answer `{"removed": false}`, so the
route confirms nothing about ids the caller may not see. The cleanup queue needed `unreject` for the
same reason within a week of shipping (`src/ports/cleanup.rs`).

### 2.3 Wire codes

`DomainError` gains `code: Option<&'static str>`, `with_code(&'static str)` and `code()`, in the
shape a downstream fork already carries so a later merge takes either. The service sets
`source_not_filled`, `verdict_not_for_source`, `unknown_origin`, `not_a_queue_key`,
`namespace_too_large` and `page_too_deep` at the raise
site; `src/http/review.rs` publishes `e.code()` and falls back to `review_decide_failed`. No
transport matches on message text. Refusals on state (a proposal already decided, a supersede
target already retired) carry the code the underlying service already raises.

## 3. The dismissed-pair ledger

### 3.1 The table

`migrations/20260922000025_review_dismissed_pairs.sql`, the next number after
`20260825000024_memory_graph.sql`. `tenant_id` carries no default, as `memory_edge` does
(`memory_graph.sql:23`).

```sql
-- A "both are fine" verdict on a conflict pair. The conflicts query joins against this so a pair
-- the owner has read and kept leaves the queue. Keyed on the two ids in uuid order: the pair is
-- unordered, and a row's text never changes under its id, so the dismissal never goes stale.
CREATE TABLE IF NOT EXISTS memory_pair_dismissed (
  tenant_id       text NOT NULL,
  lo_id           uuid NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  hi_id           uuid NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  -- Two columns because one is not enough to name anybody: a deployment that puts several people
  -- behind one client writes the same `dismissed_by` for all of them. `token_id` is the
  -- fingerprint the principal already documents as safe to log.
  dismissed_by    text NOT NULL,
  dismissed_token text NOT NULL,
  dismissed_at    timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, lo_id, hi_id),
  CHECK (lo_id < hi_id)
);

-- The cascades probe each id column without a tenant, which the primary key cannot serve.
CREATE INDEX IF NOT EXISTS memory_pair_dismissed_lo ON memory_pair_dismissed (lo_id);
CREATE INDEX IF NOT EXISTS memory_pair_dismissed_hi ON memory_pair_dismissed (hi_id);
CREATE INDEX IF NOT EXISTS memory_pair_dismissed_recent
  ON memory_pair_dismissed (tenant_id, dismissed_at DESC);
```

The migration issues no `GRANT`: the engine's own role owns the schema it created. A deployment
serving this table through a second, confined role has to grant on it, and §3.3 puts the ledger in
the conflicts statement unconditionally, so a missing grant removes the conflict source rather than
degrading it. That work belongs to whoever runs that deployment.

### 3.2 What a dismissal means, and when it ends

The person read both rows and decided they are two facts. Not "skip for now", which is the CLI's
business, and not a supersession.

It never ends on its own. Three things end it: `DELETE /admin/review/dismissed/{a}/{b}`; the
deletion of either row (cascade); the retirement of either row, after which the pair fails the live
test in the conflicts query and the ledger row is dead weight the next delete clears. A successor
is a new id and gets a fresh comparison.

### 3.3 The join

`conflicts` gains one clause beside its live and grant tests:

```sql
AND NOT EXISTS (
      SELECT 1 FROM memory_pair_dismissed d
       WHERE d.tenant_id = a.tenant_id
         AND d.lo_id = least(a.id, b.id)
         AND d.hi_id = greatest(a.id, b.id)
    )
```

Inside the query rather than a pass over results, for the reason the grant is: a pass after `LIMIT`
returns short pages and calls them full.

### 3.4 The port

Five methods on `MemoryRepository`, beside `confirm`. Not a new repository: `Repos` is built in
`src/main.rs` and five test files.

```rust
/// One row in the ledger. False when the pair was already there. Either id order.
async fn dismiss_pair(&self, tenant: &str, a: uuid::Uuid, b: uuid::Uuid, by: &str, token: &str) -> Result<bool>;
/// False when there was nothing to remove. The service checks the grant on both rows first.
async fn undismiss_pair(&self, tenant: &str, a: uuid::Uuid, b: uuid::Uuid) -> Result<bool>;
/// Newest first. `reader` runs inside the query on both rows' namespaces, as `stale` does.
async fn dismissed_pairs(&self, tenant: &str, limit: i64, reader: &[NamespaceGrant]) -> Result<Vec<DismissedPair>>;
/// For the envelope's `dismissed`. Counted in the query, so it never names an id to say how many.
async fn dismissed_count(&self, tenant: &str, reader: &[NamespaceGrant]) -> Result<i64>;
/// Live embedded rows per readable namespace, highest first. The conflicts self-join runs per
/// namespace, so the largest one bounds the work and the whole-tenant total does not.
async fn live_embedded_counts(&self, tenant: &str, reader: &[NamespaceGrant]) -> Result<Vec<(String, i64)>>;
```

```rust
#[derive(Debug, Clone, Serialize)]
pub struct DismissedPair {
    pub lo_id: uuid::Uuid,
    pub hi_id: uuid::Uuid,
    pub dismissed_by: String,
    pub dismissed_token: String,
    pub dismissed_at: DateTime<Utc>,
}
```

## 4. Grants and sensitivity

The rule the engine runs on: the sensitivity filter runs inside the query, never as a pass over
results (`CONTRIBUTING.md`, "A grant has two axes"). A batch route breaks it in a way a single-row
route cannot: a page of twenty pairs filtered to the two this caller may read has told the caller
that eighteen more exist.

**`conflicts` takes the caller's read grant, and an offset.**

```rust
async fn conflicts(&self, tenant: &str, min_similarity: f64, limit: i64, offset: i64, reader: &[NamespaceGrant]) -> Result<Vec<ConflictPair>>;
async fn stale(&self, tenant: &str, older_than_days: i32, limit: i64, offset: i64, reader: &[NamespaceGrant]) -> Result<Vec<Memory>>;
```

The SQL gains two `EXISTS ... unnest($4::text[], $5::bool[], $6::text[])` blocks, one per half, in
the shape `stale` already uses through `adapters::postgres::cleanup::grant_arrays`
(`cleanup.rs:336`, already `pub(crate)`). An empty grant returns an empty page before the query
runs.

**Both orders become total.** `ORDER BY similarity DESC, a.created_at, a.id, b.id` on conflicts and
`ORDER BY m.created_at ASC, m.id` on stale. `round4` makes similarity ties ordinary rather than
theoretical, and the whole offset scheme rests on a sort the statements did not provide.

**The conflicts read is bounded before it runs.** `live_embedded_counts` aggregates over
`memory_live (tenant_id, namespace) WHERE superseded_by IS NULL`
(`20260819000005_supersession_ageing.sql:13-15`), so it answers in milliseconds, and the service
refuses the conflict source with `namespace_too_large` when the largest count exceeds
`QUALITY.conflict_scan_max`. `CONFLICT_SCAN_MAX` defaults to 2000, validated at boot like every
other setting, from the dev-container probe in §0 where one namespace of 3,000 rows cost 13.33 s.
Recalibrate it from §10's timed run before trusting it on server hardware.

A small `limit` is no substitute, since the probe measured `LIMIT 51` and `LIMIT 2201` at the same
34.96 s, so the refusal is the honest version of what a slow caller was going to get anyway. Two
costs, stated rather than hidden. A namespace past the ceiling gets no conflicts review at all
until someone builds the per-row nearest-neighbour probe the adapter's doc comment names. And the
count and the join are two statements, so a store that crosses the ceiling between them runs one
join over it. A deployment sharing one pool across tenants should read §0's seconds against its
pool size: ten concurrent conflicts calls on a pool of ten hold every connection for the length of
the statement, and the callers who suffer asked for something else.

**Each half is still re-fetched by id.** `ConflictPair` carries no sensitivity and no ciphertext,
so the service fetches both rows through `find_by_id`, checks `can_read`, and decrypts. A row that
changed between the two statements drops the pair whole. A row that will not decrypt stays, with
`opened: false` and an empty `verdicts`: an unreadable row is a review item in its own right
(`review.rs:446-447`), and nobody supersedes text they could not read.

**Stale, the ledger listing and a proposal source** run the grant inside their own query.

**The decide path needs read and write on every row.** `writable_row` (`review.rs:406`) becomes
`pub(super)` and `review_queue` calls it, so the queue is not a laxer second path to any mutation.
That covers `keep_both`: the ledger hides a pair from every reader of the tenant, which is the
authority a supersede carries and a read grant does not. `supersede` runs `write::validate_supersedes`
as `review::supersede` does; `delete` runs `forget::by_id`, so `may_delete` is checked where it
always was.

`keep_both` is the one verdict whose purpose is to stop the store reporting something, and the
ledger is the only place this queue forgets on purpose, so it pays three times: the same write
check as every other verdict, both halves of §3.1's attribution, and §1.2's count printed beside
the tally. A dismissal nobody can attribute and nobody can count is a suppression primitive.

**A proposal source filters its own rows.** `ProposalSource::pending` returns only proposals whose
namespace and every member the caller may read; a proposal that fails on any member drops whole.
The engine re-checks each member through `can_read` and drops the item whole on a miss, then
decrypts the members as it does the other two sources.

**Staleness numbers** stay behind `reads_whole_store` and off this route.

## 5. Confirm clears a stale row

`stale` gains one clause, with `days` bound as an integer:

```sql
AND m.created_at < now() - make_interval(days => $2)
AND (m.last_confirmed_at IS NULL
     OR m.last_confirmed_at < now() - make_interval(days => greatest($2, 1)))
```

A confirmed row leaves the list for one window and comes back for re-confirmation. The column's own
migration says only that a restating write sets it and that repetition is confirmation
(`migrations/20260819000005_supersession_ageing.sql:24-26`); it says nothing about a window or a
return, so the return is this phase's choice and decision 0018 carries it with the reversal in §9. A
fact nobody has retrieved in a year is worth re-asking whatever last year's answer was, and a
confirmation that hid a row forever would turn one keypress into a permanent exemption. The window
floors at one
day so `--days 0`, which asks for every never-read row, still hides a row confirmed today; without
the floor the clause reduces to `last_confirmed_at < now()` and a confirmation changes nothing.

`write::run` confirms a row when a client restates it (`write.rs:244`, `:283`). That stays: a
restated fact is a fact somebody still holds, and the column's own migration describes it as set by
a restating write. `staleness()`'s `never_retrieved` keeps reading `last_accessed_at` alone.

## 6. The CLI

`lumberroom review` becomes the loop. `--dates` and `--registry` keep their current behaviour;
`--stale` and `--conflicts` become aliases for `--source stale` and `--source conflict`.

### 6.1 The interactive loop

```
lumberroom review [--source conflict,stale,proposal] [--limit N] [--days N] [--min-similarity X]
```

Reads one page and walks it. Per item it prints a header, the rows in full, a proposal's
`proposed_content` and `fields` as `label: value` lines, and a key line drawn from the item's
`verdicts`. The header carries the page position and, when the ledger holds anything for this
caller, the `dismissed` count:

```
[3/39] conflict  0.931  user:me  (12 dismissed)
  older  9f1c2b4e  2026-08-19  read 4x
         Aditya prefers Opus for reviews and Sonnet for implementation.
  newer  1a7d0c3f  2026-09-04  read 0x
         Reviewer and planner run on Opus; implementer, content writer and frontend run on Sonnet.
  s keep newer   o keep older   m merge   k keep both   d delete   n skip   q quit
>
```

| Key | Verdict | Then |
|---|---|---|
| `s` | supersede, keep newer | acts |
| `o` | supersede, keep older | acts |
| `m` | merge | prompts `merged text (blank line ends it):` and reads lines until a blank one; nothing typed aborts to the key line |
| `k` | keep_both | acts |
| `d` | delete | on a pair, prompts `delete which? (o/n):`; then `type the first 8 characters of <id> to confirm:` |
| `c` | confirm | stale only |
| `a` | apply | proposal only |
| `x` | dismiss | proposal only |
| `n` | none | remembers the key for this run |
| `q` | none | prints the tally and exits 0 |

An item with empty `verdicts` prints its rows, `(read only)` and the `n q` line. The key line reads
one line and takes it only when, trimmed, it is exactly one character and that character is in the
item's list. Anything else prints the line again and decides nothing. After every decision the CLI prints the server's one-line answer
(`retired 9f1c2b4e into 1a7d0c3f`, `wrote 2b7c... and retired 2`, `wrote 2b7c... and retired 1;
unfinished: 9f1c2b4e`, `kept both`, `deleted 9f1c2b4e`, `confirmed`, `applied`, `dismissed`). A
refusal prints the server's `detail` and returns to the key line.

Paging: the loop starts at offset 0. When a page holds nothing but skipped keys and `has_more` is
true, it reads the next offset; skipped items do not move, so that page is stable. When a page holds
nothing but skipped keys and `has_more` is false, or holds nothing, the loop ends with the tally:
`39 read: 22 superseded, 3 merged, 6 kept, 2 skipped, 12 dismissed in the ledger`. After a decision
it re-reads at offset 0, because a decision shifts every later page. That is one conflicts read per
decision, and §4's ceiling is what makes the repetition affordable.

Delete confirmation follows `forget`'s rule (`commands.rs:527`): the thing worth reading twice is
typed back. `forget` asks for the count; a single row's count is always 1, so this asks for the id.

Prompts read stdin without a terminal check, as `forget` does, so a piped answer file drives the
loop, and that is the trap the two rules above close. One stream feeds the merged text and every
later keypress, so a merge reading one line of a two-line paste would leave the second line to
arrive as the next item's answer, and the next item might take `d`. Reading until a blank line
consumes the whole paste; refusing a key line longer than one character makes a stray line redraw.

### 6.2 Without a prompt

| Flag | Body sent |
|---|---|
| `--json [--offset N]` | none; prints the §1.2 envelope as pretty JSON and exits |
| `--supersede <old>,<new>` | `{"key":"conflict:<old>:<new>","verdict":"supersede","keep":"<new>"}` |
| `--merge <a>,<b> --content "..." [--occurred-at <rfc3339>]` | `{"key":"conflict:<a>:<b>","verdict":"merge","content":"..."}` |
| `--merge <id> --content "..."` | `{"key":"stale:<id>","verdict":"merge","content":"..."}` |
| `--keep-both <a>,<b>` | `{"key":"conflict:<a>:<b>","verdict":"keep_both"}` |
| `--confirm <id>` | `{"key":"stale:<id>","verdict":"confirm"}` |
| `--delete <id>` | `{"key":"stale:<id>","verdict":"delete","id":"<id>"}`, after the typed confirmation, or without it under `--yes` |
| `--apply <origin>:<id>` | `{"key":"proposal:<origin>:<id>","verdict":"apply"}` |
| `--dismiss <origin>:<id>` | `{"key":"proposal:<origin>:<id>","verdict":"dismiss"}` |
| `--dismissed` | none; lists the ledger |
| `--undismiss <a>,<b>` | `DELETE /admin/review/dismissed/<a>/<b>` |

One action flag per invocation; two answer a usage error. `--merge` without `--content` is a usage
error naming decision 4: the text comes from you. Each prints the same one-line answer the loop
prints, or `--json` prints the `Decided` body.

This table is the throughput answer, so §8's "no batch decide" bounds the wire and not how fast a
backlog clears. `--json` carries every item's `key` and `rows[].id`, and every flag here acts
without prompting, so a shell loop decides a whole page unattended. `--delete` takes `--yes`.

`lumberroom supersede <old> <new>` stays as it is.

### 6.3 Against a server that fills no proposals

`sources.proposal` is `[]`, so proposal keys never appear and the loop never draws `a` or `x`.
`--source proposal`, `--apply` and `--dismiss` print the server's `source_not_filled` detail and
exit 1. Against a server older than this route, `/admin/review/queue` answers 404 and the CLI says
`this server has no review queue route; upgrade it`.

### 6.4 Wire types

`crates/lumberroom/src/wire.rs` gains `Source`, `Verdict`, `ReviewQueue`, `ReviewSources`,
`ReviewItem`, `ReviewRow`, `ReviewProposal`, `ReviewProposalField`, `Decided` and `DismissedPair`
for responses and `DecisionRequest` for the request, each doc-commented with the server symbol it
mirrors and carrying the same `snake_case` serde renames. Responses carry only what the loop prints
or branches on, that file's own rule (`wire.rs:8-11`), and requests stay exact.
`tests/fixtures/review_queue.json`, `review_decide.json` and `review_dismissed.json` pin them in
`crates/lumberroom/tests/wire.rs`.

## 7. The proposal seam and the MCP tools

### 7.1 Where each part sits

| Part | Side |
|---|---|
| Queue shape, decide path, ledger, migration 025, stale fix, grant-in-query conflicts, the scan bound, error code | upstream |
| `ProposalSource` trait and `AppState.proposals: Vec<Arc<dyn ProposalSource>>` | upstream |
| CLI loop, flags, wire types | upstream, decision 3: no CLI divergence |
| MCP tools `review_queue`, `review_decide` | upstream, stage 2 |
| Any `ProposalSource` implementation | whoever has proposals; the engine ships none |

### 7.2 The trait

Declared in `src/services/review_queue.rs` rather than `src/ports/`, because `decide` needs a
`Ctx`, the reason `SealedReader` sits in `services/mod.rs`.

```rust
#[async_trait]
pub trait ProposalSource: Send + Sync {
    /// The word after `proposal:` in every key this source owns. Unique across the sources wired.
    fn origin(&self) -> &'static str;

    /// Pending proposals the caller may read, whole or not at all, newest first, each with the
    /// member rows it names. Return up to `limit + 1`: the engine keeps `limit` and reads the
    /// extra one as "there is more", the only way it answers `has_more` without a count.
    async fn pending(&self, ctx: &Ctx, limit: i64, offset: i64) -> Result<Vec<(ProposalItem, Vec<Memory>)>>;

    /// `Apply` or `Dismiss`, with whatever gate the source keeps. Anything else is refused before
    /// this is called.
    async fn decide(&self, ctx: &Ctx, id: &str, verdict: Verdict) -> Result<ProposalDecided>;
}

#[derive(Debug, Clone, Serialize)]
pub struct ProposalDecided {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    /// Rows the act retired. Empty when it retired none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub superseded: Vec<String>,
}
```

`AppState.proposals` is a `Vec`, and the key carries the origin, so two producers coexist and a
second one is wiring rather than a shape change. `sources.proposal` lists the origins wired. A key
whose origin no source claims answers `400 unknown_origin`.

The contract a source keeps, and the acceptance suite tests through a fixture source in
`tests/review_queue.rs`: `pending` returns nothing the caller may not read whole and at most
`limit + 1`; `verdicts` on each item says what it takes, checked against every namespace the act
would write and not only the member rows, because the engine holds the members and no act; `decide`
refuses a verdict the item did not offer with its own code; the engine re-checks members, decrypts,
and empties `verdicts` on an unopened member.

`CannedProposals` in that suite is a test double. Five tests exercise the contract through it, and
none is evidence that a shipped implementation runs, because the engine ships none.

### 7.3 Stage 2, the MCP tools

Two tools in `src/mcp/review_tools.rs`, on their own `#[tool_router(router = review_tool_router)]`
added in `Lumberroom::new` the way `extra_tool_router` is (`src/mcp/mod.rs:179`).

`review_queue` takes `{ source?: [..], limit?, offset?, days?, min_similarity? }` and returns the
§1.2 envelope as structured content, with the text `render` prints. `review_decide` takes the §2
`Decision` and returns `Decided`.

`render` prints whole row content and whatever text a source supplied, so everything it takes from
a row or a `ProposalItem` goes inside a delimited block labelled as data. A model reading the queue
is reading text somebody else wrote, with `review_decide` in the same session.

Both at `Capability::Open`, and this is the whole of what that opens: `supersede` and `merge` are
`memory_write` with `supersedes`; `confirm` is what `memory_write` does on a restated fact;
`delete` runs `forget::by_id` and reads `may_delete`; `apply` and `dismiss` are the source's own
gate. `keep_both` is the one mutation new to the tool surface, and it needs read and write on both
rows, the authority a supersede of the same pair needs. `expire` is not on the tool. `TOOL_CAPABILITIES`
gains the two rows; `tests/mcp_capability.rs`'s `OPEN_TOOLS` grows to seven; `docs/permissions.md`'s
table gains two rows, and `tests/permissions_doc.rs` gains a scan that every `TOOL_CAPABILITIES`
name appears in that table.

The description of `review_decide` says when to call it: on an item the person has read with you,
never unprompted. That sentence instructs the model and is never evidence about one, and the
`unprompted` column on `tool_calls` is no better. `Invocation::parse` answers `Model` for an absent
or unrecognised header (`src/domain/types.rs:296-309`), the MCP layer defaults to `Model` when the
extension is missing (`src/mcp/mod.rs:377-379`), and no MCP client in the tree sends
`x-memory-invocation`, so `review_decide`, which lives on the MCP surface alone, records
`unprompted = true` on every call. The column marks the surface, not the turn, and §9 reverses on a
field the caller cannot declare.

## 8. What stays out

- The cleanup queue keeps its commands. The shape takes a second `ProposalSource` with origin
  `cleanup`; the queue's content does not, which is why this is open question 1 and not a task. The
  in-process pass runs hourly by default (`config.rs:907`) and queues exact matches at 1.0 and
  paraphrases from 0.97 (`services/cleanup.rs:57`, `:313-390`), while the conflict source reads
  from 0.90 (`config.rs:913`) through a self-join that anti-joins nothing. Fold it in and every
  pair at 0.97 or above arrives twice, once as `conflict:<a>:<b>` and once as
  `proposal:cleanup:<id>`, and §3.3's join covers conflicts alone, so dismissing one leaves the
  other. The pass spends a `HashSet` and five lines of comment avoiding that
  (`services/cleanup.rs:315-319`), and stale has the same overlap under a daily run. Two costs
  beside it: `ProposalSource::decide` takes `Apply` and `Dismiss` while cleanup also has `resolve`
  and `unreject` (`:785`, `:867`), and `cleanup` is not optional in `AppState`, so a filled
  `sources.proposal` on every engine deletes the subject of
  `source_proposal_on_an_engine_answers_400_source_not_filled`.
- Registry entries due and undated rows keep `--registry` and `--dates`.
- No console page. Decision 0006 put the ingest queue in the console; this queue is the CLI's until
  someone asks.
- `merge` reads lines until a blank one, so a pasted paragraph lands whole. No `$EDITOR`. A
  `--content-file` flag is one afternoon if needed.
- No `mayReview` capability. Open question 2.
- No batch decide. One key, one verdict, one call. §6.2 is where throughput comes from.
- No `expire` on the loop or the tool. Decision 0017:61.
- No undo for `supersede` beyond what exists (`forget` on the successor revives under the grant,
  decision 0013).

## 9. Reversal condition

Reverse the unified shape if, after thirty days, `lumberroom review` is not what the owner reaches
for. Shell history is the only evidence: `tool_calls` is written from the MCP path alone
(`src/mcp/mod.rs:397`) and the four review routes record nothing, so stage 1 sits on neither side
of a comparison against the commands it replaces. Read it at thirty days against the same window
before the change.

Reverse the ledger if it holds more than ten pairs the owner later wanted back, measured from
`DELETE /admin/review/dismissed` calls.

Reverse §5's one-window return if the same row comes back and gets confirmed again with no new
information more than twice.

Reverse the open capability on `review_decide` when a `client` outside the owner's named interactive
clients appears against it in `tool_calls`, or when the owner finds a decide nobody asked for.
`client` is minted at authentication and no caller declares it, which is the part the `unprompted`
column could not manage.

Reverse §4's scan bound, up or down, on a conflicts read refused over the ceiling or a
`statement_timeout` kill on the conflicts statement, in any week. Either means the number came from
the wrong hardware.

## 10. How it will be verified

The lead runs the three gate commands `CONTRIBUTING.md` names, which the plan's W1 lists. The
integration suite skips rather than fails without a database, so a run is a pass only when
`tests/review_queue.rs` reports its own count and no `skipping` line.

The manual gate runs on a local engine restored from an archive export of the owner's store, never
against a live deployment. Discovery on production is reads only.

1. The two paths agree. Against the pre-change binary on that copy, `lumberroom review --conflicts
   --limit 200` gives a count C and a set of ids at a recorded `min_similarity`, C = 39 at 0.90 on
   22 September 2026. Then `GET /admin/review/queue?source=conflict&limit=200` at the same floor
   returns the same C pairs by id, and `sources.proposal` is `[]`. A C of 200 means the clamp binds
   and the comparison has no denominator; raise it and run again first.
2. `--keep-both <a>,<b>` on one pair, then `--json`: one fewer item, `dismissed` up by one;
   `--dismissed` lists the pair with the client and the token fingerprint;
   `--undismiss <a>,<b>` then `--json`: the pair is back and `dismissed` is down by one.
3. The loop, answering `s` to every pair: the tally reads `N read: N superseded`, and `--json`
   reports 0 items. N is whatever the store holds; the loop re-reads from offset 0 and each
   supersede retires a live row, so the count falls whatever C was.
4. `--merge <a>,<b> --content "..."` on a fresh copy: `--json` shows no pair holding either id, and
   `lumberroom history <written>` shows both sources retired into it at the same depth.
5. A row written with a backdated `created_at` on the copy: `--json --days 0` lists it; `--confirm`
   removes it from the next `--json --days 0`. The return after the window is the integration
   test's, which backdates `last_confirmed_at`.
6. One timed run against the bound. Seed a scratch namespace to `CONFLICT_SCAN_MAX` rows, time
   `--source conflict` and print the seconds, then seed one row past the ceiling and confirm the
   refusal reads `namespace_too_large` in milliseconds. The archive copy cannot show this: its
   largest namespace is well under the ceiling, the shape where the query is fast.

Every count above is a design target until the run prints it. §0's two figures are measurements and
say where they came from.

## 11. The decision record

Decision `0018-one-review-queue.md`, filed by the lead in T0 in the shape `0001` sets. The decision:
one queue shape with a source field, one decide route, a dismissed-pair ledger, and a proposal seam
the engine leaves empty. What lost: a `review` subcommand per source; a stored queue table for
conflicts (drifts from the store it describes); a `skip` verdict remembered server-side (a skip is
not a judgement); a `mayReview` flag; `expire` on the loop (decision 0017 owns that); folding the
cleanup queue in now (§8 says why). Costs accepted: two round trips per pair, an offset cursor, a
per-item verdict computation, one namespace count before every conflicts read, a conflict source
that refuses past the ceiling rather than answering slowly, §5's confirmation window, and a seam the
engine publishes and does not implement. Not for: proposals the engine produces, which stay on
`cleanup`. Reversal: §9.

## Open questions

1. **The cleanup queue.** Fold `cleanup_proposal` in as a source with origin `cleanup`? Default:
   not in this phase, on the double-listing in §8. It reopens when one of two things is true: the
   conflicts and stale reads carry an anti-join against the members of pending cleanup proposals,
   or the cleanup pass is retired.
2. **A capability for `review_decide`.** Default: none, on §7.3's verdict-by-verdict argument. Yes
   means a config field, a migration in the `oauth_client` grant shape (as
   `20260821000013_history_capability.sql` did), `docs/permissions.md` and the console's grant
   editor. No telemetry catches an instruction injected inside the owner's own client: §9's trigger
   sees a new client, not a hijacked turn, and no column separates the two. A flag the owner sets is
   the only thing that would, and it is what to build first if the store holds a second person's
   write grant.
