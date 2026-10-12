#!/usr/bin/env python3
"""Judge sessions through `claude -p` (spec G2, Models and repeats).

    judge.py --root ~/lumberroom-gate/... --tasks tasks.jsonl --stage sonnet --channel structured --arm A1

One session per task, channel and compared arm. The session loads no MCP server and no built-in
tool. It sees the task, A0's final answers and the answers under test, and replies with one JSON
line. A reply that does not parse reruns once, then the verdict records as incomplete.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
import shutil
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # `python3 -I` drops the script dir
import compare
import outcomes
import session

JUDGE_MODEL = "claude-opus-5-5"


def build_judge_prompt(task_prompt, reference_answers, tested_answers):
    def block(label, answers):
        return "\n\n".join(f"[{label} {i + 1}]\n{a}" for i, a in enumerate(answers))
    return (
        "You compare answers from an AI agent that was given the same task on two builds of a "
        "memory server. Judge each answer under test against the reference answers: is it "
        "equivalent in substance (same facts, same decisions, same actions reported), ignoring "
        "wording and length?\n\n"
        f"TASK:\n{task_prompt}\n\nREFERENCE ANSWERS:\n{block('reference', reference_answers)}\n\n"
        f"ANSWERS UNDER TEST:\n{block('test', tested_answers)}\n\n"
        f"Reply with one line of JSON and nothing else: "
        '{"equivalent": [<true or false for each of the ' f"{len(tested_answers)} answers under test, in order>], "
        '"reasons": ["<one short reason per answer>"]}'
    )


def parse_reply(text, n):
    """The reply as a dict when its last JSON line carries n flags, else None."""
    for line in reversed([l.strip() for l in text.splitlines() if l.strip()]):
        if not line.startswith("{"):
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        eq = obj.get("equivalent")
        if isinstance(eq, list) and len(eq) == n and all(isinstance(x, bool) for x in eq):
            return {"equivalent": eq, "reasons": obj.get("reasons", [])}
    return None


def ask(prompt, n, claude=session.CLAUDE, model=JUDGE_MODEL):
    argv = session.build_argv(claude=claude, prompt=prompt, model=model, judge=True)
    work = tempfile.mkdtemp(prefix="lr-gate.", dir="/tmp")
    try:
        out = subprocess.run(argv, cwd=work, capture_output=True, text=True,
                             timeout=session.TIMEOUT_SECS).stdout
    except subprocess.TimeoutExpired:
        return None
    finally:
        shutil.rmtree(work, ignore_errors=True)
    parsed = outcomes.parse_stream(out.splitlines())
    return parse_reply(parsed["answer"], n)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", type=Path, default=session.GATE_ROOT)
    ap.add_argument("--tasks", type=Path, required=True)
    ap.add_argument("--stage", required=True)
    ap.add_argument("--channel", required=True)
    ap.add_argument("--arms", nargs="+", default=["A0b", "A1"], help="the arms judged against --base")
    ap.add_argument("--base", default="A0")
    a = ap.parse_args(argv)
    tasks = compare.load_tasks(a.tasks)
    recs = compare.scored([r for r in compare.load_records(a.root)
                           if r["stage"] == a.stage and r["channel"] == a.channel])
    for arm in a.arms:
        for tid, t in sorted(tasks.items()):
            ref = [r["answer"] for r in recs if r["arm"] == a.base and r["task"] == tid]
            test = [r["answer"] for r in recs if r["arm"] == arm and r["task"] == tid]
            if not ref or not test:
                continue
            out = a.root / "judge" / a.stage / a.channel / arm / f"{tid}.json"
            out.parent.mkdir(parents=True, exist_ok=True)
            prompt = build_judge_prompt(t["prompt"], ref, test)
            got = ask(prompt, len(test)) or ask(prompt, len(test))  # one rerun
            out.write_text(json.dumps(got if got else {"status": "incomplete"}, indent=1))
            print(f"{arm}/{tid}: {'judged' if got else 'incomplete'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
