#!/bin/sh
# Drop to a non-root uid before running anything in the builder image.
#
# WHY THIS EXISTS. Root ignores permission bits, so every test that asserts a refusal from the
# filesystem passes under root whatever the code underneath does. The expensive one was
# crates/lumberroom's `a_save_that_cannot_complete_leaves_the_live_file_alone`: it chmods a
# directory 0500, probes whether the mode took effect, and used to return early when it had not.
# libtest captures a passing test's stderr, so the line it printed reached nobody and the gate
# counted 387 passed with that test measuring nothing.
#
# Every consumer of this image gets the drop, not just scripts/cargo.sh, because the next person to
# write a `docker run lumberroom-builder` should not have to remember. Two consumers opt out with
# BUILDER_UID=0 and say why at the call site.
set -e

uid="${BUILDER_UID:-}"
gid="${BUILDER_GID:-}"

# Already dropped, by `docker run --user` or a compose `user:`. No privilege left to fix anything
# with, and nothing that needs fixing.
if [ "$(id -u)" != 0 ]; then
  exec "$@"
fi

# The volumes this image writes, and the marker that records which uid owns each one. The marker is
# written last, after a chown that returned 0, so a chown killed halfway leaves no claim behind and
# the next container redoes it. Reading the top-level directory's owner instead would call that
# half-chowned tree done.
OWN_MARKER=.builder-owner
shared="/app/target ${CARGO_HOME:-/usr/local/cargo}/registry"

# With no uid asked for, adopt whoever already owns one of those volumes. Two things need this.
# The `dev` compose service runs this image against the same target volume scripts/cargo.sh uses,
# and compose cannot call `id -u` to agree on a number. And these are named volumes, so every
# checkout of this repo on the machine shares them: one caller that has not been taught to pass a
# uid would chown both to a fixed default on every run while the callers that do pass one chown
# them forward again. Measured, that ping-pong is about a second of I/O each way, and two builds
# reading the registry through it race. Adopting whoever got there first ends it. Unclaimed volumes
# mean nobody has been here yet, so 1000.
for probe in $shared; do
  if [ -n "$uid" ]; then
    break
  fi
  [ -r "$probe/$OWN_MARKER" ] || continue
  claim="$(cat "$probe/$OWN_MARKER")"
  # Two numbers or nothing. A marker only a root process can corrupt is still a marker that reaches
  # `chown`, and `chown: invalid user` kills the container before it runs the command it was asked
  # for. Ignoring a bad claim rechowns once and rewrites it.
  case "$claim" in
    *[!0-9:]* | *:*:* | :* | *: | '') continue ;;
  esac
  uid="${claim%:*}"
  gid="${gid:-${claim#*:}}"
done
uid="${uid:-1000}"
gid="${gid:-1000}"

if [ "$uid" = 0 ]; then
  echo "builder: BUILDER_UID is 0, so this container keeps root. Root ignores permission bits, so any test asserting a refusal from the filesystem is inert here." >&2
  exec "$@"
fi

# BUILDER_OWN carries any extra path a caller knows about, space separated.
# scripts/eval-longmemeval.sh passes /models, a volume docker creates root-owned and the embedder
# fills on a cold run.
#
# Measured on Colima (aarch64, ext4 inside the VM): the marker check costs 13ms including the
# shell, and the chown it guards costs 275ms for the 4.1GB target volume (10,187 inodes) and 767ms
# for the 496MB registry (30,874). The chown runs on the first container after this change and
# never again, because from then on this uid writes everything below.
# shellcheck disable=SC2086
for d in $shared ${BUILDER_OWN:-}; do
  [ -d "$d" ] || continue
  if [ "$(cat "$d/$OWN_MARKER" 2>/dev/null)" = "$uid:$gid" ]; then
    continue
  fi
  echo "builder: taking ownership of $d for $uid:$gid, once" >&2
  chown -R "$uid:$gid" "$d"
  printf '%s:%s\n' "$uid" "$gid" > "$d/$OWN_MARKER"
  chown "$uid:$gid" "$d/$OWN_MARKER"
done

# /root is 0700 and this process is about to stop being root. CARGO_HOME and RUSTUP_HOME arrive
# world-writable from the rust image, so HOME holds no build cache and a per-container directory
# costs nothing: the ONNX Runtime that fastembed's build script fetches lands in OUT_DIR, inside
# the target volume.
export HOME=/home/builder

# setuid, not seteuid, and exec rather than a child: setpriv replaces this shell, so no build
# script and no nested cargo can climb back. --no-new-privs closes the setuid-binary route too.
exec setpriv --reuid="$uid" --regid="$gid" --clear-groups --no-new-privs "$@"
