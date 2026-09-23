# Permissions: writing an AUTH_TOKENS grant

## The asymmetry that bites

An absent `read` list means unrestricted, and unrestricted implies `sealedCapable`: the owner's
own client, holding nothing back. It never implies `mayDelete`, `mayIngest`, or `mayReadHistory`.
Those three read the grant field alone, with no unrestricted-read shortcut.

That gap produced a real 403. On 21 August 2026 the owner's credential carried
`"read":[{"namespace":"*","max":"sealed"}]` and no `mayDelete`, and `lumberroom forget` refused it. The
grant looked total because the read ceiling was total. `effective_may_delete` does not consult
`read` at all:

```rust
pub fn effective_may_delete(&self) -> bool {
    self.may_delete
}
```

Compare `effective_sealed_capable`, which does:

```rust
pub fn effective_sealed_capable(&self) -> bool {
    self.sealed_capable || self.read.is_none()
}
```

The reasoning in `ClientGrant`'s own comments: an unrestricted grant is the operator's own client
by definition, so it gets sealed capability for free. Delete, ingest, and history are each an
action the owner grants by name, on his own client too, because a client that can silently act
(delete a memory, fill an approval queue, read a retired fact that was corrected for a reason) is
a worse failure than one that only reads too much. Fix a `lumberroom forget` 403 on your own credential
by adding `"mayDelete":true`, not by widening `read`; widening `read` does not touch this flag at
all.

## The JSON shape

`AUTH_TOKENS` is a JSON array of these objects, one per static bearer client.

```json
[
  {
    "client": "claude-code-mac",
    "token": "CHANGE_ME_openssl_rand_hex_32",
    "read": [{"namespace": "*", "max": "sealed"}],
    "write": [{"namespace": "*", "max": "sealed"}],
    "sealedCapable": true,
    "mayDelete": true,
    "mayIngest": false,
    "mayReadHistory": true,
    "registryWrite": true
  },
  {
    "client": "browser-widget",
    "token": "CHANGE_ME_second_token",
    "read": ["user:me", "global"],
    "write": []
  }
]
```

The first is a full owner credential: sealed everywhere, every capability except ingest. The
second is narrow: open-ceiling read on two namespaces (a bare string means `max: "open"`), no
write, and every capability flag left at its default of `false`.

`AUTH_TOKENS` also accepts a compact form, `client:token,client:token`, for a credential you would
have written unrestricted read and write for anyway: `registryWrite`, `sealedCapable` and every
other capability stay at their default of `false`. `.env.example` carries the rule that
distinguishes the two forms: the string starts with `[` or `{` for JSON, anything else parses as
compact pairs.

## Every field

- **`client`** (required, string). The stable identifier recorded as `source_client` on every row
  this credential touches and on every tool call. Wrong value: memories written by this client
  attribute to the wrong name, and nothing else breaks.

- **`token`** (optional, string). The bearer value presented in the `Authorization` header. Absent
  or blank makes the entry inert; it matches no request. There is no default that makes a token
  work without one.

- **`read`** (optional, array of namespace grants). Absent means unrestricted at every level: this
  is the shape reserved for the owner's own client. An explicitly empty array, `[]`, means exactly
  what it says, no readable namespace, and must never be treated as equivalent to absent. See "The
  two axes" below for what an entry in the array looks like.

- **`write`** (optional, array of namespace grants). Same shape and same absent/empty distinction
  as `read`, evaluated independently. A client can read broadly and write narrowly, or the
  reverse.

- **`registryWrite`** (default `false`). The registry holds credential locations (see
  `docs/decisions/`), so writing to it is an operator action, not a default a namespace grant
  implies. Opens `registry_set` and `alias_set`. Wrong (left `false` for a client that needs it):
  every `registry_set` and `alias_set` call 403s regardless of the namespace grants.

- **`sealedCapable`** (default `false`, but see the asymmetry above). Asserts the client can
  decrypt sealed content locally. A client holding a sealed ceiling without this flag still only
  ever receives ciphertext, never plaintext, so the ceiling alone does not leak sealed rows to a
  client that cannot open them.

- **`mayDelete`** (default `false`). A client that can silently delete memories is a worse failure
  than one that hoards them, which is why this reads only the explicit flag. Opens
  `memory_forget`.

- **`mayIngest`** (default `false`). Opens the ingest routes and the `/admin/cleanup/*` routes:
  `admin_cleanup_run`, `admin_cleanup_list`, `admin_cleanup_post`, `admin_cleanup_show`,
  `admin_cleanup_apply`, `admin_cleanup_reject`, `admin_cleanup_resolve`, `admin_cleanup_unreject`.
  A client that can post proposals can fill the queue the owner has to read, and a queue he stops
  reading is an approval gate in name only, so cleanup rides the same flag as ingestion rather
  than a separate one. No MCP tool sits behind this; the routes are HTTP-only, for `lumberroom
  ingest` and the process the owner runs by hand.

  It opens the routes and widens nothing. Every one of them runs inside the client's `read`
  grant, applied in the query: a cleanup run reads the namespaces the grant admits and refuses a
  `namespace` outside them; the two queues list, show, reject and unreject only proposals whose
  rows the grant admits, and an id outside it answers 404; an ingested fact is accepted only for
  a namespace the client may read at the level that namespace writes at; the emission check
  answers one boolean per probe, only for rows the grant admits, and at most 200 probes per
  call. A cleanup finding posted over HTTP has to name rows the client can read, in the
  namespace it claims, holding the text it says it saw, and a `stale` finding needs `mayDelete`
  as well, because applying one deletes. `mayIngest` with an empty `read` list is a client that
  can open runs and nothing else.

  One disclosure rides the flag and no grant narrows it: `GET /admin/ingest/watermarks` lists the
  path and session id of every transcript file ingestion has walked, and the `POST` moves any
  file's mark forward. Watermarks are per file, not per namespace, and a plan needs every file's
  offset or it re-reads what another client already extracted, so they are tenant-wide by design.
  Give `mayIngest` to a process you run yourself and to nothing that reads a browser tab.

- **`mayReadHistory`** (default `false`). Whether this client may read facts that no longer hold.
  A retired fact can be more revealing than the one that replaced it, so this is off unless the
  grant says otherwise, even for a client with a full read ceiling. Opens `memory_history` and
  `registry_history`.

## The two axes

A namespace grant is not a plain list of strings; each entry is a namespace glob paired with a
sensitivity ceiling. Namespace answers whose facts, sensitivity answers how exposed. The three
levels, low to high: `open < private < sealed`.

Two shapes both parse:

```json
"read": [
  "user:me",
  {"namespace": "personal:finance", "max": "private"},
  {"namespace": "*", "max": "sealed"}
]
```

A bare string, `"user:me"`, means a ceiling of `open` on that namespace. That default exists so a
grant written before the sensitivity axis landed never silently gains access to private or sealed
rows the day the axis ships; every Phase 1 grant kept meaning exactly what it said. An object
without a `max` field means the same thing, `open`, for the same reason.

When more than one entry matches a namespace, the ceiling is the highest of them, not the lowest:
two grants touching the same namespace is the caller having been granted both, and the more
generous one is what was actually granted.

The filter this produces runs inside the query, never as a pass over results after the fact. A row
a client's ceiling does not admit must never enter that client's process, because filtering after
retrieval is a leak waiting on the one code path that forgets to filter.

## Which tools each capability opens

| Tool | Capability required | Grant field |
|---|---|---|
| `context_bootstrap` | Open | none |
| `memory_search` | Open | none |
| `memory_write` | Open | none |
| `registry_get` | Open | none |
| `alias_list` | Open | none (namespace-filtered inside the call) |
| `memory_forget` | MayDelete | `mayDelete` |
| `memory_history` | MayReadHistory | `mayReadHistory` |
| `registry_history` | MayReadHistory | `mayReadHistory` |
| `registry_set` | RegistryWrite | `registryWrite` |
| `alias_set` | RegistryWrite | `registryWrite` |
| `review_queue` | Open | none (verdicts filtered per row inside the call) |
| `review_decide` | Open | none (writable_row and, on `delete`, `mayDelete`) |

Open still means every authenticated client, not every request: the namespace and sensitivity
ceilings apply inside the call regardless of which capability gated the tool's visibility.
`registryWrite` carries `alias_set` along with `registry_set` because an alias is a naming fact of
the same class as a registry key; nobody gains alias-write who did not already hold the
higher-trust flag, and no client gains any capability it was not granted by name. This table is
`TOOL_CAPABILITIES` in `src/mcp/capability.rs`, held against the router's own tool list by a test
there so a tool added without an entry fails the build instead of shipping ungated.

### The two review tools

`review_queue` and `review_decide` sit at Open, same as `memory_search`, and the grant does its
work inside the call rather than at the tool list.

A caller holding read alone, no matching write grant, sees every item its read ceiling admits, but
each item's `verdicts` list comes back empty: the decide path needs read and write on every row in
an item (`writable_row`), so a caller who cannot write a row is never offered a key for it. Calling
`review_decide` on such an item anyway fails the same check `review_queue` used to build the empty
list, and the refusal names the row rather than the tool. A read-plus-write caller gets every
verdict an item lists except `delete`, which needs `mayDelete` on top, the same flag
`memory_forget` reads: deleting a row through the review queue is still deleting a row.

`keep_both` writes the dismissed-pair ledger, which hides the pair from every reader of the
tenant, and it takes read and write on both rows, the same authority a supersede of the pair
takes. The ledger records the client and the token fingerprint, and `dismissed` in the envelope
counts what it hides.

`apply` and `dismiss` belong to whichever `ProposalSource` owns the item's origin, and the source
keeps its own gate; this engine ships the trait and wires no source, so `sources.proposal` answers
`[]`. `source=proposal` on `review_queue` answers `source_not_filled` for every caller regardless of
grant, because the queue read refuses an empty source list outright. `review_decide` on a
`proposal:<origin>:<id>` key never reaches that check: it looks up `origin` in the source list,
finds none, and answers `unknown_origin` instead, again for every caller regardless of grant. A
deployment that wires a source inherits that source's own
permission rules for `apply` and `dismiss`, on top of the read grant `review_queue` already checked
to show the item at all.

A source may take corrected text on `apply`: the item carries `repairable`, and the caller passes
that text as `content` on the decide call. The source checks the text before it writes it and
answers `content_written`. An `apply` without `content` on an item carrying `held_by` goes past
that check instead, and the answer names the check in `overrode`, so nothing is gone past silently.
Over MCP, a proposal decision needs a `reason` and the item's `version`, both required; an HTTP
caller may leave either out. `repairable` shows only on an item this caller could also `apply`
outright, so a grant that hides `apply` hides the repair as well, with no second check to fall
through.

## Setting AUTH_TOKENS without breaking the JSON

`.env` is parsed by Docker Compose itself, so a single-quoted JSON array there is fine:

```
AUTH_TOKENS='[{"client":"claude-code-mac","token":"...", ...}]'
```

Sourcing that same file through `sh` (`source .env`, or a shell script that reads it) strips the
double quotes and hands the process invalid JSON, because `sh` does not know the value is meant to
stay a JSON string. Keep the single quotes; do not `source .env` and expect the token config to
survive it.

## Issuing a client from the console

`/console/clients` creates an OAuth client without touching `.env` and without a restart. It writes
to `oauth_client` beside the clients that register themselves, so nothing here changes how
`AUTH_TOKENS` works: a credential you put in `.env` keeps working exactly as it did.

Pick one of four shapes and the form fills the grant in:

| shape | reads | writes | capabilities |
|---|---|---|---|
| Read only | `*@sealed` | nothing | none |
| Read and write | `*@sealed` | `*@open` | none |
| Ingest bot | nothing | nothing | `mayIngest` |
| Full | `*@sealed` | `*@sealed` | everything except `mayDelete` |

Each shape decides what a client may do and how deep it may see. A scope picker beside it decides
where: ticking a namespace rewrites the shape's `*` into that namespace at the ceiling the shape
already chose. A client that should reach one project and nothing else takes two clicks and no
syntax. The picker lists the namespaces the store currently holds, plus any namespace the
client already reaches, and a text box beside it takes a namespace with nothing in it yet or a
glob such as `project:*`.

The Ingest bot shape as it stands can open runs and post nothing: a fact is accepted only for a
namespace the poster may read, so give it the namespaces it extracts for, at `open`, through the
advanced view. Write is not needed for the queue; it is what turns an `owner_typed` quote into an
auto-approval, and a client without it gets a proposal the owner reads instead of a badge.

## The cleanup daemon's client

`lumberroom cleanup daemon` holds one credential, `LUMBERROOM_CLEANUP_TOKEN`, and it needs exactly
this: `mayIngest`, and `read` over the namespaces it cleans at `open`. `{"namespace":"*","max":"open"}`
is the whole store's open rows. Nothing more.

`open` is enough because everything the daemon reads leaves the machine: the pairs it is handed go to
a provider, and the run withholds any pair with a side above `open` whatever the grant says. A ceiling
of `private` would not show the daemon one more pair and would let the same token read private rows
through every other route. Private duplicates are grouped by the in-server pass, which runs
unrestricted because it sends nothing anywhere; `docs/cleanup-schedule.md` says what switching that
pass off costs. No `write`: the daemon proposes and the owner applies, through the
console's own credential. No `mayDelete`: the model pass produces `paraphrase` and `contradiction`
findings, and the only kind that deletes is `stale`, which the in-server pass writes itself.

**No shape grants `mayDelete`**, and a test holds that. Deletion is reachable only by opening the
advanced view and ticking it, which is the point: a client that can silently remove a memory is a
worse failure than one that hoards them.

The advanced view **replaces** the shape rather than merging with it, and ticking its own box turns
the scope picker off: a namespace ticked there is ignored the moment advanced mode is on. The five
capability checkboxes decide outright, ticked or not, so a shape's `mayIngest` or `sealedCapable`
does not survive into the advanced view unless its checkbox says so. Reads and Writes are the one
exception: leave either blank and it falls back to the shape's grant for that side; type something
and that replaces it instead. Without the checkboxes deciding outright, a box nobody expanded could
silently clear a capability the shape granted.

Two things worth knowing before you use it:

**Creating a client consents to it.** `set_client_grant` writes `consented_at` with the grant, and
an owner filling in this form behind his own password has approved it more directly than the consent
screen asks. A client that registers itself still waits at that screen, because nobody approved that
one yet.

**A client secret is shown once.** Tick "Issue a client secret" and the page displays it at
creation; the store keeps only `hash_token` of it. Losing it means issuing another client. That is
stricter than `AUTH_TOKENS`, which holds its tokens in plaintext, and deliberately so: a console
reachable from a browser must not be a place every credential can be read out of. Leave it unticked
for anything running in a browser, which cannot keep a secret and is bound by PKCE instead.

## Changing a grant

For a static bearer client, edit its entry in `AUTH_TOKENS` in `.env` and restart the server;
`AUTH_TOKENS` is read once at boot and there is no live-reload path for it.

An OAuth client is different: its grant is a row in `oauth_client`, not a line in `.env`, and it
changes without a restart. The console is how: open `/console/clients`, find the client's card, and
use its "Change access" editor, the same shape/scope/advanced form the create form uses. Saving it
calls `set_client_grant`, and `OpaqueTokenAuthenticator` reads the grant off the client row on every
request rather than off the token, so the new grant governs that client's very next call; nothing
reconnects and no token is reissued. The console requires `AUTH_MODE=oauth` and `OWNER_PASSWORD_HASH`
(`deploy/oauth.md`), because it checks the owner's password before it shows a thing.

Without console access, `lumberroom clients` lists every OAuth row and its `client_id` (see
`deploy/oauth.md` §4), and the grant itself is two columns in `oauth_client`, `grant_read` and
`grant_write`, each the same `[{"namespace": "...", "max": "open|private|sealed"}]` shape `AUTH_TOKENS`
uses:

```bash
docker compose exec -T db psql -U lumberroom -d lumberroom -c \
  "UPDATE oauth_client SET grant_read = '[{\"namespace\":\"project:sivella\",\"max\":\"open\"}]'::jsonb, \
   grant_write = '[]'::jsonb WHERE client_id = '<client_id>';"
```

That is a last resort: it bypasses the form's validation, so a malformed namespace or level lands
in the row as written and only surfaces the next time that client calls.
`docs/decisions/0003-grants-in-the-database.md` records why the row lives in the database rather
than an environment variable in the first place: a dynamically registered client does not exist
until it registers, while the server is already running, so its grant cannot live in something read
once at boot. Authority follows the credential, and the two never copy into each other: a static
token's grant lives only in `AUTH_TOKENS`, an OAuth client's grant lives only in its database row.
