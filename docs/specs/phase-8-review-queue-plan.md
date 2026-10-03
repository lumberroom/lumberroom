# Phase 8. The review queue, the order of work

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. The lead
> orchestrates; subagents implement under absolute file ownership. Steps use `- [ ]` for tracking.

**Goal:** one queue shape and one decide route, an interactive `lumberroom review` over it, and the
same two operations as MCP tools, with a proposal seam the engine leaves empty.

**Architecture:** `services::review_queue` owns the shape, the key grammar and the decide
orchestration, calling the actions `services::review`, `services::write` and `services::forget`
already hold. `AppState.proposals: Vec<Arc<dyn ProposalSource>>` is the one seam. SQL changes sit in
`src/adapters/postgres/memory.rs` and one migration. T0 also retires `services::review::queue`,
which nothing in `src/` calls, and puts the two inline copies of that read on the new service.

**Tech stack:** Rust, axum, sqlx without macros, rmcp, the hand-written CLI arg parser.

**Spec:** [`phase-8-review-queue.md`](phase-8-review-queue.md). Executors read both.

## Global constraints

- No em dashes in any file. `grep -rP '\x{2014}' <file>` prints nothing.
- Comments and doc comments: two or three lines at most, why and traps only. Calibrate on
  `src/domain/policy.rs`.
- No AI attribution anywhere.
- Subagents never run git, never commit, never run `cargo test`. `./scripts/cargo.sh check` is
  allowed; errors in files you do not own are expected mid-flight.
- Every setting in `src/config.rs`. This phase adds `CONFLICT_SCAN_MAX`, and T0 is the only task
  that touches that file.
- `sqlx::query`/`query_as` with `.bind()`, literal column lists.
- Domain and services never import from adapters except `adapters::auth` and `crypto`.
- Migration `20260922000025_review_dismissed_pairs.sql` is the only migration. Forward only.
- Wire names are exactly the spec's. A paraphrased field is a runtime failure.
- T0 is the whole contract. No later task adds a type, a port method, a column, a config field, a
  wire field or a CLI flag. A task that finds one missing stops and returns it under `wire_in`.
- Never write "verified", "works" or "tested" about code that has not run. Write "implemented" and
  name the gate.

## CONTEXT block, pasted into every subagent prompt

```
You are working in the lumberroom engine, in the shared tree at the repository root. Never create
a worktree. Never run git. Never commit. Never run cargo test; the suite truncates a shared
database. You may run ./scripts/cargo.sh check --all-targets and grep for your own files in its
output; errors elsewhere belong to other tracks.

You own exactly the files your task lists and touch nothing else. If you need a change in another
file, or a type, port method, column, config field, wire field or CLI flag the lock does not
carry, stop and return it under wire_in as an exact diff. Never add one yourself.

Ground truth: read docs/specs/phase-8-review-queue.md first. The port signatures, wire shapes and
key grammar in it are locked; do not rename a field. services::review holds confirm, supersede,
delete and pub(super) writable_row. write::run with supersedes set skips duplicate collapse and
fences an occurred_at inside the last WRITE_MIN_OCCURRED_AGE_SECS. forget::by_id checks may_delete.
DomainError carries with_code and code().

Prose rules, enforced on code, comments, test names and your return text: no em dashes (U+2014).
Active voice, a human subject. No adverbs where a plain verb works. No "Note that", no "Here's
what", no "not X, it's Y". Comments say why and flag traps in two or three lines; they never
narrate what the code does. Test names are sentences in snake_case that state the property.
No AI attribution anywhere.

Nothing here is built until the lead runs the gate. In your return say "implemented", never
"verified" or "works", and label a number either an observation with its source or a design
target. Execute your own code where you can: cargo check does not compile #[cfg(test)] blocks.

Return exactly this JSON at the end:
{ "files_written": [...], "wire_in": "<exact edits needed in files you do not own, or none>",
  "tests_added": [...], "not_done": [...], "open_risks": [...] }
```

## Stages and tasks

| Id | Stage | Purpose | Owner | Tier | Depends on |
|---|---|---|---|---|---|
| T0 | 1 | Interface lock, one commit | lead | opus | none |
| T1 | 1 | SQL: ledger, stale predicate, conflicts join | agent | opus | T0 |
| T2 | 1 | Service: queue, key grammar, decide | agent | opus | T0 |
| T3 | 1 | HTTP: the four routes | agent | opus | T0 |
| T4 | 1 | CLI: loop, flags, wire types, fixtures | agent | sonnet | T0 |
| T5 | 1 | Acceptance suite `tests/review_queue.rs` | agent | opus | T0 |
| T6 | 1 | Docs: managing, README, CHANGELOG | agent | sonnet | T0 |
| W1 | 1 | Wiring pass and gates | lead | opus | T1 to T6 |
| T7 | 2 | MCP tools and capability rows | agent | opus | W1 |
| T8 | 2 | permissions.md rows and tool text in docs | agent | sonnet | W1 |
| W2 | 2 | Wiring pass, doc scan test, gates | lead | opus | T7, T8 |

Stage 3, a downstream proposal source, is planned where that source lives.

File ownership is disjoint by construction. Shared composition files (`src/http/mod.rs`,
`src/mcp/mod.rs`, `src/main.rs`, `src/services/mod.rs`, `src/domain/errors.rs`, `Cargo.toml`,
`migrations/`, `tests/`, `crates/lumberroom/src/lib.rs`, `crates/lumberroom/src/commands.rs`) are
the lead's, in T0 and the W passes.

---

### T0: Interface lock (lead, one commit, stage 1)

**Files:**
- Create: `migrations/20260922000025_review_dismissed_pairs.sql` (spec §3.1 verbatim, including
  `dismissed_token`)
- Create: `src/services/review_queue.rs` (below)
- Create: `docs/decisions/0018-one-review-queue.md` (spec §11, shape of `0001`)
- Modify: `src/config.rs`: `QualityConfig` gains `pub conflict_scan_max: i64`, read as
  `env_num("CONFLICT_SCAN_MAX", 2_000i64)?` beside `conflict_limit` at `:914`, its doc comment
  naming spec §0's dev-container probe as where 2,000 came from. `validate` refuses anything below
  100 beside the `CONFLICT_THRESHOLD` check at `:1091`, since under a hundred rows per namespace
  the bound refuses stores the query answers in milliseconds.
- Modify: `src/domain/errors.rs`: add `code: Option<&'static str>` to `DomainError`, `new` sets
  `None`, plus
  ```rust
  pub fn with_code(mut self, code: &'static str) -> Self { self.code = Some(code); self }
  pub fn code(&self) -> Option<&'static str> { self.code }
  ```
- Modify: `src/ports/memory.rs`: `conflicts` gains `offset: i64` and `reader: &[NamespaceGrant]`,
  `stale` gains `offset: i64` after `limit`, plus the five methods and `DismissedPair` from spec
  §3.4. Every signature is in spec §3.4 and §4 and nowhere else.
- Modify: `src/ports/mod.rs` (re-export `DismissedPair`)
- Modify: `src/adapters/postgres/memory.rs`: extract the conflicts statement at `:2651` into
  `const CONFLICTS_SQL: &str` and the stale statement at `:2563` into `const STALE_SQL: &str`
  (built with the same `select_memory!`/`concat!` form as `SUBJECT_HISTORY_SQL` at `:974`);
  `conflicts` takes `offset` and `reader` and binds `grant_arrays(reader)` as `$5..$7` after
  `$3 = limit + 1` and `$4 = offset`, with the two `EXISTS` grant blocks in the shape `STALE_SQL`
  carries; `stale` binds `older_than_days` as `i32`, takes `offset` as `$4`, and its existing
  `unnest($4, $5, $6)` block renumbers to `$5..$7`; stub bodies for the five new methods returning
  `DomainError::internal("not built")`
- Modify: `src/adapters/postgres/mod.rs`: `pub(crate) use cleanup::grant_arrays;`. Three statements
  now bind the same three arrays, and a published proposal seam needs the translation reachable
  outside this module, so it is exported once here rather than written a fourth time.
- Modify: `src/services/mod.rs` (`pub mod review_queue;`)
- Modify: `src/services/review.rs`: delete `queue` (`:109`) with `ReviewQueue` (`:72`),
  `ConflictItem` (`:45`), `StaleItem` (`:55`), `Row` (`:32`), `to_row` (`:422`), `preview`
  (`:448`), `visible` (`:392`), `render` (`:460`) and the five `#[cfg(test)]` tests at `:553-590`
  that read `render` and `preview`. `writable_row` (`:406`) becomes `pub(super)`.
- Modify: `src/http/mod.rs`: the conflicts call at `:1170` takes `0` and `&ctx.principal.read`, the
  stale call at `:1140` takes `0`. T3 rewrites both handlers onto `review_queue::queue` and drops
  `conflict_side` (`:2006`), which nothing else calls; T0 does the minimum that compiles.
- Modify: `src/mcp/mod.rs` (`AppState` at `:45` gains
  `pub proposals: Vec<Arc<dyn crate::services::review_queue::ProposalSource>>`)
- Modify: `src/main.rs:134` and the five `AppState {` sites in `tests/cleanup.rs:220`,
  `tests/console_cleanup.rs:228`, `tests/console.rs:228`, `tests/mcp_capability.rs:289`,
  `tests/ingest.rs:243` (`proposals: Vec::new()`). `grep -rn "AppState {" src tests crates` returns
  those six and the definition, so the list is complete.
- Modify: `tests/integration.rs`: delete `the_stale_queue_fills_its_limit_with_rows_the_caller_may_see`
  and `the_review_queue_hands_a_narrow_grant_no_tenant_wide_row_counts` (`:1671`, `:1703`, `:1716`),
  whose subject goes with `review::queue`. T5 carries both properties forward as
  `a_narrow_grant_sees_a_full_page_of_its_own_pairs_and_no_count_of_the_rest` and
  `the_envelope_carries_no_tenant_wide_count`.
- Modify: `crates/lumberroom/src/wire.rs` (the types from spec §6.4; response types carry only the
  fields the loop prints or branches on, per that file's own rule at `:8-11`, and
  `DecisionRequest` carries exactly the keys spec §2 names, serde
  `rename_all = "snake_case"` on `Source` and `Verdict`)
- Modify: `crates/lumberroom/Cargo.toml` version `0.4.0` to `0.5.0`
- Modify: `docs/decisions/README.md` (row 0018)

**The CLI argument set, locked here so T4 invents none:** `--source`, `--limit`, `--offset`,
`--days`, `--min-similarity`, `--json`, `--supersede`, `--merge`, `--content`, `--occurred-at`,
`--tags`, `--keep`, `--keep-both`, `--confirm`, `--delete`, `--yes`, `--reason`, `--apply`,
`--dismiss`, `--dismissed`, `--undismiss`, and the four that keep their meaning, `--stale`,
`--conflicts`, `--registry`, `--dates`.

**Interfaces produced.** The file T0 commits. Struct bodies come from the spec sections named, field
for field; the stubs below are what makes `check --all-targets` pass before any other task starts.
Unused parameters carry a leading underscore and every import is live, so the committed file has no
warning to hide a later error behind.

```rust
//! One queue, one decide path. Phase 8.
//!
//! The actions live in `review`, `write` and `forget`; this file addresses them by key and says
//! which verdict each item takes, so the CLI and the MCP tool draw the same key line.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Ctx;
use crate::domain::errors::{DomainError, Result};
use crate::domain::types::{Memory, Sensitivity};

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;
pub const MAX_OFFSET: i64 = 2_000;

pub mod codes {
    pub const SOURCE_NOT_FILLED: &str = "source_not_filled";
    pub const VERDICT_NOT_FOR_SOURCE: &str = "verdict_not_for_source";
    pub const UNKNOWN_ORIGIN: &str = "unknown_origin";
    pub const NOT_A_QUEUE_KEY: &str = "not_a_queue_key";
    pub const UNKNOWN_SOURCE: &str = "unknown_source";
    pub const NAMESPACE_TOO_LARGE: &str = "namespace_too_large";
    pub const PAGE_TOO_DEEP: &str = "page_too_deep";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source { Conflict, Stale, Proposal }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict { Supersede, Merge, KeepBoth, Delete, Confirm, Apply, Dismiss }

#[derive(Debug, Clone, Serialize)]
pub struct QueueRow { /* spec §1.3, field for field, including `opened` */ }

#[derive(Debug, Clone, Serialize)]
pub struct ProposalField { pub label: String, pub value: String }

#[derive(Debug, Clone, Serialize)]
pub struct ProposalItem { /* spec §1.3, including `proposed_content`, `fields`, `verdicts` */ }

#[derive(Debug, Clone, Serialize)]
pub struct QueueItem { /* spec §1.3 */ }

#[derive(Debug, Clone, Serialize)]
pub struct Sources { pub conflict: bool, pub stale: bool, pub proposal: Vec<String> }

#[derive(Debug, Clone, Serialize)]
pub struct Queue {
    pub items: Vec<QueueItem>,
    pub sources: Sources,
    /// Source to code, for a source that was asked for and did not answer. Absent when empty.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub refused: std::collections::BTreeMap<String, &'static str>,
    pub dismissed: i64,
    pub stale_days: i32,
    pub min_similarity: f64,
    pub limit: i64,
    pub offset: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone)]
pub struct QueueQuery {
    pub sources: Option<Vec<Source>>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub days: Option<i32>,
    pub min_similarity: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct Decision { /* spec §2, including `occurred_at` */ }

#[derive(Debug, Clone, Serialize)]
pub struct Decided { /* spec §2 */ }

#[derive(Debug, Clone, Serialize)]
pub struct ProposalDecided {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub superseded: Vec<String>,
}

#[async_trait]
pub trait ProposalSource: Send + Sync {
    fn origin(&self) -> &'static str;
    async fn pending(&self, ctx: &Ctx, limit: i64, offset: i64) -> Result<Vec<(ProposalItem, Vec<Memory>)>>;
    async fn decide(&self, ctx: &Ctx, id: &str, verdict: Verdict) -> Result<ProposalDecided>;
}

/// The parsed key. A conflict's two ids are unordered here; `decide` orders them from the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Conflict(uuid::Uuid, uuid::Uuid),
    Stale(uuid::Uuid),
    Proposal { origin: String, id: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct DismissedListing {
    pub lo_id: String,
    pub hi_id: String,
    pub dismissed_by: String,
    pub dismissed_token: String,
    pub dismissed_at: DateTime<Utc>,
    pub rows: Vec<QueueRow>,
}

pub fn parse_key(_raw: &str) -> Result<Key> { Err(DomainError::internal("not built")) }
/// Conflict and stale only. `writable` is every row at its stored level, and every row opened.
pub fn verdicts_for(_source: Source, _writable: bool, _may_delete: bool) -> Vec<Verdict> { vec![] }
pub async fn queue(_ctx: &Ctx, _sources: &[std::sync::Arc<dyn ProposalSource>], _q: QueueQuery) -> Result<Queue> { Err(DomainError::internal("not built")) }
pub async fn decide(_ctx: &Ctx, _sources: &[std::sync::Arc<dyn ProposalSource>], _d: Decision) -> Result<Decided> { Err(DomainError::internal("not built")) }
pub async fn dismissed(_ctx: &Ctx, _limit: Option<i64>) -> Result<Vec<DismissedListing>> { Err(DomainError::internal("not built")) }
pub async fn undismiss(_ctx: &Ctx, _a: &str, _b: &str) -> Result<bool> { Err(DomainError::internal("not built")) }
pub fn render(_q: &Queue) -> String { String::new() }
```

`Sensitivity` is used by `QueueRow` and `Memory` by `ProposalSource::pending`, so both imports are
live once the struct bodies land. Write the bodies in the same commit as the stubs rather than
leaving the placeholders in the tree.

- [ ] Write the migration exactly as spec §3.1.
- [ ] Add `conflict_scan_max` to `QualityConfig`, its `env_num` read and its `validate` check.
- [ ] Extract the two statements into constants and take the new arguments.
- [ ] Write the types and stubs above; `./scripts/cargo.sh check --all-targets` clean and no new
      warning in `src/services/review_queue.rs`.
- [ ] Delete `review::queue` and its helpers, and the two integration tests that read it.
- [ ] Write `docs/decisions/0018-one-review-queue.md` and its README row.
- [ ] Commit as the owner: `Lock the review queue's shape, port and migration`.

**Gate:** `./scripts/cargo.sh check --all-targets` prints no error. The `--lib` count drops by the
five `review.rs` tests this task deletes and by nothing else; read the count before and after and
name the difference in the commit body. `test -j 1` drops the two `integration.rs` tests.

---

### T1: SQL (agent, opus, stage 1)

**Files:**
- Modify: `src/adapters/postgres/memory.rs` only.

**Consumes:** `CONFLICTS_SQL`, `STALE_SQL`, the four stubs and the `conflicts` and `stale`
signatures from T0.

**Produces:**
- `dismiss_pair`: `INSERT INTO memory_pair_dismissed (tenant_id, lo_id, hi_id, dismissed_by,
  dismissed_token) VALUES ($1, least($2,$3), greatest($2,$3), $4, $5) ON CONFLICT DO NOTHING`,
  returning `rows_affected() == 1`. Refuse `a == b` with `DomainError::validation`.
- `undismiss_pair`: `DELETE ... WHERE tenant_id = $1 AND lo_id = least($2,$3) AND hi_id =
  greatest($2,$3)`, returning `rows_affected() == 1`.
- `dismissed_pairs` as `const DISMISSED_PAIRS_SQL`: join both rows through `memory`, the grant
  `EXISTS unnest` block on both namespaces through `grant_arrays`, `ORDER BY dismissed_at DESC
  LIMIT $2`. Empty grant returns empty without a query.
- `dismissed_count` as `const DISMISSED_COUNT_SQL`: the same two grant blocks under
  `SELECT count(*)`. Empty grant returns 0 without a query.
- `live_embedded_counts` as `const LIVE_EMBEDDED_COUNTS_SQL`:
  `SELECT m.namespace, count(*) AS n FROM memory m WHERE m.tenant_id = $1 AND <live> AND
  m.embedding IS NOT NULL AND <grant> GROUP BY m.namespace ORDER BY n DESC`, with `live!()` and one
  `grant_arrays` block. It rides `memory_live (tenant_id, namespace) WHERE superseded_by IS NULL`.
  Empty grant returns empty without a query.
- `CONFLICTS_SQL`: the `NOT EXISTS` clause from spec §3.3 after the two grant blocks; the total
  order `similarity DESC, a.created_at, a.id, b.id`; `LIMIT $3 OFFSET $4`, where the service binds
  `limit + 1` so it can answer `has_more`. Empty grant returns empty without a query.
- `STALE_SQL`: the two clauses from spec §5, `$2` bound as `i32`, `ORDER BY m.created_at ASC, m.id`
  and `LIMIT $3 OFFSET $4` on the same convention.
- Rewrite the doc comment above `conflicts` (`:2644-2650`). "It runs from `lumberroom review` by
  hand, never on a request path" is false the day this ships. Say instead that the statement is
  still O(n^2) per namespace, that `live_embedded_counts` and `QUALITY.conflict_scan_max` are what
  bound it now, and keep the per-row nearest-neighbour probe as the named way out. Two or three
  lines, why only.

Unit tests in this file's `#[cfg(test)]`, beside the statement scans that already exist, each a scan
of the constant's text:

```rust
#[test] fn the_conflicts_statement_names_the_ledger_on_both_ids_in_uuid_order()   // "least(a.id, b.id)" and "greatest(a.id, b.id)"
#[test] fn the_conflicts_statement_breaks_a_similarity_tie_on_created_at_and_both_ids()
#[test] fn both_review_statements_bind_an_offset_rather_than_skipping_rows_later()  // "OFFSET $4" in each
#[test] fn the_stale_statement_names_the_confirmation_column_with_a_floored_window() // "last_confirmed_at" and "greatest($2, 1)"
#[test] fn the_dismissed_listing_applies_the_grant_to_both_halves()                // two "unnest($" occurrences
#[test] fn the_namespace_count_reads_live_embedded_rows_and_groups_by_namespace()
```

The behaviour behind each scan is an integration test in T5.

- [ ] Write the six tests; they fail on the stubs.
- [ ] Implement; `./scripts/cargo.sh check --all-targets` clean for this file.
- [ ] Return `wire_in: none`.

**Gate (run by the lead in W1):** `tests/review_queue.rs::a_kept_pair_leaves_the_queue_and_undismiss_brings_it_back`,
`::a_confirmed_stale_row_leaves_the_list_for_one_window`,
`::a_narrow_grant_sees_a_full_page_of_its_own_pairs_and_no_count_of_the_rest`,
`::two_pairs_at_one_similarity_page_once_each`.

---

### T2: Service (agent, opus, stage 1)

**Files:**
- Modify: `src/services/review_queue.rs` only.

**Consumes:** `review::{confirm, supersede, delete, writable_row}`, `write::run`,
`forget::ForgetOutcome`, `ctx.repos.memories.{conflicts, stale, find_by_id, dismiss_pair,
undismiss_pair, dismissed_pairs, dismissed_count, live_embedded_counts}`,
`adapters::auth::{can_read, can_write}`, `services::decrypt`, `DomainError::with_code`,
`ctx.cfg.quality.{conflict_threshold, stale_days, conflict_scan_max}`,
`ctx.cfg.policy.write_min_occurred_age_secs`.

**Produces:** the bodies of every function T0 stubbed.

Rules the implementation carries:

- `parse_key`: `conflict:<uuid>:<uuid>`, `stale:<uuid>`, `proposal:<origin>:<text>`; anything else
  is `validation("… is not a queue key").with_code(codes::NOT_A_QUEUE_KEY)`. Two equal ids in a
  conflict key is the same refusal.
- `verdicts_for(Conflict, true, md)`: `[Supersede, Merge, KeepBoth]` plus `Delete` when `md`.
  `Stale`: `[Confirm, Merge]` plus `Delete`. `writable == false`: empty. `Proposal`: never called;
  the item's own list, emptied when `writable` is false.
- `queue`: clamp `limit` to `1..=MAX_LIMIT` and `days` to `0..=36_500`; floor `min_similarity` at
  `ctx.cfg.quality.conflict_threshold` and cap it at 1.0; refuse `offset` above `MAX_OFFSET` with
  `validation("offset {n} is past the last page").with_code(codes::PAGE_TOO_DEEP)`. Defaults from
  `ctx.cfg.quality`. A requested `Proposal` with `sources.is_empty()` is
  `validation("this server fills no proposal source").with_code(codes::SOURCE_NOT_FILLED)`.
  Conflicts: call `live_embedded_counts` first and, when the first row's count exceeds
  `conflict_scan_max`, put `codes::NAMESPACE_TOO_LARGE` in `refused` under `"conflict"` and read no
  pairs; otherwise `conflicts(tenant, min_similarity, limit + 1, offset, read)`, keep `limit`,
  re-fetch each half through `find_by_id` and `can_read`, drop the pair whole if either half is
  missing, run `super::decrypt` over both and set `opened` per row from the id list it returns.
  Stale: `stale(tenant, days, limit + 1, offset, read)`, same keep and decrypt. Proposals: for each
  source, `pending(ctx, limit, offset)` and keep `limit`, `can_read` per member (a miss drops the
  item whole), decrypt, `opened` per row. `writable` per item is `can_write` on every row at its
  stored level and every row opened. `has_more` when any source returned more than `limit`.
  `dismissed` from `dismissed_count`. Order: conflicts, stale, proposals. One source refusing puts
  a code in `refused` and leaves the others alone; the whole call fails only when every requested
  source failed.
- `decide`: parse the key; fetch every row through `writable_row` before any write; order a
  conflict pair by `(created_at, id)`; refuse a verdict outside the item's list with
  `validation("verdict {v} is not one a {source} item takes: {list}").with_code(codes::VERDICT_NOT_FOR_SOURCE)`.
  Then:
  - `Supersede`: `keep` defaults to the newer id; must be one of the pair; the other goes through
    `review::supersede(ctx, other, keep)`; `superseded`, `end_left_open` from `Resolved`.
  - `Merge`: `content` required. `namespace` is the rows' shared namespace (the conflicts join
    guarantees it, `memory.rs:2664-2666`; a mismatch is still refused as validation, cheaply).
    `sensitivity` is the max over rows, `as_str()`. `occurred_at` is the request's when it sent
    one; otherwise the newest row's, and only when that value is at least
    `ctx.cfg.policy.write_min_occurred_age_secs` old, because `write::run` fences anything newer
    (`write.rs:197-205`, `:660-698`) and `Decision` has no wire value for "no occurred_at".
    `write::run(ctx, content, &namespace, tags, Some(newest_id), Some(level), occurred_at)`.
    Then for each other id `review::supersede(ctx, id, &written)`; a failure appends to
    `unfinished` and does not abort the loop.
  - `KeepBoth`: `dismiss_pair(tenant, a, b, &ctx.principal.client, &ctx.principal.token_id)`;
    `already_dismissed` when false. `client` is a constant per deployment, so the fingerprint is
    the half that names anybody.
  - `Delete`: `id` required on a two-row item; must be one of the item's ids;
    `review::delete(ctx, id, reason)`; `deleted` from `ForgetOutcome.rows`.
  - `Confirm`: the one id.
  - `Apply`, `Dismiss`: the source whose `origin()` matches, else
    `validation("no proposal source named {origin}").with_code(codes::UNKNOWN_ORIGIN)`;
    `decide(ctx, id, verdict)`; map `state` to `proposal_state`, `written`, `superseded`.
- `undismiss`: both ids through `writable_row`; a `NotFound` from either answers `Ok(false)`;
  otherwise `undismiss_pair`.
- `dismissed`: `dismissed_pairs(tenant, limit, &ctx.principal.read)`, both rows through
  `find_by_id`, decrypt, always two `rows`.
- `render`: the text the MCP tool prints, one block per item in the shape spec §6.1 shows without
  the key line; a proposal prints `kind`, `proposed_content` and each `fields` entry as
  `label: value`. Row content and every source-supplied string go inside a delimited block whose
  opening line says it is data, because a model reads this with `review_decide` in the same
  session.

Unit tests in the file:

```rust
#[test] fn a_conflict_key_parses_either_order_and_refuses_a_pair_of_one_id()
#[test] fn a_proposal_key_carries_its_origin()
#[test] fn a_stale_item_takes_confirm_and_merge_and_delete_only_with_the_flag()
#[test] fn an_unwritable_or_unopened_item_takes_nothing()
#[test] fn render_wraps_row_content_and_proposal_fields_in_a_block_labelled_as_data()
#[test] fn an_offset_past_the_ceiling_is_refused_rather_than_clamped()
```

- [ ] Write the tests; run them by copying the pure functions into a scratch crate if needed.
- [ ] Implement.
- [ ] `./scripts/cargo.sh check --all-targets` clean for this file.

**Gate (W1):** `tests/review_queue.rs::a_merge_writes_once_and_retires_both_sources_into_it`,
`::a_merge_whose_second_retirement_fails_reports_the_leftover_in_unfinished`,
`::a_merge_of_two_same_day_rows_writes_without_an_occurred_at`,
`::a_verdict_the_source_does_not_take_is_refused_before_any_row_changes`,
`::delete_through_the_queue_still_needs_may_delete`,
`::keep_both_needs_the_write_grant_on_both_rows`,
`::keep_both_records_the_client_and_the_token_fingerprint`,
`::a_namespace_over_the_scan_ceiling_refuses_conflicts_and_still_answers_stale`,
`::a_row_whose_content_is_empty_still_takes_every_verdict`,
`::a_supersede_default_keeps_the_newer_row_whatever_order_the_key_spelled`.

---

### T3: HTTP (agent, opus, stage 1)

**Files:**
- Create: `src/http/review.rs`.
- Modify: `src/http/mod.rs:1125-1147` and `:1157-1187` only, to put `admin_review_stale` and
  `admin_review_conflicts` on `review_queue::queue` and delete `conflict_side` (`:2006`). Both
  responses keep the exact JSON they publish today, because an installed CLI reads them. Nothing
  else in this file; the two `wire_in` lines below are still the lead's.

**Consumes:** `super::{authed, domain_error, Http}` as `src/http/archive.rs:36` does;
`services::review_queue::*`; `http.state.proposals`.

**Produces:**

```rust
pub fn routes() -> Router<Http> {
    Router::new()
        .route("/admin/review/queue", get(queue))
        .route("/admin/review/decide", post(decide))
        .route("/admin/review/dismissed", get(dismissed))
        .route("/admin/review/dismissed/{a}/{b}", delete(undismiss))
}
```

- `queue`: `Query<QueueParams { source: Option<String>, limit, offset, days, min_similarity }>`;
  `source` is a comma list parsed to `Source`; an unknown word is `400 unknown_source`. Answers
  `Json(Queue)`, including `refused` and `dismissed`.
- `decide`: `Json<Decision>`; answers `Json(Decided)`. Every error goes through
  `domain_error(&e, e.code().unwrap_or("review_decide_failed"))`. Same rule on `queue` with
  fallback `review_queue_failed`.
- `dismissed`: `Query<LimitQuery>`; answers `{"pairs": [...]}`.
- `undismiss`: answers `{"removed": bool}`.

Unit tests in the file: `#[test] fn source_list_parses_and_refuses_an_unknown_word()`,
`#[test] fn a_coded_refusal_publishes_its_own_code_and_an_uncoded_one_the_fallback()`.

- [ ] Implement; return `wire_in` as two lines for `src/http/mod.rs`: `mod review;` beside
  `mod archive;` at line 37, and `.merge(review::routes())` inside the
  `app.merge(archive::routes())` chain at line 170, before `.with_state`, because `routes()`
  returns a `Router<Http>` and wants the state that line applies.

**Gate (W1):** `tests/review_queue.rs::the_queue_route_answers_the_envelope_with_sources_and_defaults_from_config`,
`::source_proposal_on_an_engine_answers_400_source_not_filled`,
`::the_old_conflicts_and_stale_routes_answer_the_same_json_they_did_before`.

---

### T4: CLI (agent, sonnet, stage 1)

**Files:**
- Create: `crates/lumberroom/src/review.rs`
- Modify: `crates/lumberroom/src/wire.rs` (fill doc comments on the types T0 declared)
- Modify: `crates/lumberroom/tests/wire.rs` (pin tests)
- Create: `crates/lumberroom/tests/fixtures/review_queue.json`, `review_decide.json`,
  `review_dismissed.json`

**Consumes:** `crate::client::{Client, err, Result}`, `crate::args::Args`, `crate::{out, out_json,
prompt}`, `crate::commands::{compact, urlencode, typed}` (`typed` is private at `commands.rs:31`;
return a `wire_in` asking for `pub(crate)`), `c.http_get`, `c.http_request`.

**Produces:**

```rust
/// `lumberroom review`, every form. Every read of stdin goes through `read_line`.
pub async fn run(c: &Client, args: &Args, read_line: &mut impl FnMut() -> std::io::Result<String>) -> Result<()>;

pub enum Answer { Act(wire::DecisionRequest), NeedsText, NeedsDelete, Skip, Quit, Again }

/// One `usize` per verdict plus `read` and `skipped`, and `dismissed_in_ledger: i64` from the
/// envelope, which the tally prints beside the rest.
#[derive(Default)]
pub struct Tally { /* ... */ }

pub fn key_line(verdicts: &[wire::Verdict], pair: bool) -> String;
/// One trimmed line. Anything but a single character in the item's list is `Again`, so a line left
/// over from a pasted merge redraws the prompt rather than deciding the next item.
pub fn answer_to_decision(item: &wire::ReviewItem, answer: &str) -> Answer;
/// `m`: read lines until a blank one and join them with a space. `None` when nothing was typed,
/// which aborts to the key line. Reading to the blank line is what stops a two-line paste
/// leaving its second line to be eaten as the next key press.
pub fn merge_text(read: &mut impl FnMut() -> std::io::Result<String>) -> Result<Option<String>>;
/// `d`: which row on a pair, then the first eight characters of the id typed back.
pub fn delete_target(item: &wire::ReviewItem, which: &str) -> Option<String>;
pub fn delete_confirmed(id: &str, typed: &str) -> bool;
pub fn flag_decision(args: &Args) -> Result<Option<(wire::DecisionRequest, bool)>>;  // (request, needs_delete_confirmation)
pub fn one_line(d: &wire::Decided) -> String;
pub fn tally(t: &Tally) -> String;
```

Behaviour is spec §6, all of it: the paging rule and the `dismissed` count in the header and the
tally in §6.1, the flag table in §6.2 including `--offset` on `--json`, and the codes in §6.3.
`--dates` and `--registry` keep the exact output they have today and keep calling
`/admin/review/dates` and `/admin/review/registry`; they move into this file because `run` owns the
command, not because anything about them changes.
`flag_decision` reads one table from flag to (key template, verdict) rather than one branch per
flag; the friendly spellings are the point and nine hand-written arms are not.

Tests, in the file:

```rust
#[test] fn the_key_line_draws_only_the_verdicts_the_item_takes_and_read_only_draws_n_and_q()
#[test] fn s_keeps_the_newer_row_and_o_keeps_the_older()
#[test] fn a_merged_text_of_two_lines_is_read_whole_and_leaves_nothing_on_the_stream()
#[test] fn an_empty_merged_text_aborts_to_the_key_line()
#[test] fn a_key_line_answer_longer_than_one_character_decides_nothing()
#[test] fn a_delete_confirmation_needs_the_first_eight_characters_of_the_id()
#[test] fn two_action_flags_are_a_usage_error()
#[test] fn merge_without_content_names_the_decision_that_the_text_comes_from_the_caller()
#[test] fn the_merge_line_prints_every_unfinished_id()
#[test] fn the_tally_counts_every_verdict_the_skips_and_the_ledger_total()
```

The two-line-paste test is the one that matters: drive `run` from a canned `read_line` over
`["m", "first line", "second line", "", "q"]` and assert the loop ends on `q` with one merge and no
verdict against the second item.

Pin tests added to `crates/lumberroom/tests/wire.rs`, with fixtures transcribed from the spec:

```rust
#[test] fn a_review_queue_parses_and_keeps_the_proposal_origins()
#[test] fn a_review_queue_parses_when_the_server_adds_a_field_this_client_does_not_read()
#[test] fn a_review_item_without_similarity_is_a_stale_item()
#[test] fn a_proposal_item_keeps_its_fields_in_order_and_its_verdicts()
#[test] fn a_decision_request_serialises_exactly_the_keys_the_server_reads()
#[test] fn a_decided_body_parses_without_its_optional_fields()
```

- [ ] Write tests, implement, `./scripts/cargo.sh check -p lumberroom --all-targets` clean.
- [ ] Return `wire_in`: `crates/lumberroom/src/lib.rs` gets `pub mod review;` and, in `dispatch`
  at `:104`, an arm that binds the function before borrowing it, since `run` wants
  `&mut impl FnMut` and `read_line` at `:155` is a bare `fn` item:
  ```rust
  "review" => {
      let mut read = read_line;
      review::run(client, args, &mut read).await
  }
  ```
  `crates/lumberroom/src/commands.rs` drops `pub async fn review` (lines 554-664) and makes `typed`
  `pub(crate)`; `COMMANDS` stays.

**Gate (W1):** `./scripts/cargo.sh test -j 1 -p lumberroom` passes with the new count; the manual
gate in spec §10.

---

### T5: Acceptance suite (agent, opus, stage 1)

**Files:**
- Create: `tests/review_queue.rs` (compiled in W1; the lead holds `tests/` but this file is new
  and this task alone writes it).

**Consumes:** the harness shape in `tests/integration.rs:57-120` and `:185-245` (`setup_with`,
`restricted_at`, `owner_like`, `nonce`, `common::lock_database`), the truncate statement at
`integration.rs:106-111` (ends `RESTART IDENTITY CASCADE`, so the new table needs no list change).

**Produces:** a fixture `struct CannedProposals` implementing `ProposalSource` with origin
`"canned"`, holding a `Vec<(ProposalItem, Vec<Memory>)>` and recording the last `decide` call, and
these tests, each seeding through `write::run` and calling `review_queue::{queue, decide}`
directly, plus the HTTP ones through a bound server the way `tests/cleanup.rs` does. A doc comment
on `CannedProposals` says it is a double and that the five tests over it are not evidence about any
shipped source, because the engine ships none:

```rust
a_kept_pair_leaves_the_queue_and_undismiss_brings_it_back
keep_both_needs_the_write_grant_on_both_rows
keep_both_records_the_client_and_the_token_fingerprint
undismiss_answers_false_for_a_pair_the_caller_may_not_change
a_confirmed_stale_row_leaves_the_list_for_one_window            // backdates last_confirmed_at to return it
a_narrow_grant_sees_a_full_page_of_its_own_pairs_and_no_count_of_the_rest
the_envelope_carries_no_tenant_wide_count
two_pairs_at_one_similarity_page_once_each                      // two pages of one, no repeat and no gap
an_offset_past_the_ceiling_is_refused_rather_than_clamped
a_namespace_over_the_scan_ceiling_refuses_conflicts_and_still_answers_stale
a_row_whose_content_is_empty_still_takes_every_verdict
a_merge_writes_once_and_retires_both_sources_into_it            // and carries the newest source's occurred_at
a_merge_of_two_same_day_rows_writes_without_an_occurred_at
a_merge_whose_second_retirement_fails_reports_the_leftover_in_unfinished
a_verdict_the_source_does_not_take_is_refused_before_any_row_changes
delete_through_the_queue_still_needs_may_delete
a_supersede_default_keeps_the_newer_row_whatever_order_the_key_spelled
the_queue_route_answers_the_envelope_with_sources_and_defaults_from_config
the_old_conflicts_and_stale_routes_answer_the_same_json_they_did_before
source_proposal_on_an_engine_answers_400_source_not_filled
a_deleted_row_takes_its_dismissals_with_it
a_canned_proposal_appears_with_its_members_fields_and_verdicts
a_full_page_of_canned_proposals_reports_has_more
a_canned_proposal_with_a_member_outside_the_grant_drops_whole
a_canned_proposal_with_a_member_that_will_not_open_takes_no_verdict
a_proposal_decision_reaches_the_source_named_by_the_key_and_an_unknown_origin_is_refused
```

The unfinished-merge test expires the older row between seeding and deciding, which
`write::validate_supersedes` refuses (`write.rs:618`). The unopened-member test writes a private
row and clears `ctx.keys`. The empty-content test writes `""` at `open`, where nothing decrypts, so
`opened` has to come from `decrypt`'s returned ids rather than from the text. The scan-ceiling test
lowers `conflict_scan_max` in the test config instead of seeding thousands of rows.

- [ ] Write the file. It will not compile until W1; say so in `not_done`.

---

### T6: Docs (agent, sonnet, stage 1)

**Files:**
- Modify: `docs/managing.md:181-185` (the `review` lines), `README.md:196`, `CHANGELOG.md`
  (`[Unreleased]`: Added, Changed for the stale predicate, the `conflicts` grant, the scan ceiling
  and the CLI version).

- [ ] Rewrite the two command lines and add a short paragraph on the loop and `--json`.
- [ ] Name `CONFLICT_SCAN_MAX` wherever `managing.md` covers settings an operator turns, and say
  what a `namespace_too_large` refusal means.
- [ ] CHANGELOG entries in the file's own voice, past tense, no "now".

**Gate:** `grep -rP '\x{2014}'` on the three files prints nothing; W1's `test -j 1` passes with
the doc-scan tests that already read these files.

---

### W1: Wiring pass and gates (lead, stage 1)

- [ ] Apply every `wire_in` from T1 to T6.
- [ ] `./scripts/cargo.sh check --all-targets` clean.
- [ ] `./scripts/cargo.sh test -j 1`; read the count and confirm no `skipping` line;
  `tests/review_queue.rs` reports twenty-six tests, and the whole-suite count accounts for the two
  `integration.rs` tests T0 deleted.
- [ ] `./scripts/cargo.sh test -j 1 -p lumberroom`.
- [ ] `grep -rP '\x{2014}' src crates docs migrations` prints nothing.
- [ ] Take the spec §10 criterion 1 baseline from a `main` build before merging anything, on the
  archive copy, and write the count, the `min_similarity`, the copy and the date into the PR body.
  After the merge that build is gone and the comparison has no other side.
- [ ] Manual gate, spec §10, on a local engine restored from an archive export.
- [ ] Commit per task, explicit paths, no `git add -A`. Branch `feat/review-queue`.

---

### T7: MCP tools (agent, opus, stage 2)

**Files:**
- Create: `src/mcp/review_tools.rs`
- Modify: `src/mcp/capability.rs` (two rows, `Capability::Open`, with spec §7.3's one-sentence
  reason in two lines)

**Consumes:** `super::Lumberroom`, `self.run(...)` at `src/mcp/mod.rs:347`,
`self.state.proposals`, `services::review_queue::*`.

**Produces:**

The two method signatures below are the shape T7 fills in; write bodies, not declarations, since an
inherent `impl` takes no bare signature.

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReviewQueueArgs {
    /// Which sources. Omit for every source this server fills.
    #[serde(default)] pub source: Option<Vec<String>>,
    #[serde(default)] pub limit: Option<i64>,
    #[serde(default)] pub offset: Option<i64>,
    #[serde(default)] pub days: Option<i32>,
    #[serde(default)] pub min_similarity: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReviewDecideArgs {
    pub key: String,
    /// supersede, merge, keep_both, delete, confirm, apply, dismiss.
    pub verdict: String,
    #[serde(default)] pub keep: Option<String>,
    #[serde(default)] pub id: Option<String>,
    #[serde(default)] pub content: Option<String>,
    #[serde(default)] pub tags: Option<Vec<String>>,
    /// RFC 3339. merge only.
    #[serde(default)] pub occurred_at: Option<String>,
    #[serde(default)] pub reason: Option<String>,
}

#[tool_router(router = review_tool_router, vis = "pub(crate)")]
impl Lumberroom {
    #[tool(name = "review_queue", description = "…")]
    async fn review_queue(&self, Parameters(args): Parameters<ReviewQueueArgs>, rc: RequestContext<RoleServer>) -> Result<CallToolResult, McpError>;
    #[tool(name = "review_decide", description = "…")]
    async fn review_decide(&self, Parameters(args): Parameters<ReviewDecideArgs>, rc: RequestContext<RoleServer>) -> Result<CallToolResult, McpError>;
}
```

Descriptions are instructions, as `memory_history`'s is. `review_queue`: call it when the person
asks to review, tidy or resolve their memory; read every item back to them before deciding.
`review_decide`: one item, one verdict, only on an item the person has read with you; `merge` takes
the text they gave; never call it unprompted. Those sentences are behavioural guidance and no
comment or doc line in this task may call them evidence, because `unprompted` reads true on every
MCP call (spec §7.3). Text content is `review_queue::render`, which already wraps row content and
source-supplied text in a block labelled as data; structured content is the `Queue` or `Decided`
JSON. An error's `code()` leads the tool's error text.

Unit tests in `capability.rs` extend `a_bare_grant_sees_only_the_open_tools` to seven names.

- [ ] Implement; `wire_in`: `Lumberroom::new` at `src/mcp/mod.rs:178` becomes
  `Self { state, tool_router: Self::tool_router() + Self::extra_tool_router() +
  Self::review_tool_router() }`, `pub mod review_tools;` goes beside `pub mod extra_tools;` at
  `:35`, and `tests/mcp_capability.rs:60` `OPEN_TOOLS` becomes `[&str; 7]` with `review_decide`,
  `review_queue` in sorted position.

**Gate (W2):** `tests/mcp_capability.rs` passes with seven open tools; a new test there,
`review_decide_through_mcp_refuses_delete_without_may_delete`, written by the lead in W2.

---

### T8: Permissions doc (agent, sonnet, stage 2)

**Files:**
- Modify: `docs/permissions.md:164-180` (two rows in "Which tools each capability opens"),
  `docs/connect-claude-code.md` (it lists tools; add the two), `CHANGELOG.md`.

- [ ] Add the rows and lines. W2 adds the scan that holds the table against `TOOL_CAPABILITIES`.

---

### W2: Wiring pass and gates (lead, stage 2)

- [ ] Apply T7 and T8 `wire_in`; write `review_decide_through_mcp_refuses_delete_without_may_delete`
  in `tests/mcp_capability.rs`.
- [ ] Add to `tests/permissions_doc.rs`: every name in `TOOL_CAPABILITIES` appears in
  `docs/permissions.md`, read the way that file already reads `src/config.rs`.
- [ ] Same three gate commands as W1; `tests/mcp_capability.rs` count grows by one,
  `tests/permissions_doc.rs` by one.
- [ ] Branch `feat/review-mcp`, stacked on `feat/review-queue`.

## Merge order

1. Engine `feat/review-queue` to `main` (T0 to W1). Take the §10 criterion 1 baseline first.
2. Engine `feat/review-mcp` to `main`, stacked on 1 (T7 to W2).
3. A downstream proposal source merges after 1 and needs nothing from 2 to read the queue.

Each PR body says which gate ran and pastes the count line.

## Self-review

Spec coverage: §1 T2 and T3; §2 T2, T3 and T0 (the error code); §3 T0 and T1; §4 T0, T1, T2; §5
T1; §6 T4; §7.2 T0 and T5 (the canned source); §7.3 T7, T8, W2; §8 nothing to build; §11 the
decision record in T0; §10 W1 and W2.

Placeholders: the `…` in T7's descriptions are the two instruction texts spec §7.3 spells out.

Type consistency: `Decision`, `Decided`, `Queue`, `QueueItem`, `ProposalItem`, `ProposalField`,
`ProposalDecided`, `Verdict` and `Source` are defined once in T0 and named the same in T2, T3, T4's
wire mirrors, T5's fixture and T7. `conflicts` takes `offset` and `reader` at `http/mod.rs:1170`
and in `review_queue.rs`, and `review.rs:115` goes with the function T0 deletes.

Nothing outside T0 adds surface. `conflict_scan_max` is the only config field, the ledger's five
columns the only columns, `namespace_too_large` and `page_too_deep` the only new codes,
`ProposalField` and `Tally` the only types not in the spec's own code blocks, and T0's flag list is
the whole CLI argument set.

Not done by this plan: the console page (spec §8), the cleanup queue as a source (open question 1),
a `mayReview` flag (open question 2), `expire` on any surface (decision 0017).

Open risks. A grant changed between the read and the decide answers a refusal the key line did not
predict. `live_embedded_counts` and the join are two statements, so a namespace crossing the ceiling
between them runs one join over it, bounded by whatever `statement_timeout` the deployment sets. The
2,000 ceiling comes from a dev-container probe and will be wrong in some direction on server
hardware, which is why spec §10 criterion 6 times it and §9 reverses on it. An offset cursor still
pages the same pair twice under a concurrent write, which the loop's re-read from zero tolerates and
`--json` readers have to; the tiebreak T1 adds closes the case that needed no concurrent write at
all. Nothing here catches an injected instruction inside the owner's own client, and open question 2
is where that gets answered.
