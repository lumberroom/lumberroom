# MCP answers carry what an agent acts on

Date: 12 October 2026, revised the same day after a blind review. Status: proposed. Engine first,
then the hosted fork through `scripts/merge-upstream.sh`. Decision records to write with the code:
engine `0031` and fork `0157`. Engine 0030 is taken on `origin/feat/write-cap`, so claim each number
against `main`, the open PRs and every unmerged remote branch before writing it.

## 1. What the owner decided

Aditya ruled on 12 October 2026 (lumberroom memory `85aedb3a`): do all five cuts.

1. `context_bootstrap` structured payload shrinks to the digest text plus counts. Full rows move
   behind an opt-in argument. Changed in design: the brief payload also keeps every row the text did
   not print whole, plus `inventory` and `sealed_inventory` (D4), because a Claude Code model sees
   those rows today. Estimate, not a measurement: about 21 to 26 KB against 36.2 KB.
2. `review_queue` reads about 10 items by default, and fact text returns to `structuredContent` so
   Claude Code can work the queue.
3. `memory_search` text goes out as compact JSON. Per-hit bookkeeping goes: `score`, `primary`,
   `sensitivity` when open, microsecond timestamps. Hit count and ranking stay.
4. Internal fields go: `Digest.tenant`, per-namespace counts, `promotion_queue`'s
   `owner_user_id` and `cluster_key`, `memory_history`'s `embedding_model` and access counters, and
   the raw client id fallback in the fork's `author_label`.
5. Tool descriptions get shorter.

He ruled on the gate the same day (memories `4b59b108`, `e0008c0d`): every G2 session and the judge
run as headless Claude Code on legolas on his subscription, against a restore of the pre-migration
production dump, with a synthetic teams tenant for the fork's teams tasks.

Three constraints hold over all of it.

- **A. The way to more data stays obvious.** Opt-in arguments, limits, offsets and follow-up tools
  carry it. A `more` hint appears only on an answer that holds back something the caller can still
  ask for.
- **B. Answers make agents terser.** No bookkeeping an agent will parrot back.
- **C. Agent behaviour does not change.** The same searches return the same memories in the same
  order. Agents make the same tool calls and the same decisions (write, supersede, forget, review
  verdicts). Section 9 defines the gate that shows it.

One item of cut 3 does not ship: timestamps keep full precision (D6). The owner approved this on
12 October 2026 (memory `e0008c0d`).

## 2. Baseline

Owner-measured on production with a read-only token on 12 October 2026, about 4 bytes per token.
These are observations; every other number in this spec is a design target unless a line says
otherwise.

| Surface | Structured | Text |
|---|---|---|
| `tools/list`, 7 tools a read-only token sees | 16.8 KB | |
| All 21 fork tools | about 13,900 description chars, about 13,500 schema-doc chars | |
| `context_bootstrap` | 36.2 KB, of which the `text` field is 6 KB, `recent` 11.6 KB (8 rows), `profile` 9.3 KB (10), `project_context` 6.0 KB (5), `registry` 1.8 KB | 6.0 KB |
| `memory_search`, 8 hits | 13.1 KB | 14.1 KB (the same JSON, pretty) |
| `review_queue`, default | 71.7 KB, 96 items, no fact text | 251.7 KB |

## 3. Ground truth: who reads which channel

Every MCP answer goes out on two channels from the shared wrapper `Lumberroom::run`
(`src/mcp/mod.rs:420-492`, the `Ok((text, structured))` arm at `:481-485`). A client reads one of
them and hands it to the model or to code.

| Consumer | Reads | Fields it reads | Source |
|---|---|---|---|
| Claude Code, model calls | `structuredContent` | everything | memory `76a5c2fe` |
| lumberroom-claude-code mod, SessionStart | structured | `text`, `counts.memories` | `src/digest.ts:33-45` |
| lumberroom-claude-code mod, per-prompt recall | structured | `hits[].id, namespace, content, source, occurred_at`, `similarity` as a floor; it also stores `score` and `created_at` in the recalled-hit record its status line and "why recalled" pane show | `src/recall.ts:34-44, 100-131, 160-171`, `hooks/register.ts:369` |
| lumberroom-claude-code mod, pre-write similar rows | structured | `hits[].similarity >= SIMILAR_MIN` | `hooks/register.ts:406-416` |
| Hermes plugin | structured; model-facing tool results are `json.dumps(structured)` | `text`, `hits[].id, namespace, content, source, occurred_at` | `provider.py:331, 346, 399`, `recall.py:42-53` |
| OpenClaw plugin | structured; model-facing tool results are `JSON.stringify(structured ?? {})` | `text`, `hits[]` as Hermes | `src/tools.ts:64`, `src/hooks.ts:93, 146`, `src/recall.ts:42-48` |
| lumberroom CLI | structured | search: `hits[].id, namespace, content, score` (`score` is a required field); write: `id, namespace, deduplicated`; `--json` prints the whole payload | `crates/lumberroom/src/wire.rs:40-46`, `commands.rs:214-275, 462, 1332`, `eval/runner.rs:157` |
| `scripts/policy-test.sh` | structured, through `lumberroom bootstrap --json` | `inventory`, `sealed_inventory`, `counts.by_namespace`, `text`. A missing key passes its leak checks without checking anything | `scripts/policy-test.sh:360-385, 425-520` |
| CLI `bootstrap --hook`, OpenWebUI filter | text | the digest markdown | `commands.rs:195-210`, `client/openwebui-filter.py` |
| claude.ai, ChatGPT, other connectors | text | everything | |
| Engine tests | structured | named keys; `tests/mcp_source.rs:402-411` reads the digest's `profile`, `project_context` and `recent` | `tests/mcp_capability.rs`, `tests/mcp_source.rs`, `tests/review_queue_mcp.rs` |
| lumberroom-web | neither | | no MCP call in `app/`, `lib/`, `functions/` |

Two facts settle most of the design.

- **No tool may drop `structuredContent`.** OpenClaw would hand its model `{}`. Both channels
  stay, and each carries the full answer for its readers.
- **Claude Code, Hermes and OpenClaw put the structured payload in front of the model.** For them a
  field in `structuredContent` costs tokens on every call. The CLI and `policy-test.sh` are the
  structured readers that never feed a model.

## 4. Decisions

### D1. One brief view per tool, the old shape behind an opt-in

Each tool that loses fields gets a brief view, built in `src/mcp/views.rs` the way decision 0020
built `MemoryView`: the service type stays as it is, and the MCP layer maps it. The console, HTTP
routes, export and `tests/integration.rs`'s key pin on `search::Hit` keep reading the service type.

The opt-in returns the shape the tool sends today, except where D7 drops a field everywhere and
where D4 adds `left_out`. A test pins that equality against the service serialization.

| Tool | Argument | Values | Default |
|---|---|---|---|
| `context_bootstrap` | `rows` | boolean | `false` |
| `memory_search` | `detail` | `"brief"`, `"full"` | `"brief"` |
| `memory_history` | `detail` | `"brief"`, `"full"` | `"brief"` |

`rows` follows the owner's own wording for the digest. `detail` names one enum, `Detail`, shared by
two tools, so a model learns one word. `review_queue` takes no `detail`: the two counters its brief
rows drop have no MCP reader, and `/admin/review` keeps them for an operator. A field list
(`fields: [...]`) lost: it asks the model to know field names it has never seen, and every name
becomes a pinned contract.

### D2. A declared CLI call gets the full shape

`lumberroom search` parses `score` as a required field (`wire::Hit`), and installed CLIs cannot send
`detail`. When `ctx.invocation == Invocation::Cli` (header `x-memory-invocation: cli`), every tool
in D1 defaults to its full shape. A model reads none of that payload, so the token rule has nothing
to save there.

- Hooks (`Invocation::Hook`: the CLI `--hook` path, OpenWebUI, OpenClaw's hooks) and every other
  invocation value, `user` and the CLI's `--invocation <name>` included, get the brief default.
- The header grants nothing a caller could not ask for with `detail: "full"`, so a client that sets
  it gains no access.
- The CLI in the same release sends `detail: "full"` and, for `bootstrap --json`, `rows: true`
  itself, and `wire::Hit.score` becomes `Option<f64>` with a serde default, so a CLI run under
  `--invocation user` or a hook still parses a brief answer.
- What lost: requiring every CLI to upgrade. It breaks `lumberroom search` and `lumberroom eval` on
  every installed binary with a parse error.
- Reversal: once the CLI that sends `detail` itself has shipped in two tagged releases, the
  invocation default goes.

### D3. `memory_search`

Ranking, hit count, limits (default 8, max 50, `src/config.rs:1273-1274`) and the query do not
change. The view is a projection of `search::SearchResult` in the order the service returned it.

Brief hit:

```json
{
  "content": "the fact as stored",
  "created_at": "2026-09-28T17:35:39.860834+00:00",
  "id": "eabee4bf-d01b-41d3-8c29-80d34a094372",
  "namespace": "project:lumberroom-cloud",
  "similarity": 0.7608,
  "source": "Claude Code (lumberroom)",
  "tags": ["teams", "digest"]
}
```

Key order depends on the build. `serde_json::Value` maps sort their keys unless some crate in the
dependency graph turns on serde_json's `preserve_order` feature; the fork's production build sorted
them on 12 October 2026, as the answers this spec quotes show. Nothing in this design or its gate
depends on key order.

Optional keys, present only when set: `sensitivity` (any level above `open`), `occurred_at` and
`occurred_until`, `superseded_by` (only on an `include_superseded` search, as today). `tags` is
omitted when empty.

| Field | Brief | Why |
|---|---|---|
| `id` | kept | `supersedes`, `memory_forget`, `memory_history` take it |
| `namespace` | kept | an agent files the correction in the same namespace |
| `content` | kept | the fact |
| `tags` | kept when non-empty | `tags` filtering and merge verdicts reuse them |
| `source` | kept | decision 0020; Hermes, OpenClaw and the mod print it; the fork's `author_label` reads it |
| `created_at` | kept, full precision | `lr-review` orders rows by it; `as_of` compares against it (D6) |
| `similarity` | kept | the mod's recall floor and pre-write check read it; dropping it changes what the mod injects |
| `occurred_at`, `occurred_until` | kept when set, full precision | valid time; plugins print it; `as_of` and merges use it (D6) |
| `superseded_by` | kept when set | marks a retired row on a history search |
| `sensitivity` | kept above `open` | a model quoting a private fact into a shared document needs the warning; `open` says nothing |
| `score` | dropped | the fused rank score; the array order already carries it, and agents quote it back. The mod's status pane shows no score for a brief hit |
| `primary` | dropped | which arm of the namespace set answered; `namespace` and `also_searched` already say it |

Top level: `namespaces` and `also_searched` stay (short, and `also_searched` tells an agent another
project answered).

Search carries no `more` hint. It has no similarity floor, so any store larger than the limit fills
every answer, and a hint on every answer is noise an agent would quote. Learning whether rows exist
past the limit would also mean fetching `limit + 1`, and a larger fetch can reorder the first eight,
which constraint C forbids. The way to more rows lives in the `limit` argument's doc (D9).

`detail: "full"` returns today's `SearchResult` serialization.

### D4. `context_bootstrap`

The rendered text does not change by one byte. Hooks, the OpenWebUI filter and the text channel see
what they see today, so a session that starts from the hook starts from the same context.

**What the structured copy holds today that the text does not.** `render` prints whole entries
inside `BOOTSTRAP_MAX_CHARS` (6,000 by default) and counts the rest in a footer
(`footer` at `src/services/bootstrap.rs:680-697`, `render` at `:705`). A Claude Code model sees every one of up
to 23 rows plus the registry in structured today, including rows the text left out or shortened.
Dropping those rows would hide facts from it, which constraint C forbids. So the brief payload
carries every row the text did not print whole:

```json
{
  "counts": { "memories": 1234, "registry": 40 },
  "inventory": { "project:lumberroom": 212, "user:me": 340 },
  "left_out": [
    { "content": "exact text of a row the text left out", "id": "…", "namespace": "user:me",
      "tags": ["digest"] }
  ],
  "left_out_registry": [
    { "key": "services.lumberroom.port", "kind": "service", "namespace": "global", "value": 8080 }
  ],
  "more": "rows: true adds each printed fact's id and exact text; memory_search finds more",
  "project": "project:lumberroom",
  "sealed_inventory": {},
  "text": "## Memory digest\n..."
}
```

- `left_out` lists every memory in `profile`, `project_context` or `recent` that the final layout
  did not print whole: left out, or shortened with `…`. Each entry is `{id, namespace, content}`
  with the exact stored text, plus `sensitivity` above `open` and `tags` when non-empty. A row
  printed whole in one section and repeated in another is printed, not left out.
- `one_line` (`src/services/bootstrap.rs:377`) collapses a multi-line fact into one bullet and
  escapes a leading markdown marker. Such a row counts as printed whole: every word reaches the
  model, and the line breaks carry no fact. Listing it again would cost its whole length for its
  line breaks.
- `left_out_registry` lists registry entries the layout left out, in their `RegistrySummary` shape.
- Both keys appear only when non-empty. When every row prints, the brief payload is the text plus
  about 150 bytes.
- `render` reports what it left out: `bootstrap::render_report(d, max) -> Rendered { text,
  left_out, left_out_registry }`, with `render` returning `render_report(..).text`. It covers all
  three of `render`'s returns: the full digest (`:726`), a tail mode (`:741-746`) and the
  header-only budget (`:750-756`), where every row is left out. `run` stores the ids on `Digest` in
  two `#[serde(skip)]` fields, so the cached digest carries them.
- `more` appears when any section holds a fact.

`generated_at`, `cached`, `namespaces`, `tenant`, `counts.by_namespace` and the full sections leave
the brief payload. `inventory` and `sealed_inventory` stay: the text tail prints them only while
they fit, and `scripts/policy-test.sh` reads them.

`rows: true` returns today's `Digest` serialization without `tenant` (D7), plus `left_out` and
`left_out_registry` as id lists.

G1 reports, per project on the owner's store, the count and the bytes of the rows `left_out` names.

The module comment at `src/services/bootstrap.rs:22-23` calls the structured payload authoritative
because some surface truncates the markdown. It changes with the code: the brief payload carries
what the text left out, and `rows: true` carries the rest.

**Accepted risk.** The text bullets carry no memory id (`src/services/bootstrap.rs:466-480`). A
Claude Code agent that today supersedes a printed digest fact by the id in `structuredContent` now
needs `rows: true`, a `memory_search`, or the `possible_conflicts` answer `memory_write` already
returns. The G2 group "Supersede a fact the digest shows" measures this. If it fails, fallback F1
adds `printed: [{id, namespace}]` for printed rows to the brief payload: ids without the content the
text already shows.

### D5. `review_queue`

**Default size.** When `limit` is absent on the MCP tool, each source reads
`max(2, ceil(10 / n))` items, where `n` counts the sources this call reads. The engine's default
call reads conflict and stale: 5 each. The fork adds one proposal source (dreaming, fork
`src/main.rs:494-498`): 4 each, 12 at most. A call naming one source reads 10. An explicit `limit`
keeps today's per-source meaning, so the CLI, the HTTP routes and any caller passing a number see no
change. `limit`, `offset` and `has_more` keep their semantics; the service's `DEFAULT_LIMIT` of 50
stays for HTTP.

A total cap across sources lost. Paging a concatenation of per-source pages needs per-source counts
the service does not have, and `queue()` clamps a source's page at 200
(`src/services/review_queue.rs:629`), so an offset past 200 could not be served in one read.

**Paging is live.** Each source reads its page from the live queue with SQL `LIMIT` and `OFFSET`
(`src/services/review_queue.rs:473-479` for conflicts, `:529-532` for stale). A verdict removes its
item, so after an agent decides some items, `offset` no longer points where it did. `offset` also
applies to every source at once. A hint that names a next offset is wrong as soon as the agent acts,
so the hint names no number:

```json
"more": "items you leave stay first; offset N starts after the first N items of every source."
```

The text ends with the same sentence after `more: `. It appears only when `has_more` is true.

**`lr-review` changes to match** (`client/skills/lr-review/SKILL.md`, synced into the plugins by
`scripts/sync-skills.sh`):

- "Rows you leave stay on the first page, so page on with `offset`" becomes: decided items leave
  the queue and the items you leave stay, so read again from offset 0 after a pass, narrow `source`
  to one kind before paging, and set `offset` to the number of that kind's items you chose to leave.
- "When your harness has subagents and the queue runs past about 40 items, split it" assumed one
  read showed the whole queue. With pages of 4 or 5 per source, the trigger becomes: when `more`
  appears on the first read and your harness has subagents, read each source once with `offset: 40`
  and `limit: 1`; a source that returns an item holds more than 40, so split it as the section says.

**Fact text in `structuredContent`.** Decision 0019 stripped row content, `proposed_content` and
field values from the structured copy (`strip_free_text`, `src/mcp/extra_tools.rs:552-567`) so
untrusted text reached the agent only inside a fenced block. Claude Code reads only the structured
copy, so the queue is unusable there (memory `76a5c2fe`). The fence moves with the text instead:

- The MCP view wraps every source- or caller-written string in the structured copy with the format
  `render` draws today, `as_data(text, nonce)`, one fresh nonce per string: each row's `content`,
  `proposed_content`, and each field's `label` and `value`. The label goes inside a fence too,
  because the text block prints it inside one (`src/services/review_queue.rs:1292-1296` fences
  `"{label}: {value}"` whole).
- `review_queue::as_data` and `fence_nonce` (`:1211-1217`) become `pub(crate)` so the view draws the
  same fence. `render` does not change.
- `strip_free_text` goes. Decision 0031 supersedes that clause of 0019 and says why: a value that
  carries its own markers keeps the property 0019 wanted on every channel a model reads.
- `key`, `origin`, `kind`, `version` and `held_by` stay raw, as they are today. `review_decide`
  needs `version` byte for byte.

A fenced value matches, in dotall mode,
`^----- data ([0-9a-f]{12}) -----\n(.*)\n----- end of data \1 -----$`, and group 2 is the source
string unchanged.

**Brief rows.** `access_count` and `last_accessed_at` leave each row; `sensitivity` appears above
`open` only; `opened` appears only when false. Stale items keep `age_days`. Timestamps keep full
precision (D6).

The fork's `labels::withhold_queue` (fork `src/mcp/extra_tools.rs:397`) still runs on the `Queue`
before `render` and before the view, so a withheld colleague row stays withheld on both channels.

### D6. Timestamps keep full precision

Every timestamp passes through as the service serialized it, microseconds included. Truncation was
the owner's listed cut and it changes answers:

- `as_of` keeps a row when `COALESCE(occurred_at, created_at) <= as_of`
  (`src/adapters/postgres/memory.rs:436-459`). An agent that copies a `created_at` of
  `17:35:39Z` for a row stored at `17:35:39.860834` loses that row from an `as_of` answer it gets
  today.
- `review_decide`'s merge takes `occurred_at` from the caller (`src/services/review_queue.rs:809-815`);
  `lr-review` tells the agent to pass the newest one. A truncated copy stores an earlier instant than
  the source row holds.
- A date-only `occurred_at` is worse: the merge refuses a bare date
  (`parse_rfc3339`, `src/mcp/extra_tools.rs:533-546`).

The saving given up is small: about 13 characters per timestamp, one or two timestamps per hit, so
about 100 to 200 characters on an 8-hit search of about 12 KB, near 1 percent.

### D7. Fields no MCP answer carries

| Field | Where | Why |
|---|---|---|
| `Digest.tenant` | `context_bootstrap`, both shapes | the operator's tenant id. No consumer this spec found reads it (section 3); a script outside those repositories could, and `--json` users get a CHANGELOG line. In the fork it names a tenant to a model |
| `embedding_model`, `access_count`, `last_accessed_at` | `memory_history` brief | the store's own bookkeeping; `detail: "full"` keeps them |
| `owner_user_id`, `cluster_key`, the JSON copy of `text` | fork `promotion_queue` | fork plan |
| raw `source_client` fallback | fork `author_label` | fork plan; decision 0020 says MCP output names apps, never client ids |

The service structs keep every field. Only the MCP views change.

### D8. JSON text goes out compact

Every tool whose text is `serde_json::to_string_pretty` of its JSON (`memory_search`,
`memory_write`, `registry_get`, `memory_history`, `registry_history`, `registry_set`, `alias_set`,
`alias_list`, `review_decide`) sends `serde_json::to_string` of the same value instead, through one
helper, `views::compact`. Text and structured stay equal in content for these tools. Tools whose text
is rendered markdown (`context_bootstrap`, `memory_forget`, `review_queue`) keep their renderers.

A line-per-hit text format lost: it would change what claude.ai and ChatGPT agents read far more
than whitespace does, and the fork's labels re-render the JSON (fork plan).

### D8a. `memory_history`

Brief version, per row: `id`, `namespace`, `content`, `tags` (non-empty), `source`, `sensitivity`
(above `open`), `supersedes`, `superseded_by`, `superseded_at`, `occurred_at`, `occurred_until`,
`last_confirmed_at` (each only when set), `created_at`. Top level unchanged: `versions`, `withheld`,
`depth_capped`. `lr-review` reads `occurred_at`, `created_at`, tags, namespace and the supersedes
chain from this tool, and all of them stay. The brief history ships with the review queue (engine
PR 3), since `lr-review` is its main reader.

### D9. How the descriptions advertise the opt-ins

One clause in each argument's own doc comment, and nothing in the tool description except where
section 10 trims.

| Argument | Doc comment |
|---|---|
| `context_bootstrap.rows` | "true adds every fact the digest drew from, with its id and exact text. The digest text carries no ids." |
| `memory_search.detail` | "'full' adds each hit's fused score and whether it came from the primary set. Results and order are the same either way." |
| `memory_history.detail` | "'full' adds the embedding model and read counters of each version." |
| `memory_search.limit` | "Maximum rows. Default 8, at most 50 unless the operator changed it." |
| `review_queue.limit` | "Items to read from each source. When absent, about 10 in all, split across the sources read. Held between 1 and 200." |

None of these phrases may match the `ORDERS` denylist in `tests/mcp_tool_annotations.rs:42-65`.

### D10. `tools/list` cache hints

`ttl_ms` stays 0 and `CacheScope::Private` stays (`src/mcp/mod.rs:555-567`). A nonzero TTL with
private scope would be safe: every service checks the grant again on the call, so a stale list
shows a tool late, never a capability. It would also save no model tokens: the TTL changes when a
client fetches the list, not what the client puts in front of the model. A grant change would show
late by the TTL, including the fork's role-gated tools (`team_tools::visible`). Reversal: a client
shown to refetch the list on every call.

### D11. A write refuses the fence markers

With fenced text in the structured copy, the agent most likely to work the queue now reads markers
inside the JSON values it may copy into a merge. A row stored with a marker line would carry a
forged fence into every later queue and digest.

A write refuses `content` that contains a line matching `^----- (end of )?data [0-9a-f]{12} -----$`,
with code `content_carries_fence` and the message "content holds a data fence line; send the text
between the markers". The check lives in `domain::fence` (new, pure) and runs in `write::run_inner`
(`src/services/write.rs:95`), which `run`, `run_observed` and the fork's `run_carried` all reach,
so `memory_write`, the merge, ingest approval and dreaming are covered. The archive import is
exempt: an archive carries rows as they were stored. The archive merge already writes through
`write::run_restored`, which nothing else calls, so that entry point passes the fence-line check off
and needs no new function. Ingest approval (`src/services/ingest.rs:506`) keeps the check. The G2
checker also fails any run that
writes content containing `----- data `.

## 5. Pinned contracts

| Pin | Where | Kept or changed |
|---|---|---|
| `WriteOutcome` wire contract | `src/domain/types.rs:154-174`, CLI `wire::WriteOutcome` | kept. Only the text whitespace changes (D8). `possible_conflicts` keeps full `content`: the supersede choice depends on reading it |
| `OCCURRED_AT_DESCRIPTION` verbatim | `src/mcp/tools.rs:29-44`, `write_schema_carries_the_fence_verbatim` | kept. It is the write-date fence, and shortening it is the edit constraint C forbids |
| `AS_OF_DESCRIPTION` verbatim | `src/mcp/tools.rs:46-55`, `search_schema_carries_the_as_of_wording_verbatim` | kept, for the same reason |
| review description phrases | `src/mcp/extra_tools.rs:664-686` tests | kept |
| "source is the name of the app that wrote" | `src/mcp/extra_tools.rs:712-718` | kept |
| `ORDERS` denylist, no em dash | `tests/mcp_tool_annotations.rs:42-65` | kept; new doc comments must pass it |
| `search::Hit` key set | `tests/integration.rs:985-1007` | kept: the service type does not change |
| view key-set tests | `src/mcp/views.rs:321-471` | kept for the full shapes; new tests pin the brief shapes |
| structured copy carries no free text | `tests/review_queue_mcp.rs:487` | changed on purpose: every row `content`, `proposed_content`, field `label` and field `value` matches the D5 regex, group 2 equals the source string, every nonce in one answer differs, and `version` equals the source's value byte for byte |
| digest payload carries `text`, `counts`, `sealed_inventory` | `tests/integration.rs:1011-1017` | kept: it reads the `Digest` struct |
| digest sections over MCP | `tests/mcp_source.rs:402-411` | changed on purpose: the call passes `rows: true` |
| `policy-test.sh` leak checks | `scripts/policy-test.sh:360-385` | kept: the brief digest keeps `inventory` and `sealed_inventory`, `bootstrap --json` sends `rows: true` for `counts.by_namespace`, and the script fails when `inventory` is absent |

## 6. What changes on each channel

| Tool | Text | Structured, brief |
|---|---|---|
| `context_bootstrap` | unchanged | `project`, `counts.memories`, `counts.registry`, `inventory`, `sealed_inventory`, `text`, `left_out`, `left_out_registry`, `more` |
| `memory_search` | compact JSON of the brief view | brief view |
| `memory_history` | compact JSON of the brief view | brief view |
| `review_queue` | today's render, fewer items, `more:` line | fenced free text restored, brief rows, `more` |
| `memory_write`, `registry_*`, `alias_*`, `review_decide` | compact JSON | unchanged |
| `memory_forget` | unchanged | unchanged |

## 7. Which side of the fork line

Everything in sections 4 to 6 makes the single-tenant engine better: a solo self-hoster on Claude
Code pays the same 9,000 tokens per session start. It goes upstream first. The fork takes it through
`scripts/merge-upstream.sh` pinned to the feature's merge commit, because the fork must adapt in the
same merge (decision 0151):

- fork `src/cloud/promotion/labels.rs`: `decorate_with` re-renders the text only when it equals the
  pretty JSON (`was_pretty`, `:276`). Compact text fails that check, and the labels would reach only
  the structured copy. The check becomes "the text parses to the same JSON", and the re-render goes
  through `views::compact`.
- fork `src/cloud/ownership/bootstrap.rs`: `TeamDigest` flattens the engine `Digest` (`:50-61`). It
  builds on the brief view instead, `rows` gates `company_memory.facts` and
  `promotions.shared_since_last_session`, and `render_within` (`:332`) takes `left_out` from
  `render_report` on its final render. The superset test (`:494`) compares payloads at the same
  `rows` value.
- fork `src/mcp/extra_tools.rs`: keep `withhold_queue` (`:397`) ahead of `render` and the view.

**One feature per merge.** The fork merges at engine PR 3's merge commit, which holds cuts 1 to 4,
before it takes any other engine feature. PRs 2 and 3 merge in one sitting once the gate passes, so
no other merge lands between them. PR 4 (descriptions) needs no fork adaptation and reaches the fork
through the next release tag, the default path in decision 0151. A freeze of engine `main` lost: PR
4 waits several usage windows for its own gate, and a freeze that long stalls every other engine
feature. Features already on `main` before PR 2 (`feat/write-cap` if it lands first) get their own
fork merge first; the fork plan's F1 lists the range before merging.

Fork-only cuts (promotion queue, decisions, system map, teams tool descriptions, `author_label`) and
the fence on `memory_why`'s structured copy are in the fork plan.

## 8. Expected size

Design targets, not measurements. Section 9's gate measures the real numbers.

| Surface | Today | Target |
|---|---|---|
| `context_bootstrap` structured | 36.2 KB | about 21 to 26 KB, an estimate: the text, the inventories and the rows `left_out` names. G1 measures it per project |
| `memory_search` 8 hits, structured | 13.1 KB | about 12 KB: `score`, `primary` and open `sensitivity` go; content dominates |
| `memory_search` 8 hits, text | 14.1 KB | about 12 KB |
| `review_queue` default, structured | 71.7 KB with no fact text | about 30 KB for 12 items, fact text included |
| `review_queue` default, text | 251.7 KB | about 30 KB for 12 items |
| descriptions and schema docs, 21 tools | about 27,400 chars | about 19,000 chars |

## 9. The behaviour gate

A response-size test shows nothing about constraint C. G0 guards the view: given an answer, does
the brief view drop only what this spec lists. G1 guards the build: do two builds over one store
return the same memories in the same order. G2 runs real agents and settles "same calls, same
decisions". G3 watches production after the deploy.

### G0. Projection tests, in CI

For each tool in D1, on fixture data:

- the brief view's ids, in order, equal the full view's ids;
- every brief field equals the full field;
- the full view equals the service serialization, minus D7's fields, plus D4's id lists;
- `left_out` names exactly the rows `render_report` did not print whole;
- every free-text value in `review_queue`'s structured copy matches the D5 regex, its body equals the
  source string, and no two nonces in one answer match;
- the text block equals `render` on the same `Queue` apart from nonces and the `more:` line.

### G1. Build replay, no model

Two local builds, base and branch, each on its own fresh copy of the corpus, both with
`SEARCH_USAGE_WEIGHT=0`. A search bumps `access_count`, and with the usage weight on, the order in
which two builds' bumps land would move later rankings. A script calls both builds with the same
requests in the same order and diffs the answers.

- 200 `memory_search` queries: 100 in the CLI eval fixture format (`client/eval-fixture.example.jsonl`)
  and 100 written once from the corpus, frozen in the task file. Each runs bare, with `project`, and
  with `namespaces`.
- `context_bootstrap` for every project namespace in the corpus: `text` byte for byte, and per
  project the count and the bytes of the `left_out` and `left_out_registry` entries.
- `memory_history` for 50 ids that have predecessors.
- `review_queue` pages 0 to 3 for each `source` with an explicit `limit`, keys compared. The brief
  default page is compared with the first `max(2, ceil(10 / n))` keys of each source's base page.

Pass: zero differences in id sequences, digest text and queue keys.

**Consumer check, also no model.** For one answer of each tool, the script serializes the branch
build's structured copy as each consumer does: Python `json.dumps(structured)` with default
arguments, as Hermes calls it (`provider.py:399`), and Node `JSON.stringify(structured ?? {})`, as
OpenClaw calls it (`src/tools.ts:64`). It parses each result back and asserts every field section 3
lists for that consumer is present and equal to the base build's value. It asserts the same for the
mod's fields (`similarity` included) and the CLI's fields on a `detail: "full"` call, and runs
`scripts/policy-test.sh` against the branch.

### G2. Agent replay through headless Claude Code

**Sessions.** On legolas, Claude Code 2.1.296 at `~/.local/bin/claude`, on the owner's existing
subscription login (memories `4b59b108`, `03e4cb45`). No Messages API call, no other provider, no
separate config directory, no `setup-token`, no `--bare` (it never reads OAuth). Each session:

```
cd "$(mktemp -d /tmp/lr-gate.XXXXXX)" && env -u ANTHROPIC_API_KEY ~/.local/bin/claude -p "<prompt>" \
  --setting-sources project --strict-mcp-config --mcp-config <slot>/mcp.json \
  --tools "" --allowedTools "mcp__lumberroom__context_bootstrap,mcp__lumberroom__memory_search,..." \
  --no-session-persistence --no-chrome --disable-slash-commands --include-hook-events \
  --output-format stream-json --verbose --model <claude-sonnet-5-5 | claude-opus-5-5>
```

The fresh empty working directory keeps any project `CLAUDE.md` or `AGENTS.md` out of reach.
`mcp.json` names one server at `http://127.0.0.1:<port>/mcp` with the slot's gate token. The runner
rejects a session whose `init` event shows `apiKeySource` other than `"none"`, any MCP server other
than the slot's, any built-in tool, any hook event, or any plugin outside the builtin set
(`cc-plugin-agents-md`, `cc-plugin-telemetry`, `cc-plugin-plugin-authoring`).

**Probe, before batch 1.** One session per channel against an A0 slot.
- It asks the model to list every instruction it was given. `--setting-sources project` may still
  load `~/.claude/CLAUDE.md`. If it does, the gate accepts it and the report says so: every session
  of every arm loads the same file, so it cannot separate the arms. Copying a config directory would
  drag the owner's plugins along, which is the thing the flags keep out.
- It asks the model to quote the text of one seeded stale row read through `review_queue`. On the
  base build that text exists only in the stripped-from-structured text block. A structured-channel
  session that cannot quote it shows Claude Code fed `structuredContent` to the model; a text-channel
  session through the proxy that can quote it shows the proxy works. Both must hold.

**Channels.** *Structured*: the session talks to the arm directly. *Text*: the session talks to the
arm through a proxy that deletes `structuredContent` from each `tools/call` result, so Claude Code
reads `content[].text`, the channel claude.ai and ChatGPT read.

**Slots.** One slot per arm and channel in use. Each slot is one Postgres container and one arm
server on a Docker network created with `--internal`, shared by those two containers alone. The
server publishes `127.0.0.1:<port>` and nothing else. If the installed Docker will not publish a
port from a container on an internal-only network, the lead stops and asks the owner. The slot
environment carries the gate token and the database URL, and no Razorpay, SMTP, LLM, GitHub or
OpenRouter credential.

**Corpus.** The pre-migration production dump on legolas (memory `e0008c0d`):
`~/lumberroom-gate/2026-10-mcp-response-tokens/corpus/lumberroom-20261012T093903Z.{dump,manifest.json,settings.sql}`,
mode 600, sha256 prefix `e218aa0a`. The lead checks the hash before the first restore and deletes
the corpus when the gate ends. legolas holds no encryption key, so private rows stay unreadable.

- *Fork template.* Restore the dump, apply `settings.sql` (pg_dump does not carry
  `ALTER DATABASE ... SET`, memory `85ab771e`), delete every tenant except the owner's `73740ec4`,
  seed the synthetic teams tenant from fixtures (fork plan F6; never Motive, `eefef246`), mint a
  gate-only token per tenant.
- *Engine template.* A fork build over the fork template exports `73740ec4` with
  `lumberroom archive export` and the gate token; an engine build imports it with
  `lumberroom archive import --restore`. Memory `df4547c0` recorded the engine-to-hosted direction
  of that path, not this one, so the conversion gets its own check: export the engine template again
  and diff it against the fork export row by row (id, namespace, content, tags, created_at,
  occurred_at, occurred_until, supersedes). Any difference stops the gate. Then mint the engine gate
  token through `AUTH_TOKENS` in the slot environment.
- *Empty the real queue.* In each template: `keep_both` on every real conflict pair, after running
  the conflict sweep until `conflicts_pending` reads 0; `last_confirmed_at = now()` on every real
  stale row; dismiss every real proposal in the fork.
- *Seed the review fixtures.* `scripts/eval/agent-replay/seed_review.py` imports a fixture archive
  with `archive import --restore`, so the fixtures keep their old `created_at`, into the namespace
  `project:gate-review`, which no real row uses. Each of the 4 review tasks owns 15 to 20 keys:
  conflict pairs and stale rows in the engine, plus dreaming proposals in the fork. The undated
  source stays out of G2; G0 and G1 cover it.
- Each run clones the slot's template database, reapplies `settings.sql`, restarts the server and
  starts the session.

**Arms.** No arm beyond these four.

| Arm | Build and skill | Purpose |
|---|---|---|
| A0 | base build, base `lr-review` text | the reference, on each channel |
| A0b | the same as A0, separate sessions | the noise floor |
| A1 | cuts 1 to 4, branch `lr-review` text | the change |
| A2 | A1 plus the description trim | the description change, against A1 |

A text-channel run is scored against A0's text-channel runs, never against the structured ones.

**Tasks.** 22:

| Group | Count | Example | What it catches |
|---|---|---|---|
| Recall | 4 | "Which port does the staging engine use?" | an answer that changed or got thinner |
| Recall from a left-out row | 2 | a question answered only by a row the digest text did not print whole | D4: `left_out` |
| Supersede a fact the digest shows | 2 | "The backup bucket moved to X; record it." | D4's accepted risk |
| Supersede a fact search finds | 2 | as above, for a fact outside the digest | D3 |
| New write with a stated date | 2 | "We switched to Postgres 17 on 3 October." | the date fence |
| History question | 1 | "What did we believe about X before?" | D8a |
| Forget, dry run first | 1 | "Delete the memory about Y." | `memory_forget` flow |
| Review queue, `lr-review` | 4 | "Work my review queue." on 15 to 20 seeded keys | D5: verdicts, live paging, fences |
| Needs more than the default | 3 | "List every decision about the digest." with 14 matching rows | constraint A |
| Registry lookup | 1 | "Where does the CLI token live?" | control |

Decision tasks are the 11 in the supersede, write, forget and review groups. The text channel runs
14: review, supersede-by-search, history, needs-more and recall.

**Items and outcomes.** Each seeded review key, each supersede target and each expected write is
one item. An item's outcome in a run is one of: the verdict or write the agent made; `unseen`, when
the agent never received the key and nothing touched its rows; `resolved_elsewhere`, when the agent
never decided the key but another verdict retired or rewrote its rows. Only changes made by the gate
token's client count; anything else in the diff is reported and ignored.

**Pass rules.** A0's mode for an item is the outcome most of its runs reach. When outcomes tie, the
mode is the tied set, and a run agrees when it reaches any of them.

1. *Per item, per task group.* Agreement is the share of an arm's runs that reach A0's mode. For each
   task group, A1's agreement averaged over the group's items reaches A0b's minus 5 points. An item
   fails on its own when A0 and A0b each reach the same outcome in at least 4 of 5 runs and A1
   reaches it in at most 2 of 5.
2. *Unseen keys.* A1's `unseen` count per review task stays at or below A0b's.
3. *Calls.* Per task, A1's mean tool calls stay within one call or 20 percent of A0, or within the
   A0 to A0b spread when that is wider. A tool whose share of calls on a task moves by more than the
   A0 to A0b spread plus 20 points goes on a list the owner reads; it fails nothing on its own.
4. *Answers.* The judge rates each A1 final answer against A0's answers for the task. A1's
   equivalence rate reaches A0b's minus 5 points.
5. *More path.* On needs-more tasks, A1 retrieves the required facts at least as often as A0b.
6. *Fence leak.* An A1 or A2 run that writes content containing `----- data ` fails the gate.
7. *Terseness.* A1's mean final-answer length may exceed A0's by the A0 to A0b spread plus 10
   percent, and no more.

The report prints input tokens per task and arm from the stream-json usage lines.

**Models and repeats.** The Sonnet stage (`claude-sonnet-5-5`) runs first: 5 repeats per decision
task, 3 per other task, on every arm and channel. The Opus stage (`claude-opus-5-5`) runs only after
the Sonnet stage passes: the 11 decision tasks on the structured channel, 3 repeats per arm. The
judge is the same `claude -p` command with no `--mcp-config`, handed the task, A0's answers and the
answer under test, and asked for one JSON line. Effort stays at Claude Code's default for every arm.

**Sessions and batches.**

| Stage | Sessions |
|---|---|
| Sonnet, structured: 11 decision x 5 + 11 other x 3 = 88 per arm, A0, A0b, A1 | 264 |
| Sonnet, text: 6 decision x 5 + 8 other x 3 = 54 per arm, three arms | 162 |
| Opus, structured: 11 x 3 per arm, three arms | 99 |
| Judge: 11 structured and 8 text answer tasks, against A0b and A1 | 38 |
| Engine gate for PRs 2 and 3 | **563** |
| A2 stage: Sonnet structured 88, Opus 33, judge 11 | **132** |
| Fork gate, fork plan F6 | **131**, then **36** for the fork's A2 |

Plus two probe sessions per build. A design estimate puts 60 sessions in a 5-hour window at three
concurrent sessions; batch 1 runs 40 and the lead sizes later batches from what it used. On that
estimate the engine gate takes about 10 windows, the A2 stage 3, the fork 3 and 1. Each batch
interleaves the arms over the same tasks, so a model or quota change between windows lands on every
arm alike.

**Failed sessions.** A session that ends on a usage limit, a refusal, an MCP error, a rejected
`init` event or a timeout is recorded as incomplete with its reason, kept out of every score, and
rerun in a later batch. A task whose sessions stay incomplete after three reruns is reported by name
and never counts as agreement. The runner skips any run whose record exists, so a batch resumes
where the window ended.

**Records.** Run records, transcripts and reports live in
`~/lumberroom-gate/2026-10-mcp-response-tokens/runs/` on legolas, outside `~/.cache/agent-scratch`,
so no batch-end cleanup deletes them. The owner removes them after reading the reports.

**What G2 shows.** Whether Claude Code on the same model, given the same tasks over the owner's own
memory, makes the same decisions and calls on the new answers, within the noise A0 already carries,
on both channels.

**What G2 cannot show.**

- claude.ai's, ChatGPT's, Hermes's or OpenClaw's own system prompts. The text channel feeds Claude
  Code the payload those clients read. G1's consumer check covers the fields Hermes and OpenClaw parse.
- Drift rarer than its sample. With 5 repeats per decision task, a change that alters one decision
  in twenty runs can pass.
- Private rows, undated-source paging, long sessions, other task mixes.

### G3. Production watch, after the deploy

Read-only `/statsz` per tool for 7 days before and 7 days after: calls per session for
`memory_search` and `memory_history`, the `unprompted` rate, the failure rate. A rise above 25 percent
in `memory_search` calls per session, or any rise in `memory_history` calls, sends the owner to the
G2 report for the task group that would explain it. It is a trip-wire with no causal claim.

## 10. Description trim

Last, behind G2's A2 arm, in its own PR.

**Stays, because it steers behaviour.** Every pinned phrase in section 5. `memory_write`'s
`possible_conflicts` and `supersedes` sentences (the comment at `src/mcp/mod.rs:297-300` says that
sentence is what makes supersession happen at all). The `memory_forget` irreversibility sentence and
`dry_run`. The review descriptions' authority and fence lines. Each "when absent" default.

**Goes, because it repeats or describes the server.** Implementation detail a model cannot act on
(`memory_history`: "The walk is one indexed pass bounded by a depth cap"); the second statement of
a default the argument doc already gives; examples that restate the sentence before them
(`alias_set`'s rename story keeps one name, not three); overlap between `SearchArgs.namespaces` and
`SearchArgs.project`, which explain the same default set twice.

Each removed sentence is listed in the PR description with a one-line reason. Target: descriptions
and schema docs across the 12 engine tools down about 30 percent, a design target.

## 11. Release

The release that ships PR 3 refreshes `tools-snapshot.json` in lumberroom-claude-code,
lumberroom-hermes and lumberroom-openclaw, and runs `scripts/sync-skills.sh` for the `lr-review`
change. Those repositories are outside this plan's file ownership; the release notes name them.

## 12. Not in scope

- The owner-chosen `source` labels (decision 0023), which run to about 60 characters a hit.
- `memory_forget`'s structured copy repeating its `text`.
- `registry_get`'s `provenance` block.
- The fork's teams labels on personal rows (`audience: "yours"`, `author: "you"`).
- Server instructions text.
