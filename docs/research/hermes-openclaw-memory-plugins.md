# Lumberroom memory-provider support for Hermes Agent and OpenClaw

The evidence has three layers, and this report keeps them apart. **Source** means a cited line in a harness or engine checkout. **Design** means what the design stage proposes. **Review** means a Sonnet reviewer's check of the designs against source. Nobody has run any code in this report. The reviewers traced citations but ran nothing, so treat every behaviour claim about the plugins as a proposal.

Checkouts used:
- Hermes at `fdec926e`.
- OpenClaw at `4c9869c6`, package 2026.9.6.
- Engine at `b12a03a`.
- lumberroom-cloud at `a03eece`.

The dedicated lumberroom explorer returned nothing. Engine facts below therefore come from the design agents' own reads, cited with `ENG/` (`/Users/aditya/work/cbrspn-tech/lumberroom`).

## 1. The answer

**Hermes.** Build a Python `MemoryProvider` plugin that owns `memory.provider` and talks to lumberroom's existing `/mcp` endpoint through the official `mcp` SDK.
- **Recall:** `prefetch` recalls on every non-trivial turn.
- **Writes:** the model writes through `memory_write`. The plugin proxies that tool from the engine's live `tools/list`, so the write rule keeps one source.
- **OSS and hosted:** they differ in `base_url` and auth mode only. OSS uses a static bearer or OAuth. Hosted uses OAuth only.
- **Distribution:** ship it from the engine repo at `client/hermes/`, publish it to PyPI with a `hermes_agent.memory_providers` entry point, and list it through a community-tier entry in the Hermes plugin catalog.
- **In-tree PR:** not possible. Hermes has closed in-tree memory providers (`hermes-agent/CONTRIBUTING.md:70-84`).

**OpenClaw.** Build a TypeScript plugin of `kind: "memory"` that owns `plugins.slots.memory` and also calls lumberroom over MCP with `@modelcontextprotocol/sdk`.
- **Recall:** a cached digest goes in through `prependSystemContext`, and per-turn search goes in through `prependContext`.
- **Tools:** the plugin registers the engine's tools under their own names.
- **Built-in writes:** a null `flushPlanResolver`, dreaming off, and a `before_tool_call` guard stop OpenClaw's built-in durable writes. The reviewer added the guard.
- **Distribution:** ship from the engine repo at `client/openclaw/` to npm and ClawHub, and ask OpenClaw for a docs page on the Honcho precedent.
- **Fallback:** document the MCP mount plus the write-rule snippet as the no-code option.

**Engine and fork.** Neither harness needs a Rust change for v1. Both plugins and their tests belong upstream, and the fork inherits them through `scripts/merge-upstream.sh` with no divergence entry.

## 2. How memory works in each harness today

### Hermes (source)

- **Slot:** one external provider at a time, chosen by `memory.provider` (`agent/memory_manager.py:379-389`, `hermes_cli/config_defaults.py:1286-1300`).
- **Built-in store:** `MEMORY.md` and `USER.md` load alongside the provider unless `memory_enabled` or `user_profile_enabled` is off (`agent/agent_init.py:1301-1323`, `agent/system_prompt.py:515-540`).
- **Contract:** ABC at `agent/memory_provider.py:84`. The abstract members are `name`, `is_available`, `initialize` and `get_tool_schemas` (:91-143). Every other hook has a default.
- **Recall:** `prefetch` runs on a thread with an 8.0s join. A provider that is still stuck gets skipped on later turns (`memory_manager.py:32,457-501`). The output is fenced in `<memory-context>` and appended to the current user message (`memory_manager.py:317-333`, `agent/turn_context.py:85-104`).
- **Capture:** `sync_turn` runs on a single-worker executor. Shutdown drains it for 5.0s (`memory_manager.py:31,533-596,848-894`). Interrupted turns do not sync (`run_agent.py:921-935`).
- **Errors:** `_each_provider` logs and swallows every hook failure (`memory_manager.py:367-377`). If init raises, the manager becomes None and the agent runs on (`agent_init.py:1359-1361`).
- **Tool routing:** `add_provider` builds the routing table from `get_tool_schemas()` before `initialize()` (`memory_manager.py:379-430`).
- **Discovery:** bundled, then `$HERMES_HOME/plugins/<name>`, then `./.hermes/plugins` (opt-in), then the entry point group `hermes_agent.memory_providers` (`plugins/memory/__init__.py:1-30,95-136`).
- **Setup:** `hermes memory setup` hands off to `post_setup` when the provider has it. Otherwise it walks `get_config_schema` (`hermes_cli/memory_setup.py:157-305`).
- **MCP:** MCP tools have no link to `MemoryManager`, so an MCP mount gets no prefetch or sync (`website/docs/user-guide/features/mcp.md`, `plugins/index.md:79`).
- **Profile scope:** background work must go through `spawn_context_thread`, and secrets through `get_secret` (`plugins/AGENTS.md:75-85`, `agent/secret_scope.py:200-228`).

### OpenClaw (source)

- **Slot:** `plugins.slots.memory` defaults to `memory-core`, and `none` turns it off (`src/plugins/slots.ts:22-25,67-95`). Only the slot owner of kind `memory` loads (`src/plugins/config-activation-shared.ts:250-279`).
- **Capability:** `registerMemoryCapability` is limited to kind-memory plugins. Its fields are `promptBuilder`, `flushPlanResolver`, `runtime` and `deterministicRecallToolName` (`src/plugins/registry-registrars-memory.ts:9-54`, `registry-contribution-types.ts:284-293`).
- **Dreaming sidecar:** when the slot owner is not memory-core, memory-core still loads as a dreaming sidecar. Dreaming defaults to on. Any capability fields the owner omits come from the sidecar (`src/plugins/loader-shared.ts:73-139`, `src/memory-host-sdk/dreaming.ts:22,350-404`, `src/plugins/memory-state.ts:57-72`).
- **Recall hook:** `before_prompt_build` returns `prependContext`/`appendContext`, which wrap the user message for submission only, and `prependSystemContext`/`appendSystemContext`, which stay cacheable (`src/plugins/hook-before-agent-start.types.ts:21-51`, `src/agents/embedded-agent-runner/run/attempt-llm-boundary.ts:370-468`). Registrations with `requiresToolAuthority` run after tool policy settles (`src/plugins/hooks.ts:983-1065`). The default timeout is 15s (`hooks.ts:154`).
- **Conversation access:** a non-bundled plugin needs `plugins.entries.<id>.hooks.allowConversationAccess: true`, or OpenClaw drops its conversation hooks (`src/plugins/hook-policy-decisions.ts:10-17`, `registry-registrars-tools-hooks.ts:395-411`). Installs never write this grant. Honcho's setup command writes it (openclaw-honcho `commands/cli.ts:123-143`).
- **Bootstrap files:** with no memory `runtime`, OpenClaw still injects MEMORY.md and USER.md (`src/agents/bootstrap-files.ts:240-305`).
- **Tool names:** `memory_search` and `memory_get` are core catalog ids (`src/agents/tool-catalog.ts:155-168`). active-memory defaults to those names (`extensions/active-memory/types.ts:30-31`).
- **Policy:** third-party integrations ship outside the tree (`CONTRIBUTING.md:20`, `VISION.md:95-100,137`).
- **Churn:** `slots.ts` changed 6 times between June and September 2026, and `registry-registrars-memory.ts` changed 6 times between 07-18 and 09-03 (GitHub commits API).

## 3. Options, ranked

### Hermes

1. **A. Native MemoryProvider over MCP (recommended).**
   - Forced recall through `prefetch`.
   - The plugin owns the slot, and the write rule stays single-sourced in `ENG/src/mcp/mod.rs`.
   - Weaknesses:
     - The plugin depends on `tools.mcp_oauth_manager.get_or_build_provider`, which is Hermes-internal and not a plugin API.
     - Recall blocks replay until compression.
     - Every catalog SHA bump needs a reviewed PR.
2. **A-E. Option A, keeping the built-in store and mirroring through `on_memory_write`.** The reviewer added this variant. It keeps Hermes's native review fork at `agent/turn_context.py:709-718`, which option A loses when it switches the built-in store off. The design called that fork's replacement nudge weaker.
   - Cost: the built-in tool's prompt, not lumberroom's `memory_write` description, decides what gets written.
   - Cost: `MEMORY.md` entries carry no namespace.
   - Sub-variant (proposed here): mirror into the proposal queue instead of live rows. That keeps the review gate but needs `may_ingest`.
   - This is an owner decision (section 7, Q1).
3. **B. Native provider over a new engine REST surface.** It adds a second model-facing contract, duplicates the write rule, and makes plugin releases wait on an engine release. It also still needs an OAuth library.
4. **D. General `pre_llm_call` hook plugin plus an MCP mount.** It forces recall but leaves `memory.provider` open, so the built-in store keeps writing. It also means two configs and two credentials for one server.
5. **C. MCP entry plus rules only.** It needs no code, but recall is unforced. An `optional-mcps` catalog entry pins a fixed URL, so it can only point at hosted, which makes it cloud-first by construction. Keep it as a hand-written documented fallback for self-hosters.

**Reviewer corrections folded into A.**
- `initialize` must not read `kw["cwd"]`. The ABC does not guarantee `cwd` (`agent/memory_provider.py:106-110`), and Hermes passes it only when `session_cwd` is set, which defaults to None (`agent/agent_init.py:1275-1276,2391`).
  - Gateway platforms are where `cwd` is most likely missing.
  - A `KeyError` there gets swallowed, and the provider sits inert.
- In OAuth mode with no cached token and a non-interactive caller (gateway, cron, subagent), `get_or_build_provider` raises `OAuthNonInteractiveError` synchronously (`tools/mcp_oauth_manager.py:305-311`, `tools/mcp_oauth.py:206-208,1236`).
  - `initialize_all` swallows it (`memory_manager.py:896-902`).
  - `agent_init.py:1353-1354` logs "Memory provider activated" anyway.
  - The design's failure table did not cover this case.

### OpenClaw

1. **Option 1. Native memory-slot plugin over MCP (recommended).**
   - Forced recall. The plugin owns the slot, and the engine's own tool names line up with active-memory and the coding profile.
   - One code path serves OSS and cloud.
   - Weaknesses:
     - The largest build of the four.
     - A second MCP client with its own token family.
     - Setup must write `allowConversationAccess`.
     - The plugin needs a pinned compat floor and weekly CI against the latest OpenClaw.
2. **Option 3. Bundle only (MCP mount, write-rule skill, slot `none`).** Ship it as the documented no-code fallback. Recall is unforced, the tool names come out prefixed as `lumberroom__memory_write`, and the sole-store guarantee depends on the user's config.
3. **Option 2. Hook-only sidecar plus the host MCP mount.**
   - Cloud users log in twice.
   - Prefixed tool names miss active-memory's `toolsAllow`.
   - The sidecar cannot register a capability, so it cannot stop the memory-core flush or dreaming.
4. **Option 4. Native plugin over a new REST surface.** It duplicates the MCP contract and adds hosted attack surface for no user-visible gain.

**Reviewer corrections folded into option 1.**
- **Write bypass (major).** OpenClaw's ordinary `write` tool writes any path under the workspace root, MEMORY.md and USER.md included (`src/agents/sessions/tools/write.ts:482-537`). The only append-only restriction applies inside the flush run (`src/agents/agent-tools.memory-flush.ts:5-18`), and the design disables the flush. A `promptBuilder` line alone leaves a parallel local store open. The design now registers a `before_tool_call` guard (hook at `src/plugins/hook-types.ts:127`), covered in section 4.
- **Schema wrapping.** `registerTool` parameters are TypeBox `TSchema` (`src/agents/tools/common.ts:14,58`), so the plugin wraps each raw JSON Schema with `Type.Unsafe(...)`. This is resolved and no longer an unknown.
- **active-memory with no `memory_get`.** The reviewer marked this benign. active-memory filters its allow list through `toolAuthority.allows` and falls back to `deterministicRecallToolName` (`extensions/active-memory/index.ts:239-241,355,369-370`).
- **Unresolved conflict.** The reviewer says a plugin cannot suppress bootstrap injection, because `WORKSPACE_BOOTSTRAP_FILENAMES` is a fixed list (`src/agents/workspace-bootstrap-policy.ts:27-33`). The OpenClaw explorer instead read that a slot owner which registers a `runtime` without `classifyWorkspaceMemoryPaths` gets MEMORY.md and USER.md excluded from automatic context, with a warning (`src/agents/bootstrap-files.ts:240-305`). Nobody has run either path. Suppression would not block writes, so the guard is needed either way.

## 4. Recommended design per harness

### 4.1 Hermes: `client/hermes/`

**Package layout (design).**
```
client/hermes/
  __init__.py      register(ctx) -> ctx.register_memory_provider(LumberroomProvider())
  plugin.yaml
  pyproject.toml   mcp>=2.0.0,<3 ; httpx2>=2.7,<3 ; entry point lumberroom = "lumberroom_hermes:register"
  provider.py      hooks only
  bridge.py        loop thread, two MCP sessions, call(tool, args, *, hook, timeout)
  auth.py          build_auth(cfg, mcp_url): bearer or OAuth provider
  config.py        LumberroomConfig, load, save
  recall.py        format_digest, format_hits, Budget, InjectedIds
  schemas.py       FALLBACK_TOOLS, select(allowlist, live)
  setup.py         post_setup, get_config_schema, disable_builtin, import_builtin
  cli.py           hermes lumberroom login | status | import-builtin
  tests/
```
Hermes already pins `mcp==2.0.0` and `httpx2` in its `[mcp]` extra (`pyproject.toml:391-402`).

**Config (design).** The block lives under `memory.lumberroom` in the profile's `config.yaml`. Secrets go in the profile `.env` and load through `get_secret`.

| key | default | notes |
|---|---|---|
| `base_url` | required, no default | Self-hosted URL or the hosted URL. `/mcp` is appended. `LUMBERROOM_URL` overrides it. The setup picker offers OSS and hosted without favouring either. |
| `auth` | `token` if `LUMBERROOM_TOKEN` is set, else `oauth` | Hosted is OAuth only. |
| `project` | `auto` | Git root of `cwd` when `cwd` is present. Otherwise a fixed slug, or `none` (review fix). |
| `recall`, `digest` | `true` | |
| `digest_max_chars` | 6000 | Matches the engine default (`ENG/src/config.rs:331-345`). |
| `recall_limit` / `recall_max_chars` | 4 / 1200 | |
| `review_interval` | 10 | 0 turns the nudge off. |
| `tools` | memory_search, memory_write, registry_get, memory_forget | The server grant filters it further. |
| `owner_user_ids` | `[]` | Empty means only local platforms are allowed. |
| `prefetch_timeout_s` / `tool_timeout_s` | 3.0 / 20.0 | |

Token budgets are design targets, not measurements: about 2600 tokens on the first turn and about 440 on each later turn.

**Hook by hook (design, with review fixes).**
- **`is_available`:** config and credential present, no network call. `unavailable_reason` names the missing key.
- **`get_tool_schemas`:** before init, it returns the full candidate set, which becomes the routing table. After init, it returns candidates ∩ live `tools/list`, using the server's descriptions and schemas. It never returns a name outside the candidate set.
- **`initialize`:**
  - Uses `kw.get("cwd")` with a fallback.
  - Starts the loop thread through `spawn_context_thread`.
  - Opens two sessions. The hook session sends `x-memory-invocation: hook`, and the model session sends no invocation header. Both send `x-session-id`.
  - Fetches `tools/list` and `instructions` within 2s and falls back to caches on timeout.
  - Catches `OAuthNonInteractiveError` and sets `self._unauthenticated` (review fix).
  - Evaluates the gateway owner gate.
- **`system_prompt_block`:** the server `instructions` plus one static line naming the tools. Empty when gated.
- **`on_turn_start`:** records the turn author for the owner gate.
- **`prefetch`:**
  - Returns empty when gated, off, or the breaker is open.
  - When unauthenticated, returns one line: "lumberroom is not logged in: run `hermes lumberroom login`" (review fix).
  - On the first turn after init or a switch, it calls `context_bootstrap(project)`.
  - Then `memory_search(query, limit, project)` on the hook session, dropping ids already injected.
  - Returns plain markdown, bounded to 3.0s. The host adds the `<memory-context>` fence.
- **`sync_turn`:** no write. It advances the nudge counter in the primary context only.
- **`handle_tool_call`:** `tools/call` on the model session. It passes `possible_conflicts` back unchanged. It returns the gate error or the unauthenticated error when either applies.
- **`on_session_switch`:** rebinds `x-session-id`, re-arms the digest, and clears injected ids on reset, rewind or compression.
- **`on_memory_write`:** a no-op under option A. Under A-E it becomes the mirror.
- **`shutdown`:** closes sessions within 2s, inside the host's 5s drain.

**Built-in store (option A).** `post_setup` writes:
- `memory.provider: lumberroom`
- `memory_enabled: false`
- `user_profile_enabled: false`
- `nudge_interval: 0`

It then offers `import-builtin`, which posts `MEMORY.md` and `USER.md` entries to `/admin/ingest/proposals` as `speaker: "main_model"`. That speaker never auto-approves (`ENG/src/services/ingest.rs:43,97,659-667`). The command needs `may_ingest`.

**Auth.**
- **OSS token mode:** a per-profile `AUTH_TOKENS` entry, with `LUMBERROOM_TOKEN` in the profile `.env`.
- **OSS OAuth mode and hosted:**
  - DCR plus PKCE through Hermes's OAuth manager, which wraps the SDK's `OAuthClientProvider` and adds a cross-process refresh fence (`tools/mcp_oauth_manager.py:1-6,282-298`).
  - Headless hosts take a pasted callback. The engine has DCR but no device grant (`ENG/src/authserver/routes.rs:106,156`).
  - Hosted refuses `AUTH_TOKENS` at boot (`src/cloud/auth.rs`, `refuse_static_tokens`).
  - The plugin never reuses the lumberroom CLI's token file, because sharing a refresh family revokes it (7 Sep 2026).
- **Fallback if the internal manager changes:** build the SDK's `OAuthClientProvider` directly and add the plugin's own file lock. That is the library path the manager wraps.

**Failure behaviour (design, with review additions).**

| condition | behaviour |
|---|---|
| lumberroom slow or down during prefetch | Returns empty within 3.0s. The breaker opens for 60s after 3 failures. The first failure of an outage injects "lumberroom unreachable: memory was not checked this turn". |
| lumberroom down during a tool call | Error text ending "Nothing was stored." No local write buffer. |
| 401, token mode | The error names `LUMBERROOM_TOKEN`. |
| 401, OAuth mode | The SDK refreshes once. After that, the error says to run `hermes lumberroom login`. |
| OAuth, no cached token, non-interactive (review) | `initialize` catches the error and the plugin marks itself unauthenticated. Prefetch and tools say so. `hermes lumberroom status` runs a live `tools/list`, because the host's "activated" log proves nothing. |
| no `cwd` (review) | `project` falls back to config, or to `none`. |
| gateway api_server reuse (review) | `initialize_all` runs once per gateway session across rebuilt agents (`agent_init.py:1327-1330`). The owner gate and project stay frozen until a session reset. Document this. |
| non-owner in a gateway | No prompt block, no recall, and tools refuse. |

**Tests (design, with review additions).**
- **Unit (pytest, fake server built with the SDK's server side):**
  - Candidate versus post-init schemas.
  - Digest arming.
  - Dedupe and character caps.
  - Timeout under 3.2s.
  - Breaker.
  - Header assertions per session.
  - Owner gate.
  - `sync_turn` makes no call.
  - Nudge cadence.
  - `initialize` with no `cwd` key.
  - `initialize` with a missing token in non-interactive OAuth mode.
- **Hermes contract:**
  - Load the provider from a temp `HERMES_HOME/plugins/lumberroom` and from the wheel's entry point.
  - Assert `add_provider` routes every exposed tool.
  - Assert the `get_or_build_provider` signature.
- **Integration (lead only):** `scripts/hermes-plugin-test.sh` boots the engine compose stack in token mode, then in `AUTH_MODE=oauth`. In each mode it runs a nonce write-then-recall loop and checks `lumberroom stats --by-client`, where prefetch should count as `hook` and model writes as unprompted.
- **Hosted acceptance:** the owner runs this by hand.

### 4.2 OpenClaw: `client/openclaw/`

**Package layout (design).**
```
client/openclaw/
  package.json            peerDependencies.openclaw; openclaw.compat.pluginApi ">=2026.9.6"; install.{npmSpec,clawhubSpec,minHostVersion}
  openclaw.plugin.json    id lumberroom, kind memory, configSchema, contracts.tools, toolMetadata,
                          cliCommands [lumberroom], configContracts.secretInputs ["auth.token"]
  src/index.ts            definePluginEntry
  src/config.ts           TypeBox schema
  src/types.ts            LumberroomClient, CallMeta, ToolResult, FactBody, PostReport
  src/client/mcp.ts       SDK Client + StreamableHTTPClientTransport, per-call headers
  src/client/http.ts      /admin/whoami, /admin/ingest/*
  src/client/breaker.ts
  src/auth/store.ts       0600 token file, proper-lockfile, single-flight refresh
  src/auth/oauth.ts       SDK OAuthClientProvider: DCR, PKCE, loopback, --code paste
  src/tools.ts            registerTool proxies, Type.Unsafe-wrapped schemas (review)
  src/capability.ts       promptBuilder, flushPlanResolver => null, deterministicRecallToolName
  src/guard.ts            before_tool_call write guard (review)
  src/eligibility.ts
  src/recall/{digest,turn}.ts
  src/capture/{cursor,extract,post}.ts
  src/cli/*.ts            setup, login, logout, status, doctor, import
  src/generated/{tool-snapshot.json,extract-prompt.md}
  scripts/snapshot.mjs
  test/unit, test/integration
```

**Config (design).**
```json5
plugins.entries.lumberroom.config = {
  baseUrl: "http://127.0.0.1:8787",   // required; no cloud default
  auth: { mode: "token" | "oauth", token: SecretRef, clientId: null, clientName: "openclaw" },
  project: null,
  recall: { digest: true, digestMaxChars: 6000, digestTtlMs: 1800000, digestTimeoutMs: 8000,
            perTurn: true, turnLimit: 5, turnMaxChars: 2000, turnTimeoutMs: 3000, minQueryChars: 12,
            chatTypes: ["direct"], triggers: ["user"] },
  capture: { mode: "off" | "propose", maxTurnChars: 8000, model: null },
  timeoutMs: 20000,
  dreaming: { enabled: false }
}
```
Setup also writes `plugins.slots.memory: "lumberroom"` and `plugins.entries.lumberroom.hooks.allowConversationAccess: true`, and shows the diff before it saves.

**Hook by hook (design, with review fixes).** Eligibility runs first on every hook. A call is skipped for:
- incognito session keys
- chat types outside `recall.chatTypes`
- triggers outside `recall.triggers`
- an open breaker

The hooks:
- **`registerService.start`:** MCP `initialize` and `tools/list`, cached. If the server is down, the plugin falls back to the snapshot and retries in the background.
- **`registerTool` per engine tool:**
  - A factory returns null when the grant lacks the tool.
  - Descriptions come from the server, and schemas are wrapped with `Type.Unsafe`.
  - Tool calls send no invocation header, so the engine counts them as model calls.
- **`registerMemoryCapability`:**
  - `promptBuilder`: the server `instructions` plus a line saying durable facts go to lumberroom only.
  - `flushPlanResolver`: returns null.
  - `deterministicRecallToolName`: `memory_search`.
- **`before_prompt_build`, ordinary phase:** once per session key or TTL, calls `context_bootstrap {project}` with the hook header. It returns the digest as `prependSystemContext`, byte-identical within a session so the prompt cache holds.
- **`before_prompt_build`, `requiresToolAuthority`:** runs when `toolAuthority.allows("memory_search")`. It calls `memory_search` with the hook header, drops ids already in the digest or already injected, and returns a `<lumberroom-recall>` block as `prependContext`.
- **`before_tool_call` (review fix):**
  - Denies `write` or edit calls whose resolved path matches `MEMORY.md`, `USER.md` or `memory/**` in the workspace, while lumberroom owns the slot.
  - The denial text points the model at `memory_write`.
  - `doctor` fails if the guard is not registered.
- **`agent_end`, only when `capture.mode = "propose"`:** runs `api.runtime.subagent.complete` with the engine's extraction prompt and posts to `/admin/ingest/runs` and `/admin/ingest/proposals`. It advances a per-session cursor only after a 200. Nothing reaches the live store without review.
- **`session_end` with reason other than `compaction`:** drains the capture cursor.

**What happens to built-in memory.**
- memory-core does not load.
- The flush is off.
- Dreaming is off through config, and `doctor` checks it.
- MEMORY.md and USER.md stay injected and read-only through the guard. The runtime-suppression path in section 3 is unresolved.
- `openclaw lumberroom import` sends MEMORY.md, USER.md and `memory/*.md` to the proposal queue.

**Auth.**
- **OSS token mode:** bearer from `AUTH_TOKENS`, stored as a SecretRef. The engine honours it in every `AUTH_MODE`.
- **OSS OAuth mode and hosted:**
  - The SDK handles RFC 9728 and RFC 8414 discovery, DCR, PKCE S256, and a loopback redirect.
  - Headless hosts get a `--code` paste fallback.
  - Tokens go to a 0600 file under a cross-process lock with single-flight refresh, because a replayed refresh token revokes the whole family.

**Failure behaviour (design).**
- Hooks fail open on every error or timeout, and the turn proceeds with no memory context. Warnings are rate-limited.
- After 3 consecutive failures the breaker skips calls for 60s.
- Tools return explicit error text, for example "lumberroom unreachable at <baseUrl>: <reason>. Nothing was written."
- No local write spool.
- Capture delivers at least once, and the server dedupes.
- The plugin never retries tripwire refusals.

**Tests (design, with review additions).**
- **Unit (vitest):**
  - Config defaults and rejects.
  - The eligibility table.
  - Digest clamp and cache key.
  - Dedupe.
  - Breaker.
  - Headers per invocation, with `x-session-id` clipped to 128 characters.
  - Error mapping.
  - `flushPlanResolver` returns null.
  - `promptBuilder` falls back to the snapshot.
  - Cursor idempotence.
  - A refresh race under the lockfile.
  - Guard path matching, including `../` and symlink cases.
- **Integration (engine in docker compose):**
  - Token mode, then OAuth mode.
  - `tools/list` matches the snapshot.
  - A write-then-search round trip.
  - Hook calls counted as not unprompted.
  - Proposals land in the queue and not in the store.
  - With the engine stopped, hooks return within their timeouts.
- **Host contract (real OpenClaw 2026.9.6 gateway, installed from `npm pack`):**
  - `openclaw plugins inspect lumberroom --runtime --json` shows no blocked hooks and lumberroom as slot owner.
  - memory-core is not loaded.
  - The model cannot write MEMORY.md (review addition).
  - The nonce acceptance test.
- **Compat:** a weekly CI run against the latest OpenClaw release.

## 5. What lumberroom needs to add

| item | side | shared? |
|---|---|---|
| `client/hermes/` package | engine | no |
| `client/openclaw/` package | engine | no |
| One compose-based plugin test harness that boots the engine in token and OAuth modes and scripts DCR plus PKCE consent | engine | **yes.** Both `scripts/hermes-plugin-test.sh` and the OpenClaw CI job need the same stack. Build it once. |
| Tool snapshot: the engine's `tools/list` plus `instructions`, generated from a running engine with a drift check | engine | **yes (proposed here).** Hermes `FALLBACK_TOOLS` and OpenClaw `tool-snapshot.json` hold the same data. One generated `client/tool-snapshot.json` can serve both. |
| Move the extraction prompt from `crates/lumberroom/src/ingest/prompt.rs` into a sibling `.md` loaded with `include_str!` (E2) | engine | Serves OpenClaw capture. It serves Hermes only if Hermes also gets propose-capture (Q2). |
| Narrower `mayPropose` grant, if the owner rejects shipping capture on `mayIngest`, which also opens watermarks and cleanup routes (`ENG/docs/permissions.md:105-128`) | engine | yes. Both capture paths and Hermes `import-builtin` use it. |
| Fix wrong Hermes claims: "identifies as Claude Code" (`docs/connect-agents-md.md:88-89`, `docs/research/client-capabilities.md:33`). Source uses "Claude Code" only for mcp.figma.com (`tools/mcp_oauth.py:1089-1131`). Also fix "no recall hook" in `client/hermes-notes.md`. | engine | n/a |
| `docs/connect-openclaw.md`, the Hermes connect doc, README client list, `BOOTSTRAP_MAX_CHARS_BY_CLIENT` examples | engine | n/a |
| Rust server, schema or migration changes | none for v1 | |
| Anything in lumberroom-cloud | none. merge-upstream brings `client/` down, and `docs/fork-divergence.md` stays untouched. | |
| Connect pages on lumberroom-web | separate private repo | |

Both designs put all plugin logic in harness-specific code (Python and TypeScript), so there is no shared runtime library. They share the contract: MCP tools, the `x-memory-invocation` and `x-session-id` headers (`ENG/src/http/mod.rs:43-57`, `ENG/src/domain/types.rs:289-310`), the ingest JSON shape, and the snapshot.

## 6. Path to "official"

### Hermes

What to ship:
- A tagged engine release.
- The `lumberroom-hermes` PyPI package with the entry point. The name was free on 25 Sep 2026, per the design stage.
- A PR to NousResearch/hermes-agent adding `plugin-catalog/lumberroom.yaml` with:
  - repo `the-cybersapien/lumberroom`
  - `subdir: client/hermes`
  - a 40-hex SHA
  - `category: memory`
  - `tier: community`
  - `requires_hermes` floor

What the maintainers expect (`plugin-catalog/README.md:8-93`):
- A human-merged PR.
- A SHA pin, with every bump as a new PR.
- No self-updaters.
- Declared capabilities that match behaviour.
- A passing security scan at admission.
- Owner submission.
- Dependencies declared in `pyproject.toml`, installed under Hermes pins.
- Tests per `plugins/AGENTS.md:120-124`.

Tier:
- The design reads `tier: official` as NousResearch-maintained (`website/docs/user-guide/features/plugin-catalog.md:50`).
- The explorer found no admission criterion for the tier. Treat community as the expected ceiling until a maintainer confirms.

Prior art:
- Hindsight left the tree for the catalog on 2026-09-23.
- Honcho and supermemory both carry `tier: community`.

Announce in the Nous Discord `#plugins-skills-and-skins` (`CONTRIBUTING.md:99`).

### OpenClaw

What to ship:
- The npm package, with `openclaw.compat.pluginApi` and `install.clawhubSpec` set.
- `clawhub package publish` under a publisher the owner controls.

Where to submit:
- No code PR.
- Open an issue on openclaw/openclaw asking for `docs/concepts/memory-lumberroom.md` and a card in `docs/concepts/memory.md:190-202`, following `docs/concepts/memory-honcho.md`.

What the maintainers expect:
- An external repo and publication (`VISION.md:95-100`).
- Promotion through ClawHub (`VISION.md:100-101`).

Prior art:
- Honcho (plastic-labs), mem0 (inside mem0's monorepo), supermemory, and OpenViking all ship vendor-owned packages.
- Both supermemory and OpenViking have colliding third-party ClawHub listings. Claim the `lumberroom` names on npm and ClawHub before announcing.
- The design mentions applying for "vetted-publisher status". The prior-art search found no formal badge beyond publication plus a docs link, so read that step as unconfirmed.

## 7. Open questions for the owner

1. **Hermes built-in store.** Option A switches MEMORY.md and USER.md off and replaces Hermes's review fork with a nudge every `review_interval` turns. Option A-E keeps the built-in store and mirrors its writes into lumberroom, live or into the proposal queue. A-E keeps Hermes's native review, but the built-in prompt then decides what gets written.
2. **Auto-capture.** Ship it off by default in both plugins, or on as `propose`? Should Hermes get the same opt-in propose path as OpenClaw? And should capture run on `mayIngest` or on a new upstream `mayPropose` grant?
3. **Home.** Plugins in the engine repo under `client/`, sharing CI with the engine, or separate public repos like Honcho and mem0?
4. **Names and publishing.** npm `@lumberroom/openclaw` needs the `lumberroom` npm org, which is unconfirmed. The alternative is unscoped `lumberroom-openclaw`. On PyPI, `lumberroom-hermes`. Both publications and a public GitHub Actions workflow need your approval under the standing cost rule.
5. **Hosted URL shape.** The Hermes design assumes one `https://mcp.lumberroom.cloud`. The OpenClaw design assumes `https://<tenant>.lumberroom.cloud`. The answer sets the setup prompts, the docs, and the RFC 8707 resource value the OAuth client sends.
6. **Shared-chat default.** Both designs refuse non-owners in gateways and group chats until you list owner ids. Confirm fail-closed, since the digest exposes the whole readable store.