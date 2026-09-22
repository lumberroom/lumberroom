# Managing a running lumberroom

Everything you do to a live store after it boots: approving a client, changing what one may reach,
deciding what ingestion proposed, correcting a fact, revoking a credential.

Most of it happens in the console at `/console`. The console exists only when `AUTH_MODE=oauth`,
because it checks the owner password against `OWNER_PASSWORD_HASH` and a console that cannot check a
password has no door. In token mode every console route answers with a page saying so, and the work
below happens through `lumberroom` and psql instead. [DEPLOY.md](../DEPLOY.md) covers
switching modes.

## Signing in

Open `/console`. It sends you to `/console/login`, which asks for the owner password, the same one
the OAuth consent screen asks for. The cookie it sets is scoped to `Path=/console`.

A failed password costs 750ms before the response is written, and `login_attempts_per_minute` caps
the rate per address. Both apply to the consent screen too.

## The screens

| Screen | What you do here |
|---|---|
| Reading | The arrivals list, newest first, with the namespace rail beside it. Click a fact to open it. |
| Write | Type a fact by hand, through the same write path a tool call takes. |
| Registry | Read the exact keyed facts. Writing one is a command line act. |
| Aliases | Record that two names mean one subject, or forget one. |
| Queue | Approve or reject what ingestion proposed. |
| Cleanup | Apply or reject what the cleanup passes proposed. |
| Clients | See what every client may reach, change it, approve one, revoke one. |

Every screen carries the same health line: whether the key-encryption key matches this store, which
embedder is loaded, and when the store was last written to.

## Clients

`/console/clients` is the answer to "what can this thing see". Each client is a card:

```
claude-desktop            registered itself · last used 24 Aug · x5siGax2qFPTjwsQ8lQlsjEgNpmz4lLr
READS  *@sealed    WRITES  *@open    mayIngest
[ Change access ]                                                              [ Revoke ]
```

Reads and writes are namespace globs, each carrying a sensitivity ceiling after the `@`. The
capabilities that follow are the flags beyond namespace reach: `registryWrite`, `sealedCapable`,
`mayIngest`, `mayReadHistory`, `mayDelete`. [permissions.md](permissions.md) covers every field and
which tools each capability opens.

Three states appear on a card. A consented client shows no badge and carries its controls. One that
registered itself and has not been approved says **awaiting consent** and holds nothing until you
approve it. A revoked one says so, keeps its row as a record, and loses its controls.

### Changing what a client may reach

Open **Change access** on the card. The form asks three questions, and only the first is required.

**The shape.** Four named grants: Read only, Read and write, Ingest bot, Full. The chosen one
explains itself in place. A shape decides the capabilities and the ceiling on each side.

**The scope.** Everywhere, or only the namespaces you pick. Picking `project:sivella` rewrites the
shape's `*` into that namespace at the ceiling the shape already chose, so Read and write scoped to
one project reads it at `sealed` and writes it at `open`. The list offers the namespaces the store
holds plus any this client already reaches; the box beside it takes a namespace with nothing in it
yet, or a glob such as `project:*`.

**The grant written out.** Behind "Write the grant by hand" sit the raw globs and the five capability
checkboxes. Tick its own box and the capability checkboxes decide, the scope picker is ignored, and
each of Reads and Writes decides only when you put something in it. An empty field falls back to the
shape's grant for that side.

Save, and the new grant decides that client's next call. The grant lives on the client row and
`OpaqueTokenAuthenticator` reads it on every request, so nothing reconnects and no token is
reissued. Implemented, and covered by the `changing a client's access` tests in `tests/console.rs`.

Two limits worth knowing before you reach for this. It edits OAuth clients only: a static bearer
client's grant lives in `AUTH_TOKENS`, which is read once at boot, so changing one means editing
`.env` and restarting. And a grant change leaves no audit row, so the console shows what a client
holds now and never what it used to hold.

### Approving a client that registered itself

A client that runs dynamic registration exists and holds nothing. Its card shows **awaiting
consent** with the editor already open and an **Approve** button. Choose a shape, narrow the scope,
approve. That writes `consented_at` alongside the grant, which is the same thing the consent screen
does when the client walks the browser flow itself.

Approving from here is the owner's own decision, made behind the owner's own password. Approving at
the consent screen is the same decision made while the client waits on a redirect.

### Issuing a client by hand

Folded at the bottom of the screen, because most clients register themselves. Use it for a surface
that cannot register, or one you want waiting before it first calls. It writes to `oauth_client`
beside the registered ones and touches `AUTH_TOKENS` not at all.

Creating a client consents to it. Tick "Issue a client secret" only for something running on a
server that can keep one; the page shows the secret once and the store keeps only a hash of it, so
losing it means issuing another client. A client running in a browser cannot keep a secret and is
bound by PKCE instead.

### Revoking

One click, no confirmation. The cost of a mistake is a surface that stops working and says so; the
cost of hesitating is a live credential while you look for the confirm button.

Revoking kills the client and every access and refresh token it holds, and every request checks the
client row, so it takes effect on the next call rather than at token expiry. A revoked client cannot
be given a grant again. Issue a new one.

To confirm a revoke took, call `/admin/whoami` with that client's access token. It answers what a
credential resolves to from the code path that enforces it, which settles most arguments about a
grant faster than reading the row.

## The ingest queue

`/console/queue` holds what transcript ingestion proposed. Ingestion writes nothing on its own
([decision 0011](decisions/0011-cleanup-proposes.md) covers the same rule for cleanup).

Each row separates what the posting client claimed about itself from what the server knows. The
speaker is the client's word. The auto badge is the server's, and it means the poster could have
written that row itself.

**Approve** sends the proposal through the write path, with the same classification, duplicate check
and refusals a tool call gets. A refused row stays in the queue with the rule that stopped it
printed on it.

**Reject** blocks that content for good and asks first, since the click has no undo. Rejecting is
reversible from the Rejected list, where **Return to queue** puts a row back.

The command line clears a queue in bulk: `lumberroom ingest approve --run <id>`, run from
`crates/lumberroom`, the Rust client that ingests transcripts, not the Node CLI this page uses
everywhere else. `docs/ingestion.md` covers it. Two hundred rows is not a queue anybody clears one
button at a time.

## The cleanup queue

`/console/cleanup` holds what the cleanup passes found: duplicates, paraphrases, contradictions and
stale rows. Nothing is retired until you say so.

**Apply** supersedes, and the retired row stays readable with the date it stopped holding. **Apply,
deleting** removes rows outright, so it sits behind a confirmation step and names what it will
delete. A contradiction offers **Resolve** instead, because a contradiction is a question about
which of two facts holds rather than a duplicate to collapse.

A finding you reject stays rejected, and **Return to queue** undoes that. [cleanup-schedule.md](cleanup-schedule.md)
covers the two cadences behind the queue and how to install them.

## Aliases

`/console/aliases` records that two names mean one subject, so a question naming either reaches the
facts written under both. Renames are the case it exists for.

Forgetting an alias asks first, on its own page, naming the alias and the canonical it points at.
The facts themselves stay exactly where they are; only the expansion goes.

## Writing and correcting a fact

`/console/write` takes one fact through `services::write::run`, the same function a tool call
reaches. The namespace field offers the namespaces the store already holds and still accepts one it
has never seen.

"Became true on" is the field the console exists for. A person typing a date knows when a fact
started holding; a model reads one out of context and invents it. A date inside the near-now fence,
one day by default (`WRITE_MIN_OCCURRED_AGE_SECS`), is refused, because the store already stamps the
moment it learned a thing and today's date would write that clock twice.

To correct a fact, open it from the arrivals list and use **Replace this fact** underneath it. That
retires the old row, links the two, and keeps the old wording readable with the date it stopped
holding. Correcting from the fact you are reading is the moment supersession has any chance of
firing; sending someone to a blank page to retype it is how a store fills with rows that contradict
each other.

## What has no console path

Some management still happens on the command line, and one thing needs psql.

```bash
lumberroom clients                    # every OAuth client, how it registered, consent state
lumberroom registry set|get|alias     # the registry writes; the console reads them
lumberroom review [--source conflict,stale,proposal] [--limit N] [--days N] [--min-similarity X] [--json]
lumberroom stats [--hours 168] [--by-client]
lumberroom forget <id> [--dry-run]    # needs mayDelete on the credential
lumberroom seal <key> --namespace credentials:aws
lumberroom review --dates             # undated facts whose own text names a day
lumberroom review --registry          # registry entries due for review, non-canonical keys
lumberroom review --dismissed         # lists the dismissed-pair ledger
lumberroom review --undismiss <a>,<b> # removes one pair from the ledger
lumberroom fill-date <id> <date>      # fill that day in; it never moves a date already there
lumberroom currency [--fixture f]     # does the store report the fact that held
lumberroom arity preview|declare|forget|run|list
lumberroom graph walk "<question>" | lumberroom graph rebuild
```

`review` opens one queue over conflicts, stale rows and, on a server that fills them, proposals.
`--stale` and `--conflicts` still work, as aliases for `--source stale` and `--source conflict`.
Without `--json` it walks the queue one item at a time: full row text, then a key line drawn from
what that item allows (`s` supersede keep newer, `o` keep older, `m` merge, `k` keep both, `d`
delete, `c` confirm, `a` apply, `x` dismiss, `n` skip, `q` quit). `--json` prints the page as JSON
and exits instead of walking it. The conflict source answers `namespace_too_large` and sits out of
the page when a readable namespace holds more live rows than `CONFLICT_SCAN_MAX` (default 2000):
the self-join that finds a namespace's conflicting pairs runs per namespace, so a namespace past
that ceiling would cost tens of seconds rather than answer a page. The default came from a
dev-container probe; recalibrate it against a timed run on the deployment's own hardware.

`arity` and `graph` are the two worth reading about before use, because both decide what the store
hides. Declaring a tag `single` lets the cleanup pass propose that one dated fact ended another, so
`arity preview <tag>` shows what a declaration would end before anything is written. Approving those
proposals oldest first matters: applying a later pair retires a row an earlier pair still names, and
the earlier one can then never be applied.

`graph walk` asks the router whether a walk is worth its cost and says why either way. It answers
questions that join two things the store knows separately, which ordinary search cannot reach.
`--force` walks anyway, which is how a refused question gets compared against what a walk would have
found.

Changing a static bearer client's grant means editing `AUTH_TOKENS` and restarting. Editing the
seeded namespace defaults in `sensitivity_default` is psql, and a twice-a-year job at most.

Three subcommands live in the Rust binary, because argon2 and CSPRNG bytes are not things a shell
script should improvise:

```bash
docker compose run --rm -T server lumberroom-server hash-password
docker compose run --rm -T server lumberroom-server generate-kek
docker compose exec -T server lumberroom-server verify-kek
```

## Related

- [permissions.md](permissions.md): every grant field, both axes, and which tools each capability
  opens.
- [../deploy/oauth.md](../deploy/oauth.md): the OAuth production path, per surface, with the consent
  screen.
- [../DEPLOY.md](../DEPLOY.md): the runbook. Both deploy paths, the key-encryption key, backups,
  troubleshooting.
- [faq.md](faq.md): the short answers, with links to the long ones.
