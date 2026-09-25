# Lumberroom

[lumberroom.cloud](https://lumberroom.cloud)

One memory that every AI tool you use can read from and write to, with you deciding what each one is
allowed to see.

You run seven AI surfaces. Each keeps its own memory. None of them share. You re-explain your setup
to ChatGPT, then again to Claude, then again to your coding agent, and when you correct one of them
the others never find out. Lumberroom holds the facts once, on infrastructure you own, and every tool
reads from the same place.

Three layers, doing three different jobs:

- **Memory remembers.** Fuzzy semantic recall. Commodity, kept deliberately thin and swappable.
- **Registry knows.** Exact structured facts with a canonical key and provenance. A memory holds
  "Dana mentioned her box runs Ubuntu"; the registry holds `machines.desktop.os = Ubuntu 26.04`,
  confirmed by you, on a date, superseding an older value.
- **Policy decides.** Which tool sees which facts. The layer that makes it safe to store anything
  worth storing.

The value is in the second and third. Full thesis: [`docs/prd/system-prd.md`](docs/prd/system-prd.md).

```
Claude Code, Hermes, OpenWebUI, lumberroom CLI ──── static bearer token ────┐
                                                                      │
Claude.ai, Cowork, mobile, ChatGPT ── OAuth 2.1 + PKCE ───────────────┤
                                                                      ▼
                             Caddy (auto-TLS) ──▶ lumberroom (Rust: axum + rmcp)
                                                        │
                                                        ▼
                                              Postgres 16 + pgvector (127.0.0.1)
```

One Rust binary, a Postgres database with pgvector, and two clients that talk to them. Ten MCP tools
behind four capabilities, mounted at `/mcp`. A built-in OAuth 2.1 authorization server for the
surfaces that need a browser, with static bearer tokens honoured beside it whatever `AUTH_MODE` says.
Envelope encryption for `private`, client-side encryption for `sealed`. A console at `/console` for
the decisions a person has to make.

## Install the server

```bash
git clone https://github.com/lumberroom/lumberroom.git && cd lumberroom
sudo ./deploy/install.sh
```

That is token mode on loopback: the server binds `127.0.0.1:8787`, opens no public port, and prints
the client token with the command to run on your Mac. Add `--domain` and `--auth-mode oauth` when a
browser has to reach it. The installer pulls
`ghcr.io/lumberroom/lumberroom-server:0.4.0` and falls back to building from this tree when the
pull fails; `--build-local` skips the pull. It pins a version rather than tracking `latest`, because
a memory store that upgrades itself while you sleep is not a feature.

Moved: the engine used to publish as `ghcr.io/the-cybersapien/lumberroom-server`. That image stays
public, frozen at 0.4.0, and takes no new versions; pull `ghcr.io/lumberroom/lumberroom-server`
instead.

## Install the client

The client runs on the machine holding your transcripts and your credentials, and talks to a server
you host. Four ways in, in the order most people want them.

```bash
brew install lumberroom/lumberroom/lumberroom
```

Installed from the old `the-cybersapien/lumberroom` tap? The next `brew update` follows the move and
renames the tap. Homebrew stops trusting a tap whose owner changed, so run
`brew trust lumberroom/lumberroom` once, or `brew upgrade` refuses the formula.

```bash
cargo install lumberroom
```

A binary, with no package manager. Four targets, macOS and Linux on arm64 and x86_64:

```bash
tag=v0.4.0; target=aarch64-apple-darwin      # or x86_64-apple-darwin,
                                             # aarch64-unknown-linux-musl, x86_64-unknown-linux-musl
base=https://github.com/lumberroom/lumberroom/releases/download/$tag
curl -fsSLO "$base/lumberroom-${tag#v}-$target.tar.gz"
curl -fsSL "$base/SHA256SUMS" | grep "$target.tar.gz" | shasum -a 256 -c -
tar -xzf "lumberroom-${tag#v}-$target.tar.gz" && install -m 755 lumberroom ~/.local/bin/
```

Check the archive before you run what came out of it. Every release carries one `SHA256SUMS` over
every asset, written by the job that collected them all.

On a machine where `apt` or `dnf` is how software arrives, the release also carries a `.deb` and an
`.rpm` per architecture. Both hold the musl binary, statically linked, so neither declares a runtime
dependency:

```bash
sudo dpkg -i lumberroom_0.4.0-1_amd64.deb     # or lumberroom_0.4.0-1_arm64.deb
sudo rpm -i lumberroom-0.4.0-1.x86_64.rpm     # or lumberroom-0.4.0-1.aarch64.rpm
```

Then point it at your server:

```bash
lumberroom doctor
```

`doctor` reports the endpoint, whether your credential is accepted, and which tools it opens. It is
the first command to run and the one to run when something is wrong.

To wire a Mac into Claude Code, which installs the MCP server, the session hook and the CLAUDE.md
rule in one pass:

```bash
./client/wire-mac.sh --url http://127.0.0.1:8787 --token <token>
```

Then prove the loop, which states a fact in one session and recovers it in a fresh one:

```bash
LUMBERROOM_URL=http://127.0.0.1:8787 LUMBERROOM_TOKEN=<token> ./scripts/done-when-test.sh
```

Back up `secrets/lumberroom-kek` somewhere this box does not hold. Losing it makes every `private` row
unreadable, in the database and in every backup.

## Where to go next

| | |
|---|---|
| [docs/faq.md](docs/faq.md) | The questions people actually ask: deploying, granting a client, keys, refusals |
| [docs/managing.md](docs/managing.md) | Running a live store: approving a client, changing what it may reach, the two queues |
| [DEPLOY.md](DEPLOY.md) | The runbook: both deploy paths, the KEK, backups, troubleshooting |
| [deploy/oauth.md](deploy/oauth.md) | The OAuth production path, per surface, with the consent screen |
| [docs/permissions.md](docs/permissions.md) | Every grant field, both axes, and which tools each capability opens |
| [docs/connect-claude-code.md](docs/connect-claude-code.md) | Wiring Claude Code on a Mac, end to end |
| [docs/architecture.md](docs/architecture.md) | The ports-and-adapters shape and where each part lives |
| [docs/decisions/](docs/decisions/) | Why each part is shaped the way it is, one record per choice |
| [docs/traps.md](docs/traps.md) | Findings that cost real time, and what to do instead |
| [VERIFY.md](VERIFY.md) | The gates: what each checks, how to run it, what a pass looks like |
| [docs/benchmarks.md](docs/benchmarks.md) | Every retrieval figure, with the run that produced it |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Building, testing, and what a pull request needs |
| [docs/README.md](docs/README.md) | The full index: PRDs, phase specs, research, design |

## The ten tools

Every tool sits behind one grant, and `src/mcp/capability.rs` holds the single table that decides
which. `tools/list` filters on it, so a model never sees a tool its credential cannot call, and the
service checks the grant again on the call. Signatures are additive: they have gained arguments and
never lost one.

| Grant | Tools |
|---|---|
| Open to every authenticated client | `context_bootstrap`, `memory_search`, `memory_write`, `registry_get`, `alias_list` |
| `mayDelete` | `memory_forget` |
| `mayReadHistory` | `memory_history`, `registry_history` |
| `registryWrite` | `registry_set`, `alias_set` |

Namespace and sensitivity still apply inside every call. Full signatures and what each one does:
[docs/permissions.md](docs/permissions.md).

`possible_conflicts` is what makes a correction stick. When a write lands close to an existing fact
but not close enough to collapse into it, the server returns the neighbours it refused to merge, and
the model calls `memory_write` again with `supersedes`. The store cannot tell a correction from a
restatement; the model in the conversation is the only party that can.

## Namespaces and sensitivity

- `user:me` facts about you and how you work
- `project:<slug>` facts scoped to one codebase
- `global` facts true everywhere: infrastructure, conventions, credential locations
- `personal:*` and `credentials:*` classify above `open` by default

A grant pairs a namespace glob with a sensitivity ceiling, on each of the read and write axes.
`{"namespace": "project:*", "max": "private"}` grants every project namespace up to `private`. Two
patterns matching one namespace resolve to the more generous ceiling.

The three levels are three different mechanisms:

- **`open`** is stored in the clear, indexed lexically and semantically.
- **`private`** is encrypted at rest under a per-row key and drops out of the lexical index
  ([decision 0005](docs/decisions/0005-private-drops-lexical-search.md)).
- **`sealed`** arrives encrypted by the client. The server holds no key for it and cannot read it
  under any circumstance.

**What `private` does not protect**, stated here rather than buried in a research file. The embedding
stays plaintext, because there is no ANN index over ciphertext and an unsearchable level would push
everything worth protecting back into `open`. Published inversion work recovers most of a short text
from its embedding: Morris et al. report 92% exact recovery of 32-token inputs with black-box query
access, a 2025 reproduction confirms it, and later work drops the need for the victim's model. Those
are figures from the literature, not measurements on this system. The claim lumberroom makes is narrow
and it is the one to hold it to: `private` protects the verbatim text of a row from whoever holds the
database, and leaks its gist. It protects nothing against the live server, which decrypts to answer a
search. Evidence:
[`docs/research/encryption-and-sensitivity.md`](docs/research/encryption-and-sensitivity.md) §1.

## Operating it

`lumberroom help` is the authoritative list. One client, the Rust one, shipped as a release
binary and on the Homebrew tap.

```bash
lumberroom doctor                       # connectivity, auth, readiness, tool list
lumberroom clients                      # registered OAuth clients, how each registered, consent state
lumberroom search "how do we deploy"
lumberroom write "..." --namespace user:me --tags preference
lumberroom review [--source conflict,stale,proposal] [--limit N] [--days N] [--min-similarity X] [--json]
lumberroom review --dates                # undated facts whose own text names a day
lumberroom review --registry             # registry entries due for review, non-canonical keys
lumberroom review --dismissed            # lists the dismissed-pair ledger
lumberroom review --undismiss <a>,<b>    # removes one pair from the ledger
lumberroom registry get|set|alias ...
lumberroom stats [--hours 168] [--by-client]
lumberroom export --obsidian ~/vault
lumberroom seal <key> --namespace credentials:aws
lumberroom tools
```

The `lumberroom` Rust CLI (`crates/lumberroom`) carries the archive commands:

```bash
lumberroom archive export ~/store.lumber --passphrase-stdin
lumberroom archive import ~/store.lumber --passphrase-stdin --dry-run
```

The console at `/console` carries the decisions a person has to make: what each client may reach, the
ingest queue, the cleanup queue, aliases, and writing or correcting a fact by hand. It runs only in
oauth mode, because it checks the owner password. [docs/managing.md](docs/managing.md) is the guide.

`lumberroom stats` answers the question that matters: how often does a model call these tools on its
own? Every call writes a row, refused ones included, and `unprompted` separates the model deciding
from you or the hook forcing it.

Health endpoints: `/healthz` needs no credentials, `/readyz` reports the embedder and checks the
schema dimension against the configured one, `/statsz` needs a token, and `/admin/whoami` answers
what the credential you present resolves to from the code path that enforces it.

## Development

```bash
cp .env.example .env                     # then set POSTGRES_PASSWORD
docker compose up -d db                  # Postgres 16 + pgvector on 127.0.0.1:5432
docker build -t lumberroom-builder -f Dockerfile.builder .   # again after any change to
                                                            # Dockerfile.builder or scripts/lib/builder-entrypoint.sh
./scripts/cargo.sh test -j 1
```

`-j 1` is not optional: linking the lib-test and integration binaries at the same time gets the
linker OOM-killed in the container, and it reads as a compile error rather than a memory limit. The
suite runs against a real Postgres in its own database with the hash embedder, and it skips rather
than fails when none is reachable, so read the count rather than the exit code.
[CONTRIBUTING.md](CONTRIBUTING.md) carries the rest, and [docs/traps.md](docs/traps.md) carries what
has already cost time.

One rule carries the architecture: **domain and services never import from adapters.** A service asks
a `MemoryRepository` for rows and does not know Postgres exists, which is what makes a second storage
implementation possible. See [docs/architecture.md](docs/architecture.md).

## What is not built yet

The OAuth wire protocol has a gate and the browser and mobile clients that depend on it have none,
which is the largest gap. Beyond that:

- **0.97 is still a guess.** Only the lower similarity band has a measurement behind it.
  `DEDUPE_THRESHOLD` was picked rather than calibrated, and
  [`docs/specs/phase-4-quality.md`](docs/specs/phase-4-quality.md) §2 is the procedure that would
  settle it. A numeric guard is what makes being wrong about it survivable.
- **Sealed items have no bulk listing.** `lumberroom seal` and `unseal` work one key at a time, and
  nothing enumerates what is stored.
- **RFC 8707 audience binding is not enforced on the opaque token path.** The token endpoint records
  the resource a client asked for and validation reads it without comparing it.
- **Client registration has no rate limit.** Registration is open by design and a registered client
  holds nothing until the owner consents, so the cost is rows that nothing purges.
- **A grant change leaves no audit row.** The console and the consent screen both overwrite the grant
  in place, so there is no record that a client used to hold less.
- **KEK rotation and escrow.** `KEK_ID` is written on every row so a rotation is distinguishable from
  data loss, and nothing rewraps.

Which gates cover what, and which are still open: [VERIFY.md](VERIFY.md).

## Layout

```
docs/               PRDs, phase specs, decisions, research, design
src/domain/         types, errors, namespace rules, policy, canonical keys, tripwire. No I/O
src/ports/          one file per port: memory, registry, tool_calls, sealed, oauth, embedder
src/services/       bootstrap, search, write, forget, review, export, registry, recall
src/crypto/         the KEK provider trait and the per-row envelope
src/authserver/     the built-in OAuth 2.1 server: routes, consent pages, sessions, login limiter
src/adapters/       postgres (the only module with SQL), embedding, auth
src/mcp/            tool registration and descriptions; the tool list is per credential
src/http/           axum routes, the admin surface, the well-known documents; MCP mounts at /mcp
src/console/        the console at /console: reading, write, registry, aliases, queues, clients
migrations/         SQL, applied at boot by sqlx
tests/              integration suite, against a real database
crates/lumberroom/    the client: the CLI, the session hook, transcript ingestion, the cleanup daemon
client/             wire-mac.sh, the SessionStart hook, the OpenWebUI filter, the eval fixture
deploy/             install.sh, Caddyfile, backup.sh, oauth.md, Oracle and Logto notes
scripts/            cargo.sh, lumberroom.sh, the acceptance gates and the eval harness
```

## Licence

Apache-2.0. The full text is in [LICENSE](LICENSE), and
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) lists the third-party components this project
depends on and the terms they come under.
