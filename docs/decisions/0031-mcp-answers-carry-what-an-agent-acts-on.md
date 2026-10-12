# 31. MCP answers carry what an agent acts on

**Date:** 12 October 2026 · **Status:** decided by the owner; implemented on
`feat/mcp-brief-answers`, full suite and gates G1 and G2 not run, not merged · **Decided by:** the
owner

Spec: [`docs/superpowers/specs/2026-10-12-mcp-response-tokens.md`](../superpowers/specs/2026-10-12-mcp-response-tokens.md).
Section references below (D1 to D11, G0 to G3) point into it.

## Decision

Four tools answer a model with a brief view by default and keep today's shape behind an opt-in
argument. The service types do not change; `src/mcp/views.rs` and `src/mcp/review_view.rs` map
each answer the way decision 0020 mapped `MemoryView`, so the console, the HTTP routes and the
archive keep reading what they read.

- **`context_bootstrap`** keeps its text byte for byte. Its brief structured copy holds `project`,
  `counts.memories`, `counts.registry`, `inventory`, `sealed_inventory`, `text`, and every row the
  text left out or shortened: `left_out` carries each such memory with its id, namespace and exact
  stored content, and `left_out_registry` each such registry entry. `bootstrap::render_report`
  names those rows from the same layout `render` draws. `rows: true` returns the old `Digest`
  serialization plus both lists as ids.
- **`memory_search`** drops `score` and `primary` from each hit, `sensitivity` when it reads
  `open`, and `tags` when empty. Ranking, hit count and the limits (default 8, at most 50) stay.
  `detail: "full"` returns the old `SearchResult`.
- **`memory_history`** drops `embedding_model`, `access_count` and `last_accessed_at`, and the
  chain fields when they are null. `detail: "full"` returns them.
- **`review_queue`**, called with no `limit`, reads `max(2, ceil(10 / n))` items from each of the
  `n` sources it reads: 5 each from conflict and stale on a default call, 10 from a single named
  source. An explicit `limit` keeps its per-source meaning, and HTTP keeps the service's default of
  50. When `has_more` is true, both channels carry a `more` hint that names no offset, because a
  verdict removes its item from the live queue and any number would be wrong after the first one.
  Each row loses `access_count` and `last_accessed_at`, `sensitivity` at `open`, and `opened` when
  true.

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
approved that departure from his listed cut on 12 October 2026.

## Context

The owner measured production on 12 October 2026 with a read-only token, at about 4 bytes a token:

| Surface | Structured | Text |
|---|---|---|
| `tools/list`, 7 tools | 16.8 KB | |
| `context_bootstrap` | 36.2 KB | 6.0 KB |
| `memory_search`, 8 hits | 13.1 KB | 14.1 KB |
| `review_queue`, default, 96 items | 71.7 KB | 251.7 KB |

Claude Code, Hermes and OpenClaw put `structuredContent` in front of the model, so every field
there costs tokens on every call. A self-hoster on Claude Code paid about 9,000 tokens at each
session start for a digest whose text was 6 KB. The full rows sat beside the text, a second copy of
most of it. The search hits carried a fused score agents quoted back as if it meant something.

The review queue had the opposite problem. Decision 0019 kept fact text out of the structured copy
so untrusted text reached the agent only inside a fence. Claude Code reads only the structured copy,
so an agent there saw keys and verdicts with no facts, and could not work the queue. The text block
that did carry the facts ran to 251.7 KB for one default page.

The owner ruled on 12 October 2026 to make all five cuts. This record covers cuts 1 to 4, which
ship together on `feat/mcp-brief-answers`. Cut 5, shorter tool descriptions, ships in a later PR
behind its own gate arm.

## What lost, and why

**A `fields: [...]` argument** lost to two named opt-ins. It asks a model to know field names it
has never seen, and every name it accepts becomes a pinned contract. `detail` is one enum shared by
two tools, so a model learns one word.

**A total cap across review sources** lost to a per-source split. Paging a concatenation of
per-source pages needs per-source counts the service does not have, and the service clamps a
source's page at 200, so an offset past 200 could not be served in one read.

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
  `memory_search`, or the `possible_conflicts` list `memory_write` already returns. G2's group
  "Supersede a fact the digest shows" measures it.
- **The CLI invocation default.** A header grants a shape. It grants no access a caller could not
  reach with `detail: "full"`, but it is one more branch in every handler of D1 until it goes.
- **`left_out` takes part of the digest saving.** A digest whose text overflows carries every row
  it left out, whole. The spec's estimate is about 21 to 26 KB against 36.2 KB, an estimate and not
  a measurement; on a store whose digest fits, the brief payload is the text plus about 150 bytes.
- **A stored fence line is refused**, including in a fact that quotes one on a line of its own for a
  real reason. The caller resends the text without the marker lines.
- **`--json` output changes.** A script that read `tenant` from `lumberroom bootstrap --json` loses
  it. `lumberroom search` and `lumberroom bootstrap --json` ask for the full shape and see no
  other change.

None of the saving is measured. The spec's design targets: `memory_search` at about 12 KB a channel
for 8 hits, and `review_queue` at about 30 KB a channel for a 12-item page with fact text included.
G1 (two builds over one store, diffed) and G2 (headless Claude Code against base and branch builds)
have not run.

## Not for

It does not change what any search returns or in what order, the digest text, or any service type
the console, HTTP routes or archive read. It does not shorten tool descriptions; cut 5 does that in
its own PR. It does not touch `memory_forget`'s structured copy, `registry_get`'s `provenance`, the
server instructions, or the `source` labels of decision 0023.

## Reversal

- **The CLI default (D2).** Remove it once a CLI that sends `detail` itself has shipped in two
  tagged releases.
- **Digest ids (D4's F1).** If G2's "Supersede a fact the digest shows" group fails, the brief
  digest adds `printed: [{id, namespace}]` for each printed row: ids without the content the text
  already shows.
- **The `tools/list` TTL (D10).** Revisit a nonzero TTL with private scope when a client is shown to
  refetch the list on every call.
- **The whole change.** If G2 shows agents making different decisions or calls on the brief shapes
  beyond the noise A0 and A0b carry, PRs 2 and 3 do not merge.
