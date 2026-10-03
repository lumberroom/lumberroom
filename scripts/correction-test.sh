#!/usr/bin/env bash
# The Phase 4 exit criterion: "a correction you make once does not resurface as a contradiction
# later" (system PRD §7). Phase 4 spec's "Exit test" lays out the steps; this follows them, plus
# the numeric-guard case the spec calls out as the reason supersession exists at all.
#
#   ./scripts/correction-test.sh                      its own server, its own database
#   LUMBERROOM_URL=https://memory.example.com LUMBERROOM_TOKEN=... ./scripts/correction-test.sh --live
#
# With no argument this stands up a scratch server on --port (default 8791, never 8787) against a
# database named lumberroom_correction_test, runs against that, and drops it afterwards. Every step here
# writes nonce-tagged rows, and on 21 August 2026 a run against 127.0.0.1:8787 left four of them in
# the owner's user:me, two of which survived to reach the next session's digest as facts about him.
# A gate that writes is a gate that needs its own store.
#
# --live runs against LUMBERROOM_URL with LUMBERROOM_TOKEN, which needs write access to
# LUMBERROOM_CORRECTION_NAMESPACE (default "user:me") and mayDelete, because a live run deletes every row
# it wrote before it exits. --live --keep-rows skips that teardown.
#
# The Phase 4 spec frames its exit test as three model sessions (A states a fact, B states the
# correction, C asks the question). This script drives the same three moments directly through
# memory_write/memory_search rather than through a live model session: scripts/done-when-test.sh
# already proves a model chooses to call these tools on its own, so re-proving that here would
# duplicate that script rather than add to it. What this proves instead, and what the spec's own
# steps 1, 2 and 4 check, is that the server keeps the correction, the conflict, and the history
# straight once it is told about them.
#
# Follows the house pattern in scripts/done-when-test.sh: bash, set -euo pipefail, curl and node
# only, a nonce per run, coloured PASS/FAIL lines, non-zero exit on any failure, a summary at the
# end.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NS="${LUMBERROOM_CORRECTION_NAMESPACE:-user:me}"
LIVE=0
KEEP_ROWS=0
SCRATCH_PORT=8791
SCRATCH_KEEP=0
POSITIONAL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --live) LIVE=1; shift ;;
    --keep-rows) KEEP_ROWS=1; shift ;;
    --port) SCRATCH_PORT="$2"; shift 2 ;;
    --keep) SCRATCH_KEEP=1; shift ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    -*) echo "unknown argument: $1" >&2; exit 1 ;;
    *) POSITIONAL="$1"; shift ;;
  esac
done

command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
command -v node >/dev/null 2>&1 || { echo "node is required" >&2; exit 1; }
command -v lumberroom >/dev/null 2>&1 || {
  echo "lumberroom is not on PATH. This gate drives the shipped client." >&2
  echo "  brew install lumberroom/lumberroom/lumberroom" >&2
  exit 1
}

WORK="$(mktemp -d)"
WROTE="$WORK/wrote.txt"
: > "$WROTE"

# Every id this run creates, so a --live run can take back what it wrote. Called from the trap
# rather than the happy path, because the run that leaves rows behind is the one that died at
# step 3.
record_write() { if [ -n "${1:-}" ]; then printf '%s\n' "$1" >>"$WROTE"; fi; }

forget_written() {
  [ "${LIVE:-0}" -eq 1 ] || return 0
  local count; count="$(grep -c . "$WROTE" 2>/dev/null || echo 0)"
  [ "$count" -gt 0 ] || return 0
  if [ "$KEEP_ROWS" -eq 1 ]; then
    printf '\n  --keep-rows: %s rows left in %s\n' "$count" "$URL"
    sed 's/^/    /' "$WROTE"
    return 0
  fi
  local gone=0 stuck=0 code
  while read -r id; do
    [ -n "$id" ] || continue
    code="$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE \
      -H "authorization: Bearer $TOKEN" "$URL/admin/memory/$id" 2>/dev/null || echo 000)"
    if [ "$code" = "200" ]; then gone=$((gone + 1)); else
      stuck=$((stuck + 1)); printf '  \033[31mcould not delete %s (%s)\033[0m\n' "$id" "$code"
    fi
  done <"$WROTE"
  printf '\n  cleaned up: deleted %s of %s rows this run wrote\n' "$gone" "$count"
  [ "$stuck" -eq 0 ] || printf '  \033[31m%s rows are still in %s. Delete them by hand.\033[0m\n' "$stuck" "$URL"
}

if [ "$LIVE" -eq 1 ]; then
  URL="${POSITIONAL:-${LUMBERROOM_URL:-http://127.0.0.1:8787}}"
  URL="${URL%/}"
  TOKEN="${LUMBERROOM_TOKEN:-}"
  [ -n "$TOKEN" ] || { echo "--live needs LUMBERROOM_TOKEN (write and mayDelete on $NS)" >&2; exit 1; }
  trap 'status=$?; forget_written; rm -rf "$WORK"; exit $status' EXIT INT TERM
  printf '\033[33m  running against %s, a store this script did not create.\033[0m\n' "$URL"
  printf '\033[33m  every row it writes is deleted before it exits unless --keep-rows.\033[0m\n'
else
  [ -n "$POSITIONAL" ] && {
    echo "a URL argument needs --live. Without it this script runs against its own server." >&2
    exit 1
  }
  # Local so the numeric guard and the conflict candidate mean something: both turn on two texts
  # being alike, and the hash embedder answers that question with noise.
  SCRATCH_EMBED=local
  SCRATCH_DB=lumberroom_correction_test
  SCRATCH_NAME="${LUMBERROOM_CORRECTION_TEST_SERVER:-lumberroom-correction-test-server}"
  SCRATCH_TOKEN="$(openssl rand -hex 32)"
  SCRATCH_TOKENS="[{\"client\":\"correction-test\",\"token\":\"$SCRATCH_TOKEN\",\"mayDelete\":true}]"
  export SCRATCH_EMBED SCRATCH_DB SCRATCH_NAME SCRATCH_TOKENS SCRATCH_PORT SCRATCH_KEEP
  # shellcheck source=lib/scratch-server.sh
  . "$REPO_DIR/scripts/lib/scratch-server.sh"
  trap 'status=$?; scratch_stop; rm -rf "$WORK"; exit $status' EXIT INT TERM
  scratch_start || exit 1
  URL="$SCRATCH_URL"
  TOKEN="$SCRATCH_TOKEN"
fi

NONCE="$(head -c 6 /dev/urandom | od -An -tx1 | tr -d ' \n')"
# The client under test is the shipped binary, taken from PATH.
#
# This used to be `node bin/lumberroom.mjs`, a second client written against node built-ins alone.
# It shared no types with the server and so could not be accidentally accommodated by it, which
# earned it a place here. It also fell behind: it never learned to read an authorization server's
# metadata document, so it could not log in against a deployment whose issuer differs from its API
# base, and a gate driving a client that cannot log in proves less than it looks like it does.
MEMCTL="lumberroom"

FAILED=0
say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m  %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAILED=1; }
die() { fail "$*"; printf '\ncorrection-test FAILED\n'; exit 1; }

memctl() {
  LUMBERROOM_URL="$URL" LUMBERROOM_TOKEN="$TOKEN" LUMBERROOM_CONFIG="$WORK/cfg.json" $MEMCTL "$@"
}

json_field() {
  # json_field FILE PATH: dotted path, "" and a nonzero exit when absent.
  node -e '
    const fs = require("node:fs");
    let j; try { j = JSON.parse(fs.readFileSync(process.argv[1], "utf8")); } catch { process.exit(1); }
    let v = j;
    for (const p of process.argv[2].split(".")) { if (v == null) { v = undefined; break; } v = v[p]; }
    if (v === undefined || v === null) process.exit(1);
    process.stdout.write(typeof v === "string" ? v : JSON.stringify(v));
  ' "$1" "$2" 2>/dev/null
}

json_includes_id() {
  # json_includes_id FILE ARRAY_PATH ID: true (exit 0) when possible_conflicts (or similar) has ID.
  node -e '
    const fs = require("node:fs");
    const j = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    let v = j;
    for (const p of process.argv[2].split(".")) { if (v == null) { v = undefined; break; } v = v[p]; }
    process.exit(Array.isArray(v) && v.some(x => x && x.id === process.argv[3]) ? 0 : 1);
  ' "$1" "$2" "$3" 2>/dev/null
}

admin_memory() {
  # admin_memory ID FILE: raw GET /admin/memory/<id>, writes body to FILE, prints http status.
  curl -sS -o "$2" -w '%{http_code}' -H "authorization: Bearer $TOKEN" \
    "$URL/admin/memory/$(node -e 'process.stdout.write(encodeURIComponent(process.argv[1]))' "$1")" \
    || echo 000
}

say "0/6 preflight"
if memctl doctor >"$WORK/doctor.txt" 2>&1; then
  pass "the credential authenticates ($URL)"
else
  die "could not reach $URL: $(cat "$WORK/doctor.txt")"
fi

say "1/6 write a fact, assert it is stored and retrievable"
FACT_OLD="the $NONCE support line callback number is 555-0100"
if memctl write "$FACT_OLD" --namespace "$NS" --json >"$WORK/v1.json" 2>"$WORK/v1.err"; then
  V1_ID="$(json_field "$WORK/v1.json" id || true)"
  if [ -n "$V1_ID" ]; then
    record_write "$V1_ID"
    pass "written: $V1_ID"
  else
    die "write succeeded but returned no id: $(cat "$WORK/v1.json")"
  fi
else
  die "the initial write failed: $(cat "$WORK/v1.err")"
fi
if memctl search "$NONCE support line callback number" --json >"$WORK/v1-search.json" 2>&1 \
  && grep -qF "555-0100" "$WORK/v1-search.json"; then
  pass "the fact is retrievable by search"
else
  die "the fact could not be found by search right after writing it: $(cat "$WORK/v1-search.json")"
fi

say "2/6 write the corrected version with supersedes, assert the correction is accepted"
FACT_NEW="the $NONCE support line callback number is 555-0199"
if memctl write "$FACT_NEW" --namespace "$NS" --supersedes "$V1_ID" --json >"$WORK/v2.json" 2>"$WORK/v2.err"; then
  V2_ID="$(json_field "$WORK/v2.json" id || true)"
  SUPERSEDED="$(json_field "$WORK/v2.json" superseded || true)"
  if [ -n "$V2_ID" ] && [ "$SUPERSEDED" = "$V1_ID" ]; then
    record_write "$V2_ID"
    pass "written: $V2_ID, and the write reports it superseded $V1_ID"
  else
    die "the superseding write did not report superseded=$V1_ID (got [$SUPERSEDED]): $(cat "$WORK/v2.json")"
  fi
else
  die "the superseding write was refused: $(cat "$WORK/v2.err")"
fi

say "3/6 search for the question: the new value is there, the old one is not"
if memctl search "what is the $NONCE support line callback number" --json >"$WORK/answer.json" 2>&1; then
  if grep -qF "555-0199" "$WORK/answer.json" && ! grep -qF "555-0100" "$WORK/answer.json"; then
    pass "the answer contains the new value and does not contain the old one"
  else
    die "the search results still carry the old value, the new one, or both wrongly: $(cat "$WORK/answer.json")"
  fi
else
  die "search after the correction failed: $(cat "$WORK/answer.json")"
fi

say "4/6 the old row survives, with superseded_by set: history was not deleted"
V1_STATUS="$(admin_memory "$V1_ID" "$WORK/v1-row.json")"
if [ "$V1_STATUS" = 200 ]; then
  V1_SUPERSEDED_BY="$(json_field "$WORK/v1-row.json" superseded_by || true)"
  if [ "$V1_SUPERSEDED_BY" = "$V2_ID" ]; then
    pass "the old row ($V1_ID) is still in the database, superseded_by is $V2_ID"
  else
    die "the old row's superseded_by is [$V1_SUPERSEDED_BY], expected $V2_ID: $(cat "$WORK/v1-row.json")"
  fi
else
  die "GET /admin/memory/$V1_ID returned $V1_STATUS; the old row should still exist, just retired"
fi

say "5/6 the numeric guard: near-identical text with different digits must not collapse"
PORT_OLD="the $NONCE service port is 8787"
PORT_NEW="the $NONCE service port is 8080"
if memctl write "$PORT_OLD" --namespace "$NS" --json >"$WORK/port-old.json" 2>"$WORK/port-old.err"; then
  PORT_OLD_ID="$(json_field "$WORK/port-old.json" id || true)"
  record_write "$PORT_OLD_ID"
  pass "wrote the port-8787 fact: $PORT_OLD_ID"
else
  die "could not write the port-8787 fact: $(cat "$WORK/port-old.err")"
fi
if memctl write "$PORT_NEW" --namespace "$NS" --json >"$WORK/port-new.json" 2>"$WORK/port-new.err"; then
  PORT_NEW_ID="$(json_field "$WORK/port-new.json" id || true)"
  record_write "$PORT_NEW_ID"
  PORT_DEDUPED="$(json_field "$WORK/port-new.json" deduplicated || true)"
  if [ "$PORT_DEDUPED" = "false" ] && [ -n "$PORT_NEW_ID" ] && [ "$PORT_NEW_ID" != "$PORT_OLD_ID" ]; then
    pass "the port-8080 write is a new row ($PORT_NEW_ID), not a collapse into $PORT_OLD_ID, despite the \
two texts differing by one digit run"
  else
    die "the near-identical port fact collapsed into its predecessor (deduplicated=$PORT_DEDUPED, \
id=$PORT_NEW_ID). Collapsing a correction into its own predecessor destroys data silently: \
$(cat "$WORK/port-new.json")"
  fi
  if json_includes_id "$WORK/port-new.json" possible_conflicts "$PORT_OLD_ID"; then
    pass "the old port fact came back as a possible_conflicts candidate on the new write"
  else
    die "the new write did not flag the old port fact as a possible conflict: $(cat "$WORK/port-new.json")"
  fi
else
  die "could not write the port-8080 fact: $(cat "$WORK/port-new.err")"
fi

say "6/6 resolving the flagged conflict through 'lumberroom supersede' behaves exactly like an inline correction"
if memctl supersede "$PORT_OLD_ID" "$PORT_NEW_ID" >"$WORK/supersede.txt" 2>&1; then
  pass "lumberroom supersede $PORT_OLD_ID $PORT_NEW_ID: $(cat "$WORK/supersede.txt")"
else
  die "lumberroom supersede failed: $(cat "$WORK/supersede.txt")"
fi
if memctl search "what is the $NONCE service port" --json >"$WORK/port-answer.json" 2>&1; then
  if grep -qF "8080" "$WORK/port-answer.json" && ! grep -qF "8787" "$WORK/port-answer.json"; then
    pass "after resolving the conflict, search answers 8080 and not 8787"
  else
    die "search still surfaces the retired port value, the new one, or both wrongly: $(cat "$WORK/port-answer.json")"
  fi
else
  die "search after resolving the port conflict failed: $(cat "$WORK/port-answer.json")"
fi
PORT_OLD_STATUS="$(admin_memory "$PORT_OLD_ID" "$WORK/port-old-row.json")"
if [ "$PORT_OLD_STATUS" = 200 ] && [ "$(json_field "$WORK/port-old-row.json" superseded_by || true)" = "$PORT_NEW_ID" ]; then
  pass "the retired port-8787 row is still in the database with superseded_by set to $PORT_NEW_ID"
else
  die "the retired port row's superseded_by did not end up as $PORT_NEW_ID (status $PORT_OLD_STATUS): \
$(cat "$WORK/port-old-row.json")"
fi

say "what each step proved"
cat <<SUMMARY
  1  a fact, once written, is stored and immediately retrievable
  2  a write carrying supersedes is accepted and reports what it retired
  3  the question now answers with the new value only, never the old one
  4  the retired row still exists, with superseded_by pointing at the row that replaced it
  5  two texts that differ only in a digit run never collapse into one row, however similar the
     embeddings are, and the older one is flagged as a possible conflict instead
  6  resolving that flagged conflict through 'lumberroom supersede' produces the same end state as an
     inline correction: the new value answers, the old one does not, and history survived
SUMMARY

if [ "$FAILED" = 1 ]; then
  echo ""
  echo "  correction-test FAILED"
  exit 1
fi
echo ""
echo "  correction-test PASSED"
exit 0
