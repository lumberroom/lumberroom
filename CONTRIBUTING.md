# Contributing

Pull requests are welcome. Read [`docs/traps.md`](docs/traps.md) before you spend a day on something
somebody already lost a day to, and [`docs/architecture.md`](docs/architecture.md) before you add a
module.

## Building and testing

You can run cargo directly if you have a Rust toolchain. The maintainer's machine has none, so
everything below goes through a builder image that carries `g++` (ONNX Runtime links libstdc++ and
`rust:slim` ships gcc without it) and the OpenSSL headers. Either way the `-j 1` rule applies on any
machine tight on memory.

```bash
docker build -t lumberroom-builder -f Dockerfile.builder .   # once, and again after any change to
                                                       # Dockerfile.builder or scripts/lib/builder-entrypoint.sh
docker compose up -d db                                # Postgres 16 + pgvector on 127.0.0.1:5432

./scripts/cargo.sh check --all-targets
./scripts/cargo.sh test -j 1
./scripts/cargo.sh test -j 1 -p lumberroom
```

Cargo runs in there as your own uid, never as root, because root ignores permission bits and every
test that asserts a refusal from the filesystem passes under it whatever the code does. One had been
inert for months. `scripts/lib/builder-entrypoint.sh` does the drop and takes ownership of the two
shared volumes once, which the first container after a rebuild reports on stderr and which costs
about a second. The image tag and both volumes are shared by name across every checkout on the
machine, so that rebuild reaches other worktrees too.

The first image build downloads 209MB of bge-base-en-v1.5 weights from huggingface.co into a BuildKit
cache mount. Later builds copy from that mount, and the release image carries the weights at
`/models`, which is where `MODEL_CACHE_DIR` points.

`-j 1` is not optional under memory pressure. Linking the lib-test and integration binaries at the
same time gets the linker OOM-killed in the container: `collect2: fatal error: ld terminated with
signal 9`. It reads as a compile error and it is a memory limit.

The integration suite runs against a real Postgres in its own `lumberroom_rust_test` database with the hash
embedder, so it downloads nothing. It **skips rather than fails** when no database is reachable, so a
run reporting 0 tests is not a pass. Check the count. Tests serialise themselves on a Postgres
advisory lock, because each one truncates that database and six test binaries are six processes. A
mutex serialises threads and would not do.

Run one test:

```bash
./scripts/cargo.sh test -j 1 --test integration <name>   # one integration test
./scripts/cargo.sh test -j 1 --lib <name>                # one unit test
```

## The acceptance gates

These run against a live server rather than mocks, and they are the definition of done for their
phases:

```bash
./scripts/done-when-test.sh      # a fact survives into a fresh session
./scripts/oauth-flow-test.sh     # register, PKCE, consent, token, MCP call, replay refused
./scripts/policy-test.sh         # one client provably cannot see what another can
./scripts/correction-test.sh     # a correction does not resurface as a contradiction
```

The gates drive `lumberroom` from PATH, the same binary a customer installs, so a gate proves the
thing that ships rather than a fixture.

There used to be a second client here, `bin/lumberroom.mjs`, written against node built-ins alone.
It was kept because it shared no types with the server and so could not be accidentally accommodated
by it, and it did earn that place: it caught protocol bugs the Rust tests could not. It also fell
behind. It never learned to read an authorization server's metadata document, so it could not log in
against a deployment whose issuer differs from its API base, and a gate running a client that cannot
complete a login proves less than it looks like it does. Removed in 0.4.0.

What it was protecting against is real and is now unprotected: the client and the server share
types, so a change to both at once can look correct from inside. A second implementation would
earn its place again the day somebody keeps it current.

## What a pull request needs

Say which gate or test command you ran and paste what it printed. "Implemented" and "verified" are
two different claims; if you did not run it, write implemented and name the gate that would settle
it. A number is either an observation with a place it came from or a design target, and the text
should say which.

Migrations are forward-only. `sqlx` embeds them at compile time, so once a newer binary has migrated
a store, an older image cannot boot against it. Write the rollback as a forward migration.

## Architecture rules

Ports and adapters, and one rule carries it: **domain and services never import from adapters.** A
service asks a `MemoryRepository` for rows and does not know Postgres exists, which is what makes a
second storage implementation possible. Two exceptions are deliberate and narrow: services may import
`adapters::auth`, which is grant arithmetic over a `Principal`, and `crypto`, which is key material
this layer has to reason about to refuse a write it cannot honour.

`sqlx` runs **without** the compile-time macros. Use `query`/`query_as` with `.bind()`. Dynamic SQL
built with `format!` is rejected by the type system on purpose. Build literal column lists, and for
genuinely generated DDL use `sqlx::raw_sql(sqlx::AssertSqlSafe(...))` and say why in a comment.
`src/adapters/postgres` is the only module containing SQL.

**Every setting lives in `src/config.rs` and is validated at boot.** An environment variable read
anywhere else is wrong. `PUBLIC_URL` is the single source for every externally visible URL, because
an issuer that disagrees with the host behind a reverse proxy stays invisible until a real client's
discovery fails.

**Auth modes compose, they do not exclude.** `AUTH_MODE` selects what is accepted on top of static
bearer tokens, which are honoured in every mode whenever `AUTH_TOKENS` is set. `token` is tokens
alone, `oauth` adds the built-in authorization server, `oidc` adds an external issuer's JWTs. Every
mode resolves to one `Principal` and nothing else.

**A grant has two axes.** A namespace glob carries a sensitivity ceiling, `open < private < sealed`.
A bare string means a ceiling of `open`. The sensitivity filter runs **inside the query**, never as a
pass over results: a row a client may not see must never enter that client's process.

## Prose

The same rules cover documentation, code comments, commit messages and test names.

- No em dashes anywhere. Grep for them before you push: `grep -rP '\x{2014}'` should return nothing.
- Active voice with a human subject. No inanimate thing performing a human verb.
- No adverbs where a plain verb works, and no "Note that" or "Here's what" openers.
- Comments explain why and flag traps. They do not narrate what the code obviously does.
- Vary sentence length. Cut anything that reads like a pull-quote.

Calibrate against [`docs/decisions/0001-rust-rewrite.md`](docs/decisions/0001-rust-rewrite.md) for
documents and `src/domain/policy.rs` for comments.

## Decisions

A choice whose reasoning would otherwise be lost gets a record in `docs/decisions/`, in the shape
`0001` sets: the decision, the context that forced it, what lost and why, the costs accepted, what it
is explicitly not for, and the condition under which it gets reversed. When a new decision
contradicts an old one, mark the old record superseded rather than editing it to look consistent.

## Reporting a vulnerability

Use GitHub private vulnerability reporting on
[the repository](https://github.com/lumberroom/lumberroom): Settings, Security, "Report a
vulnerability". Do not open a public issue. [`SECURITY.md`](SECURITY.md) has the detail.
