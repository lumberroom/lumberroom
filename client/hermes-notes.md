# Wiring Hermes Agent to lumberroom

Hermes is Nous Research's open-source agent CLI. It speaks remote MCP natively and takes a bearer
token today, which puts it in the same easy tier as Claude Code and OpenWebUI (`docs/specs/phase-2-surfaces.md`
§1): no OAuth, no Logto, no external approval needed before it can read and write the same store
as every other surface.

## The one thing that matters before anything else

**Never grant on what Hermes says about itself.** An earlier version of this note said Hermes
sends `"Claude Code"` as its client name in some configurations. Hermes source at `fdec926e`
(25 September 2026) says otherwise: its OAuth registration defaults `client_name` to
`"Hermes Agent"` and switches to `"Claude Code"` only for Figma's remote MCP server, whose
registration allowlist demands it (`tools/mcp_oauth.py:1089-1131`). The rule stands anyway.
`clientInfo` and `client_name` are free text a client sends about itself, unverified, and lumberroom
never uses them for policy (`docs/specs/phase-2-surfaces.md` §3).

**Identity comes from the credential and only the credential.** Issue Hermes its own token in
`AUTH_TOKENS`, distinct from every other client's, and let the server key the grant off that. This
is the one mechanism the spec calls an actual boundary: everything else (`clientInfo`, User-Agent,
egress IP) is logged for the per-client rate numbers in §5 and never checked for authorization.

## Setup

1. Issue a token, distinct from every other client's:

   ```bash
   openssl rand -hex 32
   ```

2. Add it to the server's `AUTH_TOKENS`, with a grant appropriate for what you want Hermes to see.
   Phase 2's starting position is coarse (`docs/specs/phase-2-surfaces.md` §3 table); tighten it
   once the sensitivity axis lands in Phase 3:

   ```
   AUTH_TOKENS=[{"client":"hermes","token":"<the token above>","read":["*"],"write":["*"]}]
   ```

   If you want Hermes narrower than that from day one because it is a third-party agent rather
   than your own tooling, scope it the way the spec scopes ChatGPT instead:

   ```
   AUTH_TOKENS=[{"client":"hermes","token":"<...>","read":["user:me","global"],"write":["user:me"]}]
   ```

3. Point Hermes at the MCP endpoint with that token as a plain bearer header. Hermes's own docs
   cover the exact config surface for adding a remote MCP server; the header shape lumberroom expects is
   the same one every other bearer-token surface uses:

   ```
   Authorization: Bearer <the token above>
   ```

   Endpoint: `https://<your-domain>/mcp`.

4. Verify with the CLI, using Hermes's own token rather than your Claude Code one, so you are
   testing the grant Hermes actually has and not a different client's:

   ```bash
   LUMBERROOM_URL=https://<your-domain> LUMBERROOM_TOKEN=<hermes token> lumberroom doctor
   ```

## Recall with a plain MCP mount, and the hook that forces it

Hermes has a per-turn recall hook, and an MCP mount does not reach it. A memory-provider plugin
selected with `memory.provider` gets `prefetch` before every non-trivial turn, and Hermes appends
what it returns to the user message inside a `<memory-context>` block with an 8.0s bound
(`agent/memory_provider.py:84-206`, `agent/memory_manager.py:32,317-333,457-501` at `fdec926e`).
MCP tools have no link to that manager, so with the setup on this page recall depends on Hermes
choosing to call `context_bootstrap` or `memory_search` on its own, and the Phase 2 measurement in
§5 applies to it as it does to ChatGPT.

The lumberroom provider plugin at
[`github.com/lumberroom/lumberroom-hermes`](https://github.com/lumberroom/lumberroom-hermes) uses that
hook, and it is the setup to reach for: recall on every turn, the engine's own tools for writes, and
Hermes's built-in store switched off. This page stays as the no-code fallback, an MCP mount plus the
write rule, for a Hermes install that cannot take the plugin.

## Tell it how to remember

A token gets Hermes to the store. Nothing in a token tells it to write. Hermes reads `AGENTS.md`,
the rules file every non-Anthropic agent framework has settled on, so the write rule lives in
[`AGENTS.md.snippet`](AGENTS.md.snippet) rather than in the Claude Code snippet next to it. Copy
that file into the `AGENTS.md` Hermes reads for the project, keeping the `lumberroom:begin` and
`lumberroom:end` markers so `wire-mac.sh` can refresh the block later. `wire-mac.sh` step 5 already
installs it into `$CODEX_HOME/AGENTS.md` (default `~/.codex/AGENTS.md`), which reaches Codex and
anything else reading a global `AGENTS.md`, and not Hermes.

The paragraph that does the work, verbatim from the snippet:

```
**Write.** After any exchange that establishes a decision, a preference, a constraint, or a
durable fact, call `memory_write`. Without asking. Without announcing it. One fact per call,
phrased so it stands alone in six months, carrying the numbers, identifiers, paths and dates the
fact needs, and the cause, scope qualifier and reversal condition whenever the fact turns on them.
Cut the trail of how you came to believe it: the search you ran, the file you read on the way, the
argument for the claim. No hedges, no evaluative words, no restated context, no inventory of what
you left unchanged. A list or a timeline runs long and that is right; prose about a short fact
runs long and that is bloat. Two facts from one exchange are two calls.
```

The Read paragraph above it covers the other half: `memory_search` before assuming, `registry_get`
for a host or an endpoint, `context_bootstrap` once at the start of a session. With a plain MCP
mount nothing forces the bootstrap call, so that sentence is the only thing asking for it. The
unprompted-write number in step 2 of Acceptance is what tells you the snippet landed.

## Acceptance

Run the per-surface harness from `docs/specs/phase-2-surfaces.md` §6 by hand, since Hermes has no
`scripts/done-when-test.sh` equivalent:

1. From Hermes, state a fact carrying a nonce. Do not tell it to save anything.
2. Check the store (`lumberroom search "<nonce>"`) and `lumberroom stats --by-client` for a write attributed to
   `hermes` with `unprompted = true`.
3. From a different surface, ask the question. Assert the answer contains the nonce.
4. Record whether step 1 needed prompting. That is the data point Phase 2 §5 wants, and it only
   exists if it is written down at the time.
