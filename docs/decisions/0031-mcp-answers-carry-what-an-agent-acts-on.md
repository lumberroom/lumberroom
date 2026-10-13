# 31. MCP answers carry what an agent acts on

**Date:** 12 October 2026, revised 13 October 2026 · **Status:** decided by the owner;
implemented. The server suite, the CLI crate's tests and both comparisons below ran; the policy
test passed 8 of 8 against a build of an earlier commit of this change · **Decided by:** the owner

## Decision

Four tools answer a model with a brief view by default and keep today's shape behind an opt-in
argument. The service types do not change; `src/mcp/views.rs` and `src/mcp/review_view.rs` map
each answer the way decision 0020 mapped `MemoryView`, so the console, the HTTP routes and the
archive keep reading what they read.

- **`context_bootstrap`** keeps its text byte for byte. Its brief structured copy holds `project`,
  `counts.memories`, `counts.registry`, `inventory`, `sealed_inventory`, `text`, and every row the
  text left out or shortened. `left_out` carries each such memory with its id, namespace, exact
  stored content, writer and stored timestamp, plus `sensitivity` above `open` and `tags` when
  non-empty. A printed bullet shows its date and writer, so a left-out row keeps both.
  `left_out_registry` carries each such registry entry. `bootstrap::render_report` names those
  rows from the same layout `render` draws. When the digest held any memory rows, a `more` hint
  points to `rows: true` and `memory_search`. `rows: true` returns the old `Digest` serialization
  plus both lists as ids, so `lumberroom bootstrap --json` gains `left_out` and
  `left_out_registry`.
- **`memory_search`** drops `score` and `primary` from each hit, `sensitivity` when it reads
  `open`, and `tags` when empty. Ranking, hit count and the limits (default 8, at most 50) stay.
  `detail: "full"` returns the old `SearchResult`.
- **`memory_history`** drops `embedding_model`, `access_count` and `last_accessed_at`, the chain
  fields and `last_confirmed_at` when null, `sensitivity` when `open`, and `tags` when empty.
  `detail: "full"` returns them.
- **`review_queue`**, called with no `limit`, keeps the service's default of 50 items per source,
  the same as HTTP. An explicit `limit` keeps its per-source meaning. The default page stays long
  because the agent replay showed a search during review clears staleness: a `memory_search` hit
  marks a row read, so a stale row an agent touched while researching page one left the queue
  before page two, and a short page hid stale items the agent never saw. When `has_more` is true,
  both channels carry a `more` hint that names no offset, because a verdict removes its item from
  the live queue and any number would be wrong after the first one.
  Each row loses `access_count` and `last_accessed_at`, `sensitivity` at `open`, and `opened` when
  true. The tool description tells an agent in a review to page until `more` is absent before it
  reports the queue done, and the hint opens by saying more items wait. The first agent replay
  added that sentence: with a short page and a bare hint, agents worked the first page and
  stopped, and the review tasks fell below the replay's noise floor. The sentence stays now that
  the page is long, because a store can hold more than 50 items in a source.

Fact text returns to `review_queue`'s structured copy inside the data fence `render` draws: each
row's `content`, `proposed_content`, and each proposal field's `label` and `value`, each with its
own fresh nonce. `key`, `origin`, `kind`, `version` and `held_by` stay raw, because `review_decide`
takes them back byte for byte. This supersedes the clause of decision 0019 that stripped free text
from the structured copy.

A write refuses content holding a whole line that matches
`^----- (end of )?data [0-9a-f]{12} -----$`, under the code `content_carries_fence`. The check sits
in `domain::fence` and runs in `write::run_inner`, so `memory_write`, a review merge and an ingest
approval all reach it. An archive merge goes through `run_restored` with the check off.

Every tool whose text was pretty-printed JSON now sends the same value compact, through
`views::compact`. `Digest.tenant` leaves both shapes of `context_bootstrap`.

A call that declares itself the CLI (`x-memory-invocation: cli`) gets the full shape when it names
no `detail` or `rows`. The CLI in this release asks for `detail: "full"` and, for
`bootstrap --json`, `rows: true` itself, and reads `score` as optional.

Every timestamp keeps the precision the service serialized, microseconds included. The owner
approved keeping that precision on 12 October 2026.

## Context

Each tool answers on two channels: a text block and `structuredContent`. Claude Code, Hermes and
OpenClaw put `structuredContent` in front of the model, so every field there costs tokens on every
call. Read on a working store with a read-only token, the shapes before this change looked like
this:

- `context_bootstrap`'s structured copy repeated the rendered digest text and then carried every
  row behind it in full, many times the size of the text a model needed.
- `memory_search`'s text block was the structured JSON again, pretty-printed, and each hit carried
  a fused score, a primary flag and a sensitivity of `open` that agents quoted back as if they meant
  something.
- `review_queue` read 50 items from each source by default, and its text block fenced the full text
  of every row on the page, larger than its structured copy.

The review queue had the opposite problem. Decision 0019 kept fact text out of the structured copy
so untrusted text reached the agent only inside a fence. Claude Code reads only the structured copy,
so an agent there saw keys and verdicts with no facts, and could not work the queue. The text block
that did carry the facts was the largest answer any tool gave.

The owner ruled on 12 October 2026 to shrink these answers and to shorten the tool descriptions.
This record covers the answers. Shorter tool descriptions ship in a later PR with their own
measurement.

## What lost, and why

**A `fields: [...]` argument** lost to two named opt-ins. It asks a model to know field names it
has never seen, and every name it accepts becomes a pinned contract. `detail` is one enum shared by
two tools, so a model learns one word.

**A total cap across review sources** lost to a per-source split. Paging a concatenation of
per-source pages needs per-source counts the service does not have, and the service clamps a
source's page at 200, so an offset past 200 could not be served in one read.

**A short default review page** lost. An MCP call with no `limit` first read about 10 items in
all, split across the sources. The agent replay showed that breaks reviews: the stale source lists
rows nobody has read, a `memory_search` hit marks a row read, and so a stale row touched while
researching page one dropped out of the queue before page two. Agents on the short page left stale
items unseen. The default went back to the service's 50 per source, and the `more` hint and the
paging sentence stay for the store that holds more.

**A hint naming the next offset** lost. Pages read the live queue, and a verdict removes its item,
so `offset: 10` points somewhere else once the agent decides anything. The hint states the rule and
leaves the arithmetic to the agent.

**A line per hit in the search text** lost to compact JSON. It would change what claude.ai and
ChatGPT agents read far more than whitespace does, and compact JSON parses to the same value as the
pretty form.

**Omitting `structuredContent`** lost. OpenClaw serializes the structured copy
for its model and would hand it `{}`. Both channels stay, each carrying the full answer for its
readers.

**Truncating timestamps** lost to full precision. `as_of` keeps a row when its start is at or
before the instant, so an agent that copies `17:35:39Z` for a row stored at `17:35:39.860834` loses
that row from an answer it gets today. A merge stores the `occurred_at` the caller sends, and a
truncated copy stores an earlier instant than the source row holds. The saving was about 13
characters a timestamp, near 1 percent of a search answer.

**A nonzero `tools/list` TTL** lost. It changes when a client fetches the list, not what the
client puts in front of the model, so it saves no model tokens, and a grant change would show late
by the TTL.

**Keeping 0019's stripping** lost to the fence moving with the text. A value that carries its own
markers keeps the property 0019 wanted on every channel a model reads, and the write-side refusal
stops a pasted marker from coming back into the store.

## Costs accepted

- **Digest ids sit behind `rows`.** The text bullets carry no memory id. A Claude Code agent that
  superseded a printed digest fact by the id in `structuredContent` now needs `rows: true`, a
  `memory_search`, or the `possible_conflicts` list `memory_write` already returns. No agent replay
  session called `context_bootstrap`, so no agent evidence covers the brief digest shape or its
  `left_out` list. The digest text is byte-identical to the base build's.
- **The CLI invocation default.** A header grants a shape. It grants no access a caller could not
  reach with `detail: "full"`, but it is one more branch in each brief handler until it goes.
- **`left_out` takes part of the digest saving.** A digest whose text overflows carries every row
  it left out, whole. On a store whose digest overflows, the brief payload keeps a large share of
  the old size, an estimate and not a measurement; on a store whose digest fits, the brief payload
  is the text plus a few hundred bytes of counts and inventory.
- **A stored fence line is refused**, including in a fact that quotes one on a line of its own for a
  real reason. The caller resends the text without the marker lines.
- **`--json` output changes.** A script that read `tenant` from `lumberroom bootstrap --json` loses
  it. `lumberroom search` and `lumberroom bootstrap --json` ask for the full shape and see no
  other change.

Two comparisons ran before the merge. The base and branch builds answered the same searches,
digests, history reads and review pages over one copy of a real store: search ids and order, digest
text, history ids and review keys came back identical, and the brief payloads came back smaller for
search, digest and history. An agent replay then ran the same tasks through headless Claude Code
against base, a second base run for noise, and the branch, and compared calls, end state and
answers. It found the short default review page this record first proposed, which the review
bullet above explains, and passed with the default page restored and the 0032 declaration on every
arm. After the replay, the `review_queue` description gained the advice to narrow `source`
before paging; no replay ran on that wording.

The replay passed with decision 0032 in place. Claude Code saves an MCP answer over 50,000
characters to a file, and under that behaviour base answers to a limit-50 search went to a file
while the smaller brief answers did not, so the arms searched differently for a reason outside this
change. The comparison that passed ran every arm with the declaration 0032 adds.

Two residual differences remained. On the text channel, one supersede-by-search task agreed with
base one run under the noise floor in one stage, and a rerun on fresh runs did not reproduce the gap
in skipped registry writes behind it. On one needs-more task the brief build made fewer calls than
base and still carried every required fact. The owner reviewed both on 13 October 2026 and accepted
them.

## Not for

It does not change what any search returns or in what order, the digest text, or any service type
the console, HTTP routes or archive read. It does not shorten tool descriptions; a later PR does
that. It does not touch `memory_forget`'s structured copy, `registry_get`'s `provenance`, the server
instructions, or the `source` labels of decision 0023.

## Reversal

- **The CLI default.** Remove it once a CLI that sends `detail` itself has shipped in two
  tagged releases.
- **Digest ids.** If an agent replay that calls `context_bootstrap` shows agents failing to
  supersede a printed digest fact, the brief digest adds `printed: [{id, namespace}]` for each
  printed row: ids without the content the text already shows.
- **The `tools/list` TTL.** Revisit a nonzero TTL with private scope when a client is shown to
  refetch the list on every call.
- **The whole change.** This was the merge gate. The change does not merge if the agent replay shows
  agents making different decisions or calls on the brief shapes beyond the noise two runs of the
  base build show against each other, unless the owner has reviewed those differences and accepted
  them. The owner reviewed the two residuals above on 13 October 2026 and accepted them.
