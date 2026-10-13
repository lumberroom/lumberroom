#!/usr/bin/env python3
"""Plan and run a batch of sessions: resume, concurrency, incomplete handling.

    batch.py expand --tasks tasks.jsonl --stage sonnet --model claude-sonnet-5-5 --out sonnet.jsonl
    batch.py run --batch sonnet.jsonl --tasks tasks.jsonl --limit 40 --concurrency 3

A batch file holds one job per line: stage, arm, channel, task, rep, model. `run` skips every job
whose record is complete, so a batch resumes where the usage window ended. It interleaves the arms
over the same task and repeat, so a model or quota change between windows lands on every arm alike;
`--keep-order` runs the file's own order instead. It stops queueing at the first usage limit, at a
rate-limit event past `--max-five-hour` or with any status but `allowed`, and when free disk under
the gate root falls below `--min-free-gb`. It reruns incomplete jobs up to three times. A usage
limit spends no attempt, because the session never got to run.

Before the first session it checks every slot the queue uses: the slot directory exists, and its
`tools/list` declares `anthropic/maxResultSizeChars` on `memory_search` (decision 0032).

Every job leaves a record, a failed slot reset included, so a crash in the runner shows up in the
report as an incomplete run and never as a silent gap.
"""
from __future__ import annotations

import argparse
import functools
import json
import shutil
import subprocess
import sys
import threading
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # `python3 -I` drops the script dir
import probe
import session

MAX_ATTEMPTS = 4  # one run plus three reruns
ARMS = ["A0", "A0b", "A1"]
STAGES = ("sonnet", "opus")
# A noise arm is a second set of sessions on its build, so by default it shares that build's slot.
# An arm with a build of its own takes its own slot with `--slot-of A1b=A1b`.
DEFAULT_SLOT_OF = {"A0b": "A0", "A1b": "A1"}
MAX_FIVE_HOUR = 0.6
MIN_FREE_GB = 5.0
# Exit codes for a batch that stopped early. Each one means rerun `batch.py run` once it clears.
EXIT_RATE_LIMIT, EXIT_DISK, EXIT_SLOTS = 3, 4, 5
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


def order_jobs(jobs, keep_order):
    """The batch file's own order with `keep_order`, else arms interleaved."""
    return list(jobs) if keep_order else interleave(jobs)


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


def slot_map(pairs):
    """DEFAULT_SLOT_OF with each `ARM=SLOT` pair laid over it."""
    out = dict(DEFAULT_SLOT_OF)
    for pair in pairs or []:
        arm, sep, slot = pair.partition("=")
        if not sep or not arm or not slot:
            raise ValueError(f"--slot-of {pair!r}: write ARM=SLOT")
        out[arm] = slot
    return out


def slot_name(job, slot_of=DEFAULT_SLOT_OF):
    return f"{slot_of.get(job['arm'], job['arm'])}-{job['channel']}"


def rate_limit_stop(info, max_five_hour):
    """True when a session's last rate-limit event says to stop starting sessions: a status other
    than `allowed`, or five-hour utilization past `max_five_hour`. Claude Code 2.1.x puts the
    utilization under `unifiedWindows.five_hour`; a flat `utilization` on a five-hour event counts
    too."""
    if not info:
        return False
    if info.get("status", "allowed") != "allowed":
        return True
    used = ((info.get("unifiedWindows") or {}).get("five_hour") or {}).get("utilization")
    if used is None and info.get("rateLimitType") == "five_hour":
        used = info.get("utilization")
    return isinstance(used, (int, float)) and used > max_five_hour


def disk_stop(free_bytes, min_free_gb):
    """True when free space under the gate root has fallen below the floor. Transcripts, records
    and any answer Claude Code saves to a file all grow with each session."""
    return free_bytes < min_free_gb * 2**30


def existing_parent(path: Path) -> Path:
    """`path`, or its nearest parent that exists, so free space reads before the directory does."""
    path = Path(path)
    while not path.exists() and path != path.parent:
        path = path.parent
    return path


# Claude Code writes a tool answer it saves to a file under here, outside the gate root.
SPILL_DIR = Path.home() / ".claude" / "projects"


class Halt:
    """The batch's stop flag. The first stop keeps its exit code and message: a later stop that
    races it only confirms the batch is stopping."""

    def __init__(self):
        self._event, self._lock = threading.Event(), threading.Lock()
        self.code = self.message = None

    def halt(self, code, message):
        with self._lock:
            if self.code is None:
                self.code, self.message = code, message
        self._event.set()

    def is_set(self):
        return self._event.is_set()


def preflight(names, root: Path, fetch=probe.fetch_tools, declarations=True):
    """Problems with the slots a queue uses, one line each. A slot directory must exist, and with
    `declarations` its tools/list must carry decision 0032's size declaration."""
    problems, present = [], []
    for name in names:
        d = root / "slots" / name
        if d.is_dir():
            present.append(d)
        else:
            problems.append(f"{name}: no slot at {d}")
    if declarations:
        problems += probe.declaration_problems(present, fetch=fetch)
    return problems


def failed_record(job, reason, error):
    """A record for a job whose session never ran. It scores nowhere and reruns like any other."""
    return {**{k: job[k] for k in ("stage", "arm", "channel", "task", "rep", "model")},
            "status": "incomplete", "incomplete_reason": reason, "error": error[-500:],
            "calls": [], "received_keys": [], "outcomes": {}, "answer": "",
            "end_state": {"written": [], "registry_written": [], "superseded": [], "deleted": [],
                          "verdicts": {}},
            "ignored_rows": [], "usage": {"input": 0, "cache_read": 0, "output": 0},
            "rate_limit": None, "file_stubbed": False,
            "finished_at": datetime.now(timezone.utc).isoformat(timespec="seconds")}


def reset_slot(slot_dir: Path, template):
    """Fresh database from the task's template, then the slot's description."""
    cmd = [str(SCRIPT_DIR / "slot.sh"), "reset", str(slot_dir)] + ([template] if template else [])
    subprocess.run(cmd, check=True, capture_output=True, text=True)
    return json.loads((slot_dir / "slot.json").read_text())


def run_job(job, *, root, task, prior, slot_of=DEFAULT_SLOT_OF, reset=reset_slot,
            run_session=session.run_session):
    """Reset the slot, run one session, write its record with `attempts`. Never raises for a
    failed reset or a runner error: those become incomplete records."""
    path = session.record_path(root, job["stage"], job["arm"], job["channel"], job["task"], job["rep"])
    try:
        slot = reset(root / "slots" / slot_name(job, slot_of), task.get("template"))
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


def run(args, *, run_job=run_job, disk_free=lambda p: shutil.disk_usage(p).free, check_slots=None,
        spill_dir=SPILL_DIR):
    root = args.root
    slot_of = slot_map(args.slot_of)
    tasks = {t["id"]: t for t in (json.loads(l) for l in args.tasks.read_text().splitlines() if l.strip())}
    jobs = [json.loads(l) for l in args.batch.read_text().splitlines() if l.strip()]
    existing = load_existing(root)
    todo, given_up = pending(order_jobs(jobs, args.keep_order), existing)
    for g in given_up:
        print("GIVEN UP (incomplete after three reruns):", "/".join(map(str, g)))
    if args.limit:
        todo = todo[: args.limit]
    print(f"{len(todo)} sessions queued, {len(jobs) - len(todo) - len(given_up)} already complete")
    if check_slots is None:
        check_slots = functools.partial(preflight, declarations=not args.allow_undeclared)
    names = sorted({slot_name(j, slot_of) for j in todo})
    problems = check_slots(names, root) if names else []
    if problems:
        for line in problems:
            print("SLOT CHECK FAILED:", line)
        return EXIT_SLOTS
    stop = Halt()
    slot_locks = {}
    guard = threading.Lock()

    def lock_for(name):
        with guard:
            return slot_locks.setdefault(name, threading.Lock())

    halt = stop.halt

    def one(job):
        if stop.is_set():
            return None
        with lock_for(slot_name(job, slot_of)):
            if stop.is_set():
                return None
            # Saved answers land under spill_dir, which can sit on another disk: the lower wins.
            free, where = min((disk_free(p), p) for p in (root, existing_parent(spill_dir)))
            if disk_stop(free, args.min_free_gb):
                halt(EXIT_DISK, f"free disk under {where} is {free / 2**30:.1f} GB, below "
                                f"{args.min_free_gb} GB: batch stopped, free space and rerun `batch.py run`")
                return None
            # The slot resets before every session: a fresh database from the task's template.
            prior = existing.get(key(job), {}).get("attempts", 0)
            rec = run_job(job, root=root, task=tasks[job["task"]], prior=prior, slot_of=slot_of)
            if rec["incomplete_reason"] == "usage_limit":
                halt(EXIT_RATE_LIMIT, "usage limit reached: batch stopped, rerun `batch.py run` in the next window")
            elif rate_limit_stop(rec.get("rate_limit"), args.max_five_hour):
                halt(EXIT_RATE_LIMIT, f"rate limit event {json.dumps(rec['rate_limit'])[:200]} passed "
                                      f"the stop rule (status other than allowed, or five-hour "
                                      f"utilization over {args.max_five_hour}): batch stopped, rerun "
                                      "`batch.py run` in the next window")
            print(f"{'/'.join(map(str, key(job)))}: {rec['status']} {rec['incomplete_reason'] or ''}")
            return rec

    with ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        list(pool.map(one, todo))
    if stop.is_set():
        print(stop.message)
        return stop.code
    return 0


def build_parser():
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
    r.add_argument("--keep-order", action="store_true",
                   help="run jobs in the batch file's order instead of interleaving the arms")
    r.add_argument("--slot-of", action="append", default=[], metavar="ARM=SLOT",
                   help="slot name prefix for an arm; repeatable. Default A0b=A0 A1b=A1. "
                        "Give an arm with its own build its own slot, such as A1b=A1b")
    r.add_argument("--max-five-hour", type=float, default=MAX_FIVE_HOUR,
                   help="stop starting sessions once five-hour utilization passes this share")
    r.add_argument("--min-free-gb", type=float, default=MIN_FREE_GB,
                   help="stop starting sessions once free disk under the gate root falls below this")
    r.add_argument("--allow-undeclared", action="store_true",
                   help="skip the tools/list size declaration check; only for an arm whose answers "
                        "never pass 50,000 characters")
    return ap


def main(argv=None):
    a = build_parser().parse_args(argv)
    if a.cmd == "expand":
        tasks = [json.loads(l) for l in a.tasks.read_text().splitlines() if l.strip()]
        jobs = interleave(expand(tasks, stage=a.stage, arms=a.arms, model=a.model))
        a.out.write_text("".join(json.dumps(j) + "\n" for j in jobs))
        print(f"{len(jobs)} jobs -> {a.out}")
        return 0
    return run(a)


if __name__ == "__main__":
    sys.exit(main())
