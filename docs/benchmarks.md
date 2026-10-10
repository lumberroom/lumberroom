# Benchmarks

Every retrieval number this project has, with the configuration that produced it and what it does
not say. One page, because the numbers were scattered across `VERIFY.md`, three documents and a
475KB JSON file, and somebody asking "how good is our search" should not have to assemble them.

**Two rules this page keeps.** A number appears only if a run produced it, and the run is named so
it can be repeated or disagreed with. Where a figure is a design target rather than a measurement,
it says so.

---

## 1. LongMemEval-S retrieval

What it measures: given a question and a haystack of roughly 48 chat sessions, does the right
session reach the top k. Retrieval recall alone, no answer generation and no judge model, so this
is **not** a "LongMemEval score" in the sense the paper's leaderboard uses.

Run it with `./scripts/eval-longmemeval.sh --dataset <path>`. The harness writes through the real
`memory_write` tool into real Postgres and searches through the real `memory_search`, on a scratch
server and a scratch database that are created and dropped per run.

### EmbeddingGemma 2 against bge-base, October 2026

`docs/results/2026-10-embedding-model-comparison.md` holds the 9 and 10 October runs: both models
served by llama.cpp through the `openai` embedder, scoped and corpus-wide, with paired intervals,
search and embedding latency, and the write-cost growth. Scoped R@5 is 98.0% for EmbeddingGemma 2
against 96.2% for bge-base.

### The headline, 20 August 2026

500 questions, session-as-document, scoped, embedder `all-MiniLM-L6-v2@q8`, depth 20.

| metric | lumberroom | agentmemory, published | delta |
|---|---|---|---|
| recall_any@5 | 93.2% | 95.2% | -2.0 |
| recall_any@10 | 96.0% | 98.6% | -2.6 |
| recall_any@20 | 98.4% | 99.4% | -1.0 |
| NDCG@10 | 83.0% | 87.9% | -4.9 |
| MRR | 83.3% | 88.2% | -4.9 |

By question type, against their published per-type table. The counts match theirs exactly, so it is
the same question set.

| type | n | lumberroom R@5 | theirs | delta |
|---|---|---|---|---|
| multi-session | 133 | 96.2% | 97.7% | -1.5 |
| temporal-reasoning | 133 | 91.7% | 95.5% | -3.8 |
| knowledge-update | 78 | 98.7% | 98.7% | 0.0 |
| single-session-user | 70 | 84.3% | 90.0% | -5.7 |
| single-session-assistant | 56 | 98.2% | 96.4% | **+1.8** |
| single-session-preference | 30 | 83.3% | 83.3% | 0.0 |

Two categories tie, one wins. The losses concentrate in `single-session-user` and
`temporal-reasoning`.

### Where the gap is

The ordering gap is more than twice the surfacing gap, and the rank distribution says why.

| where the first gold session lands | questions | share |
|---|---|---|
| rank 1 | 381 | 76.2% |
| rank 2 to 5 | 85 | 17.0% |
| rank 6 to 10 | 14 | 2.8% |
| rank 11 to 20 | 12 | 2.4% |
| never | 8 | 1.6% |

The right document already reaches the top twenty for 98.4% of questions. Perfectly reordering the
twenty rows already fetched would give 98.4% recall@5, which is 3.2 points above their published
figure. Candidate generation has 1.6% of headroom in total; the rest is ranking.

### The 2x2: chunking against rank fusion

Same 100 questions, recall@5.

| | linear | rrf |
|---|---|---|
| session-as-document | 88.0% | 90.0% |
| chunked | 96.0% | 96.0% |

They are substitutes rather than additive. Rank fusion is worth +2.0 when documents are long and
+0.0 once chunking has fixed the vector arm; both attack the same ordering headroom from opposite
sides. `recall@20` is 98.0% in all four cells, which is the third independent confirmation that
candidate generation never moves.

Full chunked figures, 100 questions: recall@5 96.0%, recall@10 98.0%, recall@20 98.0%,
NDCG@10 90.5%, MRR 89.7%. That beats the published 95.2 / 98.6 / 99.4 / 87.9 / 88.2 on three of
five, and it is **not comparable** to it: their harness stored one document per session, so the
honest comparison to their 95.2% is the session-as-document column at 88.0%.

Chunking is worth +8 recall@5 because the median rendered session runs 10,506 characters and the
embedder's window covers roughly the first 2,000. That finding applies to transcript ingestion,
which is where long text enters this system. It does not apply to the fact store, whose rows are
single sentences under an 8,000-character cap.

### Phase 7, and what rank fusion is worth at full scale

500 questions, session-as-document.

| metric | before phase 7 | phase 7, linear | phase 7, rrf |
|---|---|---|---|
| recall_any@5 | 93.2% | 93.2% | 93.6% |
| recall_any@10 | 96.0% | 96.0% | 96.0% |
| recall_any@20 | 98.4% | 98.4% | 98.4% |
| NDCG@10 | 83.0% | 83.0% | 83.6% |
| MRR | 83.3% | 83.3% | 84.5% |

Phase 7 reproduces the prior figures to the digit. That is a regression check passing rather than a
feature failing: the recency term ships at weight zero, an as-of read fires only when a caller asks
for one, and the benchmark records no aliases.

Rank fusion at full scale buys +1.2 MRR, +0.6 NDCG@10 and +0.4 recall@5, and nothing at 10 or 20.
The 100-question slice showed +2.0 recall@5, and that figure was flattered by a sample weighted
towards one question type. `SEARCH_FUSION` stays `linear` by default.

### The metric port is proven

`./scripts/eval-metrics-check.sh` recomputes agentmemory's own 500 stored per-question rows with our
three metric functions and reproduces their published recall_any@5, recall_any@10 and NDCG@10 to the
digit on both of their runs. recall_any@20 and MRR cannot be checked that way: those files store only
the first ten retrieved ids, so a gold session at rank 11 is invisible to any recomputation.

### What these numbers do not say

They measure ranking on synthetic chat. Supersession, valid time, policy, the registry and
provenance are where this project's value sits and none of them appears here. The stacks also
differ: their lexical side stems, expands synonyms and matches prefixes, while ours is Postgres
full text search with the `english` configuration and nothing more. Every run prints a deviations
block for this reason.

One figure to keep in perspective: agentmemory's published 95.2% comes from its vector-enabled
configuration. On the machine this was measured on, that install resolves no embedding provider, so
it runs the BM25-plus-graph path its own table puts at 86.2%.

---

## 2. HNSW recall

A different measurement: how much the approximate index misses against an exact scan. Full method
and numbers in [`research/recall-monitor.md`](research/recall-monitor.md).

On 40,001 seeded rows, mean recall@10 sat between 0.981 and 0.988 across five runs, with no true
nearest neighbour missed in 1,900 probes. The index loses nothing worth measuring at that size.

**Read the timings before the recall figure.** The monitor can compare an exact scan against an
exact scan and report 1.0. At `k=1` the planner declines the index and both arms run sequentially.
The tell is already printed: `index_ms` and `exact_ms` within a fraction of a percent of each other
means the number is a self-comparison, whatever it says.

---

## 3. Latency

Observed rather than targeted, from the LongMemEval runs.

| | |
|---|---|
| median search, 35,664 rows, chunked | 68ms |
| 500-question run, 23,867 writes | 1,023s |

Nothing here is a load test. The store these numbers were taken against holds tens of thousands of
rows because a benchmark put them there, and the owner's real store holds twenty.

---

## 4. What has never been measured

- Search latency under concurrency, at any size.
- Retrieval quality on the owner's own corpus. Every figure above is synthetic chat.
- Anything about ingestion quality. The extractor's precision and recall against a human reading the
  same transcripts is unmeasured, and the 222 proposals ingestion queued on 20 August 2026 (decision
  [`0008`](decisions/0008-valid-time.md)) are the only evidence there is.
- The console, end to end, in a browser.
