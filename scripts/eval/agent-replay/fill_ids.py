#!/usr/bin/env python3
"""Replace `@id:<seed key>` tokens in a task file with the server ids `seed.py load` recorded.

    fill_ids.py --ids store.jsonl.ids.json --in tasks.example.jsonl --out tasks.jsonl

An unknown seed key stops the run: a blank `target_id` would score as "left" and hide the gap.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

TOKEN = re.compile(r"@id:([A-Za-z0-9_.-]+)")


def fill(text: str, ids: dict) -> str:
    return TOKEN.sub(lambda m: ids[m.group(1)], text)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--ids", type=Path, required=True)
    ap.add_argument("--in", dest="inp", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args(argv)
    a.out.write_text(fill(a.inp.read_text(), json.loads(a.ids.read_text())))
    return 0


if __name__ == "__main__":
    sys.exit(main())
