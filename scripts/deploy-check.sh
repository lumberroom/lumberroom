#!/usr/bin/env bash
# Is a deployment actually working, or only answering health checks?
#
#   LUMBERROOM_TOKEN=... ./scripts/deploy-check.sh https://memory.example.com
#   LUMBERROOM_TOKEN=... ./scripts/deploy-check.sh --expect 5fbbb7a https://memory.example.com
#
# Read-only. It writes nothing to the store and needs no capability beyond the one every credential
# has. Run it after every deploy and after every change to PUBLIC_URL, AUTH_TOKENS or the image.
#
# Four failures on this box are invisible to `curl /healthz`, and each has cost real time:
#
#   rmcp validates the Host header against an allowlist derived from PUBLIC_URL. Left wrong, a
#   deployment answers every health check and metadata document while refusing every MCP request
#   with a 403 the client reports as a connection failure. Step 4 is the only one that catches it,
#   because it makes a real tool call rather than asking whether the port is open.
#
#   KEK_PROVIDER can be configured and the key still not be the one the existing rows were sealed
#   with. The server boots, serves, and refuses every private write. Step 2 reads the flag.
#
#   An issuer that disagrees with the host behind a reverse proxy stays invisible until a real
#   client's discovery fails. Step 6 compares the documents against the URL you asked about.
#
#   `docker restart` reuses the container's original image, and so does `docker compose up -d` in
#   some cases, so a rebuilt image sits on disk while the old binary keeps serving. Step 3 compares
#   the sha the running binary was built from against the one you expect, which defaults to HEAD in
#   this checkout.

set -euo pipefail

URL=""
# What the running server should have been built from. `--expect` wins over the environment, and
# both win over HEAD, so a run against a box built from a different tree can still say what it
# meant.
EXPECT_SHA="${LUMBERROOM_EXPECT_SHA:-}"
EXPECT_FROM="the environment"

usage() {
  echo "usage: LUMBERROOM_TOKEN=... $0 [--expect <sha>] https://memory.example.com" >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --expect)
      [ $# -ge 2 ] || { echo "--expect needs a sha" >&2; usage; exit 1; }
      EXPECT_SHA="$2"; EXPECT_FROM="--expect"; shift 2 ;;
    --expect=*)
      EXPECT_SHA="${1#--expect=}"; EXPECT_FROM="--expect"; shift ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "unknown option: $1" >&2; usage; exit 1 ;;
    *) URL="$1"; shift ;;
  esac
done

URL="${URL:-${LUMBERROOM_URL:-}}"
URL="${URL%/}"
TOKEN="${LUMBERROOM_TOKEN:-}"

# Resolved against this script rather than the caller's cwd. Somebody checking a remote box from
# their home directory should not pick up whatever repository they happen to be standing in, and on
# a server with no checkout there is nothing to read and step 3 says so.
if [ -z "$EXPECT_SHA" ]; then
  REPO_DIR="$(cd "$(dirname "$0")/.." 2>/dev/null && pwd || true)"
  if [ -n "$REPO_DIR" ] && command -v git >/dev/null 2>&1 \
     && git -C "$REPO_DIR" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    EXPECT_SHA="$(git -C "$REPO_DIR" rev-parse --short HEAD 2>/dev/null || true)"
    EXPECT_FROM="HEAD in $REPO_DIR"
  fi
fi

[ -n "$URL" ] || { usage; exit 1; }
command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }

PASSED=0
FAILED=0
WARNED=0
say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASSED=$((PASSED + 1)); }
warn() { printf '  \033[33mWARN\033[0m  %s\n' "$*"; WARNED=$((WARNED + 1)); }
fail() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAILED=$((FAILED + 1)); }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

get() { curl -sS -m 20 -o "$2" -w '%{http_code}' "$URL$1" 2>/dev/null || echo 000; }
auth_get() {
  curl -sS -m 20 -o "$2" -w '%{http_code}' -H "authorization: Bearer $TOKEN" "$URL$1" 2>/dev/null \
    || echo 000
}
# Reads one JSON field without needing jq, which a fresh VM does not have.
field() {
  python3 -c '
import json,sys
try: d=json.load(open(sys.argv[1]))
except Exception: sys.exit(1)
for k in sys.argv[2].split("."):
    if not isinstance(d, dict) or k not in d: sys.exit(1)
    d=d[k]
print(json.dumps(d) if isinstance(d,(dict,list,bool)) else d)
' "$1" "$2" 2>/dev/null
}

# Two forms of one commit. `git rev-parse --short` gives seven characters or more and a tag or a CI
# variable often carries the full forty, so compare by prefix in whichever direction is shorter.
# Seven is git's own floor for an abbreviation; below that a prefix match means nothing.
sha_matches() {
  a="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  b="$(printf '%s' "$2" | tr '[:upper:]' '[:lower:]')"
  [ "${#a}" -ge 7 ] && [ "${#b}" -ge 7 ] || return 1
  case "$a" in "$b"*) return 0 ;; esac
  case "$b" in "$a"*) return 0 ;; esac
  return 1
}

printf '\033[1mchecking %s\033[0m\n' "$URL"

say "1/7 the server answers"
if [ "$(get /healthz "$WORK/health.json")" = 200 ]; then
  pass "healthz: $(cat "$WORK/health.json")"
else
  fail "healthz did not answer 200. Nothing below will mean anything."
  printf '\ndeploy-check FAILED\n'; exit 1
fi

say "2/7 it is ready, and its key is the one this store was sealed with"
if [ "$(get /readyz "$WORK/ready.json")" = 200 ]; then
  pass "readyz: $(cat "$WORK/ready.json")"
  [ "$(field "$WORK/ready.json" ok)" = "true" ] \
    && pass "ok: true, so Postgres answers and the schema dimension matches (the embedder is not called)" \
    || fail "ok is not true"
  # `|| true` because `field` exits 1 on a key that is not there, and under `set -e` a bare
  # assignment from it ends the run right here with no message at all. An older server that
  # publishes no kek_verified used to kill the script mid-step.
  KEK="$(field "$WORK/ready.json" kek_provider || true)"
  VERIFIED="$(field "$WORK/ready.json" kek_verified || true)"
  if [ "$KEK" = "none" ]; then
    warn "KEK_PROVIDER=none, so every write that classifies private is refused rather than stored"
  elif [ "$VERIFIED" = "true" ]; then
    pass "kek_verified: the key matches the fingerprint this store recorded"
  else
    fail "KEK_PROVIDER=$KEK and kek_verified is false. The server is up and refusing every private \
write. Check the boot log for the fingerprint mismatch before writing anything."
  fi
else
  fail "readyz did not answer 200"
fi

say "3/7 the binary answering is the one you built"
RUNNING_SHA="$(field "$WORK/ready.json" build_sha || true)"
RUNNING_TAG="$(field "$WORK/ready.json" build_tag || true)"
RUNNING_AT="$(field "$WORK/ready.json" built_at || true)"
if [ -z "$RUNNING_SHA" ]; then
  warn "readyz published no build_sha: either it did not answer, or this server predates the \
stamp. Nothing here can tell a current container from one still running last week's image."
elif [ "$RUNNING_SHA" = unknown ]; then
  warn "the running binary reports build_sha=unknown, which is what a build that passed no stamp \
produces. Export LUMBERROOM_BUILD_SHA before 'docker compose build' and there will be something to compare."
elif [ -z "$EXPECT_SHA" ]; then
  warn "running $RUNNING_SHA, tag $RUNNING_TAG, built $RUNNING_AT. Nothing said what to expect: \
pass --expect <sha>, or run this from a checkout."
elif sha_matches "$EXPECT_SHA" "$RUNNING_SHA"; then
  pass "running $RUNNING_SHA, built $RUNNING_AT, which is what $EXPECT_FROM says it should be"
else
  fail "the server is running $RUNNING_SHA, built $RUNNING_AT, and $EXPECT_FROM says $EXPECT_SHA. \
A container keeps the image it was created from across 'docker restart' and often across \
'docker compose up -d', so a rebuilt image sits on disk unused. Recreate it: \
docker compose up -d --force-recreate server"
fi

say "4/7 a real MCP tool call, which is the check the others cannot make"
if [ -z "$TOKEN" ]; then
  warn "no LUMBERROOM_TOKEN, so the MCP endpoint went unchecked. This is the step that catches a \
deployment answering every health check and refusing every tool call."
else
  # Copied from lumberroom, which works against this server, rather than assembled from the
  # spec. An earlier version of this check invented the request and got two different 400s: rmcp
  # reads mcp-protocol-version as a stricter mode and then wants per-request _meta nobody sends.
  # Sessions left the protocol in the 2026-07-28 revision, so initialize-then-call on separate
  # connections is valid and is what every client here does.
  mcp() {
    curl -sS -m 30 -o "$2" -w '%{http_code}' \
      -H "authorization: Bearer $TOKEN" \
      -H 'content-type: application/json' \
      -H 'accept: application/json, text/event-stream' \
      -d "$1" "$URL/mcp" 2>/dev/null || echo 000
  }
  INIT_BODY='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"deploy-check","version":"0.1.0"}}}'
  INIT_STATUS="$(mcp "$INIT_BODY" "$WORK/init.json")"
  if [ "$INIT_STATUS" != 200 ]; then
    case "$INIT_STATUS" in
      403)
        fail "the MCP endpoint answered 403 while the health checks passed. This is the Host \
allowlist: rmcp derives it from PUBLIC_URL, so set PUBLIC_URL to the URL clients actually use and \
restart. A client reports this as a connection failure and never says why." ;;
      401)
        fail "the MCP endpoint refused this credential (401). Check AUTH_TOKENS on the box, and \
that sourcing .env through sh did not strip the quotes out of the JSON." ;;
      *)
        fail "initialize answered $INIT_STATUS: $(head -c 200 "$WORK/init.json" 2>/dev/null)" ;;
    esac
  else
    LIST_STATUS="$(mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' "$WORK/mcp.json")"
    if [ "$LIST_STATUS" = 200 ]; then
      TOOLS="$(grep -o '"name":"[a-z_]*"' "$WORK/mcp.json" 2>/dev/null | cut -d'"' -f4 | sort -u | tr '\n' ' ')"
      if [ -n "$TOOLS" ]; then
        pass "initialize and tools/list both answered: $TOOLS"
      else
        fail "tools/list answered 200 and named no tool: $(head -c 200 "$WORK/mcp.json")"
      fi
    else
      fail "initialize worked and tools/list answered $LIST_STATUS: $(head -c 200 "$WORK/mcp.json" 2>/dev/null)"
    fi
  fi
fi

say "5/7 the credential resolves to the grant you meant"
if [ -z "$TOKEN" ]; then
  warn "no LUMBERROOM_TOKEN, so the grant went unchecked"
elif [ "$(auth_get /admin/whoami "$WORK/who.json")" = 200 ]; then
  pass "whoami: client $(field "$WORK/who.json" client), mode $(field "$WORK/who.json" mode)"
  for flag in may_delete may_ingest may_read_history registry_write; do
    printf '        %-18s %s\n' "$flag" "$(field "$WORK/who.json" "$flag")"
  done
  pass "every capability that gates a tool is reported, so a missing tool has a named cause"
else
  fail "whoami did not answer 200 for this credential"
fi

say "6/7 the issuer agrees with the host clients reach"
META="$(get /.well-known/oauth-authorization-server "$WORK/as.json")"
if [ "$META" = 404 ]; then
  pass "no authorization server published, which is right for token mode"
elif [ "$META" = 200 ]; then
  ISSUER="$(field "$WORK/as.json" issuer)"
  if [ "${ISSUER%/}" = "$URL" ]; then
    pass "issuer $ISSUER matches the URL clients use"
  else
    fail "issuer is $ISSUER and clients reach $URL. Discovery fails at the client with no error \
here. PUBLIC_URL is the single source for this; set it and restart."
  fi
  RES="$(get /.well-known/oauth-protected-resource "$WORK/pr.json")"
  [ "$RES" = 200 ] && pass "the protected-resource document is published too" \
    || fail "the protected-resource document answered $RES, so a client cannot discover this server"
else
  fail "the authorization server metadata answered $META"
fi

say "7/7 the transport is one a browser surface will accept"
case "$URL" in
  https://*) pass "https, which every hosted surface requires" ;;
  http://127.0.0.1*|http://localhost*)
    warn "loopback. Fine for the CLI and for Claude Code on this machine, and no hosted surface \
will connect to it: those need https and a public name." ;;
  *) fail "plain http on a non-loopback address. Credentials cross the network in clear, and no \
hosted surface will connect." ;;
esac

printf '\n\033[1m%s\033[0m\n' "result"
printf '  %s passed, %s warned, %s failed\n' "$PASSED" "$WARNED" "$FAILED"
if [ "$FAILED" -ne 0 ]; then
  printf '\n\033[31mdeploy-check FAILED\033[0m\n'
  exit 1
fi
[ "$WARNED" -eq 0 ] || printf '\n  warnings are things to know, not things that are broken.\n'
printf '\n  \033[32mdeploy-check PASSED\033[0m\n'
