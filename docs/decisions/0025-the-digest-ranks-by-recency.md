# 25. The digest ranks by recency, and embeddings only drop near-duplicates

**Date:** 6 October 2026 · **Status:** decided by the owner, implemented on branch `feat/digest-ranking`, not merged · **Decided by:** the owner, 6 October 2026, after an offline evaluation (issue #109)

## Decision

**`context_bootstrap` stays recency-led.** Profile holds the 10 newest live rows from `user:me` and
`global`, within the caller's grants. Project holds the 5 newest from the active project. Recent
keeps its 14-day window and its limit of 8. The registry section does not change.

**Profile drops the tag-first rule.** A row tagged `profile`, `preference` or `identity` used to sort
above every newer untagged row. Now nothing sorts a row above a newer one.

**Embeddings do one job: of two near-duplicates, the newer row stays.** Every section reads a pool
of candidates. Across the union of the pools, newest first with ties broken by id, a row whose
cosine to a surviving newer row is at or above `BOOTSTRAP_DEDUP_COSINE` (default 0.90) is dropped.
Each section then fills from the survivors in its own order and up to its own limit, so the next
candidate takes the dropped row's slot. The newer row wins whichever section either would print in.
A cosine of 0.90 also catches corrections, "port 5432" beside "port 5433", and keeping the older
row would print the stale fact and hide the fix. A row with no vector, or a zero vector, is never
a duplicate. Setting the threshold to 1.0 turns the pass off.

**Recent skips every row profile or project chose.** It used to repeat them in the structured
payload, and the renderer dropped the repeats from the text, so recent could print fewer rows than
its limit.

How it is built:

- `DIGEST_SQL` stays one statement with its seven filtered subqueries. The three memory arms are
  CTE pools, newest first: profile and project read three times their limit, and recent reads
  three times its own plus room for every row the two sections above it could take.
- The pools carry no vectors. A final part of the statement compares the stored vectors of pooled
  rows with pgvector's `<=>` and returns only the id pairs at or below a cosine distance of
  `1 - threshold`, compared in float8. The vector expression lives in one macro,
  `digest_vector!()`, so a store that keeps its embedding elsewhere swaps one line.
- The grant and the ceiling stay inside every pool arm, and the pairs come only from pooled rows. A
  row a client may not read never reaches the process and never drops a readable twin.
- `domain::digest_dedup::Selection` decides which member of each pair survives and fills the
  sections, with no I/O and no vectors.
- Defaults: `BOOTSTRAP_PROFILE_LIMIT` 12 to 10, `BOOTSTRAP_PROJECT_LIMIT` 10 to 5, and the new
  `BOOTSTRAP_DEDUP_COSINE` 0.90, validated at boot to `0 < x <= 1`.
- The render shares from #120 (profile 40, project 25, recent 20, registry 15) stay. Profile keeps
  40% for 10 rows where it had 12, and project 25% for 5 where it had 10, so neither section has
  less room per row than before.

## The context that forced it

Issue #109 asked whether embeddings should choose the digest. An offline evaluation replayed
held-out `context_bootstrap` calls against what the sessions did next: which rows `memory_search`
returned in the following hours, and which rows a session superseded. Every ranker filled the same
budgets with whole entries.

Plain recency beat every ranker that used embeddings alone on each exact label with enough events
to tell rankers apart, and the paired intervals excluded zero. A blend of recency, prior search use and embedding similarity, fitted on a
training split, gave the other two terms no weight. Recency also beat the tag-first rule, and most
of its lead came from profile. The rows recency found and the embedding rankers missed were mostly
less than a day old.

Near-duplicates are where embeddings earned a place. A similarity ranker with a diversity pass at
cosine 0.90 put far fewer near-duplicate rows in a digest than the same ranker without it, and lost
no recall. Under recency alone near-duplicates were rarer, so the pass changes a minority of
digests and guards the store that does fill up with restatements.

## What lost, and why

**Ranking by embeddings alone.** The evaluation tried several variants: cosine to the project's
centroid, the same weighted by age, cosine to the last few project rows, that plus maximal marginal
relevance, and cosine to the project slug as a query. Each lost to recency. They pick rows that stay
relevant for weeks and miss the row written an hour before the session that is about to revise it.

**A weighted score of recency, use and similarity.** The design proposed before the evaluation
blended the three with tuned weights. The fit chose recency alone, so the blend would ship three
knobs with two of them at zero and a code path nobody exercises.

**Keeping the tag rule.** It lost to plain recency. A tag a writer chose once does not keep a row
current.

**Sections dedup in priority order.** The first version let profile's row win against any later
section, even when it was older. A correction filed under a project would then lose to the stale
rule it corrects, which is the case the owner ruled against.

**Vectors in the pools.** The first version selected `m.embedding::real[]` beside every pooled row
and compared in Rust. Under a generic plan, which sqlx's cached prepared statements reach after five
runs, Postgres detoasted and cast every candidate before the LIMIT, and the JSON payload grew by
more than thirty times. Pairs of ids cost at most a few thousand comparisons between pooled rows,
inside the database.

## Costs accepted

- **The pools and the pair comparison cost time.** Observed with `pgbench -M prepared` under
  `plan_cache_mode = force_generic_plan`, three rounds of 20 runs each, on synthetic local stores
  with random 768-dimension vectors. Ranges are across rounds, in milliseconds:

  | store | statement | p50 | p95 | payload, chars |
  |---|---|---|---|---|
  | 4,500 rows | before | 4.1 to 4.4 | 4.9 to 6.8 | 22,590 |
  | 4,500 rows | after | 8.0 to 9.5 | 13.5 to 20.0 | 58,103 |
  | 20,000 rows | before | 18.4 to 19.2 | 21.9 to 29.6 | 23,216 |
  | 20,000 rows | after | 27.1 to 29.4 | 38.2 to 46.5 | 58,987 |

  The first version, with vectors in the pools, ran at a p50 of 73 to 75 ms at 20,000 rows with a
  payload of 796,546 characters. At 20,000 rows the p95 increase spans 9 to 25 ms across rounds,
  so it does not clearly meet a 20 ms budget. These are observations on one laptop against
  synthetic data, not on a hosted store.
- **A dropped row's twin can sit outside every printed section.** A newer twin deep in the recent
  pool can drop an older profile row and then lose its own slot to rows newer still. Neither prints;
  `memory_search` still finds both.
- **Rows without a usable vector are never dropped.** A store with an embedder failure can still
  print the same fact twice.
- **The evaluation's labels are proxies.** The search label sees only the first and last emission
  of each row, and the supersession label favours recent rows, because the rows a session revises
  are the ones it just wrote. A loose upper-bound search label, which counts rows popular across
  weeks, favoured the embedding rankers.

## What this is not for

It does not touch `memory_search` ranking, which stays a similarity search. It does not dedup the
store: the conflict sweep and the review queue still decide which of two rows should retire the
other. A digest drop is a display choice for one call and writes nothing.

## Reversal condition

Revisit when the per-call recall log (decision 0024) shows a different mix: for example, that rows
popular across weeks are what sessions reach for after a bootstrap, as the loose upper-bound label
suggested. Until that log has enough sessions in it, recency is the ranker the evidence supports.
