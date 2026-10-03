#!/usr/bin/env bash
# The cleanup pass, driven through the CLI against a scratch server.
#
#   ./scripts/cleanup-test.sh [--port N] [--keep]
#
# The server side is covered by tests/cleanup.rs. What this covers is the half that file cannot
# reach: the CLI reading the candidate list, the queue printing something a person can act on, and
# apply moving rows through the routes rather than through a service call in the same process.
#
# --no-model throughout. Nothing here calls a provider, nothing leaves the machine, and no key is
# needed to run it. The model half is exercised by hand with a key; this is the deterministic path.
#
# Its own server on --port (default 8792, never 8787) against lumberroom_cleanup_test, dropped afterwards.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRATCH_PORT=8792
SCRATCH_KEEP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --port) SCRATCH_PORT="$2"; shift 2 ;;
    --keep) SCRATCH_KEEP=1; shift ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
WORK="$REPO_DIR/target/cleanup-test-work"
rm -rf "$WORK"
mkdir -p "$WORK"

SCRATCH_DB=lumberroom_cleanup_test
SCRATCH_NAME="${LUMBERROOM_CLEANUP_TEST_SERVER:-lumberroom-cleanup-test-server}"
SCRATCH_EMBED=local
TOKEN="$(openssl rand -hex 32)"
NO_CLEANUP_TOKEN="$(openssl rand -hex 32)"
SCRATCH_TOKENS="[{\"client\":\"cleanup-test\",\"token\":\"$TOKEN\",\"mayIngest\":true,\"mayDelete\":true},{\"client\":\"cleanup-test-denied\",\"token\":\"$NO_CLEANUP_TOKEN\"}]"
export SCRATCH_DB SCRATCH_NAME SCRATCH_EMBED SCRATCH_TOKENS SCRATCH_PORT SCRATCH_KEEP

# shellcheck source=lib/scratch-server.sh
. "$REPO_DIR/scripts/lib/scratch-server.sh"
trap 'status=$?; scratch_stop; rm -rf "$WORK"; exit $status' EXIT INT TERM

PASSED=0
FAILED=0
say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAILED=$((FAILED + 1)); }
die() { fail "$*"; printf '\ncleanup-test FAILED\n'; exit 1; }

echo "building lumberroom in the builder image..."
"$REPO_DIR/scripts/cargo.sh" build --release -p lumberroom >/dev/null
scratch_start || exit 1

# The binary is built inside the Linux builder image, so it runs there too. The server is reached
# by container name, because rmcp validates Host against an allowlist derived from PUBLIC_URL and a
# loopback address would answer 403 to every call while every health check stayed green.
BIN="$REPO_DIR/target/release/lumberroom"
cli_as() {
  tok="$1"; shift
  docker run --rm --network "$SCRATCH_NETWORK" \
    -v "$REPO_DIR:$REPO_DIR" -w "$REPO_DIR" \
    -e LUMBERROOM_URL="$SCRATCH_INTERNAL_URL" -e LUMBERROOM_TOKEN="$tok" -e LUMBERROOM_CONFIG="$WORK/cfg.json" \
    -e BUILDER_UID="$(id -u)" -e BUILDER_GID="$(id -g)" \
    lumberroom-builder "$BIN" "$@"
}
cli() { cli_as "$TOKEN" "$@"; }
psqlc() {
  docker compose -f "$REPO_DIR/docker-compose.yml" exec -T db \
    psql -U "${POSTGRES_USER:-lumberroom}" -d "$SCRATCH_DB" -A -t -c "$1"
}

say "0/6 the CLI reaches the scratch server"
cli doctor >"$WORK/doctor.txt" 2>&1 || die "doctor failed: $(cat "$WORK/doctor.txt")"
pass "doctor answers at $SCRATCH_URL"

say "1/6 seed two rows that say the same thing in the same words"
# Inserted directly, because services::write::run collapses a near-identical write and cannot make
# a duplicate. Every duplicate in a real store got there some other way: written before that check
# existed, restored from a dump, or put there by a harness like this one.
cli write "the deploy runbook lives in DEPLOY.md" --namespace user:me >/dev/null 2>&1 \
  || die "the first write failed"
psqlc "INSERT INTO memory (id, tenant_id, namespace, content, embedding, source_client, embedding_model, sensitivity)
       SELECT gen_random_uuid(), tenant_id, namespace, 'The deploy runbook lives in DEPLOY.md  ',
              embedding, 'seed', embedding_model, sensitivity
         FROM memory WHERE content = 'the deploy runbook lives in DEPLOY.md'" >/dev/null
COUNT="$(psqlc "SELECT count(*) FROM memory WHERE content ILIKE 'the deploy runbook%'" | tr -d ' ')"
[ "$COUNT" = "2" ] || die "expected two rows, found $COUNT"
pass "two rows differing only in case and trailing spaces"

say "2/6 the deterministic pass finds them and queues one proposal"
cli cleanup run --no-model >"$WORK/run.txt" 2>&1 || die "the pass failed: $(cat "$WORK/run.txt")"
cat "$WORK/run.txt" | sed 's/^/      /'
grep -q "1 exact groups" "$WORK/run.txt" || die "the pass did not group them: $(cat "$WORK/run.txt")"
grep -q "1 queued" "$WORK/run.txt" || die "the pass queued nothing: $(cat "$WORK/run.txt")"
pass "one exact group, one proposal queued"

say "3/6 a second run queues nothing, which is what makes an hourly cadence safe"
cli cleanup run --no-model >"$WORK/run2.txt" 2>&1 || die "the second pass failed"
grep -q "0 queued" "$WORK/run2.txt" || die "the same cluster was queued twice: $(cat "$WORK/run2.txt")"
grep -q "1 already known" "$WORK/run2.txt" || die "and it was not counted as known: $(cat "$WORK/run2.txt")"
pass "0 queued, 1 already known"

say "4/6 the queue prints a proposal a person can act on"
cli cleanup list >"$WORK/list.txt" 2>&1 || die "list failed: $(cat "$WORK/list.txt")"
cat "$WORK/list.txt" | sed 's/^/      /'
grep -q "exact" "$WORK/list.txt" || die "the queue does not name the kind"
grep -q "via exact" "$WORK/list.txt" || die "the queue does not say what produced it"
ID="$(awk '/exact/ {print $1; exit}' "$WORK/list.txt")"
[ -n "$ID" ] || die "could not read a proposal id out of the queue"
cli cleanup show "$ID" >"$WORK/show.txt" 2>&1 || die "show failed: $(cat "$WORK/show.txt")"
grep -q "keep" "$WORK/show.txt" || die "show does not say which row survives"
grep -q "retire" "$WORK/show.txt" || die "show does not say which row goes"
pass "the queue names the kind, what produced it, and which row survives"

say "5/6 apply retires through supersession, and the survivor still answers"
cli cleanup apply "$ID" >"$WORK/apply.txt" 2>&1 || die "apply failed: $(cat "$WORK/apply.txt")"
grep -q "1 retired, 0 deleted" "$WORK/apply.txt" \
  || die "an exact duplicate must retire, never delete: $(cat "$WORK/apply.txt")"
RETIRED="$(psqlc "SELECT count(*) FROM memory WHERE superseded_by IS NOT NULL" | tr -d ' ')"
[ "$RETIRED" = "1" ] || die "expected one retired row, found $RETIRED"
LIVE="$(psqlc "SELECT count(*) FROM memory WHERE content ILIKE 'the deploy runbook%' AND superseded_by IS NULL" | tr -d ' ')"
[ "$LIVE" = "1" ] || die "expected one live row, found $LIVE"
cli search "deploy runbook" >"$WORK/search.txt" 2>&1 || die "search failed"
grep -qi "deploy runbook" "$WORK/search.txt" || die "the surviving row stopped answering"
pass "one retired with superseded_by set, one live, and it still answers search"

say "6/6 a client without mayIngest cannot run the pass or read the queue"
DENIED_STATUS="$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H "authorization: Bearer $NO_CLEANUP_TOKEN" -H 'content-type: application/json' \
  -d '{}' "$SCRATCH_URL/admin/cleanup/run")"
[ "$DENIED_STATUS" = "403" ] || die "an ungranted client ran the pass (HTTP $DENIED_STATUS)"
DENIED_LIST="$(curl -sS -o /dev/null -w '%{http_code}' \
  -H "authorization: Bearer $NO_CLEANUP_TOKEN" "$SCRATCH_URL/admin/cleanup/proposals")"
[ "$DENIED_LIST" = "403" ] || die "an ungranted client read the queue (HTTP $DENIED_LIST)"
pass "both routes answer 403 without mayIngest"

printf '\n\033[1m%s\033[0m\n' "what this proved"
cat <<SUMMARY
  1  a duplicate cannot be made through the write path, so this one was seeded around it
  2  the deterministic pass groups on normalised text and queues one proposal per cluster
  3  running it again queues nothing, which is the whole argument for an hourly cadence
  4  the queue names the kind, the producer, and which of the rows survives
  5  applying retires through supersession: history survives and the survivor still answers
  6  the queue is behind a grant, so a client the owner did not name cannot fill it

  $PASSED passed, $FAILED failed
SUMMARY
[ "$FAILED" -eq 0 ] || { printf '\ncleanup-test FAILED\n'; exit 1; }
printf '\n  \033[32mcleanup-test PASSED\033[0m\n'
