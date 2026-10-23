# Phase 2. Every surface connected

> **Ends when:** a fact you tell ChatGPT shows up in Claude Code the next day, and you notice you
> did not repeat yourself.
> System PRD §7

Status: implemented 19 August 2026. Verification is pending. No acceptance script in this spec
has run against a live server yet; `scripts/oauth-flow-test.sh` is the gate for §2. Decisions
[0002](../decisions/0002-built-in-oauth-server.md) and
[0003](../decisions/0003-grants-in-the-database.md) supersede parts of §2 and §3; see the note at
the head of §2.

Phase 1 proved the loop with one client that has lifecycle hooks and accepts a static token.
Neither of those is true of the surfaces in this phase, and both assumptions being false is the
whole difficulty.

The client capability research is in
[`docs/research/client-capabilities.md`](../research/client-capabilities.md), and it settled the two
questions this spec was blocked on: both unidentified surfaces are now identified, and an
authorization server is no longer optional.

---

## 0. Deploy, which is not optional

Every surface here is a browser or mobile client. None of them can reach a laptop. The VM deploy
is step zero, not a leftover from Phase 1.

Requirements it must satisfy before any surface work starts:

- Public HTTPS on a real certificate. Self-signed will be rejected by browser clients.
- A stable hostname, because OAuth redirect URIs and connector registrations are pinned to it.
- The endpoint reachable from outside your network, verified from a phone on mobile data rather
  than from the LAN.

Runbook: [`DEPLOY.md`](../../DEPLOY.md).

---

## 1. The surfaces

Hermes is Nous Research's Hermes Agent, an open-source agent CLI. Cowork is Anthropic's, and runs
on the same connector infrastructure as Claude.ai, which has a consequence for grants below.

| Surface | Remote MCP | Auth | Client identity | Auto-recall | Order |
|---|---|---|---|---|---|
| Claude Code (Mac, wired) | yes | bearer header | distinct token | SessionStart hook | done |
| Claude Code (second install) | yes | bearer header | distinct token | SessionStart hook | 1 |
| Hermes | yes, native | bearer header | issued token only | none | 2 |
| OpenWebUI | yes, native since v0.6.31 | bearer header | issued token only | **Filter `inlet`, the only real one** | 3 |
| Claude.ai web | yes | OAuth | per-connector client_id | none | 4 |
| Cowork | yes | OAuth, same as Claude.ai | **indistinguishable from Claude.ai** | none | 4 |
| Claude.ai mobile | yes, account-synced | OAuth, same as Claude.ai | indistinguishable from Claude.ai | none | 5 |
| ChatGPT web | yes, Developer Mode, Plus and above | assume OAuth until tested | unverified | none known | 6 |
| ChatGPT mobile | unverified | unverified | unverified | unverified | 7 |

Two rows deserve attention before anything is built.

**OpenWebUI is the strategically important one**, out of proportion to how much you use it. Its
Filter `inlet` function runs on every incoming message, outside the model's tool-choice loop, and can
be made to call the memory server before the model sees the message. It is the only mechanism across
all eight surfaces that can *force* recall rather than ask for it. That makes it the one place the
project's largest unknown can be answered rather than measured: everywhere else, a low write rate is
ambiguous between "the model chose not to" and "the model never considered it."

**ChatGPT is the least understood**, and its row is built on community reports rather than primary
documentation. Two of its three unknowns are settled by ten minutes of clicking, not more research.
See the end of this spec.

The second Claude Code install is first regardless of what the research says. It is the only
surface that needs no new server capability, so it proves multi-client behaviour, per-client
instrumentation, and cross-client recall against a client that is already known to work. Debugging
"the grant is wrong" and "the OAuth flow is wrong" at the same time is avoidable, so avoid it.

---

## 2. Auth escalation, which is now mandatory

> **Superseded in part.** Logto is no longer the Phase 2 baseline. Decision
> [0002](../decisions/0002-built-in-oauth-server.md) replaces it with an authorization server built
> into lumberroom, for the reasons recorded there. The section below is left as written because the list
> of things the server must get exactly right is the acceptance checklist the built-in server was
> actually held to, and it still describes what any authorization server must do, borrowed or
> built-in. Where it says the procedure in `deploy/logto.md` "has never been executed", that is
> still true.
>
> **The static-header beta request below is retired, not just Logto.** The two consequences at the
> end of this section asked the owner to email `mcp-review@anthropic.com` for Anthropic's
> invite-gated `static_headers` beta, on the theory that a grant would let four surfaces connect
> with no authorization server at all. Decision 0002 removes the cost that beta was an escape hatch
> from: lumberroom issues its own tokens, so Claude.ai, Cowork, mobile and ChatGPT connect over OAuth with
> nothing to request from anyone. There is nothing left for the beta to unblock. The reasoning
> below, about why `static_headers` could not have been the baseline even if granted, still stands.

Phase 1 shipped two auth modes and used the simpler one. The research settles which is needed.

**Static bearer tokens work today on exactly three surfaces:** Claude Code, Hermes and OpenWebUI.
All three are self-configured tools that need the convenience least.

**The Claude.ai family needs OAuth.** Anthropic's auth-type table does include `static_headers`, but
it is beta and invite-gated behind an email to `mcp-review@anthropic.com`, and users were still
seeing OAuth-only fields as recently as August 2026. ChatGPT should be assumed OAuth-only until
someone tests it by hand.

**So Logto is the Phase 2 baseline, not a contingency.** It is the only path that reaches Claude.ai,
Cowork, mobile and probably ChatGPT, regardless of whether beta access ever arrives. Bearer mode
stays for the three surfaces that already work with it, which is why both modes were built.

One consequence stands, and one is retired below:

- **Retired: ask for the static-header beta.** Superseded by decision 0002. lumberroom is its own
  authorization server, so the four surfaces this beta would have unblocked connect over OAuth with
  nothing to request from Anthropic. There is no version of this action item still worth doing.
- **Budget Logto as its own piece of work.** The Phase 1 PRD called the OAuth integration the
  schedule risk. It has not cost anything yet only because it was deferred, and deferring it is no
  longer available.

Procedure: [`deploy/logto.md`](../../deploy/logto.md), which has never been executed. Treat the first
run as discovery.

### What the server must get exactly right

These come out of the research and several are silent failures, which is what makes them expensive.

- **Answer an unauthenticated request with `401` and a `WWW-Authenticate` header** carrying the
  resource-metadata pointer. A `200` with an error body is silently ignored, and Claude.ai fails
  before it ever shows a login screen. The server already does this. **Claude Code's fallback probing
  masks this class of bug, so a green result from Claude Code proves nothing about the browser
  surfaces**, so test against the real client.
- **Serve the protected-resource metadata at both paths**, the domain root and the path-suffixed
  variant. The server already serves both.
- **Prefer CIMD or manually issued credentials over Dynamic Client Registration.** DCR mints a fresh
  client on every connection for both Claude and ChatGPT, which accumulates phantom registrations.
  Decision 0002 answers the accumulation cost directly rather than avoiding it: registration is not
  authorization, so a self-registered client holds an empty grant until the owner consents at a
  login screen. A phantom registration sits in `oauth_client` seeing nothing, which is why DCR stays
  on rather than needing to be worked around.
- **Advertise `code_challenge_methods_supported: ["S256"]`.** Newer clients refuse to proceed
  without it. Logto's job, but verify it.
- **`/token` must accept form encoding while `/register` takes JSON.** A stack wired only for JSON
  returns 415 on token exchange while registration succeeds, which reads as almost-working.
- **Ten seconds** for discovery, registration and token calls. A cold start fails intermittently with
  no useful client-side error.
- **Anthropic's fixed egress range reaches the authorization server too**, not just `/mcp`. Any
  allowlist that covers one and not the other fails silently at discovery.
- **IPv4 and public routability.** No AAAA-only, CGNAT or private hosts. Localhost serves nothing but
  Claude Code.

## 3. Per-client identity and grants

A grant that cannot tell two clients apart is decoration. This is the section that makes §4.5 of
the system PRD real.

**What exists.** Every request resolves to a principal carrying a client id, a read glob list and
a write glob list. Reads narrow silently to the intersection; writes outside the grant fail with
403. Both paths are tested. `memory.source_client` and `tool_calls.client` already record which
client did what.

**What is missing.** One entry per surface, and a reliable way to tell the surfaces apart.

**Identity, in descending order of reliability:**

1. **A distinct credential per client.** One token, or one manually issued OAuth client id, per
   surface. The server keys the grant off the credential and nothing the client sends can override
   it. This is the only mechanism that is actually a boundary. Anthropic recommends this over
   Dynamic Client Registration for the same reason: DCR mints a meaningless fresh client id on every
   connection.
2. `clientInfo` from the initialize handshake. Free text, self-declared, no registry. Three
   different values have been reported for Claude.ai alone, and **Hermes is documented to identify
   itself as `"Claude Code"`** in some cases. Log it, never authorize on it.
3. User-Agent and egress IP. Anthropic's traffic comes from a fixed range, which confirms "this is
   Anthropic's cloud" and not which of its products.

Only the first is used for policy. The others feed the per-client rates in §5, where approximate is
fine.

**One surface family cannot be split.** Claude.ai web, Claude.ai mobile, Claude Desktop and Cowork
share one connector infrastructure and one OAuth callback. From the server they are a single client,
and no signal distinguishes them. If Cowork's autonomous sessions ever need a different grant from
interactive chat, the only lever is registering the connector twice under two separately issued
credentials and treating them as two clients. Decide that before issuing credentials, because
retrofitting it means re-adding connectors by hand on every device.

**No client sends a stable per-install identifier.** Per-device policy, if it is ever wanted, has to
be minted at setup time; it cannot be read off the wire on any surface.

**Starting grants.** The namespace axis is all that exists until Phase 3, so grants start coarse
and tighten when the sensitivity axis lands:

| Client | Read | Write |
|---|---|---|
| Claude Code (both installs) | `*` | `*` |
| OpenWebUI | `*` | `*` |
| Claude.ai | `user:me`, `global`, `project:*` | `user:me`, `global`, `project:*` |
| ChatGPT | `user:me`, `global` | `user:me`, `global` |

ChatGPT is narrower on purpose: it is the surface the PRD is most explicit about wanting to keep
away from things. This is a starting position that Phase 3 replaces with real policy, not a final
answer.

**Grant changes take effect on restart.** Grants live in `AUTH_TOKENS`. That is fine for a handful
of clients and becomes annoying at the point where grants change often, which is Phase 3's problem
to solve, not this one's.

---

## 4. Canonical registry keys

The system PRD says this matters from day one and Phase 1 shipped free-form keys. Six writers
without a scheme will produce `desktop.gpu`, `machines.desktop.gpu` and `hardware.desktop.gpu` for
one fact, and the PRD is explicit that preventing that beats cleaning it up. This must land before
the second writer connects.

**Shape.** `kind` stays the coarse category. `key` becomes a dotted path.

```
kind:  host | service | credential-ref | model-route | dataset
key:   <domain>.<entity>.<attribute>
```

```
host           machines.desktop.os          "Ubuntu 26.04"
host           machines.desktop.gpu         "RTX 4090"
service        services.postgres.endpoint   {"host":"127.0.0.1","port":5432}
credential-ref credentials.postgres.location {"where":"1Password","item":"lumberroom"}
model-route    routes.coding.model          "claude-opus-5"
dataset        datasets.tidewallet.location  "s3://..."
```

**Rules**, enforced on write:

- Lowercase `[a-z0-9-]` segments separated by dots, two to four segments.
- The first segment is a closed vocabulary: `machines`, `services`, `credentials`, `routes`,
  `datasets`, `people`, `accounts`. Adding to it is a deliberate act, not a side effect of a model
  guessing.
- Plural domain, singular attribute. `machines.desktop.os`, never `machine.desktop.oss`.
- A rejected write returns the closest valid key rather than just an error, so the caller can
  retry without a round trip through the user.

**Aliases, because rejection alone is not enough.** A model that gets rejected will invent a second
variant, not the canonical one. An alias table turns the mess into a redirect:

```sql
CREATE TABLE registry_alias (
  tenant_id  text NOT NULL,
  namespace  text NOT NULL,
  kind       text NOT NULL,
  alias_key  text NOT NULL,
  canonical  text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, namespace, kind, alias_key)
);
```

`registry_get` resolves aliases transparently and reports which key answered. When a write is
rejected, the suggested key can be recorded as an alias so the same wrong guess resolves next
time. This converts every naming mistake into a one-line redirect instead of a duplicate fact.

**Migration.** Phase 1 rows are free-form. There are few of them and they are yours: read them,
map them by hand, record the old names as aliases. Do this before other surfaces write, and there
is nothing to clean up later.

---

## 4a. Response size, which differs by surface

Ceilings are not uniform and at least one is undocumented:

| Surface | Ceiling |
|---|---|
| Claude.ai, Cowork, mobile | around 150,000 characters, 300 seconds |
| Claude Code | a token cap, `MAX_MCP_OUTPUT_TOKENS`, default 25,000 |
| ChatGPT | **undocumented and unresolved.** Responses are truncated at a line boundary; the reported workaround is to return structured content rather than a large inline text block |

The digest ceiling is 6,000 characters, roughly 1,500 tokens, and a full digest reaches about 6,850
before truncation. That is comfortable for Claude and is the likely failure case on ChatGPT, because
`context_bootstrap` returns its markdown as one text block by design, which is exactly the shape
ChatGPT truncates.

**Give the digest a per-client budget.** The ceiling is already configurable; make it resolve per
client rather than globally, and keep the structured payload authoritative so a client that
truncates the text block still has the data. Test the real payload against ChatGPT before assuming
any number is safe.

## 5. Measurement, and the largest unknown

The PRD names it plainly: connector protocols standardize how a tool talks to the system, not
whether it bothers. This phase measures which surfaces carry their weight.

**What is already recorded.** Every tool call writes a `tool_calls` row with client, tool, success,
latency and `unprompted`. A call arriving without an `X-Memory-Invocation` header counts as
model-initiated; hooks and the CLI always send one. Per-client rates fall straight out of
`lumberroom stats`.

**What to add.** A per-client view with the two numbers that decide the fallback, over a rolling
30 days:

- **Unprompted read rate.** Sessions where the client called `context_bootstrap` or
  `memory_search` without being told to, over total sessions. Sessions are not currently
  identifiable, so this needs a session correlation id, or an approximation by time-bucketing
  calls per client.
- **Unprompted write rate.** The number that matters more. A surface that reads and never writes
  is consuming a store it does not maintain.

**The fallback ladder**, in the order the PRD implies, with a trigger for each:

1. Sharpen the tool descriptions. Always first; costs nothing and applies to every client at once.
2. Per-account instructions phrased as triggers: Claude Projects instructions, ChatGPT Custom
   Instructions. Trigger: a surface reads but writes in under roughly one session in five.
3. A browser extension that writes automatically. Trigger: instructions tried and the write rate
   stayed near zero. This is a real project, and reaching it is a finding, not a failure.

Decide the thresholds before reading the data. Choosing them afterwards is how a disappointing
number becomes an acceptable one.

**The measure with no number behind it.** "You stop repeating yourself" is the PRD's primary
measure and nothing records it. Pick an approach during this phase, because after six
surfaces are live it only gets harder to instrument.

---

## 6. Per-surface acceptance

Each surface is done when it passes its own variant of the Phase 1 harness, not when it appears
connected in a settings screen.

For a surface with hooks, `scripts/done-when-test.sh` applies as written. For a surface without
them, the same shape by hand:

1. From the new surface, state a fact carrying a nonce. Do not tell it to save anything.
2. Check the store for the fact, and check `tool_calls` for `unprompted = true`.
3. From a different surface, ask the question. Assert the answer contains the nonce.
4. Record whether step 1 needed prompting. That result is the data behind §5.

Step 4 is the point of the exercise. A surface that writes only when told is connected but not
carrying the system, and the difference has to be written down at the time rather than
reconstructed later from memory.

**The phase exit test is cross-surface**, exactly as the PRD states it: a fact told to ChatGPT,
recovered in Claude Code the next day. Run it with a nonce, a day apart, without touching the
store in between.

---

## Order of work

1. **Deploy.** Nothing is testable before it. Public HTTPS, IPv4, real certificate.
2. **Second Claude Code install.** No new server capability, so it proves multi-client behaviour,
   per-client instrumentation and cross-client recall in isolation.
3. **Canonical registry keys and the alias table.** Before any new writer connects.
4. **Hermes**, then **OpenWebUI**. Both take a static bearer token today, need no external approval,
   and get the store used by more than one vendor's tooling before OAuth is in the path. OpenWebUI
   also carries the Filter hook, which is the one place forced recall can be tested at all.
5. **The built-in authorization server.** Its own piece of work, not a step inside connecting a
   surface. Decision [0002](../decisions/0002-built-in-oauth-server.md) replaced Logto with it;
   `scripts/oauth-flow-test.sh` against a live server is what proves it, not an email to Anthropic.
6. **Claude.ai web and Cowork**, which share setup. Then **mobile**, which rides along.
7. **ChatGPT**, after the hands-on checks below.
8. **Per-client rate reporting**, then the fallback ladder if the numbers call for it.

## Before building: one check worth more than more research, one retired

**ChatGPT, still worth ten minutes.** Log into a personal Plus or Pro account, Settings, Developer
Mode, and add a custom connector against a throwaway endpoint using a plain `Authorization: Bearer`
header. With OAuth built and available regardless, this no longer decides whether ChatGPT can be
connected at all; it decides whether ChatGPT can be connected with the simpler credential instead of
going through the authorization server. That single attempt still settles the tier question and the
static-header question, meaning whether ChatGPT itself accepts a bearer header, and the
write-capability question at once. Nobody has tried it, in the browser or the mobile app, and that
stays unknown regardless of which credential type ends up in use.

**Retired: the Claude.ai static-header beta check.** Emailing `mcp-review@anthropic.com` is no
longer worth doing. Decision 0002 made lumberroom its own OAuth 2.1 authorization server, so Claude.ai,
Cowork, mobile and ChatGPT connect over OAuth with nothing to request from Anthropic and no external
tenant to configure. The beta was an escape hatch from the cost of running Logto, and the project no
longer pays that cost.
