# EmbeddingGemma 2 against bge-base-en-v1.5, October 2026

Runs from 9 and 10 October 2026, on branch `feat/embeddinggemma-2`. Every number here came from a
run named below, and the JSON reports sit beside this file. Where a figure is arithmetic or a
single sample, it says so.

These are LongMemEval-S **retrieval recall** numbers: does the right session reach the top k. No
answer is generated and no judge runs, so none of this is the paper's QA accuracy.

## Setup

- **Harness:** `scripts/eval-longmemeval.sh`, 500 questions, `session-as-document`, linear fusion
  (the shipped default), depth 20. Writes go through `memory_write` and searches through
  `memory_search` on a scratch server and scratch Postgres.
- **Embedders:** both served by llama.cpp build 11515 behind an OpenAI-compatible endpoint on one
  RTX 5070 Ti, and reached through the new `EMBED_PROVIDER=openai` adapter
  (`src/adapters/embedding/remote.rs`).

| | bge-base-en-v1.5 | EmbeddingGemma 2 |
|---|---|---|
| Weights | `bge-base-en-v1.5-q8_0.gguf` (CompendiumLabs) | `embeddinggemma-2-Q8_0.gguf` (ggml-org) |
| Window | 512 tokens; input capped at 2,400 characters, cut and retried on a refusal | 8,192 tokens; input capped at 24,000 characters |
| Prefixes | query: `Represent this sentence for searching relevant passages: ` | query: `task: search result \| query: `, document: `title: none \| text: ` |
| Agreement with a reference build | cosine ≥ 0.9998 against fastembed's `Qdrant/bge-base-en-v1.5-onnx-Q`, the model the engine ships | cosine ≥ 0.9997 against the `onnx-community/embeddinggemma-2-ONNX` q8 export |

**The comparison mixes model and window.** A median session is about 2,240 Gemma tokens, so bge
reads roughly the first quarter of it and Gemma reads nearly all of it. A Gemma run capped at 512
tokens would separate the two; it has not been run.

## Scoped: each question searches its own ~48 sessions

Both runs stored all 23,867 sessions. Reports: `longmemeval-bge-base-api-20261009.json`,
`longmemeval-embeddinggemma2-20261009.json`.

| metric | bge-base | EmbeddingGemma 2 | Gemma minus bge, paired [95% CI] | Gemma better / worse |
|---|---|---|---|---|
| R@5 | 96.2% | **98.0%** | +1.8 [+0.2, +3.4] | 13 / 4 |
| R@10 | 97.4% | **99.6%** | +2.2 [+0.8, +3.8] | 13 / 2 |
| R@20 | 99.2% | **100.0%** | | |
| NDCG@10 | 88.8% | **93.8%** | +5.0 [+3.4, +6.8] | 123 / 60 |
| MRR | 89.1% | **93.6%** | +4.6 [+2.6, +6.6] | 61 / 28 |

Paired bootstrap over the 500 questions, 5,000 resamples, seed 7.

R@5 by question type:

| type | n | bge-base | EmbeddingGemma 2 |
|---|---|---|---|
| multi-session | 133 | 99.2% | 100.0% |
| temporal-reasoning | 133 | 96.2% | 97.0% |
| knowledge-update | 78 | 98.7% | 98.7% |
| single-session-user | 70 | 88.6% | 95.7% |
| single-session-assistant | 56 | 100.0% | 100.0% |
| single-session-preference | 30 | 86.7% | 93.3% |

Search latency per question (the whole `memory_search` round trip, query embedding included):

| | p50 | p90 | p99 | max |
|---|---|---|---|---|
| bge-base | 83 ms | 94 ms | 132 ms | 174 ms |
| EmbeddingGemma 2 | 92 ms | 113 ms | 228 ms | 474 ms |

The roughly 9 ms between them is the query embedding. Postgres does the same work for both: both
store 768-dimension vectors. Run wall time was 535 s for bge and 3,071 s for Gemma, almost all of it
write-side embedding of whole sessions.

## Corpus-wide: every question searches all 19,188 sessions

On this branch, `--corpus-wide` writes each unique session once (19,195 of the 23,867
references) into one namespace, `project:lme-corpus`, before the first search. The earlier behaviour
wrote and searched question by question, so early questions met a smaller pool than late ones.

**This mode is harsher than a real store.** LongMemEval simulates a different user for each question.
Pooled, "what's my dog's name?" competes with 499 other personas' dogs, where a real store holds one
person's memories. It compares models under heavy, same-shaped distraction; it does not predict
production recall.

### bge-base

Report: `longmemeval-bge-base-corpuswide-20261010.json`. 7 sessions never stored ("the embedding
server is unreachable", a short network drop); none was a gold session for any question, so no
score moved, but the harness rule calls a run clean only at zero.

| metric | corpus-wide | scoped |
|---|---|---|
| R@5 | 35.0% | 96.2% |
| R@10 | 41.6% | 97.4% |
| R@20 | 51.2% | 99.2% |
| NDCG@10 | 23.0% | 88.8% |
| MRR | 24.4% | 89.1% |

| type | n | R@5 corpus-wide | R@10 corpus-wide | R@5 scoped |
|---|---|---|---|---|
| single-session-assistant | 56 | 87.5% | 87.5% | 100.0% |
| knowledge-update | 78 | 39.7% | 46.2% | 98.7% |
| multi-session | 133 | 33.8% | 39.8% | 99.2% |
| temporal-reasoning | 133 | 24.8% | 33.1% | 96.2% |
| single-session-user | 70 | 18.6% | 27.1% | 88.6% |
| single-session-preference | 30 | 13.3% | 23.3% | 86.7% |

Questions about what the assistant said hold up; questions about the user's own facts and
preferences collapse, which fits the 500-personas reading.

Search latency over the full pool: p50 63 ms, p90 330 ms, p99 2,113 ms, max 5,214 ms. The tail grows
with the rows a search can reach. Query plans for the slow searches have not been read yet.

### EmbeddingGemma 2, against bge-base

Report: `longmemeval-embeddinggemma2-corpuswide-20261010.json`. All 19,195 sessions stored. Wall
time 2,763 s against bge's 1,165 s, almost all of it write-side embedding.

| metric | bge-base | EmbeddingGemma 2 | Gemma minus bge, paired [95% CI] | Gemma better / worse |
|---|---|---|---|---|
| R@5 | 35.0% | 37.0% | +2.0 [-2.2, +6.2] | 60 / 50 |
| R@10 | 41.6% | 45.2% | +3.6 [-0.6, +7.8] | 64 / 46 |
| R@20 | 51.2% | 54.8% | +3.6 [-0.6, +8.2] | 74 / 56 |
| NDCG@10 | 23.0% | 25.6% | +2.6 [+0.3, +5.0] | 103 / 91 |
| MRR | 24.4% | 26.0% | +1.5 [-1.3, +4.3] | 132 / 117 |

Paired bootstrap as above. Only NDCG@10's interval clears zero, so corpus-wide the two models are
close, unlike the scoped run.

| type | n | bge R@5 | Gemma R@5 | bge R@10 | Gemma R@10 |
|---|---|---|---|---|---|
| single-session-assistant | 56 | 87.5% | 98.2% | 87.5% | 98.2% |
| knowledge-update | 78 | 39.7% | 55.1% | 46.2% | 65.4% |
| single-session-user | 70 | 18.6% | 28.6% | 27.1% | 42.9% |
| temporal-reasoning | 133 | 24.8% | 24.1% | 33.1% | 33.8% |
| single-session-preference | 30 | 13.3% | 13.3% | 23.3% | 13.3% |
| multi-session | 133 | 33.8% | 23.3% | 39.8% | 30.8% |

Gemma gains on the single-fact types and loses on multi-session, where the answer spans several
sessions. Per-type counts are small, so read these as directions, not settled differences.

Search latency over the full pool: p50 57 ms, p90 268 ms, p99 2,122 ms, max 5,715 ms, the same shape
as bge's. The tail is the store's, not the model's.

## Write cost grows with the namespace

From a corpus-wide bge write phase on 9 October (Docker Postgres on a Mac, 4 writes in flight, the
run stopped at 18,000 sessions when the embedding host lost power):

| rows already in the namespace | time per 500 writes |
|---|---|
| 1,000 | 7 s |
| 4,500 | 12 s |
| 8,500 | 14 s |
| 10,500 | 18 s |
| 14,500 | 23 s |
| 16,500 | 28 s |

Embedding time per write stays flat, so the growth is server-side work that scales with rows already
stored: the dedupe or conflict checks, or the HNSW insert. Not isolated yet.

## Embedding latency

One request at a time, median of 3 unless marked.

| input | GPU, RTX 5070 Ti | CPU, Ryzen 5 3600, 2 threads | CPU, Ryzen 5 3600, 4 threads | CPU, AWS m6a.xlarge (EPYC 7R13), 2 CPUs |
|---|---|---|---|---|
| ~20 tokens | | 0.068 s | 0.049 s | 0.060 s |
| ~100 tokens | | 0.304 s | 0.187 s | 0.252 s |
| ~290 tokens | 0.05 s | | | |
| ~500 tokens | | 1.35 s | 0.81 s | 1.84 s |
| ~2k tokens | | 7.7 s | 3.97 s | 7.28 s |
| ~4k tokens | 0.2 s | | | |
| ~8k tokens | 0.6 s | 46 s | 24.8 s | 53.8 s (one run) |

EmbeddingGemma 2 Q8_0 on llama.cpp build 11515 in every column. The first GPU request after start
took 19 s; the table shows steady state.

**Memory is the trap at long inputs.** llama.cpp turns flash attention off for this architecture
("Flash Attention not supported"), so attention memory grows with the square of the micro-batch. On
the m6a at `-ub 8192`, the container reached 8.1 GiB after the 8k input. At a 2,048-token window it
should need about a sixteenth of that: arithmetic, not measured. On the GPU the server used 2.5 GiB
of VRAM with 4 slots at an 8k window.

`llama-server` refuses an input past its window ("input (N tokens) is too large to process") rather
than truncating it. The `openai` adapter caps input by characters and, on that refusal, cuts by the
token ratio in the message and retries up to five times. Token density varies inside a session (one
session's first 600 characters were 162 bge tokens and the next 600 about 400), which is why two
retries were not enough and five are.

## Not measured yet

- Gemma at a 512-token window, to split model from window.
- Why Gemma loses on multi-session questions corpus-wide.
- Query plans behind the corpus-wide search tail, and the cause of the write-cost growth.
- Any of this on the QA-accuracy protocol.
