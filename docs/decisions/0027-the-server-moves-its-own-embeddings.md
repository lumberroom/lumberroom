# 27. The server moves its own embeddings

**Date:** 10 October 2026 · **Status:** accepted, implemented on `feat/embedding-migration`; the
integration suite and the end-to-end run through Docker Compose have not run · **Decided by:** the
owner

## Decision

The running server moves its store from one embedding model to another, with no stopped window and
no write pause, and moves it back by flipping a pointer.

- **Two fixed slots and a pointer.** `memory.embedding` is slot A and `memory.embedding_b` is slot B,
  both `vector(768)` with an HNSW index each. `embedding_state` holds one row per unit (the engine's
  unit is a `tenant_id`, `me` by default) naming the model in each slot and the `active_slot`
  readers use. A slot keeps its model from the moment the sweep names it until retire clears it. No
  vector ever moves between columns. Migration `20261010000030_embedding_migration.sql` adds slot B,
  its index, both tables and the conflict changes.
- **Dual writes.** While a second model is configured and started, every write embeds with both
  models and lands both vectors, each in its own slot. The active vector is required. The second
  waits at most `EMBED_SHADOW_TIMEOUT_MS` (10,000 by default); a miss still lands the write on the
  active model and leaves the row pending for the sweep.
- **A background sweep fills the rest.** It runs every `EMBED_MIGRATE_SECS` (30 by default), keeps
  one embedding request in flight, caps a request at `EMBED_FILL_CHARS` characters, sleeps to hold
  `EMBED_FILL_DUTY` percent of wall time, spends at most 60 seconds per unit per pass, and stops
  below the disk floor `EMBED_DISK_FLOOR_MB`. Live writes never wait on it and never pause for disk.
- **The flip writes no memory row.** Once a unit has no row pending in the target slot, the sweep
  flips its pointer in one short transaction. Rollback flips it back.
- **Retire waits seven days.** Deleting the old slot's vectors waits until `flipped_at` plus
  `EMBED_ROLLBACK_DAYS` (7 by default) for every unit.
- **Each model reads its own thresholds**, from the table decision
  [0029](0029-thresholds-belong-to-the-model.md) ships. A unit flips onto a model only when none of
  that model's acting keys resolves as `guessed`.
- **Two ways to steer the same rules,** chosen by `EMBED_MIGRATION_CONTROL`. In `command` mode, the
  engine's default, the operator runs `lumberroom-server embeddings status|start|flip|rollback|retire`
  inside the server container and the command writes the intent to one control row,
  `embedding_control`, which the sweep reads on every pass. In `env` mode, `.env` steers:
  `EMBED_PREVIOUS_*` starts the move, `EMBED_FLIP` allows flips and `EMBED_RETIRE` confirms the
  deletion, each applied by a recreate.

[`docs/switching-embedding-models.md`](../switching-embedding-models.md) is the operator's guide.

## Context

The owner ruled on 9 October 2026 that the server manages a model change itself: no stopped window,
both vectors written from the moment a row lands, a 7-day rollback window, and a watch on free disk.
On 10 October 2026 the owner removed every embedding fallback (decision
[0028](0028-no-embedding-fallback.md)), so a model that fails to load stops the boot and a failed
embed fails the request. The same day the owner asked for threshold lines per model and for "a
migration command, so people can switch embeddings if required".

The trigger was a retrieval comparison. On LongMemEval-S, 500 questions, EmbeddingGemma 2 scored R@5
98.0 against bge-base-en-v1.5's 96.2 and NDCG@10 93.8 against 88.8
([`docs/results/2026-10-embedding-model-comparison.md`](../results/2026-10-embedding-model-comparison.md)).
Before this change the only way to adopt it was to change `EMBED_MODEL` and re-embed by hand, and
the server would have searched new query vectors against old document vectors until someone did.

**Why the flip writes no row.** Per eligible row at 768 dimensions, by arithmetic from the type
layouts and not measured: a vector datum is 3,080 bytes, about 3.2 KB once TOAST stores it out of
line; an HNSW entry costs about 4.1 KB of index. Filling slot B costs about 7.4 KB per row until
retire, plus one dead heap version per row until vacuum, because the indexed slot makes each fill
update non-HOT. The flip then costs one `embedding_state` row and a rollback costs nothing. Copying
the new vector into `embedding` instead rewrites both vectors into TOAST, adds an HNSW entry and an
entry in every other index on `memory`: about 12 to 14 KB per row per swap, again on every rollback,
and those bytes stay in the files until vacuum makes them reusable. At 10,000 rows that is about 74
MB against about 160 MB, and nothing on a rollback against another 130 MB.

The flip transaction sets `lock_timeout` to 2 seconds, locks the unit's state row, in `command` mode
reads `embedding_control.generation` `FOR SHARE` and refuses as stale if a command moved it, counts
the rows still pending in the target slot under its snapshot, moves the pointer, and clears the
unit's stored conflict pairs so the sweeper rescans at the new model's floor. It takes no row lock on
`memory`, so writes never wait on it. An insert that missed its second vector and commits during the
flip lands with no vector in the newly active slot; lexical search still reaches it, and the next
pass fills it.

**Why writers place vectors.** Every writer gets the unit's `UnitEmbedding` and binds the two
vectors in slot order into one `INSERT`. A unit test in `src/adapters/postgres/memory.rs` reads the
file's source and fails when an `INSERT INTO memory` does not name both slots. A trigger that placed vectors by model
would have needed a shared advisory lock on every insert and an exclusive one whenever a slot is
named or cleared, and every writer already sat in a file this change edits.

**Why a server subcommand.** Whoever switches models edits `.env` anyway, because the second model's
provider, URL, key and prefixes are configuration, and that person already holds the host and
Docker. `lumberroom-server` reaches the store with the server's own `DATABASE_URL` and computes
embedder ids from the same environment the server booted on, with no network hop and no credential
of its own. Shell access to the host is the authority, the same access the `.env` edit needs.

**Why a control row.** A container cannot edit the host's `.env` or restart itself. The command
locks `embedding_control` `FOR UPDATE`, re-reads the unit states under that lock, decides, writes
the change with `generation + 1`, and commits. The sweep reads the row at the start of each pass,
re-reads the generation before each fill or retire batch, and at the end publishes the generation it
applied, the embedder ids it runs and its status JSON. The command waits up to three intervals plus 5
seconds for its generation to apply. Two operators serialise on the row lock, and the second decides
against the first's write. The command never locks a state row, so the two locks cannot wait on each
other in a cycle.

**Why two modes.** A self-hoster needs the command. A deployment whose operator steers from
configuration, a fork that runs one unit per tenant for one, needs `.env` and no command. In `env`
mode the server neither reads nor writes `embedding_control`, and every verb of the command exits 2.

**Why thresholds come from a table.** Each model scores on its own scale, so one value cannot serve
both slots. Decision 0029 holds the table, its basis per value and the override variables. A
self-hoster moving to a model with no entry measures it on a copy with
`scripts/embedding-thresholds.sql`, which maps each old threshold to the new model by matching the
share of same-namespace neighbour scores at or above it, and writes the result into
`EMBED_THRESHOLDS`.

## What lost

**A stopped-window model-swap command.** An earlier design had the operator stop the server, run a
command that re-embedded and swapped, and start it again. The owner rejected it on 9 October 2026:
the server manages the move, with no stopped window. The `embeddings` command differs in kind. It
stops nothing, and the server still makes every embedding call and every write.

**An operator-run one-shot re-embed.** A command that walks the store once, writes new vectors and
exits either needs writes stopped or races them: a row written mid-run lands on the old model and
nothing comes back for it. It has no pacing behind live traffic, no resume, and no pointer to roll
back with.

**An UPDATE swap over an unindexed shadow column.** About 12 to 14 KB per row per swap, repeated on
every rollback (see Context), plus a slot lock that stalls inserts from the swap to its commit and an
inline path for rows written during it.

**Renaming the columns to flip.** Catalog-only, but `ALTER TABLE` takes ACCESS EXCLUSIVE on
`memory`, the rename is table-wide so every unit flips at once, and trigger column lists bind
attribute numbers.

**One statement choosing the column with `CASE`.** pgvector cannot serve `ORDER BY` on an
expression from either HNSW index, so every search would scan.

**Calibrating thresholds inside the server.** A table, an estimator, a phase and five repository
methods, to produce numbers the owner reviews once per switch anyway. A shipped table carries
measured values for the models in use, and the read-only script serves any other.

**The `lumberroom` CLI with an admin route.** The CLI reaches the server over HTTP as a client
principal, and no principal carries an instance-wide authority. Retire deletes vectors in every
namespace, so a route for it needs a new authority in all three auth modes, usable from any machine
holding the token.

**A host script that rewrites `.env` and recreates for each step.** Rewriting `.env` from a script
meets quoting, duplicate keys and comments; two operators would need a lock of their own; and every
step would cost a restart.

**A `LISTEN` wake for the control row.** One pass every 30 seconds suits a step a person takes a few
times a month.

**Recording `retire` early and applying it when the window closes.** It would stop dual writes at
once and turn the rest of the window's rollback into a refill. `retire` refuses until the window
closes.

## What it costs, accepted

- **Disk.** About 7.4 KB per eligible row until retire (arithmetic), plus one dead heap version and
  an entry in every index per filled row until vacuum. The files keep that high-water mark. Backups
  carry the second vector too.
- **Slower writes during the transition.** Each write waits for both embeddings, up to
  `EMBED_SHADOW_TIMEOUT_MS`.
- **Two models resident** from the start until retire, and the old model's weights in the image while
  either block names a `local` model.
- **Reader SQL twins.** Every vector statement gains a slot-B spelling, held to its slot-A twin by
  tests.
- **A conflict rescan** of each unit at its flip.
- **A missed second vector at the flip moment** reaches search by lexical match only, for one pass.
- **A heartbeat write per pass** in `command` mode: one single-row update of `embedding_control`
  every `EMBED_MIGRATE_SECS`.
- **One `.env` edit and recreate** before `start` to declare the second model, and one after retire
  to remove it.

## What this is not for

It is not a width change: `EMBED_PREVIOUS_DIM` must equal `EMBED_DIM`, and both slots are
`vector(768)`. It is not a per-unit model choice; one instance has one target. It does not support
more than one server process per database, because a second process's state map can lag the first's
flips. And it is not a remote control: `embeddings` runs on the host that runs the server.

## Reversal condition

- **Move the switch to an admin route** if the engine gains an instance-wide operator authority for
  other reasons.
- **Revisit the indexed slot B** if a measured fill costs more disk per row than the arithmetic
  above. The alternative is an unindexed fill and a `CREATE INDEX CONCURRENTLY` by hand before the
  first flip.
- **Lower `EMBED_FILL_DUTY`'s default** if a live query during the fill takes more than twice its
  time alone (a design target, unmeasured).
