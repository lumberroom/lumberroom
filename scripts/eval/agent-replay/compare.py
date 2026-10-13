"""Agent replay pass rules and the report.

Every rule is a pure function over lists of run records, so `test_compare.py` pins each one on
fixture records. `main` reads a records tree, applies the rules per stage, channel and task group,
prints a markdown report and exits 1 when a rule fails.

Three arms take part: the base build (A0), a second set of runs on the base build (the noise arm,
A0b), and the arm under test (A1). "Spread" is the absolute difference between the base mean and
the noise-arm mean for the quantity under test. Two runs of one build differ by the model's own
noise alone, and the noise arm is the only place the gate measures that.

The report fails closed. Missing evidence is a failure, never a pass: an arm with too few complete
runs on a task, a missing noise arm, or a missing or unparsed judge verdict all fail the gate.
"""
from __future__ import annotations

import argparse
import json
import math
import sys
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # `python3 -I` drops the script dir
import batch
import session

EPS = 1e-9
FENCE_MARKER = "----- data "
# Each arm needs this share of a task's planned runs complete: 3 of 5, or 2 of 3. Below it the
# mode and the means rest on one or two runs, and a single odd run decides the item.
MIN_COMPLETE_SHARE = 0.6


def min_complete(planned_reps):
    return math.ceil(MIN_COMPLETE_SHARE * planned_reps - EPS)


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
    The list informs the reader and fails nothing."""
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
    """Rule 6: runs whose memory or registry writes carry a fence marker line. Every run counts,
    complete or not: a session that wrote a fence before it timed out still wrote it."""
    bad = []
    for r in runs:
        st = r.get("end_state") or {}
        writes = (st.get("written") or []) + (st.get("registry_written") or [])
        if any(FENCE_MARKER in (w.get("content") or "") for w in writes):
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


def judge_path(root: Path, stage, channel, base, arm, task) -> Path:
    """One verdict file. The base sits in the path, so A2 judged against A1 never reads or
    overwrites A1 judged against A0."""
    return root / "judge" / stage / channel / base / arm / f"{task}.json"


def load_judge(root: Path):
    """judge/<stage>/<channel>/<base>/<arm>/<task>.json -> the verdict, keyed by those five.
    A file that does not parse loads as {} and reads as incomplete."""
    out = {}
    for p in root.glob("judge/*/*/*/*/*.json"):
        stage, channel, base, arm = p.parts[-5], p.parts[-4], p.parts[-3], p.parts[-2]
        try:
            out[(stage, channel, base, arm, p.stem)] = json.loads(p.read_text())
        except json.JSONDecodeError:
            out[(stage, channel, base, arm, p.stem)] = {}
    return out


def verdict_complete(j, n_runs) -> bool:
    """One boolean per complete run the verdict scores. A shorter or longer list means the judge
    saw a different set of answers than the gate scores."""
    eq = j.get("equivalent") if isinstance(j, dict) else None
    return (isinstance(eq, list) and len(eq) == n_runs and n_runs > 0
            and all(isinstance(x, bool) for x in eq))


def _rate(j):
    return _mean(1.0 if f else 0.0 for f in j["equivalent"])


def planned_reps(tasks, stage, channel, arm):
    """task id -> planned runs for this stage and channel, from the batch expansion rules."""
    jobs = batch.expand(list(tasks.values()), stage=stage, arms=[arm], model="")
    out = Counter(j["task"] for j in jobs if j["channel"] == channel)
    return dict(out)


def evaluate(records, tasks, judge, stage, channel, test_arm="A1", base_arm="A0", noise_arm="A0b"):
    """Apply the seven rules for one stage and channel. Returns (rows, failures, notes)."""
    sel = [r for r in records if r["stage"] == stage and r["channel"] == channel]
    arm = lambda a: [r for r in sel if r["arm"] == a]  # noqa: E731
    a0, a0b, a1 = arm(base_arm), arm(noise_arm), arm(test_arm)
    failures, notes, rows = [], [], []
    planned = planned_reps(tasks, stage, channel, test_arm)

    incomplete = defaultdict(int)
    for r in sel:
        if r.get("status") != "complete":
            incomplete[r["task"]] += 1
    for t, n in sorted(incomplete.items()):
        notes.append(f"task {t}: {n} incomplete run(s), kept out of every score")

    # Coverage first. A task an arm barely ran cannot pass on the runs it happens to have.
    covered = set()
    for tid, reps in sorted(planned.items()):
        need, ok = min_complete(reps), True
        for a, runs in ((base_arm, a0), (noise_arm, a0b), (test_arm, a1)):
            n = len(scored([r for r in runs if r["task"] == tid]))
            if n < need:
                ok = False
                failures.append(f"coverage: task {tid} {a} has {n} of {reps} complete runs, needs {need}")
        rows.append((f"0 coverage [{tid}]", f"needs {need} of {reps} complete runs per arm", ok))
        if ok:
            covered.add(tid)

    groups = defaultdict(list)
    for tid in planned:
        groups[tasks[tid]["group"]].append(tid)

    for group, tids in sorted(groups.items()):
        sub = lambda runs: [r for r in runs if r["task"] in tids]  # noqa: E731
        g0, g0b, g1 = sub(a0), sub(a0b), sub(a1)
        if not scored(g1):
            failures.append(f"group {group} has no complete {test_arm} run")
            rows.append((f"1 agreement [{group}]", f"no complete {test_arm} run", False))
            continue
        ag, agb = item_agreement(g0, g1), item_agreement(g0, g0b)
        ok1 = group_agreement_ok(g0, g0b, g1)
        rows.append((f"1 agreement [{group}]", f"{ag:.3f} vs floor {agb:.3f}", ok1))
        if not ok1:
            failures.append(f"rule 1: group {group} agreement {ag:.3f} below {noise_arm} {agb:.3f} minus 0.05")
        for tid in tids:
            tr = lambda runs: [r for r in runs if r["task"] == tid]  # noqa: E731
            for item in _items(tr(g0)):
                if item_fails(tr(g0), tr(g0b), tr(g1), item):
                    failures.append(f"rule 1: item {tid}/{item} fails alone")
        ok7 = terse_ok(_len(g0), _len(g0b), _len(g1))
        rows.append((f"7 terseness [{group}]",
                     f"{base_arm} {_len(g0):.0f} {noise_arm} {_len(g0b):.0f} {test_arm} {_len(g1):.0f} chars", ok7))
        if not ok7:
            failures.append(f"rule 7: group {group} answers grew past the allowance")

    for tid in sorted(planned):
        t = tasks[tid]
        tr = lambda runs: [r for r in runs if r["task"] == tid]  # noqa: E731
        if tid in covered:
            if t.get("group") == "review":
                ok2 = unseen_ok(tr(a0b), tr(a1))
                rows.append((f"2 unseen [{tid}]", f"{test_arm} {_unseen_per_run(tr(a1)):.2f} vs "
                             f"{noise_arm} {_unseen_per_run(tr(a0b)):.2f}", ok2))
                if not ok2:
                    failures.append(f"rule 2: task {tid} {test_arm} left more keys unseen than {noise_arm}")
            ok3 = calls_ok(tr(a0), tr(a0b), tr(a1))
            rows.append((f"3 calls [{tid}]", f"{base_arm} {_calls(tr(a0)):.1f} {noise_arm} "
                         f"{_calls(tr(a0b)):.1f} {test_arm} {_calls(tr(a1)):.1f}", ok3))
            if not ok3:
                failures.append(f"rule 3: task {tid} {test_arm} mean calls outside the allowance")
            moved = tool_share_moves(tr(a0), tr(a0b), tr(a1))
            if moved:
                notes.append(f"task {tid} tool-share moves (informs, fails nothing): "
                             + ", ".join(f"{k} {v:+.2f}" for k, v in sorted(moved.items())))
            if t.get("group") == "needs-more":
                ok5 = more_path_ok(t.get("required_facts", []), tr(a0b), tr(a1))
                rows.append((f"5 more path [{tid}]", f"{test_arm} vs {noise_arm} required facts", ok5))
                if not ok5:
                    failures.append(f"rule 5: task {tid} {test_arm} retrieved fewer required facts than {noise_arm}")
        # Rule 4 needs both verdicts whole. A verdict that is missing or did not parse fails.
        verdicts, ok4 = {}, True
        for who in (test_arm, noise_arm):
            j = judge.get((stage, channel, base_arm, who, tid))
            if j is None:
                ok4 = False
                failures.append(f"rule 4: task {tid} has no judge verdict for {who}")
            elif not verdict_complete(j, len(scored([r for r in (a1 if who == test_arm else a0b)
                                                     if r["task"] == tid]))):
                ok4 = False
                failures.append(f"rule 4: task {tid} judge verdict for {who} is incomplete")
            else:
                verdicts[who] = _rate(j)
        if ok4:
            r1, rb = verdicts[test_arm], verdicts[noise_arm]
            ok4 = judge_ok(rb, r1)
            rows.append((f"4 answers [{tid}]", f"{test_arm} {r1:.2f} vs {noise_arm} {rb:.2f}", ok4))
            if not ok4:
                failures.append(f"rule 4: task {tid} judge equivalence below {noise_arm} minus 0.05")
        else:
            rows.append((f"4 answers [{tid}]", "verdict missing or incomplete", False))

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


def report(records, tasks, judge, test_arm="A1", base_arm="A0", noise_arm="A0b"):
    out, failed = ["# Agent replay report", ""], False
    if not records:
        return "\n".join(out + ["FAIL: no run records under the gate root", ""]), True
    combos = {(r["stage"], r["channel"]) for r in records}
    # Every stage and channel the task plan schedules gets evaluated, records or not, so a planned
    # stage or channel with no records fails on coverage instead of dropping out of the report.
    for stage in batch.STAGES:
        jobs = batch.expand(list(tasks.values()), stage=stage, arms=[test_arm], model="")
        combos |= {(stage, j["channel"]) for j in jobs}
    combos = sorted(combos)
    for stage, channel in combos:
        rows, failures, notes = evaluate(records, tasks, judge, stage, channel, test_arm, base_arm,
                                         noise_arm)
        out += [f"## {stage} / {channel} ({test_arm} against {base_arm}, noise floor {noise_arm})", "",
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
    ap.add_argument("--root", type=Path, default=session.gate_root())
    ap.add_argument("--tasks", type=Path, required=True)
    ap.add_argument("--arm", default="A1", help="the arm under test")
    ap.add_argument("--base", default="A0", help="the reference arm; A1 when scoring A2")
    ap.add_argument("--noise", default="A0b", help="second runs on the base build; A1b when scoring A2")
    ap.add_argument("--out", type=Path)
    args = ap.parse_args(argv)
    text, failed = report(load_records(args.root), load_tasks(args.tasks), load_judge(args.root),
                          args.arm, args.base, args.noise)
    (args.out.write_text(text) if args.out else print(text))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
