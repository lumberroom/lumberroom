# The owner names a client

> **Draft.** Design only. Nothing is built yet, and this document changes as the build proceeds.

3 October 2026. Status: design accepted by the owner on 3 October 2026, nothing built. Issue
[lumberroom/lumberroom#89](https://github.com/lumberroom/lumberroom/issues/89). Decision record:
[0023](../decisions/0023-the-owner-names-a-client.md). Plan:
[`client-owner-label-plan.md`](client-owner-label-plan.md). Hosted companion:
`lumberroom-cloud/docs/superpowers/specs/2026-10-03-client-owner-label-fork.md`.

Every file and line reference below was read on engine `main` at `2b17048` (3 October 2026), which
carries #95 (consent brand warnings).

## 1. Why

A client's name today is the `client_name` it sent at dynamic registration. The owner reads that
name on the consent page, the console, `lumberroom clients`, and as `source` on every MCP answer
that names a writer. Nothing lets the owner change it.

Where the suffixes come from. The issue says two "Claude" clients become "Claude" and "Claude (2)".
The code says otherwise. `name_clients` and `stamp` in `src/services/sources.rs` (decision 0020)
group approved clients whose printed names compare equal and give each the date it was added:
`Codex (added 1 Sep)`, then the minute on a same-day tie, then the full instant on a same-minute
tie. The hosted fork prints the same strings through its wrapper (fork decision 0122). The owner
reported these on the lumberroom.cloud Overview on 3 October 2026:

| Printed | What produced it |
|---|---|
| `Claude Code (plugin:lumberroom-memory:lumberroom) (added 2 Oct)` | registered name `Claude Code (plugin:lumberroom-memory:lumberroom)`, at least one other approved client with that name |
| `Hermes Agent (lumberroom) (added 26 Sep 2026, 11:23:08)`, four times | approved clients registered as `Hermes Agent (lumberroom)`, at least two in the same minute, so the stamp fell through to the full instant |
| `Codex (added 23 Sep)` | a second approved `Codex` |

No suffix is stored. `labels` computes every one at read time, and revoked clients count toward a
group because the rows they wrote still need telling apart. A migration has nothing to strip, and
revoking the stale Hermes clients leaves the live one stamped. Naming it clears the stamp.

## 2. The change

Store an owner-chosen label beside the registered `client_name`, never over it. The resolver prints
the label when there is one. The consent page, the console, the CLI and a new authorization-server
route can set it.

### 2.1 Storage

One forward migration, `migrations/<date the PR opens>0000NN_client_owner_label.sql`, where `NN` is
the next free sequence on `main` when the PR opens:

```sql
-- Decision 0023. The name the owner gives a client, kept beside the name it registered with.
-- Null until the owner names it. Registration never writes this column.
ALTER TABLE oauth_client ADD COLUMN IF NOT EXISTS owner_label text;

-- A label belongs to a client the owner approved. Registration is anonymous, so this is the line
-- that keeps a stranger's client from arriving with a name the owner seems to have chosen.
ALTER TABLE oauth_client ADD CONSTRAINT oauth_client_label_needs_consent
  CHECK (owner_label IS NULL
         OR (consented_at IS NOT NULL AND octet_length(owner_label) BETWEEN 1 AND 200));
```

The table name stays unqualified, as in every engine migration. The hosted fork moved this table to
`control` and handles that on its side (companion note, section 3).

### 2.2 Port

`src/ports/oauth.rs`:

```rust
pub struct OauthClientRecord {
    // ...every existing field...
    /// The name the owner gave this client, cleaned. `None` until the owner names it, and always
    /// `None` on a client nobody approved. `client_name` keeps what the client registered with.
    pub owner_label: Option<String>,
}

#[async_trait]
pub trait OauthStore: Send + Sync {
    // ...every existing method...
    /// Sets or clears the owner's name for an approved client, revoked ones included. Writes
    /// `owner_label` and no other column: no grant, no `consented_at`, no token. `false` when no
    /// approved client has that id.
    async fn set_client_label(&self, client_id: &str, label: Option<&str>) -> Result<bool>;
}
```

The Postgres adapter reads `owner_label` in `find_client` and both `list_clients` statements, and
writes it with one statement:

```sql
UPDATE oauth_client SET owner_label = $2
 WHERE client_id = $1 AND consented_at IS NOT NULL
RETURNING client_id
```

### 2.3 Domain

`src/domain/oauth.rs` gains the cap, moved from the private const in `src/authserver/routes.rs:53`,
and two pure functions:

```rust
/// The longest client name stored, in bytes of UTF-8 after cleaning. Registration and the owner's
/// label share it.
pub const MAX_CLIENT_NAME: usize = 200;

/// True when a name, cleaned and lowercased, ends in a parenthesis that opens with `(added ` or
/// reads `(not approved)`. Those are the words `services::sources` appends to tell duplicates
/// apart, so a name ending in one could pass for another client's disambiguated label.
pub fn claims_a_stamp(name: &str) -> bool;

/// What the owner typed, as it is stored. Cleaned by `client_name_display`, then, in this order:
/// empty, or equal to the cleaned registered name, answers `Ok(None)`, which clears the label;
/// longer than `MAX_CLIENT_NAME` bytes answers a validation error; a name `claims_a_stamp` accepts
/// answers a validation error; anything else is the label.
pub fn owner_label(typed: &str, registered: &str) -> Result<Option<String>>;
```

`claims_a_stamp` looks only at the last parenthesised group, so `Claude Code
(plugin:lumberroom-memory:lumberroom)` passes and `Codex (added 1 Sep)`, `Codex (Added 1 Sep, 13:02)`
and `Codex (not approved)` do not.

Equality is exact on the cleaned strings and runs first. An owner who leaves the consent field as
it was prefilled has chosen nothing, and the row keeps `NULL`. That rule stops every consent from
turning into a chosen name and defeating the duplicate rule in 2.4. Running it before the length and
stamp checks lets a client whose stored name predates those checks be re-approved unchanged: the
console's manual-client form has no length cap today, so a hand-issued client can hold a name over
200 bytes.

The validation messages, which every surface prints as they stand:

- too long: `that name is too long: the limit is 200 bytes, and a letter outside the Latin alphabet
  takes two to four`
- a stamp: `a name cannot end in "(added ...)" or "(not approved)": lumberroom adds those words
  itself to tell clients apart`

**Registration refuses a stamp too.** `POST /oauth/register` answers 400 `invalid_client_metadata`
with `client_name cannot end in "(added ...)" or "(not approved)": this server adds those words
itself` when `claims_a_stamp(client_name)` holds, checked after cleaning, beside the length check
(`src/authserver/routes.rs:276-287`). The console's manual-client form (`src/console/clients.rs`
`create`) refuses the same name with the domain message. A row stored before this change keeps its
name; the resolver prints it as it does today.

### 2.4 The resolver

`src/services/sources.rs`. `name_clients` becomes `pub fn client_labels(clients:
&[OauthClientRecord]) -> HashMap<String, String>`, so the authorization server and the console use
the same rule as `labels`. The rule:

- The printed name of a client is `display(owner_label)` when the owner named it, else
  `display(client_name)`. `display` already cleans and substitutes `unnamed client`.
- Approved clients, live and revoked, group by `comparable(printed)`, as today.
- In a group of one, the printed name stands.
- In a larger group holding exactly one named client, that client prints bare and every other
  member carries its `(added ...)` stamp. The owner chose the name; the client that merely
  registered it gives way.
- In any other larger group, every member carries its stamp, as today. Two named clients that
  collide therefore both carry dates. Nothing refuses the second name: the stamp keeps the two apart,
  and the route returns the resolved label so the owner sees the date at once.
- An unapproved client cannot carry a label (2.1). It prints its registered name, with
  `(not approved)` when an approved client prints the same, as today.

One owner holds every client on a self-hosted store, so "the owner chose the name" is always true of
the person whose clients give way. The hosted fork holds many people's clients in one tenant and
narrows the bare arm so one person's label never stamps a colleague's unnamed client (companion note,
section 5).

`stamp` is unchanged. `labels` keeps its contract: one `list_clients(true)` per call, infallible,
identity outside `AUTH_MODE=oauth`.

### 2.5 Consent page

`src/authserver/pages.rs`. `ClientView` gains two fields:

```rust
pub struct ClientView<'a> {
    // ...every existing field...
    /// What the "Name this connection" field holds when the page opens: the owner's label when this
    /// client is being re-consented and has one, else the registered name, cleaned. `None` leaves
    /// the field and its hint off the page. The engine always passes `Some`; the hosted fork passes
    /// `None` when the person consenting may not name this client.
    pub name_field: Option<&'a str>,
    /// Other approved clients, live or revoked, whose cleaned registered name compares equal to
    /// this one's.
    pub same_name: usize,
}
```

Inside the form, after the profile fieldset and before the buttons, when `name_field` is `Some`:

```html
<label class="field" for="label">Name this connection</label>
<input id="label" name="label" type="text" maxlength="200" value="{escape(name_field)}"
 autocomplete="off" spellcheck="false">
<p class="hint">{count}You see this name beside everything this client writes. Leave it as it is to
keep the name the client registered.</p>
```

`{count}` is empty unless `same_name > 0`, when it reads `{n} other approved client{s} registered
under this name. `. It sits in the naming hint, below every warning, and says nothing about whether
this client is one of them. A page that carries the `warn alarm` paragraph omits it: a page telling
the owner a client borrowed a brand must not also tell them they approved that name before.

What does not change: the headline, the `<dl>` client name, the login page, the destination line,
`self_registered_notice` and `mismatched_claim` all read `client_name` and the redirect URI. The
label appears in one place on the page, the `value` attribute of the input. `auth.css` styles
`input[type=password]` only today, so it gains the same rule for `input[type=text]` and a `.hint`
class.

`src/authserver/routes.rs`:

- `consent_page` takes the count from a new async helper, `same_name_count`, which calls
  `list_clients(true)` and compares `comparable(display(client_name))` across approved clients
  other than this one. A store error logs at warn and answers 0: a hint never blocks a consent.
  Both callers, `login` and the session branch of `authorize`, await it first.
- `FlowForm` gains `label: Option<String>` with `#[serde(default)]`.
- `consent`, directly after the profile check: when `form.label` is `Some`, run
  `owner_label(text, &client.client_name)`. An error renders the existing error page with status
  400, title `name not saved`, and the domain's message followed by `Nothing was granted. Go back
  and change it.`
- After `set_client_grant` succeeds: `Some(text)` writes `set_client_label(id, result)` and then
  calls `services::bootstrap::clear_cache()`; `None` (the field was absent, as on a page rendered
  before this change) leaves the label alone. A failed label write logs at warn and the consent
  completes. The grant is the act the owner came for, and a name can be set afterwards.

The CSRF token keeps its bindings (client id, redirect URI, challenge, state). The label is owner
input on an owner-authenticated, CSRF-checked form, so it needs no binding of its own.

### 2.6 Renaming afterwards

**Authorization server.** `POST /oauth/clients/{client_id}/label`, in `src/authserver/routes.rs`,
registered beside `GET /oauth/clients`.

- Auth: a bearer whose principal holds `registry_write`, as the bearer branch of `GET
  /oauth/clients` requires. The consent session cookie is refused with 401: a cookie-authenticated
  JSON write would need its own CSRF defence, and the console already has a form for the browser.
- Body: a JSON object carrying the key `label`, a string or `null`. A missing key answers 400
  `invalid_request`, `label is required; send null to clear it`.
- Revoked clients may be renamed. Unapproved ones answer 404.
- A write that lands calls `services::bootstrap::clear_cache()` before answering.

| Status | Body |
|---|---|
| 200 | `{"client_id": "...", "client_name": "<registered, cleaned>", "owner_label": "..." \| null, "label": "<resolved>"}` |
| 400 | `{"error": "invalid_label", "detail": "<the domain message>"}` or the `invalid_request` above |
| 401 | `{"error": "unauthorized"}` with `WWW-Authenticate: Bearer` |
| 403 | `{"error": "forbidden", "detail": "client <client> may not rename OAuth clients"}` |
| 404 | `{"error": "not_found", "detail": "there is no approved client with that id"}` |

`label` comes from `client_labels` over `list_clients(true)` read after the write. The handler logs
`tracing::info!(client_id, cleared = <bool>, "owner renamed a client")` and leaves the text out of
the log.

**Listing.** `client_listing_entry` gains `"owner_label"` (cleaned or null) and `"label"`. The
handler reads `list_clients(true)` once for the labels and filters out revoked rows afterwards when
`include_revoked` is off, so the listing and MCP print one name.

**Console.** `src/console/clients.rs`. The card's name becomes the resolved label. When the owner
named the client, the meta line adds `registered as <client_name>`. Every consented card, revoked
included, carries a rename form posting to `POST /console/clients/{id}/label` with fields `csrf`
and `label`, a Save button, and a `Use the registered name` button (`name="action"
value="clear"`). A new CSRF action constant, `LABEL_ACTION = "client-label"`, binds the token to the
client id. Success calls `services::bootstrap::clear_cache()` and redirects to
`/console/clients?done=renamed`, which `done_line` prints as `Name saved.` A validation error
re-renders the listing with the message and status 400. The route line goes in
`src/console/mod.rs`.

**CLI.** `crates/lumberroom`.

```
lumberroom clients rename <client_id> <name...>
lumberroom clients rename <client_id> --clear
```

`<name...>` joins the remaining positionals with single spaces. Output on success: `<client_id> now
reads as <label>`. A 404 prints `no approved client has id <client_id>`. `wire::ClientRecord` gains
`#[serde(default)] owner_label: Option<String>` and `#[serde(default)] label: Option<String>`.
`format::client_line` prints `label`, falling back to `client_name` against an older server, and
appends `  (registered as <client_name>)` when `owner_label` is set.

### 2.7 MCP

No tool changes. `source` comes from `labels`, which follows 2.4, so `memory_search`,
`memory_history`, `registry_get`, `registry_history` and `memory_forget` print the owner's name on
their next call. `context_bootstrap` caches its rendered digest, labels included, for
`BOOTSTRAP_CACHE_MS` (30,000 ms by default, `src/config.rs:879`; cache read at
`src/services/bootstrap.rs:117-122`, cleared by `clear_cache` at `:92`). Every path that writes a label (consent, the bearer route, the
console) calls `bootstrap::clear_cache()` after the write lands, so the next digest prints the new
name too. The answer to the issue's first open question is the label alone, reasons in decision
0023.

## 3. Rules the change keeps

- `client_id` stays the identity. `source_client`, exports and the archive keep the id.
- A rename writes `owner_label` and nothing else. The adapter statement names one column.
- The consent page judges a client on its registered name and redirect host.
- Registration never writes a label. `RegistrationRequest` has no such field and no
  `deny_unknown_fields`, `NewOauthClient` has none, and the CHECK refuses one on an unapproved row.
- No name, registered or chosen, may end in the words the resolver appends.
- Every page escapes the label through `escape`. Every surface cleans it through
  `client_name_display` at write and again at render, as it does a registered name.
- A rename writes no audit row. A grant change writes none either (`docs/managing.md`, "Changing
  what a client may reach").

## 4. Threats

| Attempt | What stops it |
|---|---|
| A stranger registers a client named after one the owner already named, hoping the consent page vouches for it | The page prints the registered name and judges the redirect host. The label never reaches the headline, the `<dl>` or a warning. The same-name count sits in the naming hint, claims nothing about this client, and disappears on a page that raises the brand alarm |
| A stranger registers `Codex (added 1 Sep)` so its plain name reads as the owner's disambiguated Codex | Registration refuses a name `claims_a_stamp` accepts |
| A stranger sends `owner_label` or `label` in the registration JSON | serde drops unknown fields; `NewOauthClient` has no label; the CHECK refuses a label on an unapproved row |
| A stranger forges a consent POST carrying a label | The CSRF token and the owner session gate the POST, as they gate the grant |
| A re-consent for a client the owner named shows the owner's label to whoever started the flow | Only the signed-in owner sees the consent page. The label sits in the input value and nowhere else |
| A label carrying markup: `"><script>alert(1)</script>` | `escape` on the consent input, the console card and its form value; JSON on the routes; React on the hosted dashboard |
| A label carrying a newline, bidi override or terminal escape, aimed at the digest, a log line or the CLI | `client_name_display` turns controls into spaces and drops invisible characters at write and at render; the log line carries no label text |
| A full-profile OAuth client renames another client through the bearer route | Accepted. `registry_write` is the owner's own credential (`GrantProfile::registry_write`), the same bar that lists clients and rewrites the registry. The CLI logs in over OAuth, so refusing OAuth principals would refuse the CLI |
| A rename widens or narrows a grant | `set_client_label` updates one column; the integration test compares every other column before and after and checks the live token's principal |
| An over-long or stamp-shaped label after the grant was half written | The consent validates the label before any write, so a refusal grants nothing |

## 5. Tests

Unit, in `src/domain/oauth.rs`, `src/services/sources.rs`, `src/authserver/pages.rs` and
`src/console/clients.rs`; integration, in a new `tests/client_label.rs`. The plan lists each by
name.

## 6. Known limits

- `claims_a_stamp` folds case, width and invisible characters, and NFKC does not map across
  scripts. A name spelled with a Cyrillic `а` in `added`, or with a space inside the word (`add
  ed`), passes the check and can still read like a stamp. The redirect host and the registered name
  on the consent page stay the identity cues; the label is not one.

## 7. Not in this change

- Stopping a harness from registering a new client on every connect. That is how the Hermes
  duplicates arose; the fix belongs in the plugin or in client reuse, not in naming.
- Per-reader names. A client has one label.
- A rename MCP tool. Naming a client is owner administration (decision 0020 kept `list_clients` out
  of the toolset for the same reason).
