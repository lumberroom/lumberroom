# 32. Large read answers stay inline

**Date:** 13 October 2026 · **Status:** accepted, implemented; full suite ran with test-fast, 1,782
passed and 0 failed; not merged · **Decided by:** the owner

## Decision

Six read tools declare `_meta: {"anthropic/maxResultSizeChars": 500000}` on their `tools/list`
entry: `context_bootstrap`, `memory_search`, `memory_history`, `registry_history`, `alias_list` and
`review_queue`. The value is `MAX_RESULT_CHARS` in `src/mcp/mod.rs`, and each tool sets it through
the `meta` argument of rmcp's `#[tool]` attribute, so `tool_definitions()` and the served list carry
the same entry.

`registry_get` answers one key and declares nothing. No write declares anything: each answers with
one row or one outcome.

The value is the client's ceiling, because every lower figure is one a declared read can pass.
`review_queue` pages up to 200 items per source and a conflict item carries two rows, so a page of
2,000-character rows holds 800,000 characters of fact text before any field name. `memory_history`
walks up to 64 versions in each direction, and `alias_list` has no row limit. `context_bootstrap`'s
structured copy carries the full digest, whose rows run up to the write cap, so it can pass 50,000
characters too.

## Context

Claude Code saves a successful MCP result with no image content to a file once its text passes
50,000 characters, and hands the model a short stub with a path. A tool raises its own threshold by
declaring `anthropic/maxResultSizeChars` in `_meta` on its `tools/list` entry, up to a ceiling of
500,000 ([code.claude.com/docs/en/mcp](https://code.claude.com/docs/en/mcp)).

An agent replay on a real store found a limit-50 `memory_search` answer runs about 50 to 58 KB.
Claude Code saved it to a file, and the agent worked around the stub with narrower searches: more
calls for an answer the server had already sent. An agent replay with the declaration in place saw
no answer saved to a file.

The MCP schema types a tool's `_meta` as an open map (`{ [key: string]: unknown }`). The spec
reserves only prefixes that carry `mcp` or `modelcontextprotocol` as a label followed by another,
such as `mcp.dev/`, so `anthropic/` sits outside the reserved space.

## What lost

**`MAX_MCP_OUTPUT_TOKENS`.** It sets the client's token limit and leaves the 50,000-character file
threshold where it was. A probe of Claude Code 2.1.296 stubbed the same search with and without it.

**`ENABLE_MCP_LARGE_OUTPUT_FILES=false`.** It does not appear in Claude Code's published MCP or
environment-variable docs. The client's code reads as stopping the file and truncating the answer
instead; nobody here ran it. If it truncates, the lowest-ranked search hits
and the last items on a `review_queue` page are the ones cut, and an agent cannot decide an item
it never read. It is also a setting on each person's client, which the
server cannot set.

**Shrinking every answer below 50,000 characters.** That caps `limit`. At about 1,000 to 1,200
characters per hit, the threshold falls at 41 to 50 hits, so `memory_search` could not promise its
limit of 50, and `review_queue` would page in tens where
it pages in hundreds. A caller who asks for 50 hits would get fewer than they asked for, to suit
one client's file threshold.

**A smaller figure per tool.** Each tool's largest answer depends on settings
(`SEARCH_MAX_LIMIT`, `WRITE_MAX_CONTENT_CHARS`) and on rows stored before the current length cap. A
per-tool figure would be a guess that a setting change breaks without a test noticing.

## What it costs, accepted

- A large answer sits whole in the model's context, and every later turn pays for it. The owner
  chose that over a stub the agent has to search around.
- The declaration also lifts Claude Code's 25,000-token cap for text, so one 500,000-character
  answer, about 125,000 tokens, can sit inline. A reviewer of this change dissented on that point,
  and the owner accepted the cost. A caller who wants less passes a smaller limit.
- An answer that grows through a bug reaches the model whole up to 500,000 characters, where at
  50,000 it would have gone to a file.
- The key names one vendor's client in the engine's `tools/list`. It is one entry in `_meta`, and
  a test keeps it the only one.

## What this is not for

It does not change any answer. Limits, rankings and shapes stay where they were.

It is not a reason to raise a tool's limit or to let an answer grow. An answer past 500,000
characters still goes to a file.

It does not cover images. Claude Code holds image results to `MAX_MCP_OUTPUT_TOKENS`, and no tool
here returns one.

## Reversal condition

Drop a tool's declaration if a replay shows agents doing worse with that tool's large answers inline
than with the stub: more turns to finish, or answers that ignore the hits past the first screen.
Drop all of them if Claude Code raises its default threshold above the largest answer these tools
return at their default limits, or removes the key.
