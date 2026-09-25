# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

**lumberroom**, a personal memory control plane. One always-on MCP server holds durable facts; every AI
surface the owner uses reads and writes the same store, with per-client policy deciding what each may
see. Three layers doing three jobs: memory remembers (fuzzy recall, commodity), registry knows (exact
facts with a canonical key and provenance), policy decides (per-client grants). The value is in the
second and third.

The directory is named `memoryEngine` and the product is called lumberroom. Repo:
`github.com/lumberroom/lumberroom`, `main`.

Read in this order before starting work: `docs/architecture.md`, then `docs/decisions/README.md`.

## Commands

Build, test and gate commands live in `CONTRIBUTING.md`.

## Architecture

Ports and adapters. **One rule carries it: domain and services never import from adapters.** A
service asks a `MemoryRepository` for rows and does not know Postgres exists, which is what makes a
second storage implementation possible. The exceptions are deliberate and narrow: services may import
`adapters::auth` (pure grant arithmetic over a `Principal`) and `crypto` (key material this layer has
to reason about to refuse a write it cannot honour). See `docs/architecture.md`.

```
src/domain/     types, errors, namespace rules, the two-axis policy model, canonical registry keys,
                the credential tripwire. No I/O anywhere in here
src/ports/      one file per port. The contract adapters implement and services consume
src/services/   the use cases: bootstrap, search, write, registry, forget, review, export, recall
src/adapters/   postgres (the ONLY module containing SQL), embedding, auth
src/authserver/ the built-in OAuth 2.1 authorization server: routes, consent pages, session, limiter
src/crypto/     envelope encryption and the KEK provider
src/mcp/        tool registration and the tool descriptions
src/http/       axum routes; the MCP transport mounts at /mcp
migrations/     SQL, applied at boot by sqlx
```

`src/ports/` is split one file per port on purpose: it lets parallel work on the memory store and on
the authorization server proceed without touching the same file.

**Auth modes compose, they do not exclude.** `AUTH_MODE` selects what is accepted *on top of* static
bearer tokens, which are honoured in every mode whenever `AUTH_TOKENS` is set. `token` is tokens
alone, `oauth` adds the built-in authorization server, `oidc` adds an external issuer's JWTs. Making
them exclusive would break the CLI and the three header-only surfaces the day OAuth is switched on.
Every mode resolves to one `Principal` and nothing else.

**A grant has two axes.** A namespace glob carries a sensitivity ceiling (`open < private < sealed`).
A bare string still means a ceiling of `open`, which is what kept every Phase 1 grant valid when the
axis landed. The sensitivity filter runs **inside the query**, never as a pass over results: a row a
client may not see must never enter that client's process.

**Every setting is in `src/config.rs` and validated at boot.** Adding an environment variable read
anywhere else is wrong. `PUBLIC_URL` is the single source for every externally visible URL, because
an issuer that disagrees with the host behind a reverse proxy stays invisible until a real client's
discovery fails.

`sqlx` is used **without** the compile-time macros. Use `query`/`query_as` with `.bind()`. Dynamic SQL
built with `format!` is rejected by the type system on purpose; build literal column lists, and for
genuinely generated DDL use `sqlx::raw_sql(sqlx::AssertSqlSafe(...))` and say why in a comment.

## Traps that already cost real time

`docs/traps.md` carries them, with the evidence and what to do instead.

## Working here

**No AI attribution, anywhere, ever.** Not in commits, not in docs, not in comments, no co-author
trailers. Commit as the owner.

**Never commit or push unless asked.** Getting the work durable is not a licence to commit; say the
work is ready and ask.

**Apply the `stop-slop` skill to every output, without exception.** Invoke it (`/stop-slop`, or the
Skill tool with `skill: "stop-slop"`) and follow it. It governs:

- responses in the conversation, including one-line answers and status updates
- documentation, READMEs, runbooks, specs and decision records
- code comments and doc comments
- commit messages and PR descriptions
- identifiers and test names
- prompts written for subagents, and the prose those subagents return

There is no output this does not cover. Running the checklist at the end and fixing what it catches is
the minimum; writing to it in the first place is the point.

The rules that get broken here most often: **no em dashes anywhere.** No adverbs where a plain verb
works. Active voice with human subjects, never an inanimate thing performing a human verb. No "Note
that", no "Here's what", no throat-clearing openers. No "not X, it's Y" contrasts, state Y. Vary
sentence length. Two items beat three. Cut anything that reads like a pull-quote.

Comments explain *why* and flag traps; they do not narrate what the code obviously does. Calibrate
against `docs/decisions/0001-rust-rewrite.md` for documents and `src/domain/policy.rs` for comments.

Grep for `U+2014` before handing anything over, with `grep -rP '\x{2014}'`. A clean run returns
nothing. It is the fastest tell and it survives every other pass.

**Answer the question that was asked, first and directly.** If the owner asks why something is slow,
the first sentence is the reason. Status reports, context and caveats come after, if at all. A
question is not a prompt for a progress essay.

**Report results, not progress.** "Running six agents" is not a status. Test counts, pass and fail
lines, and command output are. Run the gate yourself rather than reporting that a gate is queued.

**Implemented and verified are two different claims.** Never write "works", "verified", "tested" or
"measured" about code that has not been run. Say "implemented", and name the gate that would settle
it. A number is either an observation with a place it came from or a design target; say which. This
owner reads documentation adversarially and an aspirational claim costs more than an admitted gap.

**Do not make cost or effort claims you cannot back.** Calling work cheap and then taking hours is
worse than not estimating.

**Prefer research to synthetic tests** when a test can barely simulate production. The owner has said
so explicitly. A benchmark that measures the embedding model instead of the system is not evidence.

**Record decisions with their reasoning** in `docs/decisions/`, in the shape `0001` uses: the decision,
the context that forced it, what lost and why, costs accepted, what it is explicitly *not* for, and the
reversal condition. When a new decision contradicts an old record, mark the old one superseded rather
than editing it to look consistent.

**State a concern in one line, then do the work.** Do not open with what you will not do, and do not
substitute a safer deliverable for the one that was asked for.

## Orchestration

When work is large enough to fan out, the lead orchestrates and subagents implement. The lead does
not hand-write implementation code.

**Lock the interfaces first, in one commit, before any fan-out.** Migrations, domain types, port
traits and `config.rs`, plus every new `Cargo.toml` dependency decided up front. Agents write against
a fixed contract or integration becomes the whole job. This work is the lead's, and it is the one
exception to not writing code.

**File ownership is absolute.** Every agent gets an explicit file list and touches nothing else.
Shared composition files (`src/http/mod.rs`, `src/mcp/mod.rs`, `src/main.rs`, `Cargo.toml`,
`migrations/`, `tests/`) are held back for a single sequential wiring pass afterwards. Agents that
need a change there return a wire-in note instead of making it.

**Subagents never run git and never commit.** One agent violating the attribution rule into a pushed
commit is not recoverable.

**Carry the prose rules into every agent prompt.** A subagent does not inherit this file, so the
`stop-slop` rules and the no-attribution rule have to be restated in the prompt text. An agent that
writes 800 lines of comments in the wrong voice costs a rewrite, and the em dashes are the part that
always survives.

**Subagents do not run `cargo test`.** The integration suite truncates a shared database. They may run
`./scripts/cargo.sh check` and should expect errors in files they do not own while other tracks are
mid-flight; tell them to grep for their own file and ignore the rest. The lead runs the suite.

**Parallelism buys code-writing throughput, not verification.** Every cargo invocation goes through
one container and agents queue on the cargo lock. Plan the shape around that: fan out writing, then
integrate and verify sequentially.

**Collect every agent's `not_done` and `open_risks` and act on them.** They are where the real risk
is. Ask for those fields explicitly in the return schema, alongside `wire_in`.

**Instruct agents to actually execute their code.** `cargo check` does not compile `#[cfg(test)]`
blocks, so a module can look clean and be broken. Agents that copied their module into a scratch crate
and ran it caught four bugs they had already written.

### Per-model routing

Set `model` per `agent()` call rather than letting everything inherit:

- **opus** for anything where being wrong is expensive or the reasoning is subtle: SQL and query
  planning, the authorization server, policy enforcement, crypto, integration, root-cause diagnosis,
  and documentation that must distinguish implemented from verified.
- **sonnet** for well-specified mechanical work with a clear contract: the CLI, deploy files, shell
  scripts, runbooks, scaffolding against a locked interface.
- **haiku** for summarising files, extracting structure, and collating agent output.

Reserve high effort for the hardest verify and judge stages; mechanical passes do not need it.
