#!/bin/sh
# Builds every test binary once, then runs them N at a time. Runs inside the builder container;
# call it through scripts/cargo.sh, which attaches the database network:
#
#   ./scripts/cargo.sh test-fast
#   TEST_FAST_JOBS=2 ./scripts/cargo.sh test-fast -p lumberroom -- --test-threads=2
#
# Arguments before `--` go to `cargo test --no-run`, the rest to every test binary, as with
# `cargo test`. The doctests run as one more job when no target flag narrows the build, which is
# when `cargo test` would run them too.
#
# `cargo test` runs its binaries one after another, and a binary waiting on Postgres or on the
# suite lock in tests/common/mod.rs holds up every binary behind it. scripts/lib/test-pool.sh says
# why the unit of parallelism is a whole binary.
#
# Logs land in test-logs/<run>/ on the host, one per binary. Each run starts by deleting run
# directories older than seven days.
set -u

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
TAB="$(printf '\t')"
# Time first so the directories sort by when they ran. mktemp keeps two runs in one second apart;
# a pid cannot, because this script is pid 1 in every container cargo.sh starts.
mkdir -p "$REPO/test-logs"
LOGS="$(mktemp -d "$REPO/test-logs/$(date +%Y%m%d-%H%M%S)-XXXXXX")"
RUN="$(basename "$LOGS")"
# The doctest wrapper is executable and not a .log, so it lives in the target volume rather than in
# the checkout, where git would list it.
SCRATCH="$REPO/target/test-fast/$RUN"

mkdir -p "$SCRATCH"
find "$REPO/test-logs" -mindepth 1 -maxdepth 1 -type d -mtime +7 -exec rm -rf {} + 2>/dev/null || true
trap 'rm -rf "$SCRATCH"' EXIT

# Split the arguments at `--`. POSIX sh has no arrays, so the cargo half is kept quoted in a string
# and replayed through eval; the binary half stays in "$@".
quote() { printf "'%s' " "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
cargo_args=""
doc=1
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    --lib | --bin | --bin=* | --bins | --test | --test=* | --tests | --bench | --bench=* | --benches \
      | --example | --example=* | --examples | --all-targets | --doc) doc=0 ;;
  esac
  cargo_args="$cargo_args$(quote "$1")"
  shift
done

echo "test-fast: building test binaries"
eval "cargo test --no-run --message-format=json-render-diagnostics $cargo_args" >"$LOGS/build-messages.log"
status=$?
if [ "$status" != 0 ]; then
  echo "test-fast: the build failed (exit $status); nothing ran" >&2
  exit "$status"
fi

# One line per test executable. `"profile":{..."test":true` is what separates a test harness from
# the plain binary cargo also builds for CARGO_BIN_EXE_*; the target object carries a "test" field
# of its own, which is why the match is anchored on "profile".
list="$LOGS/binaries.log"
grep '"reason":"compiler-artifact"' "$LOGS/build-messages.log" \
  | grep '"profile":{[^}]*"test":true' \
  | grep '"executable":"/' \
  | while IFS= read -r line; do
      exe="$(printf '%s' "$line" | sed 's/.*"executable":"\([^"]*\)".*/\1/')"
      name="$(printf '%s' "$line" | sed 's/.*"target":{[^}]*"name":"\([^"]*\)".*/\1/')"
      kind="$(printf '%s' "$line" | sed 's/.*"target":{[^}]*"kind":\["\([^"]*\)".*/\1/')"
      manifest="$(printf '%s' "$line" | sed 's/.*"manifest_path":"\([^"]*\)".*/\1/')"
      case "$kind" in
        bin | test | bench | example) ;;
        *) kind=lib ;;
      esac
      printf '%s\t%s\t%s\t%s\n' "$kind" "$name" "$(dirname "$manifest")" "$exe"
    done >"$list"

if [ "$doc" = 1 ]; then
  {
    echo '#!/bin/sh'
    echo "exec cargo test --doc $cargo_args -- \"\$@\""
  } >"$SCRATCH/doctest.sh"
  chmod +x "$SCRATCH/doctest.sh"
  lib_name="$(awk -F "$TAB" '$1 == "lib" { print $2; exit }' "$list")"
  printf 'doc\t%s\t%s\t%s\n' "${lib_name:-lib}" "$REPO" "$SCRATCH/doctest.sh" >>"$list"
fi

TEST_POOL_JOBS="${TEST_FAST_JOBS:-6}" TEST_POOL_DURATIONS="$REPO/test-logs/durations.log" \
  sh "$REPO/scripts/lib/test-pool.sh" "$list" "$LOGS" "$@"
status=$?
exit "$status"
