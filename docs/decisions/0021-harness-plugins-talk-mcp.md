# 21. Harness memory plugins are clients of the existing /mcp endpoint

**Date:** 25 September 2026 · **Status:** accepted; the Hermes plugin is built at
[`github.com/lumberroom/lumberroom-hermes`](https://github.com/lumberroom/lumberroom-hermes) · **Decided by:** the owner

## Decision

A plugin that makes lumberroom the memory of an agent harness, starting with Hermes Agent's
memory provider, talks to the engine through `/mcp` with the official MCP SDK
for its language. It uses the tools every other client uses, `context_bootstrap` and
`memory_search` for recall and `memory_write` for writes, and it proxies their names, descriptions
and schemas from the live `tools/list`. The engine gains no REST surface for plugins, and v1 needs no
Rust change.

The plugin tells the engine who is calling with the two headers the transport already reads:
`x-memory-invocation: hook` on calls the harness makes on its own, nothing on calls the model makes,
and `x-session-id` on both (`src/http/mod.rs:43-57`, `src/domain/types.rs:287-310`). One import
command uses the ingest routes under `/admin/ingest`, which exist for the CLI and need `mayIngest`.

Each plugin lives in its own repository in the lumberroom organization, starting with
`lumberroom/lumberroom-hermes`, and releases on its own cycle. The engine keeps this record, the
research behind it and the no-code fallback notes. The owner ruled this on 25 September 2026, after
the plugin was first built at `client/hermes/` here: a plugin release should not wait on an engine
release, and an engine tag should not carry a plugin version. The plugin repository kept its history.

## The context that forced it

Hermes Agent gives an external memory provider a `prefetch` hook on every turn and a slot that turns
off its own `MEMORY.md` store (`hermes-agent/agent/memory_provider.py:84-206`,
`agent/memory_manager.py:379-501`). An MCP server mounted in Hermes reaches neither: MCP tools have no
link to the memory manager, so recall stays the model's choice. To force recall the owner needs a
plugin, and a plugin needs a way to talk to the engine.

The engine has one model-facing surface. `src/mcp/mod.rs` holds the tools, and their descriptions
carry the write rule: one fact per call, self-contained, with its numbers and dates, and
`possible_conflicts` read back for supersession. That text is the main lever on what a model writes.
`/admin/recall` looks like a search route and measures retrieval quality instead
(`src/http/mod.rs:99`). No REST search, write or bootstrap exists.

## What lost, and why

**A new REST surface for plugins** (`GET /v1/recall`, `POST /v1/memories`). Plain JSON would spare
the plugin an MCP client and a loop thread. It loses on four counts:

- It is a second model-facing contract. The write rule, the capability gating in
  `src/mcp/capability.rs` and every argument description would exist twice, and nothing would keep
  the copies in step.
- Every plugin release would wait on an engine release, and every hosted edge would need new
  locations.
- OAuth still needs a library on the client, so the MCP-adjacent auth stack comes back anyway.
- `/admin` is already an ABI the open-source CLI depends on, additive-only. A third surface with the
  same obligation triples what an engine change has to keep stable.

**An MCP mount plus a rules snippet, no plugin.** Zero code, and it works today against a
hand-written `mcp_servers` entry. Recall stays unforced, Hermes's built-in store keeps writing beside
lumberroom unless the user switches it off by hand, and every call carries the same invocation header,
so the stats cannot separate forced recall from the model's choice. It stays documented as the
no-code fallback in `client/hermes-notes.md`.

**A general hook plugin beside an MCP mount.** Hermes's `pre_llm_call` hook can inject context
without claiming the memory slot. The slot then stays open, so the built-in store keeps writing, and
the user configures one server twice with two credentials.

## Costs accepted

- **An MCP client inside each plugin.** Python needs a loop thread and two sessions per plugin
  instance; TypeScript will need the same shape. The Hermes plugin's first spike settled the
  handshake: the Python SDK 2.0.0 completes a plain `initialize` against the engine, which answers
  protocol version 2025-11-25.
- **The contract crosses repositories.** A plugin in its own repository learns about an engine change
  only when its gate runs. The Hermes gate takes `--engine-src` and starts scratch engines from that
  checkout, and `--capture` regenerates the plugin's tool snapshot from it, so the check exists; it
  runs when someone runs it, not on every engine PR.
- **OAuth hardening lives in each plugin.** The Hermes plugin uses the public SDK
  `OAuthClientProvider` with its own token file and a cross-process lock, as the owner ruled. The
  SDK's 2.0.0 provider forgets a token's expiry across restarts and guesses the token endpoint when
  it holds no metadata, and nothing in it stops two processes refreshing one token, which the engine
  answers by revoking the whole family (`src/authserver/routes.rs:779-790`). The plugin closes those
  gaps itself with two public `OAuthContext` fields and a file lock
  (the plugin repository's `docs/spec.md` section 6.2). Hermes's internal OAuth manager already carries those
  fixes; the plugin gives them up to stay off a Hermes-internal API.
- **Tool-list drift is caught late.** The plugin ships a snapshot of `tools/list` for the moment
  before it connects, because Hermes builds its tool routing table before `initialize`. A renamed
  engine tool breaks routing until the snapshot is regenerated. Tools extend and never rename
  (`src/mcp/mod.rs:9-11`), which keeps the cost theoretical so far.
- **Recall replays.** Hermes stamps each prefetch into that turn's user message and resends it until
  compression. The plugin bounds it with caps and id dedupe and does not remove it.

## What this is not for

It is not a public API. Nothing here promises third parties a stable plugin protocol beyond the MCP
tools themselves.

It does not extend to capture. v1 plugins write nothing on their own. If a plugin later proposes
facts extracted from turns, it posts to the ingest queue under its own record, and that record
decides the grant it needs.

It does not make the plugin the enforcement point. The engine checks every grant on every call; the
plugin's owner gate and tool allowlist shape what a model sees and never replace the server's check.

## Reversal condition

Two findings would reopen this:

- a harness whose plugin runtime cannot hold an MCP client, such as a sandbox with no outbound
  streaming HTTP or no async runtime a plugin may start. That harness gets a narrow JSON route
  generated from the same tool definitions in `src/mcp/`, never hand-written beside them.
- measured recall latency through `/mcp` that breaks a harness's hook budget where a plain route
  would not. Hermes allows 8.0s per prefetch (`hermes-agent/agent/memory_manager.py:32`) and the
  plugin targets 3.0s; the figure that settles it comes from the integration script, not from here.
