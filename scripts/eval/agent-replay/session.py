#!/usr/bin/env python3
"""One `claude -p` session, one run record (spec section 9, G2 "Sessions").

    session.py --slot <slot>/slot.json --tasks tasks.jsonl --task review-1 \
               --stage sonnet --arm A1 --channel structured --rep 2 --model claude-sonnet-5-5

The command runs on the owner's existing subscription login. It uses no Messages API call, no
`--bare` (that flag never reads OAuth), no separate config directory and no `setup-token`.
Isolation comes from flags alone, and `check_init` rejects any session whose `init` event shows the
flags did not hold.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # `python3 -I` drops the script dir
import outcomes

GATE_ROOT = Path.home() / "lumberroom-gate" / "2026-10-mcp-response-tokens"
CLAUDE = str(Path.home() / ".local" / "bin" / "claude")  # not on PATH for non-login shells
TIMEOUT_SECS = 20 * 60
SERVER = "lumberroom"
# The twelve tools the engine registers. A session may call any of them and nothing else.
LUMBERROOM_TOOLS = [
    "context_bootstrap", "memory_search", "memory_write", "registry_get", "memory_forget",
    "memory_history", "registry_history", "registry_set", "alias_set", "alias_list",
    "review_queue", "review_decide",
]
BUILTIN_PLUGINS = {"cc-plugin-agents-md", "cc-plugin-telemetry", "cc-plugin-plugin-authoring"}

SNAPSHOT_SQL = (
    "SELECT coalesce(json_agg(t), '[]'::json) FROM (SELECT id::text, namespace, source_client, "
    "supersedes::text, superseded_by::text, occurred_at::text, content FROM memory) t"
)


class Rejected(Exception):
    """The session broke an isolation condition. It records as incomplete with reason bad_init."""


def build_argv(*, claude, prompt, model, mcp_config=None, server=SERVER, tools=None, judge=False):
    """The exact command line. A pure function so a test can pin every flag.

    `env -u ANTHROPIC_API_KEY` keeps the session on the subscription login. `--tools ""` removes
    the built-in tools. `--strict-mcp-config` with no `--mcp-config` (the judge) loads no MCP
    server at all."""
    argv = ["env", "-u", "ANTHROPIC_API_KEY", claude, "-p", prompt,
            "--setting-sources", "project", "--strict-mcp-config"]
    if not judge:
        argv += ["--mcp-config", mcp_config]
    argv += ["--tools", ""]
    if not judge:
        argv += ["--allowedTools", ",".join(f"mcp__{server}__{t}" for t in tools)]
    argv += ["--no-session-persistence", "--no-chrome", "--disable-slash-commands",
             "--include-hook-events", "--output-format", "stream-json", "--verbose",
             "--model", model]
    return argv


def build_prompt(snippet, task_prompt, skill_text):
    """CLAUDE.md snippet, then the skill body for a review task, then the task."""
    parts = [snippet.strip()]
    if skill_text:
        body = skill_text
        if body.startswith("---\n"):
            body = body.split("\n---\n", 1)[1] if "\n---\n" in body else body
        parts.append(body.strip())
    parts.append(task_prompt.strip())
    return "\n\n".join(parts)


def _plugin_name(p):
    return p.get("name", "") if isinstance(p, dict) else str(p)


def check_init(ev, server=SERVER):
    """Reject a session whose `init` event breaks an isolation condition (spec G2 Sessions)."""
    if ev.get("apiKeySource") != "none":
        raise Rejected(f"apiKeySource is {ev.get('apiKeySource')!r}, not 'none'")
    names = [s.get("name") for s in ev.get("mcp_servers", []) or []]
    if names != [server]:
        raise Rejected(f"mcp servers are {names}, not [{server!r}]")
    for t in ev.get("tools", []) or []:
        if not t.startswith(f"mcp__{server}__"):
            raise Rejected(f"built-in or foreign tool {t!r} is loaded")
    for p in ev.get("plugins", []) or []:
        if _plugin_name(p) not in BUILTIN_PLUGINS:
            raise Rejected(f"plugin {_plugin_name(p)!r} is outside the builtin set")


def check_event(ev, server=SERVER):
    """Reject a hook event, or a tool use outside the slot's server."""
    kind = f"{ev.get('type', '')}/{ev.get('subtype', '')}"
    if "hook" in kind:
        raise Rejected(f"hook event {kind}")
    if ev.get("type") == "assistant":
        for b in ev.get("message", {}).get("content", []) or []:
            if b.get("type") == "tool_use" and not b.get("name", "").startswith(f"mcp__{server}__"):
                raise Rejected(f"tool use {b.get('name')!r} is outside the slot's server")


def snapshot(slot) -> dict:
    """id -> row for every memory row in the slot's database."""
    out = subprocess.run(
        ["docker", "exec", slot["db_container"], "psql", "-U", slot["db_user"], "-d",
         slot["db_name"], "-At", "-c", SNAPSHOT_SQL],
        capture_output=True, text=True, check=True, timeout=120).stdout.strip()
    return {r["id"]: {**r, "supersedes": r["supersedes"], "superseded_by": r["superseded_by"]}
            for r in json.loads(out or "[]")}


def record_path(root: Path, stage, arm, channel, task, rep, suffix=".json", sub="runs"):
    return root / sub / stage / arm / channel / task / f"{rep}{suffix}"


def run_session(*, slot, task, stage, arm, channel, rep, model, root=GATE_ROOT, claude=CLAUDE):
    """Run one session and write its record and transcript. Returns the record."""
    arm_root = Path(slot["checkout"])
    snippet = (arm_root / "client" / "CLAUDE.md.snippet").read_text()
    skill = (arm_root / task["skill"]).read_text() if task.get("skill") else None
    prompt = build_prompt(snippet, task["prompt"], skill)
    argv = build_argv(claude=claude, prompt=prompt, model=model, mcp_config=slot["mcp_config"],
                      tools=LUMBERROOM_TOOLS)
    before = snapshot(slot)
    work = tempfile.mkdtemp(prefix="lr-gate.", dir="/tmp")
    reason, stdout = None, ""
    try:
        proc = subprocess.Popen(argv, cwd=work, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, start_new_session=True)
        try:
            stdout, _ = proc.communicate(timeout=TIMEOUT_SECS)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            stdout, _ = proc.communicate()
            reason = "timeout"
    finally:
        shutil.rmtree(work, ignore_errors=True)
    raw = stdout.splitlines()
    parsed = outcomes.parse_stream(raw)
    if reason is None:
        try:
            if parsed["init"] is None:
                raise Rejected("no init event")
            check_init(parsed["init"], slot.get("server", SERVER))
            for ev in parsed["events"]:
                check_event(ev, slot.get("server", SERVER))
        except Rejected:
            reason = "bad_init"
    if reason is None:
        reason = outcomes.incomplete_reason(parsed)
    after = snapshot(slot)
    verdicts = outcomes.verdicts_from_calls(parsed["calls"])
    end_state, ignored = outcomes.diff_state(before, after, slot["client"], verdicts)
    tpath = record_path(root, stage, arm, channel, task["id"], rep, ".jsonl", "transcripts")
    tpath.parent.mkdir(parents=True, exist_ok=True)
    tpath.write_text("\n".join(raw) + "\n")
    rec = {
        "stage": stage, "arm": arm, "channel": channel, "model": model, "task": task["id"],
        "rep": rep, "status": "complete" if reason is None else "incomplete",
        "incomplete_reason": reason,
        "calls": parsed["calls"], "received_keys": parsed["received_keys"],
        "end_state": end_state, "ignored_rows": ignored,
        "outcomes": outcomes.derive_outcomes(task, parsed["calls"], parsed["received_keys"],
                                             end_state),
        "answer": parsed["answer"], "usage": parsed["usage"] or {"input": 0, "cache_read": 0, "output": 0},
        "transcript": str(tpath.relative_to(root)),
        "finished_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }
    rpath = record_path(root, stage, arm, channel, task["id"], rep)
    rpath.parent.mkdir(parents=True, exist_ok=True)
    tmp = rpath.with_suffix(".tmp")
    tmp.write_text(json.dumps(rec, indent=1))
    tmp.replace(rpath)  # a record exists only once it is whole, so resume can trust the file
    return rec


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--slot", type=Path, required=True)
    ap.add_argument("--tasks", type=Path, required=True)
    ap.add_argument("--task", required=True)
    ap.add_argument("--stage", required=True)
    ap.add_argument("--arm", required=True)
    ap.add_argument("--channel", required=True, choices=["structured", "text"])
    ap.add_argument("--rep", type=int, required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--root", type=Path, default=GATE_ROOT)
    a = ap.parse_args(argv)
    slot = json.loads(a.slot.read_text())
    tasks = {t["id"]: t for t in (json.loads(l) for l in a.tasks.read_text().splitlines() if l.strip())}
    rec = run_session(slot=slot, task=tasks[a.task], stage=a.stage, arm=a.arm, channel=a.channel,
                      rep=a.rep, model=a.model, root=a.root)
    print(json.dumps({k: rec[k] for k in ("task", "rep", "status", "incomplete_reason")}))
    return 0 if rec["status"] == "complete" else 2


if __name__ == "__main__":
    sys.exit(main())
