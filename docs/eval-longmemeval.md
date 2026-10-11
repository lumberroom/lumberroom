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
- `WRITE_MAX_CONTENT_CHARS=200000`, far past the default 2000 (8000 before decision 0030). A haystack
  session rendered whole runs longer than the default write ceiling; a ceiling that refuses a session
  is another way to shrink the haystack for a reason unrelated to ranking.
- `AUTH_MODE=token` with a single `AUTH_TOKENS` grant scoped to the `project:` namespace prefix the
  harness writes under. The eval has no need for OAuth, and a static token keeps the run's own
  authorization out of the variables being measured.
- `EMBED_MODEL=all-MiniLM-L6-v2`. See below.

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
