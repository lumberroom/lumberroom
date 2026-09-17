# Connecting an AGENTS.md client to lumberroom

Anthropic's clients read `CLAUDE.md`. Codex, Hermes, Cursor and VS Code read `AGENTS.md`. The rules
are the same rules, so `client/AGENTS.md.snippet` carries the same two paragraphs as
`client/CLAUDE.md.snippet` between the same `lumberroom:begin` and `lumberroom:end` markers, with
the sentences that named Claude Code rewritten to name tool calls instead.

This guide covers the rules file and the credential behind it.
[`connect-claude-code.md`](connect-claude-code.md) covers the same ground for Claude Code and goes
deeper on grants, the SessionStart hook and the proof scripts; read it for anything this page
treats as settled.

Every path and merge rule below comes from each client's own documentation, read on 17 September
2026. No client on this list has been driven end to end against a live server for this guide, so
treat the wiring as implemented and the acceptance run in section 5 as the thing that settles it.

---

## 1. Where each client reads AGENTS.md

| Client | Global file | Project file |
| --- | --- | --- |
| Codex CLI | `$CODEX_HOME/AGENTS.md`, default `~/.codex/AGENTS.md` | `AGENTS.md` from the git root down to the working directory, at most one per directory |
| Hermes | none documented | the project `AGENTS.md` |
| Cursor | none; global rules live in User Rules | `AGENTS.md` in the project root and any subdirectory |
| VS Code Copilot | none | `AGENTS.md` in the workspace root, behind the `chat.useAgentsMdFile` setting |

Codex reads `AGENTS.override.md` ahead of `AGENTS.md` at each level and takes the first non-empty
file it finds there, then concatenates from the root down, so a file closer to the working
directory lands later in the prompt and wins. An `AGENTS.override.md` in your Codex home replaces
the global file rather than adding to it, which drops the lumberroom block on the floor without
saying so.

OpenWebUI reads no rules file at all. `client/openwebui-filter.py` carries the first two sentences
of the write rule in its `WRITE_RULE` constant and appends them to the system message it injects,
which is the only route the rule has to a model there.

---

## 2. Install the snippet

`client/wire-mac.sh` step 5 writes `client/AGENTS.md.snippet` into `$CODEX_HOME/AGENTS.md`
(default `~/.codex/AGENTS.md`) between the managed markers, the same way step 4 writes the Claude
Code snippet into `~/.claude/CLAUDE.md`. Start with the dry run:

```bash
LUMBERROOM_TOKEN=<token> ./client/wire-mac.sh --url https://memory.example.com --dry-run
```

```
5/5 memory rules -> /Users/you/.codex/AGENTS.md
  would append 28 lines to /Users/you/.codex/AGENTS.md
```

A later run refreshes the block in place and leaves the rest of the file alone, so your own rules
survive and edits between the markers do not. Every file it touches gets a `.lumberroom.bak` copy
next to the original.

The script writes no project-level `AGENTS.md`. It runs from a clone of this repo, so `./AGENTS.md`
would land in that clone and end up in a commit. For a project that needs the rule locally, copy it
in by hand and keep the markers:

```bash
cat /path/to/lumberroom/client/AGENTS.md.snippet >> AGENTS.md
```

Cursor and VS Code need that project-level copy, since neither documents a global `AGENTS.md`. In
VS Code, set `chat.useAgentsMdFile` to true as well or the file sits there unread.

Confirm it landed:

```bash
grep -c 'lumberroom:begin' ~/.codex/AGENTS.md
```

Codex prints the merged instruction chain with `codex --print-instructions` on recent versions,
which answers the question the `grep` only half answers: whether your override file shadowed the
block.

---

## 3. The credential

**Give every client its own token.** Identity comes from the credential and nothing else. A
`tool_calls` row takes its client from the token that made the call, so two clients sharing one
token collapse into a single line in `lumberroom stats --by-client`, and narrowing one narrows
both. `clientInfo.name` is free text the client sends about itself and lumberroom never reads it
for policy, which matters most with Hermes: it identifies itself as `"Claude Code"` in some
configurations ([`../client/hermes-notes.md`](../client/hermes-notes.md)).

Mint one and add an entry. Single-quote the whole value, since several scripts source `.env` with
`sh`:

```bash
openssl rand -hex 32
```

```
AUTH_TOKENS='[{"client":"codex","token":"<the token>","read":["*"],"write":["user:me","project:*"]}]'
```

A bare string carries a ceiling of `open`. Reaching `personal:*` or `credentials:*` takes the
object form and a restart; [`permissions.md`](permissions.md) has the rule and
[`connect-claude-code.md`](connect-claude-code.md) §5 has the refusal message.

Codex takes the endpoint in `~/.codex/config.toml`, reading the token from an environment variable
rather than from the file:

```toml
[mcp_servers.lumberroom]
url = "https://memory.example.com/mcp"
bearer_token_env_var = "LUMBERROOM_TOKEN"
```

Hermes and the other bearer-token surfaces take the same endpoint with a plain header:

```
Authorization: Bearer <the token>
```

Check the grant the server resolves, using that client's token rather than your Claude Code one:

```bash
LUMBERROOM_URL=https://memory.example.com LUMBERROOM_TOKEN=<the token> lumberroom doctor
```

---

## 4. What the snippet asks for

Two paragraphs. **Read** tells the model to call `context_bootstrap` once at the start of a session
unless the preamble already carries a digest, `memory_search` before asking or assuming, and
`registry_get` for a host, an endpoint or the place a credential lives. **Write** tells it to call
`memory_write` after any exchange that settles a decision, preference, constraint or durable fact,
without asking and without announcing it, one fact per call, phrased to stand alone in six months
and carrying the numbers, identifiers, paths and dates the fact turns on. Three bullets name the
namespaces, and a closing line rules out transient chatter, file contents and secrets.

None of these clients has a SessionStart hook. Claude Code pulls the digest through
`client/lumberroom-bootstrap-hook.sh` whether or not the model cooperates; Codex, Hermes and Cursor
recall only when the model decides to call a tool. The Read paragraph is the whole of the ask, and
the rate at which it gets obeyed is the number section 5 watches.

---

## 5. Prove it works

By hand, since `scripts/done-when-test.sh` drives the `claude` binary and has no equivalent for
these clients:

1. From the client, state a fact carrying a nonce. Say nothing about saving it.
2. `lumberroom search "<nonce>"`. The row should be there, attributed to that client.
3. `lumberroom stats --by-client`. The write should count as unprompted: the CLI and the hook send
   `X-Memory-Invocation`, so a call without that header is the model deciding for itself.
4. From a different surface, ask the question the fact answers. The answer should carry the nonce.
5. Write down whether step 1 needed prompting. That is the data point, and it exists only if
   someone records it at the time.

---

## 6. Troubleshooting

**The model never writes.** `lumberroom stats --hours 168`, then
`grep -c 'lumberroom:begin' ~/.codex/AGENTS.md`. An `unprompted` count stuck at zero with the
markers present points at the merge: an `AGENTS.override.md` in the Codex home replaces the file
holding the block, and a project `AGENTS.md` lands later in the prompt than the global one.

**The block is in the file and the client never sees it.** VS Code ignores `AGENTS.md` until
`chat.useAgentsMdFile` is true. Cursor reads the project file and not a global one. Codex reads the
git root down to the working directory, so a file above the git root is out of the chain.

**Two clients report as one in `lumberroom stats --by-client`.** They share a token. Mint a second
one and give it its own `AUTH_TOKENS` entry.

**403 on every MCP call while `/healthz` answers.** The `Host` allowlist, derived from
`PUBLIC_URL`. [`connect-claude-code.md`](connect-claude-code.md) §6 has the whole entry.

**A write refused with "only up to open".** The grant's ceiling rather than the namespace. Widen
the entry to the object form or write somewhere that classifies `open`.
