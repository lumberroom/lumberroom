#!/bin/sh
# Delete build artifacts that no build has needed for a while.
#
#   sh scripts/lib/prune-target.sh /app/target
#
# Runs inside the builder image, against the target volume, AFTER a build. Never before one: the
# whole point is that a warm cache stays warm and no build ever waits on this.
#
# Cargo has no garbage collection. Every dependency bump, feature change or flag change leaves the
# previous `-<hash>` artifacts behind permanently, so nothing ever clears them. The volume reached
# 54.9GB before anyone looked: 29.8GB of debug/deps holding 10,696 files for 1,008 distinct crate
# names, and 21.5GB of debug/incremental.
#
# THE RULE, which is cargo-sweep's rule. A unit is live if any file in its fingerprint directory
# has been READ within CARGO_PRUNE_KEEP. Everything cargo puts in deps/, build/ and .fingerprint/
# carries that unit's 16 hex digit hash in its name, so the live hashes are the keep set and
# anything carrying a hash outside it is dead. A name that does not end in `-<16 hex digits>` is
# kept, because it is not a hashed artifact and we cannot reason about it.
#
# READ, NOT WRITTEN, AND THAT IS THE WHOLE TRICK. Cargo writes nothing in a unit's fingerprint
# directory on a build where the unit is already fresh: measured on a warm no-op build, 0 of 162
# units had any file rewritten, invoked.timestamp included. Dating a unit by mtime therefore ages
# out the entire warm cache of a branch nobody has changed in a week, which is the worst thing
# this script could possibly do. Cargo does READ those files to decide the unit is fresh, and the
# volume is mounted `relatime`, where a read moves atime once a day. Once a day is plenty for a
# window measured in days.
#
# WHICH PUTS A FLOOR UNDER THE WINDOW, and it is not optional. Under `relatime` a read moves atime
# only when the old atime is already older than a day, so a live unit read every hour still looks
# untouched for up to 24 hours. Ask for anything shorter than two days and the answer is not a
# tighter prune, it is the warm cache deleted underneath you: a 10 minute window on a volume whose
# gate had just run green took it from 16.9GB to 3.2GB and made the next build recompile all 134
# units. Windows under two days are refused rather than obeyed.
#
# A NOATIME MOUNT WOULD MAKE THAT SILENTLY FALSE, so the mount is not trusted, it is probed: write
# a file, set its atime back a month, read it, ask whether atime moved. If it did not, this script
# has no signal it can use and does nothing at all. cargo-sweep reads atime with no such check.
#
# BUILD SCRIPT RUN UNITS ARE KEPT WHATEVER THEIR AGE. Their fingerprint directory holds only
# `run-build-script-build-script-build` and its .json, with no invoked.timestamp, and deleting one
# takes build/<pkg>-<hash>/out with it and recompiles every crate downstream. debug/build was
# 939MB of the 54.9GB, so that leak is bounded and cheap; deps/ at 29.8GB and incremental at
# 21.5GB are the ones worth the risk.
#
# INCREMENTAL GETS A SHORTER WINDOW and a plainer rule, because its session directories are not
# named with a fingerprint hash. Incremental compilation earns its disk: measured, it takes an
# edit-rebuild iteration on lumberroom from 30s to 16s and on lumberroom-cloud from 125s to 74s,
# so turning it off is a real penalty and it stays on. What it does not earn is 21.5GB, which was
# thousands of dead unit-hashes nobody had built in weeks. Dropping a session you have not used in
# two days costs one non-incremental rebuild if you go back to that branch.
#
# Both windows are whatever `find -newerat` accepts, so "36 hours" and "30 minutes" work as well
# as the defaults. The tests use short ones.
set -u

ROOT="${1:-/app/target}"
KEEP="${CARGO_PRUNE_KEEP:-7 days}"
KEEP_INCREMENTAL="${CARGO_PRUNE_KEEP_INCREMENTAL:-2 days}"

[ -d "$ROOT" ] || { echo "prune-target: no $ROOT, nothing to do" >&2; exit 0; }

floor=$(date -d "-2 days" +%s)
for w in "$KEEP" "$KEEP_INCREMENTAL"; do
  cutoff=$(date -d "-$w" +%s 2>/dev/null) || {
    echo "prune-target: cannot read '$w' as a time, skipping" >&2; exit 0; }
  if [ "$cutoff" -gt "$floor" ]; then
    echo "prune-target: '$w' is under two days, which relatime cannot resolve. Skipping." >&2
    exit 0
  fi
done

probe="$ROOT/.prune-atime-probe"
echo probe > "$probe" 2>/dev/null || { echo "prune-target: $ROOT is not writable, skipping" >&2; exit 0; }
touch -a -d "30 days ago" "$probe"
cat "$probe" > /dev/null
moved=$(find "$probe" -newerat "-1 days" 2>/dev/null | wc -l | tr -d ' ')
rm -f "$probe"
if [ "$moved" = 0 ]; then
  echo "prune-target: reads do not move atime on $ROOT, so nothing here can be dated. Skipping." >&2
  exit 0
fi

before=$(du -sk "$ROOT" 2>/dev/null | cut -f1)

# Reads a keep-list of hashes then a list of paths, and prints the paths whose hash is not in the
# keep list. Ported from cargo-sweep's hash_from_path_name: basename, cut at the first dot, take
# the run after the last dash, and require exactly 16 hex digits.
filter='
function hash_of(p,   n, i, j, h) {
  n = p
  sub(/.*\//, "", n)
  i = index(n, ".")
  if (i > 0) n = substr(n, 1, i - 1)
  j = 0
  for (i = length(n); i > 0; i--) if (substr(n, i, 1) == "-") { j = i; break }
  if (j == 0) return ""
  h = substr(n, j + 1)
  if (length(h) != 16) return ""
  if (h !~ /^[0-9a-fA-F]+$/) return ""
  return h
}
NR == FNR { keep[$0] = 1; next }
{ h = hash_of($0); if (h != "" && !(h in keep)) print $0 }
'

units=0
sessions=0
for fp in "$ROOT"/*/.fingerprint; do
  [ -d "$fp" ] || continue
  profile=$(dirname "$fp")

  live=$(mktemp)
  dead=$(mktemp)

  {
    find "$fp" -mindepth 2 -maxdepth 2 -type f -newerat "-$KEEP" -printf '%h\n' 2>/dev/null
    find "$fp" -mindepth 1 -maxdepth 1 -type d '!' -exec test -e '{}/invoked.timestamp' ';' -print 2>/dev/null
  } | sed -e 's|.*/||' -e 's|.*-||' | sort -u > "$live"

  # An empty keep list means no build has read anything here inside the window. Deleting a whole
  # profile on the strength of that is not a prune, it is a cargo clean nobody asked for.
  if [ ! -s "$live" ]; then
    rm -f "$live" "$dead"
    continue
  fi

  for d in deps build .fingerprint; do
    [ -d "$profile/$d" ] || continue
    find "$profile/$d" -mindepth 1 -maxdepth 1 2>/dev/null | awk "$filter" "$live" - >> "$dead"
  done

  n=$(wc -l < "$dead" | tr -d ' ')
  if [ "$n" -gt 0 ]; then
    # rm -rf, not rm: build/<name>-<hash> and .fingerprint/<name>-<hash> are directories.
    xargs -r rm -rf < "$dead" 2>/dev/null || true
    units=$((units + n))
  fi
  rm -f "$live" "$dead"

  if [ -d "$profile/incremental" ]; then
    s=$(find "$profile/incremental" -mindepth 1 -maxdepth 1 '!' -newerat "-$KEEP_INCREMENTAL" 2>/dev/null | wc -l | tr -d ' ')
    if [ "$s" -gt 0 ]; then
      find "$profile/incremental" -mindepth 1 -maxdepth 1 '!' -newerat "-$KEEP_INCREMENTAL" -exec rm -rf {} + 2>/dev/null || true
      sessions=$((sessions + s))
    fi
  fi
done

after=$(du -sk "$ROOT" 2>/dev/null | cut -f1)
echo "prune-target: $units artifacts unread for $KEEP, $sessions incremental sessions idle for $KEEP_INCREMENTAL, $(( (before - after) / 1024 ))MB reclaimed, $(( after / 1024 ))MB left" >&2
