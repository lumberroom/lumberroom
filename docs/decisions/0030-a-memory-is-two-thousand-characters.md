# 30. A memory is 2,000 characters

**Date:** 10 October 2026 · **Status:** accepted, implemented · **Decided by:** the owner

## Decision

`WRITE_MAX_CONTENT_CHARS` defaults to 2000, down from 8000. The setting stays; only the default
moves.

- **The shared refusal carries the numbers.** Every write path that refuses on length says
  `content is N chars and the limit is M.` and nothing more, under the code `content_too_long`.
  The console, an ingest approval, a review merge, an archive merge and the ingest post read it,
  and none of them can split a row into tool calls.
- **Length is checked after the refusals that say something worse.** The write path runs the
  namespace rules, the credentials-namespace refusal, the classification ceiling, the grant and
  the credential tripwire first, and the length cap after them, ahead of the near-now fence. A
  3072-bit PEM key runs past 2000 characters, and its body without the header line passes the
  tripwire's base64 exclusion: refused on length, a caller told to split stores the key at open
  piece by piece. The ingest post keeps the same order and checks length after the echo lookup too.
- **`memory_write` adds the fix.** The tool appends advice to the service's refusal when, and only
  when, the error carries `content_too_long`: split into separate `memory_write` calls, one durable
  fact each, leave out session narrative, reasoning and step-by-step history, and set `supersedes`
  when correcting a longer row. The tool runs no length check of its own.
- **`tools/list` states the limit.** `memory_write`'s description ends with `Content over M
  characters is refused.`, where M is the configured value, and gains two sentences on one fact per
  call. A model that reads the number writes under it.
- **Ingest refuses at post.** A fact over the limit gets `Refused { rule: "content_too_long" }` with
  the shared message in `detail`, and never becomes a proposal. The check runs after the tripwire,
  so a long credential reports its tripwire rule, and after the echo lookup, so a transcript quoting
  a long row the store handed out still confirms it.
- **An archive merge answers to a carried cap**, the larger of the setting and 8000, through
  `write::run_restored`. The floor is its own constant, `CARRIED_MAX_CHARS`, so it cannot follow the
  default down.

Each path that stores content, and the cap it answers to:

| Path | Entry | Cap | Why |
|---|---|---|---|
| `memory_write` | `write::run` | configured | A model composes the fact now |
| Console write and replace | `write::run` | configured | A person composes the fact now |
| `review_decide` merge, CLI `review` merge | `write::run` | configured | The caller composes the merged text now |
| Ingest approval | `write::run_observed` | configured | The extractor composed the fact; the post refuses the same number first |
| Ingest post | `write::length_refusal` | configured | Keeps out of the queue what the approval would refuse |
| Archive merge | `write::run_restored` | carried | The row was valid in the store that exported it |
| Archive restore | `restore_row` | none | A copy of a store into an empty one; unchanged |

## Context

A length study on a production store, aggregates only, ran on 10 October 2026. The median live
row ran 570 characters, 15.2% of rows ran past 1,000 and 1.1% past 2,000. Rows over 1,000
characters had a same-namespace neighbour at cosine 0.90 or above more than twice as often as
shorter rows. Agents were storing session narrative as memories, and the long rows were the ones
that repeated each other.

The fork set 2000 in its own deploy file. The owner ruled the same day that the engine takes the
same default, because agents write bloated memories on any deployment, not only the hosted one.

The old refusal said `Write the durable fact, not the transcript.` It named no way to split the
content and nothing to cut. The fork's first version put split advice into the shared message, and
its review found the problem: the console, ingest approvals and review merges read it too, and told
a person approving a queue row to make `memory_write` calls.

## What lost

**Loosening dedupe instead.** Folding near-duplicates at a lower cosine would shrink the repeats.
On the same production store, a threshold of 0.97 on EmbeddingGemma would fold 162 corrections
into the rows they correct. Collapsing a correction into its predecessor destroys data with nothing in the
store saying so, which is the bias `services::write` states at its top.

**Advice in the shared message.** One string is simpler. It lost because five of its six readers
cannot act on it: the console, an ingest approval, the ingest post, a review merge and an archive
merge. Only `memory_write` can.

**A length check in the MCP layer ahead of the service.** It let the tool add advice without an
error code. It lost because it ran before the tripwire and the grant: a long private key written at
open got split advice instead of the tripwire refusal, and a long write to `credentials:*` was never sent to
`lumberroom seal`.

**No cap on archive merges.** An exemption would let an archive written with a 200000 setting load
200000-character rows into a store that refuses them from every other path. The carried floor
keeps every row any default ever accepted and nothing past it.

**Truncating instead of refusing.** A truncated fact is a different fact, stored without the caller
knowing. Refusal tells the caller and costs one retry.

## What it costs, accepted

- Self-hosters see writes refused that passed before: anything between 2,001 and 8,000 characters.
  An agent following the refusal splits it. An operator who wants the old limit sets
  `WRITE_MAX_CONTENT_CHARS=8000`. Compose did not forward the variable before this change, so a
  compose install could not set it at all; it does now.
- An ingest queue row posted before the change and longer than 2000 stays in the queue and is
  refused at approval with the numbers. The owner rejects it or raises the setting.
- A review merge of two long rows can exceed 2000. The merge is refused and the caller writes a
  shorter merged text.
- A long write that also fails the grant or the tripwire hears only that refusal. Fixed, it can
  come back and meet the length cap on the next try: two round trips where one message could have
  named both.
- A restatement of a long fact whose proposal is already `written` reports `content_too_long` at
  post, not reinforcement. The upsert is the only lookup that finds that proposal, and it inserts
  when it finds nothing. The row stays stored; the source row and the reinforcement count are lost.
- `lumberroom import --submit --delete-source` keeps the dump when any line was refused under a
  rule other than a tripwire rule, `content_too_long` and any rule the CLI does not know among
  them, and says so. Only tripwire refusals let the file go, because keeping it keeps the
  credential on disk.
- Restating a stored row longer than the cap through `memory_write` is refused with split advice.
  The length check runs before the exact-match lookup that would confirm the row. The ingest post
  checks length after its echo lookup and confirms the same restatement.
- The transcript ingest CLI drops a fact refused for length at post. The watermark moves past it,
  so the fact comes back only when the owner re-extracts that part of the transcript.
- An archive merge dry run does not predict a length refusal. It never did; the carried floor
  keeps that gap where it was, at 8000.
- `lumberroom ingest` prints the new rule with its count and drops `detail`, because the CLI's wire
  type has no field for it yet.

## What this is not for

It does not shorten, flag or rewrite rows already stored. Rows longer than 2000 stay readable and
searchable. A client that rewrites one in full through `memory_write` is refused and told to split.

It does not apply to registry values, sealed items or the digest.

It is not a quality rule. A 1,900-character row of narrative passes. The cap bounds the worst case;
the tool description and the agent's own rules carry the style.

## Reversal condition

Raise the default if a measured store shows agents splitting facts that belong together, so that
search returns half a fact where one row would have answered. The evidence would be refusals
followed by writes whose pieces are retrieved apart, read from `tool_calls` and the rows they
produced. Lower it further only on the same kind of measurement that set it.
