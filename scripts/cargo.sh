#!/bin/sh
# Run cargo against this repo inside the builder image.
#
#   ./scripts/cargo.sh check --all-targets
#   ./scripts/cargo.sh test
#   ./scripts/cargo.sh test-fast        # the same binaries, six at a time: scripts/lib/test-fast.sh
#
# The builder image carries g++ (ONNX Runtime links libstdc++) and the OpenSSL headers; a bare
# rust:slim does not. Build it once with:  docker build -t lumberroom-builder -f Dockerfile.builder .
# The integration suite needs the compose database, which is why the container joins its network.
set -e
cd "$(dirname "$0")/.."
[ -f .env ] && { set -a; . ./.env; set +a; }

# `test` links the lib-test and integration binaries in one step. Doing that concurrently gets the
# linker OOM-killed in the container (`collect2: fatal error: ld terminated with signal 9`), which
# reads like a compile error and is a memory ceiling: Docker Desktop here is handing the
# container 6.2GB of the host's 16GB. The real fix is raising that allocation; until someone does,
# force `-j 1` by default so nobody has to rediscover this. Only for `test`, and only when the
# caller has not already picked a `-j`: `check` gets real parallelism because it never links two
# binaries at once.
#
# `test-fast` builds like `test`, so it takes the same default, and then runs the built binaries N at
# a time through scripts/lib/test-fast.sh.
FAST=
if [ "${1:-}" = "test-fast" ]; then
  FAST=1
fi
if [ "${1:-}" = "test" ] || [ "$FAST" = 1 ]; then
  has_j=0
  for arg in "$@"; do
    case "$arg" in
      -j*|--jobs*) has_j=1 ;;
    esac
  done
  if [ "$has_j" = 0 ]; then
    sub="$1"
    shift
    set -- "$sub" -j 1 "$@"
  fi
fi

# ── mold, and an image that carries it ───────────────────────────────────────────────────────────
#
# The linker for every build this script runs, and only for those. Dockerfile.builder says why the
# flag lives here and not in the image. One value for every subcommand: check, clippy and test share
# build scripts and proc macros, and a RUSTFLAGS that differs between them rebuilds those on every
# switch.
#
# lumberroom-cloud builds the same image tag, so the image holds whatever the last checkout to build
# it put there. Refused here, naming the fix, rather than as gcc's "cannot find ld" from the first
# build script link.
builder_linker="$(docker image inspect -f '{{ index .Config.Labels "lumberroom.linker" }}' lumberroom-builder 2>/dev/null || true)"
if [ "$builder_linker" != mold ]; then
  echo "cargo.sh: the lumberroom-builder image carries no mold. Rebuild it from this checkout:" >&2
  echo "  docker build -t lumberroom-builder -f Dockerfile.builder ." >&2
  exit 1
fi
RUSTFLAGS_MOLD="-C link-arg=-fuse-ld=mold"

# ── the container outlives the command that started it, unless something stops it ────────────────
#
# `docker run --rm` removes the container when it exits on its own. It does nothing when this script
# is killed: the signal reaches the docker client, the client dies, and the container keeps running
# with cargo still holding the lock on /app/target. Every later run then sits on "Blocking waiting
# for file lock on build directory" until it is killed too, and leaves another one behind. Two dead
# runs are enough to make the suite look hung for no reason anybody can see.
#
# Two things stop that. A trap removes this run's own container on any signal it can catch, which
# needs the `exec` gone: exec replaces this shell and takes its traps with it. And because SIGKILL
# catches nothing, a sweep first removes any container from an earlier run whose owner is no longer
# alive. The owner's pid rides along as a label, so "no longer alive" is a question with an answer
# rather than a guess about age.

NAME="lumberroom-cargo-$$"

sweep() {
  for c in $(docker ps -q --filter "label=lumberroom.cargo.owner" 2>/dev/null); do
    owner=$(docker inspect -f '{{ index .Config.Labels "lumberroom.cargo.owner" }}' "$c" 2>/dev/null)
    [ -n "$owner" ] || continue
    # A live owner means a real concurrent run, and cargo's own lock is what serialises those.
    if kill -0 "$owner" 2>/dev/null; then
      continue
    fi
    echo "cargo.sh: removing $(docker inspect -f '{{.Name}}' "$c" 2>/dev/null | sed 's|^/||'), left by pid $owner which is gone" >&2
    docker rm -f "$c" >/dev/null 2>&1 || true
  done
}

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
}

sweep
trap 'cleanup' EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

# The command the container runs. `test-fast` swaps cargo for the pool script and drops its own name.
if [ "$FAST" = 1 ]; then
  shift
  set -- sh /app/scripts/lib/test-fast.sh "$@"
else
  set -- cargo "$@"
fi

# target/ in a named volume rather than on the bind mount, and the same volume the `dev` compose
# service uses. Two reasons. Rust build I/O through virtiofs dominates a rebuild on macOS, and
# without this the repository carries two build directories for one checkout: this one on the host
# and the dev loop's inside Docker.
#
# It does not halve anything. `check` and `test` build the dev profile and the dev loop builds
# `dev-release`, and cargo keeps those in separate subdirectories, so the dependency tree still
# compiles once for each. What is shared is the volume, the registry and the fingerprint database.
#
# The cost: a `test` run while the dev loop is up blocks on cargo's build-directory lock until the
# dev loop's build finishes. That is cargo serialising two real builds rather than the stale
# container problem above, and it clears on its own.
#
# target/ no longer appears on the host. `docker run --rm -v lumberroom-target:/t alpine ls /t` reads it.
#
# CARGO_INCREMENTAL is deliberately not set here, which leaves incremental compilation on.
# It held 21.5GB of the 54.9GB this volume reached and turning it off looks like the obvious win.
# It is not. Measured, edit one source file and rebuild `test -j 1 -p lumberroom --no-run`:
#
#   incremental on    15s  17s        incremental off    31s  28s  31s
#
# and on lumberroom-cloud, which is twelve times the source, 74s against 125s. Doubling the loop
# every agent and every human on this repo runs all day is not worth 21.5GB, and the 21.5GB was
# never the price of incremental anyway. It was thousands of dead unit-hashes nobody had built in
# weeks, because nothing pruned. scripts/lib/prune-target.sh below does, on a two day window for
# incremental and seven for everything else.

# BUILDER_UID is what stops cargo running as root in here. Root ignores permission bits, so every
# test that asserts a refusal from the filesystem passed under it whatever the code did.
# scripts/lib/builder-entrypoint.sh does the drop and carries the detail.
# The host account's own uid, so that on a Linux host the bind-mounted tree stays writable by the
# user who owns it; under Colima on macOS the mount answers to any uid and only the non-zero part
# matters. Measured cost: 13ms of stat per run, plus one 1.0s chown of the two volumes, once.

# XDG_CACHE_HOME puts ort-sys's downloaded ONNX Runtime inside the target volume. Left at its
# default it lands in this container's $HOME and dies with the container, while the build-script
# output that names its path survives in target/. The next recompile of ort-sys then fails with
# "could not find native static library `onnxruntime`", which reads like a toolchain fault. A target volume that already holds the stale output needs
# `./scripts/cargo.sh clean -p ort-sys` once, because the build script does not rerun on this variable.
docker run --rm --name "$NAME" \
  --label "lumberroom.cargo.owner=$$" \
  -e BUILDER_UID="$(id -u)" \
  -e BUILDER_GID="$(id -g)" \
  --network "${LUMBERROOM_DOCKER_NETWORK:-lumberroom_default}" \
  -v "$PWD:/app" \
  -v lumberroom-target:/app/target \
  -v lumberroom-cargo:/usr/local/cargo/registry \
  -e DATABASE_URL="postgres://${POSTGRES_USER:-lumberroom}:${POSTGRES_PASSWORD}@db:5432/${POSTGRES_DB:-lumberroom}" \
  -e CARGO_TERM_COLOR=never \
  -e XDG_CACHE_HOME=/app/target/.cache \
  -e RUST_BACKTRACE=1 \
  -e RUSTFLAGS="$RUSTFLAGS_MOLD" \
  ${TEST_FAST_JOBS:+-e TEST_FAST_JOBS="$TEST_FAST_JOBS"} \
  lumberroom-builder "$@"
status=$?

# ── prune, after the build and never before it ───────────────────────────────────────────────────
#
# Cargo never deletes anything, so every dependency, feature or flag change leaves its old
# `-<hash>` artifacts in the volume permanently. See scripts/lib/prune-target.sh for the rule.
#
# After the build, so it cannot cost the build any time, and only after one that succeeded, so a
# compile error does not also cost a sweep. `|| true` because a volume that will not prune is not
# a reason to fail a green test run, and `set -e` would otherwise make it one.
#
# Skipped while another cargo container is alive. The prune only ever removes artifacts untouched
# for CARGO_PRUNE_KEEP, so a concurrent build almost certainly does not want them, but "almost
# certainly" is not worth a link error in somebody else's run for the sake of a few MB.
if [ "$status" = 0 ] && [ -z "$(docker ps -q --filter "label=lumberroom.cargo.owner" 2>/dev/null)" ]; then
  docker run --rm \
    -e BUILDER_UID="$(id -u)" \
    -e BUILDER_GID="$(id -g)" \
    -v lumberroom-target:/app/target \
    -v "$PWD/scripts/lib/prune-target.sh:/prune-target.sh:ro" \
    -e CARGO_PRUNE_KEEP="${CARGO_PRUNE_KEEP:-7 days}" \
    -e CARGO_PRUNE_KEEP_INCREMENTAL="${CARGO_PRUNE_KEEP_INCREMENTAL:-2 days}" \
    lumberroom-builder sh /prune-target.sh /app/target || true
fi

exit "$status"
