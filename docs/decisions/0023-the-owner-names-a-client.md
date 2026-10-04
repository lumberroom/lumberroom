# 23. The owner names a client, beside the name it registered

> **Draft.** Design only. Nothing is built yet, and this document changes as the build proceeds.

**Date:** 3 October 2026 · **Status:** draft, accepted by the owner, nothing built · **Decided by:** the owner, 3 October
2026, on [`docs/specs/client-owner-label.md`](../specs/client-owner-label.md)

## Decision

**An OAuth client carries `owner_label`, a name the owner chose, beside `client_name`.** Null until
the owner names it. A CHECK refuses a label on a client nobody approved, and registration never
writes the column.

**The owner names a client in three places.** The consent page offers "Name this connection",
prefilled with the cleaned registered name; leaving it unchanged stores nothing. The console card
and `lumberroom clients rename` change it later, the CLI through `POST
/oauth/clients/{client_id}/label`. Clearing it falls back to the registered name.

**`services::sources` prints the label first.** Every MCP `source`, the digest, the console and the
listing follow a rename on their next read; each path that writes a label clears the digest cache,
so `context_bootstrap` follows too. In a group of approved clients whose printed names collide, a
single named client prints bare and the others carry their `(added ...)` stamp. Two named clients
that collide both carry stamps.

**No name may end in a stamp.** A label or a registered `client_name` ending in `(added ...)` or
`(not approved)` is refused, so nobody can pass one client off as another's disambiguated name.

**MCP prints the label alone.** `source` stays one string, the name the owner reads.

**A rename writes no audit row on self-hosted.** It logs one info line with the client id and
whether the name was cleared.

**The consent page judges a client on what it registered.** The headline, the client-name row, the
destination and every warning keep reading `client_name` and the redirect URI. The label shows only
as the input's value. The count of other approved clients registered under the same name sits in
the naming hint and disappears when the page raises the brand alarm.

## The context that forced it

Decision 0020 made MCP print a client's registered name, with `(added 1 Sep)` when two approved
clients share it, and named an owner-set display name at consent as its reversal. On the hosted
Overview on 3 October 2026 the owner read `Claude Code (plugin:lumberroom-memory:lumberroom) (added
2 Oct)`, `Hermes Agent (lumberroom) (added 26 Sep 2026, 11:23:08)` four times, and `Codex (added 23
Sep)`. The stamps are computed at read time, so no stored value holds them and no migration can
tidy them. Only a name the owner chooses can.

## What lost, and why

**Overwriting `client_name`.** The consent page's warnings judge the name a registrant chose. Once
the owner's text replaces it, a self-registered client reads as one the owner vouched for, and the
alarm for "Claude sending codes to chatgpt.com" would key on the owner's words.

**Printing both names in MCP, `Claude laptop (Claude Code)`.** It doubles a registrant-chosen
string in every agent's context and gives one writer two names to match on. The registered name
stays on the console, the listing and the dashboard, where the owner checks identity.

**Refusing a label that equals another client's label.** The refusal needs a lock or a race
window, and in the hosted fork it would tell a member that a colleague already uses a name. The
stamp keeps two equal labels apart, and the route answers with the resolved label so the owner sees
the date straight away.

**A migration giving existing clients a cleaned default.** The `(added ...)` suffix is not stored,
so there is nothing to strip. Writing a default label would mark every client as named by the owner,
and then every duplicate group would hold two or more named clients and keep its stamps.

**An audit row per rename.** The engine has no audit table, and a grant change, which moves what a
client reaches, writes no row either. A rename moves nothing a client can do.

**Storing a label when the consent field comes back unchanged.** Every consent would then count as
a choice, and the duplicate rule would stamp every collision as it does today.

**Letting the digest cache expire on its own.** For up to `BOOTSTRAP_CACHE_MS`, 30 seconds by
default, a fresh session would read the old name right after the owner saved a new one. The review
paths already clear the cache after a write; a rename does the same.

## Costs accepted

**One more client read on the consent page.** The same-name count needs `list_clients(true)`. A
failed read prints no count and the consent goes ahead.

**A label write can fail after the grant lands.** The consent writes the grant, then the label, in
two statements. A failure between them logs at warn, and the owner renames the client afterwards.

**A full-profile OAuth client can rename clients through the bearer route.** `registry_write` marks
the owner's own credential, which already lists clients and rewrites the registry.

**Equality is exact on cleaned text.** An owner who types `codex` for a client registered as
`Codex` stores a label that differs only in case. The duplicate rule still groups the two forms.

**A client that registers a stamp-shaped name is refused.** No MCP client this server knows of
registers one, and the refusal names the reason.

## What this is not for

**Not an identity check.** A label says what the owner calls a client. It vouches for nothing the
client claims, and the consent page never treats it as a claim.

**Not a fix for re-registration.** Harnesses that register a new client on every connect produced
the four Hermes rows. Naming tells them apart; reusing a registration is a separate change.

**Not a rename of the writer on stored rows.** `source_client` keeps the id. The label applies at
read time, so a rename changes what every past row reads as.

## Reversal condition

Revisit the MCP choice if an agent needs to tell an owner-named client from one that only
registered a name, for example to weigh a source's trust; `source_named_by_owner` beside `source`
would be the additive shape. Revisit the audit choice when grant changes gain an audit row: renames
join it in that change.
