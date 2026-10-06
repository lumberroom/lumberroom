# 26. The date review joins the review queue as an opt-in source

**Date:** 6 October 2026 · **Status:** accepted by the owner, implemented on branch `feat/undated-review-source`, not merged · **Decided by:** the owner, 6 October 2026

What was run, and where: on 6 October 2026, on branch `feat/undated-review-source`.

- `./scripts/cargo.sh test -j 1 --test review_queue`: 45 passed, 0 failed, four of them new.
- `./scripts/cargo.sh test -j 1 --test review_queue_mcp`: 16 passed, 0 failed, one of them new.
- `./scripts/cargo.sh test -j 1 --test mcp_tool_annotations`: 11 passed.
- `./scripts/cargo.sh test -j 1 --lib` filtered to `review_queue`, `extra_tools`,
  `services::review`, `http::review` and `dates`: 63 passed, 0 failed.
- The full suite has not run on this branch.

## Decision

`review_queue` gains a fourth source, `undated`, and `review_decide` a verdict, `fill_date`.

- An `undated:<id>` item is a live row with no `occurred_at` whose own text names one or more past
  days. Its `dates` field lists every such day in the order the text names them, and it prints
  outside the data fence because the engine read it out of the text.
- `fill_date` takes `occurred_at`, a bare day or an RFC 3339 instant, and calls
  `review::fill_date`. Every refusal that function already made still holds: the day has to be one
  the text names, a start already there is never moved, and nothing in the future.
- The source answers only when a caller names it. Omitting `source` still returns conflicts, stale
  rows and proposals, and nothing else.
- The queue and the admin route `GET /admin/review/dates` read one scan, `review::dated_in_text`,
  so they cannot list different rows. The offset counts candidates, not scanned rows.

`fill_date` now opens a private row before checking its text. Before this, a private row read back
with empty content, named no day, and every fill on one was refused.

## The context that forced it

Filling dates ran only through the CLI (`lumberroom review --dates`, then `lumberroom fill-date`) and
the admin route behind it. An agent working the queue over MCP could see a row whose text said
"on 4 March 2026" and had no way to record that day, so a review pass had to stop and hand the
dates to someone at a terminal. The owner asked for the pass over MCP so the `lr-review` skill can
run it.

## What lost, and why

- **A separate MCP tool, `memory_fill_date`.** It would have been one more tool in every client's
  list for a pass that runs during review and nowhere else. The queue already carries keys, verdict
  lists and the decide path's grant checks.
- **Listing `undated` by default.** The CLI's wire type for `source` is a closed enum with no
  catch-all, and a client built before this source would fail to parse a default page carrying it.
  Asking by name costs a caller one word.
- **Undated rows that name no day.** Most of a store is timeless preference and rule, and a queue of
  every one of those would be the store, not a review.

## Costs accepted

- The scan reads up to 20 rows per answer and stops at 20,000 rows. A store with more undated rows
  than that can hold candidates no page reaches, and `has_more` reports only what the scan saw.
- A row the reviewer chose to leave undated stays on the first page. Paging past it takes `offset`.

## What this is not for

It does not guess a day the text does not state, and it never moves a date already recorded.
Changing a wrong start date still goes through a supersession.

## Reversal condition

If clients outgrow the closed source enum, `undated` can join the default queue in a later release
that says so in its notes.
