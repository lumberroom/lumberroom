"""
title: lumberroom memory
author: lumberroom
version: 0.1.0
description: >
    Forces recall on every incoming message by calling lumberroom's context_bootstrap tool and
    prepending the digest as a system message, before the model's tool-choice loop ever runs.

Paste this whole file into OpenWebUI: Admin Panel -> Settings -> Functions -> + -> paste -> Save,
then open the function's Valves and set lumberroom_url and lumberroom_token. Enable it globally or per model.

SINGLE-OWNER ONLY. DO NOT ENABLE THIS ON A MULTI-ACCOUNT OPENWEBUI INSTANCE.
----------------------------------------------------------------------------
lumberroom_token is one valve, set once, shared by every OpenWebUI account this function is enabled
for. OpenWebUI has no per-user credential valve to key it off instead. `_digest_for`'s cache is
keyed per `__user__["id"]` purely to keep one fast conversation from calling lumberroom on every
message; that keying is a cache-locality choice, not an access boundary, and it does not change
which token fetches the digest or what the digest contains. The digest itself is
`context_bootstrap`'s full render for whatever grant `lumberroom_token` carries: every readable
registry key with its value, project summaries, everything that credential can see. Enabling this
filter globally, or for any model a second account can reach, hands that owner's whole digest to
every account with a chat open. This is exactly the deployment
[decision 0002](../docs/decisions/0002-built-in-oauth-server.md) rules out: lumberroom assumes one
owner. If OpenWebUI ever grows a per-account credential valve, wire this filter to it before
enabling it anywhere the owner is not the only account.

WHY THIS FILE MATTERS MORE THAN ITS SIZE SUGGESTS
--------------------------------------------------
Every other surface lumberroom connects to (Claude Code, Hermes, Claude.ai, ChatGPT) leaves recall to
the model's own judgment: the tool is available and the model may or may not call it. OpenWebUI's
Filter `inlet` hook is different. It runs on every incoming message, before the request reaches
the model at all, which makes it the only mechanism across every connected surface that can FORCE
recall rather than ask for it. If lumberroom's numbers ever show a surface reading but never writing,
this file is the proof that "the model chose not to call it" and "the model was never given the
chance" are different failures. OpenWebUI removes the second one entirely.

CONTRACT
--------
- class Filter, a pydantic Valves config on it, and an async def inlet(self, body, __user__).
  This matches OpenWebUI's Filter convention exactly; do not rename these.
- Fails open on every error path: a bad URL, a wrong token, a timeout, a malformed response, or
  the server being down all fall through to returning `body` unchanged. A lumberroom outage must never
  break the user's chat, and a filter that raises stops the conversation dead.
- Caches the digest per user for `cache_seconds`, so a fast back-and-forth conversation does not
  turn into one lumberroom call per message. The cache lives on the Filter instance, which OpenWebUI
  keeps alive across requests, so this is a plain dict rather than anything external.
- Sends X-Memory-Invocation: hook, the same header lumberroom's own SessionStart hook sends, so this
  counts as forced recall in `lumberroom stats` rather than the model choosing to call a tool: those
  are different numbers and conflating them would make the Phase 2 measurement meaningless.
- Carries `WRITE_RULE` at the end of the injected system message. OpenWebUI reads no rules file,
  so this is the only route the write rule has to a model behind this filter.
- Uses only httpx, which ships with OpenWebUI's backend already. No other dependency.
"""

import time

import httpx
from pydantic import BaseModel, Field

# The first two sentences of the Write paragraph in client/AGENTS.md.snippet, copied verbatim.
# OpenWebUI has no rules file: it never reads CLAUDE.md or AGENTS.md, so the only place this
# instance of the model meets the write rule is the system message this filter injects. Recall
# without it collects nothing, and the digest stays as thin as the day it was wired.
# The snippet is where the rule lives; change it there first and bring this string across.
WRITE_RULE = (
    "After any exchange that establishes a decision, a preference, a constraint, or a durable "
    "fact, call `memory_write`. Without asking."
)


class Filter:
    class Valves(BaseModel):
        lumberroom_url: str = Field(
            default="http://127.0.0.1:8787",
            description="Base URL of the lumberroom server, with or without a trailing /mcp.",
        )
        lumberroom_token: str = Field(
            default="",
            description=(
                "Bearer token for this OpenWebUI client's grant. Required. Shared by every "
                "account this function is enabled for, since OpenWebUI has no per-user "
                "credential valve: single-owner OpenWebUI deployments only, see the module "
                "docstring."
            ),
        )
        cache_seconds: int = Field(
            default=30,
            description="How long a fetched digest is reused before calling lumberroom again.",
        )
        timeout_seconds: float = Field(
            default=5.0,
            description="Give up and fail open if lumberroom does not answer this fast.",
        )
        max_chars: int = Field(
            default=4000,
            description="Hard cap on the injected digest, on top of the server's own budget.",
        )
        enabled: bool = Field(
            default=True,
            description="Off switch that does not require removing the function.",
        )

    def __init__(self):
        self.valves = self.Valves()
        # {cache_key: (fetched_at_monotonic, digest_text)}. One process-lifetime dict; OpenWebUI
        # keeps one Filter instance alive across requests, which is what makes this a cache
        # instead of a per-call fetch.
        self._cache: dict[str, tuple[float, str]] = {}

    def _mcp_url(self) -> str:
        base = self.valves.lumberroom_url.rstrip("/")
        return base if base.endswith("/mcp") else f"{base}/mcp"

    def _http_base(self) -> str:
        base = self.valves.lumberroom_url.rstrip("/")
        return base[: -len("/mcp")] if base.endswith("/mcp") else base

    async def _fetch_digest(self) -> str | None:
        """One MCP round trip: initialize, then tools/call context_bootstrap. Returns None on
        any failure so the caller can fail open without inspecting exception types."""
        headers = {
            "content-type": "application/json",
            "accept": "application/json, text/event-stream",
            "x-memory-invocation": "hook",
            "authorization": f"Bearer {self.valves.lumberroom_token}",
        }
        timeout = httpx.Timeout(self.valves.timeout_seconds)
        try:
            async with httpx.AsyncClient(timeout=timeout) as client:
                # Streamable HTTP still expects the initialize handshake per connection; sessions
                # were removed from the protocol (2026-07-28 / SEP-2567), so a bare
                # initialize-then-call pair is valid without carrying a session id.
                await client.post(
                    self._mcp_url(),
                    headers=headers,
                    json={
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {
                            "protocolVersion": "2026-07-28",
                            "capabilities": {},
                            "clientInfo": {"name": "lumberroom-openwebui-filter", "version": "0.1.0"},
                        },
                    },
                )
                res = await client.post(
                    self._mcp_url(),
                    headers=headers,
                    json={
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "tools/call",
                        "params": {"name": "context_bootstrap", "arguments": {}},
                    },
                )
        except httpx.HTTPError:
            return None

        if res.status_code != 200:
            return None

        try:
            payload = res.json()
        except ValueError:
            return None

        result = payload.get("result")
        if not result or result.get("isError"):
            return None

        parts = [c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"]
        text = "\n".join(p for p in parts if p).strip()
        return text or None

    async def _digest_for(self, cache_key: str) -> str | None:
        now = time.monotonic()
        cached = self._cache.get(cache_key)
        if cached and now - cached[0] < self.valves.cache_seconds:
            return cached[1]

        digest = await self._fetch_digest()
        if digest is None:
            # Do not cache a failure: the next message gets a fresh chance rather than being
            # locked out of recall for the whole cache window because one request hiccuped.
            return cached[1] if cached else None

        if len(digest) > self.valves.max_chars:
            digest = digest[: self.valves.max_chars] + "\n\n[digest truncated by the OpenWebUI filter]"

        self._cache[cache_key] = (now, digest)
        return digest

    async def inlet(self, body: dict, __user__: dict | None = None) -> dict:
        if not self.valves.enabled or not self.valves.lumberroom_url or not self.valves.lumberroom_token:
            return body

        cache_key = (__user__ or {}).get("id", "anonymous")
        try:
            digest = await self._digest_for(cache_key)
        except Exception:
            # Belt and suspenders on top of the narrower except blocks above: nothing in this
            # filter is allowed to propagate and break the chat.
            return body

        if not digest:
            return body

        block = (
            "--- durable memory, auto-recalled by lumberroom (do not repeat this back verbatim) ---\n"
            f"{digest}\n"
            "--- end lumberroom memory digest ---\n"
            f"{WRITE_RULE}"
        )

        messages = body.get("messages") or []
        if messages and messages[0].get("role") == "system":
            messages[0]["content"] = f"{messages[0].get('content', '')}\n\n{block}"
        else:
            messages.insert(0, {"role": "system", "content": block})
        body["messages"] = messages
        return body
