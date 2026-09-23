# 20. MCP names the app that wrote a memory

**Date:** 23 September 2026 · **Status:** accepted, implemented · **Decided by:** the owner

What was run, and where: on 23 September 2026, `./scripts/cargo.sh check --all-targets` and
`./scripts/cargo.sh fmt --all -- --check` ran clean on this branch. The test suite, including
`tests/mcp_source.rs` and the unit tests in `src/services/sources.rs` and `src/mcp/views.rs`, has
not run yet. Until it does, every behaviour below is implemented and unverified.

## Decision

Every MCP answer that says who wrote a row carries `source`, the name of the app, where it used to
carry `source_client`. For a built-in OAuth client the name is the `client_name` it registered
with, matched by exact `client_id`, revoked clients included. Any other stored value prints as it
is: a static token's label, an OIDC client, `browser`. `services::sources::labels` holds the rule
and every tool calls it.

Two approved clients whose names compare equal carry the date each was added, `created_at`:
`Codex (added 1 Sep)`, and the time as well when they share a day, `Codex (added 1 Sep, 13:02)`.
Names compare after NFKC, lowercasing, trimming, collapsing whitespace and removing zero-width and
bidi control characters. `consented_at` decides one thing, whether a client is approved: a client
nobody approved counts toward no duplicate, and when it shares an approved client's name it prints
`Codex (not approved)`.

`labels` never fails a read. When the client table cannot be read it logs a warning and every value
prints as stored. The owner would rather see an id than lose a search or a digest.

The tools that changed are `context_bootstrap` (the digest trailer reads `via Codex`, and each fact
carries `source`), `memory_search`, `memory_history`, `registry_get`, `registry_history` and
`memory_forget`. `review_queue` shows no writer and did not change.

Storage does not change. `source_client` keeps the id in the database, the export, the archive crate,
the admin routes and the console. A client can be renamed, and an export or an audit needs the value
that held at write time.

## Context

`source_client` stores `Principal.client`. For a built-in OAuth client that is the random
32-character `client_id` (`src/adapters/auth/opaque.rs`), and the MCP tools passed it through. A
memory Codex wrote over OAuth reached the next agent as `via l9Bo4qkodrWSqZQs3ZredgblRz22g9tw`, and
no tool an agent may call maps that back to an app. On the owner's own store, five of the seven most
active sources over the thirty days to 23 September 2026 were raw client ids.

## What lost, and why

A `Serialize` change on `Memory` and `Provenance` lost. The console, the admin routes, the export
and the CLI read the same types, and a rename there changes every surface at once. The four tools
that return those types map them into views in `src/mcp/views.rs`. Search and the digest carry
`source` in their service structs, because an MCP tool is the only thing that serialises either; the
console reads the search hit's `source_client` in Rust, and serde skips that field.

Keeping the key `source_client` and putting the name in it lost. An agent that learned the key
holds an id would read a name under it and never know.

A `list_clients` tool lost. Reading who holds access is owner administration, and it does not belong
in the toolset an agent uses to recall and write facts.

A join in the Postgres adapter lost. Every read path would need it, and the duplicate rule is policy
rather than storage. `labels` makes one `list_clients(include_revoked = true)` call per invocation
and matches in Rust.

Counting unapproved clients toward a duplicate lost. Anyone who can reach `/oauth/register` can
register a name, and counting those rows would let a stranger push a date onto the owner's own
"Codex".

The approval date as the disambiguator lost to `created_at`. `set_client_grant` writes
`consented_at = now()` on every grant edit, the console's access form included, so a label keyed on
it would change whenever the owner edited that client's access, and the rows it wrote would read
under a new name. `created_at` is set once at registration and never moves.

Failing the tool call on a store error lost. A label is a convenience on top of the answer, and a
search or a digest the owner never sees costs more than one that prints an id.

Reading the client table in token and OIDC modes lost. No OAuth client can write there, so the
composition root leaves `Repos.oauth` empty and `labels` never touches the store.

## Costs accepted

**The forgery promise narrows.** `bootstrap.rs` used to say `source_client` is the one field a
writer cannot set, so a trailer forged inside a body contradicts the real one on the same line. The
server still writes the trailer, and a forged one still sits beside it. What changed is who picks
the string: an OAuth client names itself at dynamic registration (`src/authserver/routes.rs`), so a
client can register as "Cursor" and its memories read `via Cursor`. The owner approved that name on
the consent screen, which shows it. A second client with a name already taken carries the date it was
added. The id offered an agent nothing it could check, so replacing it removes a
protection nobody could use. `opaque.rs` already records that at least one surface reports itself
under another product's name; that surface now prints under the name it chose.

**Normalisation has gaps.** `to_lowercase` is simple case mapping, so "STRASSE" and "straße" stay two
names. Letters from another script that look Latin, a Cyrillic "С" for a Latin "C", pass NFKC
untouched. A name registered to equal another client's dated label, `Codex (added 1 Sep)`, prints
the same as that label. The duplicate rule covers OAuth clients only, so a client named after a
static token's label prints the same as that token. The owner sets token labels, so this stays
without a suffix.

**Mode changes change output.** A store that moves from `AUTH_MODE=oauth` to `token` prints its
OAuth-written rows by id again.

**A failed lookup prints ids.** A store error drops the labels for that call rather than the call,
so an outage of the client table reads as ids until it recovers.

**One more query per call.** Bootstrap, search, history, the registry reads and forget each make one
`list_clients` call in oauth mode. The latency cost is not measured. The digest cache holds a label
for up to `BOOTSTRAP_CACHE_MS` after a rename.

**A name keeps its markdown.** `labels` removes control characters and collapses whitespace, so a
name cannot open a section of the digest. An underscore or a parenthesis inside a name prints as is.

## Not for

It is not an identity or an authorization claim. Grants, tokens and the tool-call log key on
`client_id`, and nothing here changes that.

It is not for the console, the admin routes, the export or the archive. The console can call
`labels` in a follow-up. Owner-set display names and a `list_clients` tool are out of scope.

## Reversal condition

If an impersonation through a consented client turns up, the owner sets a display name at consent
and `labels` prefers it over `client_name`. If the trade itself proves wrong, `labels` maps every
value to itself again and MCP prints the id.
