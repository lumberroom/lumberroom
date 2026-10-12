#!/usr/bin/env python3
"""Empty a template's real review queue and seed the review fixtures (spec G2, Corpus).

    seed_review.py inject  --in owner.lumber --out seeded.lumber     engine path, before the restore
    seed_review.py backdate --slot D --cli lumberroom --keys-out map.json   any populated store
    seed_review.py empty   --slot D [--fork]                         keep_both, confirm, dismiss
    seed_review.py check   --slot D [--keys-map map.json]            queue holds only the fixtures
    seed_review.py remap-tasks --keys-map map.json --in T --out T2   point task items at live keys

Why two ways in. `lumberroom archive import --restore` keeps ids and `created_at`, but the server
refuses it unless the store holds no memory row (src/services/archive.rs:539). A merge import
mints new ids and ignores the archive's `created_at`. The fixtures need old `created_at` values,
because stale rows must be over a year old, so:

- Engine path. The engine template is itself built by `--restore` into an empty store.
  `inject` adds the fixture rows to the owner's plaintext archive first, so one restore brings in
  both and the fixtures keep their ids and dates.
- Populated store (the fork template, which is a pg_restore). `backdate` merge-imports the fixtures
  and then rewrites `created_at` in SQL. The merge mints new ids, so `--keys-out` records which
  live key stands for which fixture key. `created_at` never changes in place as far as the
  conflict tables know, so `backdate` clears the pair marks for the fixture rows and the sweeper
  rescans them in the corrected order.

`empty` leaves every `project:gate-review` item alone. The archive is plaintext because it holds
invented text only.
"""
from __future__ import annotations

import argparse
import gzip
import json
import os
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # `python3 -I` drops the script dir

GATE_NS = "project:gate-review"
HERE = Path(__file__).resolve().parent
FIXTURE = HERE / "fixtures" / "review.archive.json"
SWEEP_WAIT_SECS = 30 * 60


# ---- pure helpers ---------------------------------------------------------------------------

def _memory_record(m: dict, fixture: dict) -> dict:
    return {
        "section": "memory", "id": m["id"], "namespace": fixture["namespace"],
        "content": m["content"], "tags": m.get("tags", []),
        "source_client": fixture["source_client"], "sensitivity": "open",
        "supersedes": None, "superseded_by": None, "superseded_at": None,
        "occurred_at": m.get("occurred_at"), "occurred_until": None, "access_count": 0,
        "last_accessed_at": None, "last_confirmed_at": None, "created_at": m["created_at"],
        "embedding_model": None}


def _pack(header: dict, records: list) -> bytes:
    header = {**header, "counts": {**header["counts"], "memory": sum(r["section"] == "memory" for r in records)}}
    body = "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in [header] + records)
    return gzip.compress(body.encode(), mtime=0)


def build_archive(fixture: dict) -> bytes:
    """Gzipped NDJSON in the `lumberroom.archive` v1 layout (crates/archive/src/record.rs). The
    reader checks the header counts against the body, so they come from what is written."""
    header = {"section": "header", "format": "lumberroom.archive", "version": 1,
              "created_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
              "source": {"kind": "oss", "build": "gate-review-fixture"},
              "counts": {"memory": 0, "registry": 0, "alias": 0, "registry_history": 0, "sealed_item": 0},
              "includes_superseded": False, "excluded": {"total": 0, "reasons": []}}
    return _pack(header, [_memory_record(m, fixture) for m in fixture["memories"]])


def inject_archive(blob: bytes, fixture: dict) -> bytes:
    """Add the fixture rows to a plaintext archive and fix the header count. A fixture row already
    in the archive (same id) is not added twice."""
    recs = [json.loads(l) for l in gzip.decompress(blob).decode().splitlines() if l.strip()]
    header, body = recs[0], recs[1:]
    have = {r["id"] for r in body if r["section"] == "memory"}
    body += [_memory_record(m, fixture) for m in fixture["memories"] if m["id"] not in have]
    body.sort(key=lambda r: ["memory", "registry", "alias", "registry_history", "sealed_item"].index(r["section"]))
    return _pack(header, body)


def foreign_items(queue: dict) -> list:
    return [i["key"] for i in queue.get("items", []) if i.get("namespace") != GATE_NS]


def key_map(fixture: dict, live_keys: list, live_contents: dict) -> dict:
    """fixture key -> live key, matched on the set of row contents. Ids differ after a merge
    import, and the contents are unique per fixture row."""
    fx_content = {m["id"]: m["content"] for m in fixture["memories"]}

    def sig(key, contents):
        return frozenset(contents.get(i) for i in key.split(":")[1:])

    live = {sig(k, live_contents): k for k in live_keys}
    out = {}
    for keys in fixture["tasks"].values():
        for fk in keys:
            hit = live.get(sig(fk, fx_content))
            if hit:
                out[fk] = hit
    return out


# ---- against a slot -------------------------------------------------------------------------

def _structured(m, args):
    return m.call("review_queue", args).get("structuredContent") or {}


def wait_for_sweep(m):
    deadline = time.time() + SWEEP_WAIT_SECS
    while _structured(m, {"source": ["conflict"], "limit": 1}).get("conflicts_pending", 0) != 0:
        if time.time() > deadline:
            raise SystemExit("conflicts_pending did not reach 0 in 30 minutes")
        time.sleep(15)


def drain(m, source, act):
    """Apply `act` to every non-fixture item of `source` until none is left."""
    seen = set()
    for _ in range(200):
        offset, acted = 0, False
        while True:
            s = _structured(m, {"source": [source], "limit": 200, "offset": offset})
            items = [i for i in s.get("items", []) if i.get("namespace") != GATE_NS and i["key"] not in seen]
            if items:
                for it in items:
                    seen.add(it["key"])
                    act(it)
                acted = True
                break
            if not s.get("has_more"):
                break
            offset += 200
        if not acted:
            return


def empty_queue(m, fork: bool):
    wait_for_sweep(m)
    drain(m, "conflict", lambda it: m.call("review_decide", {"key": it["key"], "verdict": "keep_both"}))

    def confirm(it):
        m.sql(f"UPDATE memory SET last_confirmed_at = now() WHERE id = '{it['key'].split(':', 1)[1]}'")
    drain(m, "stale", confirm)
    if fork:
        drain(m, "proposal", lambda it: m.call("review_decide", {
            "key": it["key"], "verdict": "dismiss", "version": it.get("version"),
            "reason": "Emptied before the gate corpus freezes."}))


def cli_env(m):
    return {**os.environ, "LUMBERROOM_URL": f"http://127.0.0.1:{m.slot['port']}", "LUMBERROOM_TOKEN": m.token}


def backdate(m, cli, fixture):
    with tempfile.TemporaryDirectory(prefix="lr-seed.") as d:
        path = Path(d) / "review.lumber"
        path.write_bytes(build_archive(fixture))
        out = subprocess.run([cli, "archive", "import", str(path), "--allow-plaintext", "--json"],
                             env=cli_env(m), check=True, capture_output=True, text=True).stdout
    report = json.loads(out)
    if report.get("collapsed"):
        raise SystemExit(f"{report['collapsed']} fixture rows collapsed into existing rows; reword them")
    live = {r["content"]: r["id"] for r in (json.loads(x) for x in m.sql(
        f"SELECT json_build_object('id', id, 'content', content)::text FROM memory WHERE namespace = '{GATE_NS}'"))}
    ids = []
    for f in fixture["memories"]:
        lid = live.get(f["content"])
        if lid is None:
            raise SystemExit(f"fixture row {f['id']} did not land")
        ids.append(lid)
        m.sql(f"UPDATE memory SET created_at = '{f['created_at']}' WHERE id = '{lid}'")
    lst = ",".join(f"'{i}'" for i in ids)
    m.sql(f"DELETE FROM memory_conflict WHERE older_id IN ({lst}) OR newer_id IN ({lst})")
    m.sql(f"DELETE FROM memory_conflict_scan WHERE memory_id IN ({lst})")
    wait_for_sweep(m)
    return live


def current_map(m, fixture):
    live_keys, contents = [], {}
    for r in (json.loads(x) for x in m.sql(
            f"SELECT json_build_object('id', id, 'content', content)::text FROM memory WHERE namespace = '{GATE_NS}'")):
        contents[r["id"]] = r["content"]
    for src in ("conflict", "stale"):
        live_keys += [i["key"] for i in _structured(m, {"source": [src], "limit": 200}).get("items", [])
                      if i.get("namespace") == GATE_NS]
    return key_map(fixture, live_keys, contents)


def check(m, fixture, keys_map=None):
    problems = []
    s = {"items": []}
    for src in ("conflict", "stale"):
        s["items"] += _structured(m, {"source": [src], "limit": 200}).get("items", [])
    stray = foreign_items(s)
    if stray:
        problems.append(f"{len(stray)} queue items outside {GATE_NS}, first: {stray[:3]}")
    present = {i["key"] for i in s["items"]}
    for t, keys in fixture["tasks"].items():
        for k in keys:
            if (keys_map.get(k) if keys_map is not None else k) not in present:
                problems.append(f"{t}: fixture key {k} is not in the queue")
    return problems


def main(argv=None):
    import retrieval_diff as rd  # the slot-aware MCP client
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    i = sub.add_parser("inject"); i.add_argument("--in", dest="inp", type=Path, required=True)
    i.add_argument("--out", type=Path, required=True); i.add_argument("--fixture", type=Path, default=FIXTURE)
    b = sub.add_parser("backdate"); b.add_argument("--slot", type=Path, required=True)
    b.add_argument("--cli", required=True); b.add_argument("--keys-out", type=Path)
    b.add_argument("--fixture", type=Path, default=FIXTURE)
    e = sub.add_parser("empty"); e.add_argument("--slot", type=Path, required=True)
    e.add_argument("--fork", action="store_true")
    c = sub.add_parser("check"); c.add_argument("--slot", type=Path, required=True)
    c.add_argument("--keys-map", type=Path); c.add_argument("--fixture", type=Path, default=FIXTURE)
    r = sub.add_parser("remap-tasks"); r.add_argument("--keys-map", type=Path, required=True)
    r.add_argument("--in", dest="inp", type=Path, required=True); r.add_argument("--out", type=Path, required=True)
    a = ap.parse_args(argv)

    if a.cmd == "inject":
        fx = json.loads(a.fixture.read_text())
        a.out.write_bytes(inject_archive(a.inp.read_bytes(), fx))
        print(f"{len(fx['memories'])} fixture rows injected -> {a.out}")
        return 0
    if a.cmd == "remap-tasks":
        mp = json.loads(a.keys_map.read_text())
        out = []
        for line in a.inp.read_text().splitlines():
            if not line.strip():
                continue
            t = json.loads(line)
            for it in t.get("items", []):
                it["item"] = mp.get(it["item"], it["item"])
            t["seeded_keys"] = [mp.get(k, k) for k in t.get("seeded_keys", [])]
            out.append(json.dumps(t))
        a.out.write_text("\n".join(out) + "\n")
        return 0
    m = rd.Mcp(a.slot)
    if a.cmd == "empty":
        empty_queue(m, a.fork)
        print("real queue emptied")
        return 0
    fx = json.loads(a.fixture.read_text())
    if a.cmd == "backdate":
        backdate(m, a.cli, fx)
        mp = current_map(m, fx)
        if a.keys_out:
            a.keys_out.write_text(json.dumps(mp, indent=1))
        miss = sum(len(k) for k in fx["tasks"].values()) - len(mp)
        print(f"{len(mp)} fixture keys live, {miss} missing")
        return 1 if miss else 0
    problems = check(m, fx, json.loads(a.keys_map.read_text()) if a.keys_map else None)
    for p in problems:
        print(p, file=sys.stderr)
    print("queue holds only the fixtures" if not problems else f"{len(problems)} problems")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
