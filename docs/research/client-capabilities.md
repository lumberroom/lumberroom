# Research: what each surface can actually do

Status: this research fed the Phase 2 spec (implemented 19 August 2026, verification pending). Its
conclusion that an authorization server is required stands; its conclusion that the server should
be Logto does not. Decision [`0002`](../decisions/0002-built-in-oauth-server.md) supersedes §A and
§D's Logto recommendation with a built-in authorization server, for reasons this document never
examined. See the note there.

Researched 19 August 2026, for the Phase 2 spec. Confidence is marked throughout: **[doc]** means a
primary source was fetched directly, **[community]** means forum or GitHub reports corroborate but
the vendor has not confirmed, **[unverified]** means it could not be established and is flagged
rather than guessed.

**Both unknown surfaces are now identified.** Hermes is Nous Research's Hermes Agent, an open-source
agent CLI launched February 2026 that configures remote MCP servers from a config file. Cowork is
Anthropic's Cowork, which runs on the same connector infrastructure as Claude.ai. Note there is also
a Microsoft Copilot Cowork, built on Anthropic's execution technology and shipped inside Microsoft
365; this report assumes Anthropic's is the one meant.

---

## The matrix

| Surface | Remote MCP, and gating | Static bearer header | OAuth needed in practice | Client identity | Auto-recall hook | Confidence |
|---|---|---|---|---|---|---|
| **Claude Code** (either machine) | Yes, proven in Phase 1 | **Yes, today**, `claude mcp add --header` | No | **Yes**, distinct CIMD client_id, `client_name: "Claude Code"` | SessionStart hook | high |
| **Claude.ai web** | Yes. Free capped at one connector, Pro and above unrestricted | `static_headers` exists but is **beta and invite-gated** | **Yes**, until beta access is granted | No, shares infrastructure and callback with mobile, Desktop and Cowork | None. Pure model tool choice | high |
| **Claude.ai mobile** | Yes, account-synced connector list. Adding new ones from mobile is limited | As web | As web | Indistinguishable from web | None | high on sync, medium on the add flow |
| **Cowork** (Anthropic) | Yes, same infrastructure as Claude.ai | As web | As web | Indistinguishable from Claude.ai | Scheduled sessions exist, but tool choice is still the model's | high on identity |
| **ChatGPT web** | Yes, via Developer Mode. Plus and above; Free excluded | Reported, **not primary-confirmed** | Treat as required until tested | [unverified] | [unverified] | **low to medium** |
| **ChatGPT mobile** | [unverified] whether custom connectors exist there at all | [unverified] | [unverified] | [unverified] | [unverified] | low |
| **OpenWebUI** | Yes, **native** MCP client since v0.6.31, Streamable HTTP. Self-hosted, no gating | **Yes, native, no gating** | No | Weak, sends the generic SDK default `clientInfo` and no distinguishing User-Agent | **The only real one anywhere.** A Filter `inlet` runs on every incoming message, outside model tool choice | high |
| **Hermes** (Nous Research) | Yes, open-source CLI, `mcp_servers:` config, no gating | **Yes, native, today** | No | Weak. Registers as `"Hermes Agent"`; uses `"Claude Code"` only for Figma's MCP server (hermes-agent `tools/mcp_oauth.py:1089-1131` at `fdec926e`, corrected 25 Sep 2026; the earlier cell said it sometimes claims to be Claude Code) | None for an MCP mount. A memory-provider plugin gets `prefetch` per turn (`agent/memory_provider.py`) | high |

---

## A. Which surfaces force a real OAuth server

**Static bearer works today:** Claude Code, Hermes, OpenWebUI. All three are self-configured tools
that need it least.

**Supported by the product but not reliably available to us:** the Claude.ai family. Anthropic's own
auth-type table includes `static_headers`, but it is beta, invite-gated behind an email to
`mcp-review@anthropic.com`, and GitHub issues from July and August 2026 show users seeing OAuth-only
fields with no header option.

**Assume OAuth-only:** Claude.ai web, mobile, Desktop and Cowork unless beta access is granted, and
ChatGPT until tested by hand.

**Conclusion: Logto moves from optional to the Phase 2 baseline.** [Superseded: decision 0002 keeps
this section's finding that an authorization server is required and replaces "Logto" with one built
into lumberroom. The Claude.ai family and probably ChatGPT forced the conclusion, and that finding stands
untouched.] It is the only path that covers the Claude.ai family and probably ChatGPT regardless of
beta timing. Bearer mode stays as the fast path for the three surfaces that already work with it.

Server-side requirements hold regardless of which auth type a given surface ends up using, because
MCP requires RFC 9728 discovery unconditionally: publish the protected-resource metadata, always
answer with `401` and a `WWW-Authenticate` pointer rather than a `200` carrying an error body,
mandate PKCE S256, and prefer CIMD or manually issued credentials over Dynamic Client Registration.

## B. Telling the clients apart

Ranked by how much weight the answer can bear:

1. **A distinct OAuth client_id per surface, manually registered or via CIMD.** The only mechanism
   that is a real boundary. Anthropic already does this for Claude Code. For everything else we
   control it: Claude's connector setup accepts a client_id and an optional secret you supply, so
   hand out a different one per surface at setup time and key the grant off it. This is Anthropic's
   own recommendation over DCR, precisely because DCR mints a meaningless fresh client every time.
2. **A distinct static bearer token per surface**, wherever bearer auth is available. We mint it, so
   we own the mapping completely.
3. **Not usable for policy: `clientInfo` from the initialize handshake.** Free text, self-declared,
   no registry. Three different values have been reported for Claude.ai alone, and Hermes sends
   `"Claude Code"` to Figma's MCP server to pass its registration allowlist. Log it, never
   authorize on it.
4. **Weak: User-Agent and egress IP.** Anthropic's tool-call traffic comes from `Claude-User` on a
   fixed CIDR, which confirms "this is Anthropic's cloud" but not which product. OpenWebUI sends
   nothing distinguishing.
5. **Structurally impossible today:** Claude.ai web, mobile, Desktop and Cowork are one client from
   the server's point of view. Same infrastructure, same OAuth callback, no per-surface signal. If
   Cowork's autonomous sessions ever need a different policy from interactive chat, the only lever
   is adding the connector twice under two different manually issued credentials.

## C. Automatic recall

**No surface has a documented protocol-level hook that fires a tool at conversation start.** Every
one funnels tool calls through the model's own choice.

**One exception: OpenWebUI's Filters.** An admin-authored `inlet` function runs on every incoming
message, outside model tool choice, and can be made to call the memory server before the model sees
the message. It is the only genuinely non-model-mediated recall mechanism across all eight surfaces.
It requires writing custom code rather than flipping a toggle.

Everywhere else, recall reduces to two probabilistic levers: how compelling the tool descriptions
are, and the per-account instruction field. That is precisely the unknown the system PRD names as
central. On every surface except OpenWebUI, the read and write rate is a function of model
compliance, and no amount of architecture can force it.

## D. Build order

1. **Claude Code, second machine.** No new server capability. Optionally register once as a
   claude.ai custom connector so it syncs to every machine, but only if the login is a claude.ai
   subscription rather than an API key, Bedrock or Vertex, which do not get the sync.
2. **Hermes.** Native bearer, no gate, self-configured. Confirm Streamable HTTP specifically; the
   docs only say HTTP.
3. **OpenWebUI.** Native bearer, no external approval, and the only forceable recall hook, which
   makes it the highest-value surface against the write-rate unknown. Watch open issue `#14035`,
   where three or more concurrently enabled MCP tools may stop firing.
4. **Claude.ai web and Cowork together**, same infrastructure and same setup work. Build the OAuth
   path with a manual client_id rather than betting on bearer beta timing.
5. **Claude.ai mobile**, which rides along free once step 4 ships.
6. **ChatGPT web.** Most unknowns of any surface. Confirm by hand before designing: whether Free is
   excluded, whether a static header is available on a personal account, and whether write-capable
   tools are restricted to Business and Enterprise workspaces as community reports suggest.
7. **ChatGPT mobile.** Unresearched until someone checks the app.

## E. Gotchas that would cost a day

**The ones that touch our server directly:**

- **`401` plus `WWW-Authenticate` is absolute for Claude.** A `200` carrying an error body is
  silently ignored and Claude.ai fails before showing a login screen. **Claude Code's fallback
  probing masks this**, so validating against Claude Code alone gives a false pass for web and
  mobile.
- **Serve the protected-resource metadata at both locations.** RFC 9728 inserts the path before the
  suffix, but real clients also check the domain root.
- **Anthropic's fixed egress CIDR hits the authorization server too**, not just the MCP endpoint. An
  allowlist covering only `/mcp` leaves discovery and token calls failing silently.
- **IPv4 and public routability are required.** AAAA-only, CGNAT, private and loopback hosts are
  rejected by every hosted Claude surface. Localhost works for nothing but Claude Code.
- **Latency budget: 10 seconds** for discovery, registration and token calls, 30 for refresh. A cold
  start or a slow proxy fails intermittently with no useful client-side error.
- **Result size ceilings differ by surface.** Claude.ai and Cowork cap around 150,000 characters and
  300 seconds; Claude Code uses a token cap, `MAX_MCP_OUTPUT_TOKENS`, defaulting to 25,000. ChatGPT
  has an undocumented and currently unresolved truncation, where the workaround is to return
  structured content rather than a large inline text block. **Our digest ceiling is 6,000
  characters, which is safe for Claude and the likely risk case for ChatGPT.**

**The ones that touch Logto:**

- PKCE S256 must be advertised in `code_challenge_methods_supported`; newer clients refuse to
  proceed without it.
- `/token` must accept `application/x-www-form-urlencoded` while `/register` uses JSON. A stack
  wired only for JSON returns 415 on token exchange while registration works, which is a confusing
  almost-working failure.
- DCR mints a fresh client on every connection for both Claude and ChatGPT. Fine at this scale, but
  it accumulates phantom registrations. Prefer CIMD or manual credentials from the start.

**The ones that touch the client side:**

- **Claude Code stores `--header` tokens in plaintext** in `.mcp.json` or `~/.claude.json`; only
  OAuth-flow tokens reach the keychain. A project-scoped `.mcp.json` committed to git is a real leak.
  Use variable expansion or a headers helper. Our wiring script writes user scope, not project scope,
  which avoids this.
- **No client anywhere sends a stable per-install identifier.** Per-device policy, if it is ever
  wanted, has to be minted at setup time. It cannot be read off the wire.

---

## What would settle the remaining unknowns

Two hands-on tests, neither longer than ten minutes, both worth more than further searching:

1. **ChatGPT.** Log into a personal Plus or Pro account, Settings, Developer Mode, add a custom
   connector against a throwaway endpoint with a plain `Authorization: Bearer` header. That single
   attempt resolves the tier question, the static-header question and the write-capability question
   at once. Repeat in the mobile app to resolve that row.
2. **Claude.ai static headers.** Email `mcp-review@anthropic.com` for beta access now, since the
   answer determines whether Logto is required before Claude.ai can be connected at all, and the
   request costs nothing to make early.
