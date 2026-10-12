#!/usr/bin/env python3
"""Compare two plaintext archive exports row by row, the check on the engine template.

    diff_archives.py fork-export.lumber engine-export.lumber

Compares id, namespace, content, tags, created_at, occurred_at, occurred_until and supersedes,
the three times as instants.
Bookkeeping fields (access counts, source client, embedding model) may differ. Exit 1 on any
difference, which stops the gate.
"""
from __future__ import annotations

import gzip
import json
import sys
from datetime import datetime
from pathlib import Path

FIELDS = ("namespace", "content", "tags", "created_at", "occurred_at", "occurred_until", "supersedes")
# Compared as instants: the fixture file writes `Z` and the engine exports `+00:00` for one time.
TIMES = {"created_at", "occurred_at", "occurred_until"}


def _value(field, v):
    if field in TIMES and isinstance(v, str):
        try:
            return datetime.fromisoformat(v.replace("Z", "+00:00"))
        except ValueError:
            return v
    return v


def load_rows(blob: bytes) -> dict:
    out = {}
    for line in gzip.decompress(blob).decode().splitlines():
        if line.strip():
            r = json.loads(line)
            if r.get("section") == "memory":
                out[r["id"]] = r
    return out


def diff_rows(a: dict, b: dict) -> list:
    out = []
    for i in sorted(a):
        if i not in b:
            out.append(f"{i}: missing from the second export")
            continue
        for f in FIELDS:
            if _value(f, a[i].get(f)) != _value(f, b[i].get(f)):
                out.append(f"{i}: {f} differs")
    out += [f"{i}: extra in the second export" for i in sorted(b) if i not in a]
    return out


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 2:
        print(__doc__.splitlines()[0], file=sys.stderr)
        return 2
    diffs = diff_rows(load_rows(Path(argv[0]).read_bytes()), load_rows(Path(argv[1]).read_bytes()))
    for d in diffs[:50]:
        print(d)
    print(f"{len(diffs)} differences")
    return 1 if diffs else 0


if __name__ == "__main__":
    sys.exit(main())
