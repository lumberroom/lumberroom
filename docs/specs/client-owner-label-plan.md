# The owner names a client: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An approved OAuth client carries an owner-chosen `owner_label` beside `client_name`; the
consent page, the console, the CLI and a bearer route set it; `services::sources` prints it first.

**Architecture:** One column and one CHECK, one port method writing one column, two pure domain
functions, one rule change in the resolver, a field on the consent form, a form on the console card,
a bearer route, a CLI subcommand, and a digest cache clear after every label write. One engine PR.
The hosted fork and the web follow in their own PRs
(`lumberroom-cloud/docs/superpowers/plans/2026-10-03-client-owner-label.md`).

**Tech stack:** Rust, axum, sqlx 0.9 without macros, Postgres 16, server-rendered HTML.

**Spec:** [`client-owner-label.md`](client-owner-label.md). Decision:
[0023](../decisions/0023-the-owner-names-a-client.md), accepted by the owner on 3 October 2026.

## Global constraints

- The spec's SQL (2.1, 2.2), signatures (2.2, 2.3, 2.5), route table (2.6) and copy strings are the
  contract. Paste them; do not paraphrase.
- `set_client_label` updates `owner_label` and no other column.
- Every path that writes a label calls `services::bootstrap::clear_cache()` after the write lands:
  consent, the bearer route, the console.
- The consent headline, `<dl>`, login page, destination line and every warning read `client_name`.
  The label reaches the consent page only as the input's `value`.
- Every interpolation into HTML goes through `escape`. Every label passes `client_name_display` at
  write and at render.
- No label and no registered name may end in a stamp (`claims_a_stamp`).
- Migrations forward-only. The new file takes the date the PR opens and the next free sequence on
  `main`: `000027` while `20261003000026_memory_touch_hot.sql` is the last. The write-time
  conflicts plan also wants the next number; whichever PR opens second takes the one after.
- No new dependency. No new environment variable.
- Prose rules in the CONTEXT block apply to code comments, test names and docs.

## Review focus

1. A self-registered client named "Helper" sending codes to `https://evil.example`, which the owner
   earlier named "Claude Desktop", re-consents: the headline says Helper, the warning names Helper,
   and "Claude Desktop" appears only inside the input's `value` (E3 test
   `a_label_never_reaches_the_headline_or_the_warning`).
2. An owner presses Allow without touching the field on four Hermes registrations: nothing stored,
   all four keep their stamps (E6 `consent_with_the_prefilled_name_stores_nothing`).
3. A 70-letter Devanagari name (210 bytes) is refused with the domain message and the consent
   grants nothing (E1 `owner_label_over_200_bytes_after_cleaning_is_refused`, E6
   `consent_with_an_overlong_name_grants_nothing`).
4. A rename on a client holding a live token: every other column of the row is identical
   afterwards and the token's principal reads and writes what it did (E6
   `a_rename_changes_no_other_column`).
5. A page rendered before the deploy posts without a `label` field: an existing label survives (E6
   `consent_without_the_field_keeps_an_existing_label`).
6. A rename inside the digest cache window: the next `context_bootstrap` prints the new name (E6
   `the_digest_prints_a_new_name_inside_the_cache_window`).

## Merge order

One PR, `feat/client-owner-label`, against `main`. The fork merges it down afterwards through
`scripts/merge-upstream.sh`, after the fork's PR F1 (migration connection) has landed. Order across
repositories is in the fork plan.

## Task table

| id | purpose | files owned | model | depends on | output contract | gate |
|---|---|---|---|---|---|---|
| E-L | lock interfaces | `migrations/<date>000027_client_owner_label.sql`; `src/ports/oauth.rs`; in `src/domain/oauth.rs` only `MAX_CLIENT_NAME` and the `claims_a_stamp` and `owner_label` signatures with `todo!()` bodies; in `src/services/sources.rs` only the rename `name_clients` to `pub fn client_labels` and the test double's new method; `src/authserver/pages.rs` `ClientView` fields; every `OauthClientRecord` literal and every `OauthStore` test double (`src/authserver/routes.rs`, `src/adapters/auth/opaque.rs`, `src/console/clients.rs`, `tests/dump_console.rs`); `src/adapters/postgres/oauth.rs` placeholder `owner_label: None` and a `set_client_label` returning `DomainError::internal("not built")` | lead (opus) | none | one commit; SQL and signatures verbatim from spec 2.1 to 2.5; `routes.rs` uses `crate::domain::oauth::MAX_CLIENT_NAME` and drops its private const | `./scripts/cargo.sh check --all-targets` clean; migration applies to an empty scratch database |
| E1 | domain functions and resolver rule | `src/domain/oauth.rs`, `src/services/sources.rs` | opus | E-L | `claims_a_stamp` and `owner_label` per spec 2.3; `client_labels` per spec 2.4; unit tests listed under E1 | `cargo check` clean in own files; both modules' tests run in a scratch crate and pass |
| E2 | Postgres adapter | `src/adapters/postgres/oauth.rs` | opus | E-L | `owner_label` in `client_from_row` and the three client SELECTs; `set_client_label` with the spec 2.2 statement as a `const` | `cargo check` clean in own file; statement matches spec 2.2 |
| E3 | registration refusal, consent field, bearer route, listing | `src/authserver/pages.rs`, `src/authserver/routes.rs`, `src/authserver/auth.css` | opus | E-L | spec 2.3 registration paragraph, 2.5, 2.6 (authorization server and listing), cache clears; unit tests listed under E3 | `cargo check` clean in own files; `pages.rs` tests run in a scratch crate and pass |
| E4 | console rename and manual-name refusal | `src/console/clients.rs` | sonnet | E-L | spec 2.6 console paragraph and the 2.3 manual-client refusal; cache clear; `wire_in` with the exact route line for `src/console/mod.rs` | `cargo check` clean in own file |
| E5 | CLI | `crates/lumberroom/src/commands.rs`, `crates/lumberroom/src/wire.rs`, `crates/lumberroom/src/format.rs`, `crates/lumberroom/src/lib.rs` | sonnet | E-L | spec 2.6 CLI paragraph; `format.rs` unit tests for `client_line` | `./scripts/cargo.sh check -p lumberroom` clean |
| E-W | wiring pass | `src/console/mod.rs`, anything named in `wire_in` | lead (opus) | E1 to E5 | every `wire_in` applied | `./scripts/cargo.sh check --all-targets` clean; `./scripts/cargo.sh test -j 1 --lib` passes |
| E6 | integration tests and gate script | `tests/client_label.rs` (new), `tests/mcp_source.rs`, `scripts/oauth-flow-test.sh` | opus | E-W | its own harness and consent driver (below), the tests listed under E6, each failing with its behaviour removed | lead runs `./scripts/cargo.sh test -j 1 --test client_label --test mcp_source --test console` and the full suite, then `./scripts/oauth-flow-test.sh` |
| E7 | docs | `docs/decisions/README.md`, `docs/decisions/0020-mcp-names-the-source.md` (status line only), `docs/decisions/0023-the-owner-names-a-client.md` (status and run line), `docs/managing.md`, `CHANGELOG.md`, `docs/specs/client-owner-label.md` (status line) | sonnet | E6 green | README row for 0023; 0020 status gains `partly superseded by 0023 (a client's name can be the owner's)`; 0023 status names the commands run and their pass counts; `docs/managing.md` gains "Naming a client" under Clients; CHANGELOG Unreleased entry naming the registration refusal as a behaviour change | `grep -rP '\x{2014}'` clean on touched files; every count quoted matches the lead's run |
| E-R | blind whole-branch review | none (read only) | opus, fresh agent with no session context | E7 | findings against spec section 4 and this plan's Review focus, severity-tagged | lead acts on every blocker before the PR opens |

## Task detail

### E-L: lock

- [ ] Write the migration from spec 2.1 verbatim.
- [ ] Add `owner_label: Option<String>` to `OauthClientRecord` and `set_client_label` to
  `OauthStore`, doc comments from spec 2.2.
- [ ] In `src/domain/oauth.rs` add `pub const MAX_CLIENT_NAME: usize = 200;`,
  `pub fn claims_a_stamp(name: &str) -> bool { todo!("E1") }` and
  `pub fn owner_label(typed: &str, registered: &str) -> Result<Option<String>> { todo!("E1") }`.
- [ ] In `src/services/sources.rs` rename `name_clients` to `pub fn client_labels` and update its
  callers and tests. Behaviour stays as it is until E1.
- [ ] Add `name_field: Option<&'a str>` and `same_name: usize` to `ClientView`; `consent_page`
  passes `name_field: Some(&client.client_name), same_name: 0` until E3; the `pages.rs` test helper
  `view()` sets `name_field: Some("Claude"), same_name: 0`.
- [ ] Add `owner_label: None` to every `OauthClientRecord` literal and `unimplemented!()` bodies for
  `set_client_label` in every test double.
- [ ] Check, commit (lead only), record the commit id in the batch log.

### E1: domain and resolver

Interfaces consumed: `MAX_CLIENT_NAME`, `client_name_display`, `comparable_client_name`,
`DomainError::validation`, `OauthClientRecord.owner_label`. Produced: `claims_a_stamp`,
`owner_label`, `client_labels`.

```rust
pub fn claims_a_stamp(name: &str) -> bool {
    // Folded the way names are compared, so fullwidth parentheses, case and invisible characters
    // cannot hide the shape.
    let folded = comparable_client_name(name);
    let Some(body) = folded.strip_suffix(')') else { return false };
    let Some(open) = body.rfind('(') else { return false };
    let last = &body[open + 1..];
    last.starts_with("added") || last == "not approved"
}

pub fn owner_label(typed: &str, registered: &str) -> Result<Option<String>> {
    let cleaned = client_name_display(typed);
    if cleaned.is_empty() || cleaned == client_name_display(registered) {
        return Ok(None);
    }
    if cleaned.len() > MAX_CLIENT_NAME {
        return Err(DomainError::validation(
            "that name is too long: the limit is 200 bytes, and a letter outside the Latin \
             alphabet takes two to four",
        ));
    }
    if claims_a_stamp(&cleaned) {
        return Err(DomainError::validation(
            "a name cannot end in \"(added ...)\" or \"(not approved)\": lumberroom adds those \
             words itself to tell clients apart",
        ));
    }
    Ok(Some(cleaned))
}
```

The equality check runs before the length and stamp checks on purpose: a client whose stored name
predates them (a hand-issued client named past 200 bytes in the console, or one registered under a
stamp-shaped name) can still be re-consented with the field untouched.

`client_labels` keeps `name_clients`' structure. The printed name becomes
`display(c.owner_label.as_deref().unwrap_or(&c.client_name))`, and the approved arm becomes:

```rust
Some(_) if group.len() < 2 => name,
Some(_) if c.owner_label.is_some()
    && group.iter().filter(|o| o.owner_label.is_some()).count() == 1 => name,
Some(_) => format!("{name} (added {})", stamp(c, group)),
```

Unit tests, `src/domain/oauth.rs`:

- `owner_label_cleans_and_keeps_a_new_name`: `(" Claude\u{200B} laptop ", "Claude")` gives `Some("Claude laptop")`.
- `owner_label_equal_to_the_registered_name_is_none`: `("Codex", "Co\u{202E}dex")` gives `None`.
- `owner_label_blank_or_invisible_is_none`: `"   "` and `"\u{200B}\u{2066}"` give `None`.
- `owner_label_over_200_bytes_after_cleaning_is_refused`: 70 copies of `"\u{0915}"` (210 bytes) is an error whose message contains `200 bytes`.
- `owner_label_at_200_bytes_of_multibyte_text_is_kept`: 66 copies of `"\u{0915}"` plus `"ab"` (200 bytes) is kept.
- `owner_label_strips_bidi_and_newlines`: `"Codex\n### Registry\u{202E}"` gives `Some("Codex ### Registry")`.
- `owner_label_refuses_a_stamp_shaped_name`: `("Codex (added 1 Sep)", "Codex CLI")` is an error naming `(added ...)`.
- `owner_label_equal_to_a_stamp_shaped_registered_name_is_none`: `("Old (added 1 Sep)", "Old (added 1 Sep)")` gives `None`.
- `owner_label_equal_to_an_overlong_registered_name_is_none`: a 210-byte name typed back unchanged gives `None`.
- `a_name_ending_in_an_added_stamp_claims_one`: `"Codex (added 1 Sep)"`, `"Codex (Added 1 Sep, 13:02)"`, `"Codex \u{FF08}added 1 Sep\u{FF09}"`.
- `a_name_ending_in_not_approved_claims_one`: `"Codex (not approved)"`, `"Codex (NOT  approved)"`.
- `a_parenthesis_that_is_not_last_claims_nothing`: `"Codex (added 1 Sep) laptop"` and `"Claude Code (plugin:lumberroom-memory:lumberroom)"` give false.

Unit tests, `src/services/sources.rs` (extend the `client()` helper with an `owner_label` setter):

- `a_named_client_prints_its_label`
- `a_cleared_label_prints_the_registered_name`
- `one_named_client_in_a_group_prints_bare_and_the_others_carry_dates`: A registered `Codex CLI`
  labelled `Codex`, B registered `Codex`; A reads `Codex`, B reads `Codex (added 3 Sep)`.
- `two_named_clients_that_collide_both_carry_dates`
- `a_label_equal_to_an_unapproved_name_leaves_that_client_not_approved`
- `a_named_revoked_client_keeps_its_label`
- `a_label_carrying_line_breaks_stays_on_one_line`

Every existing test in both modules keeps passing unchanged.

### E2: adapter

```rust
const SET_CLIENT_LABEL: &str = "UPDATE oauth_client SET owner_label = $2
 WHERE client_id = $1 AND consented_at IS NOT NULL
RETURNING client_id";
```

`set_client_label` binds `client_id` and `label`, `fetch_optional`, answers `is_some()`. Add
`owner_label` to the column list of `find_client` and both `list_clients` statements, and read it
in `client_from_row`.

### E3: registration, consent and bearer route

Interfaces consumed: `claims_a_stamp`, `owner_label`, `client_labels`, `set_client_label`,
`ClientView` fields, `crate::services::bootstrap::clear_cache`. Produced: `POST
/oauth/clients/{client_id}/label`, `same_name_count`.

- [ ] `register`: after the length check on the cleaned name, refuse `claims_a_stamp(&name)` with
  400 `invalid_client_metadata` and the spec 2.3 registration message.
- [ ] `pages::consent` renders the field and hint from spec 2.5 when `name_field` is `Some`, and
  neither when it is `None`. The count sentence is omitted when `self_registered_notice` returned
  the `warn alarm` class: have it return the class beside the two strings, or compute
  `mismatched_claim` once and pass it down.
- [ ] `auth.css`: `input[type=text]` gets the `input[type=password]` rules, light and dark; add
  `.hint{margin:.35rem 0 1rem;font-size:.85rem;color:#5b5f66}` with a dark-mode colour matching
  the file's other muted text.
- [ ] `routes.rs`: `same_name_count(store, client) -> usize`; `consent_page` callers await it;
  `name_field` is `Some(owner_label)` when the client has one, else
  `Some(client_name_display(&client.client_name))`.
- [ ] `FlowForm.label`; validation directly after the profile check; write after
  `set_client_grant`, then `clear_cache()`, per spec 2.5.
- [ ] The label route per spec 2.6. Read the body as `serde_json::Value`; require an object with the
  key `label` holding a string or null; `clear_cache()` after a write that landed.
- [ ] `client_listing_entry(c, label)` gains `owner_label` and `label`.

Unit tests, `src/authserver/pages.rs`:

- `the_label_field_carries_the_name_field_escaped`
- `no_name_field_renders_no_input_and_no_hint`
- `a_label_never_reaches_the_headline_or_the_warning`: registered `Helper`, redirect
  `https://evil.example/cb`, `self_registered: true`, `name_field: Some("Claude Desktop")`. Strip the
  input element from the page; the rest contains `Helper` twice or more and no `Claude Desktop`.
- `the_same_name_count_sits_in_the_hint`
- `the_same_name_count_is_absent_under_the_brand_alarm`: registered `Claude`, redirect
  `https://chatgpt.com/cb`, `same_name: 2`; the page carries `warn alarm` and no `other approved`.
- `a_hostile_name_field_cannot_close_the_attribute`: `name_field: Some("\"><script>alert(1)</script>")`;
  the page contains no `<script>` and contains `&quot;&gt;&lt;script&gt;`.

### E4: console

- [ ] Card name is `labels[&c.client_id]` from `client_labels` over the listing's clients; meta line
  adds `registered as {escape(client_name_display(&c.client_name))}` when `owner_label` is set.
- [ ] Rename form on every card with `consented_at` set, revoked included, per spec 2.6.
- [ ] Handler `label(State, HeaderMap, Path(id), Bytes)`: guard, `console_csrf_ok(LABEL_ACTION, id)`,
  `action == "clear"` sends `None`, else `owner_label(form.one("label"), &client.client_name)`;
  a missing or unapproved client re-renders the listing with `There is no approved client with that
  id.` and 404; a write that lands calls `crate::services::bootstrap::clear_cache()`.
- [ ] `create` refuses a manual client name that `claims_a_stamp` accepts, re-rendering the listing
  with the domain's stamp message and 400.
- [ ] `done_line("renamed")` answers `Name saved.`
- [ ] Unit tests: `a_named_card_shows_the_label_and_the_registered_name_escaped`,
  `a_revoked_card_keeps_its_rename_form`, `an_unapproved_card_has_no_rename_form`.
- [ ] `wire_in`: `.route("/console/clients/{id}/label", post(clients::label))` for
  `src/console/mod.rs`.

### E5: CLI

- [ ] `commands::clients` dispatches `positional_at(1) == Some("rename")` to a new `clients_rename`.
- [ ] `clients_rename` requires `positional_at(2)`; `--clear` sends `{"label": null}`, else the
  remaining positionals joined with spaces; `POST /oauth/clients/{id}/label` via `http_request`.
- [ ] 200 prints `<client_id> now reads as <label>`; 404 prints `no approved client has id <id>`;
  400 prints the server's `detail`; other statuses print `rename failed (<status>): <compact body>`.
- [ ] `wire::ClientRecord` gains the two `#[serde(default)]` fields; `format::client_line` per spec.
- [ ] `lib.rs` usage text names `clients rename`.
- [ ] Unit tests in `format.rs`: `client_line_prints_the_label_and_the_registered_name`,
  `client_line_falls_back_to_client_name_for_an_older_server`.

### E6: integration

No engine test drives the OAuth consent flow in Rust today: `tests/console.rs` skips the password
form and mints a session cookie with the signer, and one integration test file cannot import
another. E6 therefore writes, inside `tests/client_label.rs`:

- a harness modelled on `setup()` in `tests/console.rs:134` (database, config with
  `AUTH_MODE=oauth`, a static `AUTH_TOKENS` entry holding `registry_write` and one without it, app
  state, a live server on a loopback port, a session cookie minted by the same signer). It may call
  what `tests/common/mod.rs` already exports and must not edit that file;
- a consent driver modelled on `authorize_login_consent` in `scripts/oauth-flow-test.sh:301-390`:
  register over `POST /oauth/register`, build an S256 challenge with `sha2` and `base64` (both
  already dependencies), `GET /oauth/authorize` with the session cookie, read the `csrf` hidden
  field and the `label` input's `value` from the page, then `POST /oauth/consent` with
  `action=allow`, a profile, the flow fields and an optional `label`, and answer the response;
- a console driver for test 13 that reads the per-card CSRF token the way `client_form_csrf` in
  `tests/console.rs:1072` does.

Estimated size of the helpers: 250 to 300 lines before the tests (an estimate, not a count).

`tests/client_label.rs` tests, each against a live Postgres through the real router:

1. `registration_never_stores_a_label`
2. `registration_refuses_a_stamp_shaped_name`
3. `the_database_refuses_a_label_on_an_unapproved_client` (raw `UPDATE`, SQLSTATE `23514`)
4. `consent_with_a_changed_name_stores_it`
5. `consent_with_the_prefilled_name_stores_nothing`
6. `consent_without_the_field_keeps_an_existing_label`
7. `consent_with_an_overlong_name_grants_nothing` (`consented_at` stays null, no code row)
8. `a_rename_changes_no_other_column`: `SELECT to_jsonb(c) - 'owner_label' FROM oauth_client c`
   before and after, equal; token rows unchanged; the client's token still authenticates with the
   same `read` and `write`
9. `the_label_route_refuses_a_cookie_a_missing_token_and_a_narrow_token`
10. `the_label_route_answers_404_for_unknown_and_unapproved_clients`
11. `a_revoked_client_can_be_renamed`
12. `clearing_falls_back_to_the_registered_name`
13. `the_listing_carries_owner_label_and_the_resolved_label`
14. `a_hostile_label_is_escaped_on_the_consent_page_and_the_console`
15. `the_digest_prints_a_new_name_inside_the_cache_window`: `context_bootstrap` once, rename through
    the route, `context_bootstrap` again inside 30 seconds; the second digest carries the new name

`tests/mcp_source.rs`: `a_renamed_client_reads_as_its_label_in_mcp`,
`a_cleared_label_reads_as_the_registered_name_in_mcp`.

`scripts/oauth-flow-test.sh`: after the consent step, `lumberroom clients rename <id> "flow gate"`
and `lumberroom clients` shows `flow gate`; `pass` and `die` lines in the script's style.

## CONTEXT block for every delegated agent

```
You are implementing one task of the lumberroom engine plan
docs/specs/client-owner-label-plan.md, spec docs/specs/client-owner-label.md, in this repository.
Read both, then docs/architecture.md and the files you
own. Read CLAUDE.md and CONTRIBUTING.md "Architecture rules" and "Prose".

Hard rules
- Touch only the files your task owns. If you need a change elsewhere, return it as wire_in.
- Run no git command. Run no cargo test. You may run ./scripts/cargo.sh check --all-targets and
  expect errors in files other agents own; grep the output for your own files.
- To execute pure code, copy it into ~/.cache/agent-scratch/<task-id>/, build with
  CARGO_TARGET_DIR=~/.cache/agent-scratch/target, run its tests, and delete the directory before
  you return.
- Domain and services never import adapters. sqlx without macros: query/query_as with .bind().
  SQL lives only in src/adapters/postgres.
- Paste the spec's SQL, signatures, routes and copy strings exactly.
- The consent page judges a client on client_name and the redirect URI. The owner label appears on
  that page only as the input's value.
- Every value interpolated into HTML goes through escape. Every label goes through
  client_name_display at write and at render.
- set_client_label writes owner_label and no other column. Every caller that writes a label calls
  services::bootstrap::clear_cache() after the write lands.
- No label or registered name may end in "(added ...)" or "(not approved)" (claims_a_stamp).

Ground truth
- Engine main is 2b17048 (3 October 2026). Consent brand warnings (#95) are in pages.rs.
- name_clients/stamp in src/services/sources.rs produce every "(added ...)" suffix at read time.
- MAX_CLIENT_NAME is 200 bytes after cleaning.
- context_bootstrap caches its digest for BOOTSTRAP_CACHE_MS (30000 ms default).

Writing rules (code comments, test names, docs, your reply)
- No em dashes anywhere. Grep your files with grep -rP '\x{2014}' before returning.
- No adverbs where a verb works. Active voice with a human subject. No "Note that", no "Here's
  what", no "not X, it's Y". Comments say why and flag traps; they do not narrate the code.
- No AI attribution anywhere: no co-author lines, no "generated" notes.
- Say "implemented" for code you have not run. Say "passed" only for tests you ran, with the count.

Return JSON:
{"task": "<id>", "files_changed": [...], "wire_in": [...], "tests_written": [...],
 "ran": [{"command": "...", "result": "..."}], "not_done": [...], "open_risks": [...]}

If a task burns a large budget with no output, the lead does it directly rather than re-delegating.
```
