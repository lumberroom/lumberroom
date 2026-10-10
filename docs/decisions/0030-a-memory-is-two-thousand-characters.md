# 30. A memory is 2,000 characters

**Date:** 10 October 2026 · **Status:** accepted, implemented · **Decided by:** the owner

## Decision

`WRITE_MAX_CONTENT_CHARS` defaults to 2000, down from 8000. The setting stays; only the default
moves.

- **The shared refusal carries the numbers.** Every write path that refuses on length says
  `content is N chars and the limit is M.` and nothing more. The console, an ingest approval, a
  review merge and an archive merge read it, and none of them can split a row into tool calls.
- **`memory_write` adds the fix.** The MCP layer checks the same count first and appends advice to
  split into separate `memory_write` calls, one durable fact each, to leave out session narrative,
  reasoning and step-by-step history, and to set `supersedes` when correcting a longer row.
- **`tools/list` states the limit.** `memory_write`'s description ends with `Content over M
  characters is refused.`, where M is the configured value, and gains two sentences on one fact per
  call. A model that reads the number writes under it.
- **Ingest refuses at post.** A fact over the limit gets `Refused { rule: "content_too_long" }` with
  the shared message in `detail`, and never becomes a proposal.
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

On the hosted fork's production store, read on 10 October 2026, the median live row ran 570
characters, 751 of 4,941 rows ran past 1,000 and 55 past 2,000. Rows over 1,000 characters had a
same-namespace neighbour at cosine 0.90 or above more than twice as often as shorter rows. Agents
were storing session narrative as memories, and the long rows were the ones that repeated each
other. Those figures come from the fork's PR #135; this repository has no production store of its
own to measure.

The fork set 2000 in its own deploy file. The owner ruled the same day that the engine takes the
same default, because agents write bloated memories on any deployment, not only the hosted one.

The old refusal said `Write the durable fact, not the transcript.` It named no way to split the
content and nothing to cut. The fork's first version put split advice into the shared message, and
its review found the problem: the console, ingest approvals and review merges read it too, and told
a person approving a queue row to make `memory_write` calls.

## What lost

**Loosening dedupe instead.** Folding near-duplicates at a lower cosine would shrink the repeats.
The fork measured it at 0.97 on EmbeddingGemma and found it would fold 162 corrections into the
rows they correct. Collapsing a correction into its predecessor destroys data with nothing in the
store saying so, which is the bias `services::write` states at its top.

**Advice in the shared message.** One string is simpler. It lost because four of the five readers
cannot act on it.

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
- The MCP layer counts the content once and the write path counts it again. The two read the same
  setting through the same function, `write::length_refusal`, so they cannot disagree.
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
