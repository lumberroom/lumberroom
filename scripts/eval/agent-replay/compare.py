"""G2 pass rules and the report (spec section 9, G2 "Pass rules").

Every rule is a pure function over lists of run records, so `test_compare.py` pins each one on
fixture records. `main` reads a records tree, applies the rules per stage, channel and task group,
prints a markdown report and exits 1 when a rule fails.

"Spread" is not defined further in the spec. This module reads it as the absolute difference
between A0's mean and A0b's mean for the quantity under test. Two runs of one build cannot differ
by more than the model's own noise, and A0 against A0b is the only place the gate measures that.
"""
from __future__ import annotations

import argparse
import json
import sys
from collections import Counter, defaultdict
from pathlib import Path

EPS = 1e-9
FENCE_MARKER = "----- data "
GATE_ROOT = Path.home() / "lumberroom-gate" / "2026-10-mcp-response-tokens"


def scored(runs):
    """Incomplete runs never score; they wait for a rerun."""
    return [r for r in runs if r.get("status") == "complete"]


def _mean(xs):
    xs = list(xs)
    return sum(xs) / len(xs) if xs else 0.0


def _by_task(runs):
    out = defaultdict(list)
    for r in scored(runs):
        out[r["task"]].append(r)
    return out


def mode_set(runs, item):
    """The outcomes most of `runs` reach for `item`. A tie returns every tied outcome."""
    counts = Counter(r["outcomes"].get(item) for r in scored(runs) if item in r["outcomes"])
    if not counts:
        return set()
    top = max(counts.values())
    return {o for o, n in counts.items() if n == top}


def _items(runs):
    return sorted({i for r in scored(runs) for i in r["outcomes"]})


def item_agreement(a0, other):
    """Mean over (task, item) of the share of `other` runs that reach A0's mode for the item."""
    a0_by, other_by = _by_task(a0), _by_task(other)
    shares = []
    for task, a0_runs in a0_by.items():
        runs = other_by.get(task, [])
        if not runs:
            continue
        for item in _items(a0_runs):
            modes = mode_set(a0_runs, item)
            hits = sum(1 for r in runs if r["outcomes"].get(item) in modes)
            shares.append(hits / len(runs))
    return _mean(shares)


def _share(runs, item, outcome):
    runs = scored(runs)
    if not runs:
        return 0.0
    return sum(1 for r in runs if r["outcomes"].get(item) == outcome) / len(runs)


def item_fails(a0, a0b, a1, item):
    """An item fails alone when A0 and A0b each reach one outcome in 4 of 5 runs (or more, by
    share) and A1 reaches it in 2 of 5 or fewer."""
    outcomes = {r["outcomes"].get(item) for r in scored(a0) if item in r["outcomes"]}
    for o in outcomes:
        if _share(a0, item, o) >= 0.8 - EPS and _share(a0b, item, o) >= 0.8 - EPS \
                and _share(a1, item, o) <= 0.4 + EPS:
            return True
    return False


def group_agreement_ok(a0, a0b, a1):
    """Rule 1: A1's agreement with A0 reaches A0b's agreement with A0, minus 5 points."""
    return item_agreement(a0, a1) >= item_agreement(a0, a0b) - 0.05 - EPS


def _unseen_per_run(runs):
    runs = scored(runs)
    return _mean(sum(1 for o in r["outcomes"].values() if o == "unseen") for r in runs)


def unseen_ok(a0b, a1):
    """Rule 2: A1's unseen keys per run stay at or below A0b's."""
    return _unseen_per_run(a1) <= _unseen_per_run(a0b) + EPS


def _calls(runs):
    return _mean(len(r["calls"]) for r in scored(runs))


def calls_ok(a0, a0b, a1):
    """Rule 3: within one call, 20 percent of A0, or the A0 to A0b spread, whichever is widest."""
    base = _calls(a0)
    allowed = max(1.0, 0.2 * base, abs(base - _calls(a0b)))
    return abs(_calls(a1) - base) <= allowed + EPS


def _tool_shares(runs):
    counts, total = Counter(), 0
    for r in scored(runs):
        for c in r["calls"]:
            counts[c["tool"]] += 1
            total += 1
    return {t: n / total for t, n in counts.items()} if total else {}


def tool_share_moves(a0, a0b, a1):
    """Rule 3, second half: tools whose share of calls moved past the spread plus 20 points.
    The list informs the owner and fails nothing."""
    s0, s0b, s1 = _tool_shares(a0), _tool_shares(a0b), _tool_shares(a1)
    moved = {}
    for t in set(s0) | set(s0b) | set(s1):
        spread = abs(s0.get(t, 0.0) - s0b.get(t, 0.0))
        delta = s1.get(t, 0.0) - s0.get(t, 0.0)
        if abs(delta) > spread + 0.20 + EPS:
            moved[t] = delta
    return moved


def judge_ok(a0b_rate, a1_rate):
    """Rule 4: A1's equivalence rate reaches A0b's, minus 5 points."""
    return a1_rate >= a0b_rate - 0.05 - EPS


def more_path_ok(required, a0b, a1):
    """Rule 5: A1 gets at least as many required facts into its answer as A0b does.

    The record keeps the final answer and not each tool result, so a fact counts when the answer
    carries it."""
    def rate(runs):
        runs = scored(runs)
        if not runs or not required:
            return 0.0
        return _mean(
            sum(1 for f in required if f.lower() in r.get("answer", "").lower()) / len(required)
            for r in runs)
    return rate(a1) >= rate(a0b) - EPS


def fence_leak(runs):
    """Rule 6: runs whose written content carries a fence marker line."""
    bad = []
    for r in scored(runs):
        if any(FENCE_MARKER in w.get("content", "") for w in r["end_state"]["written"]):
            bad.append(r)
    return bad


def terse_ok(a0_mean, a0b_mean, a1_mean):
    """Rule 7: A1's mean answer length may exceed A0's by the spread plus 10 percent of A0."""
    return a1_mean <= a0_mean + abs(a0_mean - a0b_mean) + 0.10 * a0_mean + EPS


def _len(runs):
    return _mean(len(r.get("answer", "")) for r in scored(runs))


# ---- records, report ------------------------------------------------------------------------

def load_records(root: Path):
    """runs/<stage>/<arm>/<channel>/<task>/<rep>.json -> list of records."""
    out = []
    for p in sorted(root.glob("runs/*/*/*/*/*.json")):
        try:
            out.append(json.loads(p.read_text()))
        except json.JSONDecodeError:
            continue
    return out


def load_tasks(path: Path):
    return {t["id"]: t for t in (json.loads(l) for l in path.read_text().splitlines() if l.strip())}


def load_judge(root: Path):
    """judge/<stage>/<channel>/<arm>/<task>.json -> {"equivalent": [...], "reasons": [...]}"""
    out = {}
    for p in root.glob("judge/*/*/*/*.json"):
        stage, channel, arm = p.parts[-4], p.parts[-3], p.parts[-2]
        try:
            out[(stage, channel, arm, p.stem)] = json.loads(p.read_text())
        except json.JSONDecodeError:
            continue
    return out


def _rate(judged):
    flags = [bool(x) for j in judged for x in j.get("equivalent", [])]
    return _mean(1.0 if f else 0.0 for f in flags) if flags else None


def evaluate(records, tasks, judge, stage, channel, test_arm="A1", base_arm="A0"):
    """Apply the seven rules for one stage and channel. Returns (rows, failures, notes)."""
    sel = [r for r in records if r["stage"] == stage and r["channel"] == channel]
    arm = lambda a: [r for r in sel if r["arm"] == a]  # noqa: E731
    a0, a0b, a1 = arm(base_arm), arm("A0b"), arm(test_arm)
    failures, notes, rows = [], [], []

    incomplete = defaultdict(int)
    for r in sel:
        if r.get("status") != "complete":
            incomplete[r["task"]] += 1
    for t, n in sorted(incomplete.items()):
        notes.append(f"task {t}: {n} incomplete run(s), kept out of every score")

    groups = defaultdict(list)
    for tid, t in tasks.items():
        groups[t["group"]].append(tid)

    for group, tids in sorted(groups.items()):
        sub = lambda runs: [r for r in runs if r["task"] in tids]  # noqa: E731
        g0, g0b, g1 = sub(a0), sub(a0b), sub(a1)
        if not scored(g1):
            continue
        ag, agb = item_agreement(g0, g1), item_agreement(g0, g0b)
        ok1 = group_agreement_ok(g0, g0b, g1)
        rows.append((f"1 agreement [{group}]", f"{ag:.3f} vs floor {agb:.3f}", ok1))
        if not ok1:
            failures.append(f"rule 1: group {group} agreement {ag:.3f} below A0b {agb:.3f} minus 0.05")
        for tid in tids:
            for item in _items([r for r in g0 if r["task"] == tid]):
                tr = lambda runs: [r for r in runs if r["task"] == tid]  # noqa: E731
                if item_fails(tr(g0), tr(g0b), tr(g1), item):
                    failures.append(f"rule 1: item {tid}/{item} fails alone")
        ok7 = terse_ok(_len(g0), _len(g0b), _len(g1))
        rows.append((f"7 terseness [{group}]",
                     f"A0 {_len(g0):.0f} A0b {_len(g0b):.0f} A1 {_len(g1):.0f} chars", ok7))
        if not ok7:
            failures.append(f"rule 7: group {group} answers grew past the allowance")

    for tid, t in sorted(tasks.items()):
        tr = lambda runs: [r for r in runs if r["task"] == tid]  # noqa: E731
        if not scored(tr(a1)):
            continue
        if t.get("group") == "review":
            ok2 = unseen_ok(tr(a0b), tr(a1))
            rows.append((f"2 unseen [{tid}]",
                         f"A1 {_unseen_per_run(tr(a1)):.2f} vs A0b {_unseen_per_run(tr(a0b)):.2f}", ok2))
            if not ok2:
                failures.append(f"rule 2: task {tid} A1 left more keys unseen than A0b")
        ok3 = calls_ok(tr(a0), tr(a0b), tr(a1))
        rows.append((f"3 calls [{tid}]",
                     f"A0 {_calls(tr(a0)):.1f} A0b {_calls(tr(a0b)):.1f} A1 {_calls(tr(a1)):.1f}", ok3))
        if not ok3:
            failures.append(f"rule 3: task {tid} A1 mean calls outside the allowance")
        moved = tool_share_moves(tr(a0), tr(a0b), tr(a1))
        if moved:
            notes.append(f"task {tid} tool-share moves (owner reads, fails nothing): "
                         + ", ".join(f"{k} {v:+.2f}" for k, v in sorted(moved.items())))
        if t.get("group") == "needs-more":
            ok5 = more_path_ok(t.get("required_facts", []), tr(a0b), tr(a1))
            rows.append((f"5 more path [{tid}]", "A1 vs A0b required facts", ok5))
            if not ok5:
                failures.append(f"rule 5: task {tid} A1 retrieved fewer required facts than A0b")
        j1 = judge.get((stage, channel, test_arm, tid))
        jb = judge.get((stage, channel, "A0b", tid))
        for who, j in ((test_arm, j1), ("A0b", jb)):
            if j is not None and "equivalent" not in j:
                notes.append(f"judge verdict for {tid} ({who}) is incomplete and kept out of rule 4")
        if j1 and jb:
            r1, rb = _rate([j1]), _rate([jb])
            if r1 is not None and rb is not None:
                ok4 = judge_ok(rb, r1)
                rows.append((f"4 answers [{tid}]", f"A1 {r1:.2f} vs A0b {rb:.2f}", ok4))
                if not ok4:
                    failures.append(f"rule 4: task {tid} judge equivalence below A0b minus 0.05")

    leaks = fence_leak(a1)
    rows.append(("6 fence leak", f"{len(leaks)} run(s)", not leaks))
    for r in leaks:
        failures.append(f"rule 6: {r['task']} rep {r['rep']} wrote a fence marker")
    return rows, failures, notes


def token_table(records, stage, channel):
    cells = defaultdict(list)
    for r in scored([r for r in records if r["stage"] == stage and r["channel"] == channel]):
        u = r.get("usage") or {}
        cells[(r["task"], r["arm"])].append(
            (u.get("input", 0) + u.get("cache_read", 0), u.get("output", 0)))
    arms = sorted({a for _, a in cells})
    lines = ["| task | " + " | ".join(f"{a} in/out" for a in arms) + " |",
             "|---|" + "---|" * len(arms)]
    for task in sorted({t for t, _ in cells}):
        row = []
        for a in arms:
            v = cells.get((task, a))
            row.append(f"{_mean(x[0] for x in v):.0f} / {_mean(x[1] for x in v):.0f}" if v else "-")
        lines.append(f"| {task} | " + " | ".join(row) + " |")
    return "\n".join(lines)


def report(records, tasks, judge, test_arm="A1", base_arm="A0"):
    out, failed = ["# G2 agent replay report", ""], False
    combos = sorted({(r["stage"], r["channel"]) for r in records})
    for stage, channel in combos:
        rows, failures, notes = evaluate(records, tasks, judge, stage, channel, test_arm, base_arm)
        out += [f"## {stage} / {channel} ({test_arm} against {base_arm}, noise floor A0b)", "",
                "| rule | measure | result |", "|---|---|---|"]
        out += [f"| {n} | {m} | {'pass' if ok else 'FAIL'} |" for n, m, ok in rows]
        out += [""]
        if failures:
            failed = True
            out += ["Failures:", ""] + [f"- {f}" for f in failures] + [""]
        if notes:
            out += ["Notes:", ""] + [f"- {n}" for n in notes] + [""]
        out += ["Input tokens (input plus cache read) / output tokens, mean per run:", "",
                token_table(records, stage, channel), ""]
    return "\n".join(out), failed


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", type=Path, default=GATE_ROOT)
    ap.add_argument("--tasks", type=Path, required=True)
    ap.add_argument("--arm", default="A1", help="the arm under test")
    ap.add_argument("--base", default="A0", help="the reference arm; A1 when scoring A2")
    ap.add_argument("--out", type=Path)
    args = ap.parse_args(argv)
    text, failed = report(load_records(args.root), load_tasks(args.tasks), load_judge(args.root),
                          args.arm, args.base)
    (args.out.write_text(text) if args.out else print(text))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
