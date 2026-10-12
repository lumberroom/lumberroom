# Agent replay gate

The gate answers one question: does an agent make the same calls and the same decisions on the new
MCP answers as on the old ones, within the noise the old ones already carry. It drives headless
Claude Code against a base build and a branch build over the owner's own memory, then scores the
runs. The design is `docs/superpowers/specs/2026-10-12-mcp-response-tokens.md`, section 9 (G1 and
G2). This page covers the tooling in `scripts/eval/agent-replay/`.

Everything runs on legolas, on the owner's Claude subscription login. No script calls the Messages
API or any other model provider. Standard library Python 3 only, plus Node for one serialization
check. Run records live under `~/lumberroom-gate/2026-10-mcp-response-tokens/`, never under
`~/.cache/agent-scratch`, so a batch-end cleanup cannot delete them.

Every Python script starts with `python3 -I`. Isolated mode drops the script directory from the
import path, so each script puts its own directory back at the head of `sys.path`.

## Files

| File | Job |
|---|---|
| `slot.sh` | One slot: database, arm server, optional strip proxy. Reset, template and teardown commands |
| `session.py` | One `claude -p` session, one run record. Holds the pure command builder and the `init` check |
| `outcomes.py` | Stream-json parsing, the database diff and per-item outcomes |
| `batch.py` | Expands a stage into jobs, resumes, interleaves arms, caps reruns |
| `strip_proxy.py` | The text channel: deletes `result.structuredContent`, changes nothing else |
| `probe.py` | The two probe questions, before batch 1 |
| `retrieval_diff.py` | G1 build replay, the consumer check and `freeze` for the frozen queries |
| `consumer_check.mjs` | The `JSON.stringify(structured ?? {})` half of the consumer check |
| `compare.py` | The seven G2 rules and the report |
| `judge.py` | Judge sessions through `claude -p` |
| `seed_review.py` | Empties the real queue and seeds the review fixtures |
| `diff_archives.py` | Row-by-row diff of two plaintext archive exports (the engine template check) |
| `fill_ids.py` | Replaces `@id:<seed key>` tokens in a task file with server ids |
| `fixtures/make_review_fixture.py` | Regenerates `fixtures/review.archive.json` |
| `tasks.example.jsonl` | 22 tasks against `scripts/seed/seed.py`'s fixture |

Tests: `python3 -I -m pytest scripts/eval/agent-replay -q`. They need `pytest` and no network,
database or model.

## The session command

`session.build_argv` is a pure function and `test_session.py` pins its exact output. The command:

```
env -u ANTHROPIC_API_KEY ~/.local/bin/claude -p "<prompt>" \
  --setting-sources project --strict-mcp-config --mcp-config <slot>/mcp.json \
  --tools "" --allowedTools "mcp__lumberroom__context_bootstrap,..." \
  --no-session-persistence --no-chrome --disable-slash-commands --include-hook-events \
  --output-format stream-json --verbose --model <model>
```

It runs in a fresh `mktemp -d /tmp/lr-gate.XXXXXX` directory, so no project `CLAUDE.md` is in
reach. A probe on 12 October 2026 (Claude Code 2.1.296) showed this command runs on the existing
login with `apiKeySource: "none"`, loads only the builtin plugins, and fires no hooks. The
runner uses no `--bare` (it never reads OAuth), no separate `CLAUDE_CONFIG_DIR` and no
`setup-token`.

`session.check_init` rejects a session, as `incomplete` with reason `bad_init`, when its `init`
event shows any of:

- an `apiKeySource` other than `"none"`;
- an MCP server other than the slot's;
- a tool that does not start with `mcp__lumberroom__`;
- a plugin outside `cc-plugin-agents-md`, `cc-plugin-telemetry`, `cc-plugin-plugin-authoring`.

`check_event` also rejects a hook event and any tool use outside the slot's server.

`--setting-sources project` may still load `~/.claude/CLAUDE.md`. The probe reports whether it
does. If it does, the gate accepts it: every session of every arm loads the same file, so it
cannot separate the arms.

## Slots

One slot per arm and channel in use: A0, A1 and (later) A2, each on `structured` and `text`.
A0b shares A0's slot, because it is a second set of sessions on the same build.

```
slot.sh up --dir D --name A1-structured --image IMG --checkout DIR --port 8841 --client gate-A1 \
           --settings-sql settings.sql
slot.sh up --dir D2 --name A1-text --channel text --proxy-port 8941 ...
```

The network follows the spec. The database sits only on a per-slot `--internal` network. Docker
will not publish a port from a container attached only to an internal network, so the arm server
is created on the default bridge with `-p 127.0.0.1:PORT:8787`, joins the internal network, and
then starts. The environment holds the gate token and the database URL and no Razorpay, SMTP, LLM,
GitHub or OpenRouter credential. The bridge leg gives the server the egress it needs to fetch its
local embedding model on first boot. `slot.env` and `mcp.json` are mode 600.

`slot.sh up` sets `SEARCH_USAGE_WEIGHT=0` for every arm, as G1 requires: a search bumps
`access_count`, and with the usage weight on, the order of two builds' bumps would move later
rankings. The gate grant reads and writes `*` at `open` and carries `registryWrite`, `mayDelete`
and `mayReadHistory`.

The live database is named `lumberroom`, as in production, so a `settings.sql` written for that
name applies unchanged. The template is `lrgate_template`.

| Command | Does |
|---|---|
| `load-template D DUMP` | `pg_restore` a custom-format dump into the template |
| `freeze-template D` | Copy the live database into the template (the engine path) |
| `reset D` | Stop the server, drop the live database, clone the template, reapply `settings.sql`, start |
| `psql D SQL` | Run SQL against the live database |
| `down D` | Remove containers, volumes, network and proxy |

`reset` runs two separate `-c` strings for the drop and the create, because Postgres rejects
`DROP DATABASE` inside a transaction block. A clone loses `ALTER DATABASE ... SET`, so `reset`
reapplies `settings.sql`; without the hnsw settings a filtered search returns no rows.

The text channel starts `strip_proxy.py` on `--proxy-port` and points `mcp.json` at it. The
engine declares no `outputSchema`, so a client that validates structured output has nothing to
object to once the field is gone.

## Corpus and templates

The lead checks the sha256 prefix of the corpus dump before the first restore and deletes the
corpus when the gate ends (spec G2, Corpus).

**Fork template.** `slot.sh load-template`, then `reset`, then the fork-specific steps in fork
plan F6 (delete every tenant except the owner's, seed the synthetic teams tenant).

**Engine template.** A fork build exports the owner's tenant with `lumberroom archive export`.
An engine build imports it with `lumberroom archive import --restore` into an empty store.
Export the engine store again and run `diff_archives.py fork-export engine-export`. It compares
id, namespace, content, tags, created_at, occurred_at, occurred_until and supersedes, and exits 1
on any difference. A difference stops the gate. Both exports must be plaintext
(`--allow-plaintext`).

### Emptying the queue and seeding the fixtures

A restore refuses a store that holds any memory row (`src/services/archive.rs:539`), and a merge
import mints new ids and ignores the archive's `created_at`. The spec's order (empty the queue,
then restore the fixtures) cannot run as written. `seed_review.py` has two paths:

- *Engine path.* Run `seed_review.py inject --in fork-export.lumber --out seeded.lumber` before the
  restore. One `--restore` then brings in the owner's rows and the fixtures, with the fixtures'
  ids and old `created_at` values intact. After the import, boot the server and run
  `seed_review.py empty`.
- *Populated store (the fork).* `seed_review.py backdate` merge-imports the fixtures, rewrites
  `created_at` in SQL, clears the conflict-pair marks for those rows so the sweeper rescans them
  in the corrected order, and writes `--keys-out map.json`. Merge mints new ids, so run
  `seed_review.py remap-tasks --keys-map map.json --in tasks.jsonl --out tasks.live.jsonl`
  and use the result.

`empty` waits for `conflicts_pending` to read 0, applies `keep_both` to every real conflict pair,
sets `last_confirmed_at = now()` on every real stale row, and with `--fork` dismisses every real
proposal. It never touches an item in `project:gate-review`. `check` asserts the queue holds only
`project:gate-review` items and that every fixture key is present. Run `empty` and `check`, then
`slot.sh freeze-template`.

The fixture holds 4 review tasks of 16 keys each (6 conflict pairs and 10 stale rows), 100 rows in
all. Each pair and each stale row has its own invented service. Rows about one service score as
near-duplicates, so sharing a subject would create conflict pairs nobody listed. The pairs cover
the shapes `lr-review` has a rule for: a fuller row, dated contradictions, dated snapshots, a
tainted copy that carries an instruction, an unordered contradiction and a merge. The stale rows
are over a year old at 12 October 2026. A gate run in 2027 or later needs the dates moved.

Measured on 12 October 2026 against the engine build `lumberroom-server:0.1.0-dev` with its
default local embedder: the sweeper recorded all 24 fixture pairs and no other pair, and the stale
source listed all 40 fixture stale rows. A different embedder or conflict threshold can move a
pair across the line. After seeding, count the queue per task and reword a pair that is missing.

## The probe

`probe.py` runs before batch 1: two sessions per channel against A0 slots.

1. Asks the model to list every instruction it was given. The report states whether
   `~/.claude/CLAUDE.md` text appeared.
2. Asks it to call `review_queue` for one stale row and quote the row text, or reply `NO TEXT`.
   On the base build the structured payload carries no row text and the text block does. A
   structured session that cannot quote shows Claude Code feeds the model `structuredContent`. A
   text session through the proxy that can quote shows the proxy works. Both must hold or the
   probe exits 1.

The record lands in `probe.json` under the gate root.

## Batches

```
batch.py expand --tasks tasks.jsonl --stage sonnet --model claude-sonnet-5-5 --out sonnet.jsonl
batch.py run --batch sonnet.jsonl --tasks tasks.jsonl --limit 40 --concurrency 3
```

`expand` writes the spec's job counts for the example task file: 264 structured and 162 text
sessions for the Sonnet stage, 99 for the Opus stage (decision tasks, structured channel, 3
repeats). Sonnet runs 5 repeats per decision task and 3 per other task, on the channels each task
lists.

`run` resets the slot from its template before every session and keeps one session per slot at a
time. Across slots it runs `--concurrency` sessions together. It interleaves the arms over the
same task and repeat, so a model or quota change between windows lands on every arm alike.

- *Resume.* A job with a complete record is skipped. A record appears only once it is whole.
- *Usage limit.* The first `usage_limit` stops the queue. Sessions already running finish. Rerun
  the same command in the next window.
- *Incomplete runs.* A session that ends on a usage limit, a refusal, an MCP error, a rejected
  `init`, a 20-minute timeout or output it cannot parse is recorded as `incomplete` with the
  reason and kept out of every score. It reruns up to three times across batches. After that the
  batch prints `GIVEN UP` with the job name, and the run never counts as agreement.

A run record follows the plan's shape. Additions: `attempts`, `finished_at`, `ignored_rows` (new
rows another client wrote) and `ok` on each call. `usage.input` is input plus cache-creation
tokens, and `usage.cache_read` is separate.

An outcome is the verdict or write the agent made, `unseen` (never received the key and nothing
touched its rows), `resolved_elsewhere` (never decided, but another verdict retired or rewrote its
rows) or `left` (received and untouched). Only changes by the gate token's client count.

A vanished memory row counts as `deleted`. The table keeps no deleter, and the slot runs no
cleanup job.

## G1, no model

```
retrieval_diff.py freeze --slot D --count 100 --out frozen-queries.jsonl
retrieval_diff.py run --base-slot D1 --branch-slot D2 --cli-fixture client/eval-fixture.example.jsonl \
    --frozen frozen-queries.jsonl --branch-checkout <engine checkout> --reset --out g1.md
```

`freeze` writes 100 queries from the live corpus with a fixed seed, once. Commit the file so later
runs reuse it. `run` sends both builds the same requests in the same order: each query bare, with
`project` and with `namespaces`; `context_bootstrap` per project (text byte for byte, plus the
count and bytes of `left_out` on the branch); `memory_history` for up to 50 ids that have
predecessors; `review_queue` pages 0 to 3 per source and the brief default page. Then it runs the
consumer check (Python `json.dumps` and Node `JSON.stringify` round trips against each consumer's
fields) and `scripts/policy-test.sh` from the branch checkout. It exits 1 on any difference. A
tool either build refuses is reported as a difference. `--n-sources` is 2 for the engine and 3 for
the fork.

## Reading the report

`compare.py --root <gate root> --tasks tasks.jsonl [--arm A1] [--base A0] [--out report.md]`
prints one section per stage and channel, with a row per rule and per task group, the failures,
notes, and a token table (input plus cache read, and output, mean per run). It exits 1 when a rule
fails. Score A2 with `--arm A2 --base A1`. A0b stays the noise floor.

| Rule | Reads as |
|---|---|
| 1 | Per task group, A1's agreement with A0's mode reaches A0b's agreement minus 5 points. An item also fails alone when A0 and A0b each reach one outcome in 80 percent of runs and A1 reaches it in 40 percent or fewer |
| 2 | Per review task, A1's `unseen` keys per run stay at or below A0b's |
| 3 | Per task, A1's mean calls stay within one call, 20 percent of A0, or the A0 to A0b spread. Tool-share moves past that spread plus 20 points print as notes and fail nothing |
| 4 | The judge's equivalence rate for A1 reaches A0b's minus 5 points |
| 5 | On needs-more tasks, A1's answers carry at least as many `required_facts` as A0b's |
| 6 | Any A1 or A2 run that wrote content containing `----- data ` fails the gate |
| 7 | Per task group, A1's mean answer length stays under A0's plus the A0 to A0b spread plus 10 percent of A0 |

The spec does not define "spread". The code reads it as the absolute difference between A0's mean
and A0b's mean for the quantity under test. Rule 5 checks the final answer for the required
fact text, because the record keeps the answer and not each tool result. A text-channel run
scores against A0's text-channel runs only.

`judge.py --root <gate root> --tasks tasks.jsonl --stage sonnet --channel structured` runs one
`claude -p --model claude-opus-5-5` session per task and arm (A0b and A1 against A0), with no MCP
server and no tools. The prompt holds the task, A0's answers and the answers under test, and asks
for `{"equivalent": [...], "reasons": [...]}` on one line. A reply that does not parse reruns
once, then the verdict records as incomplete and the report says so.

## What the gate cannot show

- claude.ai's, ChatGPT's, Hermes's or OpenClaw's own system prompts. The text channel feeds Claude
  Code the payload those clients read. G1's consumer check covers the fields Hermes and OpenClaw
  parse.
- Drift rarer than its sample. With 5 repeats per decision task, a change that alters one decision
  in twenty runs can pass.
- Private rows, undated-source paging, long sessions, other task mixes.

## Status of this tooling

Run on a developer machine on 12 October 2026: the pytest suite; `slot.sh up`, `reset`,
`freeze-template`, `psql` and `down` against the engine image `lumberroom-server:0.1.0-dev`;
`strip_proxy.py` against that server; `retrieval_diff.py` between two copies of one store;
`seed_review.py backdate`, `empty`, `check` and `remap-tasks`; the restore of an injected archive.
Not run: any `claude -p` session, `probe.py`, `judge.py`, `batch.py run`, `seed_review.py empty
--fork`, and the policy-test step of `retrieval_diff.py`. Those need legolas and the owner's login.
