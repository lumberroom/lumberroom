#!/usr/bin/env python3
"""Plan and run a batch of sessions: resume, concurrency, incomplete handling.

    batch.py expand --tasks tasks.jsonl --stage sonnet --model claude-sonnet-5-5 --out sonnet.jsonl
    batch.py run --batch sonnet.jsonl --tasks tasks.jsonl --limit 40 --concurrency 3

A batch file holds one job per line: stage, arm, channel, task, rep, model. `run` skips every job
whose record is complete, so a batch resumes where the usage window ended. It interleaves the arms
over the same task and repeat, so a model or quota change between windows lands on every arm alike.
It stops queueing at the first usage-limit and reruns incomplete jobs up to three times. A usage
limit spends no attempt, because the session never got to run.

Every job leaves a record, a failed slot reset included, so a crash in the runner shows up in the
report as an incomplete run and never as a silent gap.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import threading
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # `python3 -I` drops the script dir
import session

MAX_ATTEMPTS = 4  # one run plus three reruns
ARMS = ["A0", "A0b", "A1"]
STAGES = ("sonnet", "opus")
# A noise arm is a second set of sessions on its build, so it shares that build's slot.
SLOT_OF = {"A0b": "A0", "A1b": "A1"}
SCRIPT_DIR = Path(__file__).resolve().parent


def key(job):
    return (job["stage"], job["arm"], job["channel"], job["task"], job["rep"])


def expand(tasks, *, stage, arms, model):
    """Jobs for one stage. Sonnet: 5 repeats per decision task, 3 per other task, every channel the
    task lists. Opus: decision tasks on the structured channel, 3 repeats."""
    jobs = []
    for t in tasks:
        if stage == "opus":
            if not t.get("decision"):
                continue
            plan = [("structured", 3)]
        else:
            reps = 5 if t.get("decision") else 3
            plan = [(c, reps) for c in t["channels"]]
        for channel, reps in plan:
            for arm in arms:
                for rep in range(1, reps + 1):
                    jobs.append({"stage": stage, "arm": arm, "channel": channel,
                                 "task": t["id"], "rep": rep, "model": model})
    return jobs


def interleave(jobs):
    """Order by channel, task, repeat, then arm, so arms alternate over identical work."""
    arm_rank = {a: i for i, a in enumerate(ARMS + ["A1b", "A2"])}
    return sorted(jobs, key=lambda j: (j["stage"], j["channel"], j["task"], j["rep"],
                                       arm_rank.get(j["arm"], 9)))


def pending(jobs, existing):
    """(todo, given_up). `existing` maps a job key to its record. A complete record is skipped; an
    incomplete one reruns until it has used MAX_ATTEMPTS; after that it is reported and not queued."""
    todo, given_up = [], []
    for j in jobs:
        rec = existing.get(key(j))
        if rec is None:
            todo.append(j)
        elif rec["status"] == "complete":
            continue
        elif rec.get("attempts", 1) >= MAX_ATTEMPTS:
            given_up.append(key(j))
        else:
            todo.append(j)
    return todo, given_up


def next_attempts(prior, reason):
    """A usage limit cut the session off before it did anything, so it spends no attempt."""
    return prior if reason == "usage_limit" else prior + 1


def slot_name(job):
    return f"{SLOT_OF.get(job['arm'], job['arm'])}-{job['channel']}"


def failed_record(job, reason, error):
    """A record for a job whose session never ran. It scores nowhere and reruns like any other."""
    return {**{k: job[k] for k in ("stage", "arm", "channel", "task", "rep", "model")},
            "status": "incomplete", "incomplete_reason": reason, "error": error[-500:],
            "calls": [], "received_keys": [], "outcomes": {}, "answer": "",
            "end_state": {"written": [], "registry_written": [], "superseded": [], "deleted": [],
                          "verdicts": {}},
            "ignored_rows": [], "usage": {"input": 0, "cache_read": 0, "output": 0},
            "finished_at": datetime.now(timezone.utc).isoformat(timespec="seconds")}


def reset_slot(slot_dir: Path, template):
    """Fresh database from the task's template, then the slot's description."""
    cmd = [str(SCRIPT_DIR / "slot.sh"), "reset", str(slot_dir)] + ([template] if template else [])
    subprocess.run(cmd, check=True, capture_output=True, text=True)
    return json.loads((slot_dir / "slot.json").read_text())


def run_job(job, *, root, task, prior, reset=reset_slot, run_session=session.run_session):
    """Reset the slot, run one session, write its record with `attempts`. Never raises for a
    failed reset or a runner error: those become incomplete records."""
    path = session.record_path(root, job["stage"], job["arm"], job["channel"], job["task"], job["rep"])
    try:
        slot = reset(root / "slots" / slot_name(job), task.get("template"))
    except (subprocess.CalledProcessError, OSError, ValueError) as e:
        detail = getattr(e, "stderr", None) or str(e)
        rec = failed_record(job, "slot_reset", str(detail))
    else:
        try:
            rec = run_session(slot=slot, task=task, stage=job["stage"], arm=job["arm"],
                              channel=job["channel"], rep=job["rep"], model=job["model"], root=root)
        except (subprocess.SubprocessError, OSError, ValueError, KeyError) as e:
            rec = failed_record(job, "runner_error", repr(e))
    rec["attempts"] = next_attempts(prior, rec["incomplete_reason"])
    session.write_record(path, rec)
    return rec


def load_existing(root: Path):
    out = {}
    for p in root.glob("runs/*/*/*/*/*.json"):
        try:
            r = json.loads(p.read_text())
            out[(r["stage"], r["arm"], r["channel"], r["task"], r["rep"])] = r
        except (json.JSONDecodeError, KeyError):
            continue
    return out


def run(args):
    root = args.root
    tasks = {t["id"]: t for t in (json.loads(l) for l in args.tasks.read_text().splitlines() if l.strip())}
    jobs = [json.loads(l) for l in args.batch.read_text().splitlines() if l.strip()]
    existing = load_existing(root)
    todo, given_up = pending(interleave(jobs), existing)
    for g in given_up:
        print("GIVEN UP (incomplete after three reruns):", "/".join(map(str, g)))
    if args.limit:
        todo = todo[: args.limit]
    print(f"{len(todo)} sessions queued, {len(jobs) - len(todo) - len(given_up)} already complete")
    stop = threading.Event()
    slot_locks = {}
    guard = threading.Lock()

    def lock_for(name):
        with guard:
            return slot_locks.setdefault(name, threading.Lock())

    def one(job):
        if stop.is_set():
            return None
        with lock_for(slot_name(job)):
            if stop.is_set():
                return None
            # The slot resets before every session: a fresh database from the task's template.
            prior = existing.get(key(job), {}).get("attempts", 0)
            rec = run_job(job, root=root, task=tasks[job["task"]], prior=prior)
            if rec["incomplete_reason"] == "usage_limit":
                stop.set()  # leave the rest for the next window
            print(f"{'/'.join(map(str, key(job)))}: {rec['status']} {rec['incomplete_reason'] or ''}")
            return rec

    with ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        list(pool.map(one, todo))
    if stop.is_set():
        print("usage limit reached: batch stopped, rerun `batch.py run` in the next window")
        return 3
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("expand", help="write the full job list for a stage")
    e.add_argument("--tasks", type=Path, required=True)
    e.add_argument("--stage", choices=STAGES, required=True)
    e.add_argument("--model", required=True)
    e.add_argument("--arms", nargs="+", default=ARMS)
    e.add_argument("--out", type=Path, required=True)
    r = sub.add_parser("run", help="run pending jobs from a batch file")
    r.add_argument("--batch", type=Path, required=True)
    r.add_argument("--tasks", type=Path, required=True)
    r.add_argument("--limit", type=int, default=0, help="sessions in this batch; 0 means all pending")
    r.add_argument("--concurrency", type=int, default=3)
    r.add_argument("--root", type=Path, default=session.gate_root())
    a = ap.parse_args(argv)
    if a.cmd == "expand":
        tasks = [json.loads(l) for l in a.tasks.read_text().splitlines() if l.strip()]
        jobs = interleave(expand(tasks, stage=a.stage, arms=a.arms, model=a.model))
        a.out.write_text("".join(json.dumps(j) + "\n" for j in jobs))
        print(f"{len(jobs)} jobs -> {a.out}")
        return 0
    return run(a)


if __name__ == "__main__":
    sys.exit(main())
