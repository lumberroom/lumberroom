#!/bin/sh
# Runs a list of test binaries N at a time and reports the way `cargo test` does.
#
#   TEST_POOL_JOBS=4 sh scripts/lib/test-pool.sh <list> <log dir> [args for every binary]
#
# scripts/lib/test-fast.sh builds the list and calls this inside the builder container. Each line
# of <list> is four tab-separated fields: kind (lib, bin, test, doc), target name, the directory to
# run in, and the executable. `cargo test` runs each binary from its package root, so the pool does.
#
# The unit is a whole binary because the suite's shared state is scoped per binary. Several
# binaries keep a `OnceLock` config, a `static SERIAL` mutex or an `env::set_var` that every test in
# the binary relies on, and the binaries that share a database queue on a Postgres advisory lock
# (tests/common/mod.rs). Running whole binaries side by side keeps all of that as `cargo test` has
# it and only removes the wait between them.
#
# One log per binary, named <kind>-<name>.log, and results.log with one line per binary: label,
# exit status, seconds. Every file here ends in .log so git ignores the directory without a
# .gitignore entry.
#
# TEST_POOL_DURATIONS names a file of "label<TAB>seconds" lines from an earlier run. When it is
# there the pool starts the slowest binaries first, so the one that takes 90 seconds does not start
# last and set the wall clock on its own. It is rewritten at the end of every run.
#
# scripts/test-pool-test.sh is the test for this file.
set -u

TAB="$(printf '\t')"

# ── one binary, called back through xargs ─────────────────────────────────────────────────────────
if [ "${1:-}" = "__one" ]; then
  n="$2" list="$3" logs="$4"
  shift 4
  IFS="$TAB" read -r kind name dir exe <<EOF
$(sed -n "${n}p" "$list")
EOF
  label="$kind-$name"
  start="$(date +%s)"
  (cd "$dir" && exec "$exe" "$@") >"$logs/$label.log" 2>&1
  status=$?
  secs=$(($(date +%s) - start))
  printf '%s\t%s\t%s\n' "$label" "$status" "$secs" >>"$logs/results.log"
  if [ "$status" = 0 ]; then word=ok; else word="FAILED (exit $status)"; fi
  printf '    finished %s: %s in %ss\n' "$label" "$word" "$secs"
  # Always 0. xargs stops launching jobs the moment one exits 255, and a binary's own status is
  # already in results.log.
  exit 0
fi

if [ $# -lt 2 ]; then
  echo "usage: test-pool.sh <list> <log dir> [args for every binary]" >&2
  exit 2
fi
list="$1" logs="$2"
shift 2
jobs="${TEST_POOL_JOBS:-4}"
case "$jobs" in
  '' | *[!0-9]* | 0)
    echo "test-pool: TEST_POOL_JOBS must be a positive integer, got '$jobs'" >&2
    exit 2
    ;;
esac

mkdir -p "$logs"
: >"$logs/results.log"
ordered="$logs/order.log"
durations="${TEST_POOL_DURATIONS:-}"
if [ -n "$durations" ] && [ -f "$durations" ]; then
  # Unknown binaries sort first: a new one might be the slowest.
  awk -F "$TAB" 'NR == FNR { d[$1] = $2; next }
                 { l = $1 "-" $2; print ((l in d) ? d[l] : 999999) "\t" $0 }' "$durations" "$list" \
    | sort -t "$TAB" -k1,1nr | cut -f2- >"$ordered"
else
  cp "$list" "$ordered"
fi

count="$(grep -c . "$ordered")"
if [ "$count" = 0 ]; then
  # A run with no binaries ran nothing, and a green line over zero tests is the trap docs/traps.md
  # records for the integration suite's silent skip.
  echo "test-pool: the list is empty, so nothing ran" >&2
  exit 1
fi

echo "test-pool: $count binaries, $jobs at a time, logs in $logs"
started="$(date +%s)"
awk '{ print NR }' "$ordered" | xargs -P "$jobs" -I @ sh "$0" __one @ "$ordered" "$logs" "$@"
wall=$(($(date +%s) - started))

# ── the summary ───────────────────────────────────────────────────────────────────────────────────

# The cargo spelling of a target, which is what anybody reruns one with.
selector() {
  case "$1" in
    lib) echo "--lib" ;;
    doc) echo "--doc" ;;
    *) echo "--$1 $2" ;;
  esac
}

passed=0 failed=0 ignored=0 bad=0 ran=0
failed_list=""
while IFS="$TAB" read -r kind name dir exe; do
  label="$kind-$name"
  log="$logs/$label.log"
  status="$(awk -F "$TAB" -v l="$label" '$1 == l { print $2 }' "$logs/results.log")"
  ran=$((ran + 1))
  read -r p f i <<EOF
$(awk '
    /^test result: (ok|FAILED)\. [0-9]+ passed; [0-9]+ failed; [0-9]+ ignored;/ {
      p += $4; f += $6; i += $8
    }
    END { print p + 0, f + 0, i + 0 }' "$log" 2>/dev/null || echo 0 0 0)
EOF
  passed=$((passed + p)) failed=$((failed + f)) ignored=$((ignored + i))
  if [ "$status" != 0 ]; then
    bad=$((bad + 1))
    failed_list="$failed_list    \`$(selector "$kind" "$name")\`
"
    printf '\n==== %s failed (exit %s), log %s ====\n' "$(selector "$kind" "$name")" "${status:-none}" "$log"
    blocks="$(awk '/^---- .* (stdout|stderr) ----$/ { p = 1 } p && /^failures:$/ { p = 0 } p { print }' "$log")"
    if [ -n "$blocks" ]; then
      printf '%s\n' "$blocks"
    else
      # No libtest failure block: it crashed, was killed, or failed before its harness started.
      echo "(no failure block; the last 30 lines of the log)"
      tail -n 30 "$log"
    fi
  fi
done <"$ordered"

echo
echo "slowest:"
sort -t "$TAB" -k3,3nr "$logs/results.log" | head -n 5 | awk -F "$TAB" '{ printf "    %5ss  %s\n", $3, $1 }'

if [ -n "$durations" ]; then
  cut -f1,3 "$logs/results.log" >"$durations.tmp" && mv "$durations.tmp" "$durations"
fi

if [ "$bad" = 0 ]; then word=ok; else word=FAILED; fi
echo
echo "test result: $word. $passed passed; $failed failed; $ignored ignored; $ran binaries, $jobs at a time; finished in ${wall}s"
if [ "$bad" != 0 ]; then
  if [ "$bad" = 1 ]; then noun=target; else noun=targets; fi
  printf 'error: %s %s failed:\n%s' "$bad" "$noun" "$failed_list"
  exit 101
fi
exit 0
