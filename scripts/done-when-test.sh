#!/usr/bin/env bash
# The done-when test from PRD §1, run against a live deployment.
#
#   LUMBERROOM_URL=https://memory.example.com LUMBERROOM_TOKEN=... ./scripts/done-when-test.sh
#
# It proves the loop end to end with the real Claude Code client:
#   Session A  a fact is stated in conversation; the model writes it to memory on its own
#   Session B  a fresh session, no mention of the fact; the SessionStart hook injects the digest
#              and the model answers from memory
#
# Nothing in your ~/.claude is touched: the MCP server and the SessionStart hook are supplied
# per-invocation with --mcp-config and --settings.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
URL="${LUMBERROOM_URL:-http://127.0.0.1:8787}"
TOKEN="${LUMBERROOM_TOKEN:-}"
MODEL="${DONE_WHEN_MODEL:-sonnet}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
# Both sessions run from a directory named for the project the fact is about. The hook sends the
# directory as the project, so session B's digest carries project:lumberroom, where session A
# usually files the fact. A query-less digest prints other projects only as a count.
SESSION_DIR="$WORK/lumberroom"
mkdir -p "$SESSION_DIR"

# AUTH_TOKENS holds either `client:token` pairs or JSON. Handle both.
if [ -z "$TOKEN" ] && [ -f "$REPO_DIR/.env" ]; then
  LINE="$(grep -E '^AUTH_TOKENS=' "$REPO_DIR/.env" | head -1 | cut -d= -f2-)"
  case "$LINE" in
    '['*|'{'*)
      TOKEN="$(printf '%s' "$LINE" | node -e '
        let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>{
          const g=JSON.parse(s); const list=Array.isArray(g)?g:[g];
          const hit=list.find(x=>x.token); process.stdout.write(hit?hit.token:"");
        })')" ;;
    *:*) TOKEN="${LINE%%,*}"; TOKEN="${TOKEN#*:}" ;;
  esac
fi
[ -n "$TOKEN" ] || { echo "set LUMBERROOM_TOKEN" >&2; exit 1; }
command -v claude >/dev/null 2>&1 || { echo "the claude CLI is required" >&2; exit 1; }

MCP_URL="$URL"; case "$MCP_URL" in */mcp) ;; *) MCP_URL="$URL/mcp" ;; esac
export LUMBERROOM_URL="$MCP_URL" LUMBERROOM_TOKEN="$TOKEN"
# The client under test is the shipped binary, taken from PATH.
#
# This used to be `node bin/lumberroom.mjs`, a second client written against node built-ins alone.
# It shared no types with the server and so could not be accidentally accommodated by it, which
# earned it a place here. It also fell behind: it never learned to read an authorization server's
# metadata document, so it could not log in against a deployment whose issuer differs from its API
# base, and a gate driving a client that cannot log in proves less than it looks like it does.
MEMCTL="lumberroom"

# A nonsense token makes retrieval provable: no model can produce this from prior knowledge or
# by reading the repo, so answering the question is evidence of memory, not of guessing.
# The fact is shaped as a stated preference, which is what this system is for. A fact shaped like
# an authorization claim ("X is signed off") makes a careful model refuse to record it on a bare
# assertion. That is correct behaviour, and worth knowing before you wonder why nothing was written.
NONCE="$(head -c 4 /dev/urandom | od -An -tx1 | tr -d ' \n')"
FACT="I want the internal nickname for the lumberroom project to be QUARTZLARK-$NONCE. Use it in commit messages and status notes"
QUESTION="What internal nickname do I use for the lumberroom project?"

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

# Repeated runs leave one nickname fact per run behind, and Phase 1 does not consolidate
# (PRD §5: supersedes is recorded, not acted on). --cleanup drops them first, for local runs.
if [ "${1:-}" = "--cleanup" ]; then
  say "cleanup"
  if docker compose -f "$REPO_DIR/docker-compose.yml" exec -T db \
       psql -U "${POSTGRES_USER:-lumberroom}" -d "${POSTGRES_DB:-lumberroom}" \
       -c "DELETE FROM memory WHERE content LIKE '%QUARTZLARK-%'" 2>/dev/null; then
    echo "  removed nickname facts from earlier runs"
  else
    echo "  no local database reachable; leaving earlier runs in place"
  fi
fi

say "0/4 preflight"
$MEMCTL doctor >/dev/null || { echo "server not reachable at $MCP_URL" >&2; exit 1; }
echo "  endpoint $MCP_URL is healthy"

cat > "$WORK/mcp.json" <<JSON
{"mcpServers":{"lumberroom":{"type":"http","url":"$MCP_URL","headers":{"Authorization":"Bearer $TOKEN"}}}}
JSON

# The SessionStart hook, supplied per-invocation. This is the same script wire-mac.sh installs.
cat > "$WORK/settings.json" <<JSON
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "LUMBERROOM_BIN=$(command -v lumberroom) LUMBERROOM_URL=$MCP_URL LUMBERROOM_TOKEN=$TOKEN bash $REPO_DIR/client/lumberroom-bootstrap-hook.sh",
            "timeout": 15
          }
        ]
      }
    ]
  }
}
JSON

WRITE_RULE="$(cat "$REPO_DIR/client/CLAUDE.md.snippet")"

say "1/4 session A: state the fact, expect an unprompted write"
echo "  fact: $FACT"
# toolStats groups by (tool, client), so filter by client. Without it, a second client's row can
# satisfy the delta and the test passes on a write that never came from the surface under test.
CLIENT="${DONE_WHEN_CLIENT:-claude-code-mac}"
unprompted_writes() {
  $MEMCTL stats --hours 1 --json | CLIENT="$CLIENT" node -e '
    let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>{
      const j=JSON.parse(s), c=process.env.CLIENT;
      const rows=j.by_tool.filter(t=>t.tool==="memory_write" && t.client===c);
      console.log(rows.reduce((n,r)=>n+r.unprompted,0));
    })'
}
BEFORE="$(unprompted_writes)"

cd "$SESSION_DIR"
claude -p "$FACT. That is settled." \
  --mcp-config "$WORK/mcp.json" --strict-mcp-config \
  --append-system-prompt "$WRITE_RULE" \
  --allowedTools "mcp__lumberroom__memory_write,mcp__lumberroom__memory_search,mcp__lumberroom__context_bootstrap" \
  --disallowedTools "Read,Write,Edit,Grep,Glob,Bash,WebSearch,WebFetch" \
  --model "$MODEL" < /dev/null > "$WORK/a.txt" 2>&1 || true
sed 's/^/  A: /' "$WORK/a.txt" | head -6

sleep 2
AFTER="$(unprompted_writes)"

say "2/4 did the fact land?"
if $MEMCTL search "$QUESTION" --limit 5 | grep -q "QUARTZLARK-$NONCE"; then
  echo "  PASS  the fact is in the store and retrievable"
else
  echo "  FAIL  the model did not write the fact"
  $MEMCTL search "$QUESTION" --limit 5 | sed 's/^/        /'
  exit 1
fi
if [ "$AFTER" -gt "$BEFORE" ]; then
  echo "  PASS  the write was unprompted (memory_write unprompted count $BEFORE -> $AFTER)"
else
  echo "  WARN  the write was recorded as prompted, not model-initiated"
fi
FILED="$($MEMCTL search "$QUESTION" --limit 5 --json | NONCE="$NONCE" node -e '
  let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>{
    const hit=(JSON.parse(s).hits||[]).find(h=>h.content.includes("QUARTZLARK-"+process.env.NONCE));
    process.stdout.write(hit ? `${hit.namespace} [${(hit.tags||[]).join(",")}]` : "unknown");
  })')"
echo "  filed in $FILED"
case "$FILED" in
  "project:lumberroom "*) ;;
  "user:me ["*preference*|"user:me ["*rule*|"user:me ["*convention*) ;;
  *) echo "  WARN  a query-less digest for project:lumberroom prints that project's rows and the newest rule-tagged user:me rows; session B sees a fact filed here only as a count" ;;
esac

say "3/4 session B: fresh session, the question never mentions the fact"
echo "  question: $QUESTION"
claude -p "$QUESTION Answer from what you already know." \
  --mcp-config "$WORK/mcp.json" --strict-mcp-config \
  --settings "$WORK/settings.json" \
  --allowedTools "mcp__lumberroom__context_bootstrap,mcp__lumberroom__memory_search,mcp__lumberroom__registry_get" \
  --disallowedTools "Read,Write,Edit,Grep,Glob,Bash,WebSearch,WebFetch" \
  --model "$MODEL" < /dev/null > "$WORK/b.txt" 2>&1 || true
sed 's/^/  B: /' "$WORK/b.txt" | head -8

# The verdict greps for the nonce. A session B that surfaces the string while refusing to trust
# it still counts as PASS here, which is why the transcript is printed above: read what B
# actually said before believing the green line.
say "4/4 verdict"
if grep -qi "QUARTZLARK-$NONCE" "$WORK/b.txt"; then
  echo "  PASS  a fresh session recovered the fact without being told it"
  echo ""
  echo "  done-when test PASSED"
  $MEMCTL stats --hours 1 | sed 's/^/  /'
  exit 0
fi
echo "  FAIL  the fresh session did not surface the fact"
echo ""
echo "  session B transcript:"
sed 's/^/    /' "$WORK/b.txt"
exit 1
