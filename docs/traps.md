# Traps

Findings that cost real time to get. Each one arrived as a passing test, a plausible number, or a
silent wrong answer, which is why they are written down rather than left for the next person to
re-derive. Where a longer write-up exists with its evidence, the entry links it.

## Retrieval and the database

**HNSW silently truncates a filtered search.** `hnsw.iterative_scan` defaults to off, and every
search here filters by namespace. Reproduced at 40k rows with a namespace holding 0.5% of them: a
query asking for 10 rows returned zero, because the scan pulled 40 candidates and the filter removed
all 40. The caller was told nothing is known. Migration `003` sets `strict_order` and `ef_search=100`
on the database so the setting travels with the schema; never remove or override it.
[`research/pgvector-at-scale.md`](research/pgvector-at-scale.md).

**A Postgres `tsvector` is not an index over the document, it is the document, stemmed.** So private
content drops out of lexical search and becomes semantic-only, and the GIN index is partial on
`sensitivity = 'open'`. The embedding stays plaintext and leaks the gist, which
[decision 0005](decisions/0005-private-drops-lexical-search.md) states rather than hides.

**`SET LOCAL` on a pooled connection with no transaction open is a warning and no effect.**
`nearest_ids(exact)` ran `SET LOCAL enable_indexscan = off` that way, so every "exact ground truth"
scan went through HNSW and the recall monitor compared the index against itself for a whole phase.
It could not have caught the truncation failure it existed to catch. Wrap it in a transaction. On a
pool you never know whether one is open.

**At `k=1` the planner declines the HNSW index**, so both arms of the monitor run sequentially and
report a self-comparison a second way, unrelated to the `SET LOCAL` bug.
[`research/recall-monitor.md`](research/recall-monitor.md) carries the method.

**Migrations are forward-only.** `sqlx` embeds them at compile time, so once a newer binary has
migrated a store, an older image cannot boot against it. Plan rollbacks as forward migrations.

**A window that filters both sides of a pair comparison misses the commonest duplicate there is.** A
new row restating an old fact gives one new row and one old one. Anchor on what changed and compare
against every live row. `similar_pairs` does, and a test asserts the `OR` that makes it so.

**A run that finds nothing still has to advance its watermark.** Set the mark from the candidates and
a quiet run advances nothing, so every later run re-reads the same rows forever. `newest_in_scope`
exists for that.

**A column named in a partial index predicate counts as indexed, and that turns off HOT.** The
`memory_never_accessed` index carried `WHERE last_accessed_at IS NULL`, so the touch after every
search wrote a new entry into every index on `memory`, the HNSW graph included. On a 5,000-row
scratch store, 2,400 touches ran 0 HOT with the index and 2,027 HOT after migration 026 dropped it.
Never name `last_accessed_at` or `access_count` in an index on `memory`, key or predicate;
`tests/search_touch_hot.rs` reads the catalog for it, and that covers indexes a fork adds.

**A bare parameter takes its type from the first place it appears.** `PREPARE p AS SELECT 1 WHERE
$1 > 0 AND $1 * 1.0::float8 > 0` gives `$1` the type `integer` on Postgres 17, the later float
use notwithstanding. A guard added ahead of a parameter's existing uses can retype it. Cast it where
it first appears, as the lexical arm's `$10::float8 > 0` does.

**A conflict scan inside a trigger on `memory`, or inside the inserting transaction, loses pairs.**
Under `READ COMMITTED` a scan sees only rows committed before its statement starts. Two writes into
one namespace that overlap each scan before the other commits, both rows get marked scanned, and the
pair between them is never found or retried. The same holds for an `AFTER INSERT` trigger and for a
deferred constraint trigger. Only the sweeper calls `memory_conflict_record`, and it runs after commit:
a wake reaches it once Postgres releases the notification, and a timer sweep reads committed rows.
`tests/conflict_pairs.rs` reads `pg_proc` and fails if any trigger function on `memory` calls
`memory_conflict_record` or inserts into `memory_conflict`. The argument is in
[`specs/write-time-conflicts.md`](specs/write-time-conflicts.md) section 6.

**A pooler in transaction mode drops conflict wakes.** `LISTEN` binds to one server session, and
transaction pooling hands that session to other clients, so the listener hears nothing and raises no
error. Rows stay pending until the next timer sweep, up to `CONFLICT_SWEEP_SECS` (default 60), and
then record normally. Nothing is lost and nothing warns. Set `CONFLICT_SWEEP_SECS=0` and the same
deployment records no pairs at all, which looks identical to a quiet store until the pending count
stops falling. Spec section 5 lists the wake failure path.

## Policy and disclosure

**Four disclosures shipped, and no gate could have caught them.** Each published a value computed
before the grant ran, or after only one of the two axes: a client name from `/statsz`, a 60-character
excerpt from `/admin/recall`, unfiltered counts in the digest inventory, and namespace names in
`also_searched`. The suites assert that a nonce is absent, and none of those four is a nonce.
`policy-test.sh` now asserts the class. Five of the new tests were validated by reverting the fix and
watching them fail, because a test written after a fix that never saw the bug is not evidence.

**The digest has seven grant-filtered subqueries, not five.** `DIGEST_SQL` in
`src/adapters/postgres/memory.rs` is one round trip on purpose, and every arm joins `reachable` and
compares `sensitivity_rank`. The two a reviewer misses are the registry arms, and the registry holds
credential locations. Unit tests count the `JOIN reachable rg` and `<= rg.max_rank` occurrences, so
adding an arm moves the counts and you update the test on purpose.

**`namespace_counts` keeps an unfiltered signature.** A glob grant resolves against concrete names,
so discovery has to run before the grant can. The port documents the contract: neither its counts nor
its names may reach a response as they stand.

**Empty plaintext is never a comparison.** A private neighbour arrives from the repository with
`content == ""`, and a collapse guard run against an empty string passes for any text without digits,
identifiers or negations. `src/services/write.rs` decrypts every empty-content candidate before the
bands run.

**An alias resolved from one side only.** The group lookup keyed on `namespace = $2`, and an alias
row lives in whichever namespace the writer typed. Measured on a real store: 8 hits one way, 4 the
other, `also_searched: []`, no error. The scope is the prefix now, so `project:` covers every project
namespace and `personal:warden` stays a different subject.

**Recall reinforces itself into duplication.** One audited store held the same preference 808 times,
because recalled memories were re-extracted as new ones by a capture hook with no matcher. Anything
that writes back from context has to exclude what the digest and search put there. The whole
ingestion design is arranged around this.

**Switching `KEK_PROVIDER` from `none` to a key changes every content digest.**
`recall_emission.content_sha256` and `ingest_proposal.fingerprint` are HMACs under a key derived
from the KEK (`crypto::digest`); with no KEK they are plain SHA-256. Emissions and proposals
recorded before the switch never meet the ones recorded after, so the echo check answers false
for old emissions and a rejected proposal re-proposes once. Run migration 000017's `DELETE` by hand
after the switch if the stale rows bother you. Migration 000009's column comment still describes
the unkeyed hash; migrations are forward-only, so `crypto/digest.rs` is the description that holds.

## Protocol and auth

**`rmcp` validates the `Host` header against an allowlist defaulting to loopback only.** A deployment
reached at its real domain answers every health check, every metadata document and every operator
endpoint while refusing every MCP request with a 403 the client reports as a connection failure.
Nothing local reproduces it. `allowed_hosts` in `src/http/mod.rs` derives the list from `PUBLIC_URL`
plus the loopback names, and an entry with no port matches any port, which keeps it working behind a
proxy terminating on 443.

**`rmcp` defaults to legacy session mode**, and sessions left MCP in the 2026-07-28 revision. The
transport is built from `Default` and adjusted, because the config is `#[non_exhaustive]`. It also
injects the whole `http::request::Parts` into the tool context rather than copying axum extensions
onto it, so middleware state lives one level in: `rc.extensions.get::<Parts>()` then
`parts.extensions.get::<Principal>()`.

**`#[tool_handler]` without `router = self.tool_router` builds a fresh router on every call.** It
carries only the first `#[tool_router]` block, so a second block's tools list correctly and answer
`tool not found`. The bare form looks right and compiles.

**RFC 7636 defaults an omitted `code_challenge_method` to `plain`, not S256.** The missing case is
refused rather than read charitably, and metadata advertises `["S256"]` alone.

**An OAuth loopback redirect port has to be persisted, not ephemeral.** Migration 007 compares
`redirect_uri` exactly and never prefix-matches, so a client that binds port 0 registers one port and
comes back on another. The second login fails where the first worked.

**The published wire contract is snake_case throughout and pinned by a test.** A rename on the domain
side once turned every latency field into `-ms` with nothing failing.

## Build and containers

**A cache mount is not part of the image.** Binaries have to be copied to a real path inside the same
`RUN`. A later stage reading `/build/target` finds an empty directory.

**Adding a stage at the end of a Dockerfile changes what an untargeted build produces.** Both compose
services name their `target:` for that reason.

**`COPY` preserves source mtimes.** With a `fn main() {}` stub used for dependency caching, cargo
skipped the real rebuild and the image shipped a 325KB binary with an empty `/models`, passing every
check at a plausible 118MB. Cache mounts do the job now. Any scheme that fakes a source tree to warm
the cache needs an assertion on the built artefact.

**`cargo build --release` at a workspace root builds only the root package.** The image shipped no
client while the size assertion passed on the binary that did get built. Pass `--workspace`.

**`docker run --rm` does not remove a container when the client is killed.** The container keeps
running and keeps cargo's build lock, and every later run blocks on it. `scripts/cargo.sh` sweeps by
owner pid.

**`docker restart` reuses the container's original image**, and so does `docker compose up -d` in
some cases. `--force-recreate` picks up a rebuild. Nothing in the product reports that a running
server is behind the image on disk.

**Compose interpolates every service whether or not its profile is active.** A `${VAR:?...}` on a
profile-gated service breaks `docker compose up -d db` for a deployment that never wanted it.

**Compose `secrets:` ignores `uid`, `gid` and `mode`.** Those fields are Swarm-only. Under plain
`docker compose` the file lands root-owned at 0444 while `docker compose config` prints a
resolved-looking value that was never applied, and `src/crypto/kek.rs` refuses a key file readable by
group or other. The KEK is a plain bind mount. Docker Desktop's virtiofs also reports bind-mounted
files as root inside the container whatever the host-side ownership says.

**A Docker bind mount does not reliably show a host directory deleted and recreated at the same
path.** Measured while fixing `scripts/ingest-test.sh`: a container started right after `rm -rf` then
`mkdir` saw an empty directory on four tries out of six, while twelve tries at twelve distinct paths
were all correct. Two harness steps read a corpus of zero files and still passed their assertions.
Give every case a fresh path.

**The image is large because of model selection.** `EmbeddingModel::BGEBaseENV15Q` resolves to
`model_optimized.onnx` at roughly 2 bytes per parameter, which is fp16 rather than int8: 209MB of
weights against a 35MB binary. The earlier explanation blamed `COPY` dereferencing the HuggingFace
cache's symlinks, and that was checked against the built image and is wrong.

**Rebuilding `lumberroom-builder` changes every checkout on the machine at once.** The image tag and
the `lumberroom-target` and `lumberroom-cargo` volumes are all shared by name, so a rebuild here
reaches another worktree's next `scripts/cargo.sh` run with no warning. When the builder entrypoint
landed, a checkout still on the old `cargo.sh` passed no `BUILDER_UID`, took the default, and chowned
the registry out from under a run that had just claimed it: the symptom was
`couldn't read .../fnv-1.0.7/lib.rs: Permission denied` in the middle of a clippy pass. The
entrypoint now adopts whichever uid already claimed the volume when no caller names one, so an
unconfigured checkout joins rather than fights. Two containers that both start before any marker
exists can still pick different uids and chown in turn; that settles after one cycle and the loser
sees `Permission denied` on a crate source only if it is fetching at that moment. Landed 8
September 2026.

**lumberroom-cloud rebuilds the same builder image.** Both repositories tag their
`Dockerfile.builder` as `lumberroom-builder`, so the image holds whatever the last checkout to build
it put there. On 30 September 2026 the image on the maintainer's machine dated from 8 September. It
carried no clippy, although both Dockerfiles install it, so every run downloaded clippy. And
lumberroom-cloud's lld setting had never reached it, so every build there linked with GNU ld. The two
Dockerfiles now install the same packages and carry the same `lumberroom.linker=mold` label, and
`scripts/cargo.sh` reads the label and refuses to start without mold, naming the rebuild command.
`docker image inspect lumberroom-builder -f '{{.Created}} {{.Config.Labels}}'` shows what is there.

**A root process writing into a claimed build volume leaves files no later run can replace.** The
ownership marker `.builder-owner` records which uid claimed `lumberroom-target` or
`lumberroom-cargo`, and the entrypoint reads the marker rather than walking 41,000 inodes on every
run. So a `docker exec` into a running builder container, which docker gives you as root whatever
the entrypoint did, can leave root-owned artifacts the marker says nothing about. The symptom is
`Permission denied` on a path that plainly exists. The recovery is one line, and it re-claims the
whole tree on the next run:

```bash
docker run --rm -v lumberroom-target:/t -v lumberroom-cargo:/c --entrypoint sh lumberroom-builder \
  -c 'rm -f /t/.builder-owner /c/.builder-owner'
```

## Tests

**A test can pass against the mutation it exists to catch.** The cleanup window test ran through
`cleanup::run`, where a different query was quietly finding the pair. Assert at the layer that owns
the property, and check by mutating the code rather than by reading it.

**A mutex serialises threads, not processes.** Six test binaries sharing one database each had their
own `static SERIAL`, so one binary truncated the table another was asserting against. Every file
passed alone and two failed in a full run, with assertions that read like logic bugs.
`tests/common/mod.rs` takes a Postgres advisory lock, which the session holds and every process sees.
The guard has to be carried out of `setup`; an unused-variable warning was the only tell when it was
not.

**A catalog view in a test sees every database on the cluster.** `pg_locks`, `pg_stat_activity` and
`pg_db_role_setting` are cluster-wide. `tests/migration_lock.rs` counted every advisory lock on the
cluster to prove a failed migration released its own, and passed because `cargo test` runs one binary
at a time. In lumberroom-cloud's copy of the test, the first run of `scripts/cargo.sh test-fast` put
another binary's suite lock beside it and it failed with "left 2 advisory lock(s) held". Filter on
`current_database()`, or on the role or pid the test owns. Two suites from two worktrees sharing the
cluster break the unfiltered form the same way. The probe database carries the pid for the same
reason: a fixed name lets one run's `DROP ... WITH (FORCE)` kill the other's connection.

**The integration suite skips rather than fails with no database reachable**, so a run reporting a low
count is not a pass. Check the split, not the exit code.

**A test that works around the bug reads as coverage.** The alias test called `put_alias` once per
namespace, which is the only way the old one-sided lookup answered from every side. A feature that
works when you record it three times is not working.

**`services::write::run` collapses a near-identical write**, so a duplicate cannot be made through it.
Every duplicate in a real store arrived some other way, and a test that writes one through the normal
path is testing nothing.

**Two rows with identical normalised text also sit at a cosine of 1.0**, so the same pair queues once
as `exact` and once as `paraphrase` under two cluster keys. `scripts/cleanup-test.sh` caught it and
nothing else did.

**`s.index("#[cfg(test)]")` finds the first occurrence, not the module's own test block.** A Python
slice built that way to split a Rust file at its test module duplicated 374 lines, because an inner
`#[cfg(test)]` attribute (on a helper, not the trailing `mod tests`) appeared earlier in the file
than the block the slice was meant to isolate. Anchor on the last occurrence, or on the attribute
immediately preceding `mod tests`. Landed 24 August 2026.

**Root ignores permission bits, so a test that asserts a filesystem refusal passes under it whatever
the code does.** `scripts/cargo.sh` ran cargo as root in the builder container for months.
`config.rs`'s `a_save_that_cannot_complete_leaves_the_live_file_alone` chmods a directory 0500,
probed whether the mode had taken, and returned early when it had not. libtest captures a passing
test's stderr, so the `skipping:` line it printed reached nobody and the gate counted 387 passed with
that test measuring nothing. Two halves to the fix, and either alone leaves the trap armed:
`scripts/lib/builder-entrypoint.sh` drops the container to a non-root uid, and the fixture now panics
naming the precondition rather than returning. A fixture that cannot establish its precondition is
broken, and a broken test has to be loud. Landed 8 September 2026.

## Shell, config and rendering

**Two `.env` quoting traps fail in opposite directions.** Shell scripts that source `.env` with `sh`
strip double quotes, so `AUTH_TOKENS` in its JSON form turns invalid and `["*"]` becomes a glob `sh`
tries to expand. Docker Compose instead expands `$` inside env files, and an argon2 PHC string is
almost entirely `$` segments, so an unquoted `OWNER_PASSWORD_HASH` reaches the server mangled and
every login answers 500. Single-quote every value.

**A `#` comment after a line continuation inside `RUN` comments out the rest of the command.**

**GNU `stat` reads `-f` as `--file-system`.** `stat -f %m || stat -c %Y` returns a filesystem block
count concatenated with the real epoch, and the arithmetic dies under `set -u`. Every macOS run
passed; a review that ran the script in a debian container found it.

**A rendered CSS class with no rule is invisible to every other check.** The contradiction controls
shipped unusable while the handler, the markup and the tests were all correct.

**Duplicated navigation drifts.** The console nav existed in three hardcoded copies, so a new tab
appeared on one page and not the others. One `pages::nav` now.

**`text-decoration` on an ancestor paints through inline descendants.** A `line-through` on a
container struck out a nested span meant to explain the strike, so the explanation itself read as
crossed out. Set `text-decoration: none` on the child; the decoration does not stop at an element
boundary on its own. Landed 24 August 2026.

## Crate specifics

[`rust-spike-findings.md`](rust-spike-findings.md) carries these with the spike that established them. sqlx and pgvector
must resolve the same sqlx version, and pgvector's sqlx support rides an implicit feature absent from
its manifest. fastembed's `embed` takes `&mut self` and is CPU-bound, so the adapter locks and uses
`spawn_blocking`. ONNX Runtime links statically, so the runtime image needs no `.so`. sqlx 0.9's
`query()` takes `impl SqlSafeStr`, which only `&'static str` satisfies, so every statement spells its
columns out rather than sharing a constant.

The embedding weights differ from the pre-rewrite TypeScript build: fastembed pulls
`Qdrant/bge-base-en-v1.5-onnx-Q`, the old build used `Xenova/bge-base-en-v1.5` at q8. Same family,
same 768 dimensions, not provably the same quantised weights. `memory.embedding_model` records which
produced each row, so a mixed store is detectable.


## The personal namespace is `user:me`, not `user:<TENANT_ID>`

`namespaces::user_namespace()` returns the literal `user:me` and takes no argument. It used to
interpolate `TENANT_ID`, which put the personal namespace out of step with the CLI: `lumberroom
write` names `user:me` in its own help text, so on a store configured with any other tenant a write
to `user:me` succeeded and then never appeared in a search, because `default_read_namespaces` asked
for `user:<tenant>` instead.

Nothing errors when this happens. A namespace nobody asks for is not a failure, it is silence, which
is why this survived until someone counted rows.

If you are upgrading a store that ran with a custom `TENANT_ID`, boot warns with the affected
namespace and its row count. Move the rows, then fix any grant that names the old namespace.
