# MCP names the app that wrote a memory

Written 23 September 2026. Scope for an engine change, branch `feat/mcp-source-label`. Nothing below
is built.

## The problem

Every memory records `source_client`, the principal's `client` at write time
(`src/services/write.rs:349`, `src/services/registry.rs:349`). What that string holds depends on
how the writer signed in:

| Auth path | `Principal.client` | Readable |
|---|---|---|
| Static token | the label in `AUTH_TOKENS` (`src/adapters/auth/token.rs:62`) | yes |
| External OIDC | the configured grant's client (`src/adapters/auth/oidc.rs:234`) | yes |
| Browser session | `"browser"` (`src/adapters/auth/mod.rs:134`) | yes |
| Built-in OAuth | the random `client_id` (`src/adapters/auth/opaque.rs:134`) | no |

The MCP tools hand that string to the agent unchanged. A memory Codex wrote over OAuth reaches the
next agent as `via l9Bo4qkodrWSqZQs3ZredgblRz22g9tw`. The agent cannot tell which app that is, and
nothing it can call answers the question. On a store running `AUTH_MODE=oauth`, Claude.ai, ChatGPT
and Codex all connect through the built-in server, and on the hosted fork every OAuth client does.
On the owner's own store, five of the seven most active sources in the last 30 days were raw client
ids (lumberroom.cloud Overview, 23 September 2026).

A `list_clients` tool would answer it, but reading who holds access is owner administration. It
does not belong in the toolset an agent uses to recall and write facts.

## The change

MCP responses name the source instead of identifying it. Where a response carries a source today,
it carries `source`: the OAuth client's `client_name` when `source_client` is a `client_id` the
store knows, live or revoked, and the stored `source_client` unchanged otherwise. MCP output drops
the raw id.

Storage does not change. `source_client` keeps the stable id, because a client can be renamed and
an export, an archive or an audit needs the value that was true at write time. The archive crate
(`crates/archive/src/record.rs:73`) keeps writing `source_client` as it is.

### Where names get resolved

A new service, `services::sources`, owns one function:

```rust
/// Readable name for each stored `source_client`. A value no OAuth client matches maps to itself.
pub async fn labels(ctx: &Ctx, stored: &[String]) -> HashMap<String, String>
```

It calls `OauthStore::list_clients(include_revoked = true)` (`src/ports/oauth.rs:150`) once per
call and matches exactly. Services cannot reach that store today: `Repos` (`src/services/mod.rs:66`)
has no OAuth handle. `Repos` gains `oauth: Option<Arc<dyn OauthStore>>`, filled by the composition
root when `AUTH_MODE=oauth` and `None` otherwise. With `None`, every value maps to itself and the
port is never touched. Services already depend on ports, so the dependency
direction in `docs/architecture.md` holds.

Two approved clients with the same name after normalisation (NFKC, trimmed, case-folded, zero-width
and bidi controls removed) each get the date they were added (`created_at`) appended:
`Codex (added 1 Sep)`, with the time as well when two were added the same day:
`Codex (added 1 Sep, 13:02)`. The date is `created_at` rather than `consented_at` because
`set_client_grant` rewrites `consented_at` on every grant edit, so a label keyed on it would move
each time the owner edited that client's access. `consented_at` decides only whether a client is
approved: an unapproved client adds no suffix to an approved one, and prints `(not approved)` beside
a same-named approved client. Matching by exact id means a token label that happens to look like a
client id stays as it is. A failed `list_clients` call logs a warning and every value prints as
stored; `labels` never fails a read.

### What changes in each tool

The implementer confirms this list with `grep -rn source_client src/services src/mcp` before
starting:

- **`context_bootstrap`.** The digest trailer reads `via Codex` instead of `via <client_id>`
  (`src/services/bootstrap.rs:348-357`). `Fact` gains the label.
- **`memory_search`.** `SearchHit` (`src/services/search.rs:60`) carries `source`.
- **`memory_history`.** The timeline serialises domain `Memory` directly
  (`src/mcp/extra_tools.rs:167`). It gets an MCP view struct with `source` in place of
  `source_client`, rather than a `Serialize` change on the domain type the console and export also
  use.
- **`registry_get`, `registry_history`.** `Provenance` (`src/domain/types.rs:179`) reaches MCP
  through the registry service; the MCP output carries `source` beside it.
- **`review_queue`.** Any item that shows who wrote a row carries `source`.
- **Tool descriptions.** Each description that mentions where a fact came from says `source` is
  the name of the app that wrote it.

The field is named `source`, not `source_client`, so an agent never reads a name where it once read
an id under the same key. No client in this repository parses `source_client` out of MCP output.
The Open WebUI filter and the hook scripts under `client/` were checked.

## The trade this makes

`bootstrap.rs:347` says: "`source_client` is the one field a writer cannot set, so a trailer forged
inside the body contradicts the real one on the same line." The server still sets `source`, and a
forged trailer in the body still sits beside the real one. What changes is who picks the string. An
OAuth client names itself at dynamic registration (`src/authserver/routes.rs:267`), so a client can
register as "Claude Code" and its memories will read `via Claude Code`.

What limits that:

- The owner approved every OAuth client on the consent page, which shows the name it registered
  with.
- A second client using a name already taken gets the date it was added, so two "Claude Code"
  sources read differently.
- The id offered an agent nothing to check against, so replacing it removes a protection the agent
  could not use.

What it gives up: a trailer's `via` value used to be impossible for a client to choose. It is now
chosen by the client and approved by the owner. The decision record says so.

Reversal condition: if an impersonation through a consented client turns up, the fix is an
owner-set display name at consent, which then overrides `client_name` in `labels`.

## The fork

lumberroom-cloud merges this down. Two things it has to handle, recorded here so the merge is not a
surprise:

- The fork's `OauthStore::list_clients` takes a tenant (`src/ports/oauth.rs:226` in the fork). The
  fork's copy of `services::sources::labels` passes the principal's tenant. That is a fork-divergence
  row.
- The fork's dashboard spec (lumberroom-web `docs/2026-09-23-human-dashboard.md`) adds
  `source_label` to its REST API with the same rule. Once this lands and merges down, the fork's API
  calls `services::sources::labels` instead of carrying its own copy, so MCP and the dashboard never
  disagree about a name.

## Not in scope

- The engine console shows raw `source_client` too (15 references under `src/console/`). It can use
  `labels` in a follow-up. This change covers MCP.
- A `list_clients` MCP tool.
- Owner-set display names.
- Any change to what is stored, exported or archived.

## Decision record

`docs/decisions/0020-mcp-names-the-source.md`, in the shape `0001` uses. It records the trade above:
the forgery comment's promise narrows from "a writer cannot set it" to "the owner approved it", and
the reversal condition.

## Done means

- An integration test writes through an OAuth client named "Codex" and reads the memory back
  through `memory_search`, `memory_history` and `context_bootstrap`. Each shows `Codex` and none
  contains the `client_id`.
- The same test with a static-token writer shows the token's label unchanged.
- A revoked client's memories still show its name.
- Two clients named "Codex" produce two different labels.
- With `AUTH_MODE=token`, `labels` never calls the OAuth store.
- `./scripts/cargo.sh check --all-targets --features test-support`, `fmt --check` and
  `test -j 1 --features test-support` pass, with the test count reported.
