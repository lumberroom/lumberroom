# Phase 6. Ingestion

> **Ends when:** one week of one project's transcripts produces a proposal list the owner reads in
> a sitting, approving one lands a fact through the same write path a model uses, and a second
> ingest run over the same corpus adds nothing.

This criterion is written here rather than quoted. The system PRD stops at five phases and §5 states the
system is **not a memory extraction engine**, so this spec owes the PRD a reconciliation before it
owes anything else. §1 pays it.

Status: specified 19 August 2026, revised 20 August 2026 against two reviews and again the same day
against live provider measurements (§10.10, §10.11). Implemented against this design since: the
`ingest_proposal` and related tables exist, `crates/lumberroom/src/ingest/` and the `/admin/ingest`
routes exist, and the `lumberroom-ingest` skill exists. None of it has been run end to end against a
real transcript directory; [`docs/ingestion.md`](../ingestion.md) carries the current status and the
gate that would settle it.

The revision resolves four items that make the difference between a design and a working one, and
each is marked where it lands: the recall-emission layer was unreachable as wired (§4.4), a partial
extraction run lost transcript bytes with no recovery (§8.3, §10.7), the artifact marker could
blacklist the owner's own session forever (§7.2), and an interrupted run fenced off every later
entry in that session with no counter and no bound (§7.3).

Three sources of evidence run through this document and they are kept apart. "The agentmemory pass"
means the study of `agentmemory` 0.9.22 written on 19 August 2026, whose corpus measurements are
first-hand; that study belongs at `docs/research/agentmemory.md` and **is not in the tree yet**,
so every figure credited to it is unciteable until it lands. "Measured here" means a figure taken
while writing this spec, against the owner's own `~/.claude` and `~/.codex`, and the command that
produced it is described in the sentence that uses it. "Measured on 20 August 2026" means a live
HTTP call to the provider endpoint named in the sentence, made with a real key; §10.10 carries those
figures and §10.11 carries the endpoint probes.

None of this pipeline has been run as code. The provider figures come from calls made by hand,
outside the pipeline, with spans pasted into the request body.

---

## 1. Why this does not contradict PRD §5

PRD §5 refuses to compete on turning conversations into clean facts. Phase 6 keeps that refusal by
splitting the problem in two and putting a person between the halves.

Extraction is a judgment call and always will be. It runs **offline**, outside the server, and it
produces **proposals**. A proposal is a row in a queue with no reader other than the owner. The
write path stays what it is today: `memory_write` embeds and stores, with no model anywhere in it.

So lumberroom still buys extraction rather than building it. The extractor is whichever agent the owner
already runs. What lumberroom contributes is the part the market does badly: provenance, a speaker axis,
exclusion of its own output, and an approval gate. The store's contents stay facts the owner
approved, which is the property PRD §8 measures.

One consequence, stated plainly. This is the highest-risk operation anyone will ever run against
this store. A month of coding sessions holds `.env` dumps, tokens, keys and other people's names.
§9 is the section to read adversarially.

---

## 2. Ingestion proposes, it never writes

### 2.1 The rule

**Nothing in the ingestion pipeline calls the memory repository.** The pipeline's only write is to
`ingest_proposal`. A proposal becomes a memory in exactly one way: the owner approves it, and the
approval handler calls `services::write::run` with the proposal's content, namespace, tags and no
sensitivity override.

That single call is what keeps the guarantees. Going through `write::run` runs, in its order:

- the `credentials:*` plaintext refusal (a0)
- the classification table and the ceiling check (a, b)
- the grant check at the resolved level (c)
- the credential tripwire (d)
- exact-duplicate collapse, which records the restatement as a confirmation (e)
- the dedupe bands with the numeric, identifier and negation guards (f)
- supersession validation, when the proposal carries a target (h)

An approval path that inserted a row itself would be a second write path, and every one of those
seven checks would then exist in one place and be missing from the other. The proposal table stores
what to write. It never stores an embedding, and it never stores a memory id until `write::run`
returns one.

**One carve-out, and it inserts nothing.** The proposal handler calls `confirm` on an existing
memory in two places: when the emission check recognises content the store itself handed out
(§4.4), and when a re-proposal arrives against a fingerprint already at `state = 'written'` (§2.3).
`confirm` is the metadata touch the exact-duplicate path at (e) already performs, it bumps a count
and a timestamp, and it can neither create a row nor change one's content. Ingestion reaching the
memory table for anything else is the rule breaking.

### 2.2 No new MCP tools

The tool surface stays at five. Proposals are posted, listed, approved and rejected over CLI and
admin HTTP routes only, the same shape sealed put and get already use.

The reason is the queue itself. A model that can post a proposal can fill it, and a queue the
owner stops reading is an approval gate in name only. Keeping ingestion off MCP means the only
thing that can create a proposal is a process the owner started.

```
POST   /admin/ingest/runs                       open a run, get a run id
POST   /admin/ingest/scan                       tripwire on span text, rule names only (§9.1)
POST   /admin/ingest/emissions/check            {probes:[{content, observed_at?}]}, at most 200, answers {echoes:[bool]} (§4.4)
POST   /admin/ingest/proposals                  batch post, idempotent on fingerprint
GET    /admin/ingest/proposals?state=proposed
POST   /admin/ingest/proposals/{id}/approve     calls services::write::run
POST   /admin/ingest/proposals/{id}/reject
POST   /admin/ingest/proposals/{id}/unreject    returns a rejected row to proposed (§2.3)
GET    /admin/ingest/watermarks?skipped=true    what is stamped and why (§7.2)
POST   /admin/ingest/watermarks/unskip          clears one file's skip_reason (§7.2)
GET    /admin/ingest/runs/{id}                  the run report
```

Two of those routes exist because a layer with no consumer is a layer that never runs.
`/admin/ingest/scan` is the tripwire the CLI cannot call in-process, and `/admin/ingest/emissions/check`
is the same problem for the emission layer: `submit` is JavaScript and the emission table is
Postgres, so without a route the check in §4.4 has no way to happen. The authoritative check still
runs server side inside `POST /admin/ingest/proposals`, which is what stops a CLI from skipping it;
the read-only route exists so `--dry-run` and the report can say what will be confirmed before
anything is posted.

Every route requires `mayIngest` on the grant, default false, for the same reason `memory_forget`
requires `mayDelete`: the header that distinguishes a CLI from a model is one a model can set for
free.

### 2.3 The schema

Migration `009`, `migrations/20260819000009_ingestion.sql`.

```sql
CREATE TABLE ingest_proposal (
  id            uuid PRIMARY KEY,
  tenant_id     text NOT NULL,
  fingerprint   text NOT NULL,          -- sha256 of normalised content: the identity
  content       text NOT NULL,          -- what memory_write would receive
  namespace     text NOT NULL,
  tags          text[] NOT NULL DEFAULT '{}',
  supersedes    uuid REFERENCES memory(id),
  speaker       text NOT NULL,          -- the first source's speaker, frozen at insert (§5)
  quote         text,                   -- verbatim owner span, set only for owner_typed
  auto          boolean NOT NULL,       -- passed the substring check in §2.4, frozen at insert
  extractor     text NOT NULL,          -- 'agent:claude-code' | 'provider:openai/gpt-...'
  state         text NOT NULL,          -- proposed | rejected | written
  memory_id     uuid REFERENCES memory(id),
  last_error    text,                   -- the write refusal, rule name only, never the match
  last_error_at timestamptz,
  decided_at    timestamptz,
  created_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, fingerprint)
);

CREATE TABLE ingest_proposal_source (
  proposal_id  uuid NOT NULL REFERENCES ingest_proposal(id) ON DELETE CASCADE,
  source_key   text NOT NULL,           -- file_path '#' entry_uuid
  file_path    text NOT NULL,
  session_id   text,
  is_sidechain boolean NOT NULL,
  entry_uuid   text,
  speaker      text NOT NULL,
  observed_at  timestamptz,
  run_id       uuid NOT NULL,
  PRIMARY KEY (proposal_id, source_key)
);
```

The unique constraint on `fingerprint` is the whole idempotency story, and it is stolen from the
agentmemory pass §5B. Re-proposing the same content inserts a source row and touches nothing else.
That turns "the same preference appeared 808 times" into one proposal with 808 sources, which is an
exact answer to "have I already counted this" rather than a similarity guess.

**`speaker` and `auto` are frozen at first insert and never upgraded.** A fact first proposed as
`main_model` and later arriving from an `owner_typed` span with a valid quote gains a source row and
stays queued. Two reasons, and the first is enough. Re-evaluating `auto` on a later arrival means a
row the owner has been staring at in the queue can write itself while he is reading it, which is the
one behaviour an approval gate may not have. The second is that `write::run` is the same call either
way, so the owner loses a keystroke and nothing else. `ingest_proposal_source.speaker` carries the
per-source value, `lumberroom ingest show` prints the strongest speaker across sources with its quote, and
the owner approves from there. Nothing computes a strongest speaker onto the parent row.

A re-proposal landing on a fingerprint already at `state = 'written'` inserts its source row and
calls `confirm` on the memory id, which is what an exact restatement through `memory_write` already
does at check (e). It counts as `proposals_reinforced`.

**A rejection is permanent by default, and reversible by hand.** The row stays at
`state = 'rejected'` and its fingerprint blocks the same content from being proposed again. A queue
that re-asks a question the owner already answered is a queue the owner stops opening.

Permanent and irreversible are different claims, and the second one was wrong. Mistyping
`lumberroom ingest reject <id>` against the wrong uuid would otherwise block that content forever, with
hand-typing it into `memory_write` as the only recovery, and a queue read at speed is exactly where
that typo happens. So `reject` asks for confirmation, printing the first line of the content, unless
`--yes` is passed, and `lumberroom ingest unreject <id>` returns the row to `proposed` with its sources
intact. The state history stays on the row through `decided_at`, and an unrejected row is listed
with its earlier rejection so the owner sees what he undid.

Three more tables, each load-bearing, each specified in the section that needs it:
`recall_emission` (§4.4), `ingest_watermark` (§6.2) and `ingest_run` (§6.4).

### 2.4 Auto-approval is a rule, not a claim

Decision 3 of the brief: auto-approve a fact the owner stated in their own words, queue anything
the model inferred. A model asserting "the owner said this" is not evidence, so `lumberroom ingest submit`
checks it:

1. The proposal must carry `speaker = owner_typed` and a `source_key` resolving to a span the
   planner classified as owner-typed (§5).
2. `submit` re-reads that span from the frozen byte range and normalises both texts: lowercase,
   collapse whitespace, strip terminal punctuation.
3. The normalised proposal content must be a **substring** of the normalised span.

Pass, and `submit` approves the proposal itself, in the same call, through `write::run`. The row
goes straight to `state = 'written'` and the report lists it. Fail, and the proposal queues like any
other, with the failure recorded. The model gets to select and trim a sentence the owner typed. It
does not get to paraphrase one and call it a quote.

`--no-auto` holds everything for review, and it is the flag to use on the first run against an
unfamiliar corpus. The default writes, because the owner settled that: a fact he stated in his own
words does not need him to say so twice. Every check in §2.1 still runs on the way in, so an
auto-approved fact that trips the tripwire or collapses into an existing row behaves exactly as a
model's write would.

This is strict on purpose and it will reject useful paraphrases. Queuing those costs a keystroke.
The alternative costs the property that the store holds what the owner actually said.

**One stated limit: pasted text and typed text are the same bytes on disk.** Claude Code records a
paste as ordinary `user` message text, so an owner pasting a colleague's Slack message, a chunk of
someone else's README or a quoted spec makes those words `owner_typed`, and a fact drawn from them
passes the substring check and auto-approves. Nothing in the transcript distinguishes the two, so
this spec does not claim to. The guards that still apply are the tripwire on both sides, the
sensitive-path refusal and the six-month durability test in the extraction prompt. `--no-auto` is
the flag for a stretch of work heavy on pasted material, and `lumberroom ingest list --state written`
after a run is how the owner sees what auto-approval decided.

---

## 3. What the pipeline is

Three stages, and the split is copied from graphify: deterministic work in a CLI, judgment in
dispatched agents, deterministic work again on the way back.

| Stage | Kind | Where |
|---|---|---|
| `lumberroom ingest plan` | deterministic | `lumberroom`: the local filesystem and `/admin/ingest/scan` (§9.1), nothing else |
| extraction | judgment | Mode A, subagents inside the host agent (§8); Mode B, a provider the CLI calls (§10); Mode C, a batch job the CLI submits (§10.11, built 21 August 2026, never submitted) |
| `lumberroom ingest submit` | deterministic | `lumberroom` plus the admin routes |

**Mode A and Mode B are co-equal and the owner picks one per run.** Mode A uses the agent the
run was started from, which needs no key and hands the extractor project context it already has.
Mode B has the CLI speak HTTP to a model, which is what makes a 685-file backfill finish and what
makes a scheduled run possible at all. §10.1 states the one data-flow difference and the one
capability difference between them, and states each once.

**A third mode is specified and it is not in the first cut.** Mode C posts every chunk of a run as
one batch job and collects the results hours later, which buys half price on the providers that
publish a batch discount and sidesteps the concurrency limits Mode B has to respect. It writes the
same `out/chunk-NN.json` files, so `plan` and `submit` need nothing new for it. §10.11 specifies it
and §13 keeps it out of the first cut.

`plan` never calls a model. `submit` never calls a model. Extraction never touches the database,
in any mode.

Each stage writes its output to a working directory so a failed run resumes rather than restarts:

```
${XDG_STATE_HOME:-~/.local/state}/lumberroom/ingest/<run-id>/
  worklist.json        the run: scope, exclusions, chunk manifest
  spans/chunk-00.json  candidate spans, one file per chunk
  out/chunk-00.json    subagent output, one file per chunk
  state.json           per-chunk status, attempts and observed usage (§10.7)
  report.json          what submit posted, what it refused, what it held back
```

`spans/` and `out/` hold transcript text and are deleted on a clean exit or by the retention sweep
(§10.9). `worklist.json`, `state.json`, `report.json` and `run.log` hold counters and paths, and they
stay.

Mode 0700 on the directory and 0600 on every file, because `spans/` holds excerpts of a month of
work.

---

## 4. Exclusion by provenance, at parse time

This is the section that decides whether the feature is safe to run at all. lumberroom injects its digest
into every Claude Code session through the SessionStart hook, so **the store appears verbatim near
the top of every main-thread transcript.** Ingesting that closes the loop the agentmemory pass
documents: recall output captured, summarised, promoted to a fact, recalled again.

The corpus is already contaminated by a second system. The agentmemory pass measured 324 of 685
transcripts containing `mcp__agentmemory` calls and 66 carrying its injected-context marker.

Exclusion happens **at parse time, by entry provenance**. Similarity after the fact is the wrong
tool: it cannot tell a genuine restatement from an echo, and it fails the moment a model paraphrases.

**The extraction prompt is not a second line of defence, and two of three models proved it.**
Measured on 20 August 2026: five spans went to each of three GLM models under the §8.2 rules plus
one line telling the extractor not to extract a memory system's own digest. One of the five spans
was lumberroom's own digest, the text the SessionStart hook injects. `glm-4.7` and `glm-4.5-flash` pulled
lumberroom's own facts out of it and proposed them back as new memories. `glm-5.3` passed. §10.10 has the
table.

That is the 808-duplicate failure the agentmemory pass documents, reproduced in one controlled test
at a cost of five spans. Two consequences, and the design already assumed the first one. E1 through
E7 are the only defence, so a rule that leaks means the loop closes, whatever the prompt says. And
`glm-5.3` passing is luck rather than safety: the model that behaves today is one release from
behaving differently, and the owner switching models for cost or speed changes the answer with
nothing in the run report to show it. Never weaken an exclusion rule on the strength of a model
having declined the bait.

§12 step 1 feeds a digest span and asserts zero proposals. This measurement is what turns that step
from prudence into the test the feature is unsafe without.

The §8.2 prompt keeps its instructions against summaries and restatements, and adding a digest line
to it would cost nothing and buy nothing measurable. Write it if it helps a model on a good day. Do
not count it anywhere.

### 4.1 Claude Code

**E1. Every `type: "attachment"` entry, whole.** Measured here, the key sets differ by subtype:
`hook_success` carries `content`, `stdout`, `stderr`, `command`, `hookName`, `hookEvent`,
`toolUseID` and `exitCode`; `hook_additional_context` carries `content` with no `stdout`. Both
channels hold the injected text, so dropping the entry is the only version that has no seam. The
agentmemory pass counted 14 subtypes in a 30-file sample and found two more in a 40-file sample, so
the subtype set is open and an allowlist would leak the day Claude Code adds one.

Dropping all attachments loses `edited_text_file` snippets, which are the model's edits and not the
owner's words. That is an acceptable loss.

**E2. Every `tool_result` whose joined tool is a memory tool.** The result entry carries
`tool_use_id`, `content` and `is_error` and **not** the tool name, measured here. The name lives on
the `tool_use` block of the assistant entry, whose keys are `id`, `name`, `input`, `type` and
`caller`. The parser holds a map from `tool_use.id` to `tool_use.name` for the file and joins every
result to it.

Failing to build that join is the specific bug the agentmemory pass names as making
provenance-based exclusion impossible. A result with no matching `tool_use` in range is dropped and
counted, never kept with an unknown name.

Memory tools are matched by prefix: `mcp__lumberroom__*`, `mcp__agentmemory__*`, plus
`INGEST_MEMORY_TOOL_PREFIXES` for any other memory server. The `tool_use` blocks of those same tools
are dropped too: a `memory_write` argument is content already in the store, so it can only produce a
duplicate of a row that exists.

**E3. Text-level backstop.** Any span containing `<agentmemory-context>`, `<lumberroom-context>` or the
digest preamble the SessionStart hook emits is dropped and counted. E1 should have caught all of
these. A backstop that never fires is the evidence that E1 works, and its counter is reported.

`<lumberroom-context>` does not exist yet. The SessionStart preamble in `lumberroom` around line 614
emits four lines of prose and the digest with no wrapper, so today that token can never fire and a
zero counter would prove nothing about it. Wrapping the preamble and the digest in
`<lumberroom-context>...</lumberroom-context>` is work item 0 in §13, and it ships before the first run. Until
it does, and for every transcript already on disk, the preamble text is the handle that works, so
the backstop matches the first line of that preamble verbatim as well as the tag. Reading the E3
counter means reading its three sub-counters, one per token, since only the preamble one can fire
against the historical corpus.

**E4. Sidechain Task prompts stay, attributed.** The plain string on a `user` entry in an
`agent-*.jsonl` file is the Task prompt the parent model wrote, so §5 classifies it as
`main_model` and it is never auto-approvable.

### 4.2 Codex

Measured here against `~/.codex/sessions`, two session files, one of them 567 entries:

**E5. Every `response_item` whose `payload.role` is `developer`.** This is where injected context
lands on the Codex side. In a session recorded 4 July 2026, two `developer` entries carried the
`<permissions instructions>` preamble and an `<agentmemory-context>` block with the project's
pinned slots in it. The other memory system reaches Codex too, and it reaches it through this
entry shape.

**E6. `function_call_output` and `custom_tool_call_output` joined by `call_id`.** Codex records
`call_id` on both halves, and `function_call` additionally carries a `namespace` field holding
`mcp__<server>__`. That field is a cleaner exclusion handle than anything on the Claude Code side:
one string comparison identifies an MCP memory server with no join at all. The join is still built,
because `custom_tool_call` carries a bare `name`.

**E7. The `<environment_context>` block** arrives as `response_item` / `message` / `role: "user"`,
which is the reason §5 does not treat Codex `role: "user"` as the owner.

### 4.3 Everything else

Unknown entry types and unknown attachment subtypes are **counted per run and reported**, never
dropped silently. That counter is how a Claude Code release that adds a type gets noticed. The
agentmemory pass names silent fall-through as one of its ten things to avoid, and it is right.

### 4.4 The belt-and-braces layer: record what was handed out

Every exclusion above is a rule about the file. This one is a fact about the store: content the
store emitted cannot come back to it as a new fact.

**The session id is not the key, and the first version of this section had it wrong.** `Ctx`
carries a session id only when a client sends the `x-session-id` header (`src/http/mod.rs:211-218`,
injected at 174, read at `src/mcp/mod.rs:311`). Nothing sends it. Claude Code's MCP client attaches
no per-session header, `lumberroom bootstrap` sends none, and `client/lumberroom-bootstrap-hook.sh` discards
the stdin JSON that carries Claude Code's own session id. So every emission would have carried
`session_id = None` against a primary key that required it, and the layer would have fired never.
The two id spaces also differ: the server's correlation id and the `sessionId` field inside a
transcript are minted by different processes, so the join `submit` was going to make could not match
even with the header in place.

So the check is tenant-wide on content hash, inside a time window, and it never touches a session
id. The loop being guarded does not care which session the echo happened in.

```sql
CREATE TABLE recall_emission (
  tenant_id        text NOT NULL,
  content_sha256   text NOT NULL,        -- of the normalised content as emitted
  memory_id        uuid NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  tool             text NOT NULL,        -- context_bootstrap | memory_search
  session_id       text,                 -- diagnostics only, null until a client sends the header
  first_emitted_at timestamptz NOT NULL DEFAULT now(),
  last_emitted_at  timestamptz NOT NULL DEFAULT now(),
  emit_count       bigint NOT NULL DEFAULT 1,
  PRIMARY KEY (tenant_id, content_sha256, memory_id, tool)
);

CREATE INDEX recall_emission_lookup
  ON recall_emission (tenant_id, content_sha256, first_emitted_at);
```

`session_id` stays as a nullable diagnostic so the column is ready the day a client does send the
header. `submit` never joins on it and no query filters on it.

`context_bootstrap` and `memory_search` record the rows they returned. A repeat emission of the same
content bumps `last_emitted_at` and `emit_count` through `ON CONFLICT`, so a fact read a thousand
times is one row. The write is batched through the fire-and-forget path that already updates
`last_accessed_at` (`src/adapters/postgres/memory.rs:952`), so a read does not turn into a write
storm.

**One digest, or the hashes never meet.** `content_sha256` here and `fingerprint` on
`ingest_proposal` are the same function of the same input: lowercase, collapse whitespace, strip
terminal punctuation, then HMAC-SHA256 under a key derived from the KEK (plain SHA-256 on a
deployment with no KEK). Two implementations would give this layer a hash that cannot match
anything the extractor produces, which is the quiet way to build the same unreachable layer twice.
The function lives once, `crypto::Digester::digest`, and the proposal handler, the emission
writer and the check route all call it. The key is why a database dump cannot confirm a guessed
sentence against the emission table.

**The check.** For each candidate fact, `submit` sends the content itself (never a hash: the
server computes the digest so the client cannot probe with one it made offline) to
`POST /admin/ingest/emissions/check` as `{probes:[{content, observed_at?}]}`, at most 200 probes
per call, and the proposal handler runs the same query again on the way in so the CLI cannot skip
it. The route answers `{echoes:[bool]}`, one per probe in probe order, and nothing else. A hit is
a row for this tenant with a matching `content_sha256` whose `first_emitted_at` falls at or before
the source span's timestamp plus five minutes of clock slack, and within `INGEST_EMISSION_WINDOW`
(default 90 days) of it, and whose memory row the caller's read grant admits at its stored level.
The store handed the content out before the transcript recorded it, which is the direction that
makes it an echo rather than a coincidence.

A hit is a **confirmation**: the handler calls `confirm` on the emitted `memory_id`, posts no
proposal, and counts it in `ingest_run.confirmations`. The report names the memory id, so a spike in
confirmations reads as an exclusion rule having broken rather than as a quiet success.

This layer exists because E1 through E7 are rules about a file format that Anthropic and OpenAI
change without telling anyone. When one of them breaks, this one still holds. §12 step 8 proves it
by feeding `submit` exactly what a broken E1 would produce and asserting a confirmation comes back
instead of a proposal.

**It catches a verbatim echo and nothing softer.** An exact hash match means the extractor copied a
line the store handed out, which is what a broken E1 produces and what the digest loop looks like. A
model that paraphrases the digest before proposing it passes this check untouched. Similarity matching
would catch that case and would also collapse genuine restatements into echoes, which §4 opens by
refusing. So the coverage is exact-echo only, the dedupe bands at `write::run` check (f) are what
catch a near-duplicate on approval, and this layer is honest about being the belt rather than both
belts.

**What it does not cover, stated because the gap is large.** `recall_emission` is empty until the
batched writer ships, so it protects nothing in the historical backfill. Every transcript already on
disk, and there are 685 of them, has to rely on E1 through E7 alone. That is the argument for
running the first cut over one week of one project with the exclusions watched rather than over the
whole corpus, and it is the argument for work item 2 in §13 landing before the first run rather than
alongside it.

---

## 5. The speaker taxonomy

Auto-approval rests on one signal and that signal is small. The agentmemory pass measured `type:
"user"` at roughly 91% tool results in main-thread files and 100% in subagent files, and 82% of the
685 files are subagent transcripts carrying the parent's session id. So the owner's own words are
roughly 9% of `user` entries in 18% of the files, and isolating them takes real work.

Six values. Every span carries exactly one.

### `owner_typed`, the owner typed it

**Claude Code.** `type: "user"`, `isSidechain` false, `isMeta` absent or false, and
`message.content` is a plain string or a list whose items are all `text`. A single `tool_result`
item anywhere in the list disqualifies the entry. The file must not be named `agent-*.jsonl`.

Then four text filters, because Claude Code writes machine text into this slot too: drop a string
opening with `<command-name>`, `<local-command-stdout>`, `<command-message>` or `<system-reminder>`.
The agentmemory pass found zero `<system-reminder>` blocks inside `user` message text in a 40-file
sample, so that last filter should never fire; it is counted, not trusted.

**Codex.** `event_msg` with `payload.type == "user_message"`, and only that. Measured here:
one session held 3 `user_message` events against 4 `response_item` / `role: "user"` entries, one of
which was `<environment_context>`. Reading Codex `role: "user"` as the owner would be wrong on the
majority of entries in that file.

**Only this value is auto-approvable**, and only after the substring check in §2.4.

### `main_model`, the main-thread model said it

Claude Code: `type: "assistant"`, `isSidechain` false. Codex: `response_item` / `message` /
`role: "assistant"`. The Codex `event_msg` / `agent_message` entries render the same text a second
time and are dropped as duplicates, measured here at 30 against 30 in one file.

Also `main_model`: the Task prompt at the head of a sidechain file, per E4.

### `subagent`, a subagent said it

`isSidechain: true` on the entry, or a basename matching `agent-*.jsonl`. Both, because the two
disagree: the agentmemory pass sampled 40 files and found none mixing sidechain and main-thread
assistant entries, so the file-level signal is reliable, and the entry-level flag is what makes it
provable per span.

### `tool_returned`, a tool returned it

Claude Code: a `user` entry carrying `tool_result` items, joined to a tool name by `tool_use_id`.
Codex: `function_call_output` or `custom_tool_call_output`, joined by `call_id`. The joined name
rides on the span, so a later stage can say a fact came from a `Read` of a specific file.

### `hook_injected`, a hook injected it

Claude Code: `type: "attachment"` with `attachment.type` matching `hook_*`. Codex: `response_item`
/ `message` / `role: "developer"`. Excluded outright by §4, and it keeps a taxonomy value so the
run report can say how much was dropped for this reason.

### `system`, the harness produced it

Every remaining `attachment` subtype, `type: "system"`, `summary`, `mode`, `permission-mode`,
`bridge-session`, `ai-title`, `pr-link`, `last-prompt`, `queue-operation`, `file-history-*`,
`started`, `result`, and Codex `turn_context`, `session_meta`, `token_count`, `compacted`.
Excluded, counted.

### What reaches extraction

`owner_typed`, `main_model` and `subagent` spans, plus `tool_returned` spans from non-memory tools
when `--include-tool-output` is passed, which is off by default. Tool output is the bulk of the
corpus by bytes and it is where the credentials are.

---

## 6. Identity and incrementality

### 6.1 A session id does not identify a file

The agentmemory pass measured 562 of 685 files named `agent-<id>.jsonl`, all carrying the parent's
session id, and one session id spanning 9 files. Keying on session id alone means the last file
walked wins.

The identity tuple is **(file path, session id, isSidechain)**, and the file path is the primary
key. Everything else is an attribute of the file.

**The layout is nested, and it moved after the research was written.** Measured here on 20 August
2026 with `find ~/.claude/projects -name 'agent-*.jsonl'`: 576 subagent files, none of them beside
a main-thread transcript. 278 sit at `<project>/<session-uuid>/subagents/agent-*.jsonl` and 298 sit
two levels deeper at `<project>/<session-uuid>/subagents/workflows/wf_*/agent-*.jsonl`. The 71
main-thread transcripts sit at `<project>/<session-uuid>.jsonl`. The research describes a flat
directory, so the walk it implies would find the main threads and miss every subagent file.

Two consequences. The walk recurses to an unbounded depth rather than reading one directory, and it
refuses symlinks at every level rather than only at the top. And this is live evidence for the
thesis §4.4 rests on: the format shifted under the parser between the research and the spec, in a
month, with no announcement. Basename matching survived it. A path pattern would not have.

### 6.2 The watermark

```sql
CREATE TABLE ingest_watermark (
  tenant_id     text NOT NULL,
  file_path     text NOT NULL,
  session_id    text,
  is_sidechain  boolean NOT NULL DEFAULT false,
  byte_offset   bigint NOT NULL,        -- bytes consumed, always a line boundary
  prefix_sha256 text NOT NULL,          -- hash of bytes [0, byte_offset)
  entries_seen  bigint NOT NULL DEFAULT 0,
  skip_reason   text,                   -- set once, cleared only by hand (§7.2)
  skip_run_id   uuid,                   -- which run stamped it, so unskip has an audit trail
  fence_from    bigint,                 -- a fence this file resolved: dropped from here (§7.3)
  fence_until   bigint,                 -- to here, both written by the plan that closed it
  fence_run_id  uuid,                   -- whose ingest conversation it was
  last_run_id   uuid,
  updated_at    timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, file_path)
);
```

A live transcript grows all day. Reading it whole on every run reprocesses everything already seen,
which the agentmemory pass names as the failure lumberroom would hit hardest. `plan` seeks to
`byte_offset` and reads forward.

`prefix_sha256` catches the case the offset cannot: a file rewritten or truncated in place. `plan`
hashes the first `byte_offset` bytes and compares. Mismatch, and the file is re-read from zero and
the run reports it. Cheap because it is a sequential read of bytes already on disk, and worth it
because a silently shifted offset produces garbage spans forever.

**The advance is monotonic, and two runs cannot rewind each other.** The owner is told in §10.9 to
schedule a nightly Mode B run, and Mode A is interactive, so two runs overlapping is a normal
Tuesday rather than an edge case. Each froze its own `plan_ceiling`, and a plain assignment lets the
older run finish last and drag the watermark backwards, which reprocesses everything between the two
ceilings and re-proposes it.

Two mechanisms, and they solve different halves. The advance is
`byte_offset = GREATEST(byte_offset, :new)` in SQL, so an older run's write is a no-op rather than a
rewind. That is the correctness half and it holds across machines. On top of it, `plan` takes an
exclusive `flock` on `<state-dir>/lumberroom/ingest/.lock` for the length of the walk and `submit` takes
it again for the length of the advance, so two runs on one machine serialise instead of interleaving
their reads of the same growing file. A run that cannot take the lock within ten seconds exits 1
naming the run id that holds it.

`prefix_sha256` is written under the same `GREATEST` guard: it belongs to whichever offset won, so
the two never disagree.

### 6.3 Streaming, not readFile

The agentmemory pass measured 11 files above 10MB and a largest of 96.2MB. `readFile` on that one
means a 96MB string plus an array of every line. `plan` uses a `createReadStream` over the byte
range with a bounded line buffer, and a line longer than `INGEST_MAX_LINE_BYTES` (default 8MB) is
skipped and counted rather than buffered.

### 6.4 The run record, and truncation that reports itself

```sql
CREATE TABLE ingest_run (
  id                  uuid PRIMARY KEY,
  tenant_id           text NOT NULL,
  started_at          timestamptz NOT NULL DEFAULT now(),
  finished_at         timestamptz,
  scope               jsonb NOT NULL,   -- roots, project filter, date window
  extractor           text NOT NULL,
  files_seen          int NOT NULL DEFAULT 0,
  files_skipped       jsonb NOT NULL DEFAULT '{}',   -- reason -> count
  entries_seen        bigint NOT NULL DEFAULT 0,
  entries_excluded    jsonb NOT NULL DEFAULT '{}',   -- rule -> count
  unknown_types       jsonb NOT NULL DEFAULT '{}',
  spans_cut           int NOT NULL DEFAULT 0,
  chunks              int NOT NULL DEFAULT 0,
  chunks_missing      int NOT NULL DEFAULT 0,        -- no out/ file or unparseable (§8.3)
  chunks_failed       int NOT NULL DEFAULT 0,        -- extract gave up on them (§10.5)
  files_held_back     jsonb NOT NULL DEFAULT '[]',   -- {file, held_at, ceiling} per file (§8.3)
  fenced_entries      int NOT NULL DEFAULT 0,        -- dropped inside an ingest fence (§7.3)
  fences_unclosed     int NOT NULL DEFAULT 0,        -- closed without an end marker (§7.3)
  proposals_new       int NOT NULL DEFAULT 0,
  proposals_reinforced int NOT NULL DEFAULT 0,
  confirmations       int NOT NULL DEFAULT 0,
  traversal_capped    boolean NOT NULL DEFAULT false,
  artifact_sessions   jsonb NOT NULL DEFAULT '[]'    -- session ids this run itself created (§7.2)
);
```

`traversal_capped` is copied from the agentmemory pass §5C and it earns its column. Silent partial
coverage of a corpus reads exactly like complete coverage. When `INGEST_MAX_FILES` or
`INGEST_MAX_ENTRIES` fires, `plan` says so on stdout and `submit` stamps the run, so nobody reads
"41 proposals from one week" as "everything one week held".

`lumberroom ingest plan` prints the exclusion table on every run:

```
files      184 seen, 6 skipped (sensitive path 4, symlink 1, unparseable 1)
           1 ingest artifact (agent-a3f9.jsonl, run 4c1e), 0 held back
entries    91,204 seen, 87,331 excluded
           attachment 41,882 · tool_result 38,004 · memory_tool 1,317
           system 5,910 · sensitive 218 · ingest_fence 96
speakers   owner_typed 619 · main_model 2,701 · subagent 553
spans      1,204 cut into 34 chunks
fences     1 closed by end marker, 0 by run record, 0 by bound
unknown    entry types: 0   attachment subtypes: nested_memory 12
```

Nothing in that table is a total the reader has to compute. Every exclusion is counted by the rule
that made it, including the two this revision added: `ingest_fence` for entries dropped as part of an
earlier ingest conversation, and `held back` for files whose watermark refused to advance because a
chunk went missing. An exclusion with no counter is an exclusion nobody finds.

---

## 7. Ingesting the live session without eating the store

The owner wants the current session ingested, not only older ones. That session's transcript grows
while ingest runs, it records the ingest conversation, and when extraction runs as parallel
subagents each subagent writes its own `agent-*.jsonl` full of quoted memory content. A later run
would read all of it.

Three mechanisms, each covering a hole the others leave.

### 7.1 A frozen byte ceiling, taken at plan start

`plan` stats every candidate file once, at the top of the run, and records `plan_ceiling` per file.
It never reads past that offset, whatever the file has grown to by the time it gets there.

Covers: appends made **during** the run, including the ingest conversation itself and everything
the subagents write into the parent transcript.

Does not cover: anything the run appends that a **later** run will read.

### 7.2 A run marker the parser checks, on subagent files only

`plan` mints a `run_id`. The skill puts the literal token `lumberroom-ingest-run:<run_id>` in every
subagent prompt (§8.2). Claude Code records the Task prompt verbatim as the first `user` entry of
the subagent's transcript, so the token lands on disk inside the artifact it needs to mark.

**Three conditions, all required, before a file is stamped `skip_reason = 'ingest_artifact'`:**

1. The basename matches `agent-*.jsonl`.
2. The token appears in the **first `user` entry** of the parsed file, not anywhere in the bytes.
3. The token carries a full uuid that matches an `ingest_run.id` for this tenant.

The first version required only that the token appear in the raw first 64KB of any candidate file,
and that version blacklists the owner's own main-thread session forever. The Agent tool records a
dispatch as a `tool_use` in the **parent** transcript with the whole subagent prompt verbatim in
`input.prompt`, so the token lands in the main thread as well as in the artifact. A session where the
owner types `/lumberroom-ingest` early, which §13 describes as the shape of the first run, puts that
dispatch inside the first 64KB of the parent file, and the next run stamps the parent and skips it
forever. That is exactly what §7.4 says must not happen. Honest caveat, measured here: 0 of the 71
main-thread files on this machine has a dispatch inside its first 64KB, because those dispatches came
deep in mature sessions. The mechanism permitted it, which is enough.

Condition 3 exists because this spec and the skill both contain the literal string
`lumberroom-ingest-run:` as placeholder text. A session in which the owner reads or edits either file
carries the token, so a prefix match with no run id would blacklist the session where this feature
gets built.

Condition 2 also fixes an ordering bug. A raw byte prescan runs before parsing, so it preempts the
fence in §7.3: the file gets stamped and skipped whole instead of having its ingest conversation
excised and the rest kept. The check now happens inside the parse of the first entry, which is where
it can tell a subagent's own prompt from a parent's record of dispatching it.

**The stamp is reversible and it is reported.** `plan` prints the file and the run id on the line it
stamps, `files_skipped` counts it, and `lumberroom ingest unskip <path>` clears `skip_reason` so the file
re-enters the corpus from its current `byte_offset`. `skip_run_id` records which run stamped it. A
silent irreversible blacklist over a heuristic is not a mechanism the owner can audit.

Main-thread files rely on §7.3 and are never stamped by this rule.

The agentmemory pass names a marker that nothing reads as one of its ten things to avoid:
`context.ts:204` writes `<agentmemory-context>` and `grep` finds no consumer. The marker here is
defined by its consumer, and §12 steps 5 and 9 assert the consumer fires and does not overfire.

`submit` also writes the ingesting session id and every subagent session id it saw into
`ingest_run.artifact_sessions`. Nothing skips a file on the strength of that list: the marker is the
mechanism, and the list is how a marker miss gets found. When a later run proposes a fact whose
source session id sits in `artifact_sessions`, the run report says so by name and the parser has a
bug worth fixing that afternoon.

Covers: subagent transcripts, forever, across runs.

### 7.3 A bounded fence around the ingest conversation

**The invariant: a fence always closes.** The most an ingest run can cost the owner is one bounded
region of one file, every entry inside it lands in the `ingest_fence` counter, and a fence closed by
anything other than its own end marker is reported as `fences_unclosed` with the file and the byte
range named. No mechanism here can drop an unbounded tail of a session.

That invariant is the fix. The first version said the parser "drops everything after an unmatched
begin", which loses every entry after an interrupted run, in that session, forever, with no counter
anywhere. A skill instruction is not a mechanism: the owner pressing Escape during extraction, a
`submit` that errors, and a model that skips the echo all happen, and the cost of any of them was a
week of real work excluded with no line in any report.

Three ways a fence closes, in the order the parser tries them.

**The end marker.** `plan` prints `lumberroom-ingest-begin:<run_id>` as its first line of output and
`submit` prints `lumberroom-ingest-end:<run_id>` as its last, and the skill echoes both again as plain text
at §11 steps 2 and 7. All four land in the main-thread transcript. A begin with a matching end is a
closed fence, every entry between them is dropped and counted, and duplicate markers for one run id
collapse into one fence. This is the normal case.

**The run record.** The marker carries a run id, so the run record is the close and no file has to be
identified. On meeting an unmatched begin, `plan` fetches `GET /admin/ingest/runs/{run_id}`. A set
`finished_at` means that run reached the end of `submit`, and it becomes the grace anchor: the fence
closes at the first entry timestamped more than `INGEST_FENCE_GRACE` after it.

The first draft of this had `submit` write the fence onto "the ingesting file's watermark row", which
does not work and fails the same way the session-id join in §4.4 failed. `submit` runs inside a Bash
tool call. It holds no transcript path and no session id, so it cannot name the file whose
conversation it is part of, and a write keyed on a file it cannot identify is a write that never
happens. Keying on the run id, which the marker itself carries, needs no discovery at all.

**The bound.** With a begin, no end and no run record to read, either because the run never reached
`submit` or because the server is unreachable, the fence closes at the first entry timestamped more
than `INGEST_FENCE_GRACE` (default 60 minutes) after the **begin marker's own timestamp**, or at
`fence_from + INGEST_FENCE_MAX_BYTES` (default 4MB), whichever comes first. Both bounds swallow a real
ingest run and both leave a week of later work intact.

Whichever of the three closes it, the `plan` that resolved a fence writes `fence_from`, `fence_until`
and `fence_run_id` onto that file's watermark, and this `plan` does know the file. A resolved fence is
never re-derived, so a re-read of those bytes after a `prefix_sha256` mismatch skips them without
consulting anything. The parser counts the entries it dropped, marks a bound-closed fence
`fences_unclosed`, and names the file and the byte range in the report. A fence closed by the run
record counts there too: the end marker not landing is worth seeing whether or not the close was
clean.

The fence is a byte range and the watermark is a position, and they do not constrain each other. A
file held back below its `plan_ceiling` by §8.3 step 7 keeps its held-back bytes for the next run and
still drops the fenced ones above them, which is what both rules want.

**The marker scan reads every entry's text, before any exclusion runs.** The case that forces this: an
owner runs Mode B by typing `lumberroom ingest run` into a Bash tool call inside a Claude Code session, so
the skill never runs and none of its echoes exist. The model's narration of that run, quoting the
facts the report listed, lands as `main_model` text and reaches extraction next run. The CLI printing
the markers on its own stdout covers it, and that output lands inside a `tool_result`. A scan that ran
after E1 and E2, or that read only `owner_typed` text, would never see them, and the fix would do
nothing. So the fence scan is the first pass over an entry, before provenance exclusion and before
speaker classification, and it reads the text of every entry whatever its type. The fence outranks
every other rule: an entry inside a fence is dropped as `ingest_fence` and is never counted twice.

**The self-fence case, written down so nobody re-derives it.** `plan` mints the run id, so nothing
can name it earlier than `plan` itself. `plan` prints the begin marker as the first line of its own
stdout, which lands in the `tool_result` for that invocation and is why the scan has to read
tool-result text. That marker sits **above** the `plan_ceiling` this run freezes a moment later, since
the ceiling is taken per candidate file at the top of the walk and the transcript entry carrying the
marker is written after it. So this run never sees its own begin, and the fence costs it nothing.

The next run is the one that matters. It reads from the watermark, meets the begin marker, and closes
the fence by one of the three routes above, which puts the exclusion table, the dispatch records, the
`submit` report and the owner's reading of it all inside the fence. What stays outside is the
narration before `plan` was invoked, per §11.

Covers: the ingest conversation inside the surviving main thread, which §7.1 misses on the next run.

### 7.4 Why the ingesting session is not skipped wholesale

Recording the ingesting session id and skipping that file forever is the obvious move and it is
wrong. The owner's real work in that session happened before ingest started, and it is exactly the
material this feature exists to capture. §7.1 and §7.3 remove the ingest span and keep the rest,
and §7.2 is written so that it cannot stamp that file by accident.

### 7.5 The termination argument

An ingest run leaves three kinds of new bytes on disk: the ingest conversation in the parent
transcript, the subagent transcripts, and the working directory. §7.3 fences the first within a
bound it always reaches, §7.2 stamps the second on three conditions and records what it stamped, and
the working directory sits outside every scanned root and is deleted by `lumberroom ingest clean`. No run
produces an artifact a later run treats as a candidate, so the sequence terminates.

The claim is narrower than it was and it is the one that holds. A fence closing by its
bound can leave the tail of an ingest conversation inside the corpus, which reaches extraction as
`main_model` narration. That produces queue noise the owner rejects, never an auto-approval, since
`auto` needs an `owner_typed` span and the substring check. Losing a week of the owner's work to an
unbounded fence was the worse trade, and this is the direction the error now runs in.

The test that settles it is in §12: run `plan` and `submit` twice back to back and assert the second
run posts zero new proposals.

---

## 8. Mode A, the agent as extractor

Mode A runs `lumberroom ingest` from inside the agent the owner is already working in: Claude Code today,
Codex or Hermes once their transcript parsers land. That agent is the extractor. No key is needed,
no span leaves the owner's infrastructure, and the extractor arrives holding the project context. The
run is interactive by nature, since a person has to be in the session for the agent to dispatch
anything.

"Leaves the owner's infrastructure" is the accurate claim, and "leaves the machine" overstated it. `plan` posts every span's plaintext to `POST /admin/ingest/scan` for the
tripwire (§9.1), and the lumberroom server is frequently not the machine the transcripts sit on: the config
sample in §10.4 shows `https://lumberroom.example.com`. One tripwire implementation is still the right
call, since a JavaScript port of the rules would drift from the Rust one within a phase. The traffic
is span text to the owner's own server over TLS, and §10.8 names it in the consent block so nobody
learns about it from a packet capture.

The shape is graphify's, and graphify is the working local precedent: a CLI does detection and
deterministic extraction, then the skill dispatches parallel subagents through the Agent tool, one
per chunk, all in a single message, each writing JSON that a later step merges.

### 8.1 `lumberroom ingest plan`, deterministic

`plan` and `submit` belong to both modes. They are written out here because Mode A is the flow they
were designed against, and Mode B substitutes one step in the middle.

```
lumberroom ingest plan [--source claude|codex|all] [--project <path|slug>] [--since 7d]
                 [--max-files N] [--include-tool-output] [--json]
```

0. Take the run lock (§6.2), echo `lumberroom-ingest-begin:<run_id>` on stdout, and sweep run directories
   older than `INGEST_RUN_RETENTION_DAYS` (default 7).
1. Walk the roots, recursing to any depth. `~/.claude/projects` for Claude Code, `~/.codex/sessions`
   for Codex. Main-thread transcripts sit at `<project>/<session-uuid>.jsonl` and subagent files sit
   under `subagents/` and `subagents/workflows/wf_*/`, per §6.1. Refuse symlinks at every level.
   Apply `--project` by matching the `cwd` field on the first entry carrying one, and `--since` by
   file mtime plus the first timestamp inside. Skip files carrying a `skip_reason`.
2. For each file, load or create the watermark, verify `prefix_sha256`, take `plan_ceiling`.
3. Stream entries between `byte_offset` and `plan_ceiling`. Build the `tool_use_id` to name map as
   you go, since a result always follows its use in the same file.
4. Scan every entry's text for the fence markers first, whatever its type, and resolve open fences
   per §7.3. Then apply every exclusion in §4 and §9.2. Count each one by rule.
5. Classify each surviving entry into one speaker value (§5).
6. Cut candidate spans. A span is a contiguous run of entries sharing a speaker, capped at
   `INGEST_SPAN_CHARS` (default 6000), carrying `{file_path, session_id, is_sidechain, entry_uuids,
   byte_range, speaker, tool_name?, timestamp, cwd, text}`.
7. Group spans into chunks. One chunk holds up to 40 spans or 24,000 characters, whichever comes
   first, and spans from one session stay together so a chunk reads as a conversation.
8. Write `worklist.json` and `spans/chunk-NN.json`. Print the exclusion table.
9. Advance nothing. The watermark moves in `submit`, not here, so a run that dies between the two
   re-plans the same bytes instead of losing them.

### 8.2 The skill dispatches

The skill reads `worklist.json`, prints the estimate, and calls the Agent tool **once per chunk, all
in the same message.** Graphify is emphatic on both halves: reading the files inline instead is 5 to
10 times slower, and dispatching in separate messages runs them in sequence and buys nothing.
`subagent_type` is `general-purpose`, because the read-only type cannot write its chunk file and
drops its results silently.

Each subagent receives this prompt with `CHUNK_PATH`, `OUT_PATH`, `CHUNK_NUM`, `TOTAL_CHUNKS` and
`RUN_ID` substituted:

```
lumberroom-ingest-run:RUN_ID

You are extracting durable facts from chunk CHUNK_NUM of TOTAL_CHUNKS of one person's agent
transcripts. Read CHUNK_PATH. Write your result to OUT_PATH. Touch no other file.

Do not call any memory tool. Do not call any tool whose name starts with mcp__lumberroom__ or
mcp__agentmemory__. Do not write to any store. Your only output is the JSON file.

CHUNK_PATH holds a JSON array of spans. Each span has: id, speaker, text, session_id,
timestamp, cwd, and for tool spans, tool_name. The speaker values mean:
  owner_typed  the person typed this
  main_model   the assistant said this
  subagent     a subagent said this
  tool_returned  a tool produced this

A durable fact is one that will still be true and still be worth knowing in six months. It is
about the person, their machines, their projects, their preferences or their decisions.

Extract:
  - a stated preference: how they want work done, which tool they use, what they refuse
  - a fact about their setup: a machine, an OS, a port, a path, a service, a model route
  - a decision with its reason: what was chosen and what it lost to
  - a correction: something they said was wrong, and what replaced it

Do not extract:
  - anything true only inside one session: a file being edited, a test currently failing
  - a summary of what happened, or a narration of the conversation
  - a fact about a codebase that the codebase already states
  - a restatement of something another span in this chunk already says
  - anything containing a password, API key, token, private key or connection string with
    credentials in it, whatever else the span says

If the chunk holds no durable fact, write exactly this to OUT_PATH and stop:

  {"facts": [], "refusal": "<no-facts/>"}

That is a correct and expected answer. Most chunks are ordinary work with nothing durable in
them. Returning nothing costs nothing. Inventing a fact to look productive costs the person
their store.

Otherwise write:

  {"facts": [
    {
      "content": "one sentence, standalone, no pronouns referring outside itself",
      "namespace": "user:me" | "project:<slug>" | "global",
      "tags": ["short", "lowercase"],
      "source_span_id": "the id of the span this came from",
      "speaker": "the speaker of that span, copied",
      "quote": "the exact substring of that span, only when speaker is owner_typed",
      "confidence": "stated" | "inferred"
    }
  ]}

Rules that decide whether a fact is usable:
  - "stated" means the person said it in their own words in an owner_typed span, and quote is
    a verbatim substring of that span. Anything else is "inferred". A wrong quote is worse
    than no quote: it will be checked against the transcript and the fact discarded.
  - content is what a person would want read back to them, not a report. Write "the Postgres
    port on the dev box is 5433", not "the user discussed Postgres configuration".
  - one fact per entry. Two facts joined by "and" are two entries.
  - Prose rules, and they are enforced: no em dashes anywhere. Active voice. No adverb where a
    plain verb works. No "Note that". No "not X, it's Y" contrasts, state Y. Never mention an
    AI, an assistant or a model as the author of anything.

Write the file even if facts is empty. A missing file reads as a crashed agent.
```

The skill collects, warns on each missing or unparseable chunk file, and aborts if more than half
are missing. A `<no-facts/>` refusal is a success and is counted as one.

### 8.3 `lumberroom ingest submit`, deterministic

```
lumberroom ingest submit --run <id> [--dry-run]
```

1. Merge `out/chunk-*.json`. Reject a fact whose `source_span_id` is not in the worklist. Record
   which chunks are missing or unparseable, and read `state.json` for the ones `extract` marked
   failed.
2. Normalise and fingerprint each fact.
3. Run the credential tripwire on every fact before it goes anywhere (§9.1).
4. Check the substring claim (§2.4) and set `auto`.
5. Check every fact's normalised hash against `recall_emission` for the tenant, through
   `POST /admin/ingest/emissions/check` (§4.4). A hit becomes a confirmation and posts no proposal.
   The proposal handler runs the same check again on the way in.
6. Post the batch. The server deduplicates on fingerprint, inserts source rows, and runs the
   emission check and the tripwire itself.
7. Advance the watermarks, per the hold-back rule below. Stamp `skip_reason` on the subagent files
   this run created, and close the run record by setting `finished_at`, which is what lets a later
   `plan` bound this run's fence (§7.3).
8. Auto-approve every proposal whose `auto` is true, unless `--no-auto` was passed, and record the
   memory id each one returned.
9. Print the report: written, queued, reinforced, confirmed, refused, files held back, and why each
   refusal happened. Print `lumberroom-ingest-end:<run_id>` last, and release the run lock.

**The hold-back rule, which is step 7 and is the one place this pipeline can lose data.** A file's
watermark advances to the **first byte of the earliest span that landed in a missing or failed
chunk**, and to `plan_ceiling` only when every one of its spans came back extracted.

The first version held back only a file whose spans *all* landed in a missing chunk, and that rule
loses transcript bytes permanently on an ordinary interrupted run. Chunks cap at 40 spans or 24,000
characters (§8.1 step 7), so any substantial file spreads its spans across many chunks, and the
96.2MB one spreads across hundreds. Kill `extract` at chunk 400 as §10.7 narrates, then run `submit`:
a file with spans in chunks 398 through 405 had some of them extracted, so the old rule advanced it
to `plan_ceiling`, and the bytes behind chunks 401 to 405 were never extracted and never planned
again. Nothing forces `--retry-failed` first, so that was the default path after a kill. §10.7 calls
advancing past unextracted bytes the one failure in this pipeline with no recovery, and the old rule
permitted it.

Three clauses make the rule complete:

- **A file with no surviving spans still advances to `plan_ceiling`.** Those bytes were read,
  classified and excluded, so there is nothing left to extract. Most of the corpus is this case, and
  a rule that held it back would stall the watermark on 90% of files forever.
- **A held-back file is named.** `files_held_back` records `{file, held_at, ceiling}` per file, the
  report prints the count and the byte gap, and `plan` prints it again next run. The owner learns
  what is pending from the report rather than from a proposal that never arrives.
- **The advance stays monotonic** (§6.2). A hold-back is a smaller advance, never a rewind, so an
  overlapping run that already extracted those bytes keeps its progress.

`extract --retry-failed` then `submit` again closes the gap with no re-planning, since the spans are
still in `spans/` and the watermark never passed them.

Then the owner reads the queue:

```
lumberroom ingest list [--state proposed|written|rejected] [--limit 50]
lumberroom ingest show <id>                        # the fact, its sources, and the spans they came from
lumberroom ingest approve <id>...                  # one or many ids in one call
lumberroom ingest approve --run <id> [--auto] [--yes]
lumberroom ingest reject <id> [--reason "..."] [--yes]
lumberroom ingest unreject <id>                    # returns a rejected row to proposed (§2.3)
```

`approve` takes several ids in one call and takes a `--run` filter, because a backfill queue runs to
hundreds of rows and a queue that costs one command per row is a queue the owner abandons at row
thirty. The filtered form prints every row it will approve, counts them, and asks once unless
`--yes` is passed. `--auto` narrows it to the rows the server marked `auto` itself; there is no
`--speaker` filter, because the speaker column is the poster's claim and a bulk approval on a claim
is a bulk approval on nothing.

Either form calls `write::run` once per proposal and records the returned memory id, including the
case where `write::run` collapses it into an existing row as a duplicate. That outcome is a success
and the report says `deduplicated`. One refusal does not stop a batch.

When `write::run` refuses, on the tripwire, on the ceiling, on a missing KEK or on a superseded
target, the proposal stays at `state = 'proposed'` and the refusal is stored on the row as
`last_error`, rule name and all, with no echo of the matched text. It shows in `lumberroom ingest list`
with the error attached, so a refusal is a thing the owner reads rather than a row that silently
stops moving. `ingest_proposal` carries `last_error text` and `last_error_at timestamptz` for it.

---

## 9. Safety

### 9.1 The tripwire runs before a proposal exists

`services::write::run` scans content with `domain::tripwire` at (d), and that scan happens on
approval, which is far too late to be the only one. A proposal is a row holding text, and text
holding an API key is a leak whether or not anyone approves it.

So the tripwire runs twice:

- **On the span, at plan time.** A span that trips is dropped and never reaches a model, local or
  remote. This is the one that matters for §9.4.
- **On the fact, at submit time.** Server side, in the proposal handler, so the CLI cannot skip it.
  A tripped fact is refused with the rule name and no echo of the matched text, exactly as
  `Finding::refusal` already does.

The tripwire lives in `src/domain/tripwire.rs` and the CLI cannot call Rust. Rather than porting the
rules to JavaScript and letting the two drift, `plan` posts each span's text to a new
`POST /admin/ingest/scan` route that returns rule names only. That costs a round trip per chunk and
keeps one implementation of the rules.

Two consequences worth stating rather than discovering. Span plaintext travels to the lumberroom server,
which is the owner's own infrastructure and is often not the machine holding the transcripts, so §8
claims no more than that (and §10.8 names it in the consent block). And `plan` cannot run against an
unreachable server: with no scan there is no tripwire, and a `plan` that shrugged and continued would
put credentials in `spans/` and then in a model's context. `plan` exits 1 when the scan route fails,
naming the route.

### 9.2 Sensitive paths are refused at parse time

A file read whose path looks like a secret store is dropped before its content is classified,
whatever the tripwire would have said about the bytes.

Refuse a path when any segment matches, case-insensitively:

- a filename of `.env` or beginning `.env.`
- a segment of `.ssh`, `.gnupg`, `.aws`, `.kube`, `.docker` or `secrets`
- a filename of `id_rsa`, `id_ed25519`, `id_ecdsa`, `id_dsa`, or any of those with `.pub` removed
- a filename ending `.pem`, `.p12`, `.pfx`, `.keystore` or `.jks`
- a filename containing `credential`, `_token`, `-token`, `.token`, `_secret` or `.netrc`

Written fresh rather than copied. The agentmemory pass reproduces a working pattern set and its
licensing section is clear that the idea is free and the expression is not, so these are lumberroom's
words for lumberroom's corpus.

**The test is the valuable half.** `tests/ingest_sensitive.rs` pins the false positives, because a
filter that refuses ordinary work is a filter the owner switches off:

| Path | Verdict |
|---|---|
| `~/work/api/.env.production` | refused |
| `~/.ssh/config` | refused |
| `~/work/auth/jsonwebtoken-demo/src/index.ts` | allowed |
| `~/work/lib/secrethandshake/README.md` | allowed |
| `~/work/nlp/tokeniser/tests/test_token.py` | allowed |
| `~/.claude/projects/-Users-x-work/abc.jsonl` | allowed |
| `~/work/lumberroom/docs/environment.md` | allowed |

The check runs on the `file_path` argument of `Read`, `Edit`, `Write`, `Glob` and `NotebookEdit`,
and on any absolute path found in a `Bash` command string. The Bash case is best effort and the spec
says so: `cat $SECRETS_DIR/prod` defeats it, and the tripwire on the output is the layer that
catches the payload.

Symlinks are refused during the walk and counted.

### 9.3 What sensitivity an ingested proposal gets

**It gets whatever the classification table gives its namespace.** `approve` passes `sensitivity:
None` to `write::run`, so `resolve_for_write` applies the namespace default and nothing else.

The argument for defaulting to `private` instead is real. A transcript span is incidental in a way a
sentence the owner deliberately typed into `memory_write` is not, and raising the floor is the
conservative move.

Three things beat it.

**It fails closed on a default install.** `KEK_PROVIDER=none` is shipped, and `write::run` refuses a
private write when it cannot encrypt rather than storing plaintext. Defaulting proposals to private
means every approval on a default install returns `Unavailable`. A safety default that makes the
feature not work gets removed within a week, and it gets removed by the person it was protecting.

**It is a second classification path.** The classification table is the single place that decides
what a namespace holds. A rule that says "except when it came from a transcript" is the same mistake
as a second write path: two answers to one question, and they drift.

**The content is a fact, not a span.** What reaches `memory` is one distilled sentence the owner
read and approved. The span stays in the working directory and is deleted. A fact about the dev
box's Postgres port is not more sensitive because a transcript is where it was noticed.

What ingestion adds instead of a blanket bump: the tripwire twice, the sensitive-path refusal, the
approval gate, and no lexical index on `ingest_proposal`. Operators who want the floor raised get
`INGEST_MIN_SENSITIVITY`, unset by default, and setting it to `private` makes a run **refuse** when
the KEK is missing rather than quietly downgrading. Classification only ever goes up, and that rule
does not bend here.

### 9.4 The working directory is the exposure

`spans/` holds excerpts of a month of work in plaintext on disk. Mode 0600, under
`$XDG_STATE_HOME`, never inside a repo, and `lumberroom ingest clean --run <id>` deletes it. `submit`
prints the path and the reminder. This is an accepted cost and it is written down rather than
implied.

Manual deletion is enough for a run the owner watched and useless for a scheduled one, so two
automatic sweeps back it up: `lumberroom ingest run` deletes `spans/` and `out/` on exit 0, and `plan`
expires run directories older than `INGEST_RUN_RETENTION_DAYS` at the top of every run (§10.9).
Without those a nightly cron line leaves a new directory of transcript excerpts on disk every night
for as long as it is scheduled.

---

## 10. Mode B, the direct provider path, and Mode C behind it

### 10.1 What separates Mode A from Mode B, said once

Mode B has the CLI call a model itself over HTTP. Two differences with Mode A, and both are facts
rather than advice.

**Data flow.** A hosted provider sees the spans the run sends it. A base URL pointing at Ollama, LM
Studio or vLLM on the same machine sees them and no span reaches a third party. Privacy follows the
endpoint, and the endpoint is independent of the mode: a Mode B run against loopback keeps the corpus
local exactly as Mode A does. Both modes still post span text to the lumberroom server for the tripwire
scan, which is the owner's own infrastructure and is named in §8 and §9.1. The owner picks the
endpoint per run.

**Who has to be present.** Mode A is the only mode that needs somebody watching. An interactive
agent cannot be scheduled, so every cron job, every script and every nightly pass on a server is
Mode B by necessity, and so is a backfill across 685 files that no one wants to sit through.
§10.9 treats the unattended run as a normal use rather than an exception, and Mode C in §10.11 is
unattended by construction, since its results arrive hours after the command returns.

Neither mode is a fallback for the other and the CLI states no preference. `lumberroom ingest extract` is
Mode B's command and requires `--provider` with no default. Mode A has no `extract` command at all:
the skill dispatches subagents and they write the same `out/chunk-NN.json` files, so `plan` and
`submit` cannot tell which mode filled them apart from the `extractor` string each records.

### 10.2 Position: speak REST with fetch, add no dependency

`lumberroom` stays dependency-free. The rule earns its place: this CLI is a client the server
cannot accidentally accommodate, and it caught both Phase 1 protocol bugs. A provider SDK per vendor
would end it for a feature that needs one HTTP POST.

`fetch` is a node built-in. Speaking the REST APIs directly adds zero packages, and the shape of the
work is small: every provider here is HTTP and JSON, one OpenAI-compatible code path covers all of
them but Anthropic, and Anthropic needs a second small path for its message shape and its `x-api-key`
header.
The cost is owning the request bodies when a provider changes them, which for chat completions is a
stable surface, and the failure mode is a clear HTTP status rather than a silent wrong answer. An
SDK would also drag in its own retry and its own timeout, and this pipeline needs both to obey the
run's checkpoint rather than a library's idea of one.

### 10.3 Two request paths

**OpenAI-compatible.** `POST {base_url}/chat/completions`, `Authorization: Bearer <key>` when a key
exists, body `{model, messages:[{role:"system"},{role:"user"}], temperature:0,
response_format:{type:"json_object"}}`. Covers OpenAI (`https://api.openai.com/v1`), OpenRouter
(`https://openrouter.ai/api/v1`) and any custom base URL, which is where Ollama, LM Studio and vLLM
land. The token is optional on the custom path, since a local server usually wants none, and the
`Authorization` header is omitted rather than sent empty when no token is configured. OpenRouter
takes two extra headers, `HTTP-Referer` and `X-Title`, and lumberroom sends `https://github.com/lumberroom/lumberroom`
and `lumberroom` so a run is identifiable in that dashboard.

**`temperature: 0` holds, and JSON mode carries a hard requirement on GLM.** Measured on 20 August
2026 against z.ai, whose own documentation contradicts itself on the temperature question:
`temperature: 0` returns 200 and behaves. `response_format: {"type":"json_object"}` sent with the
default thinking mode **hangs**, with no error and no response, and the same request with
`thinking: {"type":"disabled"}` returns in 2.1 seconds. So a request to a GLM model carries
`thinking: {"type":"disabled"}` beside `response_format`, and it is mandatory rather than an
optimisation. §10.10 gives the timeouts, the token counts and the rest of that run.

That field goes only to providers that accept it. OpenAI answers an unknown top-level argument with
a 400, so the per-provider table in the CLI carries the extra body fields each one takes, next to
its base URL, and the shared code path merges them into the body it builds.

**Anthropic.** `POST {base_url}/v1/messages`, headers `x-api-key` and
`anthropic-version: 2023-06-01`, body `{model, max_tokens, system, messages}`, and the response text
sits inside `content` rather than at `choices[0].message.content`.

Take **the first block in `content` whose `type` is `text`**, never `content[0]`. `content` is an
array of blocks and a thinking block leads it whenever the model has extended thinking on, so
`content[0].text` reads `undefined` and the chunk fails to parse against a response that was
perfectly good. Concatenate nothing: a second text block would be a second answer, and the parser
reports that as a failed chunk rather than guessing which half to keep.

One shared prompt, the §8.2 text with the file-writing instructions replaced by "return the JSON
object and nothing else". One shared parser, tolerant of a fenced code block around the JSON, and
tolerant of the `<no-facts/>` refusal arriving as bare text rather than inside the object. The fence
tolerance is a requirement rather than caution: on 20 August 2026 `glm-4.7` wrapped its object in a
json code fence in a call whose prompt told it to return an object and nothing else. A chunk
whose response parses to neither is a failed chunk, named in the report with the first 200
characters of what came back.

### 10.4 Where the keys live

`docs/ingestion-providers.md` is the current record for this section and for 10.3. It carries the
per-model measurements taken after this spec was written, including which models honour
`response_format` and what reasoning costs when it is left on.


**Not in `.env`.** That file is the server's, it is read by Docker Compose and by several shell
scripts, and `AUTH_TOKENS` already proved how easily its contents end up somewhere unintended.

Provider keys live in `~/.config/lumberroom/config.json`, the file the CLI already owns, under
`ingest.providers.<name>`:

```json
{
  "url": "https://lumberroom.example.com",
  "token": "...",
  "ingest": {
    "providers": {
      "openai":     { "key": "sk-..." },
      "anthropic":  { "key": "sk-ant-..." },
      "openrouter": { "key": "sk-or-..." },
      "zai":        { "key": "...",
                      "base_url": "https://api.z.ai/api/coding/paas/v4",
                      "model": "glm-5.3",
                      "body": { "thinking": { "type": "disabled" } } },
      "local":      { "base_url": "http://127.0.0.1:11434/v1", "model": "qwen3:8b" }
    }
  }
}
```

`body` is the per-provider extras from §10.3, merged into every request that provider receives. The
`zai` entry carries the one field a GLM request cannot go out without, and §10.10 says what happens
when it is missing.

The CLI creates that file at 0600 and refuses to read it when it is group or world readable, the
same check `src/crypto/kek.rs` applies to the key file. `lumberroom ingest keys set <provider>` reads the
key from stdin, never from an argument, and writes it in place.

**There is no `--api-key` flag.** Every argument of a running process is world readable through
`ps`, and an interactive shell writes the command to its history file, so a key passed that way
leaks to two places the owner did not choose. One environment variable per provider is honoured,
`LUMBERROOM_INGEST_KEY_<PROVIDER>`, for the cron case where there is no interactive shell to read a
config file into. The key never enters the working directory, the run record, a log line or an
error message; failures print the provider name and the HTTP status.

### 10.5 Concurrency, rate limit and retry

```
lumberroom ingest extract --run <id> --provider openai|anthropic|openrouter|zai|custom
                    [--model <id>] [--base-url <url>]
                    [--concurrency 4] [--rpm N] [--timeout 120]
                    [--max-attempts 5] [--dry-run] [--yes] [--json]
```

`--concurrency` bounds in-flight requests, default 4. `--rpm` adds a token bucket over request
starts for providers that meter by rate rather than by concurrency; unset means no bucket.

Retry covers 429 and 5xx and nothing else. The delay comes from `Retry-After` when the response
carries it, and otherwise from exponential backoff starting at one second and doubling, with jitter
of up to half the interval, capped at `--max-attempts` attempts per chunk. A 429 also halves the
in-flight limit for the rest of the run, with a floor of one, because a provider that is refusing
work does not want four more requests a second later.

Every other 4xx fails the chunk on the first response. A 401 is a bad key, and a 400 is a bad body
or a bad model id, and none of them improves by being sent again. The chunk is marked failed with
its status and the provider's error message, the run continues, and the report lists it.

Key the retry on the HTTP status and treat the provider's own code as text for the report. Measured
on 20 August 2026: z.ai answers an unknown model with 400 and the body
`{"error":{"code":"1214","message":"modelCode: does not exist"}}`, where `code` is a JSON **string**
rather than a number. A client comparing that field against an integer, or switching on it to decide
a retry, gets the wrong answer on the first bad model id anyone types.

A request that outlives `--timeout` seconds is aborted through `AbortController` and counts as an
attempt.

### 10.6 Cost, estimated before and reported after

**Before.** `extract` prints an estimate from the characters already sitting in `spans/`: input
tokens at four characters per token, output tokens at a flat 400 per chunk, priced from a static
per-model table in the CLI carrying the date it was written. It is a design estimate and the line
says so. `--price-in` and `--price-out`, in dollars per million tokens, override the table for a
model it does not know, and an unknown model with no override prints token counts and no dollar
figure rather than a made-up one. Add a flat per-request overhead for JSON mode, since the provider
injects a system instruction of its own: measured at 59 prompt tokens on z.ai, 22 against 81 on
identical messages (§10.10).

**After.** The actual figure comes from the response: `usage.prompt_tokens` and
`usage.completion_tokens` on the OpenAI-compatible path, `usage.input_tokens` and
`usage.output_tokens` on the Anthropic path. `extract` sums them across chunks and prints tokens and
dollars, marked as observed.

A local endpoint may omit `usage` altogether. In that case the report says "usage not reported by
the endpoint" and prints no number. A synthesized total that reads like a measurement is worse than
an absent one, and this document has a house rule about it: a number is an observation with a place
it came from or a design target, and the line says which.

```
estimate   34 chunks · 812 spans · 198,412 chars · ~49,600 in / ~13,600 out tokens
           ~$0.09 at gpt-5.1-mini list price (table dated 19 Aug 2026, estimate)
actual     34 chunks · 51,204 in / 9,880 out tokens · $0.08 (observed, from usage)
```

### 10.7 Checkpoint and resume

The working directory is the checkpoint. `out/chunk-NN.json` existing and parsing means that chunk
is done, so re-invoking `extract` with the same `--run` sends only what is missing. A run
interrupted at chunk 400 of 685 resumes at 401.

`state.json` beside it carries per-chunk status for the parts a file's existence cannot express:

```json
{"chunks": {"00": {"status": "done", "attempts": 1, "in_tokens": 1504, "out_tokens": 288},
            "01": {"status": "failed", "attempts": 5, "last_status": 429},
            "02": {"status": "pending"}}}
```

`extract --retry-failed` re-sends the failed chunks and leaves the done ones alone. `submit` reads
whatever `out/` holds, reports how many chunks are missing or failed, and applies the hold-back rule
in §8.3 step 7: a file's watermark stops at the first byte of its earliest unextracted span. Advancing
past bytes nobody extracted would lose them permanently, which is the one failure in this pipeline
with no recovery, and §10.5 makes a permanently failed chunk ordinary rather than rare, since any
non-429 4xx fails a chunk on its first response.

The pairing to remember: interrupt `extract`, run `extract --retry-failed`, then `submit`. Running
`submit` straight after an interrupt is safe now and it was not before; it holds the affected files
back and names them instead of skipping their bytes.

### 10.8 `--dry-run`

Prints exactly what §10.6 estimates, plus the destination host, the model id and the chunk count,
then exits 0 having opened no connection. It is the command to run before the first provider pass
over an unfamiliar corpus, and it is what a cron job runs once by hand before it is scheduled.

Without `--dry-run` and without `--yes`, `extract` prints the same block and asks once:

```
sending 34 chunks, 812 spans, ~198,000 characters to api.openai.com (gpt-5.1-mini)
span text also reaches your lumberroom server at lumberroom.example.com for the tripwire scan (§9.1)
these are excerpts of your own transcripts. continue? [y/N]
```

The second line is there because the tripwire scan is real traffic the owner did not ask about, and
the consent prompt is where an owner decides what leaves. It names the lumberroom host, and it prints
`(loopback)` beside it when the server is local.

**The order of the three checks, since two of them interact.** Loopback first: a base URL resolving
to loopback or a private range skips the prompt and prints which rule it applied, so a cron line
against Ollama needs no `--yes`. Then `--yes`, which skips the prompt for a remote endpoint. Then the
terminal: with a remote endpoint, no `--yes` and no TTY on stdin, `extract` **exits 1 with a message
naming `--yes`** and sends nothing. It never treats EOF as consent and it never blocks waiting for a
keystroke nobody can type, which are the two things a cron line missing a flag would otherwise do.

### 10.9 Unattended runs

Scheduled ingest is Mode B, and the CLI makes it one command:

```
lumberroom ingest run --source claude --project lumberroom --since 1d \
                --provider custom --base-url http://127.0.0.1:11434/v1 --yes --json
```

`run` composes `plan`, `extract` and `submit` against one run id, writes its output to
`<run-dir>/run.log`, and exits 0 when every chunk succeeded, 2 when the run finished with failed
chunks, held-back files or a truncated traversal, and 1 when it could not start or could not take the
run lock. A cron line reads the exit code, and 2 is the one that means the numbers are partial.

**`run` cleans up after itself, because a nightly job that does not is a growing pile of plaintext.**
§9.4 names `spans/` as the exposure and makes deletion a manual `lumberroom ingest clean`, which nobody
types on a schedule. So on exit 0, `run` deletes `spans/` and `out/` and keeps `report.json`,
`worklist.json` and `run.log`, which hold counters and file paths and no transcript text. On exit 2 it
keeps everything and prints the path, since a `--retry-failed` needs the spans. `plan` sweeps run
directories older than `INGEST_RUN_RETENTION_DAYS` (default 7) at the top of every run, so an
abandoned exit-2 directory expires rather than accumulating. `lumberroom ingest clean --all` does the sweep
on demand.

Three things hold under cron the same way they hold interactively: auto-approval still needs the
owner-typed substring check in §2.4, everything else still queues for a person, and the tripwire
still runs at plan time and again at submit time. A scheduled run fills a queue. It does not decide
anything.

Running this on the server after a nightly transcript sync is the same command with different roots.
This spec does not design the sync, and until one exists the roots are local paths on the owner's
machine.

### 10.10 Measured against z.ai, 20 August 2026

Every figure here comes from a live call to `https://api.z.ai/api/coding/paas/v4/chat/completions`
with a real key on 20 August 2026. They replace what the first draft of §10.3 inferred from z.ai's
documentation, which contradicts itself on whether the endpoint takes `temperature: 0`. It does.

**Request behaviour.**

- `temperature: 0` returns 200. Keep sending it.
- `do_sample: false` works too. `temperature: 0` travels to every OpenAI-compatible provider and
  `do_sample` does not, so the CLI sends temperature and this spec drops the `do_sample` advice.
- `thinking: {"type":"disabled"}` is accepted and it changes the run. Same prompt, same answer:
  the default returned 209 completion tokens in 5.6 seconds with `reasoning_content` on the message,
  and disabled returned 5 completion tokens in 1.8 seconds with no reasoning block. 40 times fewer
  output tokens for the same content. Send this field on every GLM request. It belongs in the body
  the CLI builds rather than in a paragraph about saving money.
- **JSON mode plus default thinking hangs, and this is the one that costs an afternoon.**
  `response_format: {"type":"json_object"}` with thinking left at its default returned nothing:
  the first call timed out at 25 seconds and the retry timed out at 90. No error, no status, no
  partial body. The identical request with `thinking: {"type":"disabled"}` came back in 2.1 seconds.
  A client that sends JSON mode and leaves thinking alone hangs on every chunk of every run, and the
  response holds nothing to diagnose it with. **JSON mode requires thinking disabled.**
- JSON mode adds about 59 prompt tokens, 22 to 81 on identical messages, so the provider injects a
  system instruction of its own. Add it to the per-chunk estimate in §10.6.
- An unknown model id returns 400 with a string `code`, per §10.5.
- `glm-4.7` fenced its JSON output, per §10.3.

**Model comparison.** Five spans went to each model: three real owner-typed turns from this project's
history and two deliberate traps. Trap one was lumberroom's own digest, the text the SessionStart hook
injects, which has to yield zero facts. Trap two was ephemeral chatter, "run the tests again, check
the status", which has to yield zero facts for a different reason.

| model | wall clock | facts | digest trap | ephemeral trap |
|---|---|---|---|---|
| `glm-5.3` | 3.2s | 3 | passed | passed |
| `glm-4.7` | 9.1s | 4 | **fired** | passed |
| `glm-4.5-flash` | 66.8s | 4 | **fired** | passed |

Two of the three proposed lumberroom's own facts back to lumberroom from its own digest, against a prompt that
forbids it in as many words. §4 carries what that means for the design.

All three returned valid JSON with no fence in this run, and every quote came back as a genuine
substring of its source span, so the auto-approval gate in §2.4 works against any of them. The parser
still keeps its fence tolerance, because the same model fenced its output in a separate call the same
day.

**The recommended default on z.ai is `glm-5.3`.** It ran three times faster than `glm-4.7` and
twenty times faster than `glm-4.5-flash`, it was the only one to decline the digest, and it proposed
three facts where the others proposed four, which on these five spans is the better number. It costs
more per token than
`glm-4.7`. Per-token price is the wrong figure to optimise on this workload: the cheaper model spent
three times the wall clock and produced a proposal the owner has to reject, and a rejected proposal
costs a person's attention rather than a fraction of a cent.

**Free models are out for backfill work.** `glm-4.7-flash` returned 429 with
`1305 service may be temporarily overloaded` twice inside 600 milliseconds, so the retry rules in
§10.5 spend the run backing off. `glm-4.5-flash` took 66.8 seconds for five spans, which across
thousands of spans is days of wall clock. Both are fine for a one-chunk experiment and neither
finishes a backfill.

**Cost anchor.** Those five spans cost 555 prompt tokens and 249 completion tokens. It is one data
point from a hand-run call, and §10.6 still prints an estimate the CLI computes rather than this
number.

### 10.11 Mode C, the batch extractor

Mode C lives inside §10 because it reuses what §10 already specifies: the same key file, the same
prompt, the same tolerant parser, the same cost accounting, the same failed-chunk handling. The
transport is the only part that changes.

**The seam is already there.** `extract` is a boundary with files on both sides. Chunks go in as
`spans/chunk-NN.json` and results come out as `out/chunk-NN.json`, and `plan` and `submit` cannot
tell what filled them. Mode A dispatches subagents, Mode B posts one request per chunk and waits,
Mode C posts every chunk as one job and collects the results later. Same contract on both sides, so
the third mode adds no table, no admin route and no change to `submit`.

**Two levers, and the owner's question tangled them together.** Batch buys a discount on a model
already chosen. OpenRouter buys access to cheaper models, and that needs no batch support at all,
since it is a different `base_url` and `--model` on the synchronous path in §10.3. Nobody should
build Mode C expecting it to make OpenRouter cheaper. It makes whichever model the owner picked
cheaper, on the providers that publish a discount.

**Who supports it, probed live on 20 August 2026.**

- **OpenRouter.** `POST https://openrouter.ai/api/beta/batches` creates a batch and
  `GET https://openrouter.ai/api/beta/batches/:id` polls it and returns the results, both with
  `Authorization: Bearer <key>`. An invalid body sent on purpose came back 400 with
  `Batch body ended before a requests array was found`, so the route exists and parses what it gets.
  Their documentation quotes the discount: batch requests bill at 50% of the model's standard
  per-token pricing, mirroring OpenAI and Anthropic. Non-token components stay at standard rates,
  web-search calls included, and prompt-caching rates vary by model.
- **z.ai coding plan**, the host §10.10 measured. `GET /batches` and `GET /files` both return 404.
  No batch API there.
- **z.ai general API**, `https://api.z.ai/api/paas/v4`. `GET /batches` returns 200 with
  `{"object":null,"data":null,"first_id":null,"last_id":null,"has_more":null}` and `GET /files`
  returns 200 with `{"data":[],"object":"list"}`. Those are OpenAI's list envelopes, so the standard
  files-then-batch flow should work. The endpoints exist and respond; the full flow is untested.
  z.ai publishes no batch discount and this spec quotes none.
- **OpenAI and Anthropic** both publish batch APIs at roughly half price. Neither was probed here.

**The flow, against OpenRouter, which is the one to build first.**

1. Build the request array from `spans/`. One item per chunk,
   `{"custom_id": "<chunk id>", "body": {...}}`, where `body` is the §10.3 request the synchronous
   path would have sent for that chunk, prompt and all.
2. POST the batch. Requests go inline in the body, so there is no file to upload.
3. Poll `GET /api/beta/batches/:id`. The status runs `validating` to `in_progress` to `finalizing`
   to `completed`, with `failed`, `expired`, `cancelling` and `cancelled` as the other outcomes.
   `completed`, `failed`, `expired` and `cancelled` are terminal.
4. Read the results off the completed batch. They arrive inline as an array, each item carrying
   exactly one of `response`, with `status_code`, `request_id` and `body`, or `error`. There is no
   download step.
5. Split them by `custom_id` back into `out/chunk-NN.json`, through the same §10.3 parser Mode B
   uses. `submit` then runs unchanged and cannot tell which mode produced the files.

**The key-order trap, and it belongs in the same register as the JSON-mode hang.** OpenRouter parses
the batch body as a stream, so the order of keys in the JSON decides whether the request works. Their
documentation says to serialize `endpoint` and `model` before `requests`, and the 400 above is that
parser reaching the end of the body without having found the array. `JSON.stringify` preserves
insertion order for string keys, so the request succeeds when the object literal is written in that
order and fails when anyone reorders the fields or round-trips the object through anything that sorts
keys. Build the body as a literal with `endpoint` and `model` first, and pin the order in a test: a
reordering reads as cosmetic in review and turns every batch into a 400.

**Turnaround, and what it rules out.** `24h` is the only completion window OpenRouter supports, so
one round of tuning the extraction prompt costs a day. Mode C is for the full backfill and for
scheduled runs. Calibrate the prompt first with Mode A or Mode B on a handful of chunks, where a bad
instruction costs seconds, and switch to Mode C once the prompt has stopped changing.

**The batch id is the better checkpoint, and it composes with the one §10.7 has.** §10.7 checkpoints
on `out/chunk-NN.json` existing, which is per chunk and local to the machine. Mode C's job id is one
string the provider holds the whole run's state behind, so `state.json` gains
`batch: {"id": ..., "provider": ..., "status": ..., "created_at": ...}` and `extract --run <id>` on a
run that already carries a batch id polls it rather than creating a second one. Losing the run
directory's `out/` files costs a re-fetch instead of a re-send, and the results stay fetchable until
the batch expires. The two checkpoints coexist: the batch id resumes the fetch, and the chunk files
stay the record of what came back.

**A partial failure lands as failed chunks and never as missing output.** A batch completes with
`request_counts` shaped `{"total":100,"completed":98,"failed":2}`, and each result item carries
`response` or `error`. Every `error` item writes its chunk into `state.json` as failed with the
provider's status, which is the shape §10.7 already defines, so `extract --retry-failed` picks it up
and `submit` counts it in `chunks_failed`. No chunk leaves a run without a line in the report saying
what became of it. §8.3 step 7 then holds the affected files' watermarks at the first byte of their
earliest unextracted span, so a batch that half-failed costs a retry rather than transcript bytes.

**Expiry is recoverable.** A batch that misses its window ends at `expired`, and the chunks it never
returned are failed chunks by the rule above. `spans/` survives, since §10.9 deletes it only on exit
0 and an expired batch exits 2, so `extract --retry-failed` resends without re-planning. A started
batch may not be stoppable: OpenRouter's status list carries `cancelling` and `cancelled` and its
documentation names no cancellation endpoint, so a Mode C run costs whatever it costs once the owner
submits it.

**The spans sit in a third party's object storage for 30 days.** OpenRouter stores batch inputs and
results as JSONL artifacts in Google Cloud Storage and deletes them 30 days after creation. That is a
different exposure from a synchronous call, where the spans live in the provider's request path and
whatever it retains for abuse monitoring, so the consent block in §10.8 states it on a Mode C run
beside the destination host and the tripwire line. §9.4 covers the local copy in `spans/`. This is
the remote one, and the owner decides on it before the batch goes out.

**Selecting Mode C is explicit and never inferred.** `--batch` is a flag the owner types. The
provider entry declares support, `"batch": { "endpoint": "https://openrouter.ai/api/beta/batches" }`,
and `extract --batch` against a provider with no such entry exits 1 naming the provider rather than
falling back to the synchronous path. Nobody should wait a day for an interactive run because a
default moved under them. `extract --run <id> --batch-status` prints the job's status and its counts
and creates nothing.

**What we do not know, recorded rather than guessed.** OpenRouter documents no limit on batch size,
no limit on concurrent batches, and no list of which models are eligible. Find all three before a
corpus-wide run, since 685 files is thousands of chunks in one array.

## 11. The skill

**Location:** `~/.claude/skills/lumberroom-ingest/SKILL.md`, installed by `scripts/wire-mac.sh` alongside
the SessionStart hook. A copy lives in the repo at `client/skills/lumberroom-ingest/SKILL.md` so it is
reviewable and versioned.

**Name:** `lumberroom-ingest`. Invoked as `/lumberroom-ingest` from inside Claude Code. The skill is Mode A:
it exists so the host agent can be the extractor. When the owner asks for a provider run, the skill
runs `lumberroom ingest extract --provider ...` on step 4 and skips the dispatch, which makes it a wrapper
around Mode B and leaves the run identical everywhere else.

**Description line, which is what routing actually reads:** use when the owner asks to ingest agent
transcripts into lumberroom, extract memories from past sessions, or run a lumberroom ingest.

What it must instruct, in order:

1. Run `lumberroom ingest plan` with the arguments the owner gave, defaulting to the current project and
   the last 7 days. Print the exclusion table verbatim. Do not summarise it.
2. Echo `lumberroom-ingest-begin:<run_id>` as plain text, as the first thing after `plan` returns.
3. Read `worklist.json` for the chunk count. Print the estimate.
4. **Dispatch one Agent call per chunk, every call in a single message**, `subagent_type`
   `general-purpose`, each with the §8.2 prompt. State the two failure modes: separate messages run
   in sequence, and a read-only subagent type writes nothing and fails silently.
5. Wait, then verify each `out/chunk-NN.json` exists and parses. Warn per missing chunk, name the
   chunk, and stop if more than half are missing. Re-dispatch a named chunk rather than the run.
6. Run `lumberroom ingest submit --run <id>`, passing `--no-auto` through when the owner asked for it or
   invoked the skill with it. Print the report, including the held-back files and the fence counters.
7. Echo `lumberroom-ingest-end:<run_id>`.
8. Show the owner what `submit` wrote and what it queued, and tell them the approve and reject
   commands. Approve nothing on their behalf beyond what §2.4 already wrote.

**Where the fence really starts, since the skill cannot echo a run id it does not have yet.** `plan`
mints the run id, so the earliest the skill can name it is step 2. The fence does not depend on the
skill for its opening: `plan` prints `lumberroom-ingest-begin:<run_id>` as the first line of its own stdout,
which lands inside the `tool_result` for step 1, and §7.3's scan reads every entry's text whatever its
type. So the fence opens at `plan`'s first output line in both modes and whether or not the model
echoes anything. The skill's echo at step 2 is the redundant copy that survives a change to how tool
output is recorded, and two begin markers for one run id open one fence.

What sits outside the fence is the model's narration before it invoked `plan`, which is the owner
asking for an ingest and the model agreeing to run one. Losing the fence over that is not worth a
mechanism.

Hard rules the skill states to itself and to every subagent: call no memory tool during a run, run
no git command, write no file outside the run directory, and never claim a fact was verified when it
was extracted.

---

## 12. Exit test

`scripts/ingest-test.sh`, run against a live server and a real transcript directory, the way Phase 1
and Phase 4 are tested.

1. **The digest does not feed itself.** Seed the store with a distinctive fact. Start a Claude Code
   session so the SessionStart hook injects the digest containing it. Run plan and submit over that
   transcript. Assert zero proposals whose content matches the seeded fact, and assert the
   `attachment` exclusion counter is non-zero. This is the invariant the agentmemory pass says has no
   equivalent test anywhere in that repo. The test asserts on the pipeline's exclusions and never on
   the extractor's judgment: two of three models pulled facts out of a digest span on 20 August 2026
   against a prompt forbidding it (§4), so a run that passes because the model behaved has not been
   tested. Feed the digest span to whichever extractor the run uses and require zero proposals out
   of the pipeline rather than zero facts out of the model.
2. **A memory tool result does not become a fact.** In the same session, call `memory_search` for the
   seeded fact. Assert the joined tool name appears in the memory-tool exclusion count and that no
   proposal carries the result text.
3. **The owner's words auto-approve and the model's do not.** Assert a fact the owner typed lands
   at `state = 'written'` with a `quote` that is a substring of the transcript span, and that a fact
   the model inferred stays at `state = 'proposed'`. Re-run with `--no-auto` and assert the first
   one queues instead.
4. **Approval goes through the write path.** Approve one and assert the row exists, carries an
   embedding, and that approving a near-duplicate returns `deduplicated: true` rather than a second
   row. Approve a proposal containing a fake credential and assert the tripwire refuses it.
5. **Running twice adds nothing.** Run plan and submit again immediately. Assert zero new proposals,
   zero new source rows, and that every subagent transcript from run one carries
   `skip_reason = 'ingest_artifact'`.
6. **A rejection sticks, and it can be undone.** Reject one, re-run, assert it is not proposed
   again. Then `lumberroom ingest unreject` it, assert it is back at `state = 'proposed'` with its source
   rows intact, and assert `reject` with no `--yes` and no TTY exits 1 rather than rejecting.
7. **Mode B says what it will send, then resumes where it stopped.** Point `--provider custom` at a
   local OpenAI-compatible endpoint. Assert `--dry-run` prints the estimate and opens no connection.
   Run it for real, kill it mid-run, re-invoke with the same run id, and assert it sends only the
   chunks with no `out/` file. Feed one chunk's output through both modes and assert identical
   content collapses on fingerprint, so the two modes fill one queue and the only row that differs
   is `extractor`. Assert that a remote base URL with no TTY and no `--yes` exits 1 naming the flag,
   and that a loopback base URL with no TTY proceeds.

8. **The emission layer fires, and it fires on the case a broken E1 produces.** This one defeats E1
   on purpose, because a layer justified as the thing that survives a format change has to be proven
   rather than assumed. Seed the store with a distinctive fact and read it back through
   `memory_search` so `recall_emission` holds its hash. Then hand-write
   `out/chunk-99.json` containing that fact verbatim against a `source_span_id` that is in the
   worklist, which is exactly the output a parser with E1 removed would produce. Run `submit`.
   Assert zero proposals for that content, `confirmations` incremented by one, and the seeded
   memory's confirmation count raised. Then set `INGEST_EMISSION_WINDOW=0`, repeat, and assert the
   same input now becomes a proposal, which proves the window is live rather than a hash match
   against everything ever emitted. Post the same content directly to
   `POST /admin/ingest/proposals` with the CLI's check bypassed and assert the server confirms it
   too, since the server-side check is the one a CLI cannot skip.

9. **The artifact stamp does not blacklist the owner's session.** Start a session, invoke the skill
   immediately so the dispatch `tool_use` entries land in the first 64KB of the main-thread file, and
   let the run finish. Assert every `agent-*.jsonl` from that run carries
   `skip_reason = 'ingest_artifact'`, and assert the **main-thread file carries none**. Then
   `lumberroom ingest unskip` one subagent file and assert it re-enters the next plan's file count.

10. **An interrupted run does not eat the rest of the session.** Run the skill, kill it during
    dispatch so the begin marker lands with no end marker and `submit` never runs. Type more work
    into that session. Run `plan` again and assert the entries after the fence bound reach
    extraction, that `fences_unclosed` is 1, that `entries_excluded.ingest_fence` is non-zero, and
    that the report names the file and the byte range. Repeat with `submit` having run and no end
    marker echoed, and assert the fence closes on `ingest_run.finished_at` rather than on the begin
    marker's timestamp.

11. **A partial extract holds bytes back instead of losing them.** Plan a corpus that puts one
    file's spans across several chunks. Delete one chunk's `out/` file and run `submit`. Assert that
    file's watermark sits at the first byte of the earliest span in the deleted chunk rather than at
    `plan_ceiling`, that `files_held_back` names it, and that a file whose spans were all excluded
    still advanced. Then `extract --retry-failed`, `submit` again, and assert the watermark reaches
    `plan_ceiling` and every fact from the recovered chunk arrives.

Step 5 is the criterion at the top of this document. Steps 1, 2 and 8 are the ones that decide
whether the store survives the feature, and steps 9, 10 and 11 are the ones that decide whether the
owner's transcripts survive it.

---

## 13. Order of work, and how big this is

This is the largest phase in the project. Ingestion touches the filesystem, two transcript formats,
a new table set, the CLI, the admin routes, a skill, and a provider client. Sections 4, 5 and 7 are
the parts where being wrong is expensive and none of them is mechanical.

**The first cut, which produces value on one week of one project.** Mode A end to end, because the
owner will be sitting in a session anyway on the first run and watching the exclusions fire is the
point of it. Every fix the reviews forced sits in this list rather than in the Mode B tail, because
the first cut runs inside a live session and those four failures are the ones a live session
triggers:

0. Wrap the SessionStart preamble and digest in `<lumberroom-context>...</lumberroom-context>` in `lumberroom`,
   around line 614. It is first because E3's counter reads as evidence only once the token it names
   can fire (§4.1), and because nothing else depends on it.
1. Migration 009: `ingest_proposal`, `ingest_proposal_source`, `ingest_watermark` with its fence and
   skip columns, `ingest_run` with the counters in §6.4. Lock it before anything else is written,
   per the orchestration rule, since five tracks bind against it.
2. `recall_emission` on the §4.4 shape, keyed on content hash and never on session id, plus the
   batched writer in `context_bootstrap` and `memory_search` piggybacked on the `last_accessed_at`
   path at `src/adapters/postgres/memory.rs:952`. It has to be recording before the first run, or
   that run has no belt-and-braces layer at all. This is where the first blocker landed and it is
   the only work item whose value depends on shipping early.
3. The Claude Code parser in `lumberroom`: the recursive walk over the nested layout in §6.1, the
   streaming reader, the watermark with its monotonic advance, the `tool_use_id` join, the fence scan
   that runs before every exclusion, every exclusion in §4.1, the speaker classifier in §5, span
   cutting, chunking. This is the bulk of the work and it is where the correctness lives.
4. `POST /admin/ingest/scan` and the plan-time tripwire call, plus the sensitive-path refusal and its
   false-positive test.
5. `src/ports/ingest.rs`, the port the proposal store sits behind, then the Postgres adapter for it,
   then the admin routes and the service, with `approve` calling `services::write::run`. One file per
   port and no SQL outside the adapter, the same as every other store in the tree.
   `POST /admin/ingest/emissions/check` and the server-side emission check inside the proposal
   handler belong to this step: the layer from step 2 has no consumer until they exist.
6. `lumberroom ingest plan`, `submit`, `list`, `show`, `approve` with its ids and its `--run` filter,
   `reject`, `unreject`, `unskip`, `clean`. The hold-back rule in §8.3 step 7 is in `submit` and it
   is the second blocker; write it before the skill exists, since an interrupted Mode A run is how it
   gets exercised.
7. The skill, and the §8.2 prompt. The three artifact conditions in §7.2 and the fence markers in
   §7.3 are what make a run in a live session safe to repeat, so they land with it.
8. `scripts/ingest-test.sh`, steps 1, 2, 5, 8 and 9 first. Step 8 is the only thing that turns the
   emission layer from an assumption into a fact, and step 9 is the only thing that proves the
   artifact stamp does not overfire onto the owner's own session.

**Mode B, in the same phase.** Sequenced after the list above because it consumes the same
`spans/` and `out/` contract, and the contract has to exist before a second producer writes into it.
Nothing here waits on how the first run goes:

9. The OpenAI-compatible request path, the key file and the `keys set` command, `--dry-run`, the
   estimate, and the consent block in §10.8 with its loopback, `--yes`, no-TTY ordering. One code path
   reaches OpenAI, OpenRouter and any local endpoint, so this is the step that unlocks both a hosted
   backfill and a fully local unattended one.
10. Concurrency, the rate limit, retry with backoff, `state.json` and resume, and the observed-usage
    figures. This is what makes 685 files a run that finishes rather than a run that is restarted.
11. The Anthropic path, which is a second request shape and one response accessor that takes the
    first text block rather than the first block.
12. `lumberroom ingest run` for cron, its exit codes, the exit-0 cleanup and the retention sweep, and
    `scripts/ingest-test.sh` steps 7, 10 and 11.

**The first run, small enough to do this week and to abandon without cost.** One project, one week,
Mode A, in a session the owner is sitting in:

```
lumberroom ingest plan --source claude --project lumberroom --since 7d --max-files 40
/lumberroom-ingest --no-auto            # the skill dispatches the chunks and passes the flag to submit
lumberroom ingest list --state proposed
lumberroom ingest clean --run <id>
```

Three flags make that run safe to get wrong, and one property makes it safe to abandon.
`--project lumberroom --since 7d` holds it to tens of files out of 685, so the exclusion table is short
enough to read line by line, which is the point of a first run. `--max-files 40` caps the walk and
stamps `traversal_capped`, so a surprise stays small. `--no-auto` writes nothing at all: every row
queues, and the owner reads what auto-approval *would* have written before trusting it with a
keystroke. The property is §2.1: nothing in the run reaches the store except through
`lumberroom ingest approve`, so the worst outcome is a queue worth rejecting and a directory worth deleting.

Read the run report for three things before scaling up. The `attachment` exclusion count, non-zero or
E1 is broken. The `ingest_fence`, held-back and `unknown` lines, all zero on a first run and the ones
that matter on the second. And the count of proposals against the count of `owner_typed` spans, which
is the number that says whether extraction is worth the tokens on a corpus this size.

Then run it twice back to back before pointing it at anything larger. The second run posting zero new
proposals is §12 step 5, and it is the criterion at the top of this document.

**Deferred, and named so it is not mistaken for done:**

- **Codex.** §4.2 and §5 specify the parser from first-hand measurement and it is not built. Codex is
  245 sessions and 567MB, and it is the second-largest thing this feature will eventually read.
- **Corpus-wide ingestion.** 685 files and 732MB, with 82% of them subagent transcripts. Run it after
  one week of one project has proved the exclusions hold. This is Mode B work, and steps 9 to 12 are
  what it waits on. `recall_emission` holds nothing for any of it, per §4.4, so the backfill runs on
  E1 through E7 alone and `--no-auto` is the right default for it. Mode C is the cheaper way to run
  it, and the prompt has to be calibrated first either way.
- **Mode C, the batch extractor.** §10.11 specifies it from live probes. What it takes: the batch
  client (build the request array, POST, poll, split the inline results into `out/chunk-NN.json`),
  the `--batch` and `--batch-status` flags, the `batch` entry in the provider config, the batch id in
  `state.json`, the 30-day storage line in the consent block, and an exit-test step of its own. It
  buys half price on the providers that publish a discount and it costs up to 24 hours of turnaround,
  so it lands after the extraction prompt has stopped changing.
- **Compaction.** Neither the agentmemory pass nor the sampling here found `isCompactSummary` or
  `compactMetadata` in a Claude Code file, and the Codex side carries a `compacted` entry type that
  was seen once and not characterised. Sample for it before writing a branch for it.
- **Supersession from ingestion.** A proposal can carry `supersedes`, the column exists, and nothing
  fills it. Letting an offline extractor retire a live fact is a bigger decision than this spec
  makes.
- **Hermes.** No local state was found. There is nothing to parse yet.
