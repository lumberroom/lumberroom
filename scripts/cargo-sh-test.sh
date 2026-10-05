#!/bin/sh
# Proves which database and which `-j` scripts/cargo.sh hands the builder container, against a fake
# `docker` on PATH.
#
#   ./scripts/cargo-sh-test.sh
#
# The test suites truncate whatever DATABASE_URL names, so a cargo.sh that routes `test` to the
# developer's own store, or accepts an override naming it, destroys data on the first run. And a
# default `-j` above what Docker's memory can link OOM-kills the linker. Each case runs a copy of the
# real cargo.sh with a fake `docker` that records its arguments and starts nothing, then asserts on
# the `docker run` and `docker compose` lines it recorded.
#
# Needs nothing but a POSIX shell and grep, so it runs on the host without Docker.
#
# SC2015: `check && pass || fail` is safe here because pass only prints and counts.
# shellcheck disable=SC2015
set -u

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

PASSED=0
FAILED=0
pass() { printf '  PASS  %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '  FAIL  %s\n' "$*"; FAILED=$((FAILED + 1)); }
say() { printf '\n%s\n' "$*"; }

WORK="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/cargo-sh-test.XXXXXX")" && pwd)"
trap 'rm -rf "$WORK"' EXIT

# A checkout with no .env, so the developer's own credentials never reach a case.
mkdir -p "$WORK/repo/scripts" "$WORK/bin"
cp "$REPO_DIR/scripts/cargo.sh" "$WORK/repo/scripts/cargo.sh"

# The fake answers only what cargo.sh asks. FAKE_MEM unset makes `docker info` fail, as it does
# with the daemon unreachable.
cat >"$WORK/bin/docker" <<'EOF'
#!/bin/sh
echo "docker $*" >>"$FAKE_LOG"
case "$1" in
  image) echo mold ;;
  info) [ -n "${FAKE_MEM:-}" ] || exit 1; echo "$FAKE_MEM" ;;
  inspect) echo "${FAKE_TESTDB_HEALTH:-}" ;;
  compose)
    # Real compose refuses any command while a `:?` variable in any service is unset.
    [ -n "${LUMBERROOM_DOMAIN:-}" ] || { echo "LUMBERROOM_DOMAIN missing" >&2; exit 1; }
    case "$*" in
      *" ps "*) [ -n "${FAKE_TESTDB_HEALTH:-}" ] && echo fakeid ;;
      *" up "*) exit "${FAKE_UP_STATUS:-0}" ;;
    esac ;;
esac
exit 0
EOF
chmod +x "$WORK/bin/docker"

GIB=1073741824

# run [VAR=value ...] -- <cargo.sh arguments>
run() {
  : >"$WORK/log"
  envs=""
  while [ "$1" != "--" ]; do envs="$envs $1"; shift; done
  shift
  # shellcheck disable=SC2086
  OUT="$(env -i PATH="$WORK/bin:/usr/bin:/bin" HOME="$WORK" FAKE_LOG="$WORK/log" \
    POSTGRES_USER=u POSTGRES_PASSWORD=p $envs sh "$WORK/repo/scripts/cargo.sh" "$@" 2>&1)"
  STATUS=$?
  RUNLINE="$(grep -F -- '--name lumberroom-cargo-' "$WORK/log" || true)"
}
ran() { printf '%s\n' "$RUNLINE" | grep -F -- "$1" >/dev/null; }
logged() { grep -F -- "$1" "$WORK/log" >/dev/null; }

say "test: -j follows Docker's memory"
run FAKE_MEM=$((16 * GIB)) FAKE_TESTDB_HEALTH=healthy -- test
ran "cargo test -j 4" && pass "16 GiB gives -j 4" || fail "16 GiB: $RUNLINE"
run FAKE_MEM=$((12 * GIB)) FAKE_TESTDB_HEALTH=healthy -- test
ran "cargo test -j 4" && pass "12 GiB gives -j 4" || fail "12 GiB: $RUNLINE"
run FAKE_MEM=$((12 * GIB - 1)) FAKE_TESTDB_HEALTH=healthy -- test
ran "cargo test -j 1" && pass "just under 12 GiB gives -j 1" || fail "under 12 GiB: $RUNLINE"
run FAKE_TESTDB_HEALTH=healthy -- test
ran "cargo test -j 1" && pass "a failed docker info gives -j 1" || fail "no info: $RUNLINE"
run FAKE_MEM=garbage FAKE_TESTDB_HEALTH=healthy -- test
ran "cargo test -j 1" && pass "a non-number gives -j 1" || fail "garbage: $RUNLINE"

say "an explicit -j wins"
run FAKE_MEM=$((16 * GIB)) FAKE_TESTDB_HEALTH=healthy -- test -j 2
ran "cargo test -j 2" && pass "-j 2 kept" || fail "-j 2: $RUNLINE"
ran "-j 4" && fail "a second -j added: $RUNLINE" || pass "no second -j"

say "test-fast takes the same -j and the same database"
run FAKE_MEM=$((16 * GIB)) FAKE_TESTDB_HEALTH=healthy -- test-fast
ran "sh /app/scripts/lib/test-fast.sh -j 4" && pass "test-fast -j 4" || fail "test-fast: $RUNLINE"
ran "DATABASE_URL=postgres://u:p@testdb:5432/lumberroom_test" && pass "test-fast on testdb" \
  || fail "test-fast url: $RUNLINE"

say "test runs on testdb"
run FAKE_MEM=$((16 * GIB)) FAKE_TESTDB_HEALTH=healthy -- test
ran "DATABASE_URL=postgres://u:p@testdb:5432/lumberroom_test" && pass "DATABASE_URL names testdb" \
  || fail "url: $RUNLINE"
logged "compose --profile test up" && fail "started a healthy testdb again" || pass "a healthy testdb is left alone"
run FAKE_MEM=$((16 * GIB)) -- test
logged "compose --profile test up -d --wait testdb" && pass "a missing testdb is started" \
  || fail "no start: $(cat "$WORK/log")"
run FAKE_MEM=$((16 * GIB)) FAKE_TESTDB_HEALTH=starting -- test
logged "compose --profile test up -d --wait testdb" && pass "an unhealthy testdb is waited on" \
  || fail "no wait: $(cat "$WORK/log")"

say "testdb starts with LUMBERROOM_DOMAIN unset, as a development .env leaves it"
run FAKE_MEM=$((16 * GIB)) -- test
[ "$STATUS" = 0 ] && ran "@testdb:5432/" && pass "started without a domain" || fail "no domain: $STATUS $OUT"
run FAKE_MEM=$((16 * GIB)) LUMBERROOM_DOMAIN=real.example -- test
[ "$STATUS" = 0 ] && pass "a set domain still works" || fail "set domain: $STATUS $OUT"

say "a testdb that will not start stops the run"
run FAKE_MEM=$((16 * GIB)) FAKE_UP_STATUS=1 -- test
[ "$STATUS" != 0 ] && pass "exit non-zero ($STATUS)" || fail "exit 0"
[ -z "$RUNLINE" ] && pass "cargo never ran" || fail "ran: $RUNLINE"
printf '%s' "$OUT" | grep -F "testdb" >/dev/null && pass "the message names testdb" || fail "message: $OUT"

say "other subcommands keep the store's DATABASE_URL and their own -j"
run FAKE_MEM=$((16 * GIB)) -- check --all-targets
ran "DATABASE_URL=postgres://u:p@db:5432/lumberroom " && pass "check on db" || fail "check url: $RUNLINE"
ran "cargo check --all-targets" && pass "check gets no -j" || fail "check: $RUNLINE"
logged "compose" && fail "check started testdb" || pass "check leaves testdb alone"

say "LUMBERROOM_TEST_DATABASE_URL overrides, unless it names the store"
run FAKE_MEM=$((16 * GIB)) LUMBERROOM_TEST_DATABASE_URL=postgres://a:b@elsewhere:5432/scratch -- test
ran "DATABASE_URL=postgres://a:b@elsewhere:5432/scratch " && pass "override used" || fail "override: $RUNLINE"
logged "compose" && fail "override still started testdb" || pass "override skips testdb"

refused() {
  [ "$STATUS" != 0 ] && [ -z "$RUNLINE" ] && printf '%s' "$OUT" | grep -F "LUMBERROOM_TEST_DATABASE_URL" >/dev/null
}
run LUMBERROOM_TEST_DATABASE_URL=postgres://a:b@elsewhere:5432/lumberroom -- test
refused && pass "the default store name refused" || fail "store name: $STATUS $RUNLINE $OUT"
run POSTGRES_DB=mine LUMBERROOM_TEST_DATABASE_URL=postgres://a:b@elsewhere:5432/mine -- test
refused && pass "a custom POSTGRES_DB refused" || fail "custom store: $STATUS $RUNLINE $OUT"
run LUMBERROOM_TEST_DATABASE_URL='postgres://a:b@elsewhere:5432/lumberroom?sslmode=disable' -- test-fast
refused && pass "the store name behind a query string refused" || fail "query: $STATUS $RUNLINE $OUT"
run LUMBERROOM_TEST_DATABASE_URL=postgres://a:b@elsewhere:5432/ -- test
refused && pass "an empty database name refused" || fail "empty: $STATUS $RUNLINE $OUT"
run LUMBERROOM_TEST_DATABASE_URL=postgres://a:b@elsewhere:5432 -- test
refused && pass "no database name refused" || fail "none: $STATUS $RUNLINE $OUT"

printf '\n%s passed, %s failed\n' "$PASSED" "$FAILED"
[ "$FAILED" = 0 ]
