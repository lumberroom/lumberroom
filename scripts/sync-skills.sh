#!/usr/bin/env bash
# Copies the agent skills under skills/ into the client plugins that ship them, or with --check
# reports any copy that has drifted. The plugins live in their own repositories; point at their
# checkouts with PLUGIN_ROOTS (space-separated), which defaults to siblings of this checkout.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
roots="${PLUGIN_ROOTS:-$here/../lumberroom-claude-code $here/../lumberroom-openclaw $here/../lumberroom-hermes}"
check=0; [ "${1:-}" = "--check" ] && check=1
status=0
for skill in "$here"/skills/*/; do
  name="$(basename "$skill")"
  for root in $roots; do
    [ -d "$root" ] || { echo "skip: $root not found" >&2; continue; }
    dest="$root/skills/$name/SKILL.md"
    if [ "$check" = 1 ]; then
      if ! cmp -s "$skill/SKILL.md" "$dest"; then echo "drift: $dest" >&2; status=1; fi
    else
      mkdir -p "$(dirname "$dest")" && cp "$skill/SKILL.md" "$dest" && echo "copied: $dest"
    fi
  done
done
exit $status
