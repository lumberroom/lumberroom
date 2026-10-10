# Switching embedding models

This guide moves a Docker Compose install of lumberroom from one embedding model to another while
the server keeps serving, and back again if retrieval gets worse. The design and its reasons are in
decision [0027](decisions/0027-the-server-moves-its-own-embeddings.md); the per-model thresholds
are in decision [0029](decisions/0029-thresholds-belong-to-the-model.md).

**Status.** Implemented on `feat/embedding-migration`. Nobody has run these steps end to end through
Docker Compose yet. Every output line quoted below comes from the command's source, not from a run.

## What a switch does

You name the new model in `.env` as `EMBED_*` and keep the current one as `EMBED_PREVIOUS_*`. After
`embeddings start`, the server:

1. embeds every new write with both models and stores both vectors, one per slot;
2. fills the new model's vectors into older rows in the background, paced behind live traffic;
3. once you run `embeddings flip` and the fill is complete, flips the store to the new model in one
   small transaction that rewrites no row;
4. keeps the old model's vectors current, so `embeddings rollback` flips back with no re-embedding;
5. deletes the old vectors when you run `embeddings retire`, which it accepts only after the 7-day
   rollback window.

The server does every embedding call, fill, flip and deletion. The command records what you want in
one database row and reports what the server did with it. Nothing stops, and writes never pause.

## What it costs

- **Disk.** About 7.4 KB per row until retire: about 3.2 KB for the second vector and about 4.1 KB
  of HNSW index. At 10,000 rows that is about 74 MB. Each filled row also leaves one dead row
  version and an entry in every index until vacuum reclaims it. These figures are arithmetic from
  the storage layout; nobody has measured them on a real fill.
- **Two models resident** from `start` until you remove the old block after retire. A local model
  holds its weights in the server's memory; a remote one costs a call per write.
- **Slower writes.** Each write waits for both embeddings, the second one for at most
  `EMBED_SHADOW_TIMEOUT_MS` (10 seconds by default). A write whose second vector misses still lands,
  on the active model, and the fill picks the row up later.
- **Fill time.** The fill sends one request at a time and, at the default `EMBED_FILL_DUTY=50`,
  sleeps as long as each request took. Expect about twice what your embedder takes to embed the
  whole store once.
- **After the flip, searches depend on the new model.** If its endpoint goes down, searches and
  writes fail with its error, as on any install with a remote `EMBED_PROVIDER`. There is no
  fallback (decision [0028](decisions/0028-no-embedding-fallback.md)).

## Before you start

- **Same width.** Both models must produce 768-dimension vectors. `EMBED_PREVIOUS_DIM` must equal
  `EMBED_DIM`, and both vector columns are `vector(768)`.
- **The new endpoint must answer from inside the server container.** In there `127.0.0.1` is the
  container itself, not your host. Use the name of a service on the same Compose network (for
  example `http://embedder:8080/v1` for an embedding server you added to this compose project) or an
  address of the host that the container can route to. On Linux, `host.docker.internal` resolves
  only for a service that declares `extra_hosts: ["host.docker.internal:host-gateway"]`, and the
  shipped compose file does not.
- **Move any old single threshold variable.** `DEDUPE_THRESHOLD`, `CONFLICT_THRESHOLD`,
  `BOOTSTRAP_DEDUP_COSINE`, `GRAPH_ROUTE_MAX_TOP` and `GRAPH_ROUTE_MAX_SPREAD` describe one model.
  With a second block configured the server refuses to boot while any of them is set. Move each into
  the override of the model you tuned it on. That is usually the model you are leaving, so it goes
  into `EMBED_PREVIOUS_THRESHOLDS`:

  ```sh
  # before
  DEDUPE_THRESHOLD=0.96
  # after
  DEDUPE_THRESHOLD=
  EMBED_PREVIOUS_THRESHOLDS=dedupe=0.96
  ```

- **Leave the sweep on.** `EMBED_MIGRATE_SECS=0` turns it off, and every verb but `status` then
  refuses.
- **If you build the image yourself,** build it with both blocks in `.env`. Compose passes both
  providers as build arguments, and the image carries a local model's weights only when a block
  names it. The published image carries bge-base-en-v1.5.

## The steps

Every command runs from the directory that holds `docker-compose.yml` and `.env`.

**1. Declare the new model.** Put the new model in `EMBED_*` and move the current model's lines to
`EMBED_PREVIOUS_*`. For a store on the default local bge-base-en-v1.5 moving to EmbeddingGemma 2
served by llama-server:

```sh
# The model the store moves to.
EMBED_PROVIDER=openai
EMBED_MODEL=google/embeddinggemma-2
EMBED_BASE_URL=http://embedder:8080/v1
EMBED_DIM=768
# About three characters per token of one llama-server slot (2,048 tokens here).
EMBED_MAX_INPUT_CHARS=6000

# The model the store holds now.
EMBED_PREVIOUS_PROVIDER=local
EMBED_PREVIOUS_MODEL=Xenova/bge-base-en-v1.5
```

Check the file before the server sees it. `run` starts a fresh container that reads the edited
`.env`, and `verify-embedding` runs the boot checks against the store without loading any model:

```sh
docker compose run --rm -T server lumberroom-server verify-embedding
```

Then recreate the server. `exec` sees the environment a container booted with, so an `.env` edit
reaches the commands below only after this:

```sh
docker compose up -d server
```

The server builds both embedders and stays `steady` until `start`: writes still embed once, with
the model the store holds now, the one `EMBED_PREVIOUS_*` names.

**2. Start.**

```sh
docker compose exec server lumberroom-server embeddings start
```

`start` sends one probe embedding to a remote target first and refuses with the error if it fails.
From the next pass, writes embed with both models and the fill begins.

**3. Wait for the fill.**

```sh
docker compose exec server lumberroom-server embeddings status
```

Repeat until every unit reads `held`. A unit line has this shape:

```text
unit me: filling, active slot a on Xenova/bge-base-en-v1.5@q8, other openai:google/embeddinggemma-2, pending 1840 of 12000 eligible, failed 0, retire_after none
```

The engine has one unit, `me` (your `TENANT_ID`), plus any other tenant id that holds vectors. The
console's reading page shows the same status.

**4. Flip.**

```sh
docker compose exec server lumberroom-server embeddings flip
```

Each unit flips on the next pass whose fill is complete. You may run `flip` before the fill ends:
the command writes the request and the flip waits for the fill. `status` then shows `flipped` and
`rollback: instant`.

**5. If retrieval is worse, roll back.**

```sh
docker compose exec server lumberroom-server embeddings rollback
```

Every write since the flip stored an old-model vector as well, unless that second vector missed
`EMBED_SHADOW_TIMEOUT_MS`, and a `flipped` unit fills those misses on its next pass. When `status`
shows `rollback: instant`, each unit flips back on the next pass with no embedding calls. Under
`rollback: needs_fill` the server first fills the rows still missing their old vector, then flips.

To move forward again after a rollback, run `start` and then `flip`. `flip` alone does nothing,
because the rollback made the old model the target. The new model's slot is still full, so after
`start` each unit reaches `held` at once.

```sh
docker compose exec server lumberroom-server embeddings start
docker compose exec server lumberroom-server embeddings flip
```

**6. Once the window has closed, retire.** Seven days after the last flip of every unit
(`EMBED_ROLLBACK_DAYS`):

```sh
docker compose exec server lumberroom-server embeddings retire
```

The server deletes the old model's vectors in batches and clears its slot. When `status` shows every
unit `steady`, remove the `EMBED_PREVIOUS_*` lines (and `EMBED_PREVIOUS_THRESHOLDS`) from `.env` and
recreate:

```sh
docker compose up -d server
```

Until retire runs, rollback stays instant whatever the date: the window bounds retire, not rollback.
Each flip restarts the unit's window, a rollback's flip included.

Before a flip, `rollback` cancels the start instead: the server deletes the new model's partial
vectors and every unit stays on the old model. A later `start` refills them, picking up from
whatever the cancel had not deleted yet.

## The verbs

Run each as `docker compose exec server lumberroom-server embeddings <verb>`. The four writes accept
`--no-wait`; `status` accepts `--json`. Every refusal names the units, counts or dates it rests on
and the next step.

Every write first compares the embedder ids it computes from its environment with the ids the
server's last pass ran. If they differ, someone edited `.env` without recreating, and the command
refuses until you run `docker compose up -d server`. If no pass ran in the last three intervals, it
skips the comparison and prints a warning.

| Verb | Writes | Refuses when |
|---|---|---|
| `status` | nothing | never; prints the intent, the server's last pass, each unit's phase and counts, `rollback`, each model's thresholds with their sources, and a `next:` line |
| `start` | target = the configured model no unit is on, flip off | one block configured (recreate first if you just added it); both blocks compute the same id; no unit holds vectors yet; units sit on different models; the units' model is in neither block; the model you leave has a `guessed` acting key (a rollback would land on it); an inactive slot holds a third model, or a retire of one is still deleting; free disk at or below the floor; the probe to a remote target fails |
| `flip` | flip on | no target (run `start`); the target is in no configured block; the target has a `guessed` acting key |
| `rollback` | before a flip: no target, retire the target (a cancel). After one: target = the old model, flip on | the old model's block is gone from `.env`; units on the target hold different old models |
| `retire` | retire = the model in the inactive slots | a unit is not on the target yet; the server has published no status for a unit; a unit has failed rows; a unit's window has not closed (it names the date); inactive slots hold more than one model |

A verb that has nothing to change prints why and exits 0. That covers `start` toward the current
target, `flip` when every unit is on the target or a flip is already requested, `rollback` with
nothing started, and `retire` when no unit holds a second model or that model is already being
retired.

The guessed-key refusals print the line that fixes them, holding the values the model runs on today,
for example:

```sh
EMBED_PREVIOUS_THRESHOLDS=dedupe=0.97,conflict=0.9,cleanup_near_certain=0.97
```

Add it to `.env`, recreate, and run the verb again.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | written and applied by a server pass, or nothing to write, or `status` |
| 1 | refused; nothing written. Also an `.env` that fails validation, or a store the command cannot reach |
| 2 | usage error, or the server runs `EMBED_MIGRATION_CONTROL=env` |
| 3 | written, and no server applied it within three intervals plus 5 seconds |

After a write the command polls every 2 seconds until a pass applies it, then prints each unit's
phase. Exit 3 is not a failure: the intent stays in the database and applies on the next pass.
`--no-wait` returns 0 as soon as the write commits.

## `exec` or `run`

Use `docker compose exec server ...` while the server runs. The command then sees the exact
environment that server booted with, which is what the drift check compares.

With the server stopped, `exec` fails with Compose's own error. Use
`docker compose run --rm -T server lumberroom-server embeddings <verb>` instead: Compose starts the
database if needed, the command writes the intent, warns that no pass has run recently, and exits 3
when nothing applies it. A server applies it when it starts. `run` reads the current `.env`, so
against a live server that has not been recreated since an edit, `run` refuses on drift. That is
the check doing its job.

`run` is also the way out when the server refuses to boot over the control row, for example after
you removed the block of a model the row still targets:

```sh
docker compose run --rm -T server lumberroom-server embeddings rollback
```

## Thresholds per model

Each model reads its own cosine thresholds. The server ships a table:

| Key | Acts | bge-base-en-v1.5 | EmbeddingGemma 2 |
|---|---|---|---|
| `dedupe` | yes | 0.97 | 0.995 |
| `conflict` | yes | 0.90 | 0.91 |
| `bootstrap_dedup` | no | 0.90 | 0.919 |
| `cleanup_near_certain` | yes | 0.97 | 0.995 |
| `cleanup_worth_asking` | no | 0.65 | 0.754 |
| `route_max_top` | no | 0.65 | 0.65 |
| `route_max_spread` | no | 0.08 | 0.08 |

A model matches a row when its id contains the family name. `status` prints every value with its
source: `override` (your line), `legacy` (an old single variable), `shipped`, `study`, `sweep`
(measured by the LongMemEval fusion sweep), `carried` (bge's value, unmeasured for this model) or
`guessed`. Decision 0029 explains each.

- **A model with no entry runs on bge's values,** marked `guessed`, and the boot log names every
  guessed key. An acting key (`dedupe`, `conflict`, `cleanup_near_certain`) changes or merges data
  with nobody reading it first, so the server will not flip a unit onto a model whose acting keys
  are guessed, and `start` will not leave one.
- **Override per block.** `EMBED_THRESHOLDS=dedupe=0.98,conflict=0.92` sets keys for the model
  `EMBED_*` names. `EMBED_PREVIOUS_THRESHOLDS` does the same for `EMBED_PREVIOUS_*`'s model. Neither
  line reaches the other model, and boot refuses a key it does not know.
- **Measure a model with no entry** once both slots are full, which is when `status` shows the unit
  `held`. Run `scripts/embedding-thresholds.sql` on a copy of the database, never on the one serving.
  It maps each old threshold to the new model by matching the share of same-namespace neighbour
  scores at or above it, prints `support` beside each value, and ends with a line to paste after
  `EMBED_THRESHOLDS=`. A `support` under 30 means too few old scores sit near that threshold to trust
  the mapped value. The script reads slot A as the old model and slot B as the new. When `status`
  shows the old model in slot B, add `-v old=embedding_b -v new=embedding` to the `psql` line; the
  script uses each column for both the sampled rows and their neighbours, so the two models never
  meet in one score. Feed it the old model's values from `status`:

  ```sh
  docker compose exec -T db pg_dump -U lumberroom -Fc lumberroom > lumberroom-copy.dump
  docker run -d --name lr-thresholds -e POSTGRES_PASSWORD=scratch pgvector/pgvector:pg16
  docker exec -i lr-thresholds pg_restore -U postgres -d postgres --no-owner < lumberroom-copy.dump
  docker exec -i lr-thresholds psql -U postgres -v tenant='*' \
    -v thresholds='dedupe=0.97,conflict=0.90,cleanup_near_certain=0.97' \
    -f - < scripts/embedding-thresholds.sql
  docker rm -f lr-thresholds && rm lumberroom-copy.dump
  ```

  Wait a few seconds after `docker run` for Postgres to accept connections. The dump holds every
  row, so delete it when you are done.

## Phases

`status` shows one phase per unit.

| Phase | Meaning | What the server does |
|---|---|---|
| `steady` | one model, nothing started | fills rows missing their active vector; a row whose vector carries another model's id shows as `foreign_model` in `status --json` and stays as it is |
| `filling` | started, the new slot is not full | fills it |
| `held` | the new slot is full, no flip requested | keeps it current; waits for `flip` |
| `ready` | full and flip requested | flips on this pass |
| `flipped` | on the new model, the old one still configured | keeps the old slot current for rollback |
| `retiring` | retire requested | deletes the old vectors once the window has passed and no row lacks its active vector |
| `blocked` | see the reason | keeps filling where it can; only the flip waits |

The blocked reasons:

- `other_holds_third`: the inactive slot holds a model neither block names. The unit fills nothing
  while it stays blocked. `embeddings retire` clears it, and the server fills any row missing its
  active vector before it deletes the first old vector.
- `kek`: private rows need the key-encryption key, and this boot did not verify one. Fix the KEK
  (`curl -s 127.0.0.1:8787/readyz` shows `kek_verified`).
- `failed`: rows the embedder refused three times while it answered other rows. `status --json` and the
  console name up to 20 of them. The server retries them every pass, and the block clears once they land.
- `guessed`: the flip is requested and the target's acting keys are guessed. Set them in `.env` and
  recreate.

A pass in which every request to a model fails counts as an outage, blames no row, and shows in the
`error:` line of `status`.

## The disk floor

`EMBED_DISK_FLOOR_MB` (0, off, by default) stops the fill and the retire batches while the
filesystem holding `EMBED_DISK_PATH` (default `/`) has less than that much free. Live writes never
pause; a write adds one row's second vector, and the fill is the volume. `status` shows
`paused: disk_floor` with free and floor bytes, the log says `embedding paused: disk` once, and the
fill resumes on the first pass with room. `start` refuses at or below the floor, and no command
overrides it.

The server reads free space from inside its own container. On a default Docker install the
container's root filesystem and the `pgdata` volume both live under Docker's data root, so `/` reads
the disk the database grows on. Compare before you rely on it:

```sh
docker compose exec server df -h /
df -h /var/lib/docker
```

If the two differ, or the database lives on another machine, leave the floor at 0.

## When the server refuses to boot

- **"units active on a model no block configures".** You changed `EMBED_*` without moving the old
  model to `EMBED_PREVIOUS_*`. Restore `EMBED_*`, or name the old model in `EMBED_PREVIOUS_*`.
- **An old single threshold variable beside a second block.** Move it as "Before you start" shows.
- **`EMBED_FLIP` or `EMBED_RETIRE` set in command mode.** Those steer `env` mode only. Unset them.
- **The control row targets a model no block names.** Restore that model's block, or run
  `embeddings rollback` through `docker compose run --rm -T server`.
- **A guessed acting key on the model you are leaving.** Pin it with the `EMBED_PREVIOUS_THRESHOLDS`
  line the message prints.
- **`EMBED_PREVIOUS_DIM` unlike `EMBED_DIM`,** or the hash embedder in either block while two are
  configured.

`docker compose run --rm -T server lumberroom-server verify-embedding` runs the same checks without
starting the server.

## Steering from `.env` instead

`EMBED_MIGRATION_CONTROL=env` replaces the command with configuration: `EMBED_PREVIOUS_*` starts the
move, `EMBED_FLIP` (`all`, `none`, or a list of unit ids) allows flips, and `EMBED_RETIRE` names the
model to delete once its window has passed, each applied by a recreate. Write `EMBED_RETIRE` as the
id `status` prints: `openai:<model>` for a remote model, `<model>@q8` for a local one. A bare name
reads as a local model and gains the `@q8`, so a remote model needs its `openai:` prefix. Boot
refuses a value that names either configured block. Rollback swaps the two blocks. In this mode
every `embeddings` verb exits 2. The command suits a single-unit install; `env` mode suits a
deployment that manages many units from configuration.
