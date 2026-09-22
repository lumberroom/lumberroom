# Decisions

Numbered records of choices that shape the build. One record per decision whose reasoning would
otherwise be lost, or which someone would revisit for the wrong reason a month later. Each carries
what was decided, what it was decided against, what it costs, and the condition under which it gets
reversed.

Phase 1's own decision log is [`DECISIONS.md`](../../DECISIONS.md) at the repository root. It stays
as a record of what was decided then; where a record below reverses part of it, the original text
carries a marker pointing here.

| | Decision | Date | Status |
|---|---|---|---|
| [0001](0001-rust-rewrite.md) | Rewrite the service in Rust, before Phase 2 rather than after | 19 Aug 2026 | accepted, done |
| [0002](0002-built-in-oauth-server.md) | Build the OAuth 2.1 authorization server into lumberroom instead of standing up Logto | 19 Aug 2026 | accepted, verified |
| [0003](0003-grants-in-the-database.md) | An OAuth client's grant is a row in Postgres; a static bearer client's grant stays in `AUTH_TOKENS` | 19 Aug 2026 | accepted, verified |
| [0004](0004-kek-provider.md) | `KEK_PROVIDER` selects `none`, `file` or `env`, defaulting to a refused private write | 19 Aug 2026 | accepted, verified; escrow left open |
| [0005](0005-private-drops-lexical-search.md) | The lexical index covers `open` only, so private content is semantic-only | 19 Aug 2026 | accepted, shipped in migration 004 |
| [0006](0006-console-decides-the-queue.md) | The console decides the ingest queue through the same service call the CLI makes | 20 Aug 2026 | accepted, verified |
| [0007](0007-longmemeval-as-the-retrieval-gate.md) | LongMemEval-S retrieval recall is the standing retrieval gate, run on the embedder the one published comparison used | 20 Aug 2026 | accepted, verified |
| [0008](0008-valid-time.md) | A memory carries valid time beside transaction time, and the as-of query reads it | 20 Aug 2026 | accepted, implemented; amended twice, on the deferral and on rule D1 |
| [0009](0009-aliases-are-query-expansion.md) | Two names for one subject is an alias with valid time, and search expands over the group rather than walking a graph | 20 Aug 2026 | accepted, implemented |
| [0010](0010-registry-history.md) | A registry upsert keeps the value it replaces, readable behind may_read_history and through no tool | 21 Aug 2026 | accepted, implemented |
| [0011](0011-cleanup-proposes.md) | A periodic pass proposes cleanups and never acts, and the model half only ever sees rows at open | 21 Aug 2026 | accepted, verified |
| [0012](0012-cli-distribution.md) | `lumberroom` ships as four raw binaries off a git tag, built from two places | 22 Aug 2026 | accepted, scripted; the darwin leg unverified |
| [0013](0013-delete-splices-the-chain.md) | A delete splices the supersession chain and revives a predecessor only under the caller's grant | 23 Aug 2026 | accepted, implemented; not run live on this branch |
| [0014](0014-nothing-finds-the-pair-a-supersession-needs.md) | The store can record a supersession and nothing finds the pair; four parts, three of them held | 25 Aug 2026 | draft; all four parts implemented and tested, the gating rerun passed, part 4's own result argues for the extractor it was built to avoid |
| [0015](0015-sealed-enumeration.md) | `SealedRepository` gains an enumeration method, reachable only after a caller already passed the whole-store gate | 30 Aug 2026 | accepted, design |
| [0016](0016-audience-is-checked-where-a-token-becomes-a-principal.md) | The audience on an access token is checked where the token becomes a principal, and a token with no audience is admitted until the population drains | 8 Sep 2026 | accepted, implemented |
| [0017](0017-a-fact-can-expire.md) | A live read answers both clocks, so a fact can be retired with no successor and brought back | 15 Sep 2026 | accepted, implemented |
| [0018](0018-one-review-queue.md) | One queue shape with a source field, one decide route, a ledger that remembers "both are fine", and a proposal seam the engine leaves empty | 22 Sep 2026 | accepted, implemented |

**On "verified".** All four gates ran against a live server on 20 August 2026 and passed. An OAuth
flow completed end to end, a private row was encrypted and read back, and a replayed refresh token
was refused. `scripts/oauth-flow-test.sh` settles 0002 and 0003 at 43 PASS, `scripts/policy-test.sh`
settles 0004 at 20 PASS, and `VERIFY.md` carries the output of both verbatim. No hosted MCP client
has seen any of it, because nothing is deployed.

0005 is the exception in the list, and only because it cost nothing to land: every row in the store
is `open` today, so building the index with a `WHERE` predicate changed no behaviour on the day it
shipped. Its reversal is cheap only until private encryption turns on for new writes.

Two records depart from a written spec, and each says so in its own text rather than leaving a
reader to find the mismatch. 0002 supersedes
[`docs/specs/phase-2-surfaces.md`](../specs/phase-2-surfaces.md) §2, which called Logto the Phase 2
baseline; what that spec settled was which surfaces force an authorization server, and that finding
stands untouched. 0004 departs from
[`docs/specs/phase-3-policy-encryption.md`](../specs/phase-3-policy-encryption.md) §3, which
specified OCI Vault, because the product now has to come up wherever compose runs.
