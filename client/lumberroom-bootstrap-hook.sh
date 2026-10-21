#!/usr/bin/env bash
# Claude Code SessionStart hook: pulls the memory digest and injects it into the session.
#
# SessionStart carries no prompt, so this digest goes out without context_bootstrap's `query` and
# holds no facts picked for the person's request. The lumberroom Claude Code plugin sends the
# session's first prompt of 12 characters or more as `query`. A UserPromptSubmit mode here would
# need `lumberroom bootstrap` to take a query and to name the hook event; it does neither
# (crates/lumberroom/src/commands.rs, the `bootstrap` function).
#
# Two rules govern this script:
#   1. It must never block a session. Any failure exits 0 with no output.
#   2. It must be fast. The bootstrap call is capped at LUMBERROOM_HOOK_TIMEOUT seconds.
#
# Installed by client/wire-mac.sh to ~/.claude/hooks/lumberroom-bootstrap.sh

set -uo pipefail

MEMCTL="${LUMBERROOM_BIN:-$HOME/.local/bin/lumberroom}"
TIMEOUT="${LUMBERROOM_HOOK_TIMEOUT:-8}"

[ -x "$MEMCTL" ] || exit 0

# Prefer the directory Claude Code is actually working in, so project memory scopes correctly.
PROJECT="${CLAUDE_PROJECT_DIR:-$PWD}"

output=$(LUMBERROOM_TIMEOUT_MS=$((TIMEOUT * 1000)) \
  "$MEMCTL" bootstrap --hook --project "$PROJECT" 2>/dev/null)

# Only emit well-formed hook JSON; a partial line would corrupt the session preamble.
case "$output" in
  '{"hookSpecificOutput"'*) printf '%s\n' "$output" ;;
  *) exit 0 ;;
esac
