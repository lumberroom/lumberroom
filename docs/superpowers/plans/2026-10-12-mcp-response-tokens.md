# MCP response tokens: engine implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut what `context_bootstrap`, `memory_search`, `memory_history`, `review_queue` and the tool list cost a model, with the old shapes behind opt-ins and a gate that shows agents behave the same.

**Architecture:** The services keep their types. `src/mcp/views.rs` and a new `src/mcp/review_view.rs` map each answer to a brief view, the way decision 0020 mapped `MemoryView`. `bootstrap::render_report` says which digest rows the text left out, so the brief digest carries them. JSON text goes out compact. A write refuses fence marker lines. A harness under `scripts/eval/agent-replay/` drives headless Claude Code on legolas against the base and branch builds and diffs items, calls and answers.

**Tech Stack:** Rust (axum, rmcp, serde_json, schemars). The harness: Python 3 standard library plus `pytest`, Node for the OpenClaw serialization check, and the `claude` CLI already on legolas.

**Spec:** `docs/superpowers/specs/2026-10-12-mcp-response-tokens.md`. Read it before any task. D1 to D11 and G0 to G3 point into it.

**Fork side:** `lumberroom-cloud/docs/superpowers/plans/2026-10-12-mcp-response-tokens-fork.md`, after this plan's PR 4 merges and is tagged.

## Global Constraints

- Search ranking, hit count, `SEARCH_DEFAULT_LIMIT` 8 and `SEARCH_MAX_LIMIT` 50 do not change.
- `context_bootstrap` text does not change by one byte.
- Timestamps pass through at full precision. No view reformats one (D6).
- `structuredContent` is never omitted on a success.
- `Invocation::Cli` reads the full shape by default (D2). Every other invocation reads brief.
- `review_queue` MCP default: `max(2, ceil(10 / n))` per source. An explicit `limit` keeps today's per-source meaning. HTTP keeps `DEFAULT_LIMIT` 50.
- The review `more` hint names no offset (D5).
- Pinned and kept: `WriteOutcome`, `OCCURRED_AT_DESCRIPTION`, `AS_OF_DESCRIPTION`, the review description phrases, "source is the name of the app that wrote", the `ORDERS` denylist.
- No new Rust dependency, no new environment variable, no migration.
- Decision number: 0031. Claim it against `main`, open PRs and unmerged remote branches; 0030 is on `origin/feat/write-cap`.
- G2 runs only as `claude -p` on legolas on the owner's subscription. No Messages API, no other provider.
- Prose rules in the CONTEXT block apply to code comments, docs, test names and commit messages.

## Review Focus

1. A digest whose text left rows out: the brief payload carries each of them whole in `left_out`. Tasks 2 and 2b pin it.
2. A proposal field whose `label` holds an instruction: the label sits inside a fence in the structured copy. Task 3 pins it.
3. An installed CLI with no `detail` argument running `lumberroom search`: it still parses `score`. Task 5 pins it through `Invocation::Cli`; task 4 makes a brief answer parse too.
4. An agent copying a fenced value into a merge: the write refuses it with `content_carries_fence`. Task 3b pins it.
5. `review_queue` with `source: ["undated"]` and no `limit`: it reads 10, and `more` names no number. Task 3 pins it.

---

## CONTEXT block

Paste this block, unchanged, at the top of every delegated agent's prompt.

```
CONTEXT FOR THIS TASK (lumberroom engine, /Users/aditya/work/cbrspn-tech/lumberroom)

You implement one task of a written plan. Read these first:
- docs/superpowers/specs/2026-10-12-mcp-response-tokens.md (the spec; D1 to D11, G0 to G3)
- docs/superpowers/plans/2026-10-12-mcp-response-tokens.md, your task only
- CLAUDE.md and docs/architecture.md

Hard rules
- Touch only the files your task lists. Shared composition files (src/mcp/mod.rs, src/main.rs,
  Cargo.toml, migrations/, tests/) belong to the lead unless your task names them. Need a change
  there? Return it as a wire_in note.
- Never run git. Never commit. Never run `cargo test`: the integration suite truncates a shared
  database. You may run `./scripts/cargo.sh check`; expect errors in files you do not own and
  grep the output for your own files.
- `cargo check` does not compile #[cfg(test)]. Copy your pure functions and their tests into a
  scratch crate under ~/.cache/agent-scratch/<task-id>/, build it with
  CARGO_TARGET_DIR=~/.cache/agent-scratch/target, run the tests, then delete the directory before
  you return.
- No new Cargo dependency. No new environment variable (every setting lives in src/config.rs).
- Domain and services never import from adapters.
- Behaviour must not change: same memories, same order, same text for context_bootstrap, every
  timestamp at full precision. When a choice could change what a model reads beyond the spec, stop
  and report it as an open_risk.

Ground truth you can rely on
- Lumberroom::run sends (text, structured) together: src/mcp/mod.rs:420-492.
- search::Hit is the service and console type; tests/integration.rs:985-1007 pins its keys. Do not
  change it. Build views in src/mcp/.
- serde_json key order depends on the build (preserve_order). Never depend on it.
- Ctx carries `invocation: Invocation`. Invocation::Cli comes from the header
  x-memory-invocation: cli.
- render prints whole entries and counts the rest in a footer: src/services/bootstrap.rs:680-705.
- Review pages read the live queue with LIMIT and OFFSET: src/services/review_queue.rs:473-479,
  529-532. A verdict removes its item.

Writing rules (comments, docs, test names, your report)
- No em dashes anywhere. Check with: grep -rnP '\x{2014}' <your files>. A clean run prints nothing.
- No AI attribution anywhere: no "generated by", no co-author lines, no tool names.
- Active voice, a person or a named component as the subject. No adverbs where a verb works.
- No "Note that", no "Here's what", no throat-clearing, no "not X, it's Y" contrasts.
- Comments say why or flag a trap. They do not narrate the code.
- Write "implemented", never "verified" or "works", for anything you did not run.
- Tool and argument descriptions state facts and never order the model. They must not contain any
  phrase in ORDERS (tests/mcp_tool_annotations.rs:42-65), such as "do not", "never pass",
  "pass it", "without asking".

Return exactly this JSON, nothing else:
{
  "task": "<id>",
  "files_changed": ["<path>", ...],
  "ran": ["<command> -> <pass/fail line>", ...],
  "wire_in": ["<exact change the lead must make in a file you do not own>", ...],
  "not_done": ["<item>", ...],
  "open_risks": ["<risk, with file:line>", ...]
}

If you spend a large share of your budget without producing the files, stop and return what you
have with not_done filled. The lead finishes the task directly and does not re-delegate it.
```

---

## Task table

| Id | Purpose | Files owned | Tier | Depends on | Output contract | Gate |
|---|---|---|---|---|---|---|
| 0 | Gate harness | `scripts/eval/agent-replay/**` (new), `docs/eval-agent-replay.md` (new) | sonnet | none | runner, proxy, review seeder, G1 script, compare, judge; records in the shape below | `pytest` on `test_compare.py` passes; task 5c's probe passes |
| 1 | Lock interfaces | `src/mcp/views.rs` (new items), `src/mcp/review_view.rs` (new), `src/mcp/mod.rs` (args, `pub mod`), `src/mcp/extra_tools.rs` (arg structs), `src/services/review_queue.rs` (two `pub(crate)`), `src/services/bootstrap.rs` (`Rendered`, `render_report` signature, two `Digest` fields), `src/domain/fence.rs` (new, signature), `src/domain/mod.rs` | lead, opus | none | one commit that compiles; bodies `todo!()` except `compact`, `detail_for`, `rows_for` | `./scripts/cargo.sh check`; `cargo test --test mcp_tool_annotations` |
| 2 | Search, history and digest views | `src/mcp/views.rs` | opus | 1 | bodies of `search`, `timeline_value`, `digest`; tests | scratch tests; lead runs `cargo test --lib mcp::views` |
| 2b | `render_report` | `src/services/bootstrap.rs` | opus | 1 | `render_report` body; `run` fills the two `Digest` fields; tests | scratch tests; lead runs `cargo test --lib services::bootstrap` and `--test bootstrap_ranking` |
| 3 | Review queue view, extra-tools handlers, `lr-review` | `src/mcp/review_view.rs`, `src/mcp/extra_tools.rs`, `client/skills/lr-review/SKILL.md` | opus | 1 | fenced brief queue, default limit, `more`, compact text, brief history wiring, skill paging text | scratch tests; lead runs `cargo test --lib mcp::review_view mcp::extra_tools` |
| 3b | A write refuses fence lines | `src/domain/fence.rs`, `src/services/write.rs` (`run_inner`, `run_restored`), `src/services/archive.rs` (comment) | opus | 1 | `carries_fence`, the refusal, the archive exemption, tests | scratch tests; lead runs `cargo test --lib domain::fence services::write services::archive` |
| 4 | CLI and policy test | `crates/lumberroom/src/wire.rs`, `crates/lumberroom/src/format.rs`, `crates/lumberroom/src/commands.rs`, `crates/lumberroom/src/eval/runner.rs`, `crates/lumberroom/src/eval/corpus.rs`, `crates/lumberroom/tests/wire.rs`, `scripts/policy-test.sh` | sonnet | 1 | `detail`, `rows`, optional `score`, the inventory existence check | lead runs `cargo test -p lumberroom` and `scripts/policy-test.sh` |
| 5 | Wiring, integration tests, docs | `src/mcp/mod.rs` (handlers), `src/services/bootstrap.rs:22-23` (comment, after 2b), `tests/mcp_brief.rs` (new), `tests/review_queue_mcp.rs`, `tests/mcp_source.rs`, `tests/mcp_capability.rs`, `CHANGELOG.md`, `docs/decisions/0031-mcp-answers-carry-what-an-agent-acts-on.md` (new), `docs/decisions/0019-a-caller-corrects-a-proposal.md` (superseded note), `docs/decisions/README.md` | lead, opus | 2, 2b, 3, 3b, 4 | the full suite green | `./scripts/cargo.sh test`; `grep -rnP '\x{2014}'` clean on every touched file |
| 5c | Corpus, templates, slots and probe on legolas | nothing in the repo; `~/lumberroom-gate/2026-10-mcp-response-tokens/` on legolas | lead; haiku runs commands | 0 | fork and engine templates, gate tokens, emptied queues, fixtures, probe record | hash matches; conversion diff empty; probe passes |
| 6 | Gate A0, A0b, A1 | `docs/results/2026-10-mcp-response-tokens-gate.md` (new) | lead runs; haiku collates | 5, 5c | report with every G1 and G2 rule and the token table | G1 and the consumer check pass; G2 Sonnet stage, then Opus stage, pass |
| 7 | Description trim | `description` strings and argument docs in `src/mcp/mod.rs` and `src/mcp/extra_tools.rs`, except the pins | opus | 6 | about 30 percent shorter; cut table | annotation tests; `cargo test --lib mcp` |
| 8 | Gate A2 | `docs/results/2026-10-mcp-response-tokens-gate.md` (append), `CHANGELOG.md` | lead runs; haiku collates | 7 | report section for A2 | G2 rules, A2 against A1, A0b as the floor |

Tasks 2, 2b, 3, 3b and 4 run in parallel after task 1. Tasks 0 and 5c run in parallel with all of them.

## Stacked PRs and merge order

| PR | Branch | Tasks | Holds |
|---|---|---|---|
| 1 | `feat/agent-replay-gate` | 0 | the harness, mergeable alone |
| 2 | `feat/mcp-brief-answers` | 1, 2, 2b, 4, task 5's wiring for `context_bootstrap`, `memory_search` and the compact text of `memory_write` and `registry_get`, decision 0031 | cuts 1, 3 and 4, history excepted |
| 3 | `feat/mcp-brief-review-queue`, stacked on 2 | 3, 3b, task 5's tests for both, the 0019 note | cut 2, the brief history, the fence refusal |
| 4 | `feat/mcp-description-trim`, stacked on 3 | 7 | cut 5 |

Order: PR 1 merges first. Task 6 runs on PR 3's head (arm A1). On a pass, PR 2 and then PR 3 merge in one sitting, with no other engine merge between them, because the fork merges at PR 3's merge commit (spec section 7). Task 8 runs on PR 4's head; on a pass, PR 4 merges. Retarget each stacked PR with `gh pr edit <n> --base main` before its base merges. PR 4 reaches the fork with the next tag.

---

## Task 0: Gate harness

**Files:**
- Create: `scripts/eval/agent-replay/session.py` (one `claude -p` session, one record)
- Create: `scripts/eval/agent-replay/batch.py` (a batch: resume, concurrency, incomplete handling)
- Create: `scripts/eval/agent-replay/strip_proxy.py` (text channel)
- Create: `scripts/eval/agent-replay/retrieval_diff.py` (G1 and the consumer check)
- Create: `scripts/eval/agent-replay/consumer_check.mjs` (the `JSON.stringify` half)
- Create: `scripts/eval/agent-replay/compare.py` (G2 pass rules and the report)
- Create: `scripts/eval/agent-replay/judge.py` (judge sessions through `claude -p`)
- Create: `scripts/eval/agent-replay/seed_review.py` (empties the real queue in a template, then imports the fixture archive with `--restore` into `project:gate-review`)
- Create: `scripts/eval/agent-replay/fixtures/review.archive.json` (15 to 20 keys per review task, old `created_at` values)
- Create: `scripts/eval/agent-replay/tasks.example.jsonl` (against `scripts/seed/seed.py`)
- Create: `scripts/eval/agent-replay/test_compare.py`
- Create: `docs/eval-agent-replay.md`

**Interfaces produced.**

A task line:

```json
{"id": "review-1", "group": "review", "decision": true, "channels": ["structured", "text"],
 "prompt": "Work my review queue.", "skill": "client/skills/lr-review/SKILL.md",
 "items": [{"item": "conflict:a:b", "kind": "verdict"}, {"item": "stale:c", "kind": "verdict"}],
 "seeded_keys": ["conflict:a:b", "stale:c"]}
```

A run record, at `~/lumberroom-gate/2026-10-mcp-response-tokens/runs/<stage>/<arm>/<channel>/<task>/<rep>.json`:

```json
{
  "stage": "sonnet", "arm": "A1", "channel": "structured", "model": "claude-sonnet-5-5",
  "task": "review-1", "rep": 2, "status": "complete", "incomplete_reason": null,
  "calls": [{"tool": "review_queue", "args": {}}, {"tool": "review_decide", "args": {"key": "conflict:a:b", "verdict": "supersede", "keep": "b"}}],
  "received_keys": ["conflict:a:b"],
  "end_state": {
    "written": [{"namespace": "global", "supersedes": "b", "content": "..."}],
    "superseded": ["a"], "deleted": [],
    "verdicts": {"conflict:a:b": "supersede"}
  },
  "outcomes": {"conflict:a:b": "supersede:b", "stale:c": "resolved_elsewhere"},
  "answer": "final assistant text",
  "usage": {"input": 41230, "cache_read": 35100, "output": 1650},
  "transcript": "transcripts/<same path>.jsonl"
}
```

`status` is `complete` or `incomplete`; `incomplete_reason` is one of `usage_limit`, `refusal`, `mcp_error`, `timeout`, `bad_init`, `bad_output`. An outcome is the verdict or write, `unseen`, or `resolved_elsewhere` (spec G2, Items and outcomes).

**Session command** (`session.py`), exactly as spec G2 "Sessions" gives it: a fresh `mktemp -d /tmp/lr-gate.XXXXXX` working directory, `env -u ANTHROPIC_API_KEY`, `--setting-sources project --strict-mcp-config --mcp-config <slot>/mcp.json --tools "" --allowedTools <lumberroom tools> --no-session-persistence --no-chrome --disable-slash-commands --include-hook-events --output-format stream-json --verbose --model <model>`. The prompt is `client/CLAUDE.md.snippet` plus the task; review tasks append the `lr-review` text from the arm's own checkout. The `init` check rejects a session per spec G2.

- [ ] **Step 1: Write the failing compare tests**

```python
# scripts/eval/agent-replay/test_compare.py
from compare import item_agreement, item_fails, unseen_ok, calls_ok, scored, terse_ok

def run(arm, task, outcomes, calls=4, status="complete"):
    return {"arm": arm, "task": task, "channel": "structured", "status": status,
            "outcomes": outcomes, "calls": [{"tool": "t", "args": {}}] * calls}

def test_item_agreement_averages_per_item_against_the_a0_mode():
    a0 = [run("A0", "t", {"k1": "x", "k2": "y"}) for _ in range(3)]
    a1 = [run("A1", "t", {"k1": "x", "k2": "z"}), run("A1", "t", {"k1": "x", "k2": "y"})]
    assert abs(item_agreement(a0, a1) - 0.75) < 1e-9

def test_an_item_fails_only_on_a_strong_old_mode_and_a_weak_new_one():
    def five(arm, hits):
        return [run(arm, "t", {"k": "x" if i < hits else "y"}) for i in range(5)]
    assert item_fails(five("A0", 4), five("A0b", 5), five("A1", 2), "k")
    assert not item_fails(five("A0", 4), five("A0b", 5), five("A1", 3), "k")
    assert not item_fails(five("A0", 3), five("A0b", 5), five("A1", 0), "k")

def test_a_tied_mode_accepts_any_tied_outcome():
    a0 = [run("A0", "t", {"k": "x"}), run("A0", "t", {"k": "y"})]
    assert item_agreement(a0, [run("A1", "t", {"k": "y"})]) == 1.0

def test_terseness_allows_the_old_spread_plus_ten_percent():
    assert terse_ok(a0_mean=100, a0b_mean=110, a1_mean=120)
    assert not terse_ok(a0_mean=100, a0b_mean=110, a1_mean=122)

def test_unseen_keys_may_not_exceed_the_floor():
    a0b = [run("A0b", "t", {"k1": "unseen", "k2": "x"})]
    assert unseen_ok(a0b, [run("A1", "t", {"k1": "unseen", "k2": "x"})])
    assert not unseen_ok(a0b, [run("A1", "t", {"k1": "unseen", "k2": "unseen"})])

def test_calls_within_one_or_twenty_percent_or_the_a0_spread():
    a0 = [run("A0", "t", {}, calls=5)] * 3
    a0b = [run("A0b", "t", {}, calls=5)] * 3
    assert calls_ok(a0, a0b, [run("A1", "t", {}, calls=6)])
    assert not calls_ok(a0, a0b, [run("A1", "t", {}, calls=8)])

def test_incomplete_runs_never_score():
    assert scored([run("A1", "t", {"k": "x"}, status="incomplete")]) == []
```

- [ ] **Step 2: Run it and watch it fail**

Run: `python3 -I -m pytest scripts/eval/agent-replay/test_compare.py -q`
Expected: FAIL, `ModuleNotFoundError: No module named 'compare'`.

- [ ] **Step 3: Implement**, one requirement per line:
  - `session.py`: one session per call; checks the `init` event; parses stream-json for tool calls, tool results, usage and the final message; marks `incomplete` on a usage-limit message, a refusal, an MCP error, a rejected `init`, a 20-minute timeout or output it cannot parse; derives `received_keys` from every `review_queue` result; derives `end_state` from a database diff between the slot's start and end, counting only rows the gate token's client changed; maps it to per-item `outcomes`.
  - `seed_review.py`: per spec G2 "Empty the real queue" and "Seed the review fixtures"; it asserts after seeding that `review_queue` with `limit: 200` returns only `project:gate-review` keys.
  - `batch.py`: takes a batch file of (stage, arm, channel, task, rep); skips every run that already has a complete record; runs at most `--concurrency` sessions (default 3); restores the slot before each session; interleaves arms; stops the batch at the first `usage_limit` and leaves the rest for the next window; reruns incomplete runs up to three times across batches.
  - `strip_proxy.py`: forwards HTTP to the arm and deletes `result.structuredContent` from `tools/call` answers; nothing else.
  - `retrieval_diff.py`: G1 as the spec says, both builds with `SEARCH_USAGE_WEIGHT=0`, the `left_out` counts and bytes per project, then the consumer check with Python `json.dumps(structured)` at default arguments and `consumer_check.mjs` running `JSON.stringify(structured ?? {})`, then `scripts/policy-test.sh`; exit 1 on any difference.
  - `compare.py`: the seven G2 rules, scored per task group, with tied modes as spec G2 defines them and text-channel runs scored against A0's text-channel runs; a markdown report with the token table and the tool-frequency list; exit 1 on a failed rule.
  - `judge.py`: one `claude -p --model claude-opus-5-5` session per task, channel and compared arm, with no MCP server and no tools; the prompt holds the task, A0's answers and the answers under test, and asks for `{"equivalent": [true, false, ...], "reasons": ["..."]}` on one line; a reply that does not parse reruns once, then records incomplete.
  - Records and transcripts live under `~/lumberroom-gate/2026-10-mcp-response-tokens/`, never under `~/.cache/agent-scratch`.

- [ ] **Step 4: Run the tests**

Run: `python3 -I -m pytest scripts/eval/agent-replay/test_compare.py -q`
Expected: `7 passed`.

- [ ] **Step 5: Write `docs/eval-agent-replay.md`**: slots, the probe, batches and resume, the corpus conversion, reading the report, and the cannot-show list from spec G2.

- [ ] **Step 6: Hand back** the JSON report. The lead commits.

---

## Task 1: Lock interfaces (lead)

**Files:** as in the table. Exact items:

```rust
// src/mcp/views.rs, appended
use crate::domain::types::Invocation;
use crate::services::bootstrap::Digest;
use crate::services::search::SearchResult;

/// How much of an answer a caller asked for. Brief is what a model reads unless it asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Detail {
    #[default]
    Brief,
    Full,
}

/// An installed CLI parses `score` as required and cannot send `detail`, so a declared CLI call
/// reads the full shape. Remove once a CLI that sends `detail` itself has shipped in two tags.
pub fn detail_for(invocation: Invocation, asked: Option<Detail>) -> Detail {
    asked.unwrap_or(if invocation == Invocation::Cli { Detail::Full } else { Detail::Brief })
}

/// `rows` for `context_bootstrap`, by the rule `detail_for` follows.
pub fn rows_for(invocation: Invocation, asked: Option<bool>) -> bool {
    asked.unwrap_or(invocation == Invocation::Cli)
}

/// The text channel's copy of a JSON answer.
pub fn compact(json: &serde_json::Value) -> String {
    serde_json::to_string(json).unwrap_or_default()
}

/// `memory_search`'s structured answer.
pub fn search(result: &SearchResult, detail: Detail) -> serde_json::Value {
    todo!()
}

/// `memory_history`'s structured answer.
pub fn timeline_value(t: &TimelineView, detail: Detail) -> serde_json::Value {
    todo!()
}

/// `context_bootstrap`'s structured answer. Brief carries the text, the counts and every row the
/// text did not print whole; `rows` true is the `Digest` without `tenant`.
pub fn digest(d: &Digest, rows: bool) -> serde_json::Value {
    todo!()
}

pub const DIGEST_MORE: &str =
    "rows: true adds each printed fact's id and exact text; memory_search finds more";
```

```rust
// src/services/bootstrap.rs, additions
/// What one render printed, and which rows it did not print whole.
pub struct Rendered {
    pub text: String,
    /// Memory ids from profile, project_context and recent that the layout left out or shortened.
    pub left_out: Vec<String>,
    /// `(namespace, kind, key)` of registry entries the layout left out.
    pub left_out_registry: Vec<(String, String, String)>,
}

pub fn render_report(d: &Digest, max_chars: usize) -> Rendered {
    todo!()
}

// Digest gains two fields. #[serde(skip)] keeps the service serialization unchanged; views::digest
// adds them to the MCP shapes.
    #[serde(skip)]
    pub left_out: Vec<String>,
    #[serde(skip)]
    pub left_out_registry: Vec<(String, String, String)>,
```

```rust
// src/mcp/review_view.rs
//! `review_queue`'s structured answer: brief rows, every free-text value fenced (decision 0031).

use crate::services::review_queue::{Queue, Source};

/// Items an MCP call reads in all when it names no `limit`, split across the sources it reads.
pub const MCP_QUEUE_BUDGET: i64 = 10;

/// Verdicts remove items from the live queue, so the hint names no offset.
pub const QUEUE_MORE: &str =
    "items you leave stay first; offset N starts after the first N items of every source.";

pub fn default_limit(sources_read: usize) -> i64 {
    todo!()
}

pub fn sources_read(asked: Option<&[Source]>, proposal_sources: usize) -> usize {
    todo!()
}

pub fn queue(q: &Queue) -> serde_json::Value {
    todo!()
}
```

```rust
// src/domain/fence.rs
//! The data fence `review_queue` draws, recognised on the way back in (decision 0031).

pub const CONTENT_CARRIES_FENCE: &str = "content_carries_fence";

/// True when any line is `----- data <12 hex> -----` or `----- end of data <12 hex> -----`.
pub fn carries_fence(content: &str) -> bool {
    todo!()
}
```

Argument fields with the spec D9 doc comments verbatim: `BootstrapArgs.rows: Option<bool>`, `SearchArgs.detail: Option<views::Detail>` and the new `SearchArgs.limit` doc, `MemoryHistoryArgs.detail: Option<super::views::Detail>`, the new `ReviewQueueArgs.limit` doc. `review_queue::fence_nonce` and `as_data` (`:1211-1217`) become `pub(crate)`.

- [ ] **Step 1:** Make the edits.
- [ ] **Step 2:** `./scripts/cargo.sh check`. Expected: clean apart from `todo!()` warnings.
- [ ] **Step 3:** `./scripts/cargo.sh test --test mcp_tool_annotations`. Expected: pass.
- [ ] **Step 4:** Commit: `Lock the brief answer interfaces`.

---

## Task 2: Search, history and digest views

**Files:** `src/mcp/views.rs` (bodies and tests).

**Interfaces:** consumes task 1's signatures, `Hit` (`src/services/search.rs:57-86`), `TimelineView`, and `Digest` with `left_out` and `left_out_registry`. `Detail::Full` returns `serde_json::to_value(x)` for search and history.

Brief rules on the `serde_json::Value` of the full shape:
- search hits: remove `score` and `primary`; remove `sensitivity` when `"open"`; remove `tags` when empty. Timestamps untouched.
- history versions: remove `embedding_model`, `access_count`, `last_accessed_at`; remove `sensitivity` when `"open"`; remove `tags` when empty; remove `supersedes`, `superseded_by`, `superseded_at`, `last_confirmed_at` when null. Timestamps untouched.
- digest brief: `{"counts": {"memories", "registry"}, "inventory", "sealed_inventory", "project", "text"}`; `left_out` as `[{id, namespace, content}]` plus `sensitivity` above open and `tags` when non-empty, looked up in profile, project_context and recent by `d.left_out`, in `d.left_out`'s order, when non-empty; `left_out_registry` as the `RegistrySummary` objects named by `d.left_out_registry`, when non-empty; `"more": DIGEST_MORE` when any section holds a fact.
- digest full (`rows` true): `to_value(d)` minus `tenant`, plus `left_out` and `left_out_registry` as id lists.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_brief_search_keeps_ids_in_order_and_drops_only_the_listed_fields() {
    let result = search_fixture(); // three hits: one private, one with occurred_at, one untagged
    let full = search(&result, Detail::Full);
    let brief = search(&result, Detail::Brief);
    let ids = |v: &serde_json::Value| -> Vec<String> {
        v["hits"].as_array().unwrap().iter().map(|h| h["id"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(ids(&full), ids(&brief));
    assert_eq!(full, serde_json::to_value(&result).unwrap());
    for (f, b) in full["hits"].as_array().unwrap().iter().zip(brief["hits"].as_array().unwrap()) {
        assert!(b.get("score").is_none() && b.get("primary").is_none());
        for kept in ["similarity", "content", "source", "created_at", "id", "namespace"] {
            assert_eq!(b[kept], f[kept], "{kept}");
        }
        assert_eq!(b.get("occurred_at"), f.get("occurred_at"), "full precision kept");
        assert_eq!(b.get("sensitivity").is_some(), f["sensitivity"] != "open");
    }
}

#[test]
fn a_brief_history_keeps_the_chain_and_drops_the_counters() {
    let t = timeline_view(Timeline { versions: vec![row()], withheld: 0, depth_capped: false }, &names());
    let brief = timeline_value(&t, Detail::Brief);
    let v = &brief["versions"][0];
    for gone in ["embedding_model", "access_count", "last_accessed_at"] {
        assert!(v.get(gone).is_none(), "{gone} survived: {v}");
    }
    let full = timeline_value(&t, Detail::Full);
    assert_eq!(full, serde_json::to_value(&t).unwrap());
    assert_eq!(v["created_at"], full["versions"][0]["created_at"]);
    assert_eq!(v["occurred_at"], full["versions"][0]["occurred_at"]);
}

#[test]
fn a_brief_digest_carries_every_row_the_text_left_out_whole() {
    let mut d = digest_fixture(); // profile p1, p2; recent r1; tenant "t-1"
    d.left_out = vec!["p2".into(), "r1".into()];
    let brief = digest(&d, false);
    let left: Vec<&str> =
        brief["left_out"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(left, ["p2", "r1"]);
    assert_eq!(brief["left_out"][0]["content"], d.profile[1].content.as_str());
    assert!(brief.get("profile").is_none() && brief.get("tenant").is_none());
    assert!(brief["counts"].get("by_namespace").is_none());
    assert_eq!(brief["inventory"], serde_json::to_value(&d.inventory).unwrap());
    assert_eq!(brief["sealed_inventory"], serde_json::to_value(&d.sealed_inventory).unwrap());
    assert_eq!(brief["text"], d.text.as_str());
    assert_eq!(brief["more"], DIGEST_MORE);
}

#[test]
fn a_digest_that_printed_everything_carries_no_left_out_key() {
    let d = digest_fixture();
    assert!(digest(&d, false).get("left_out").is_none());
}

#[test]
fn the_full_digest_drops_only_the_tenant() {
    let d = digest_fixture();
    let full = digest(&d, true);
    let mut expected = serde_json::to_value(&d).unwrap();
    expected.as_object_mut().unwrap().remove("tenant");
    for (k, v) in expected.as_object().unwrap() {
        assert_eq!(full.get(k), Some(v), "{k}");
    }
    assert!(full.get("tenant").is_none());
}

#[test]
fn a_cli_call_reads_the_full_shape_unless_it_asks_otherwise() {
    assert_eq!(detail_for(Invocation::Cli, None), Detail::Full);
    assert_eq!(detail_for(Invocation::Hook, None), Detail::Brief);
    assert_eq!(detail_for(Invocation::User, None), Detail::Brief);
    assert_eq!(detail_for(Invocation::Cli, Some(Detail::Brief)), Detail::Brief);
    assert!(rows_for(Invocation::Cli, None) && !rows_for(Invocation::Model, None));
}
```

- [ ] **Step 2:** Scratch run in `~/.cache/agent-scratch/task-2/`. Expected: FAIL on `todo!()`.
- [ ] **Step 3:** Implement by the rules above.
- [ ] **Step 4:** Scratch run passes. `./scripts/cargo.sh check`, grep for `src/mcp/views.rs`. Delete the scratch directory.
- [ ] **Step 5:** Return the report.

---

## Task 2b: `render_report`

**Files:** `src/services/bootstrap.rs`.

`Entry` (`:398-407`) gains `id: Option<String>` for memory entries and `registry: Option<(String, String, String)>` for registry entries. `render_report` runs the layout `render` runs today and collects every memory id whose final `Pick` is not `Whole`, plus the ids of section facts that never became an entry for any reason other than the `seen` dedupe (a fact printed whole in an earlier section is printed). Registry entries whose final pick is `Out` go to `left_out_registry`. `render_report` covers `render`'s three returns: the full digest (`:726`), a tail mode (`:741-746`) and the header-only budget (`:750-756`). `render` becomes `render_report(d, max_chars).text`, so its output stays byte-identical. A multi-line fact that `one_line` flattened counts as printed whole (spec D4). `run` sets `digest.left_out` and `digest.left_out_registry` from the same call that sets `digest.text`.

- [ ] **Step 1: Failing tests** in the module's `mod tests`:

```rust
#[test]
fn render_report_prints_the_same_text_as_render() {
    let d = overflowing_digest(); // the fixture behind the "more profile entries left out" test
    assert_eq!(render_report(&d, 1000).text, render(&d, 1000));
}

#[test]
fn render_report_names_every_row_not_printed_whole_and_nothing_printed_whole() {
    let d = overflowing_digest();
    let r = render_report(&d, 1000);
    for f in d.profile.iter().chain(&d.project_context).chain(&d.recent) {
        let whole = r.text.lines().any(|l| l.starts_with("- ") && l.contains(&one_line(&f.content)));
        assert_eq!(!whole, r.left_out.contains(&f.id), "{}", f.id);
    }
}

#[test]
fn a_tail_mode_render_reports_what_it_left_out() {
    let d = overflowing_digest();
    let r = render_report(&d, 1000); // returns from the tail-mode loop, :741-746
    assert!(r.text.contains("left out at 1,000 chars") && !r.left_out.is_empty());
}

#[test]
fn a_header_only_budget_leaves_every_row_out() {
    let d = overflowing_digest();
    let r = render_report(&d, 0); // no room for the header and footer: the return at :750-756
    let all: Vec<&String> =
        d.profile.iter().chain(&d.project_context).chain(&d.recent).map(|f| &f.id).collect();
    for id in all {
        assert!(r.left_out.contains(id), "{id}");
    }
}

#[test]
fn a_digest_that_fits_leaves_nothing_out() {
    let d = small_digest();
    let r = render_report(&d, 6000);
    assert!(r.left_out.is_empty() && r.left_out_registry.is_empty());
}
```

- [ ] **Steps 2 to 5:** scratch fail, implement, scratch pass, report. The fork's `render_within` calls `render`; its change is in the fork plan.

---

## Task 3: Review queue view, extra-tools handlers, `lr-review`

**Files:** `src/mcp/review_view.rs`, `src/mcp/extra_tools.rs` (handlers `:181-461`; delete `strip_free_text` at `:548-567`), `client/skills/lr-review/SKILL.md`.

**Interfaces.** Handlers return:
- `memory_history`: `let json = views::timeline_value(&timeline, views::detail_for(ctx.invocation, args.detail)); Ok((views::compact(&json), json))`.
- `registry_history`, `registry_set`, `alias_set`, `alias_list`, `review_decide`: today's JSON, text through `views::compact`.
- `review_queue`: `limit` = `args.limit.or(Some(review_view::default_limit(review_view::sources_read(sources.as_deref(), proposals.len()))))`; text = `review_queue::render(&envelope)` plus `"more: "` and `QUEUE_MORE` on a last line when `has_more`; structured = `review_view::queue(&envelope)`.

Brief queue rules: rows drop `access_count` and `last_accessed_at`; `sensitivity` only above `open`; `opened` only when false; timestamps untouched; `content`, `proposal.proposed_content`, each `proposal.fields[].label` and `.value` become `as_data(s, &fence_nonce())`, one nonce each; `key`, `origin`, `kind`, `version`, `held_by`, `verdicts`, `dates` untouched; `"more": QUEUE_MORE` when `has_more`.

`lr-review` edits, per spec D5: the undated section's paging sentence, and the "Large queues" trigger, which sizes each source with `offset: 40, limit: 1`. Every other line of the skill stays word for word.

- [ ] **Step 1: Failing tests** in `review_view.rs`:

```rust
/// `^----- data ([0-9a-f]{12}) -----\n(.*)\n----- end of data \1 -----$` in dotall mode, matched
/// by hand: the engine takes no regex crate for one pattern.
fn fenced(s: &str) -> Option<(String, String)> {
    let rest = s.strip_prefix("----- data ")?;
    if rest.len() < 12 {
        return None;
    }
    let (nonce, rest) = rest.split_at(12);
    if !nonce.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)) {
        return None;
    }
    let body = rest.strip_prefix(" -----\n")?;
    let body = body.strip_suffix(&format!("\n----- end of data {nonce} -----"))?;
    Some((nonce.to_string(), body.to_string()))
}

#[test]
fn every_free_text_value_matches_the_fence_and_keeps_its_source_text() {
    let q = queue_fixture(); // one stale row, one proposal with proposed_content and one field
    let v = queue(&q);
    let p = q.items[1].proposal.as_ref().unwrap();
    let checks = [
        (&v["items"][0]["rows"][0]["content"], q.items[0].rows[0].content.clone()),
        (&v["items"][1]["proposal"]["proposed_content"], p.proposed_content.clone().unwrap()),
        (&v["items"][1]["proposal"]["fields"][0]["label"], p.fields[0].label.clone()),
        (&v["items"][1]["proposal"]["fields"][0]["value"], p.fields[0].value.clone()),
    ];
    let mut nonces = std::collections::HashSet::new();
    for (value, source) in checks {
        let (nonce, body) = fenced(value.as_str().unwrap()).expect("fenced");
        assert_eq!(body, source);
        assert!(nonces.insert(nonce), "a nonce repeated");
    }
    assert_eq!(v["items"][1]["proposal"]["version"], "v1");
}

#[test]
fn the_default_splits_ten_across_the_sources_read() {
    assert_eq!(default_limit(1), 10);
    assert_eq!(default_limit(2), 5);
    assert_eq!(default_limit(3), 4);
    assert_eq!(default_limit(6), 2);
    assert_eq!(default_limit(0), 10);
}

#[test]
fn sources_read_counts_every_filled_proposal_source() {
    assert_eq!(sources_read(None, 0), 2);
    assert_eq!(sources_read(None, 1), 3);
    assert_eq!(sources_read(Some(&[Source::Undated]), 1), 1);
    assert_eq!(sources_read(Some(&[Source::Proposal, Source::Stale]), 2), 3);
}

#[test]
fn more_names_no_offset_and_appears_only_when_more_waits() {
    let mut q = queue_fixture();
    q.has_more = true;
    assert_eq!(queue(&q)["more"], QUEUE_MORE);
    assert!(!QUEUE_MORE.chars().any(|c| c.is_ascii_digit()));
    q.has_more = false;
    assert!(queue(&q).get("more").is_none());
}

#[test]
fn a_brief_row_drops_the_read_counters_and_keeps_its_times() {
    let q = queue_fixture();
    let row = &queue(&q)["items"][0]["rows"][0];
    assert!(row.get("access_count").is_none() && row.get("last_accessed_at").is_none());
    assert_eq!(row["created_at"], q.items[0].rows[0].created_at.as_str());
}
```

- [ ] **Step 2:** Scratch run. Expected: FAIL on `todo!()`.
- [ ] **Step 3:** Implement. `default_limit`:

```rust
pub fn default_limit(sources_read: usize) -> i64 {
    let n = i64::try_from(sources_read.max(1)).unwrap_or(1);
    ((MCP_QUEUE_BUDGET + n - 1) / n).max(2)
}
```

  Then the handlers, the deletion and the two skill edits.
- [ ] **Step 4:** Scratch pass; `./scripts/cargo.sh check`; delete the scratch directory.
- [ ] **Step 5:** Return the report with a `wire_in` for `tests/review_queue_mcp.rs:487` (task 5).

---

## Task 3b: A write refuses fence lines

**Files:** `src/domain/fence.rs`, `src/services/write.rs` (the check in `run_inner`, `:95`, before the length check; `run_inner` gains a `FenceLines` argument, `Refuse` from `run` and `run_observed`, `Allow` from `run_restored`, which only the archive merge calls), `src/services/archive.rs` (comment only). Ingest approval (`src/services/ingest.rs:506`) keeps `run_observed` and the check.

```rust
pub fn carries_fence(content: &str) -> bool {
    content.lines().any(|line| {
        let rest = line
            .strip_prefix("----- end of data ")
            .or_else(|| line.strip_prefix("----- data "));
        rest.and_then(|r| r.strip_suffix(" -----")).is_some_and(|n| {
            n.len() == 12 && n.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        })
    })
}
```

The refusal: `DomainError::validation("content holds a data fence line; send the text between the markers").with_code(fence::CONTENT_CARRIES_FENCE)`.

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn a_fence_line_is_recognised_and_a_sentence_about_fences_is_not() {
    assert!(carries_fence("x\n----- data 3f9a1c07b2e4 -----\ny"));
    assert!(carries_fence("----- end of data 3f9a1c07b2e4 -----"));
    assert!(!carries_fence("the review queue draws ----- data blocks around rows"));
    assert!(!carries_fence("----- data 3F9A1C07B2E4 -----"));
    assert!(!carries_fence("----- data 3f9a -----"));
}
```

  In `write.rs`'s tests: `run` and `run_observed` refuse fenced content with `CONTENT_CARRIES_FENCE`; `run_restored` stores it.
- [ ] **Steps 2 to 5:** scratch fail, implement, scratch pass, report.

---

## Task 4: CLI and the policy test

**Files:** `crates/lumberroom/src/wire.rs:40-46, 537-549, 595-598`, `crates/lumberroom/src/format.rs:38-40`, `crates/lumberroom/src/commands.rs`, `crates/lumberroom/src/eval/runner.rs`, `crates/lumberroom/src/eval/corpus.rs`, `crates/lumberroom/tests/wire.rs`, `scripts/policy-test.sh`.

```rust
// wire::Hit
    /// Absent on a brief answer: a call made without `detail: "full"` under `--invocation user`
    /// or a hook.
    #[serde(default)]
    pub score: Option<f64>,

// SearchArgsRequest gains
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

// BootstrapArgsRequest gains
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<bool>,

// format.rs
pub fn search_line(score: Option<f64>, namespace: &str, content: &str) -> String {
    match score {
        Some(s) => format!("{s:.3}  [{namespace}] {content}"),
        None => format!("-      [{namespace}] {content}"),
    }
}
```

Every search call sets `detail: Some("full".into())`; `bootstrap --json` sets `rows: Some(true)`; `--hook` and the plain text path leave `rows` unset. `eval/runner.rs` keeps scoring by rank and reads `score` through the `Option`.

`scripts/policy-test.sh`, before the first `json_has_key ... inventory` (`:369`): fail when `bootstrap-narrow.json` has no `inventory` key, with the message "the digest payload carries no inventory key, so the leak checks below would pass without checking anything".

- [ ] **Step 1:** Update `crates/lumberroom/tests/wire.rs`: search requests carry `detail`, `--json` bootstrap requests carry `rows`, and a hit without `score` parses. Expected to fail first.
- [ ] **Step 2:** Make the edits.
- [ ] **Step 3:** `./scripts/cargo.sh check -p lumberroom`. Return the report; the lead runs `cargo test -p lumberroom` and `scripts/policy-test.sh`.

---

## Task 5: Wiring, integration tests, docs (lead)

- [ ] **Step 1:** Handler bodies in `src/mcp/mod.rs`:

```rust
// context_bootstrap (:239-250)
let digest = bootstrap::run(&ctx, args.project.as_deref()).await?;
let rows = views::rows_for(ctx.invocation, args.rows);
Ok((digest.text.clone(), views::digest(&digest, rows)))

// memory_search (:262-295), after search::run_tagged
let json = views::search(&result, views::detail_for(ctx.invocation, args.detail));
Ok((views::compact(&json), json))

// memory_write (:344-345) and registry_get (:379-380): to_string_pretty becomes views::compact
```

- [ ] **Step 2:** `tests/mcp_brief.rs`, over HTTP like `tests/mcp_source.rs`:
  - `a_model_search_reads_the_brief_hit_and_a_cli_search_reads_score`: one query with no header and with `x-memory-invocation: cli`; ids equal in order; only the CLI answer has `score`.
  - `the_bootstrap_text_is_the_same_on_both_shapes`.
  - `a_digest_that_overflows_carries_its_left_out_rows_whole`: rows past 6,000 characters; every id the full shape's `left_out` lists appears in the brief `left_out` with its stored content.
  - `an_unknown_argument_is_ignored`: `memory_search` with `{"query": "x", "frobnicate": 1}` succeeds.
  - `an_as_of_from_a_brief_created_at_still_finds_the_row`: take `created_at` from a brief hit, search with it as `as_of`, and the row comes back.
  - `a_merge_with_fenced_content_is_refused`: a `review_decide` merge whose content holds a fence line answers `content_carries_fence`.
- [ ] **Step 3:** `tests/review_queue_mcp.rs:487` becomes `every_free_text_value_in_the_structured_copy_sits_in_a_fence`, per spec section 5. `tests/mcp_source.rs:402` passes `{"rows": true}`.
- [ ] **Step 4:** The `src/services/bootstrap.rs:22-23` comment per spec D4. Decision 0031 in the shape of 0001: decision; context; what lost (field lists, a total queue cap, an offset-naming hint, line-per-hit text, omitting `structuredContent`, timestamp truncation); costs accepted (digest ids behind `rows`, the CLI invocation default, the `left_out` share of the saving); not for; reversal (D2, D4's F1, D10). A dated line at the top of 0019 marks its stripping clause superseded by 0031. README row.
- [ ] **Step 5:** CHANGELOG, Unreleased. Changed: the brief shapes, the opt-ins, `left_out`, the review default and hint, compact text, the CLI default, `content_carries_fence`. Removed: `tenant` from `context_bootstrap`. A line for `--json` users and for the plugin snapshot refresh (spec section 11).
- [ ] **Step 6:** `./scripts/cargo.sh test`. Expected: full suite passes. `grep -rnP '\x{2014}' src/mcp src/domain/fence.rs tests/mcp_brief.rs docs/decisions/0031-*.md CHANGELOG.md client/skills/lr-review/SKILL.md`. Expected: nothing.
- [ ] **Step 7:** Commit per the PR grouping.

---

## Task 5c: Corpus, templates, slots and probe on legolas (lead; haiku runs commands)

Everything in spec G2 "Sessions", "Probe", "Slots" and "Corpus", in this order:

- [ ] **Step 1:** Check the corpus files under `~/lumberroom-gate/2026-10-mcp-response-tokens/corpus/` against the sha256 prefix `e218aa0a` and `manifest.json`. A mismatch stops the gate.
- [ ] **Step 2:** Fork template: restore into a `pgvector/pgvector:pg16` container on an `--internal` network, apply `settings.sql`, delete every tenant but `73740ec4`, seed the synthetic teams tenant (fork plan F6), mint a gate token per tenant, empty the real queue and seed fixtures with `seed_review.py`.
- [ ] **Step 3:** Engine template: export `73740ec4` from a fork build over the fork template; import into an engine build with `archive import --restore`; export the engine template again and diff row by row against the fork export (id, namespace, content, tags, created_at, occurred_at, occurred_until, supersedes). A difference stops the gate. Then `seed_review.py`, and the engine gate token in `AUTH_TOKENS`.
- [ ] **Step 4:** Probe per spec G2 on the A0 build, both channels, and again on A1. Record flags, `init` events and the two probe answers in `~/lumberroom-gate/2026-10-mcp-response-tokens/probe.md`.
- [ ] **Step 5:** When the gate ends, delete `corpus/` and the templates. Keep `runs/` for the owner.

---

## Task 6: Gate A0, A0b, A1 (lead runs; haiku collates)

- [ ] **Step 1:** Build A0 from PR 2's base and A1 from PR 3's head.
- [ ] **Step 2:** `retrieval_diff.py` (G1 and the consumer check), both builds with `SEARCH_USAGE_WEIGHT=0`. Expected: exit 0. Any difference stops the gate.
- [ ] **Step 3:** Sonnet stage in batches: 426 sessions plus 38 judge sessions after it. Batch 1 runs 40 sessions; the lead sizes later batches from its usage. Every batch interleaves A0, A0b and A1. Then `compare.py`. Expected: exit 0.
- [ ] **Step 4:** Opus stage, only after step 3 passes: 99 sessions. Then `compare.py`. Expected: exit 0.
- [ ] **Step 5:** Haiku collates `docs/results/2026-10-mcp-response-tokens-gate.md`: G1 counts with `left_out` per project, each G2 rule with its numbers, the token table, sessions run and incomplete, the cannot-show list. The lead checks every number against the records.

A failure on "supersede a fact the digest shows" switches on fallback F1 (spec D4) and reruns that group.

---

## Task 7: Description trim

**Files:** `description = "..."` strings and argument doc comments in `src/mcp/mod.rs` and `src/mcp/extra_tools.rs`. Not `OCCURRED_AT_DESCRIPTION` or `AS_OF_DESCRIPTION`.

- [ ] **Step 1:** Measure the description and schema-doc characters from `tool_definitions()` with an `#[ignore]` test in `extra_tools.rs`'s test module.
- [ ] **Step 2:** Cut by spec section 10. Keep every phrase the tests at `extra_tools.rs:664-718` and `tests/mcp_tool_annotations.rs` assert, and `memory_write`'s `possible_conflicts` and `supersedes` sentences whole.
- [ ] **Step 3:** Measure again; target about 30 percent less.
- [ ] **Step 4:** Return a table: tool, sentence removed, reason, characters saved.

---

## Task 8: Gate A2 (lead runs; haiku collates)

Task 6 steps 3 to 5 for arm A2 on the structured channel, compared with A1, with A0b's agreement as the floor. G1 is skipped: descriptions do not touch retrieval. A failure names the sentence group to restore; the lead restores it and reruns that group.

---

## Self-review

- Spec coverage: D1 tasks 1 to 3; D2 tasks 1, 4, 5; D3 task 2; D4 tasks 2, 2b, 5; D5 task 3; D6 tasks 2, 3, 5; D7 task 2; D8 tasks 3, 5; D8a task 3; D9 task 1; D10 no code; D11 tasks 3b, 5; section 5 pins tasks 3, 4, 5, 7; G0 tasks 2, 2b, 3, 3b; G1 and G2 tasks 0, 5c, 6, 8; section 11 in task 5's notes.
- Names across tasks: `Detail`, `detail_for`, `rows_for`, `compact`, `search`, `timeline_value`, `digest`, `DIGEST_MORE`, `Rendered`, `render_report`, `Digest.left_out`, `Digest.left_out_registry`, `MCP_QUEUE_BUDGET`, `QUEUE_MORE`, `default_limit`, `sources_read`, `queue`, `carries_fence`, `CONTENT_CARRIES_FENCE`. Task 1 defines each.
- The task file and the corpus stay outside the repository by design (spec G2).
