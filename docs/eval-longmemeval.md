# Running LongMemEval-S against lumberroom

## What this measures, and what it does not

[LongMemEval](https://arxiv.org/abs/2410.10813) (ICLR 2025) is a benchmark for long-term memory in
chat assistants. Its S variant is 500 questions, each with roughly 53 haystack sessions and a set of
gold session ids that answer it. The official metric is QA accuracy: retrieve, generate an answer,
score the answer with a GPT-4o judge.

This harness runs a different, smaller thing: does the store's search surface a gold session in its
top-k results, for k in 5, 10 and 20, plus NDCG@10 and MRR over the ranked list. No answer is
generated and no judge model runs. This is retrieval recall, not the official metric, and a reader
must not call the result a "LongMemEval score." A retrieval-recall number and a QA-accuracy number
answer different questions and are not interchangeable.

agentmemory publishes retrieval-recall numbers on the same dataset and the same protocol
(`recall_any@5` 95.2%, `recall_any@10` 98.6%, NDCG@10 87.9%, MRR 88.2%, on their BM25+vector
configuration). That is the number this harness sets out to sit beside, and the harness exists
because lumberroom had no retrieval number of its own that survived scrutiny.

## Fetching the dataset

The source is `xiaowu0162/longmemeval-cleaned` on Hugging Face, 265MB as a single JSON array of 500
questions:

```bash
pip install huggingface_hub
python3 -c "
from huggingface_hub import hf_hub_download
hf_hub_download(
    repo_id='xiaowu0162/longmemeval-cleaned',
    filename='longmemeval_s_cleaned.json',
    repo_type='dataset',
    local_dir='.',
)
"
```

Each question carries `question_id`, `question_type`, `question`, `question_date`,
`haystack_session_ids`, `haystack_sessions` (each an array of `{role, content}` turns),
`haystack_dates`, and `answer_session_ids`, the gold set. Inspect it with `python3` or `jq`; it is
too large to print whole in a shell command.

## Running the harness

The harness is a subcommand of `lumberroom`, the dependency-free client in `crates/lumberroom`, driven
against a live lumberroom server over its normal HTTP surface: `write`, then `search`, once per question.
It writes real rows into real Postgres through the real MCP path; nothing about it is simulated.

```bash
lumberroom eval \
  --dataset longmemeval_s_cleaned.json \
  --protocol session-as-document \
  --out report.json
```

`--limit N` stops after N questions, for a smoke run before committing to the full 500. `--resume`
skips a question whose namespace already holds rows, so a run interrupted partway can continue
without re-writing what already landed. `--skip-abstention` drops the 30 questions whose id ends in
`_abs`; the default keeps them in, because agentmemory's published run scored them too. `--json`
writes the machine-readable report in place of the printed table.

## Two protocols, one comparable and one not

**`session-as-document`** writes one memory per haystack session, the whole transcript as its
content. This is what agentmemory's own harness does, and it is the configuration whose number can
sit next to theirs.

**`chunked`** cuts each session into pieces sized for how lumberroom's chunker actually splits real
conversation, and writes each piece as its own row. This is closer to how the store is used day to
day, but a chunked run and a session-as-document run answer different questions about the same
data: chunking changes what a single retrieved row means, so a chunked recall number and
agentmemory's number are not comparable and must not be placed in the same table without saying so.

## What the eval server sets, and why

The harness targets a scratch server built for the run, never the owner's live deployment. Every one
of these is set on that scratch server:

- `SENSITIVITY_TRIPWIRE=false`. The tripwire refuses a write whose content looks like a credential.
  LongMemEval's synthetic chat sessions contain API-key-shaped and token-shaped strings by design,
  as part of what the benchmark tests memory over, and a tripwire built to catch exactly that shape
  would refuse haystack sessions for a reason that has nothing to do with retrieval. Refusing them
  would silently shrink the haystack the same way a write failure does, so the tripwire has to be
  off for a run whose write-failure count needs to mean what it says.
- `WRITE_MAX_CONTENT_CHARS` raised past the default 8000. A haystack session rendered whole can run
  longer than the default write ceiling; a ceiling that truncates a session mid-write is another way
  to shrink the haystack for a reason unrelated to ranking.
- `AUTH_MODE=token` with a single `AUTH_TOKENS` grant scoped to the `project:` namespace prefix the
  harness writes under. The eval has no need for OAuth, and a static token keeps the run's own
  authorization out of the variables being measured.
- `EMBED_MODEL=all-MiniLM-L6-v2`. See below.
- `SEARCH_DEBUG_SCORES=true`. Each hit carries its cosine, keyword score and fused score, which the
  harness copies into the per-hit log. The setting runs a sibling statement that adds columns to the
  final select list and changes nothing above it, so the order is the order a deployment gets.
  `tests/search_minmax.rs` compares the two orders on a real Postgres; that file has not run yet.

## Reusing one corpus across blends

A fusion sweep changes how search combines its two arms and nothing about what is stored. Writing
the corpus again for each blend embeds every session again, so the script writes each corpus once
and reruns only the searches.

Write once, per model and per mode, and keep the database:

```bash
./scripts/eval-longmemeval.sh --db lme_gemma_scoped --keep --out gemma-scoped-linear035.json
./scripts/eval-longmemeval.sh --db lme_gemma_corpus --corpus-wide --keep \
  --out gemma-corpus-linear035.json
```

Then search the kept database once per blend:

```bash
./scripts/eval-longmemeval.sh --db lme_gemma_scoped --search-only --lexical-weight 0.2 \
  --out gemma-scoped-linear020.json
./scripts/eval-longmemeval.sh --db lme_gemma_scoped --search-only --fusion linear_minmax \
  --out gemma-scoped-minmax035.json
```

Each run starts a fresh scratch server with that run's `SEARCH_*` settings: `--fusion` sets
`SEARCH_FUSION` (`linear`, `linear_minmax` or `rrf`), `--lexical-weight` sets
`SEARCH_LEXICAL_WEIGHT` and `--rrf-k` sets `SEARCH_RRF_K`. Unset, each keeps the server default:
linear, 0.35 and 60. The script reads them from its flags alone and never from `.env`.

`--search-only` writes no memory and deletes none. It refuses a database that does not exist, refuses
`--isolate` (which deletes each haystack), refuses `--resume`, and stops before the first search if
any namespace it needs holds no rows. Scoped mode needs every `project:lme-qNNNN` namespace the
writing run filled, so pass the same `--limit`, `--type` and `--protocol`. The database survives the
run whatever `--keep` says.

Two things differ from a writing run, and the report carries both:

- **The access counters reset first.** Every search raises `access_count` on the rows it returns,
  and the use boost (`SEARCH_USAGE_WEIGHT`, 0.05) reads it. Without a reset each blend would rank
  against the reads of every blend before it. The script sets `access_count` to 0 and
  `last_accessed_at` to NULL on every row before the server starts, which is where the writing run's
  rows stood when it searched them.
- **The map from rows to sessions comes from the store.** A writing run records which session each
  write landed on from the id the server returned. A search-only run reads every row through
  `/admin/export` and matches it by the session id the harness put in its tags, then by exact text.
  A session whose write collapsed into a near-identical row matches neither and counts in
  `sessions_never_stored`, where the writing run scored it through the other row. A nonzero count in a
  search-only report against a zero in the writing run's report is that difference, and it is the
  same in every blend run from one corpus.

## The per-hit log

A run given `--out` writes one JSON line per question beside the report: `report.json` gets
`report.hits.jsonl`. A run with no `--out` writes neither file. Lines are flushed as they are
written, so an interrupted run keeps the questions it finished.

```json
{"question_id": "e47becba", "question_type": "single-session-user",
 "gold_session_ids": ["answer_280352e9"],
 "hits": [
   {"rank": 1, "memory_id": "9f1c...", "session_ids": ["answer_280352e9"], "score": 1.0319,
    "cosine": 0.8123, "keyword": 0.0911, "fused": 1.031885, "cosine_norm": 1.0, "gold": true}
 ]}
```

`rank` is the hit's position in the store's answer, before hits fold into sessions. `score` is the
rounded score `memory_search` always returns. `cosine`, `keyword` and `fused` are the unrounded
parts, null when the server ran without `SEARCH_DEBUG_SCORES`. A row only the keyword arm found
has `cosine` 0, and a row only the vector arm found has `keyword` 0. `cosine_norm` appears under
`linear_minmax`, and `vector_rank` and `keyword_rank` under `rrf`. `gold` is true when any session
the hit maps to is in the gold set; a hit that maps to no session lists none and is never gold.

## The min-max blend

`SEARCH_FUSION=linear_minmax` scores `cosine_norm * SEARCH_VECTOR_WEIGHT + keyword *
SEARCH_LEXICAL_WEIGHT`, plus the use boost and namespace penalty the linear blend applies.
`cosine_norm` rescales each query's candidate cosines to 0..1: the best becomes 1 and the worst 0.
The candidates are the rows the linear statement already scores, the union of the two arms. Only
rows the vector arm returned set the minimum and maximum; a row the keyword arm alone found scores 0,
as it scores a cosine of 0 under the linear blend. A pool whose cosines are all equal, one row
included, scores 1 rather than dividing by zero.

The reason to try it: an embedder that packs neighbours into a narrow band (EmbeddingGemma 2's
neighbour median sits at 0.807 against bge-base's 0.745) leaves the cosine term spread over a few
hundredths, so a keyword weight tuned on one model means something else on another. No run has
measured it yet.

## What is being compared, and what is not

**Matched:** the embedder. `all-MiniLM-L6-v2` is agentmemory's model, run here at its native 384
dimensions and zero-padded into lumberroom's 768-dimension pgvector column, which leaves cosine similarity
unchanged. A retrieval comparison run on a different embedder measures the embedder, not the search
stack, so this is the one variable held constant on purpose.

**Not matched, and this is most of what the number means:**

- agentmemory's published run scored BM25 plus brute-force cosine, fused by reciprocal rank fusion.
  This harness runs lumberroom's real search: Postgres full-text search plus HNSW, blended by a weighted
  sum.
- Their lexical side stems, expands synonyms, and matches prefixes. Postgres FTS does none of that
  beyond the `english` text search configuration's own stemming.
- Their harness embedded only the first 512 characters of a session. bge and MiniLM both cut input
  at 512 tokens, a different bound and usually a longer one, so lumberroom's embedder sees more of each
  session than theirs did.
- Their harness replaced the store with an in-process map and built a fresh index per question. This
  harness writes through the real HTTP path into real Postgres, session by session, question by
  question.

A higher or lower number than agentmemory's therefore says something about the whole stack these
harnesses each actually ran, not about the embedder alone or about ranking quality in isolation.

## Reading the report

The report carries an overall `Aggregate` (recall@5, @10, @20, NDCG@10, MRR across all scored
questions), a per-question-type breakdown in the same shape, and two counts that qualify every other
number in the file: `questions_with_write_failures` and `sessions_never_stored`.

**The one line that invalidates a run:** a non-zero `sessions_never_stored`. A session that was
never written cannot be retrieved, so a question whose gold session sits in that count is scored as
a retrieval miss for a reason that has nothing to do with search quality. A run reporting a nonzero
count here has measured the write path's reliability at least as much as the search path's, and the
retrieval numbers in that report should not be read as clean.

## What has been run

Nothing, end to end, as of this writing. `dataset::load`, `corpus::build`, `runner::run`, and
`report::print` are locked signatures with unimplemented bodies; the metric functions in
`crates/lumberroom/src/eval/mod.rs` are the only part of the harness that has run, and they are
checked against agentmemory's own checked-in per-question results, reproducing their published
`recall_any@5`, `recall_any@10` and NDCG@10 to the digit. "Implemented" here means the contract in
`eval/mod.rs` and this document describe a harness that has not yet produced a report. The gate that
settles it is one full run of `lumberroom eval` against a live scratch server with
`sessions_never_stored` at zero, whose `report.json` is the first legitimate number to put beside
agentmemory's table.
