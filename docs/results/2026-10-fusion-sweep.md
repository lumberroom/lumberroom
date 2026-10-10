# Search fusion sweep on LongMemEval-S, October 2026

Runs from 10 October 2026, on branch `exp/fusion-sweep`. This is the companion to
`2026-10-embedding-model-comparison.md`: same two embedders, same 500 questions, and this time the
search blend changes while the stored corpus stays fixed. Every number here came from a run named
below or from the sweep's analysis files, and those paths are relative to the sweep directory. The raw
outputs (run reports, per-hit logs, analysis files) are not committed. Where a figure is derived
from run data rather than read off a run, it says so.

These are LongMemEval-S **retrieval recall** numbers. No answer is generated and no judge runs.

## Setup

- **Harness:** `scripts/eval-longmemeval.sh`, 500 questions, `session-as-document`, 20 hits scored
  per question, `SEARCH_DEBUG_SCORES=true` so each hit's cosine, keyword and fused score land in the
  per-hit log. `docs/eval-longmemeval.md` describes the flags and the log.
- **Embedders:** bge-base-en-v1.5 and EmbeddingGemma 2, both served by llama.cpp and reached through
  the `EMBED_PROVIDER=openai` adapter. Input caps as in the companion: 2,400 characters for bge,
  24,000 for Gemma.
- **Modes:** scoped, where each question searches its own ~48 sessions, and corpus, where every
  question searches one pool of 19,195 sessions (`--corpus-wide`).
- **Variants:**

| name | flags | blend |
|---|---|---|
| lin035 | none | linear, lexical weight 0.35 (the shipped default) |
| lin020 | `--lexical-weight 0.2` | linear, 0.2 |
| lin010 | `--lexical-weight 0.1` | linear, 0.1 |
| lin050 | `--lexical-weight 0.5` | linear, 0.5 |
| minmax | `--fusion linear_minmax` | per-query min-max cosine, 0.35 |
| rrf60 | `--fusion rrf --rrf-k 60` | reciprocal rank fusion, k = 60 |
| lin035b | none | lin035 again, run last, to measure repeatability |

- **One write per model and mode.** Each of the four stores was written once, then every variant ran
  `--search-only` against it. Order per mode: write, lin035, lin020, lin010, lin050, minmax, rrf60,
  then lin035b in a later pass (`progress.log`). Both corpus writes ended with zero sessions never
  stored. The Gemma corpus write took two tries: the first left 6 sessions unstored at 06:45Z, so
  the script dropped the database and wrote it again.
- **Bootstrap:** paired over the 500 questions, 5,000 resamples, seed 7, 95% interval from the 2.5
  and 97.5 percentiles (the `method` field in the sweep's `analysis/pairs/*.json`). The same as the
  companion.

The lead recomputed every paired delta and key mapping with an independent script (`verify.py`,
output in `verify.out`). Point values match the analysis files. Key intervals differ in the third or
fourth decimal (scoped route_max_top [0.7502, 0.7637] against [0.7503, 0.7643]), which moves no
recommendation.

## The noise floor: lin035 run twice

lin035b repeats lin035 on the same store. Deltas in percentage points, with the questions where the
repeat scored better / worse.

| model | mode | R@5 | R@10 | NDCG@10 | MRR |
|---|---|---|---|---|---|
| bge | scoped | 0.00 (0/0) | 0.00 (0/0) | 0.00 (0/0) | 0.00 (0/0) |
| bge | corpus | 0.00 (0/0) | 0.00 (0/0) | -0.001 [-0.004, 0.000] (0/1) | -0.007 [-0.015, -0.001] (0/5) |
| Gemma | scoped | 0.00 (0/0) | 0.00 (0/0) | 0.00 (0/0) | 0.00 (0/0) |
| Gemma | corpus | +0.20 [0.00, +0.60] (1/0) | +0.20 [0.00, +0.60] (1/0) | +0.11 [-0.01, +0.33] (3/2) | +0.11 [+0.0003, +0.29] (7/4) |

Sources: `analysis/pairs/<model>-<mode>-lin035b-vs-lin035.json`.

Scoped runs repeat exactly. Corpus runs drift, and Gemma drifts more than bge: one question changes
recall and 11 change rank.

The drift comes from access counts. Every search raises `access_count` on the rows it returns, and
the use boost (`SEARCH_USAGE_WEIGHT`, 0.05) reads it. The harness resets the counts before each
search-only run, and every corpus harness log in the sweep records that reset, so counts do not carry
from one run to the next. Inside a corpus run all 500 questions share one namespace, so each
question ranks against the counts that earlier questions left. The engine writes those counts on a
spawned task that the search does not wait for (`touch_accessed`, `src/adapters/postgres/memory.rs`),
so two runs of the same blend can see different counts at the same question. In scoped mode each
question has its own namespace and nothing carries.

A delta below counts as a finding only when it exceeds the noise row for its model, mode and metric
**and** its interval excludes zero. An interval with a bound at 0.00 touches zero and fails.

## Every run

| model | mode | variant | R@5 | R@10 | R@20 | NDCG@10 | MRR | never stored |
|---|---|---|---|---|---|---|---|---|
| bge | scoped | lin035 | 96.2% | 97.4% | 99.2% | 88.77% | 89.06% | 0 |
| bge | scoped | lin035b | 96.2% | 97.4% | 99.2% | 88.77% | 89.06% | 0 |
| bge | scoped | lin020 | 96.2% | 97.4% | 99.2% | 88.58% | 88.67% | 0 |
| bge | scoped | lin010 | 95.8% | 97.2% | 99.2% | 88.10% | 88.07% | 0 |
| bge | scoped | lin050 | 96.2% | 97.4% | 99.2% | 88.89% | 89.19% | 0 |
| bge | scoped | minmax | 95.8% | 97.2% | 99.2% | 88.05% | 88.00% | 0 |
| bge | scoped | rrf60 | 95.8% | 97.4% | 99.2% | 88.46% | 88.61% | 0 |
| bge | corpus | lin035 | 34.6% | 41.2% | 51.0% | 22.75% | 24.25% | 59 |
| bge | corpus | lin035b | 34.6% | 41.2% | 51.0% | 22.75% | 24.25% | 59 |
| bge | corpus | lin020 | 33.6% | 41.0% | 50.8% | 22.44% | 23.97% | 59 |
| bge | corpus | lin010 | 31.8% | 40.8% | 50.4% | 21.76% | 23.19% | 59 |
| bge | corpus | lin050 | 34.8% | 41.2% | 51.4% | 22.73% | 24.26% | 59 |
| bge | corpus | minmax | 31.8% | 40.0% | 49.8% | 21.29% | 22.72% | 59 |
| bge | corpus | rrf60 | 33.4% | 41.0% | 52.6% | 22.03% | 23.11% | 59 |
| Gemma | scoped | lin035 | 98.0% | 99.6% | 100.0% | 93.75% | 93.54% | 0 |
| Gemma | scoped | lin035b | 98.0% | 99.6% | 100.0% | 93.75% | 93.54% | 0 |
| Gemma | scoped | lin020 | 97.8% | 99.6% | 100.0% | 93.71% | 93.43% | 0 |
| Gemma | scoped | lin010 | 97.8% | 99.6% | 100.0% | 93.71% | 93.43% | 0 |
| Gemma | scoped | lin050 | 98.0% | 99.6% | 100.0% | 93.74% | 93.55% | 0 |
| Gemma | scoped | minmax | 97.8% | 99.6% | 100.0% | 93.59% | 93.22% | 0 |
| Gemma | scoped | rrf60 | 97.6% | 99.6% | 100.0% | 93.13% | 92.90% | 0 |
| Gemma | corpus | lin035 | 36.8% | 44.6% | 55.2% | 25.50% | 25.95% | 48 |
| Gemma | corpus | lin035b | 37.0% | 44.8% | 55.6% | 25.61% | 26.07% | 48 |
| Gemma | corpus | lin020 | 36.8% | 44.0% | 55.0% | 25.20% | 25.67% | 48 |
| Gemma | corpus | lin010 | 35.6% | 44.4% | 55.0% | 24.79% | 24.70% | 48 |
| Gemma | corpus | lin050 | 37.2% | 45.2% | 55.4% | 25.77% | 26.36% | 48 |
| Gemma | corpus | minmax | 36.8% | 45.8% | 58.4% | 25.69% | 26.34% | 48 |
| Gemma | corpus | rrf60 | 38.6% | 48.2% | 60.2% | 26.25% | 26.55% | 48 |

Source: `analysis/runs/<model>-<mode>-<variant>.json`, fields `overall` and `sessions_never_stored`.

**The never-stored counts in corpus rows do not mean lost sessions.** Both corpus writes stored
every session. A search-only run maps rows back to sessions through the store, and a session whose
write collapsed into a near-identical row matches no row, so the search-only report counts it as
missing. The count is identical across every variant of one store (59 for bge, 48 for Gemma), so it
shifts no delta.

Scoped mode sits near the ceiling: R@10 leaves 2.6 pp of headroom for bge and 0.4 pp for Gemma. In
scoped mode NDCG@10 and MRR carry the comparison.

## Paired deltas against lin035

Δ pp [95% CI] (questions better / worse). Bold marks a finding under the test above. The pair files
carry no interval for R@20, so R@20 stays in the run table only.

### bge, scoped

| variant | R@5 | R@10 | NDCG@10 | MRR |
|---|---|---|---|---|
| lin020 | +0.00 [+0.00, +0.00] (0/0) | +0.00 [+0.00, +0.00] (0/0) | -0.19 [-0.44, +0.07] (1/8) | -0.39 [-0.90, +0.07] (1/6) |
| lin010 | -0.40 [-1.00, +0.00] (0/2) | -0.20 [-0.60, +0.00] (0/1) | **-0.67 [-1.13, -0.26] (1/15)** | **-0.98 [-1.69, -0.35] (1/12)** |
| lin050 | +0.00 [+0.00, +0.00] (0/0) | +0.00 [+0.00, +0.00] (0/0) | **+0.12 [+0.001, +0.34] (4/0)** | +0.13 [+0.00, +0.40] (1/0) |
| minmax | -0.40 [-1.00, +0.00] (0/2) | -0.20 [-0.60, +0.00] (0/1) | **-0.72 [-1.22, -0.27] (1/15)** | **-1.05 [-1.81, -0.38] (1/12)** |
| rrf60 | -0.40 [-1.00, +0.00] (0/2) | +0.00 [+0.00, +0.00] (0/0) | -0.31 [-0.96, +0.29] (7/17) | -0.44 [-1.36, +0.40] (5/7) |

### bge, corpus

| variant | R@5 | R@10 | NDCG@10 | MRR |
|---|---|---|---|---|
| lin020 | **-1.00 [-2.00, -0.20] (0/5)** | -0.20 [-1.00, +0.60] (2/3) | -0.31 [-0.72, +0.08] (12/26) | -0.28 [-0.79, +0.24] (19/32) |
| lin010 | **-2.80 [-4.60, -1.20] (3/17)** | -0.40 [-2.00, +1.20] (7/9) | **-1.00 [-1.72, -0.31] (28/48)** | **-1.06 [-2.06, -0.10] (38/52)** |
| lin050 | +0.20 [-0.80, +1.20] (4/3) | +0.00 [-0.60, +0.60] (1/1) | -0.03 [-0.26, +0.19] (11/13) | +0.00 [-0.41, +0.37] (16/24) |
| minmax | **-2.80 [-5.40, -0.20] (15/29)** | -1.20 [-3.80, +1.40] (19/25) | **-1.46 [-2.77, -0.24] (61/65)** | **-1.53 [-3.10, -0.01] (77/73)** |
| rrf60 | -1.20 [-3.60, +1.20] (14/20) | -0.20 [-2.40, +2.00] (15/16) | -0.72 [-1.75, +0.26] (59/56) | -1.15 [-2.42, +0.07] (71/64) |

### EmbeddingGemma 2, scoped

| variant | R@5 | R@10 | NDCG@10 | MRR |
|---|---|---|---|---|
| lin020 | -0.20 [-0.60, +0.00] (0/1) | +0.00 [+0.00, +0.00] (0/0) | -0.03 [-0.12, +0.03] (5/2) | -0.11 [-0.31, +0.00] (0/2) |
| lin010 | -0.20 [-0.60, +0.00] (0/1) | +0.00 [+0.00, +0.00] (0/0) | -0.03 [-0.12, +0.03] (5/2) | -0.11 [-0.31, +0.00] (0/2) |
| lin050 | +0.00 [+0.00, +0.00] (0/0) | +0.00 [+0.00, +0.00] (0/0) | -0.01 [-0.07, +0.04] (2/1) | +0.01 [-0.08, +0.10] (2/1) |
| minmax | -0.20 [-0.60, +0.00] (0/1) | +0.00 [+0.00, +0.00] (0/0) | **-0.16 [-0.36, -0.01] (4/6)** | **-0.32 [-0.71, -0.02] (0/5)** |
| rrf60 | -0.40 [-1.00, +0.00] (0/2) | +0.00 [+0.00, +0.00] (0/0) | **-0.61 [-1.24, -0.08] (4/17)** | -0.64 [-1.47, +0.11] (3/8) |

Gemma scoped lin010 and lin020 produced identical metrics and identical counts. Nobody has opened
the per-question files to check whether the rankings match.

### EmbeddingGemma 2, corpus

| variant | R@5 | R@10 | NDCG@10 | MRR |
|---|---|---|---|---|
| lin020 | +0.00 [-0.80, +0.80] (2/2) | -0.60 [-1.80, +0.60] (3/6) | -0.30 [-0.69, +0.09] (14/31) | -0.29 [-0.70, +0.11] (19/40) |
| lin010 | -1.20 [-2.60, +0.20] (3/9) | -0.20 [-1.60, +1.20] (6/7) | **-0.72 [-1.34, -0.12] (29/47)** | **-1.25 [-2.06, -0.51] (37/59)** |
| lin050 | +0.40 [-0.40, +1.20] (3/1) | +0.60 [+0.00, +1.40] (3/0) | **+0.27 [+0.03, +0.57] (20/9)** | **+0.40 [+0.12, +0.77] (31/14)** |
| minmax | +0.00 [-3.01, +3.00] (31/31) | +1.20 [-1.80, +4.40] (34/28) | +0.19 [-1.25, +1.66] (91/71) | +0.39 [-1.54, +2.28] (125/83) |
| rrf60 | +1.80 [-0.80, +4.60] (28/19) | **+3.60 [+0.80, +6.60] (36/18)** | +0.75 [-0.45, +2.03] (97/62) | +0.59 [-0.98, +2.24] (123/69) |

Sources: `analysis/pairs/<model>-<mode>-<variant>-vs-lin035.json` (bge scoped lin020 is
`bge-scoped-lin020-vs-bge-scoped-lin035.json`).

Lowering the lexical weight or switching to minmax loses for both models. Only two results go the
other way, both Gemma in corpus mode: rrf60 on R@10 and lin050 on NDCG@10 and MRR.

## The search default

**bge-base-en-v1.5: keep `SEARCH_FUSION=linear` and `SEARCH_LEXICAL_WEIGHT=0.35`.** Dropping the
weight to 0.2 or 0.1 costs R@5 in corpus mode (-1.00 and -2.80 pp), and 0.1 also costs NDCG@10 and
MRR in scoped mode. minmax loses in both modes. rrf60 never clears the test. lin050 ties in corpus
mode and gains 0.12 pp NDCG@10 in scoped mode from 4 questions, with a lower bound of +0.001. That
does not carry a default change.

**EmbeddingGemma 2: keep `SEARCH_FUSION=linear` and keep 0.35 for now.**

- rrf60's +3.60 pp on R@10 shows up in corpus mode alone, 18 times the drift row. Corpus mode pools
  500 personas; scoped mode stands closer to one person's store, and there rrf60 loses 0.61 pp
  NDCG@10 [-1.24, -0.08]. rrf also stands the graph router down (next section).
- lin050 leans positive. It gains NDCG@10 and MRR in corpus mode at 2.5 and 3.6 times the drift
  rows, and costs nothing scoped (NDCG@10 -0.01 [-0.07, +0.04], MRR +0.01 [-0.08, +0.10]). The
  margin still sits within the drift: its NDCG@10 lower bound (+0.03) falls below the drift row's
  upper bound (+0.33), and the sweep measured drift for lin035 alone.
- To settle 0.5, write a new corpus store and run lin050 and lin035 against it, each with a repeat.
  If the weight moves, remap the two router keys below: they were mapped from lin035 scores.

## Router thresholds for EmbeddingGemma 2

The graph router reads the fused search score of each hit (`src/services/graph.rs`), which under
linear fusion is cosine plus `SEARCH_LEXICAL_WEIGHT` times the keyword score. It walks when the
rank-1 score falls below `GRAPH_ROUTE_MAX_TOP` **and** rank 1 minus rank 5 falls below
`GRAPH_ROUTE_MAX_SPREAD` (`src/domain/routing.rs`). bge's defaults are 0.65 and 0.08. Gemma packs
neighbours into a higher, narrower band, so those values mean something else on it.

**Method.** For each mode, take the share *s* of questions whose bge value sits at or above bge's
threshold, then take the Gemma value at the (1 - *s*) quantile. The same share of questions then
clears the line under both models. Values come from the lin035 hit logs. Intervals come from 5,000
bootstrap resamples of questions, seed 7. The mapped values are arithmetic over measured scores.

| key | bge | Gemma | scoped | corpus | confidence |
|---|---|---|---|---|---|
| `GRAPH_ROUTE_MAX_TOP` | 0.65 | **0.76** | s = 0.338, support 169, g = 0.7559 [0.7503, 0.7643] | s = 0.666, support 333, g = 0.7655 [0.7576, 0.7730] | medium |
| `GRAPH_ROUTE_MAX_SPREAD` | 0.08 | **0.07** | s = 0.768, support 384, g = 0.0665 [0.0603, 0.0728] | s = 0.426, support 213, g = 0.0696 [0.0584, 0.0861] | medium |

Sources: `analysis/keys/route_max_top.json`, `analysis/keys/route_max_spread.json`.

0.76 and 0.07 each lie inside both modes' intervals. Confidence stays at medium for two reasons.
Quantile matching reproduces bge's walk share and says nothing about whether the walks help. And
the corpus spread interval still contains bge's 0.08.

**Walk shares.** Tabulated from the lin035 hit logs at each mode's point estimates:

| | bge at (0.65, 0.08) | Gemma at (0.65, 0.08) | Gemma at mapped values |
|---|---|---|---|
| scoped, all | 23.2% (116/500) | 11.8% (59/500) | 23.4% (117/500) |
| scoped, gold at rank 1 | 15.8% (66/419) | 7.4% (33/448) | 18.1% (81/448) |
| scoped, gold not at rank 1 | 61.7% (50/81) | 50.0% (26/52) | 69.2% (36/52) |
| corpus, all | 32.0% (160/500) | **0.0% (0/500)** | 32.8% (164/500) |
| corpus, gold at rank 1 | 13.6% (14/103) | 0.0% (0/113) | 15.9% (18/113) |
| corpus, gold not at rank 1 | 36.8% (146/397) | 0.0% (0/387) | 37.7% (146/387) |

Source: `analysis/keys/route_joint.json`. The mapped columns use (0.7559, 0.0665) scoped and
(0.7655, 0.0696) corpus. At the rounded pair (0.76, 0.07) Gemma walks 125 of 500 scoped (25.0%)
and 150 of 500 corpus (30.0%), against bge's 116 and 160 at (0.65, 0.08)
(`analysis/keys/route_rounded_pair.json`).

With bge's values, Gemma walks 0 of 500 corpus questions and 59 scoped against bge's 116. A Gemma
deployment left on the defaults keeps the router from walking in corpus mode.

**rrf and linear_minmax stand the router down.** Both put scores on a scale the thresholds do not
share, and the router declines to compare (`src/config.rs` maps them to `Scale::Ranked`). Under
either blend the two keys stop mattering.

## Not measured yet

- Answer quality. No answers, no judge.
- Whether graph walks help. No run exercised a walk or scored its output.
- Drift for any corpus variant except lin035, and a lin050 repeat on a newly written store.
- Lexical weights above 0.5 and RRF k values other than 60. Gemma's corpus gain at 0.5 sits at the
  edge of the grid, so the sweep cannot place Gemma's best weight.
- R@20 intervals and per-type intervals. The pair files carry per-type means only, and the smallest
  type (single-session-preference, n = 30) moves 3.3 pp on one question.
- A store size between the two modes. A real single-user store sits somewhere between ~48 sessions
  and 19,195.
- Latency. The run files record it, but the extraction looks faulty (bge scoped lin035b reports
  84 ms at p50, p90 and p99 alike), so this document draws no latency conclusion.
