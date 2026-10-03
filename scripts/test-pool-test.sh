#!/bin/sh
# Proves what scripts/lib/test-pool.sh reports, against fake test binaries.
#
#   ./scripts/test-pool-test.sh
#
# The pool runs the suite's test binaries N at a time and then adds up what they printed. Its
# summary is the only thing anybody reads after a parallel run, so a pool that miscounts, drops a
# failure, or exits 0 over a red binary turns a broken suite green. Each case below builds fakes that
# print what libtest prints, runs the real pool over them, and asserts on the summary and the exit.
#
# Needs nothing but a POSIX shell, awk and xargs, so it runs on the host and in the builder image.
#
# SC2015: `check && pass || fail` is safe here because pass only prints and counts.
# SC2016: the single-quoted backticks are literal text the pool prints, and the fakes' `$(pwd)` has
# to expand when the fake runs, not when this script writes it.
# shellcheck disable=SC2015,SC2016
set -u

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
POOL="$REPO_DIR/scripts/lib/test-pool.sh"

PASSED=0
FAILED=0
pass() { printf '  PASS  %s\n' "$*"; PASSED=$((PASSED + 1)); }
fail() { printf '  FAIL  %s\n' "$*"; FAILED=$((FAILED + 1)); }
say() { printf '\n%s\n' "$*"; }

# Through `pwd`, because macOS's TMPDIR ends in a slash and the cwd case compares paths as text.
WORK="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/test-pool-test.XXXXXX")" && pwd)"
trap 'rm -rf "$WORK"' EXIT

# fake <file> <passed> <failed> <ignored> [failing test name]
# Prints libtest's shape: a result line, and for a failure the stdout block and the failures list.
fake() {
  file="$1" p="$2" f="$3" i="$4" name="${5:-}"
  {
    echo '#!/bin/sh'
    echo 'echo "running tests"'
    echo 'echo "args: $*"'
    echo 'echo "cwd: $(pwd)"'
    if [ "$f" -gt 0 ]; then
      echo 'echo'
      echo 'echo "failures:"'
      echo 'echo'
      echo "echo '---- $name stdout ----'"
      echo "echo 'thread panicked at tests/$name.rs:12:5: the fake assertion'"
      echo 'echo'
      echo 'echo "failures:"'
      echo "echo '    $name'"
      echo 'echo'
      echo "echo 'test result: FAILED. $p passed; $f failed; $i ignored; 0 measured; 0 filtered out; finished in 0.01s'"
      echo 'exit 101'
    else
      echo "echo 'test result: ok. $p passed; 0 failed; $i ignored; 0 measured; 0 filtered out; finished in 0.01s'"
    fi
  } >"$file"
  chmod +x "$file"
}

# run_pool <list> <jobs> [binary args]; sets OUT, STATUS and LOGS.
run_pool() {
  list="$1" jobs="$2"
  shift 2
  LOGS="$WORK/logs-$(date +%s)-$$-$PASSED-$FAILED"
  OUT="$(TEST_POOL_JOBS="$jobs" sh "$POOL" "$list" "$LOGS" "$@" 2>&1)"
  STATUS=$?
}

has() { printf '%s\n' "$OUT" | grep -F -- "$1" >/dev/null; }

say "every binary passes"
mkdir -p "$WORK/a"
fake "$WORK/a/alpha" 3 0 1
fake "$WORK/a/beta" 5 0 0
printf 'test\talpha\t%s\t%s\ntest\tbeta\t%s\t%s\n' "$WORK" "$WORK/a/alpha" "$WORK" "$WORK/a/beta" >"$WORK/a.list"
run_pool "$WORK/a.list" 2
[ "$STATUS" = 0 ] && pass "exit 0" || fail "exit $STATUS on an all-green run"
has "test result: ok. 8 passed; 0 failed; 1 ignored" && pass "totals add up" \
  || fail "totals: $(printf '%s\n' "$OUT" | grep 'test result' || echo none)"
[ -f "$LOGS/test-alpha.log" ] && [ -f "$LOGS/test-beta.log" ] && pass "one log per binary" \
  || fail "missing a per-binary log in $LOGS: $(find "$LOGS" 2>&1 | tr '\n' ' ')"

say "one binary fails"
mkdir -p "$WORK/b"
fake "$WORK/b/good" 4 0 0
fake "$WORK/b/bad" 2 1 0 the_bad_test
printf 'test\tgood\t%s\t%s\ntest\tbad\t%s\t%s\n' "$WORK" "$WORK/b/good" "$WORK" "$WORK/b/bad" >"$WORK/b.list"
run_pool "$WORK/b.list" 2
[ "$STATUS" != 0 ] && pass "exit non-zero ($STATUS)" || fail "exit 0 over a failing binary"
has "test result: FAILED. 6 passed; 1 failed; 0 ignored" && pass "totals count the failure" \
  || fail "totals: $(printf '%s\n' "$OUT" | grep 'test result' || echo none)"
has '`--test bad`' && pass "the failed binary is named in cargo's form" || fail "no \`--test bad\` in the summary"
if has '`--test good`'; then fail "a passing binary is listed as failed"; else pass "the passing binary is not listed"; fi
has "---- the_bad_test stdout ----" && has "the fake assertion" && pass "the failure's stdout block is printed" \
  || fail "the stdout block is missing"

say "a binary dies without a result line"
mkdir -p "$WORK/c"
printf '#!/bin/sh\necho "about to fall over"\nkill -9 $$\n' >"$WORK/c/crash"
chmod +x "$WORK/c/crash"
printf 'test\tcrash\t%s\t%s\n' "$WORK" "$WORK/c/crash" >"$WORK/c.list"
run_pool "$WORK/c.list" 1
[ "$STATUS" != 0 ] && pass "exit non-zero ($STATUS)" || fail "exit 0 over a crashed binary"
has '`--test crash`' && pass "the crashed binary is named" || fail "the crashed binary is not named"
has "about to fall over" && pass "its log tail is printed" || fail "its log tail is missing"

say "binaries run at the same time"
# Each fake waits until all three have started. Run one at a time, the first gives up after two
# seconds and fails; run three at a time, all three meet.
mkdir -p "$WORK/d"
for n in one two three; do
  cat >"$WORK/d/$n" <<EOF
#!/bin/sh
touch "$WORK/d/started-$n"
i=0
while [ \$i -lt 20 ]; do
  [ -f "$WORK/d/started-one" ] && [ -f "$WORK/d/started-two" ] && [ -f "$WORK/d/started-three" ] && {
    echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s'
    exit 0
  }
  sleep 0.1
  i=\$((i + 1))
done
echo 'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.00s'
exit 101
EOF
  chmod +x "$WORK/d/$n"
  printf 'test\t%s\t%s\t%s\n' "$n" "$WORK" "$WORK/d/$n" >>"$WORK/d.list"
done
run_pool "$WORK/d.list" 3
[ "$STATUS" = 0 ] && has "3 passed; 0 failed" && pass "three jobs meet" || fail "three jobs did not overlap: $OUT"

say "arguments and working directory reach the binary"
mkdir -p "$WORK/e" "$WORK/e/member"
fake "$WORK/e/argv" 1 0 0
printf 'test\targv\t%s\t%s\n' "$WORK/e/member" "$WORK/e/argv" >"$WORK/e.list"
run_pool "$WORK/e.list" 1 --test-threads=1 some_filter
grep -F -- "args: --test-threads=1 some_filter" "$LOGS/test-argv.log" >/dev/null \
  && pass "arguments after the list reach the binary" || fail "args: $(grep args "$LOGS/test-argv.log" 2>&1)"
grep -F -- "cwd: $WORK/e/member" "$LOGS/test-argv.log" >/dev/null \
  && pass "the binary runs in its package directory" || fail "cwd: $(grep cwd "$LOGS/test-argv.log" 2>&1)"

say "lib, bin and doc targets are named the way cargo names them"
mkdir -p "$WORK/f"
fake "$WORK/f/lib" 0 1 0 lib_case
fake "$WORK/f/bin" 0 1 0 bin_case
fake "$WORK/f/doc" 0 1 0 doc_case
printf 'lib\tlumberroom_server\t%s\t%s\nbin\tlumberroom-server\t%s\t%s\ndoc\tlumberroom_server\t%s\t%s\n' \
  "$WORK" "$WORK/f/lib" "$WORK" "$WORK/f/bin" "$WORK" "$WORK/f/doc" >"$WORK/f.list"
run_pool "$WORK/f.list" 3
has '`--lib`' && pass "--lib" || fail "no \`--lib\`"
has '`--bin lumberroom-server`' && pass "--bin <name>" || fail "no \`--bin lumberroom-server\`"
has '`--doc`' && pass "--doc" || fail "no \`--doc\`"

say "an empty list is a failure"
: >"$WORK/g.list"
run_pool "$WORK/g.list" 2
[ "$STATUS" != 0 ] && pass "exit non-zero ($STATUS)" || fail "exit 0 with nothing run"
if has "test result: ok"; then fail "a green line over zero binaries"; else pass "no green line"; fi

printf '\n%s passed, %s failed\n' "$PASSED" "$FAILED"
[ "$FAILED" = 0 ]
