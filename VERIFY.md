# Verification

The gates: what each one checks, why it exists, how to run it, and what a pass looks like. Every
claim this project makes about behaviour has a script here that settles it, so you can disagree with
a number by re-running the thing that produced it. Each gate names what it proves and what it does
not, and the last section carries the ones with no run behind them.

## Running a gate

Most gates stand up their own server against their own database and drop both when they exit, which
[`scripts/lib/scratch-server.sh`](scripts/lib/scratch-server.sh) does for them. That file refuses
port 8787 and refuses a database name equal to `POSTGRES_DB`, because a gate that writes
nonce-tagged rows into a real store leaves facts behind that reach the next session's digest. You
need Docker, `curl`, `openssl` and `node` on the host, `POSTGRES_PASSWORD` set in `.env`, and the
server image built once:

```bash
docker compose build server        # builds lumberroom-server:0.4.0, which the gates run
docker compose up -d db            # Postgres 16 + pgvector; the gates reuse it if it is up
```

Then run any gate with no arguments. Each takes `--port N` to move off its default and `--keep` to
leave the container and database in place. Every gate prints the same shape: numbered steps, a
`PASS` or `FAIL` line per assertion, a count, and a final `<name> PASSED` or `<name> FAILED`. A pass
is exit 0 with a non-zero PASS count and zero FAIL lines. There is no `SKIP`, because a harness that
can skip reports success while testing nothing.

## The gates

### `scripts/done-when-test.sh`

The product's own done-when criterion, from the system PRD: state a fact to Claude Code on Monday,
open a fresh session on Wednesday, and `context_bootstrap` surfaces it without you mentioning it.
Four steps against a running server, using the real `claude` CLI. Session A states a nonce-tagged
fact and the model chooses to call `memory_write` on its own; the gate asserts the row landed and
that the `unprompted` counter moved. Session B is a separate process with a separate context, and
its question never names the fact. A pass means a fresh session recovered it through the
SessionStart hook's digest.

```bash
LUMBERROOM_URL=https://memory.example.com LUMBERROOM_TOKEN=<token> ./scripts/done-when-test.sh
```

This is the one gate that needs a server you point it at, since it drives a real MCP client rather
than curl. It touches nothing in `~/.claude`: the MCP server and the hook go in per invocation with
`--mcp-config` and `--settings`.

### `scripts/oauth-flow-test.sh`

Proves the built-in OAuth 2.1 authorization server end to end, with no browser and no loopback
listener: the authorization code arrives in a `Location` header the script reads directly. It stands
up a scratch server in `AUTH_MODE=oauth` on port 8793 with an owner password minted for the run.
Thirteen steps, 43 assertions:

```
 1/13  protected-resource metadata, both paths
 2/13  an unauthenticated call to /mcp is a 401 with a WWW-Authenticate pointer, not a 200
 3/13  authorization-server metadata advertises S256 and refuses to offer plain
 4/13  dynamic client registration
 5/13  authorize, sign in, and consent: the code arrives in a redirect
 6/13  the token exchange, form encoded
 7/13  the same code cannot be redeemed twice
 8/13  a wrong PKCE verifier is refused
 9/13  a redirect_uri that does not match exactly is refused
10/13  the access token opens the MCP surface: initialize, tools/list, one real call
11/13  refreshing the access token
12/13  the new access token works
13/13  the rotated-out refresh token is refused on reuse
```

Real clients check both metadata paths, and RFC 7636 makes the omitted default `plain`, which is why
steps 1 and 3 exist. Steps 7 and 13 are the two that matter under attack: a replayed code revokes
the whole token family it issued, and a reused refresh token is refused.

This gate has no live mode. Step 4 self-registers a client on every run and no HTTP route deletes
one, so a run against a real deployment leaves a row behind permanently;
[`scripts/purge-oauth-flow-test-clients.sh`](scripts/purge-oauth-flow-test-clients.sh) removes rows
a run like that left. **What it does not prove:** that Claude.ai's or ChatGPT's own client code
completes this flow. It exercises the wire protocol from curl, and curl is not a browser.

### `scripts/policy-test.sh`

The system PRD's policy exit criterion: one client provably cannot see a fact another can. Eight
steps, 20 assertions, against a scratch server with two credentials, one full and one narrow.
Step 2 refuses the narrow credential four different ways, because a namespace it cannot reach has to
be invisible to search, to the digest inventory, to `registry_get` and to a write. Step 3 is the
two-axis case: a namespace the narrow grant names, with a ceiling that refuses the content anyway.
Step 5 asserts sealed content comes back as ciphertext to a client without `sealedCapable`. Step 6
drives the credential tripwire, which refuses a live-looking secret at `open` and names the rule
without echoing the secret back. Step 7 asserts both denials reach `tool_calls`, since a silent
denial is unauditable, and step 8 checks the operator routes answer the narrow credential without
describing the rest of the store. Run it after every grant change. `--live` points it at
`LUMBERROOM_URL` with two configured tokens, and a live run deletes every row it wrote before it
exits, so the full credential needs `mayDelete` there.

### `scripts/correction-test.sh`

The correction exit criterion: a correction made once does not resurface as a contradiction later.
Six steps, 13 assertions. A fact is written, then corrected with `supersedes`. Search answers the
corrected value and not the old one, and the old row survives with `superseded_by` set, because
history is retired rather than deleted. Step 5 is the numeric guard: two texts differing by one
digit run have to stay two rows, and the older has to come back flagged in `possible_conflicts`.
Step 6 resolves that flagged conflict through `lumberroom supersede` and asserts the end state
matches an inline correction.

### `scripts/cleanup-test.sh`

The half `tests/cleanup.rs` cannot reach: the CLI reading the candidate list, the queue printing
something a person can act on, and apply moving rows through the HTTP routes. Six steps, `--no-model`
throughout, so nothing calls a provider and no key is needed.
Step 3 is the one that makes a cadence safe: a second run over the same store queues nothing. Step 5
asserts apply retires through supersession and the survivor still answers. Step 6 asserts a client
without `mayIngest` can neither run the pass nor read the queue.

### `scripts/ingest-test.sh`

Transcript ingestion, seven steps over fixture transcripts the script writes itself under
`target/ingest-test-work`. It never walks a real `~/.claude/projects`, and it ignores the URL and
token in the environment on purpose: `ingest plan` opens a run row, `ingest submit` moves
watermarks, and both are writes.
Steps 1 to 3 check what ingestion refuses to look at: a digest span yields nothing, attachment
entries are excluded and counted by subtype, and a memory-tool result is excluded while a file read
is not. Step 4 refuses a sensitive path before the file is opened, step 5 is the watermark that
makes a second plan over an unchanged fixture cut zero spans, and step 7 submits a batch and follows
its chunks through.

### `scripts/eval-longmemeval.sh`

The standing retrieval gate ([decision
0007](docs/decisions/0007-longmemeval-as-the-retrieval-gate.md)). It writes through the real
`memory_write` tool into real Postgres and searches through the real `memory_search`, on a scratch
server and database created and dropped per run. The dataset is not checked in; the fetch command
sits in the script's header, and `--limit N` gives a smoke run before the full 500 questions.

```bash
./scripts/eval-longmemeval.sh --dataset longmemeval_s_cleaned.json
```

Results, and what they do and do not say, live in [`docs/benchmarks.md`](docs/benchmarks.md).
[`scripts/eval-metrics-check.sh`](scripts/eval-metrics-check.sh) settles the arithmetic: it
recomputes recall and NDCG from a published set of per-question rows and compares them against that
set's own aggregates, which rules the metric functions out as the source of a surprising number.

## The test suite

```bash
./scripts/cargo.sh test -j 1                # the server crate and the integration suite
./scripts/cargo.sh test -j 1 -p lumberroom  # the client crate
```

Last observed at 870 tests passed, 0 failed, in the server crate, from
`./scripts/cargo.sh test -j 1 -p lumberroom-server`, on 24 August 2026. The client crate's count
moves with every change; run `./scripts/cargo.sh test -j 1 -p lumberroom` and read the last line. `-j 1` is not optional: plain
`./scripts/cargo.sh test` links the lib-test and integration binaries at once, and the container's
memory limit kills the linker with a message that reads as a compile error. A green suite with a
parked test is not a green suite, so `grep -rn '#\[ignore' src/ tests/` returning nothing is part of
the check.

## Numbers that are observations

The recall monitor compares an approximate HNSW scan against an exact one over the same corpus.
Re-run on 21 August 2026 against 40,001 seeded rows: mean recall@10 between 0.981 and 0.988 across
five runs, with no true nearest neighbour missed in 1,900 probes. Method and full output:
[`docs/research/recall-monitor.md`](docs/research/recall-monitor.md).

**Read the monitor's timings before its recall figure.** The exact arm has twice reported a
self-comparison: once because `SET LOCAL enable_indexscan = off` ran outside a transaction, which
Postgres answers with a warning and no effect, and once at `k=1`, where the planner declines the
index and both arms run sequentially. Treat any recall figure whose `index_ms` and `exact_ms` land
within a fraction of a percent of each other as a self-comparison. The monitor also samples `open`
rows only, and nothing that renders its report says so.

**The filtered-search truncation finding stands**, from a direct reproduction rather than from the
monitor. At 40,000 rows, with a namespace holding 0.5% of them, a query asking for 10 rows returned
zero: HNSW pulled 40 candidates and the namespace filter removed all 40, with no error. Migration
`003` sets `strict_order` and `ef_search=100` so the setting travels with the schema. See
[`docs/research/pgvector-at-scale.md`](docs/research/pgvector-at-scale.md). Every other retrieval
figure lives in [`docs/benchmarks.md`](docs/benchmarks.md) with the run that produced it.

## Open gates

Nothing has run behind these. Read them at the same weight as the passes above.

- **A browser or mobile MCP client has never driven the OAuth flow.** `oauth-flow-test.sh` proves
  the wire protocol from curl, and Claude Code's fallback probing masks a class of metadata bug that
  shows up only against a hosted client.
- **`AUTH_MODE=oidc` has never run against a live external issuer.** Tests cover it against a
  configured issuer, and the switch procedure in [`deploy/logto.md`](deploy/logto.md) comes from the
  issuer's documentation rather than from a run.
- **The CLI has no darwin release leg.** The Rust client builds for Linux inside the builder image
  and reaches a Mac only through a container.
- **The dedupe and conflict thresholds are design targets.** `DEDUPE_THRESHOLD` at 0.97 and
  `CONFLICT_THRESHOLD` at 0.90 were picked before any real data existed. Calibration needs a few
  hundred real rows and a person reading the pairs above 0.85, per
  [`docs/specs/phase-4-quality.md`](docs/specs/phase-4-quality.md) §2.
- **Nothing has been load tested.** `conflicts()` is O(n²) with no index able to help, seconds at a
  few thousand rows, and unmeasured.
- **Latency has no measurement at a store size worth the name.** The README's figures came from a
  store holding tens of rows, and a private row costs a ciphertext round trip nothing has timed.
- **A grant change leaves no audit row.** Consent overwrites the client profile in place, so no gate
  can assert what a client used to be allowed to do.
