# 29. Thresholds belong to the model

**Date:** 10 October 2026 · **Status:** accepted, implementation in progress · **Decided by:** the owner

## Decision

A cosine threshold belongs to the embedding model whose vectors it compares. The server ships a table
of default values per model family in `src/domain/similarity.rs`, and each value carries its basis.

- **A family matches by substring.** An embedder id matches a family when its lowercased form
  contains the family name. `bge-base-en-v1.5` matches `Xenova/bge-base-en-v1.5@q8` and
  `openai:BAAI/bge-base-en-v1.5`. `embeddinggemma-2` matches `openai:google/embeddinggemma-2` and
  not `openai:google/embeddinggemma-300m`. The stored id keeps its exact form; only the lookup
  lowercases it.
- **Each key resolves to the first of four values:** the override in the block that configures the
  model (`EMBED_THRESHOLDS`, source `override`), the key's old single variable when set (`legacy`),
  the family's table value (its basis), and bge-base-en-v1.5's value (`guessed`).
- **Overrides belong to one block.** `EMBED_THRESHOLDS=dedupe=0.98,conflict=0.92` sets two keys for
  the model `EMBED_*` names and for no other. Boot refuses a key no table registers.
- **The old single variables stay legal with one block.** `DEDUPE_THRESHOLD`,
  `CONFLICT_THRESHOLD`, `BOOTSTRAP_DEDUP_COSINE`, `GRAPH_ROUTE_MAX_TOP` and `GRAPH_ROUTE_MAX_SPREAD`
  override their key for the one configured model, as before. Boot warns once for each one whose
  value differs from the model's table value, and names the variable, both values and the model.
  The dual-model change (decision 0027, the next PR) will refuse them beside a second block, name
  each with its value, and say to move it into the override for the model it was tuned on.
- **An unlisted model boots on guesses.** Each key it lacks takes bge-base-en-v1.5's value, and boot
  logs one warning per model naming every guessed key. The dual-model change will flip a unit onto
  a model only when none of its acting keys (`dedupe`, `conflict`, `cleanup_near_certain`) is
  guessed.
- **Cross-key checks run per model.** `conflict` at or below `dedupe`, and `cleanup_worth_asking`
  below `cleanup_near_certain`. Boot refuses a resolved model that fails one and names the model and
  both values.
- **A fork registers its own keys** with `Registry::extend(keys, families, checks)`, one call in
  `src/main.rs`. A key or a family value registered twice panics at boot: both are registration
  bugs.

The table this record ships:

| Key | Acts | bge-base-en-v1.5 | EmbeddingGemma 2 | Gemma basis | Confidence |
|---|---|---|---|---|---|
| `dedupe` | yes | 0.97 | 0.995 | study | low |
| `conflict` | yes | 0.90 | 0.91 | study | medium |
| `bootstrap_dedup` | no | 0.90 | 0.919 | study | medium |
| `cleanup_near_certain` | yes | 0.97 | 0.995 | study | low |
| `cleanup_worth_asking` | no | 0.65 | 0.754 | study | medium |
| `route_max_top` | no | 0.65 | 0.76 | sweep | medium |
| `route_max_spread` | no | 0.08 | 0.07 | sweep | medium |

Every bge-base-en-v1.5 value has basis `shipped`: production ran on it before this record. The label
claims nothing more. `DEDUPE_THRESHOLD` and `CONFLICT_THRESHOLD` were picked before any real data
existed (`VERIFY.md`), and `src/domain/routing.rs` calls both route values design targets. `study`
means the threshold study below measured it. `sweep` means the LongMemEval-S fusion sweep of 10
October 2026 measured it ([`docs/results/2026-10-fusion-sweep.md`](../results/2026-10-fusion-sweep.md)).
`carried` means bge-base-en-v1.5's shipped value, copied with no measurement for this model; no
EmbeddingGemma 2 key carries it now. An acting key changes or merges data with no person reading it
first.

## Context

The owner asked on 10 October 2026: "Apply threshold lines for new model -> Need support for old and
new models separately." The store is moving from bge-base-en-v1.5 to EmbeddingGemma 2
([`docs/results/2026-10-embedding-model-comparison.md`](../results/2026-10-embedding-model-comparison.md)),
and every cosine threshold in the server was set on bge. During a switch both models run side by
side, each unit reads with one of them, and each comparison must use the values of the model that
produced its vectors.

The two models score on different scales. A threshold study on a production store, aggregates only,
ran on 10 October 2026
([`docs/results/2026-10-threshold-study.md`](../results/2026-10-threshold-study.md) carries every
figure below with the output key it came from). It took 1,999 anchor rows, scored each anchor's 20 nearest rows in its own
namespace under both models, and pooled 39,574 scores per model. The pool's median score was 0.807
under EmbeddingGemma 2 against 0.745 under bge, and the bge range 0.65 to 0.97 lands at 0.754 to
0.987. A bge value read against Gemma vectors sits at a different point of the distribution.

The study mapped by rank. For a bge threshold t it counted the pool scores at or above t, then took
the Gemma score with the same count above it. The 95% intervals come from 1,000 bootstrap resamples
of the anchors. A regression line would have pulled every high threshold toward the mean: the two
models correlate at 0.70 on the pool, and a least-squares fit puts `dedupe` at 0.900. Where the pool
held too few high scores, the study read labelled pairs instead: pairs a person dismissed as "both
are fine", corrections clients wrote with `supersedes`, and pairs a person or a model merged as the
same fact. Every figure below is a study measurement except the two route keys, which the fusion
sweep measured.

- **`dedupe` 0.995, low confidence.** The mapping had a support of 2 scores, and 373 of the 1,000
  resamples held none at bge 0.97, so labels decided it. All 248 human-judged must-not-merge pairs
  sit below 0.995; the highest sits at 0.9902. 14 client corrections score at or above 0.995,
  against 115 at or above bge's 0.97 today.
- **`cleanup_near_certain` 0.995, low confidence.** Under bge's 0.97 the cleanup pass proposed 3
  paraphrase pairs as near-certain, and a person dismissed all 3. At 0.995 none of the 4 labelled
  cleanup pairs qualifies, so each one reaches a model first.
- **`conflict` 0.91, medium confidence.** It flags 71.0% of client corrections (69.9% under bge
  today) and 64.1% of dismissed pairs (100% today), and keeps 13 of the 14 confirmed corrections
  among stored candidates. The mapped value, 0.9185 (interval 0.9138 to 0.9234), would drop
  correction recall to 65.4%. Neither model separates corrections from dismissed pairs well: the
  best AUC was 0.65.
- **`bootstrap_dedup` 0.919, medium confidence.** Mapped 0.9185, support 246, interval 0.9138 to
  0.9234.
- **`cleanup_worth_asking` 0.754, medium confidence.** Mapped 0.7544, support 34,613, interval
  0.7523 to 0.7567.
- **`route_max_top` 0.76 and `route_max_spread` 0.07, from the fusion sweep, medium confidence.**
  Both compare a query with stored rows. The store keeps no query text, the recall log records ids
  and ranks with no scores, and EmbeddingGemma 2 embeds queries with a different prefix from
  documents, so the doc-to-doc mapping does not carry over. The threshold study left them
  unmeasured and the table carried bge's 0.65 and 0.08. The sweep then ran LongMemEval-S on 10
  October 2026 and matched each key by quantile on the fused score at `SEARCH_LEXICAL_WEIGHT=0.35`.
  `route_max_top` matches the share of questions whose rank-1 fused score sits at or above bge's
  0.65: scoped 0.7559 (interval 0.7503 to 0.7643), corpus 0.7655 (0.7576 to 0.7730).
  `route_max_spread` matches the rank-1 minus rank-5 gap against bge's 0.08: scoped 0.0665 (0.0603
  to 0.0728), corpus 0.0696 (0.0584 to 0.0861). 0.76 and 0.07 lie inside all four intervals, and the
  corpus interval for the spread still contains 0.08. With bge's pair, Gemma's router walked 0 of
  500 corpus questions and 59 of 500 scoped. At 0.76 and 0.07 Gemma's router walks 125 of 500 scoped
  questions (25.0%) and 150 of 500 corpus (30.0%), against bge's 23.2% and 32.0% at 0.65 and 0.08,
  measured on the sweep's baseline runs. Quantile matching shows the
  router fires as often as it does under bge. It does not show that the walks help.

The study names its own limits. It read one snapshot. Live rows exclude writes that dedupe had
already folded, so the pool holds few near-duplicates. Every dismissed pair and every stored
conflict candidate scores bge 0.90 or above, so no label says how a pair below that floor would
have been judged. A store with different writing habits could need different values.

The previous revision of the migration design kept one setting per key, declared that the settings
described one baseline model (`SIMILARITY_BASELINE_MODEL`), and asked the operator to pin every
other model by hand. The study gave EmbeddingGemma 2 values of its own, and this record replaces
that design.

## What lost

**Pins only, with a baseline model.** Every model but one needed a hand-written pin line, and
`SIMILARITY_BASELINE_MODEL` had to name whichever model the settings were tuned on. An operator who
switched models and forgot the pin ran the new model on the old one's values with nothing in the log.
The table carries measured values for the two models in use, and overrides cover the rest.

**One value for every model.** The old single variables applied to whatever model ran. During a
switch that hands a bge-tuned 0.97 to EmbeddingGemma 2, where the study puts the same key at 0.995.
Boot now warns when one departs from the model's table, and the dual-model change will refuse them
while two blocks are configured.

**Keys by exact embedder id.** The same weights reach the server under several ids:
`Xenova/bge-base-en-v1.5@q8` in process, `openai:BAAI/bge-base-en-v1.5` through an endpoint. An
exact-id table needs a row per quantisation and per host, each a copy of the last, and an id nobody
listed falls to the guess.

## What it costs, accepted

- **Substring matching assumes quantisations of one model share thresholds.** The evidence is one
  pair: llama.cpp Q8_0 and the ONNX q8 export of EmbeddingGemma 2 agreed at cosine 0.9997 or above
  (measured, `docs/results/2026-10-embedding-model-comparison.md`). A fine-tune whose id contains a
  family name inherits that family's values; its operator sets `EMBED_THRESHOLDS`.
- **EmbeddingGemma 2's route values rest on distribution matching.** No run exercised the graph
  walk or scored its output, so the sweep shows the router fires about as often as under bge and
  not that the walks help. The values hold only at fused scores from `SEARCH_FUSION=linear` with
  `SEARCH_LEXICAL_WEIGHT=0.35`; a different weight needs a remap, and under `rrf` or `linear_minmax`
  the router stands down and the values stop mattering.
- **`dedupe` at 0.995 folds fewer true duplicates at write:** 7 of the study's 131 merge-like pairs,
  against 24 under bge at 0.97. Cleanup still finds the rest with a model in the loop. A wrong fold
  loses a fact with nobody watching; a missed one leaves a duplicate somebody can still merge.
- **`conflict` at 0.91 records more pairs.** In the pool, 0.86% of Gemma scores sit at or above
  0.91, against 0.62% of bge scores at or above 0.90: about 38% more candidates to review.
- **Two values ship at low confidence.** `dedupe` and `cleanup_near_certain` rest on 248 and 4
  labelled pairs.

## What this is not for

**It is not calibration inside the server.** That needed a table, an estimator, a migration phase
and five repository methods, to produce numbers the owner reviews once per switch anyway. A
self-hoster moving to a model with no entry measures it offline and writes the result into
`EMBED_THRESHOLDS`.

It does not retune search fusion weights, and it does not claim the study's values fit every store.

## Reversal condition

- **Two quantisations of one model measure apart.** If a mapping on one store puts a key for two
  quantisations of one family further apart than that key's bootstrap interval, key that family by
  exact id.
- **A run that scores the graph walk disagrees.** The sweep reported on 10 October 2026 and
  found that bge's values do not hold: with 0.65 and 0.08 Gemma's router walks 0 of 500 corpus
  questions. It replaced EmbeddingGemma 2's `route_max_top` and `route_max_spread` with 0.76 and
  0.07, basis `sweep`. If a later run that scores walk output, or a move of
  `SEARCH_LEXICAL_WEIGHT` off 0.35, puts the best pair elsewhere, replace the two values.
- **A correction folds at 0.995.** If the fold log line shows a correction collapsing into the row
  it corrects, revisit EmbeddingGemma 2's `dedupe`.
