# Cosine thresholds for EmbeddingGemma 2 against bge-base-en-v1.5, October 2026

A study run on 10 October 2026 against a copy of a production store, aggregates only, restored the
same day. It holds no memory text, no row ids and no namespace names. Decision 0029 takes
EmbeddingGemma 2's table values from it, and every figure 0029 cites appears here with the script
and output key it came from.

Two scripts produced the numbers: `study2.py` built the neighbour pools, the mappings, the bootstrap
and the label statistics and wrote `results2.json`; `extra2.py` counted the acting keys at candidate
values and wrote `results2_extra.json`. Both ran beside the store copy and are not in this
repository, because their inputs are private rows. A figure with no key below came from a direct
count over the study's labelled-pair table, and says so.

This file covers the engine's keys: `dedupe`, `conflict`, `bootstrap_dedup`,
`cleanup_near_certain` and `cleanup_worth_asking`, and why the study could not measure
`route_max_top` and `route_max_spread`.

## Results

| key | bge-base-en-v1.5 | EmbeddingGemma 2 | acts unattended | confidence | basis |
|---|---|---|---|---|---|
| `dedupe` | 0.97 | 0.995 | yes | low | highest human-judged must-not-merge pair 0.9902, plus a margin of 0.0048; the pool mapping has support 2 |
| `cleanup_near_certain` | 0.97 | 0.995 | yes | low | all 3 paraphrase pairs bge proposed as near-certain were dismissed by a person; their highest Gemma score is 0.9902 |
| `conflict` | 0.90 | 0.91 | yes | medium | chosen to reproduce the share of client corrections flagged (71.0% against 69.9%); the pool mapping gives 0.9185 |
| `bootstrap_dedup` | 0.90 | 0.919 | no | medium | pool mapping 0.9185, support 246 |
| `cleanup_worth_asking` | 0.65 | 0.754 | no | medium | pool mapping 0.7544, support 34,613 |
| `route_max_top` | 0.65 | not measured | no | none | compares a query with stored rows; see "Query-side keys" |
| `route_max_spread` | 0.08 | not measured | no | none | as `route_max_top` |

## Method

**Rows.** Live rows (`superseded_by IS NULL` and not yet ended) that carry plaintext and a vector
under both models. Every EmbeddingGemma 2 vector has unit norm within 1e-4.

**Neighbour pool (`study2.py`).** The study ordered the live rows by `md5(id::text)` and took the
first 2,000 as anchors. For each anchor it scored every other live row in the same namespace under
both models, with exact dot products on unit vectors, and kept each model's own 20 nearest. One
anchor has no neighbour in its namespace and 53 have fewer than 20, which leaves 1,999 anchors and
39,574 scores per model (`results2.json: mapping`).

**Mapping by rank.** For a bge threshold t, the support k is the count of pooled bge scores at or
above t. The mapped Gemma value is the k-th highest pooled Gemma score, so the same count of pool
scores sits at or above it under each model. The 95% interval comes from 1,000 bootstrap resamples
of the anchors (seed 20261010), percentiles 2.5 and 97.5.

**Why rank and not a fit.** A least-squares line over the neighbour pairs, Gemma = 0.430 × bge +
0.483, puts `dedupe` at 0.900. The two models correlate at Pearson 0.698 over the pool
(`results2.json: sanity`), and at that correlation a regression pulls every high threshold toward
the mean. The report gives no output key for the fit itself.

**Labels.** The study rebuilt every labelled pair from the store's own tables and computed both
cosines as `1 - (a <=> b)`:

| set | rule | pairs |
|---|---|---|
| human_dismissed | every row of `memory_pair_dismissed`, the "both are fine" ledger | 223 |
| dream_human_dismiss | a merge a dreaming pass proposed and a person dismissed | 28 |
| dream_human_apply | a merge a dreaming pass proposed and a person applied | 2 |
| dream_model_applied | a merge a dreaming pass applied on its model's verdict | 24 |
| cleanup_applied_paraphrase | a cleanup paraphrase proposal, applied | 1 |
| cleanup_proposed_paraphrase | a cleanup paraphrase proposal, still proposed | 3 |
| supersession_client | a `supersedes` link a client wrote | 760 |
| supersession_dream_rewrite | a `supersedes` link a dreaming rewrite wrote | 101 |
| conflict_dismissed | a `memory_conflict` candidate also in the dismissed ledger | 223 |
| conflict_superseded | a `memory_conflict` candidate where one row supersedes the other | 14 |
| conflict_neither | the remaining conflict candidates | 329 |

All 223 human-dismissed pairs are conflict candidates, so `human_dismissed` and
`conflict_dismissed` are one set. Every dismissed pair and every stored conflict candidate scores
bge 0.90 or above, because 0.90 is the candidate floor. The 101 rewrites restate the same fact, so
the study counts them as merge-like and keeps them out of the must-not-merge set. The recomputed bge
cosine matches the stored `memory_conflict.similarity` on all 566 candidates.

Two derived sets carry the acting keys:

- **Must-not-merge, human-judged: 248 pairs.** The 223 human-dismissed pairs and the 25
  same-namespace pairs of dream_human_dismiss. Dedupe compares within a namespace, so only those
  count. The 751 same-namespace client supersessions join them for a total of 999
  (`results2.json: acting.dedupe`).
- **Merge-like: 131 pairs.** dream_human_apply (2), dream_model_applied (24),
  cleanup_applied_paraphrase (1), cleanup_proposed_paraphrase (3) and supersession_dream_rewrite
  (101) (`results2_extra.json: dedupe`).

## Distribution mapping

Same-namespace pool, 1,999 anchors, 39,574 scores per model (`results2.json: mapping`):

| key | bge | support | share of pool | Gemma mapped | 95% bootstrap |
|---|---|---|---|---|---|
| `dedupe`, `cleanup_near_certain` | 0.97 | 2 | 0.005% | 0.9869 | not estimable: 373 of 1,000 resamples hold no score at or above 0.97 |
| `conflict`, `bootstrap_dedup` | 0.90 | 246 | 0.62% | 0.9185 | 0.9138 to 0.9234 |
| `cleanup_worth_asking` | 0.65 | 34,613 | 87.46% | 0.7544 | 0.7523 to 0.7567 |

Pool quantiles (`results2.json: pool_quantiles`):

| model | min | p10 | p25 | p50 | p75 | p90 | p99 | p99.9 | max |
|---|---|---|---|---|---|---|---|---|---|
| bge | 0.426 | 0.635 | 0.698 | 0.745 | 0.783 | 0.815 | 0.889 | 0.943 | 0.986 |
| Gemma | 0.573 | 0.748 | 0.777 | 0.807 | 0.833 | 0.855 | 0.906 | 0.949 | 0.990 |

Gemma compresses the scale upward. The median moves from 0.745 to 0.807, and the bge range 0.65 to
0.97 maps to 0.754 to 0.987.

Support at bge 0.90 and above is thin: 246 pool scores at 0.90 and 2 at 0.97. Every value above that
line rests on those counts or on labels.

## Acting keys

### `dedupe`, 0.995

Must-not-merge pairs at the top of each set (`results2.json: acting.dedupe`):

| set | n | highest bge | highest Gemma | at or above bge 0.97 |
|---|---|---|---|---|
| supersession_client, same namespace | 751 | 1.000 | 0.99993 | 115 |
| human_dismissed | 223 | 0.9855 | 0.9902 | 3 |
| dream_human_dismiss, same namespace | 25 | 0.8800 | 0.9008 | 0 |

Counts at candidate Gemma values (`results2_extra.json: dedupe`; the 0.9767 and 0.9869 rows come
from a direct count over the labelled-pair table):

| cut | client supersessions at or above | human-judged must-not-merge at or above (of 248) | merge-like at or above (of 131) |
|---|---|---|---|
| bge 0.97 today | 115 | 3 | 24 |
| Gemma 0.97 | 162 | 5 | 34 |
| Gemma 0.9767 | 115 | 3 | 22 |
| Gemma 0.98 | 95 | 3 | 17 |
| Gemma 0.9869 (pool-mapped) | 55 | 1 | 10 |
| Gemma 0.99 | 38 | 1 | 9 |
| Gemma 0.995 | 14 | 0 | 7 |

At 0.995 all 248 human-judged must-not-merge pairs sit below the threshold. The highest sits at
0.9902, a margin of 0.0048. 14 client supersessions score at or above 0.995, against 115 at or above
bge's 0.97 today. The cost: dedupe folds 7 of the 131 merge-like pairs, against 24 under bge today.

No threshold keeps every client supersession below it under either model: three score at or above
Gemma 0.9999 and four at bge 0.99999 or above (`results2_extra.json: dedupe.sup_ge_09999,
sup_bge_eq1`). In the engine, a write that carries `supersedes` skips the dedupe step
(`src/services/write.rs`, step (f)). The `memory_write` flow stores a correction once without the
link before it sends the link, so that first call still meets dedupe. A correction whose digits,
identifiers or negation differ from the row it corrects never folds (`collapse_block`); the study
did not count how many of the 14 that guard covers.

The pool mapping (0.9869) rests on a support of 2, so the labels decided this key.

### `cleanup_near_certain`, 0.995

Under bge's 0.97 the cleanup pass queued 3 paraphrase pairs as near-certain, and all 3 also sit in
the human-dismissed ledger (`results2_extra.json: cleanup_also_dismissed`). At 0.995 none of the 4
labelled cleanup pairs qualifies as near-certain (`results2.json: labels`). The one applied
paraphrase (Gemma 0.9886) still reaches a model, since it sits far above `cleanup_worth_asking`.

### `conflict`, 0.91

Positives are the 751 same-namespace client supersessions, since a confirmed correction is what a
conflict scan should catch. Negatives are the 223 human-dismissed pairs. Rates at candidate values
(`results2_extra.json: conflict`):

| Gemma value | client corrections flagged | dismissed pairs flagged | confirmed candidate corrections kept (of 14) |
|---|---|---|---|
| bge 0.90 today | 69.9% | 100% | 14 |
| 0.905 | 75.0% | 70.9% | 14 |
| 0.91 | 71.0% | 64.1% | 13 |
| 0.9114 (same count as bge) | 69.9% | 62.3% | 13 |
| 0.915 | 67.6% | 59.6% | 13 |
| 0.9185 (mapped) | 65.4% | 54.7% | 12 |

Separation (`results2.json: acting.conflict`), as AUC with the rule "at or above is positive":

| comparison | bge AUC | Gemma AUC |
|---|---|---|
| client corrections (751) against dismissed (223) | 0.526 | 0.603 |
| confirmed candidate corrections (14) against dismissed (223) | 0.557 | 0.651 |

Neither model separates corrections from "both are fine" pairs well: the best AUC is 0.65. So the
value was chosen to keep today's behaviour. 0.91 flags 71.0% of client corrections against 69.9%
today, flags 64.1% of dismissed pairs against 100% today, and keeps 13 of the 14 confirmed candidate
corrections; the fourteenth sits at Gemma 0.9098. The mapped 0.9185 (interval 0.9138 to 0.9234)
would drop correction recall to 65.4%.

0.91 sits below the mapped interval, so the scan records more pairs. 0.86% of pooled Gemma scores sit
at or above 0.91, against 0.62% of bge scores at or above 0.90 (`results2.json:
acting.dreaming_min.grid` pool share, `mapping.conflict.share`): about 38% more candidates.

## Mapped keys

| key | bge | Gemma | support | 95% bootstrap |
|---|---|---|---|---|
| `bootstrap_dedup` | 0.90 | 0.919 (0.9185) | 246 | 0.9138 to 0.9234 |
| `cleanup_worth_asking` | 0.65 | 0.754 (0.7544) | 34,613 | 0.7523 to 0.7567 |

Both come from the same-namespace pool (`results2.json: mapping`).

## Query-side keys

`route_max_top` (0.65) and `route_max_spread` (0.08) compare a query vector with stored rows. The
store keeps no query text, and the recall log records memory ids and ranks with no scores, so the
copy cannot measure either. EmbeddingGemma 2 also embeds queries with a different prefix from
documents, which rules out reusing the document-to-document mapping. A retrieval harness that
records the cosine and rank of every returned hit under both models can map `route_max_top` by the
share of top-1 scores at or above bge's value, and `route_max_spread` by the distribution of top-1
minus top-k gaps.

## Limits

- **Thin labels.** `dedupe` and `cleanup_near_certain` rest on 248 and 4 labelled pairs, and the
  pool mapping at bge 0.97 on 2 scores. `conflict` rests on 14 confirmed candidate corrections.
- **Selection bias.** Every dismissed pair and every conflict candidate scores bge 0.90 or above, so
  no label says how a pair below that floor would have been judged, and the bge separation figures
  lean toward chance.
- **One snapshot.** Live rows exclude writes dedupe had already folded, so the pool holds few
  near-duplicates. That explains why only 2 pool scores reach bge 0.97.
- **Reordered neighbours.** The two models agree on the top-1 same-namespace neighbour for 48.6% of
  anchors (`results2.json: sanity`). Dedupe matches and conflict candidates will change membership
  after a switch even with matched thresholds.
- **Other stores.** A store with different writing habits could need different values.
