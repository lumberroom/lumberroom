#!/usr/bin/env bash
# Proves the built-in OAuth 2.1 + PKCE authorization server end to end, with no browser and no
# loopback listener: the authorization code arrives in a `Location` header this script reads
# directly, never in a request a browser would have made.
#
#   ./scripts/oauth-flow-test.sh                    its own server, its own database
#   ./scripts/oauth-flow-test.sh --port 8888 --keep
#
# Stands up a scratch server on --port (default 8793, never 8787) in AUTH_MODE=oauth, against a
# database named lumberroom_oauth_flow_test, with an owner password minted fresh for this run, and drops
# both when it exits. This gate has no --live mode: step 4 self-registers a client on every run, and
# there is no HTTP route that deletes one, only scripts/purge-oauth-flow-test-clients.sh reading rows
# straight out of the database. A run against 127.0.0.1:8787 on 19 August 2026 left three
# oauth-flow-test-* rows in the owner's live oauth_client table this way, permanently, because the
# earlier version of this script had no store of its own. A gate with no API-level teardown needs its
# own store unconditionally, not an opt-in one.
#
# Follows the house pattern in scripts/done-when-test.sh and the scratch-server shape in
# scripts/policy-test.sh, scripts/correction-test.sh and scripts/cleanup-test.sh: bash, set -euo
# pipefail, curl and node only, a nonce per run, coloured PASS/FAIL lines, non-zero exit on any
# failure, a summary at the end.
#
# What this does NOT prove: that Claude.ai's or ChatGPT's actual client code completes this flow.
# Phase 2 spec is explicit that Claude Code's own fallback probing masks a whole class of bug that
# only shows up against the real browser surfaces. This script exercises the wire protocol those
# clients depend on; it is not a substitute for testing against them.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRATCH_PORT=8793
SCRATCH_KEEP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --port) SCRATCH_PORT="$2"; shift 2 ;;
    --keep) SCRATCH_KEEP=1; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1. This gate takes no positional URL; it never runs against a \
store you name. See --help." >&2; exit 1 ;;
  esac
done

command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
command -v node >/dev/null 2>&1 || { echo "node is required" >&2; exit 1; }
command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
command -v openssl >/dev/null 2>&1 || { echo "openssl is required" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

SCRATCH_DB=lumberroom_oauth_flow_test
SCRATCH_NAME="${LUMBERROOM_OAUTH_FLOW_TEST_SERVER:-lumberroom-oauth-flow-test-server}"
# AUTH_MODE=oauth needs no static bearer grant. scratch_require only checks this is non-empty; an
# empty array is never sent to the container, since this run's whole point is exercising the
# built-in authorization server instead.
SCRATCH_TOKENS='[]'
export SCRATCH_DB SCRATCH_NAME SCRATCH_TOKENS SCRATCH_PORT SCRATCH_KEEP
# shellcheck source=lib/scratch-server.sh
. "$REPO_DIR/scripts/lib/scratch-server.sh"

# A fresh owner password every run, hashed inside the already-built image rather than here: argon2
# is not in Node's built-ins, which is the whole reason `lumberroom hash-password` exists (the old JS client's
# own hash-password command just prints this same docker command instead of computing a weaker hash
# itself). Held only in $PASSWORD and the container's own environment, never written to disk.
PASSWORD="$(openssl rand -hex 20)"

# scratch_start (scratch-server.sh) pins PUBLIC_URL to the container name for rmcp's Host allowlist,
# which AUTH_MODE=oauth's own boot check (src/config.rs) refuses: it accepts only https or
# 127.0.0.1/localhost, because a browser MCP client needs https and an owner password must not cross
# the network in the clear. This script's curl runs from the host either way, so 127.0.0.1 satisfies
# both checks at once. Everything else about bring-up matches scratch_start; this is a copy with
# that one substitution and the oauth-specific environment, not a reimplementation.
scratch_start_oauth() {
  SCRATCH_REPO_DIR="${SCRATCH_REPO_DIR:-$REPO_DIR}"
  SCRATCH_NETWORK="${LUMBERROOM_DOCKER_NETWORK:-lumberroom_default}"
  SCRATCH_PG_USER="${POSTGRES_USER:-lumberroom}"
  scratch_require || return 1

  echo "bringing up the compose database (reusing it if already running)..."
  scratch_compose up -d db >/dev/null
  local i=0
  until scratch_compose exec -T db pg_isready -U "$SCRATCH_PG_USER" >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -ge 60 ] && { echo "postgres did not become ready within 60s" >&2; return 1; }
    sleep 1
  done

  # Dropped and recreated, so a run never inherits the oauth_client rows the run before it left.
  scratch_compose exec -T -e PGOPTIONS="-c client_min_messages=warning" db \
    psql -U "$SCRATCH_PG_USER" -d postgres -c "DROP DATABASE IF EXISTS $SCRATCH_DB" >/dev/null
  scratch_compose exec -T db \
    psql -U "$SCRATCH_PG_USER" -d postgres -c "CREATE DATABASE $SCRATCH_DB" >/dev/null

  local owner_password_hash
  owner_password_hash="$(printf '%s\n' "$PASSWORD" | docker run --rm -i lumberroom-server:0.4.0 lumberroom-server hash-password \
    2>"$WORK/hash-password.err")" || {
    echo "could not hash the scratch owner password: $(cat "$WORK/hash-password.err")" >&2
    return 1
  }
  local oauth_cookie_secret
  oauth_cookie_secret="$(openssl rand -hex 32)"

  docker rm -f "$SCRATCH_NAME" >/dev/null 2>&1 || true
  echo "starting the scratch oauth server on port $SCRATCH_PORT against database $SCRATCH_DB..."
  docker run -d --name "$SCRATCH_NAME" --network "$SCRATCH_NETWORK" \
    -p "127.0.0.1:${SCRATCH_PORT}:${SCRATCH_PORT}" \
    -e PORT="$SCRATCH_PORT" \
    -e HOST=0.0.0.0 \
    -e TENANT_ID=scratch \
    -e DATABASE_URL="postgres://${SCRATCH_PG_USER}:${POSTGRES_PASSWORD}@db:5432/${SCRATCH_DB}" \
    -e PUBLIC_URL="http://127.0.0.1:${SCRATCH_PORT}" \
    -e AUTH_MODE=oauth \
    -e OWNER_PASSWORD_HASH="$owner_password_hash" \
    -e OAUTH_COOKIE_SECRET="$oauth_cookie_secret" \
    -e EMBED_PROVIDER=hash -e EMBED_DIM=768 \
    -e KEK_PROVIDER=none \
    lumberroom-server:0.4.0 >/dev/null

  i=0
  until curl -sf "http://127.0.0.1:${SCRATCH_PORT}/readyz" >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -ge 90 ]; then
      echo "the scratch oauth server did not become ready within 180s. Last log lines:" >&2
      docker logs --tail 40 "$SCRATCH_NAME" >&2 || true
      return 1
    fi
    sleep 2
  done

  SCRATCH_URL="http://127.0.0.1:${SCRATCH_PORT}"
  export SCRATCH_URL
}

trap 'status=$?; scratch_stop; rm -rf "$WORK"; exit $status' EXIT INT TERM
scratch_start_oauth || exit 1
URL="$SCRATCH_URL"
MCP_URL="$URL/mcp"

NONCE="$(head -c 6 /dev/urandom | od -An -tx1 | tr -d ' \n')"

FAILED=0
say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m  %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAILED=1; }
die() { fail "$*"; printf '\noauth-flow-test FAILED\n'; exit 1; }

# ---- tiny HTTP helper. Writes headers to $WORK/headers, body to $WORK/body, status to $STATUS ----
STATUS=""
http() {
  # http METHOD PATH [-- curl-args...]
  local method="$1" path="$2"
  shift 2
  : >"$WORK/body"; : >"$WORK/headers"
  STATUS="$(curl -sS -o "$WORK/body" -D "$WORK/headers" -w '%{http_code}' -X "$method" "$@" "$URL$path")" \
    || STATUS="000"
}

header() {
  # header NAME: last matching response header, case-insensitive, CR stripped.
  awk -v n="$1" 'BEGIN{IGNORECASE=1} tolower($0) ~ ("^"tolower(n)":") {
    sub(/^[^:]*:[ \t]*/, ""); sub(/\r$/, ""); v=$0
  } END{print v}' "$WORK/headers"
}

body() { cat "$WORK/body"; }

json_field() {
  # json_field PATH: dotted path into $WORK/body, e.g. "access_token" or "error"
  node -e '
    const fs = require("node:fs");
    let j; try { j = JSON.parse(fs.readFileSync(process.argv[1], "utf8")); } catch { process.exit(1); }
    let v = j;
    for (const p of process.argv[2].split(".")) { if (v == null) { v = undefined; break; } v = v[p]; }
    if (v === undefined || v === null) process.exit(1);
    process.stdout.write(typeof v === "string" ? v : JSON.stringify(v));
  ' "$WORK/body" "$1" 2>/dev/null
}

pkce() {
  node -e '
    const crypto = require("node:crypto");
    const verifier = crypto.randomBytes(32).toString("base64url");
    const challenge = crypto.createHash("sha256").update(verifier).digest("base64url");
    process.stdout.write(verifier + " " + challenge);
  '
}

url_param() {
  # url_param URL NAME: a query parameter out of an absolute URL, via node's URL, not regex.
  node -e '
    const u = new URL(process.argv[1]);
    process.stdout.write(u.searchParams.get(process.argv[2]) || "");
  ' "$1" "$2"
}

MCP_INIT_BODY='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"oauth-flow-test","version":"0.1.0"}}}'
mcp_call() {
  # mcp_call TOKEN JSON_BODY
  http POST /mcp \
    -H 'content-type: application/json' \
    -H 'accept: application/json, text/event-stream' \
    -H "authorization: Bearer $1" \
    --data "$2"
}

mcp_result_field() {
  # Streamable HTTP replies with plain JSON here (json_response=true server-side) or, tolerated,
  # a single SSE frame. Either way this pulls .result.<path> out of it.
  node -e '
    const fs = require("node:fs");
    const raw = fs.readFileSync(process.argv[1], "utf8");
    const type = process.argv[2];
    let text = raw;
    if (type.includes("text/event-stream")) {
      const lines = raw.split("\n").filter(l => l.startsWith("data:")).map(l => l.slice(5).trim());
      text = lines[lines.length - 1] || "";
    }
    let j; try { j = JSON.parse(text); } catch { process.exit(1); }
    if (j.error) { process.stderr.write(JSON.stringify(j.error)); process.exit(2); }
    let v = j.result;
    for (const p of process.argv[3].split(".")) { if (v == null) { v = undefined; break; } v = v[p]; }
    if (v === undefined) process.exit(1);
    process.stdout.write(typeof v === "string" ? v : JSON.stringify(v));
  ' "$WORK/body" "$(header content-type)" "$1"
}

REDIRECT_URI="http://127.0.0.1:$((30000 + 0x${NONCE:0:4} % 20000))/callback"

say "1/13 protected-resource metadata, both paths"
http GET /.well-known/oauth-protected-resource
if [ "$STATUS" = 200 ] && [ -n "$(json_field authorization_servers.0)" ]; then
  pass "GET /.well-known/oauth-protected-resource names an authorization server"
else
  die "GET /.well-known/oauth-protected-resource did not name an authorization server (status $STATUS): $(body | head -c 300)"
fi
http GET /.well-known/oauth-protected-resource/mcp
if [ "$STATUS" = 200 ] && [ -n "$(json_field authorization_servers.0)" ]; then
  pass "GET /.well-known/oauth-protected-resource/mcp answers too (real clients check both)"
else
  die "the path-suffixed variant did not answer (status $STATUS)"
fi

say "2/13 an unauthenticated call to /mcp is a 401 with a WWW-Authenticate pointer, not a 200"
http POST /mcp -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' --data "$MCP_INIT_BODY"
WWW="$(header WWW-Authenticate)"
if [ "$STATUS" = 401 ]; then
  pass "unauthenticated POST /mcp is 401, not a 200 with an error body"
else
  die "unauthenticated POST /mcp returned $STATUS, not 401. A 200 here is silently ignored by hosted Claude clients."
fi
if printf '%s' "$WWW" | grep -q 'resource_metadata='; then
  pass "WWW-Authenticate carries resource_metadata: $WWW"
else
  die "WWW-Authenticate is missing resource_metadata: got [$WWW]"
fi

say "3/13 authorization-server metadata advertises S256 and refuses to offer plain"
http GET /.well-known/oauth-authorization-server
METHODS="$(json_field code_challenge_methods_supported || true)"
if [ "$STATUS" = 200 ] && printf '%s' "$METHODS" | grep -q 'S256'; then
  pass "code_challenge_methods_supported contains S256"
else
  die "code_challenge_methods_supported did not contain S256 (status $STATUS): $METHODS"
fi
if ! printf '%s' "$METHODS" | grep -q 'plain'; then
  pass "code_challenge_methods_supported does not offer plain"
else
  die "code_challenge_methods_supported offers plain, which lets a client downgrade PKCE: $METHODS"
fi

say "4/13 dynamic client registration"
http POST /oauth/register \
  -H 'content-type: application/json' \
  --data "$(node -e '
    process.stdout.write(JSON.stringify({
      client_name: "oauth-flow-test-" + process.argv[1],
      redirect_uris: [process.argv[2]],
      grant_types: ["authorization_code", "refresh_token"],
      response_types: ["code"],
      token_endpoint_auth_method: "none",
      software_id: "lumberroom-oauth-flow-test",
    }));
  ' "$NONCE" "$REDIRECT_URI")"
if [ "$STATUS" = 404 ]; then
  die "server has no /oauth/register: it is not running in oauth or oidc mode"
fi
CLIENT_ID="$(json_field client_id || true)"
CLIENT_SECRET="$(json_field client_secret || true)"
SECRET_ARGS=()
if [ -n "$CLIENT_SECRET" ]; then SECRET_ARGS=(--data-urlencode "client_secret=$CLIENT_SECRET"); fi
# Every expansion below uses ${SECRET_ARGS[@]+"${SECRET_ARGS[@]}"}, not a bare "${SECRET_ARGS[@]}".
# Under `set -u`, bash 3.2 (what macOS ships as /bin/bash) treats an EMPTY array as unset and
# aborts with "SECRET_ARGS[@]: unbound variable". A public client gets no client_secret, so the
# array is empty on the exact path this script is built to exercise.
if [ "$STATUS" -lt 300 ] && [ -n "$CLIENT_ID" ]; then
  pass "POST /oauth/register returned a client_id ($CLIENT_ID)"
else
  die "registration failed (status $STATUS): $(body | head -c 300)"
fi

# One full authorize -> login -> consent round trip, given a PKCE challenge and a state nonce.
# Sets LAST_CODE and LAST_STATE. Never follows the consent redirect: the redirect target is a
# loopback port nothing is listening on, and the code is read straight out of the Location header.
LAST_CODE=""
LAST_STATE=""
authorize_login_consent() {
  local challenge="$1" state="$2" label="$3"

  http GET /oauth/authorize \
    -G \
    --data-urlencode "response_type=code" \
    --data-urlencode "client_id=$CLIENT_ID" \
    --data-urlencode "redirect_uri=$REDIRECT_URI" \
    --data-urlencode "code_challenge=$challenge" \
    --data-urlencode "code_challenge_method=S256" \
    --data-urlencode "scope=memory.read memory.write" \
    --data-urlencode "resource=$MCP_URL" \
    --data-urlencode "state=$state"
  if [ "$STATUS" = 200 ] && grep -q 'action="/oauth/login"' "$WORK/body"; then
    pass "$label: GET /oauth/authorize renders a login page, not a redirect"
  else
    die "$label: GET /oauth/authorize did not render a login page (status $STATUS)"
  fi

  http POST /oauth/login \
    --data-urlencode "client_id=$CLIENT_ID" \
    --data-urlencode "redirect_uri=$REDIRECT_URI" \
    --data-urlencode "code_challenge=$challenge" \
    --data-urlencode "code_challenge_method=S256" \
    --data-urlencode "response_type=code" \
    --data-urlencode "state=$state" \
    --data-urlencode "resource=$MCP_URL" \
    --data-urlencode "password=$PASSWORD"
  local cookie_line cookie
  cookie_line="$(header Set-Cookie)"
  cookie="$(printf '%s' "$cookie_line" | sed -n 's/^lumberroom_owner=\([^;]*\).*/lumberroom_owner=\1/p')"
  if [ "$STATUS" -lt 400 ] && [ -n "$cookie" ]; then
    pass "$label: POST /oauth/login with the owner password sets the session cookie"
  else
    die "$label: login did not set a session cookie (status $STATUS): $(body | head -c 300)"
  fi

  local csrf
  csrf="$(grep -o 'name="csrf" value="[^"]*"' "$WORK/body" | sed 's/.*value="//; s/"$//' || true)"
  if [ -z "$csrf" ]; then
    # The login POST may have redirected instead of rendering consent inline. Fall back to
    # re-fetching /oauth/authorize with the now-live cookie and scrape the consent page there.
    http GET /oauth/authorize \
      -G \
      -H "Cookie: $cookie" \
      --data-urlencode "response_type=code" \
      --data-urlencode "client_id=$CLIENT_ID" \
      --data-urlencode "redirect_uri=$REDIRECT_URI" \
      --data-urlencode "code_challenge=$challenge" \
      --data-urlencode "code_challenge_method=S256" \
      --data-urlencode "scope=memory.read memory.write" \
      --data-urlencode "resource=$MCP_URL" \
      --data-urlencode "state=$state"
    csrf="$(grep -o 'name="csrf" value="[^"]*"' "$WORK/body" | sed 's/.*value="//; s/"$//' || true)"
  fi
  if [ -n "$csrf" ]; then
    pass "$label: a consent screen with a CSRF token is reachable"
  else
    die "$label: no consent screen with a csrf field was found after login"
  fi

  http POST /oauth/consent \
    -H "Cookie: $cookie" \
    --data-urlencode "client_id=$CLIENT_ID" \
    --data-urlencode "redirect_uri=$REDIRECT_URI" \
    --data-urlencode "code_challenge=$challenge" \
    --data-urlencode "code_challenge_method=S256" \
    --data-urlencode "response_type=code" \
    --data-urlencode "state=$state" \
    --data-urlencode "resource=$MCP_URL" \
    --data-urlencode "csrf=$csrf" \
    --data-urlencode "profile=full" \
    --data-urlencode "action=allow"
  local location
  location="$(header Location)"
  if [ "$STATUS" -lt 400 ] && [ -n "$location" ]; then
    pass "$label: consent redirects back to the client with a Location header"
  else
    die "$label: consent did not redirect (status $STATUS): $(body | head -c 300)"
  fi

  LAST_CODE="$(url_param "$location" code)"
  LAST_STATE="$(url_param "$location" state)"
  if [ -n "$LAST_CODE" ]; then
    pass "$label: an authorization code came back in the Location header"
  else
    die "$label: no code= in the Location header: $location"
  fi
  if [ "$LAST_STATE" = "$state" ]; then
    pass "$label: state is echoed back unchanged"
  else
    die "$label: state was not echoed. sent [$state] got [$LAST_STATE]"
  fi
}

say "5/13 authorize, sign in, and consent: the code arrives in a redirect, never a browser"
read -r VERIFIER CHALLENGE <<EOF
$(pkce)
EOF
STATE="run-a-$NONCE"
authorize_login_consent "$CHALLENGE" "$STATE" "flow A"
CODE_A="$LAST_CODE"

say "6/13 the token exchange, form encoded"
http POST /oauth/token \
  -H 'content-type: application/x-www-form-urlencoded' \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "code=$CODE_A" \
  --data-urlencode "redirect_uri=$REDIRECT_URI" \
  --data-urlencode "client_id=$CLIENT_ID" \
  --data-urlencode "code_verifier=$VERIFIER" \
  --data-urlencode "resource=$MCP_URL" \
  ${SECRET_ARGS[@]+"${SECRET_ARGS[@]}"}
ACCESS_TOKEN="$(json_field access_token || true)"
REFRESH_TOKEN="$(json_field refresh_token || true)"
if [ "$STATUS" = 200 ] && [ -n "$ACCESS_TOKEN" ] && [ -n "$REFRESH_TOKEN" ]; then
  pass "POST /oauth/token (form encoded) returned an access token and a refresh token"
else
  die "token exchange failed (status $STATUS): $(body | head -c 300)"
fi

say "7/13 the same code cannot be redeemed twice"
http POST /oauth/token \
  -H 'content-type: application/x-www-form-urlencoded' \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "code=$CODE_A" \
  --data-urlencode "redirect_uri=$REDIRECT_URI" \
  --data-urlencode "client_id=$CLIENT_ID" \
  --data-urlencode "code_verifier=$VERIFIER" \
  --data-urlencode "resource=$MCP_URL" \
  ${SECRET_ARGS[@]+"${SECRET_ARGS[@]}"}
REPLAY_ERROR="$(json_field error || true)"
if [ "$STATUS" -ge 400 ] && [ -z "$(json_field access_token || true)" ] && [ "$REPLAY_ERROR" = "invalid_grant" ]; then
  pass "replaying the same authorization code is refused (invalid_grant)"
else
  die "a replayed code should have been refused with invalid_grant (status $STATUS, error [$REPLAY_ERROR])"
fi

say "8/13 a wrong PKCE verifier is refused"
read -r VERIFIER_B CHALLENGE_B <<EOF
$(pkce)
EOF
authorize_login_consent "$CHALLENGE_B" "run-b-$NONCE" "flow B"
CODE_B="$LAST_CODE"
http POST /oauth/token \
  -H 'content-type: application/x-www-form-urlencoded' \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "code=$CODE_B" \
  --data-urlencode "redirect_uri=$REDIRECT_URI" \
  --data-urlencode "client_id=$CLIENT_ID" \
  --data-urlencode "code_verifier=not-the-verifier-that-hashed-to-this-challenge" \
  --data-urlencode "resource=$MCP_URL" \
  ${SECRET_ARGS[@]+"${SECRET_ARGS[@]}"}
if [ "$STATUS" -ge 400 ] && [ -z "$(json_field access_token || true)" ]; then
  pass "a wrong code_verifier is refused"
else
  die "a wrong code_verifier should have been refused (status $STATUS)"
fi

say "9/13 a redirect_uri that does not match exactly is refused"
read -r VERIFIER_C CHALLENGE_C <<EOF
$(pkce)
EOF
authorize_login_consent "$CHALLENGE_C" "run-c-$NONCE" "flow C"
CODE_C="$LAST_CODE"
http POST /oauth/token \
  -H 'content-type: application/x-www-form-urlencoded' \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "code=$CODE_C" \
  --data-urlencode "redirect_uri=${REDIRECT_URI}x" \
  --data-urlencode "client_id=$CLIENT_ID" \
  --data-urlencode "code_verifier=$VERIFIER_C" \
  --data-urlencode "resource=$MCP_URL" \
  ${SECRET_ARGS[@]+"${SECRET_ARGS[@]}"}
if [ "$STATUS" -ge 400 ] && [ -z "$(json_field access_token || true)" ]; then
  pass "a redirect_uri that is not an exact match is refused, not prefix-matched"
else
  die "an inexact redirect_uri should have been refused (status $STATUS)"
fi
# flow C's code is spent on that refusal. RFC 6749 codes are single-use, and a server that
# invalidates one on any presentation, successful or not, is being more defensive than the
# spec requires, not less correct. Nothing here redeems it a second time.

# Steps 10 to 13 run on a flow of their own, not on flow A. Step 7 redeemed flow A's code a second
# time, and the server answers a replayed code by revoking the whole token family that code issued
# ("authorization code replayed, revoking the token family" in the log), which OAuth 2.1 4.1.3 asks
# for. That kills the access and refresh tokens step 6 collected. Reusing them here reads as a
# broken MCP surface when the cause is the replay test two steps up.
read -r VERIFIER_D CHALLENGE_D <<EOF
$(pkce)
EOF
authorize_login_consent "$CHALLENGE_D" "run-d-$NONCE" "flow D"
http POST /oauth/token \
  -H 'content-type: application/x-www-form-urlencoded' \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "code=$LAST_CODE" \
  --data-urlencode "redirect_uri=$REDIRECT_URI" \
  --data-urlencode "client_id=$CLIENT_ID" \
  --data-urlencode "code_verifier=$VERIFIER_D" \
  --data-urlencode "resource=$MCP_URL" \
  ${SECRET_ARGS[@]+"${SECRET_ARGS[@]}"}
ACCESS_TOKEN="$(json_field access_token || true)"
REFRESH_TOKEN="$(json_field refresh_token || true)"
if [ "$STATUS" = 200 ] && [ -n "$ACCESS_TOKEN" ] && [ -n "$REFRESH_TOKEN" ]; then
  pass "flow D: a live access token and refresh token for the steps below"
else
  die "flow D: token exchange failed (status $STATUS): $(body | head -c 300)"
fi

say "10/13 the access token opens the MCP surface: initialize, tools/list, one real call"
mcp_call "$ACCESS_TOKEN" "$MCP_INIT_BODY"
if [ "$STATUS" = 200 ]; then
  pass "initialize succeeds with the issued access token"
else
  die "initialize failed with the issued access token (status $STATUS)"
fi
mcp_call "$ACCESS_TOKEN" '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
TOOL_NAMES="$(mcp_result_field tools 2>/dev/null | node -e '
  let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>{
    try { const tools=JSON.parse(s); process.stdout.write(tools.map(t=>t.name).join(",")); }
    catch { process.exit(1); }
  })' || true)"
if printf '%s' "$TOOL_NAMES" | grep -q 'context_bootstrap'; then
  pass "tools/list includes context_bootstrap: $TOOL_NAMES"
else
  die "tools/list did not include context_bootstrap: [$TOOL_NAMES]"
fi
mcp_call "$ACCESS_TOKEN" '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"context_bootstrap","arguments":{}}}'
IS_ERROR="$(mcp_result_field isError 2>/dev/null || echo false)"
if [ "$STATUS" = 200 ] && [ "$IS_ERROR" != "true" ]; then
  pass "context_bootstrap succeeds over the OAuth-issued token"
else
  die "context_bootstrap failed over the OAuth-issued token (status $STATUS, isError $IS_ERROR)"
fi

say "11/13 refreshing the access token"
http POST /oauth/token \
  -H 'content-type: application/x-www-form-urlencoded' \
  --data-urlencode "grant_type=refresh_token" \
  --data-urlencode "refresh_token=$REFRESH_TOKEN" \
  --data-urlencode "client_id=$CLIENT_ID" \
  ${SECRET_ARGS[@]+"${SECRET_ARGS[@]}"}
NEW_ACCESS_TOKEN="$(json_field access_token || true)"
NEW_REFRESH_TOKEN="$(json_field refresh_token || true)"
if [ "$STATUS" = 200 ] && [ -n "$NEW_ACCESS_TOKEN" ] && [ "$NEW_ACCESS_TOKEN" != "$ACCESS_TOKEN" ]; then
  pass "refresh_token grant returns a new, different access token"
else
  die "refreshing the access token failed (status $STATUS)"
fi

say "12/13 the new access token works"
# Checked before the reuse-detection test on purpose: reuse detection commonly revokes the whole
# token family it was issued from, which can include the access token step 11 just minted. Proving
# this token works first means step 13's refusal is read as reuse detection, not as fallout from
# testing reuse detection.
mcp_call "${NEW_ACCESS_TOKEN:-$ACCESS_TOKEN}" '{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}'
if [ "$STATUS" = 200 ]; then
  pass "the token issued by the refresh grant is itself usable"
else
  die "the refreshed access token did not work (status $STATUS)"
fi

say "13/13 the rotated-out refresh token is refused on reuse"
http POST /oauth/token \
  -H 'content-type: application/x-www-form-urlencoded' \
  --data-urlencode "grant_type=refresh_token" \
  --data-urlencode "refresh_token=$REFRESH_TOKEN" \
  --data-urlencode "client_id=$CLIENT_ID" \
  ${SECRET_ARGS[@]+"${SECRET_ARGS[@]}"}
if [ "$STATUS" -ge 400 ] && [ -z "$(json_field access_token || true)" ]; then
  pass "presenting the old refresh token after rotation is refused, proving reuse detection"
else
  die "the superseded refresh token should have been refused (status $STATUS)"
fi

say "what each step proved"
cat <<SUMMARY
  1  both protected-resource metadata paths point at an authorization server
  2  a bare POST to /mcp is a 401 carrying resource_metadata, not a silent 200
  3  discovery advertises S256 only, never plain
  4  dynamic client registration issues a usable client_id
  5  authorize -> login -> consent hands back a code and echoes state, with no browser involved
  6  the code exchanges for an access token and a refresh token over form encoding
  7  a replayed authorization code is refused (invalid_grant)
  8  a wrong PKCE verifier is refused
  9  a redirect_uri that is not an exact match is refused
  10 the access token drives a real MCP session: initialize, tools/list, context_bootstrap
  11 the refresh grant rotates in a new access token
  12 the rotated-in token is itself live
  13 the token it rotated out is refused if presented again

  every row this ran wrote lived in $SCRATCH_DB, dropped when this script exited.
SUMMARY

if [ "$FAILED" = 1 ]; then
  echo ""
  echo "  oauth-flow-test FAILED"
  exit 1
fi
echo ""
echo "  oauth-flow-test PASSED"
exit 0
