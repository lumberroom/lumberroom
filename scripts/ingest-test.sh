#!/bin/sh
# Phase 6 exit test, docs/specs/phase-6-ingestion.md §12, steps 1, 2, 3 (memory-tool half), 4, 5, 6.
#
#   ./scripts/ingest-test.sh
#   ./scripts/ingest-test.sh --port 8789 --keep
#
# SAFETY, and it is the reason this script stands up its own everything. `ingest plan` opens a run
# row on the server before it reads a byte, and `ingest submit` moves watermarks. Both are writes.
# So this script never talks to the owner's server and never opens the owner's database: it starts
# a scratch server on --port (default 8789, never 8787) against a database named lumberroom_ingest_test,
# mints two throwaway credentials for the run, and drops both on exit. LUMBERROOM_URL and LUMBERROOM_TOKEN in
# the environment are ignored on purpose; there is no flag that points this at a live deployment.
#
# The transcripts it reads are fixtures it writes itself under target/ingest-test-work, reached
# through LUMBERROOM_CLAUDE_ROOT. The owner's ~/.claude/projects is never walked.
#
# Flags:
#   --port N   the scratch server's port. Default 8789. 8787 is refused.
#   --keep     leave the container and lumberroom_ingest_test in place for inspection.
#
# LUMBERROOM_CLI, if set, is a path to a lumberroom binary that runs on this host, and every step runs
# through it directly. Unset, the script builds crates/lumberroom in the builder image and runs the
# binary inside a container on the compose network. On macOS that is the only mode that works:
# target/release/lumberroom is a Linux ELF.
#
# Follows the house shape in policy-test.sh and correction-test.sh: PASS and FAIL lines, a counter,
# a summary, a non-zero exit on any failure. There is no SKIP. Every step below is decidable
# against a server this script controls, and a harness that can skip is a harness that reports
# success while testing nothing.

set -e
cd "$(dirname "$0")/.."
REPO_DIR="$PWD"
[ -f .env ] && { set -a; . ./.env; set +a; }

PORT=8789
KEEP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done

TEST_DB=lumberroom_ingest_test
SERVER_NAME="${LUMBERROOM_INGEST_TEST_SERVER:-lumberroom-ingest-test-server}"
NETWORK="${LUMBERROOM_DOCKER_NETWORK:-lumberroom_default}"
POSTGRES_USER="${POSTGRES_USER:-lumberroom}"

# The two values a mistake here would cost the owner. Checked rather than commented, because the
# port is a flag and the database name is one edit away from being one.
[ "$PORT" = "8787" ] && { echo "8787 is the owner's live server. Pick another port." >&2; exit 1; }
[ "$TEST_DB" = "${POSTGRES_DB:-lumberroom}" ] && { echo "the test database is the owner's database. Refusing." >&2; exit 1; }

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "python3 is required" >&2; exit 1; }
command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
[ -n "${POSTGRES_PASSWORD:-}" ] || {
  echo "POSTGRES_PASSWORD is not set. Copy .env.example to .env and fill it in first." >&2
  exit 1
}

compose() { docker compose -f "$REPO_DIR/docker-compose.yml" "$@"; }

docker image inspect lumberroom-server:0.4.0 >/dev/null 2>&1 || {
  echo "the lumberroom-server:0.4.0 image is not built. Build it once with:  docker compose build server" >&2
  exit 1
}

HOST_CLI="${LUMBERROOM_CLI:-}"
BIN="$REPO_DIR/target/release/lumberroom"
if [ -z "$HOST_CLI" ]; then
  docker image inspect lumberroom-builder >/dev/null 2>&1 || {
    echo "the lumberroom-builder image is not built. Build it once with:" >&2
    echo "  docker build -t lumberroom-builder -f Dockerfile.builder ." >&2
    exit 1
  }
  # Rebuilt every run. It is incremental and takes seconds warm, and a binary older than the parser
  # turns this script into a test of what the parser used to do.
  echo "building lumberroom in the builder image..."
  "$REPO_DIR/scripts/cargo.sh" build --release -p lumberroom >/dev/null
fi

WORK="$REPO_DIR/target/ingest-test-work"
# Under the repo, not under /tmp: the CLI runs in a container and the fixture root has to resolve to
# the same absolute path on both sides of the bind mount, which is what lets the script read back
# the run directory the CLI wrote.
rm -rf "$WORK"
mkdir -p "$WORK"
# Set before the first CLI call and never left empty. An empty LUMBERROOM_CLAUDE_ROOT falls back to
# $HOME/.claude/projects, which is the owner's real corpus.
CASE=0
FIXTURE_ROOT="$WORK/claude-root-0"
DIR="$FIXTURE_ROOT"
mkdir -p "$FIXTURE_ROOT"

TOKEN="$(openssl rand -hex 32)"
NO_INGEST_TOKEN="$(openssl rand -hex 32)"

cleanup() {
  status=$?
  docker rm -f "$SERVER_NAME" >/dev/null 2>&1 || true
  # Step 7 leaves a python listener on the host. `set -e` is on, so this is an if rather than a
  # && list: a failed kill there would exit the trap before it reported the real status.
  if [ -n "${STUB_PID:-}" ]; then
    kill "$STUB_PID" >/dev/null 2>&1 || true
    STUB_PID=""
  fi
  if [ "$KEEP" -eq 1 ]; then
    echo ""
    echo "  left $WORK and database $TEST_DB in place. Drop the database with:"
    echo "    docker compose exec db dropdb -U $POSTGRES_USER $TEST_DB"
  else
    rm -rf "$WORK"
    compose exec -T db psql -U "$POSTGRES_USER" -d postgres \
      -c "DROP DATABASE IF EXISTS $TEST_DB" >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

echo "bringing up the compose database (reusing it if already running)..."
compose up -d db >/dev/null
i=0
until compose exec -T db pg_isready -U "$POSTGRES_USER" >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -ge 60 ] && { echo "postgres did not become ready within 60s" >&2; exit 1; }
  sleep 1
done

# Dropped and recreated, so a run never inherits the watermarks of the run before it. Step 5 asks
# whether a watermark moved, and a stale one from yesterday answers that question wrong.
compose exec -T -e PGOPTIONS="-c client_min_messages=warning" db \
  psql -U "$POSTGRES_USER" -d postgres -c "DROP DATABASE IF EXISTS $TEST_DB" >/dev/null
compose exec -T db psql -U "$POSTGRES_USER" -d postgres -c "CREATE DATABASE $TEST_DB" >/dev/null

# The scratch server is the shipped image with four settings changed. EMBED_PROVIDER=hash keeps the
# run offline and instant: nothing here measures retrieval, and the local embedder would download
# weights to answer a question no step asks. KEK_PROVIDER=none is honest about a store that holds
# nothing worth a key. PUBLIC_URL names the container, because rmcp validates Host against an
# allowlist derived from it and the CLI reaches this server by container name.
docker rm -f "$SERVER_NAME" >/dev/null 2>&1 || true
echo "starting the scratch server on port $PORT against database $TEST_DB..."
docker run -d --name "$SERVER_NAME" --network "$NETWORK" \
  -p "127.0.0.1:${PORT}:${PORT}" \
  -e PORT="$PORT" \
  -e HOST=0.0.0.0 \
  -e TENANT_ID=ingesttest \
  -e DATABASE_URL="postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@db:5432/${TEST_DB}" \
  -e PUBLIC_URL="http://${SERVER_NAME}:${PORT}" \
  -e AUTH_MODE=token \
  -e AUTH_TOKENS="[{\"client\":\"ingest-test\",\"token\":\"$TOKEN\",\"mayIngest\":true},{\"client\":\"ingest-test-denied\",\"token\":\"$NO_INGEST_TOKEN\"}]" \
  -e EMBED_PROVIDER=hash \
  -e EMBED_DIM=768 \
  -e KEK_PROVIDER=none \
  lumberroom-server:0.4.0 >/dev/null

i=0
until curl -sf "http://127.0.0.1:${PORT}/readyz" >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$i" -ge 60 ]; then
    echo "the scratch server did not become ready within 120s. Last log lines:" >&2
    docker logs --tail 40 "$SERVER_NAME" >&2 || true
    exit 1
  fi
  sleep 2
done

if [ -n "$HOST_CLI" ]; then
  CLI_URL="http://127.0.0.1:${PORT}"
else
  CLI_URL="http://${SERVER_NAME}:${PORT}"
fi

NONCE="$(head -c 6 /dev/urandom | od -An -tx1 | tr -d ' \n')"

PASSED=0
FAILED=0
say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAILED=$((FAILED + 1)); }
die() { printf '\n  %s\n\n  ingest-test could not run\n' "$*" >&2; exit 1; }

# Never echo captured CLI output raw. `plan` writes a fence marker naming its run, and a marker that
# reaches a terminal lands in the transcript of the session running this script, where it opens a
# fence over everything below it that no end marker closes.
excerpt() { grep -v 'lumberroom-ingest-' "$1" 2>/dev/null | tr '\n' ' ' | cut -c1-600; }

# Every invocation is pinned to this script's own config file, state directory and fixture root.
# Nothing here reads ~/.config/lumberroom, and LUMBERROOM_URL and LUMBERROOM_TOKEN from the caller's environment do
# not reach the binary.
cli_as() {
  tok="$1"; cfg="$2"; state="$3"; shift 3
  if [ -n "$HOST_CLI" ]; then
    LUMBERROOM_URL="$CLI_URL" LUMBERROOM_TOKEN="$tok" LUMBERROOM_CONFIG="$cfg" LUMBERROOM_STATE_DIR="$state" \
      LUMBERROOM_CLAUDE_ROOT="$FIXTURE_ROOT" "$HOST_CLI" "$@"
  else
    docker run --rm --network "$NETWORK" \
      --add-host host.docker.internal:host-gateway \
      -v "$REPO_DIR:$REPO_DIR" -w "$REPO_DIR" \
      -e LUMBERROOM_URL="$CLI_URL" -e LUMBERROOM_TOKEN="$tok" \
      -e LUMBERROOM_CONFIG="$cfg" -e LUMBERROOM_STATE_DIR="$state" \
      -e LUMBERROOM_CLAUDE_ROOT="$FIXTURE_ROOT" \
      -e BUILDER_UID="$(id -u)" -e BUILDER_GID="$(id -g)" \
      lumberroom-builder "$BIN" "$@"
  fi
}

cli() { cli_as "$TOKEN" "$WORK/cfg.json" "$WORK/state" "$@"; }

# Cut the JSON body out of a captured run. `plan` prints its fence marker, and any sweep notice,
# above the body; `submit` prints its end marker below it. Parsing the whole stream fails on both.
body() { sed -n '/^{/,/^}/p' "$1" >"$2"; }

# count FILE PATH: the integer at a dotted path. 0 for an absent path, -1 for a file that will not
# parse, so a broken run reads as broken rather than as a counter that happened to be zero.
count() {
  python3 - "$1" "$2" <<'PY'
import json, sys
try:
    v = json.load(open(sys.argv[1]))
except Exception:
    print(-1); raise SystemExit(0)
for part in sys.argv[2].split("."):
    if not isinstance(v, dict) or part not in v:
        print(0); raise SystemExit(0)
    v = v[part]
print(v if isinstance(v, int) else 0)
PY
}

# field FILE PATH: a string at a dotted path, empty when absent.
field() {
  python3 - "$1" "$2" <<'PY'
import json, sys
try:
    v = json.load(open(sys.argv[1]))
except Exception:
    raise SystemExit(0)
for part in sys.argv[2].split("."):
    if not isinstance(v, dict) or part not in v:
        raise SystemExit(0)
    v = v[part]
print(v if isinstance(v, str) else "")
PY
}

# A fixture root per case, at a path no earlier case used. Sets FIXTURE_ROOT and DIR.
#
# The fresh path is not tidiness. Recreating a directory the host just deleted is not reliably
# visible inside a container on Docker Desktop: measured here, a container started right after
# `rm -rf` and `mkdir` on one path saw an empty directory on four tries out of six, while twelve
# tries at twelve distinct paths were all correct. Reusing one root made two steps report a corpus
# of zero files, which is the failure this whole script exists to catch, arriving as a pass.
new_fixture_dir() {
  CASE=$((CASE + 1))
  FIXTURE_ROOT="$WORK/claude-root-$CASE"
  DIR="$FIXTURE_ROOT/-tmp-lumberroom-ingest-test"
  mkdir -p "$DIR"
}

# plan NAME [extra flags]: run a plan, leave the raw stream in $WORK/NAME.raw and the JSON body in
# $WORK/NAME.json. Returns the CLI's exit status.
plan() {
  name="$1"; shift
  if cli ingest plan --source claude --json "$@" >"$WORK/$name.raw" 2>"$WORK/$name.err"; then
    body "$WORK/$name.raw" "$WORK/$name.json"
    return 0
  fi
  : >"$WORK/$name.json"
  return 1
}

say "0/7 preflight"
if cli doctor >"$WORK/doctor.txt" 2>&1; then
  pass "the scratch credential authenticates against $CLI_URL"
else
  die "the scratch server answered nothing on $CLI_URL: $(excerpt "$WORK/doctor.txt")"
fi
# Proved before any step depends on it. A lumberroom-server:0.4.0 image predating Phase 6 answers every health
# check and 404s the thirteen ingest routes, which would fail all six steps for one reason.
if ! cli ingest list --json >"$WORK/routes.txt" 2>&1; then
  die "the scratch server has no /admin/ingest routes, so the image predates ingestion. Rebuild it
  with \`docker compose build server\`. Detail: $(excerpt "$WORK/routes.txt")"
fi

say "1/7 a digest span produces zero proposals"
new_fixture_dir
python3 - "$DIR/session1.jsonl" "$NONCE" <<'PY'
import json, sys
path, nonce = sys.argv[1], sys.argv[2]
# The literal E3 watches for, in a plain-string `user` entry: the shape the digest actually arrives
# in, and the one shape that reaches `owner_typed` if nothing stops it.
text = ("Durable memory for this user, retrieved automatically at session start from their own "
        f"memory server. The {nonce} support line callback number is 555-0100.")
line = {"type": "user", "uuid": f"u-{nonce}-1", "timestamp": "2026-08-20T00:00:00Z",
        "sessionId": f"fx-{nonce}", "cwd": "/tmp/lumberroom-ingest-test",
        "message": {"role": "user", "content": text}}
open(path, "w").write(json.dumps(line) + "\n")
PY
if plan plan1; then
  CUT="$(count "$WORK/plan1.json" counters.spans_cut)"
  BACKSTOP="$(count "$WORK/plan1.json" counters.backstop.digest_preamble)"
  SPEAKERS="$(count "$WORK/plan1.json" counters.speakers.owner_typed)"
  [ "$CUT" = "0" ] && pass "the digest entry cut zero spans" \
    || fail "the digest entry cut $CUT spans, wanted 0: $(excerpt "$WORK/plan1.json")"
  [ "$BACKSTOP" = "1" ] && pass "the digest_preamble backstop counted the entry it dropped" \
    || fail "backstop.digest_preamble is $BACKSTOP, wanted 1: $(excerpt "$WORK/plan1.json")"
  [ "$SPEAKERS" = "0" ] && pass "no speaker was counted, so nothing reached an extractor" \
    || fail "owner_typed is $SPEAKERS, wanted 0: the digest reached the owner's slot"
else
  fail "ingest plan failed: $(excerpt "$WORK/plan1.err")"
fi

say "2/7 an attachment entry is excluded and counted by subtype"
new_fixture_dir
python3 - "$DIR/session2.jsonl" "$NONCE" <<'PY'
import json, sys
path, nonce = sys.argv[1], sys.argv[2]
# E1 drops a whole `attachment` entry whatever its subtype, and counts a subtype nobody has met.
# The second line is the release-canary half: it has to land in unknown_types, not pass unremarked.
lines = [
    {"type": "attachment", "uuid": f"u-{nonce}-att", "timestamp": "2026-08-20T00:01:00Z",
     "sessionId": f"fx-{nonce}", "cwd": "/tmp/lumberroom-ingest-test",
     "attachment": {"type": "hook_success", "text": f"hook output holding {nonce}"}},
    {"type": "attachment", "uuid": f"u-{nonce}-att2", "timestamp": "2026-08-20T00:01:01Z",
     "attachment": {"type": "ingest_test_new_subtype", "text": f"unmet subtype holding {nonce}"}},
]
open(path, "w").write("".join(json.dumps(x) + "\n" for x in lines))
PY
if plan plan2; then
  ATT="$(count "$WORK/plan2.json" counters.entries_excluded.attachment)"
  NEW="$(count "$WORK/plan2.json" counters.unknown_types.attachment_subtype:ingest_test_new_subtype)"
  CUT="$(count "$WORK/plan2.json" counters.spans_cut)"
  [ "$ATT" = "2" ] && pass "both attachment entries were excluded by the attachment rule" \
    || fail "entries_excluded.attachment is $ATT, wanted 2: $(excerpt "$WORK/plan2.json")"
  [ "$NEW" = "1" ] && pass "the unmet subtype landed in unknown_types instead of passing quietly" \
    || fail "the unmet attachment subtype was not counted: $(excerpt "$WORK/plan2.json")"
  [ "$CUT" = "0" ] && pass "no span was cut from an attachment" \
    || fail "attachments cut $CUT spans, wanted 0"
else
  fail "ingest plan failed: $(excerpt "$WORK/plan2.err")"
fi

say "3/7 a memory-tool result is excluded, a Read result is not"
new_fixture_dir
python3 - "$DIR/session3.jsonl" "$NONCE" <<'PY'
import json, sys
path, nonce = sys.argv[1], sys.argv[2]
# E2 is a join: a `tool_result` carries no tool name, only the `tool_use_id` of the `tool_use` that
# produced it. So each result needs its own `tool_use` earlier in the same file, or the parser drops
# it as unjoined and the step proves nothing about memory tools.
lines = [
    {"type": "assistant", "uuid": f"u-{nonce}-a1", "timestamp": "2026-08-20T00:02:00Z",
     "sessionId": f"fx-{nonce}", "cwd": "/tmp/lumberroom-ingest-test",
     "message": {"role": "assistant", "content": [
         {"type": "text", "text": f"checking the store for {nonce}"},
         {"type": "tool_use", "id": f"t-mem-{nonce}", "name": "mcp__lumberroom__memory_search",
          "input": {"query": nonce}}]}},
    {"type": "user", "uuid": f"u-{nonce}-r1", "timestamp": "2026-08-20T00:02:01Z",
     "message": {"role": "user", "content": [
         {"type": "tool_result", "tool_use_id": f"t-mem-{nonce}",
          "content": f"MEMTEXT-{nonce} recalled from the store"}]}},
    {"type": "assistant", "uuid": f"u-{nonce}-a2", "timestamp": "2026-08-20T00:02:02Z",
     "message": {"role": "assistant", "content": [
         {"type": "text", "text": "now reading a file"},
         {"type": "tool_use", "id": f"t-read-{nonce}", "name": "Read", "input": {}}]}},
    {"type": "user", "uuid": f"u-{nonce}-r2", "timestamp": "2026-08-20T00:02:03Z",
     "message": {"role": "user", "content": [
         {"type": "tool_result", "tool_use_id": f"t-read-{nonce}",
          "content": f"READTEXT-{nonce} the file body"}]}},
]
open(path, "w").write("".join(json.dumps(x) + "\n" for x in lines))
PY
if plan plan3 --include-tool-output; then
  MEMTOOL="$(count "$WORK/plan3.json" counters.entries_excluded.memory_tool)"
  RETURNED="$(count "$WORK/plan3.json" counters.speakers.tool_returned)"
  WORKLIST="$(field "$WORK/plan3.json" worklist)"
  # Two: the memory tool's `tool_use` on the assistant entry, and its result on the user entry. The
  # first is what supplies the name the second is refused by.
  [ "$MEMTOOL" = "2" ] && pass "the memory tool's use and its result were both excluded" \
    || fail "entries_excluded.memory_tool is $MEMTOOL, wanted 2: $(excerpt "$WORK/plan3.json")"
  [ "$RETURNED" = "1" ] && pass "one tool_returned speaker, the Read result, survived" \
    || fail "speakers.tool_returned is $RETURNED, wanted 1: $(excerpt "$WORK/plan3.json")"
  if [ -n "$WORKLIST" ] && [ -f "$WORKLIST" ]; then
    RUN_DIR="$(dirname "$WORKLIST")"
    if grep -rqF "MEMTEXT-$NONCE" "$RUN_DIR"; then
      fail "the memory tool's output reached $RUN_DIR, where an extractor would read it"
    else
      pass "the memory tool's output is nowhere in the run directory"
    fi
    if grep -rqF "READTEXT-$NONCE" "$RUN_DIR"; then
      pass "the Read result reached the chunk an extractor is handed"
    else
      fail "the Read result never reached $RUN_DIR, so the exclusion is too wide"
    fi
  else
    fail "plan named no worklist, so its output could not be read: $(excerpt "$WORK/plan3.json")"
  fi
else
  fail "ingest plan failed: $(excerpt "$WORK/plan3.err")"
fi

say "4/7 a sensitive path is refused before the file is opened"
new_fixture_dir
mkdir -p "$DIR/.ssh"
# Valid JSONL a plan would happily classify, so the only thing keeping it out is the path rule.
python3 - "$DIR/.ssh/id_rsa.jsonl" "$NONCE" <<'PY'
import json, sys
path, nonce = sys.argv[1], sys.argv[2]
line = {"type": "user", "uuid": f"u-{nonce}-ssh", "timestamp": "2026-08-20T00:04:00Z",
        "sessionId": f"fx-{nonce}", "cwd": "/tmp/lumberroom-ingest-test",
        "message": {"role": "user", "content": f"SSHTEXT-{nonce} the private key passphrase"}}
open(path, "w").write(json.dumps(line) + "\n")
PY
if plan plan4; then
  SENS="$(count "$WORK/plan4.json" counters.files_skipped.sensitive_path)"
  SEEN="$(count "$WORK/plan4.json" counters.entries_seen)"
  WORKLIST="$(field "$WORK/plan4.json" worklist)"
  [ "$SENS" = "1" ] && pass "the .ssh directory was skipped and counted by the sensitive_path rule" \
    || fail "files_skipped.sensitive_path is $SENS, wanted 1: $(excerpt "$WORK/plan4.json")"
  [ "$SEEN" = "0" ] && pass "zero entries were read, so the file was named and never opened" \
    || fail "the run read $SEEN entries from a fixture whose only file sits under .ssh"
  if [ -n "$WORKLIST" ] && [ -f "$WORKLIST" ]; then
    if grep -rqF "SSHTEXT-$NONCE" "$(dirname "$WORKLIST")"; then
      fail "the contents of the sensitive file reached $(dirname "$WORKLIST")"
    else
      pass "nothing from under .ssh reached the run directory"
    fi
  else
    fail "plan named no worklist, so its output could not be read: $(excerpt "$WORK/plan4.json")"
  fi
else
  fail "ingest plan failed: $(excerpt "$WORK/plan4.err")"
fi

say "5/7 a second plan over an unchanged fixture cuts zero spans"
new_fixture_dir
python3 - "$DIR/session5.jsonl" "$NONCE" <<'PY'
import json, sys
path, nonce = sys.argv[1], sys.argv[2]
line = {"type": "user", "uuid": f"u-{nonce}-wm", "timestamp": "2026-08-20T00:03:00Z",
        "sessionId": f"fx-{nonce}", "cwd": "/tmp/lumberroom-ingest-test",
        "message": {"role": "user", "content": f"the {nonce} watermark fact, plain text"}}
open(path, "w").write(json.dumps(line) + "\n")
PY
if plan plan5a; then
  RUN_ID="$(field "$WORK/plan5a.json" run_id)"
  WORKLIST="$(field "$WORK/plan5a.json" worklist)"
  CUT_A="$(count "$WORK/plan5a.json" counters.spans_cut)"
  if [ "$CUT_A" = "1" ] && [ -n "$RUN_ID" ]; then
    pass "the first plan cut one span from the fixture"
  else
    fail "the first plan cut $CUT_A spans, wanted 1: $(excerpt "$WORK/plan5a.json")"
  fi
  if [ -n "$WORKLIST" ] && [ -f "$WORKLIST" ]; then
    # An empty answer per chunk, which is what an extractor returns for a chunk holding nothing
    # durable and the common case on a real corpus. Written from the worklist's own chunk indices
    # rather than a guessed filename, so a change to the chunk numbering shows up as a failure here
    # instead of as a submit that quietly held every byte back.
    python3 - "$WORKLIST" <<'PY'
import json, os, sys
worklist = sys.argv[1]
run_dir = os.path.dirname(worklist)
w = json.load(open(worklist))
out = os.path.join(run_dir, "out")
os.makedirs(out, exist_ok=True)
for chunk in w["chunks"]:
    with open(os.path.join(out, "chunk-%02d.json" % chunk["index"]), "w") as f:
        json.dump({"facts": [], "refusal": "<no-facts/>"}, f)
PY
    if cli ingest submit --run "$RUN_ID" --no-auto --json \
        >"$WORK/submit5.raw" 2>"$WORK/submit5.err"; then
      body "$WORK/submit5.raw" "$WORK/submit5.json"
      HELD="$(count "$WORK/submit5.json" files_held_back)"
      [ "$HELD" = "0" ] && pass "submit held no file back, so the watermark moved to the ceiling" \
        || fail "submit held $HELD files back: $(excerpt "$WORK/submit5.json")"
      if plan plan5b; then
        CUT_B="$(count "$WORK/plan5b.json" counters.spans_cut)"
        SEEN_B="$(count "$WORK/plan5b.json" counters.entries_seen)"
        [ "$CUT_B" = "0" ] && [ "$SEEN_B" = "0" ] \
          && pass "the second plan re-read nothing and cut zero spans" \
          || fail "the second plan read $SEEN_B entries and cut $CUT_B spans, wanted 0 and 0"
      else
        fail "the second ingest plan failed: $(excerpt "$WORK/plan5b.err")"
      fi
    else
      fail "ingest submit failed: $(excerpt "$WORK/submit5.err")"
    fi
  else
    fail "plan named no worklist: $(excerpt "$WORK/plan5a.json")"
  fi
else
  fail "ingest plan failed: $(excerpt "$WORK/plan5a.err")"
fi

say "6/7 ingest list answers, and a client without may_ingest gets 403"
if cli ingest list --json >"$WORK/list6.txt" 2>&1; then
  pass "ingest list answered for the credential carrying mayIngest"
else
  fail "ingest list failed for a credential with mayIngest: $(excerpt "$WORK/list6.txt")"
fi
DENIED_CODE=0
cli_as "$NO_INGEST_TOKEN" "$WORK/cfg-denied.json" "$WORK/state-denied" ingest list --json \
  >"$WORK/list6-denied.txt" 2>&1 || DENIED_CODE=$?
# Exit 2 is the client's code for 401 and 403 alone, which is why it is checked beside the text: a
# server that answered 500 with the word "forbidden" in it would otherwise pass this.
if [ "$DENIED_CODE" = "2" ] && grep -qi "403" "$WORK/list6-denied.txt"; then
  pass "the credential without mayIngest was refused with 403 and exit 2"
else
  fail "the no-mayIngest credential was not cleanly refused (exit $DENIED_CODE): $(excerpt "$WORK/list6-denied.txt")"
fi

say "7/7 a batch submits, splits into out/, and its chunks reach submit"
new_fixture_dir
python3 - "$DIR/session7.jsonl" "$NONCE" <<'PY'
import json, sys
path, nonce = sys.argv[1], sys.argv[2]
# One owner-typed span, so the run has exactly one chunk and one span id for the stub to answer
# against. Nothing here tests extraction quality; the stub decides what comes back.
line = {"type": "user", "uuid": f"u-{nonce}-batch", "timestamp": "2026-08-20T00:05:00Z",
        "sessionId": f"fx-{nonce}", "cwd": "/tmp/lumberroom-ingest-test",
        "message": {"role": "user", "content": f"the {nonce} batch fixture line, plain text"}}
open(path, "w").write(json.dumps(line) + "\n")
PY

# A provider stub, not OpenRouter. It answers what batch.rs believes a batch endpoint answers:
# a 202 carrying an id, then a completed job whose results array is keyed by the custom ids the
# submission sent. What it cannot settle is whether OpenRouter agrees, which is why the module
# comment in batch.rs says no batch has ever gone to a real provider.
cat >"$WORK/batch-stub.py" <<'PY'
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

port_file, span_id, nonce = sys.argv[1], sys.argv[2], sys.argv[3]
state = {"created": 0, "custom_ids": []}


def fact(index):
    return {"facts": [{
        "content": f"the {nonce} batch fixture line is how step 7 proves Mode C reaches submit",
        "namespace": "project:ingest-test",
        "source_span_id": span_id,
        "speaker": "owner_typed",
        "confidence": "high",
    }]}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _send(self, code, body):
        raw = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(n) or b"{}")
        state["created"] += 1
        state["custom_ids"] = [r.get("custom_id") for r in body.get("requests", [])]
        self._send(202, {"id": "batch_stub_1", "status": "validating"})

    def do_GET(self):
        # The count is how the script asks whether a poll created a second batch.
        if self.path == "/created":
            return self._send(200, {"created": state["created"]})
        results = []
        for index, cid in enumerate(state["custom_ids"]):
            content = json.dumps(fact(index))
            results.append({"custom_id": cid, "error": None, "response": {
                "status_code": 200, "request_id": f"r-{index}",
                "body": {"choices": [{"message": {"content": content}}]}}})
        self._send(200, {
            "id": "batch_stub_1",
            "status": "completed",
            "request_counts": {"total": len(results), "completed": len(results), "failed": 0},
            "usage": {"prompt_tokens": 111, "completion_tokens": 22},
            "results": results,
        })


server = HTTPServer(("0.0.0.0", 0), Handler)
with open(port_file, "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
PY

if plan plan7; then
  RUN_ID="$(field "$WORK/plan7.json" run_id)"
  WORKLIST="$(field "$WORK/plan7.json" worklist)"
  if [ -z "$RUN_ID" ] || [ -z "$WORKLIST" ] || [ ! -f "$WORKLIST" ]; then
    fail "plan named no worklist, so the batch step had nothing to send: $(excerpt "$WORK/plan7.json")"
  else
    RUN_DIR="$(dirname "$WORKLIST")"
    SPAN_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["spans"][0]["id"])' "$WORKLIST")"
    rm -f "$WORK/stub-port"
    python3 "$WORK/batch-stub.py" "$WORK/stub-port" "$SPAN_ID" "$NONCE" >"$WORK/stub.log" 2>&1 &
    STUB_PID=$!
    i=0
    while [ ! -s "$WORK/stub-port" ]; do
      i=$((i + 1))
      [ "$i" -ge 50 ] && break
      sleep 0.1
    done
    STUB_PORT="$(cat "$WORK/stub-port" 2>/dev/null || true)"
    # The CLI runs in a container by default and the stub listens on the host, so the endpoint it
    # is given is not the one this script polls.
    if [ -n "$HOST_CLI" ]; then
      STUB_HOST="127.0.0.1"
    else
      STUB_HOST="host.docker.internal"
    fi
    if [ -z "$STUB_PORT" ]; then
      fail "the batch stub did not report a port: $(excerpt "$WORK/stub.log")"
    else
      # A config file of its own, at 0600, because the CLI refuses to read one that group or other
      # can. The endpoint key is the only way a provider other than OpenRouter gets a batch at all.
      python3 - "$WORK/cfg-batch.json" "http://$STUB_HOST:$STUB_PORT/batches" <<'PY'
import json, os, sys
path, endpoint = sys.argv[1], sys.argv[2]
json.dump({"ingest": {"providers": {"custom": {"batch": {"endpoint": endpoint}}}}}, open(path, "w"))
os.chmod(path, 0o600)
PY
      batch_cli() {
        cli_as "$TOKEN" "$WORK/cfg-batch.json" "$WORK/state" ingest extract --run "$RUN_ID" \
          --provider custom --model stub-model --base-url "http://$STUB_HOST:$STUB_PORT/v1" \
          --yes "$@"
      }
      STATE_JSON="$RUN_DIR/state.json"

      if batch_cli --batch >"$WORK/batch-submit.txt" 2>&1; then
        BATCH_ID="$(field "$STATE_JSON" batch.id)"
        [ "$BATCH_ID" = "batch_stub_1" ] \
          && pass "the batch id reached state.json, so the results survive a lost out/" \
          || fail "state.json holds batch id '$BATCH_ID', wanted batch_stub_1: $(excerpt "$WORK/batch-submit.txt")"
        # The consent block prints whatever --yes says: a cron log with no record of where a corpus
        # went is not a log.
        if grep -q "the spans go to $STUB_HOST" "$WORK/batch-submit.txt" \
          && grep -q "30 days" "$WORK/batch-submit.txt"; then
          pass "the submission named its destination and the retention it accepts"
        else
          fail "the batch went out without naming the host or the retention: $(excerpt "$WORK/batch-submit.txt")"
        fi

        # --batch-status prints and creates nothing. A second batch here is a second bill.
        batch_cli --batch-status >"$WORK/batch-status.txt" 2>&1 || true
        CREATED="$(curl -sf "http://127.0.0.1:$STUB_PORT/created" | python3 -c 'import json,sys; print(json.load(sys.stdin)["created"])' 2>/dev/null || echo -1)"
        if [ "$CREATED" = "1" ] && [ ! -f "$RUN_DIR/out/chunk-00.json" ]; then
          pass "--batch-status polled the job and neither resent it nor wrote a chunk"
        else
          fail "--batch-status left $CREATED batches created and out/chunk-00.json present: $(excerpt "$WORK/batch-status.txt")"
        fi

        if batch_cli --batch >"$WORK/batch-fetch.txt" 2>&1; then
          MISSING=0
          for f in $(python3 -c 'import json,sys; w=json.load(open(sys.argv[1])); print("\n".join("chunk-%02d.json" % c["index"] for c in w["chunks"]))' "$WORKLIST"); do
            [ -f "$RUN_DIR/out/$f" ] || MISSING=$((MISSING + 1))
          done
          [ "$MISSING" = "0" ] \
            && pass "the completed batch split into one out/ file per chunk" \
            || fail "$MISSING chunks came back with no out/ file: $(excerpt "$WORK/batch-fetch.txt")"
        else
          fail "splitting the completed batch failed: $(excerpt "$WORK/batch-fetch.txt")"
        fi

        # The point of the whole mode: a batched chunk is indistinguishable to submit, and the
        # proposal records which provider and model produced it.
        if cli_as "$TOKEN" "$WORK/cfg-batch.json" "$WORK/state" ingest submit --run "$RUN_ID" \
            --no-auto >"$WORK/batch-submit-stage.txt" 2>&1; then
          if grep -q "extractor provider:custom/stub-model" "$WORK/batch-submit-stage.txt"; then
            pass "submit read the batch's chunks and credited provider:custom/stub-model"
          else
            fail "submit did not credit the batch's provider: $(excerpt "$WORK/batch-submit-stage.txt")"
          fi
        else
          fail "ingest submit failed on a batched run: $(excerpt "$WORK/batch-submit-stage.txt")"
        fi
      else
        fail "the batch did not go out: $(excerpt "$WORK/batch-submit.txt")"
      fi
    fi
    kill "$STUB_PID" >/dev/null 2>&1 || true
    STUB_PID=""
  fi
else
  fail "ingest plan failed: $(excerpt "$WORK/plan7.err")"
fi

say "what each step proved"
cat <<SUMMARY
  1  a digest entry cuts nothing, and the text-level backstop counts the entry it dropped
  2  an attachment is excluded whatever its subtype, and an unmet subtype is counted rather than
     passed over
  3  a tool_result joined to a memory tool is excluded, its text is nowhere in the run directory,
     and a Read result still reaches the chunk an extractor is handed
  4  a file under a sensitive directory is skipped by name, never opened, and never quoted
  5  a submit advances the watermark, and the plan after it re-reads none of the same bytes
  6  ingest list answers for a credential with mayIngest and refuses one without it
  7  a batch reaches a provider stub, its id survives in state.json, a status poll resends nothing,
     the completed job splits into out/, and submit credits the provider that produced it
SUMMARY

echo ""
echo "  server $CLI_URL, database $TEST_DB, fixtures under $WORK"
echo "  $PASSED passed, $FAILED failed"
if [ "$FAILED" -gt 0 ]; then
  echo "  ingest-test FAILED"
  exit 1
fi
if [ "$PASSED" -eq 0 ]; then
  echo "  ingest-test reported zero passes, which is not a pass"
  exit 1
fi
echo "  ingest-test PASSED"
exit 0
