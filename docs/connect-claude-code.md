# Connecting Claude Code to lumberroom

Wiring Claude Code on a Mac to a lumberroom deployment: one credential, one MCP registration, one hook,
one write rule, then the commands that prove the loop. Run everything below from a clone of this
repo. The deployment itself is [`DEPLOY.md`](../DEPLOY.md); this picks up once the server answers.

## Which credential

**Take a static bearer token.** Claude Code accepts an `Authorization` header natively, the Mac is
yours, and OAuth adds a browser round trip that buys nothing on your own machine. `wire-mac.sh`
defaults to `--token-mode` for that reason.

**Take OAuth when you want one credential model across every surface**, or when you want to revoke
this client without editing `AUTH_TOKENS` and restarting the server. An OAuth client's grant is a
row in `oauth_client` that the consent screen writes and rewrites; a static token's grant sits in
`.env` and every change costs a restart
([decision 0003](decisions/0003-grants-in-the-database.md)).

Both work against the same deployment. Static tokens are honoured in every mode whenever
`AUTH_TOKENS` is set. `AUTH_MODE` decides what the server accepts *on top of* them: `oauth` adds the
built-in authorization server ([decision 0002](decisions/0002-built-in-oauth-server.md)), `oidc`
adds an external issuer's JWTs.

---

## 1. Register the server with Claude Code

`client/wire-mac.sh` does the whole job. Start with the dry run, which prints every change and
touches nothing:

```bash
LUMBERROOM_TOKEN=<token> ./client/wire-mac.sh --url https://memory.example.com --dry-run
```

Drop `--dry-run` to apply. Every file it edits gets a backup next to the original with a
`.lumberroom.bak` suffix, and each of its four steps is idempotent:

```bash
LUMBERROOM_TOKEN=<token> ./client/wire-mac.sh --url https://memory.example.com
```

There is no `--token` flag: a value on the command line sits in `ps` output and in shell history for
as long as either persists. Leave `LUMBERROOM_TOKEN` unset in a terminal and `wire-mac.sh` prompts for
the token with echo off instead.

1. writes `~/.config/lumberroom/config.json` at mode 600, carrying the endpoint and the token
2. installs `lumberroom` to `~/.local/bin/lumberroom` and `client/lumberroom-bootstrap-hook.sh` to
   `~/.claude/hooks/lumberroom-bootstrap.sh`
3. registers the MCP server with Claude Code and adds the SessionStart hook to
   `~/.claude/settings.json`
4. writes `client/CLAUDE.md.snippet` into `~/.claude/CLAUDE.md` between `lumberroom:begin` and `lumberroom:end`
   markers

It needs `jq` and `node` on PATH, plus `curl` in oauth mode, and it appends `/mcp` to `--url` if you
left it off. In token mode it finishes by running `lumberroom doctor` against the endpoint and fails the
whole run if the server does not answer. Other flags:
`--name` renames the MCP entry (default `lumberroom`), `--scope` picks the Claude Code config scope
(default `user`), `--oauth-mode` takes no token at all.

The registration step by hand, which is what step 3 runs:

```bash
claude mcp add --transport http lumberroom https://memory.example.com/mcp \
  --scope user --header "Authorization: Bearer <token>"
```

In oauth mode there is no header. Claude Code meets the 401 plus `WWW-Authenticate` on first
connect and runs its own client registration and consent against the server's discovery metadata:

```bash
claude mcp add --transport http lumberroom https://memory.example.com/mcp --scope user
```

`claude mcp get lumberroom` prints the current entry and `claude mcp remove lumberroom --scope user` drops it.
The script runs both when an entry already exists, so a re-run replaces rather than duplicates.

Start a new session and run `/mcp`. The `lumberroom` server should read as connected.

### If you chose OAuth

Claude Code and the `lumberroom` CLI get separate credentials. Claude Code negotiates its own on first
connect; the CLI and the hook need `lumberroom login`:

```bash
LUMBERROOM_URL=https://memory.example.com/mcp ~/.local/bin/lumberroom login
```

It registers a client named `lumberroom`, opens the consent screen in your browser, and catches the
redirect on `http://127.0.0.1:8976/callback`. That port is persisted in `config.json` alongside the
`client_id` and rebound on every later login, because migration 007 compares `redirect_uri` exactly
and never by prefix; a login on an ephemeral port works once and fails after. `--reregister --port
<n>` starts over on a different port. If `config.json` still holds a static `token`, login prints a
note and that token keeps winning until you remove it. Until `lumberroom login` has run once, the
SessionStart hook fails open and adds no digest.

This subsection comes from the code rather than from a run: no OAuth flow has completed against a
live server, and `scripts/oauth-flow-test.sh` is the gate that would settle it. The one edge observed
today is what login says when the server is not in oauth mode:

```
$ lumberroom login
lumberroom: server has no /oauth/register: it is not running in oauth or oidc mode
```

---

## 2. The SessionStart hook

`client/lumberroom-bootstrap-hook.sh` pulls the digest at the start of every session and injects it into
the preamble. This is what makes recall happen whether or not the model decides to call a tool.
Without it, a session begins knowing nothing and `context_bootstrap` is one more tool the model may
skip.

wire-mac.sh installs the script to `~/.claude/hooks/lumberroom-bootstrap.sh` and appends this to
`~/.claude/settings.json` through `jq`, skipping the edit when the command is already in the array,
so hooks you already have survive:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/Users/you/.claude/hooks/lumberroom-bootstrap.sh",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
```

**What it costs.** Phase 1 measured `context_bootstrap` server-side at p50 4 ms and p95 30 ms, read
from the `tool_calls` table on a store holding tens of rows, every one of them `open`, with the
digest served from a 30 second cache. End to end through the CLI, including Node startup, a
bootstrap took about 160 ms. Nobody has re-measured either since Phase 1, and no measurement exists
at a store size worth the name. Two rules bound the downside: the script caps the call at
`LUMBERROOM_HOOK_TIMEOUT` seconds (default 8), and any failure exits 0 with no output, so a dead server
costs you the timeout and never blocks a session.

Run it by hand before trusting it:

```bash
LUMBERROOM_BIN=~/.local/bin/lumberroom ~/.claude/hooks/lumberroom-bootstrap.sh
```

A working hook prints one line of JSON beginning `{"hookSpecificOutput"`. Anything malformed and it
prints nothing, since a partial line would corrupt the session preamble.

---

## 3. The write rule

**This is the highest-leverage part of the whole setup.** The tool descriptions in
`src/mcp/mod.rs` and `client/CLAUDE.md.snippet` are the only two levers on whether a model uses the
store at all. Everything else here moves bytes; these two decide whether there are any bytes to
move. A server with good recall and no write rule collects nothing, and you find out weeks later
when the digest is still empty.

Read `client/CLAUDE.md.snippet` before you install it. It tells the model to call `memory_write`
after any exchange that settles a decision, preference, constraint or durable fact, without asking
and without announcing it, one fact per call, phrased to stand alone in six months, carrying the
numbers, identifiers, paths and dates the fact needs, and the cause, scope qualifier and reversal
condition whenever the fact turns on them. It tells the model to cut the trail of how it came to
believe the fact, the hedges and the list of what it left unchanged, and that a list or a timeline
runs long and that is right. It names the three namespaces (`user:me`, `project:<slug>`, `global`) and lists what to leave out: transient
chatter, file contents, secrets. Step 4 of wire-mac.sh appends it to `~/.claude/CLAUDE.md` between
managed markers and refreshes the block in place on later runs, so your own edits to the rest of
that file survive and edits inside the markers do not.

Confirm it landed:

```bash
grep -c 'lumberroom:begin' ~/.claude/CLAUDE.md
```

Whether the rule works is a number rather than an opinion. `lumberroom stats --hours 168` reports
`unprompted` per tool, counting calls that arrived without an `X-Memory-Invocation` header. The CLI
and the hook always send one, so `unprompted` is the model deciding for itself.

---

## 4. Prove it works

In this order. Each step assumes the one above it passed.

### lumberroom doctor

```bash
lumberroom doctor
```

Against the stack on `127.0.0.1:8787` in token mode, 19 August 2026:

```
endpoint: http://127.0.0.1:8787/mcp
healthz:  200 {"name":"lumberroom","ok":true,"version":"0.4.0"}
readyz:   200 {"auth_mode":"token","db_ms":2,"embedder":"Xenova/bge-base-en-v1.5@q8","embedder_degraded":false,"embedding_dim":768,"kek_provider":"none","kek_verified":false,"ok":true}
credential: static token
whoami:   200 {"client":"claude-code-mac","embedder":"Xenova/bge-base-en-v1.5@q8","may_delete":false,"mode":"token","read":[{"max":"open","namespace":"*"}],"registry_write":true,"scopes":[],"sealed_capable":false,"tenant":"me","token_fingerprint":"aee1bc266593","write":[{"max":"open","namespace":"*"}]}
server auth mode:     token
credential auth mode: token
tools:    context_bootstrap, memory_search, memory_write, registry_get
all checks passed
```

A pass ends with `all checks passed` and exits 0. Read four lines of it before moving on: `readyz`
must say `embedder_degraded: false` or every write lands as a hash vector that retrieves badly;
`credential` must name the credential you meant; `whoami` is the grant the server will enforce; and
`tools` is what your credential can see, which section 5 explains. This transcript predates
`alias_list`: a bare grant today lists five tools, not four, and section 5 has the current count.

### lumberroom bootstrap

```bash
lumberroom bootstrap
```

The same digest the hook injects, as markdown. From the same run:

```
## Memory digest
Store: 3 memories, 1 registry entries across user:me, global, project:memoryengine.

### About the user and standing preferences
- The lumberroom server is written in Rust, with rmcp for MCP and sqlx for compile-checked SQL [rust, decision] _(global, 2026-08-19, via claude-code-mac)_
- Lumberroom measures its own recall by comparing indexed search against an exact scan [recall, verification] _(global, 2026-08-19, via claude-code-mac)_

### Recently learned
- The internal nickname for the lumberroom project is QUARTZLARK-8297b522; use it in commit messages and status notes for lumberroom. [naming, convention] _(project:lumberroom, 2026-08-19, via claude-code-mac)_

### Registry
- host/mcp-endpoint: "https://lumberroom.example.com/mcp" _(global)_
```

`QUARTZLARK-8297b522` is a per-run nonce that `scripts/done-when-test.sh` generates and plants, so
your own digest carries a different one. The `via` at the end of each bullet names the client that
wrote the row, which a body cannot forge; a row that carries line breaks or a heading of its own is
flattened onto one bullet before it is rendered. One more line sits under the `Store:` line naming the
active project namespace and telling the model to pass it to `memory_search`. A pass here means facts and a count. Section 6 covers a digest that
comes back with a count and no facts.

### scripts/done-when-test.sh

The real proof, because it drives the `claude` binary rather than the CLI. Session A states a fact
and the model has to write it without being asked. Session B is a fresh session that never mentions
the fact and has to recover it through the SessionStart hook. Both sessions get the MCP config and
the hook per invocation, so your `~/.claude` stays untouched.

```bash
LUMBERROOM_URL=https://memory.example.com LUMBERROOM_TOKEN=<token> ./scripts/done-when-test.sh
```

The lines it prints on a pass, taken from the script rather than from a run (this guide did not
execute it, since every run writes another nickname row):

```
2/4 did the fact land?
  PASS  the fact is in the store and retrievable
  PASS  the write was unprompted (memory_write unprompted count <before> -> <after>)
4/4 verdict
  PASS  a fresh session recovered the fact without being told it

  done-when test PASSED
```

It ends by printing `lumberroom stats --hours 1`. Two things to read rather than skim. `WARN the write was
recorded as prompted` means the fact landed and the call carried an invocation header, so it did not
come from the model choosing; the loop works and the write rule has not been proved. And the verdict
greps for a nonce, so a session B that quotes the string while refusing to trust it still scores
PASS. The script prints B's transcript above the green line so you read it before believing it.

Each run leaves one nickname fact behind, since Phase 1 records `supersedes` without acting on it.
`--cleanup` drops the earlier ones when a local database is reachable. One survivor from an earlier
run is visible in the digest above.

The three later scripts take the same shape and belong to their own phases:
`scripts/oauth-flow-test.sh`, `scripts/policy-test.sh`, `scripts/correction-test.sh`.

---

## 5. What the model can and cannot do

Driven by the grant. This is the part a reader gets wrong, so both halves below are observable from
`lumberroom doctor` rather than taken on faith.

### A bare `"*"` means a ceiling of open

A grant entry carries two axes: a namespace glob and a sensitivity ceiling. Written as a bare
string, the ceiling is `open`, which is what kept every Phase 1 grant valid on the day the axis
landed. The live token on this stack is written that way:

```
AUTH_TOKENS=[{"client":"claude-code-mac","token":"...","read":["*"],"write":["*"],"registryWrite":true}]
```

and `whoami` in the doctor output above resolves it to `"read":[{"max":"open","namespace":"*"}]`,
with the same for `write`. The model reaches every namespace and stops at `open`. A write into a
namespace that classifies higher is refused:

```
$ lumberroom write "..." --namespace personal:finance
lumberroom: memory_write failed: client claude-code-mac may write to personal:finance only up to open, not private
```

`personal:finance` and `personal:health` classify `private` by default and `credentials:*`
classifies `sealed` (migration `20260819000004_sensitivity.sql`). To reach them, write the object
form and restart the server: `{"namespace":"*","max":"sealed"}`. Three spellings of a grant list
mean three different things, and `.env.example` carries the rule above `AUTH_TOKENS`.

### memory_forget is absent unless the grant carries mayDelete

The MCP surface has twelve tools (`src/mcp/capability.rs` is the exhaustive list). A bare grant
like the one above sees seven: `context_bootstrap`, `memory_search`, `memory_write`,
`registry_get`, `alias_list`, `review_queue`, `review_decide`. `review_queue` lists what the read
grant already admits; because this grant also carries `"write":["*"]`, `review_decide` may
supersede, merge, confirm or keep_both any row it can see, at `open`, since `writable_row` passes
wherever read and write both hold at the row's stored level. `delete` is the one verdict this
grant is refused, because it also needs `mayDelete`, the same flag `memory_forget` reads below. A
read-only grant refuses every row instead; `docs/permissions.md` ("The two review tools") covers
that empty-verdicts case. Once the person has asked it to work the queue, the model may decide a
proposal item itself, one `review_decide` call per item, and it reports back what it applied over
a check the item had already failed.
`memory_forget` is missing because the grant sets no `mayDelete`, and `tools/list`
filters it out per credential (`src/mcp/mod.rs`). Keeping it out of the list keeps the idea away
from the model in the first place; the service refuses the call again if one arrives anyway.
`.env.example` argues this is a decision rather than an omission: a model that can delete memories
fails worse than one that hoards them.

`lumberroom forget --query "..." --dry-run` will mislead you here. The dry run is client-side: the CLI
searches, prints candidates and stops, so it succeeds under a credential that cannot delete
anything. The refusal arrives on the real `DELETE`, where `assert_may_delete` runs ahead of the
lookup. The flag gates the CLI for the same reason it gates the tool, since the header separating a
CLI from a model is one a model can set for free.

Two ways to change it, both in `.env.example`: set `"mayDelete":true` on this entry and hand
`memory_forget` to the model along with it, or add a second entry no model holds, client `lumberroom`,
object-form grants at `sealed` plus `"mayDelete":true`, and run the CLI against it with
`LUMBERROOM_TOKEN`.

---

## 6. Troubleshooting

Every entry here comes from a failure this project has already had.

**403 on every MCP request while the health checks pass.** The Host allowlist does not include your
domain. `rmcp` validates the `Host` header against a list defaulting to loopback, so a deployment
reached at a real hostname answers `/healthz`, `/readyz` and every metadata document while refusing
every MCP call with a 403 that Claude Code reports as a connection failure. `allowed_hosts` in
`src/http/mod.rs` derives the list from `PUBLIC_URL` plus the three loopback names, and an entry
with no port matches any port, which keeps a proxy terminating on 443 and forwarding to 8787
working. Fix `PUBLIC_URL` on the box and restart. Nothing local reproduces this, so it shows up on
the first real deployment and nowhere before.

**A write refused with "only up to open".** The grant's ceiling, not the namespace. The message
names the namespace it refused and the ceiling your credential holds for it, and it never names any
other namespace's ceiling. Widen the entry to the object form or write into a namespace that
classifies `open`. Section 5 has both.

**A tool call hangs, or the connection never completes.** The embedding model is downloading. The
server warms the embedder and only then opens the listener (`src/main.rs`), so from Claude Code this
reads as a connect that never finishes rather than a slow tool. A baked image should never do it:
the Dockerfile prefetches the weights into `/models` at build time and asserts the binary exceeds
5MB, after a `COPY` mtime trap once shipped a 325KB stub with an empty `/models` past every check.
Ask `curl -s <url>/healthz` and read `docker compose logs server`; the line to wait for is
`embedder ready`. A hang past a minute means the process is not the baked image and fastembed is
pulling roughly 209MB.

**An empty digest with a non-zero count.** The private read path. `memories_count` in `DIGEST_SQL`
counts every row your ceiling admits, then the service drops any private row whose plaintext it
could not produce and leaves the count alone. Three causes, each with its own log line: no
ciphertext reader wired, `KEK_PROVIDER=none`, or a key this store was not sealed with. Check
`docker compose logs server | grep -i kek` and run `lumberroom-server verify-kek`, which does the boot comparison
on demand. A credential whose ceiling stops at `open` never sees this, because private rows never
enter its count either.

**Claude Code shows the server as failed.** Run `lumberroom doctor` from the Mac. A 401 means the token in
the MCP registration does not match `AUTH_TOKENS` on the box. Re-run `wire-mac.sh` with the right
one.

**The model never calls a tool on its own.** `lumberroom stats --hours 168`. If `unprompted` stays at
zero, the digest is arriving and the write rule is not: check the `lumberroom` block is still in
`~/.claude/CLAUDE.md`.

**The hook produces nothing.** It exits 0 with no output on any failure so it can never block a
session. Run it by hand as section 2 shows, and check `~/.local/bin/lumberroom` is executable.

**A browser client fails before it offers to authenticate.** An unauthenticated request must return
401 with a `WWW-Authenticate` header carrying the resource-metadata pointer. Claude Code's fallback
probing hides this whole class of bug, so a green result here proves nothing about Claude.ai or
ChatGPT: `curl -si https://<domain>/mcp | grep -i www-authenticate`.

---

## 7. A second Claude Code install

The Phase 2 spec puts this first among the surfaces because it needs no new server capability. It
proves multi-client behaviour, per-client instrumentation and cross-client recall against a client
already known to work, which keeps "the grant is wrong" and "the OAuth flow is wrong" from arriving
on the same afternoon.

**Give it its own token.** A grant that cannot tell two clients apart decides nothing: every
`tool_calls` row takes its client from the credential, so two installs sharing one token collapse
into a single row in `lumberroom stats --by-client`, and you cannot narrow one without narrowing both.
Identity comes from the `client` field of the `AUTH_TOKENS` entry. `wire-mac.sh --name` names the
MCP entry inside Claude Code and the server never sees it.

On the box, mint a token and add a second entry. Single-quote the whole value. Several scripts
source `.env` with `sh`, which strips the double quotes and leaves invalid JSON, while Docker
Compose parses the unquoted form fine, which is why the trap hid for so long.

```bash
openssl rand -hex 32
```

Leave the first entry as it stands and append the second. The example below keeps the live
bare-string grant on `claude-code-mac`, so pasting it changes nothing about the install you already
have:

```
AUTH_TOKENS='[{"client":"claude-code-mac","token":"<first>","read":["*"],"write":["*"],"registryWrite":true},{"client":"claude-code-laptop","token":"<second>","read":[{"namespace":"*","max":"open"}],"write":[{"namespace":"user:me","max":"open"},{"namespace":"project:*","max":"open"}],"mayDelete":false}]'
```

Raising either entry to a `sealed` ceiling takes `"sealedCapable":true` on that entry as well.
Without the flag a client holds the ceiling and still receives ciphertext it cannot open.

```bash
docker compose up -d server
```

On the second Mac, from its own clone:

```bash
LUMBERROOM_TOKEN=<second> ./client/wire-mac.sh --url https://memory.example.com --dry-run
LUMBERROOM_TOKEN=<second> ./client/wire-mac.sh --url https://memory.example.com
```

Then check the server can tell them apart:

```bash
lumberroom stats --by-client
```

With one client on this stack today:

```
window: last 168h
  claude-code-mac    calls   29  reads   19  writes   10  write/read 0.53  unprompted-write 71%
```

A second install adds a second line with its own write-to-read ratio. `/admin/whoami` called with
the new token answers what it resolves to, straight from the code path that enforces it.

That restart is the cost OAuth removes. Under `AUTH_MODE=oauth` the consent screen writes the grant
into `oauth_client` and a change takes effect on the next request, with no `.env` edit anywhere.
Revoking still has no subcommand in either CLI: `lumberroom clients` lists the rows and the revoke itself
is one `UPDATE` in psql, which `deploy/oauth.md` §4 spells out. With static tokens, every grant
change is an `.env` edit and a restart.

---

## One CLI note

`lumberroom --help` is not implemented. `lumberroom` reads the first positional argument as the command
and defaults to `doctor`, and `--help` parses as a flag, so `lumberroom --help` runs `doctor` against
whatever endpoint your config points at. The usage block at the top of `lumberroom` is the
authoritative list, and an unknown command prints a short version of it.
