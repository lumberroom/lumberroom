#!/usr/bin/env python3
"""Build replay with no model, and the consumer check.

    retrieval_diff.py freeze --slot SLOT_A --count 100
    retrieval_diff.py run --base-slot SLOT_A --branch-slot SLOT_B \
        --cli-fixture client/eval-fixture.example.jsonl --branch-checkout <engine checkout>

`--frozen` and `--out` default to frozen-queries.jsonl and build-replay.md under LR_GATE_ROOT. Both carry
store text, so they stay with the run records and out of every checkout.

Two builds run on their own fresh copies of one corpus, both with SEARCH_USAGE_WEIGHT=0 (slot.sh
sets it): a search bumps `access_count`, and with the usage weight on, the order of two builds'
bumps would move later rankings. This script sends both builds the same requests in the same order
and diffs the answers. It exits 1 on any difference.
"""
from __future__ import annotations

import argparse
import json
import random
import re
import subprocess
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # `python3 -I` drops the script dir
import session

SCRIPT_DIR = Path(__file__).resolve().parent
KEY_RE = re.compile(r"^(?:conflict|stale|undated|proposal):")
HOME_SOURCES = ["conflict", "stale"]  # the engine; the fork adds proposal sources
PAGES = range(4)


# ---- pure helpers ---------------------------------------------------------------------------

def collect_ids(node):
    """Every `id` value, in document order. A hit list keeps its order across builds even when
    serde_json sorts object keys differently."""
    out = []
    if isinstance(node, dict):
        for k, v in node.items():
            if k == "id" and isinstance(v, str):
                out.append(v)
            else:
                out.extend(collect_ids(v))
    elif isinstance(node, list):
        for v in node:
            out.extend(collect_ids(v))
    return out


def collect_keys(node):
    """Every review item `key`, in document order."""
    out = []
    if isinstance(node, dict):
        for k, v in node.items():
            if k == "key" and isinstance(v, str) and KEY_RE.match(v):
                out.append(v)
            else:
                out.extend(collect_keys(v))
    elif isinstance(node, list):
        for v in node:
            out.extend(collect_keys(v))
    return out


# What an MCP `review_queue` call with no `limit` reads from each source: the service default, the
# same as HTTP (decision 0031). A shorter default page hid stale items, because a search during
# review marks a row read and drops it from the stale source before the agent pages on.
DEFAULT_PAGE = 50


def default_page_diffs(full, branch_default, has_more_hint):
    """Differences between the branch's default review page and what decision 0031 promises.

    `full` maps each source to its whole key list in queue order, `branch_default` to the keys the
    default page carried, and `has_more_hint` says whether that page carried `more`. Each source
    holds its first DEFAULT_PAGE keys, and `more` appears only when a source holds past that."""
    diffs = []
    for src, keys in full.items():
        got = branch_default.get(src, [])
        d = diff_lists(f"review {src} default page", keys[:DEFAULT_PAGE], got[:DEFAULT_PAGE])
        if d:
            diffs.append(d)
        if len(got) > DEFAULT_PAGE:
            diffs.append({"name": f"review {src} default page size", "base": DEFAULT_PAGE,
                          "branch": len(got)})
    # A source the branch answered from that the replay never read in full has nothing to compare
    # against, so its keys are a difference rather than a silent pass.
    for src in sorted(set(branch_default) - set(full)):
        diffs.append({"name": f"review {src} default page (source not compared)", "base": [],
                      "branch": branch_default[src]})
    want_more = any(len(keys) > DEFAULT_PAGE for keys in full.values())
    if want_more != bool(has_more_hint):
        diffs.append({"name": "review default page more", "base": want_more, "branch": bool(has_more_hint)})
    return diffs


def diff_lists(name, base, branch):
    return None if base == branch else {"name": name, "base": base, "branch": branch}


def split_by_source(keys):
    out = {}
    for k in keys:
        out.setdefault(k.split(":", 1)[0], []).append(k)
    return out


def left_out_stats(structured):
    lo = structured.get("left_out", []) or []
    lr = structured.get("left_out_registry", []) or []
    size = len(json.dumps(lo, separators=(",", ":"))) + len(json.dumps(lr, separators=(",", ":"))) \
        if (lo or lr) else 0
    return {"left_out": len(lo), "left_out_registry": len(lr), "bytes": size}


# Fields each consumer reads from a search hit. A brief answer may drop any other field, and none
# of these.
HIT_FIELDS = {
    "hermes": ["id", "namespace", "content", "source", "occurred_at"],
    "openclaw": ["id", "namespace", "content", "source", "occurred_at"],
    "mod": ["id", "namespace", "content", "source", "occurred_at", "similarity", "created_at"],
    "cli": ["id", "namespace", "content", "score"],
}


def check_search_fields(base, branch, consumer):
    """Differences between the base hits and the branch hits, limited to what `consumer` reads."""
    problems = []
    bh, nh = base.get("hits", []), branch.get("hits", [])
    if len(bh) != len(nh):
        return [f"hit count differs ({len(bh)} against {len(nh)})"]
    for i, (b, n) in enumerate(zip(bh, nh)):
        for f in HIT_FIELDS[consumer]:
            if f not in b:
                continue  # the base never carried it for this hit (an optional key)
            if f not in n:
                problems.append(f"hits[{i}].{f} is missing")
            elif n[f] != b[f]:
                problems.append(f"hits[{i}].{f} differs")
    return problems


def check_bootstrap_fields(base, branch, rows):
    """`text`, `counts.memories`, `inventory`, `sealed_inventory` always; `counts.by_namespace`
    only on a `rows: true` call, because the brief bootstrap payload drops it and `rows: true`
    brings it back."""
    problems = []
    if branch.get("text") != base.get("text"):
        problems.append("text differs" if "text" in branch else "text is missing")
    if branch.get("counts", {}).get("memories") != base.get("counts", {}).get("memories"):
        problems.append("counts.memories differs")
    for f in ("inventory", "sealed_inventory"):
        if branch.get(f) != base.get(f):
            problems.append(f"{f} differs" if f in branch else f"{f} is missing")
    if rows:
        if "by_namespace" not in branch.get("counts", {}):
            problems.append("counts.by_namespace is missing")
        elif branch["counts"]["by_namespace"] != base.get("counts", {}).get("by_namespace"):
            problems.append("counts.by_namespace differs")
    return problems


def sse_or_json(body: str):
    body = body.strip()
    if body.startswith("{") or body.startswith("["):
        return json.loads(body)
    for line in body.splitlines():
        if line.startswith("data:"):
            return json.loads(line[5:].strip())
    raise ValueError("no JSON in the response body")


# ---- talking to a slot ----------------------------------------------------------------------

class Mcp:
    def __init__(self, slot_dir: Path):
        self.slot = json.loads((slot_dir / "slot.json").read_text())
        cfg = json.loads((slot_dir / "mcp.json").read_text())["mcpServers"]["lumberroom"]
        self.token = cfg["headers"]["Authorization"].split(" ", 1)[1]
        # The build replay talks to the arm directly, never through the strip proxy.
        self.url = f"http://127.0.0.1:{self.slot['port']}/mcp"
        self.dir = slot_dir
        self._id = 0

    def call(self, tool, args):
        self._id += 1
        req = urllib.request.Request(
            self.url, method="POST",
            data=json.dumps({"jsonrpc": "2.0", "id": self._id, "method": "tools/call",
                             "params": {"name": tool, "arguments": args}}).encode(),
            headers={"Authorization": f"Bearer {self.token}", "Content-Type": "application/json",
                     "Accept": "application/json, text/event-stream"})
        with urllib.request.urlopen(req, timeout=120) as r:
            msg = sse_or_json(r.read().decode())
        if "error" in msg:
            raise RuntimeError(f"{tool}: {msg['error']}")
        return msg["result"]

    def sql(self, q):
        return subprocess.run([str(SCRIPT_DIR / "slot.sh"), "psql", str(self.dir), q],
                              capture_output=True, text=True, check=True).stdout.strip().splitlines()


def structured(result):
    return result.get("structuredContent") or {}


# ---- the replay -----------------------------------------------------------------------------

def search_variants(q, projects):
    origin = q.get("origin", "")
    slug = origin.split(":", 1)[1] if origin.startswith("project:") else (projects[0].split(":", 1)[1] if projects else None)
    variants = [("bare", {"query": q["question"]})]
    if slug:
        variants.append(("project", {"query": q["question"], "project": slug}))
    if origin:
        variants.append(("namespaces", {"query": q["question"], "namespaces": [origin]}))
    return variants


def replay(base: Mcp, branch: Mcp, queries, projects, page_limit, sources=HOME_SOURCES):
    diffs, notes, left_out = [], [], {}

    def both(tool, args, name):
        """Call both builds. A refusal from either build is a difference, not a crash."""
        out = []
        for side, m in (("base", base), ("branch", branch)):
            try:
                out.append(m.call(tool, args))
            except RuntimeError as e:
                diffs.append({"name": f"{name} ({side} failed)", "base": str(e)[:160], "branch": ""})
                return None
        return out

    # 1. memory_search
    for q in queries:
        for label, args in search_variants(q, projects):
            pair = both("memory_search", args, f"search[{label}] {q['question'][:60]}")
            if pair is None:
                continue
            b, n = pair
            d = diff_lists(f"search[{label}] {q['question'][:60]}",
                           collect_ids(structured(b)), collect_ids(structured(n)))
            if d:
                diffs.append(d)
    # 2. context_bootstrap, per project
    for p in projects:
        args = {"project": p.split(":", 1)[1]}
        pair = both("context_bootstrap", args, f"bootstrap {p}")
        if pair is None:
            continue
        b, n = pair
        bt = (b.get("content") or [{}])[0].get("text", "")
        nt = (n.get("content") or [{}])[0].get("text", "")
        if bt != nt:
            diffs.append({"name": f"bootstrap text {p}", "base": f"{len(bt)} bytes", "branch": f"{len(nt)} bytes"})
        left_out[p] = left_out_stats(structured(n))
    # 3. memory_history for 50 ids that have predecessors
    for i in base.sql("SELECT id FROM memory WHERE supersedes IS NOT NULL "
                      "ORDER BY created_at, id LIMIT 50"):
        pair = both("memory_history", {"id": i}, f"history {i}")
        if pair is None:
            continue
        b, n = pair
        d = diff_lists(f"history {i}", collect_ids(structured(b)), collect_ids(structured(n)))
        if d:
            diffs.append(d)
    # 4. review_queue: explicit-limit pages, then the brief default page
    for src in sources:
        for page in PAGES:
            args = {"source": [src], "limit": page_limit, "offset": page * page_limit}
            pair = both("review_queue", args, f"review {src} page {page}")
            if pair is None:
                continue
            b, n = pair
            d = diff_lists(f"review {src} page {page}", collect_keys(structured(b)), collect_keys(structured(n)))
            if d:
                diffs.append(d)
    dflt = both("review_queue", {}, "review default page")
    if dflt is not None:
        full = {}
        for src in sources:
            full_call = both("review_queue", {"source": [src], "limit": 200}, f"review {src} full page")
            if full_call is not None:
                full[src] = collect_keys(structured(full_call[0]))
        branch_page = structured(dflt[1])
        diffs.extend(default_page_diffs(full, split_by_source(collect_keys(branch_page)),
                                        "more" in branch_page))
    return diffs, notes, left_out


def consumer_check(base: Mcp, branch: Mcp, queries, projects):
    """Serialize the branch answers as Hermes (Python json.dumps) and OpenClaw (JSON.stringify)
    do, parse them back and assert every field each consumer reads."""
    problems = []
    samples = []  # (label, base_structured, branch_structured, kind, extra)
    q = queries[0]
    brief_b = structured(base.call("memory_search", {"query": q["question"]}))
    brief_n = structured(branch.call("memory_search", {"query": q["question"]}))
    full_n = structured(branch.call("memory_search", {"query": q["question"], "detail": "full"}))
    samples += [("search", brief_b, brief_n, "search", ("hermes", "openclaw", "mod")),
                ("search detail=full", brief_b, full_n, "search", ("cli",))]
    for p in projects[:1]:
        args = {"project": p.split(":", 1)[1]}
        bb = structured(base.call("context_bootstrap", args))
        samples.append(("bootstrap", bb, structured(branch.call("context_bootstrap", args)), "bootstrap", False))
        samples.append(("bootstrap rows=true", bb,
                        structured(branch.call("context_bootstrap", {**args, "rows": True})), "bootstrap", True))
    # Python half: Hermes calls json.dumps(structured) with default arguments.
    py = [json.loads(json.dumps(s[2])) for s in samples]
    # Node half: OpenClaw calls JSON.stringify(structured ?? {}).
    node = subprocess.run(["node", str(SCRIPT_DIR / "consumer_check.mjs")],
                          input=json.dumps([s[2] for s in samples]), capture_output=True, text=True)
    if node.returncode != 0:
        return [f"node consumer_check.mjs failed: {node.stderr.strip()[:200]}"]
    js = json.loads(node.stdout)
    for (label, b, n, kind, extra), via_py, via_js in zip(samples, py, js):
        for via, doc in (("python", via_py), ("node", via_js)):
            if kind == "search":
                for consumer in extra:
                    problems += [f"{label} / {consumer} / {via}: {p}"
                                 for p in check_search_fields(b, doc, consumer)]
            else:
                problems += [f"{label} / {via}: {p}" for p in check_bootstrap_fields(b, doc, extra)]
    return problems


def run_policy_test(checkout: Path):
    r = subprocess.run([str(checkout / "scripts" / "policy-test.sh")], capture_output=True, text=True)
    return r.returncode, (r.stdout + r.stderr)[-1500:]


def load_queries(*paths):
    out = []
    for p in paths:
        out += [json.loads(l) for l in Path(p).read_text().splitlines() if l.strip()]
    return out


def cmd_freeze(a):
    """Write `count` queries from the live corpus, once, with a fixed seed, in the CLI eval
    fixture format. The file holds corpus text: it stays under LR_GATE_ROOT, where later runs
    reuse it, and never enters a repository."""
    m = Mcp(Path(a.slot))
    rows = [json.loads(r) for r in m.sql(
        "SELECT json_build_object('id', id, 'ns', namespace, 'text', left(content, 160))::text "
        "FROM memory WHERE superseded_by IS NULL AND length(content) > 60 ORDER BY id")]
    random.Random(7).shuffle(rows)
    Path(a.out).parent.mkdir(parents=True, exist_ok=True)
    with open(a.out, "w") as f:
        for r in rows[: a.count]:
            text = " ".join(r["text"].split())
            f.write(json.dumps({"question": re.split(r"(?<=[.!?])\s", text)[0][:140],
                                "expect_id": r["id"], "origin": r["ns"]}) + "\n")
    print(f"{min(a.count, len(rows))} queries -> {a.out}")
    return 0


def cmd_run(a):
    if a.reset:
        for d in (a.base_slot, a.branch_slot):
            subprocess.run([str(SCRIPT_DIR / "slot.sh"), "reset", str(d)]
                           + ([a.template] if a.template else []), check=True)
    base, branch = Mcp(Path(a.base_slot)), Mcp(Path(a.branch_slot))
    queries = load_queries(a.cli_fixture, a.frozen)
    projects = a.projects or [p for p in base.sql(
        "SELECT DISTINCT namespace FROM memory WHERE namespace LIKE 'project:%' ORDER BY 1")]
    diffs, notes, left_out = replay(base, branch, queries, projects, a.page_limit, a.sources)
    problems = consumer_check(base, branch, queries, projects)
    pol_rc, pol_out = (run_policy_test(Path(a.branch_checkout)) if a.branch_checkout else (None, "skipped"))
    lines = ["# Build replay", "",
             f"- search requests: {len(queries)} queries, bare + project + namespaces",
             f"- bootstrap projects: {len(projects)}", f"- differences: {len(diffs)}",
             f"- consumer check problems: {len(problems)}",
             f"- policy-test.sh exit: {pol_rc}", "", "## left_out per project (branch)", "",
             "| project | left_out | left_out_registry | bytes |", "|---|---|---|---|"]
    lines += [f"| {p} | {s['left_out']} | {s['left_out_registry']} | {s['bytes']} |" for p, s in sorted(left_out.items())]
    if diffs:
        lines += ["", "## Differences", ""] + [f"- `{d['name']}`: base {d['base']} / branch {d['branch']}" for d in diffs[:100]]
    if problems:
        lines += ["", "## Consumer check problems", ""] + [f"- {p}" for p in problems]
    if pol_rc not in (None, 0):
        lines += ["", "## policy-test.sh output (tail)", "", "```", pol_out, "```"]
    text = "\n".join(lines) + "\n"
    Path(a.out).parent.mkdir(parents=True, exist_ok=True)
    Path(a.out).write_text(text)
    print(f"report -> {a.out}")
    return 1 if (diffs or problems or pol_rc not in (None, 0)) else 0


def build_parser():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--base-slot", required=True)
    r.add_argument("--branch-slot", required=True)
    r.add_argument("--cli-fixture", required=True, help="client/eval-fixture.example.jsonl format")
    r.add_argument("--frozen", type=Path, help="queries written once by `freeze`; default under LR_GATE_ROOT")
    r.add_argument("--projects", nargs="*", help="project namespaces; default: every one in the corpus")
    r.add_argument("--sources", nargs="+", default=HOME_SOURCES,
                   help="sources the default review call reads; the fork adds proposal")
    r.add_argument("--page-limit", type=int, default=5)
    r.add_argument("--branch-checkout", help="repo checkout whose scripts/policy-test.sh runs last")
    r.add_argument("--reset", action="store_true", help="reset both slots from their templates first")
    r.add_argument("--template", help="template name for --reset; default: the slot's default template")
    r.add_argument("--out", type=Path, help="report path; default build-replay.md under LR_GATE_ROOT")
    f = sub.add_parser("freeze")
    f.add_argument("--slot", required=True)
    f.add_argument("--count", type=int, default=100)
    f.add_argument("--out", type=Path, help="default frozen-queries.jsonl under LR_GATE_ROOT")
    return ap


def resolve_outputs(a):
    """Fill the corpus-derived paths the operator left out with their places under the gate root."""
    root = session.gate_root()
    if a.cmd == "freeze":
        a.out = a.out or root / "frozen-queries.jsonl"
    else:
        a.frozen = a.frozen or root / "frozen-queries.jsonl"
        a.out = a.out or root / "build-replay.md"
    return a


def main(argv=None):
    a = resolve_outputs(build_parser().parse_args(argv))
    return cmd_freeze(a) if a.cmd == "freeze" else cmd_run(a)


if __name__ == "__main__":
    sys.exit(main())
