# Agent replay gate

The gate answers one question: does an agent make the same calls and the same decisions on the new
MCP answers as on the old ones, within the noise the old ones already carry. It drives headless
Claude Code against a base build and a branch build over a restored copy of a real store you
are allowed to use, then scores the runs. This page covers the tooling in
`scripts/eval/agent-replay/`.

Everything runs on the gate host, on a Claude subscription login. No script calls the Messages
API or any other model provider. Standard library Python 3 only, plus Node for one serialization
check.

## Where output lives

Run output holds store content: row text in transcripts, answers and snapshots, corpus queries in
`frozen-queries.jsonl`, hit lists in `build-replay.md`, the corpus itself in every dump and archive. **Never
commit any of it.** Permission to use the corpus never covers publishing it.

`LR_GATE_ROOT` holds all of it: slots, dumps, run records, transcripts, judge verdicts,
`probe.json`, `frozen-queries.jsonl` and `build-replay.md`. It defaults to `~/.local/share/lumberroom-gate`,
outside any checkout, because a default relative to the working directory would land store content
in the repository whenever a script runs from its root. Keep it outside any scratch directory a
batch-end cleanup deletes. `scripts/eval/agent-replay/.gitignore` catches the usual file names as a
backstop for a file written there by hand.

`LR_GATE_CLAUDE` is the Claude Code binary (default `claude`, found on `PATH`). Set it when the
binary is not on `PATH` for non-login shells.

`LR_GATE_EXTRA_TOOLS` declares tools a server registers beyond the engine's twelve, as bare names
separated by commas: `LR_GATE_EXTRA_TOOLS=tool_a,tool_b`. Every session in the run allows them in
`--allowedTools` and `check_init` expects them in `init`. Set it in the environment of every
harness command: `batch.py`, `session.py` and `probe.py` run sessions with it, and `batch.py
expand`, `compare.py` and `judge.py` refuse a task file that names a tool it does not declare. It
applies to every arm in the run, so arms whose builds register different tools run in separate
batches.

`LR_GATE_TOOLS` replaces the engine's twelve with a complete list, in the same form:
`LR_GATE_TOOLS=tool_a,tool_b`. Use it when a server does not expose every engine tool, for example
a principal granted some engine tools and others of its own. `LR_GATE_EXTRA_TOOLS` cannot remove a
tool, and `check_init` rejects a session whose server lacks one it expects. Extras still add on top
of whichever list applies. With `LR_GATE_TOOLS` set, an engine tool missing from it counts as
undeclared, and a session whose `init` lists it is rejected.

Neither variable reaches the session itself, which inherits only the allowlist below. A name with
a server prefix or a double underscore, or an `LR_GATE_TOOLS` that is set but names no tool, stops
`batch.py run` and `expand` before they start, `compare.py` and `judge.py` before they read
records, and `probe.py` before it reads a slot. `batch.py`, `compare.py` and `probe.py` exit 2.

Every Python script starts with `python3 -I`. Isolated mode drops the script directory from the
import path, so each script puts its own directory back at the head of `sys.path`.

## Files

| File | Job |
|---|---|
| `slot.sh` | One slot: database, arm server, optional strip proxy. Templates, dump, reset and teardown |
| `session.py` | One `claude -p` session, one run record. Holds the pure command builder and the `init` check |
| `outcomes.py` | Stream-json parsing, the database diff and per-item outcomes |
| `batch.py` | Expands a stage into jobs, resumes, interleaves arms, caps reruns, checks slots and stops on rate limit or low disk |
| `strip_proxy.py` | The text channel: deletes `result.structuredContent`, changes nothing else |
| `probe.py` | The `tools/list` size declaration check and the two probe questions, before batch 1 |
| `retrieval_diff.py` | The build replay with no model, the consumer check, and `freeze` for the frozen queries |
| `consumer_check.mjs` | The `JSON.stringify(structured ?? {})` half of the consumer check |
| `compare.py` | The eight pass rules and the report |
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
env -i HOME=... PATH=... USER=... LANG=... LC_ALL=... TMPDIR=... TERM=... \
  CLAUDE_CODE_DISABLE_AUTO_MEMORY=1 "$LR_GATE_CLAUDE" -p "<prompt>" \
  --setting-sources project --strict-mcp-config --mcp-config <slot>/mcp.json \
  --tools "" --allowedTools "mcp__lumberroom__context_bootstrap,..." \
  --no-session-persistence --no-chrome --disable-slash-commands --include-hook-events \
  --output-format stream-json --verbose --model <model>
```

`env -i` empties the environment and passes back only the seven variables shown, each when it is
set. Claude Code reads `ANTHROPIC_*`, `CLAUDE_CODE_USE_BEDROCK` and `CLAUDE_CODE_USE_VERTEX` to pick
an API key or a cloud provider over the subscription login, and an allowlist cannot miss the next
such variable the way a deny list can. The judge runs the same way.

`CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` is set on every session whatever the caller's environment
holds. Auto memory is on by default in Claude Code 2.1.296. In a probe on 12 October 2026, a
session without the variable carried auto memory's instructions in its system prompt,
`memory_paths.auto` in its `init` event and a new directory under
`~/.claude/projects/-tmp-lr-gate-*`. With the variable, the documented switch
(code.claude.com/docs/en/memory), all three were gone.

The session runs in a fresh `mktemp -d /tmp/lr-gate.XXXXXX` directory, so no project `CLAUDE.md` is
in reach. The runner uses no `--bare` (it never reads OAuth), no separate `CLAUDE_CONFIG_DIR` and
no `setup-token`.

`session.check_init` rejects a session, as `incomplete` with reason `bad_init`, when its `init`
event shows any of:

- an `apiKeySource` other than `"none"`;
- an MCP server other than the slot's, or the slot's server with a status other than `connected`
  (a server that failed lists no tools, and the session would answer from nothing);
- `memory_paths.auto`, which means auto memory is on;
- a tool that does not start with `mcp__lumberroom__`;
- a lumberroom tool outside the base list (the twelve, or `LR_GATE_TOOLS`) and the extras
  `LR_GATE_EXTRA_TOOLS` declares;
- a tool from the base list or a declared extra missing from `tools`;
- a plugin outside `cc-plugin-agents-md`, `cc-plugin-telemetry`, `cc-plugin-plugin-authoring`.

`check_event` also rejects a hook event and any tool use outside the slot's server. On the text
channel, a tool result that carries a `structuredContent` key anywhere in its event marks the run
incomplete with reason `structured_on_text`: the proxy failed for that session, and the run would
measure the structured channel under the text channel's name.

A tool answer Claude Code saved to a file leaves a signature in the transcript. The tool result
for the `mcp__lumberroom__*` call holds a short stub with a path under `tool-results/` in place of
the answer, and the model then calls the built-in `Read` on that path. `check_event` rejects the
`Read` with "tool use 'Read' is outside the slot's server", so the run records as `bad_init`.
`outcomes.parse_stream` sets `file_stubbed` when it sees either half, the run record carries it, and
the report counts stubbed sessions per arm. Claude Code writes the saved file under
`~/.claude/projects/` even with `--no-session-persistence`, outside `LR_GATE_ROOT`, and it holds
store content: delete those files with the rest of the run output.

`--setting-sources project` does not load `~/.claude/CLAUDE.md`: in the probe on the gate host on
12 October 2026, neither instruction listing repeated a line of it. Had it loaded, the gate would
accept it, because every session of every arm would load the same file.

### What has been seen live

Six probe sessions ran on the gate host on 12 October 2026 with Claude Code 2.1.296, through
`session.run_session` as it stood before the `env -i` allowlist and the full-tool-list check: the
command used `env -u ANTHROPIC_API_KEY` plus `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`. Seen there:

- `init` lists each MCP server with `name`, `status` and `source`, and each plugin with `name`,
  `path` and `source`. With auto memory off, `memory_paths` is empty. `test_session.py` pins
  that shape.
- On the structured channel a `tool_result`'s `content` is one string, the `structuredContent`
  as JSON. On the text channel it is a list of text blocks. The stream also carries
  `system/thinking_tokens` and `rate_limit_event`, which name no tool. `test_outcomes.py` pins
  both shapes.
- `apiKeySource: "none"`, only the builtin plugins, and no hook events.

Not seen live: the full-tool-list check in `check_init`, and whether the subscription login
survives `env -i`. `probe.py` itself, `batch.py run` and `judge.py` have not run.

## Slots

One slot per arm and channel in use: A0, A1 and (later) A2, each on `structured` and `text`. A2 is
a later branch build, scored against A1 with its own A1b noise arm. By default a
noise arm shares its build's slot: A0b runs on A0's slots, A1b on A1's. `batch.py` finds a slot at
`$LR_GATE_ROOT/slots/<slot>-<channel>`, where `<slot>` is the arm's name unless `--slot-of ARM=SLOT`
says otherwise. The default is `A0b=A0 A1b=A1`. An arm with a build of its own gets its own slots
with `--slot-of A1b=A1b`, and the slot check refuses a batch whose slot directory is missing, so a
wrong mapping stops before any session runs.

```
slot.sh up --dir "$LR_GATE_ROOT/slots/A1-structured" --name A1-structured --image IMG \
           --checkout DIR --port 8841 --client gate-A1 --settings-sql settings.sql
slot.sh up --dir "$LR_GATE_ROOT/slots/A1-text" --name A1-text --channel text --proxy-port 8941 ...
```

The database sits only on a per-slot `--internal` network, so it cannot reach out and nothing
outside can reach it. Docker will not publish a port from a container attached only to an internal
network, so the arm server is created on the default bridge with `-p 127.0.0.1:PORT:8787`, joins
the internal network, and then starts. The environment holds the gate token and the database URL
and no billing, SMTP, LLM, GitHub or OpenRouter credential. The bridge leg gives the server the
egress it needs to fetch its local embedding model on first boot. `slot.env` and `mcp.json` are
mode 600.

`slot.sh up` sets `SEARCH_USAGE_WEIGHT=0` for every arm: a search bumps `access_count`, and with
the usage weight on, the order of two builds' bumps would move later rankings and the build replay
would report differences neither build caused. The gate grant reads and writes `*` at `open` and
carries `registryWrite`, `mayDelete` and `mayReadHistory`.

`--archive-client NAME` adds a second grant that reads `*` at `sealed` and carries
`mayReadHistory`. `archive export` refuses a narrower grant. Its token goes to `archive.token` in
the slot directory, mode 600, and never into `mcp.json`, so no session holds it. Use it for
`archive export` alone: `LUMBERROOM_TOKEN="$(cat D/archive.token)" lumberroom archive export ...`.
`--extra-env FILE` appends `KEY=VALUE` lines to `slot.env`. A multi-tenant build reads
`TENANT_ID=<tenant under test>` from it to bind the static tokens to that tenant.

The live database is named `lumberroom`, so a `settings.sql` written for that name applies
unchanged. A slot holds several templates: the default `lrgate_template`, and one named template
per review task, `lrgate_template_review_1` and so on.

| Command | Does |
|---|---|
| `load-template D DUMP [NAME]` | `pg_restore` a custom-format dump into a template, after creating the roles in `LR_GATE_RESTORE_ROLES` and skipping a `SCHEMA public` entry |
| `freeze-template D [NAME]` | Copy the live database into a template |
| `dump D OUT` | `pg_dump` the live database to `OUT` on the host, mode 600 |
| `wipe D` | Drop the live database, create it empty, restart the server so it migrates |
| `reset D [NAME]` | Stop the server, drop the live database, clone a template, reapply `settings.sql`, start |
| `psql D SQL` | Run SQL against the live database |
| `down D` | Remove containers, volumes, network and proxy |

`reset` runs two separate `-c` strings for the drop and the create, because Postgres rejects
`DROP DATABASE` inside a transaction block. A clone loses `ALTER DATABASE ... SET`, so `reset`
reapplies `settings.sql` to the live database; without the hnsw settings a filtered search
returns no rows. A `settings.sql` that names its target with `current_database()` therefore sets
the database the server reads.

A dump from another build can need two things at restore. Its row policies may name roles this
engine never creates; list them in `LR_GATE_RESTORE_ROLES` (space-separated) and `load-template`
creates each one NOLOGIN before restoring. It may also carry its own `CREATE SCHEMA public`, which fails on the schema every new
database has, so `load-template` lists the dump with `pg_restore -l`, drops that one TOC entry and
restores the rest with `-L`.

The text channel starts `strip_proxy.py` on `--proxy-port` and points `mcp.json` at it. The
engine declares no `outputSchema`, so a client that validates structured output has nothing to
object to once the field is gone.

## Corpus and templates

The corpus is a restored copy of a real store you are allowed to use, kept on the gate host under
`LR_GATE_ROOT`. It never enters a repository. Check the sha256 prefix of the corpus dump before the
first restore and delete the corpus, every dump and every run record when the gate ends.

### One template per review task

Each review task runs on a template that holds the corpus and that task's fixture rows only. A
task's prompt says "Work my review queue", and `review_queue` takes no namespace filter, so one
template holding all four tasks' rows would put 64 keys in front of every review session where
each task lists 16. A template per task was the simpler of the two designs that fix it: a
namespace per task would need a prompt that names the namespace, and the agent could still list
the whole queue. Every task line in the review group carries `"template": "review-N"`, and
`batch.py` resets the slot from that template before the session. Every other task, and the build
replay, uses the default template, which holds all four tasks' rows.

So each slot needs five templates: the default and `review-1` to `review-4`. Build each one once,
on one slot, and carry it to the rest with `slot.sh dump` and `slot.sh load-template`. Every slot
then holds the same bytes, so the same fixture ids and the same queue keys. Building a template on
each slot separately would mint different ids on the fork path.

### Emptying the queue and seeding the fixtures

A restore refuses a store that holds any memory row (`src/services/archive.rs:539`), and a merge
import mints new ids and ignores the archive's `created_at`. Emptying the real queue first and then
restoring the fixtures cannot work as stated, so `seed_review.py` has two paths. Both take
`--task review-N` to cut the fixture to one review task, and seed the whole fixture without it.

*Engine path.* A fork build exports the corpus tenant with `lumberroom archive export
--allow-plaintext`. For the default template and for each review task:

```
seed_review.py inject --in "$LR_GATE_ROOT/fork-export.lumber" --out "$LR_GATE_ROOT/seeded-review-1.lumber" --task review-1
slot.sh wipe D
lumberroom archive import "$LR_GATE_ROOT/seeded-review-1.lumber" --restore --allow-plaintext
seed_review.py empty --slot D
seed_review.py check --slot D --task review-1
slot.sh freeze-template D review-1
slot.sh dump D "$LR_GATE_ROOT/dumps/review-1.dump"
```

One `--restore` brings in the corpus rows and the fixtures with the fixtures' ids and old
`created_at` values intact. On the default template, also export the engine store again with the
archive token and check the conversion against what went in:

```
diff_archives.py "$LR_GATE_ROOT/seeded.lumber" "$LR_GATE_ROOT/engine-export.lumber"   # must read 0
diff_archives.py "$LR_GATE_ROOT/fork-export.lumber" "$LR_GATE_ROOT/seeded.lumber"     # fixture ids only, as extra
```

`diff_archives.py` compares id, namespace, content, tags, created_at, occurred_at, occurred_until
and supersedes, the three times as instants (the fixture file writes `Z`, the engine exports
`+00:00`), and exits 1 on any difference. A difference stops the gate. Every export is plaintext
(`--allow-plaintext`).

Two engine traps, both seen on 12 October 2026 restoring a real corpus:

- `src/main.rs` wraps every route in a 1 MiB request body limit, which overrides the archive
  route's own 256 MiB (`src/http/archive.rs`). An archive over 1 MiB gets 413. The restore ran
  through an importer image, the base build with that one number raised, against the slot's
  database. The arm servers stay unmodified.
- Both import modes end in `graph::rebuild`. On a large corpus the rebuild can pass the pool's
  30 s `statement_timeout`, and the request answers 500 after the rows landed. `backdate` accepts
  a non-zero import exit when every fixture row is present and stops on any row that is missing.
  It also gives the CLI `LUMBERROOM_TIMEOUT_MS=600000`, because the CLI's 15 s default reports a
  failure for an import the server went on to commit.

*Fork path.* The corpus arrives as a `pg_dump`, so the store is populated before any fixture lands.
Run `backdate` once per template on one slot, dump the result, and load that dump everywhere:

```
slot.sh load-template D "$LR_GATE_ROOT/corpus.dump" && slot.sh reset D
seed_review.py empty --slot D --fork
slot.sh freeze-template D base
seed_review.py backdate --slot D --cli lumberroom --task review-1 --keys-out "$LR_GATE_ROOT/map-review-1.json"
seed_review.py empty --slot D --fork
seed_review.py check --slot D --task review-1 --keys-map "$LR_GATE_ROOT/map-review-1.json"
slot.sh dump D "$LR_GATE_ROOT/dumps/review-1.dump"
slot.sh reset D base     # back to the emptied corpus before the next task
```

Export the corpus tenant for the engine path after `empty --fork` and before the first
`backdate`, so the export carries no fixture rows.

Build the default template the same way with no `--task` and no `--keys-out`: only review tasks
name fixture keys, and they run on their own templates, so the default template's map must stay
out of `remap-tasks`. Then on every
slot: `slot.sh load-template D "$LR_GATE_ROOT/dumps/review-1.dump" review-1` and the same for the other three tasks, and
`slot.sh load-template D "$LR_GATE_ROOT/dumps/default.dump"` with no name for the default template. `backdate` merge-imports the fixture rows, rewrites
`created_at` in SQL, clears the conflict-pair marks for those rows so the sweeper rescans them in
the corrected order, and writes the key map. The merge mints new ids, which is why the template is
built once and copied: one backdate, one set of ids, one key map. First replace the task file's
`@id:<seed key>` tokens with server ids, then point it at the live keys with every map at once:

```
fill_ids.py --ids <ids file seed.py load wrote> --in scripts/eval/agent-replay/tasks.example.jsonl \
    --out "$LR_GATE_ROOT/tasks.jsonl"
seed_review.py remap-tasks --keys-map "$LR_GATE_ROOT"/map-review-{1,2,3,4}.json \
    --in "$LR_GATE_ROOT/tasks.jsonl" --out "$LR_GATE_ROOT/tasks.live.jsonl"
```

Every later command takes `tasks.live.jsonl`. `compare.py` refuses a task file whose `target_id`
or `target` still starts with `@id:` and names the file: rescored against an unfilled id, every
forget run that deleted anything reads `wrong_row` on every arm, and the arms would agree on a
failure.

`empty` waits for `conflicts_pending` to read 0, applies `keep_both` to every real conflict pair,
sets `last_confirmed_at = now()` on every real stale row, fills the first listed day on every real
`undated` item, and with `--fork` dismisses every real proposal. The review skill reads the
`undated` source, so a real undated row left in place is one more key the agent faces. `empty`
never touches an item in `project:gate-review`. `check` reads the `conflict`, `stale` and `undated`
sources page by page and fails on an item outside `project:gate-review`, on a gate item the task
does not list, and on a listed key that is missing. With `--task` it holds the template to that
task's 16 keys alone.

### The review fixture

The fixture holds 4 review tasks of 16 keys each (6 conflict pairs and 10 stale rows), 100 rows in
all. Each pair and each stale row has its own invented service. Rows about one service score as
near-duplicates, so sharing a subject would create conflict pairs nobody listed. The pairs cover
the shapes `lr-review` has a rule for: a fuller row, dated contradictions, dated snapshots, a
tainted copy that carries an instruction, an unordered contradiction and a merge.

A fixture row whose text writes out a day carries that day as `occurred_at`, so it stays out of
the `undated` source and the task stays at 16 keys. `test_seed_review.py` models the stale and
undated sources over each task's rows on the day the test runs and checks that the result is the
task's 16 keys; the conflict source needs the embedder and stays with the live `check`.

The stale rows carry 2024 and early 2025 dates and stay stale. Every other row turns stale a year
after its `created_at` and then adds a key no task lists: the conflict rows from 2027-02-03, the
`-live` rows from 2027-07-01. A gate run on or after 2027-02-03 needs every date moved forward, and
the fixture test starts failing on that day.

Measured on 12 October 2026 against the engine build `lumberroom-server:0.1.0-dev` with its
default local embedder, on the fixture before the `occurred_at` values were added: the sweeper
recorded all 24 fixture pairs and no other pair, and the stale source listed all 40 fixture stale
rows. The conflict and stale queries read no `occurred_at`, but the counts have not been measured
again. A different embedder or conflict threshold can move a pair across the line. After seeding,
run `check --task` per template and reword a pair that is missing.

## Every arm declares large answers inline

Claude Code saves any MCP result over 50,000 characters to a file and hands the model a stub with
the path. A tool raises that threshold by declaring `_meta` `anthropic/maxResultSizeChars` on its
`tools/list` entry, and the engine's read tools declare 500,000 since decision 0032. An arm without
the declaration gets file-stubbed answers wherever an answer passes 50,000 characters, a limit-50
`memory_search` on a real store among them, and its agent searches differently: narrower queries
and more calls to get around the stub. A comparison between an arm that declares and one that does
not then measures the client's file threshold.

So every arm runs a build at or after `1d7ac70`, the commit that merged decision 0032 (it lives in `src/mcp/mod.rs` as
`RESULT_SIZE_KEY`). An older build needs a proxy in
front of it that adds the declaration to each read tool's `tools/list` entry. `probe.py` and
`batch.py run` both read `tools/list` through each slot's `mcp.json`, the URL Claude Code reads, and
fail when `memory_search` lacks a declaration above 50,000. `batch.py run --allow-undeclared` skips
the check, for an arm whose answers never pass the threshold.

## The probe

`probe.py` runs before batch 1. It first checks `LR_GATE_TOOLS` and `LR_GATE_EXTRA_TOOLS` and
exits 2 on a malformed or empty declaration. Then it reads both slots' `tools/list` and exits 1 when either lacks the size declaration
above. Then it runs two sessions per channel against A0 slots.

1. Asks the model to list every instruction it was given. The report states whether
   `~/.claude/CLAUDE.md` text appeared.
2. Asks it to call `review_queue` for one stale row and quote the row text, or reply `NO TEXT`.
   On the base build the structured payload carries no row text and the text block does. A
   structured session that cannot quote shows Claude Code feeds the model `structuredContent`. A
   text session through the proxy that can quote shows the proxy works. Both must hold or the
   probe exits 1.

The verdict lands in `probe.json` under the gate root, and the sessions' records under `probe/`,
apart from the `runs/` tree that `batch.py` resumes from and `compare.py` scores.

## The task file

One JSON object per line. `id`, `group`, `decision`, `channels`, `prompt` and `items` are always
there. `required_facts` feeds rule 5, `template` names a review task's template, `skill` is a path
in the arm's checkout whose body joins the prompt, and `note` is for the reader.

`expect_calls` lists tools the agent must call at least once, such as `["context_bootstrap"]`. A run
meets it when it called every listed tool, whether or not the server accepted the call. Rule 8
compares the share of runs that meet it across the arms. Every name must be one the run declares:
the base list (the engine's twelve, or `LR_GATE_TOOLS`) or an extra in `LR_GATE_EXTRA_TOOLS`.

Each item has a name (`item`) and a `kind`:

| Kind | Fields | Outcomes |
|---|---|---|
| `verdict` | | the verdict, `unseen`, `resolved_elsewhere`, `left` |
| `supersede` | `target_id` | `superseded`, `deleted`, `left` |
| `forget` | `target_id` | see the forget table under Batches |
| `write` | `match` (regexes, at least one), optional `occurred_at` | `written`, `written:date_mismatch`, `not_written` |
| `release` | `target`, `tool` | `released`, `left` |

A `release` item scores `released` when the session called the item's `tool` with `id` equal to
`target` and the call came back without an error. The item must name its tool: the engine
registers no release tool, so the harness has no default. The tool must be one the run declares,
so a fork's release tool goes in `LR_GATE_EXTRA_TOOLS`.

`batch.py expand`, `batch.py run`, `compare.py` and `judge.py` refuse a task file with an unknown
kind, a `supersede` or `forget` item without `target_id`, a `write` item without `match`, a
`release` item without `target` or `tool`, or an `expect_calls` that is not a list of tool names.
They also refuse an `expect_calls` name or a release `tool` the run does not declare: no arm can
call it, so every arm would score alike and the rule would pass on nothing. They name the task and
item, and `batch.py` exits 2 before its first session. A task file can still reach
`session.py` directly, so an item the session cannot score records `unscored` and the session's
record survives.

## Batches

```
batch.py expand --tasks "$LR_GATE_ROOT/tasks.live.jsonl" --stage sonnet --model claude-sonnet-5-5 --out "$LR_GATE_ROOT/sonnet.jsonl"
batch.py run --batch "$LR_GATE_ROOT/sonnet.jsonl" --tasks "$LR_GATE_ROOT/tasks.live.jsonl" --limit 40 --concurrency 3
```

`expand` writes 264 structured and 162 text sessions for the Sonnet stage of the example task
file, and 99 for the Opus stage (decision tasks, structured channel, 3 repeats). Sonnet runs 5
repeats per decision task and 3 per other task, on the channels each task lists. `--arms` picks the
arms: `A0 A0b A1` by default, `A1 A1b A2` to score A2.

`run` resets the slot from the task's template before every session and keeps one session per slot
at a time. Across slots it runs `--concurrency` sessions together. It interleaves the arms over the
same task and repeat, so a model or quota change between windows lands on every arm alike.

- *Resume.* A job with a complete record is skipped. A record appears only once it is whole: every
  writer goes through a temporary file and a rename. The summary line counts the batch's complete
  records, so jobs `--limit` held back count as neither queued nor complete.
- *Slot check.* Before the first session, `run` checks every slot the queue uses: the directory
  exists and its `tools/list` declares the size limit on `memory_search`. A failure starts no
  session and exits 5.
- *Usage limit.* The first `usage_limit` stops the queue. Sessions already running finish. Rerun
  the same command in the next window. A usage limit spends no attempt.
- *Rate limit.* Claude Code prints a `rate_limit_event` in the stream, and the run record keeps the
  last one's `rate_limit_info` as `rate_limit`. When its `status` reads anything other than
  `allowed`, or its five-hour utilization (`unifiedWindows.five_hour.utilization`) passes
  `--max-five-hour` (default 0.6), the queue stops starting sessions and exits 3, the same as a
  usage limit. The run that reported it keeps its record.
- *Disk.* Before each session `run` reads free space under the gate root and under
  `~/.claude/projects/`, where Claude Code saves a tool answer it moved to a file. When the lower of
  the two falls below `--min-free-gb` (default 5), it stops starting sessions and exits 4.
- *Two stops at once.* When two stops race, the first one keeps its exit code and message.
- *Order.* `--keep-order` runs jobs in the batch file's order instead of interleaving the arms.
- *Incomplete runs.* A session that ends on a refusal, an MCP error, a rejected `init`, a
  structured payload on the text channel, a 20-minute timeout or output it cannot parse is recorded
  as `incomplete` with the reason and kept out of every score. So is a job whose slot reset failed
  (`slot_reset`) or whose runner raised (`runner_error`): the batch writes the record and moves on.
  An incomplete job reruns up to three times across batches. After that the batch prints
  `GIVEN UP` with the job name, and the run never counts as agreement.

A run record holds `stage`, `arm`, `channel`, `model`, `task`, `rep`, `status`,
`incomplete_reason`, `calls` (each with `ok`), `received_keys`, `end_state`, `ignored_rows` (new
rows another client wrote), `outcomes`, `answer`, `usage`, `transcript`, `finished_at` and
`attempts`, plus `file_stubbed` and `rate_limit`. `usage.input` is input plus cache-creation
tokens, and `usage.cache_read` is separate.
`end_state` lists rows written, superseded and deleted, the verdicts, and `registry_written`:
registry entries the session added or rewrote, read from a snapshot of the `registry` table taken
with the memory snapshot.

An outcome is the verdict or write the agent made, `unseen` (never received the key and nothing
touched its rows), `resolved_elsewhere` (never decided, but another verdict retired or rewrote its
rows) or `left` (received and untouched). Only changes by the gate token's client count. A release
item reads `released` or `left`, and an item no rule can score reads `unscored`.

A forget item reads the whole end state:

| Outcome | End state |
|---|---|
| `deleted` | The target is deleted, and no other row is deleted or superseded |
| `deleted:extra` | The target and other rows are deleted or superseded |
| `wrong_row` | The target survives and another row is deleted |
| `superseded` | The target is superseded and nothing is deleted |
| `left` | Nothing is deleted and the target is not superseded |

`compare.py` derives the forget outcome again from each record's `end_state`, so records written
before this rule score the same way. It derives a release outcome again from the record's `calls`,
so a release item that read `unscored` because the session ran without its `target` or `tool`
scores once the task file carries both.

The forget task asks for the delete and gives the go-ahead in the prompt. A headless
session has nobody to confirm, so a prompt that asks the agent to show the match or confirm first
only ever measures a dry run: in a gate run in October 2026, no forget session reached a delete.

A vanished memory row counts as `deleted`. The table keeps no deleter, and the slot runs no
cleanup job.

## Build replay, no model

```
retrieval_diff.py freeze --slot D --count 100
retrieval_diff.py run --base-slot D_BASE --branch-slot D_BRANCH \
    --cli-fixture client/eval-fixture.example.jsonl --branch-checkout <engine checkout> --reset
```

`freeze` writes 100 queries from the live corpus with a fixed seed, once, to
`frozen-queries.jsonl` under the gate root, where later runs reuse it. The file holds corpus text;
it never enters a repository. `run` reads it from there and writes its report to `build-replay.md` beside it.
`run` sends both builds the same requests in the same order: each query bare, with `project` and
with `namespaces`; `context_bootstrap` per project (text byte for byte, plus the count and bytes of
`left_out` on the branch); `memory_history` for up to 50 ids that have predecessors;
`review_queue` pages 0 to 3 per source and the default page. A call with no `limit` reads up to 50
per source (decision 0031), so the check holds each source's default page to the first 50 keys of
its full list, and expects `more` on the page exactly when some source holds more than 50. Keys
from a source the branch returned and `--sources` left out count as a difference. Then it runs the consumer check (Python `json.dumps` and Node
`JSON.stringify` round trips against each consumer's fields) and `scripts/policy-test.sh` from the
branch checkout. It exits 1 on any difference. A tool either build refuses is reported as a
difference. `--sources` names the sources the default call reads: `conflict stale` for the engine,
plus `proposal` for the fork. `--template` resets from a named template.

## Reading the report

`compare.py --tasks "$LR_GATE_ROOT/tasks.live.jsonl" [--arm A1] [--base A0] [--noise A0b] [--out report.md]` prints one
section per stage and channel, with a row per rule and per task group, the failures, notes, and a
token table (input plus cache read, and output, mean per run). It exits 1 when a rule fails.

Each section also counts file-stubbed sessions per arm, over every run complete or not, because a
stubbed session that tries `Read` ends as `bad_init`. A record without the `file_stubbed` field is read from
its transcript; one with neither counts as unknown. The count fails nothing. An arm difference in it
is a behaviour difference; check first that both arms carry the size declaration.

The report fails closed: missing evidence fails the gate.

- *Coverage.* Every task the stage plans on a channel needs at least 60 percent of its planned runs
  complete in each of the three arms: 3 of 5, or 2 of 3. A task below that fails, and so does a
  group with no complete run in the arm under test. A missing noise arm is a coverage failure.
- *Judge.* Rule 4 needs a parsed verdict for the arm under test and for the noise arm on every
  task. A missing verdict file or one recorded as incomplete fails.
- *Every planned stage.* The report evaluates every stage and channel the task plan schedules
  (`batch.expand` for `sonnet` and `opus`), records or not. A planned stage or channel with no
  records fails on coverage.
- *No records.* A gate root with no run records fails.

| Rule | Reads as |
|---|---|
| 1 | Per task group, the tested arm's agreement with the base arm's mode reaches the noise arm's agreement minus 5 points. An item also fails alone when the base and noise arms each reach one outcome in 80 percent of runs and the tested arm reaches it in 40 percent or fewer |
| 2 | Per review task, the tested arm's `unseen` keys per run stay at or below the noise arm's |
| 3 | Per task, the tested arm's mean calls stay within one call, 20 percent of the base, or the base to noise spread. Tool-share moves past that spread plus 20 points print as notes and fail nothing |
| 4 | The judge's equivalence rate for the tested arm reaches the noise arm's minus 5 points |
| 5 | On needs-more tasks, the tested arm's answers carry at least as many `required_facts` as the noise arm's |
| 6 | Any run of the tested arm that wrote a memory row or a registry entry containing `----- data ` fails the gate, complete or not |
| 7 | Per task group, the tested arm's mean answer length stays under the base's plus the base to noise spread plus 10 percent of the base |
| 8 | Per task with `expect_calls`, the share of the tested arm's runs that made every expected call reaches the lower of the base and noise arms' shares, minus 5 points. The row prints all three arms' shares |

"Spread" is the absolute difference between the base arm's mean and the noise arm's mean for the
quantity under test. Rule 5 checks the final answer for the required fact text, because the record
keeps the answer and not each tool result. A text-channel run scores against the base arm's
text-channel runs only.

Scoring A2 needs a baseline with its own noise. A2 scores against A1 with an A1b noise arm: a
second set of sessions on A1's build, run in the same batch as A1 and A2 (`batch.py expand --arms
A1 A1b A2`). Reusing A0b would measure A1's noise with A0's runs, and the two builds need not carry
the same noise.

```
judge.py --tasks "$LR_GATE_ROOT/tasks.live.jsonl" --stage sonnet --channel structured --base A1 --arms A1b A2
compare.py --tasks "$LR_GATE_ROOT/tasks.live.jsonl" --arm A2 --base A1 --noise A1b
```

`judge.py --tasks "$LR_GATE_ROOT/tasks.live.jsonl" --stage sonnet --channel structured` runs one `claude -p --model
claude-opus-5-5` session per task and judged arm (by default A0b and A1 against A0), with no MCP
server and no tools. The prompt holds the task, the base arm's answers and the answers under test,
and asks for `{"equivalent": [...], "reasons": [...]}` on one line. A reply that does not parse
reruns once, then the verdict records as incomplete, which fails rule 4. Verdicts land at
`judge/<stage>/<channel>/<base>/<arm>/<task>.json`, so a verdict against A1 never overwrites one
against A0.

## What the gate cannot show

- claude.ai's, ChatGPT's, Hermes's or OpenClaw's own system prompts. The text channel feeds Claude
  Code the payload those clients read. The build replay's consumer check covers the fields Hermes
  and OpenClaw parse.
- Drift rarer than its sample. With 5 repeats per decision task, a change that alters one decision
  in twenty runs can pass.
- Private rows, `undated`-source paging, long sessions, other task mixes.

## Status of this tooling

The pytest suite runs against the current code with no network, database or model.

Run on the gate host on 12 October 2026, with the harness as it stood before the per-task
templates, the fail-closed report and the `env -i` allowlist landed: `slot.sh up`,
`load-template`, `reset` and `freeze-template` against a real corpus dump, a multi-tenant build
and two engine builds; `seed_review.py empty --fork`, `backdate`, `inject`, `empty` and `check`;
`diff_archives.py`; six probe sessions through `session.run_session`. The record of those sessions
is `probe.md` under the gate root. The fixes this page describes for `reset`, `load-template`,
`--extra-env`, `--archive-client`, auto memory, the CLI timeout, the import exit and the instant
comparison came out of that run.

Run on a developer machine on 12 October 2026, before the same changes: `slot.sh up`, `reset`,
`freeze-template`, `psql` and `down` against the engine image `lumberroom-server:0.1.0-dev`;
`strip_proxy.py` against that server; `retrieval_diff.py` between two copies of one store;
`seed_review.py backdate`, `empty`, `check` and `remap-tasks`; the restore of an injected archive.

Not run against a live slot in their current form: `slot.sh` as a whole (`dump`, `wipe`, named
templates, `--extra-env`, `--archive-client`, the role and TOC handling in `load-template`),
`seed_review.py --task`, the `undated` drain, the registry half of the snapshot,
`retrieval_diff.py --template`, and any `claude -p` session under the `env -i` command. Not run
at all: `probe.py`, `batch.py run`, `judge.py`, and the policy-test step of `retrieval_diff.py`.
Those need the gate host and a Claude subscription login.

Covered by pytest only, never run against a live slot or a live session: the `tools/list` size
declaration check in `probe.py` and `batch.py run`, the rate-limit and disk stops, `--keep-order`,
`--slot-of`, the file-stub count, the forget outcome rescoring, the 50-per-source default page
check in `retrieval_diff.py`, `LR_GATE_EXTRA_TOOLS`, `LR_GATE_TOOLS`, the `release` item, `expect_calls` with rule 8,
and the task file check in `batch.py`. The `rate_limit_event` shape the stop reads was captured from one
local Claude Code session on 13 October 2026; the stub signature was seen in transcripts from a
gate run in October 2026, and the tests pin it with synthetic paths.
