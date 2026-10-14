#!/usr/bin/env python3
"""One `claude -p` session, one run record.

    session.py --slot <slot>/slot.json --tasks tasks.jsonl --task review-1 \
               --stage sonnet --arm A1 --channel structured --rep 2 --model claude-sonnet-5-5

The command runs on an existing Claude Code subscription login. It uses no Messages API call, no
`--bare` (that flag never reads OAuth), no separate config directory and no `setup-token`.
Isolation comes from flags and an emptied environment, and `check_init` rejects any session whose
`init` event shows they did not hold.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # `python3 -I` drops the script dir
import outcomes


def claude_bin(env=os.environ):
    """The Claude Code binary: LR_GATE_CLAUDE, else `claude` on PATH."""
    return env.get("LR_GATE_CLAUDE") or "claude"


def gate_root(env=os.environ):
    """Where run records, slots, dumps and reports live: LR_GATE_ROOT, else
    ~/.local/share/lumberroom-gate.

    Everything under it carries store content. The default sits under the home directory because a
    path relative to the working directory lands that content in the checkout whenever a script
    runs from the repository root."""
    if env.get("LR_GATE_ROOT"):
        return Path(env["LR_GATE_ROOT"])
    home = Path(env["HOME"]) if env.get("HOME") else Path.home()
    return home / ".local" / "share" / "lumberroom-gate"


CLAUDE = claude_bin()
TIMEOUT_SECS = 20 * 60
SERVER = "lumberroom"
# The twelve tools the engine registers. A session may call these, or the list LR_GATE_TOOLS puts in
# their place, plus the extras LR_GATE_EXTRA_TOOLS declares, and nothing else.
LUMBERROOM_TOOLS = [
    "context_bootstrap", "memory_search", "memory_write", "registry_get", "memory_forget",
    "memory_history", "registry_history", "registry_set", "alias_set", "alias_list",
    "review_queue", "review_decide",
]
# No double underscore: that is how a server-prefixed name such as mcp__lumberroom__x reads.
TOOL_NAME_RE = re.compile(r"[a-z][a-z0-9]*(?:_[a-z0-9]+)*")


def _tool_names(env, var):
    """Bare tool names from a comma-separated variable. Raises ValueError on a malformed name."""
    names = [n.strip() for n in (env.get(var) or "").split(",") if n.strip()]
    bad = [n for n in names if not TOOL_NAME_RE.fullmatch(n)]
    if bad:
        # A prefixed name never matches an init entry, so every session would fail as bad_init.
        raise ValueError(f"{var} holds {bad}: list bare tool names, such as tool_a")
    return names


def extra_tools(env=os.environ):
    """Tools a server registers on top of the base list, from LR_GATE_EXTRA_TOOLS, comma
    separated. A build that adds tools declares them here instead of editing LUMBERROOM_TOOLS, so
    one harness serves every build and `check_init` still rejects a tool nobody declared."""
    return _tool_names(env, "LR_GATE_EXTRA_TOOLS")


def base_tools(env=os.environ):
    """LR_GATE_TOOLS when set, else the engine's twelve. A server whose principal lacks some engine
    tools exposes a set extras cannot reach, because extras only add."""
    if "LR_GATE_TOOLS" not in env:
        return list(LUMBERROOM_TOOLS)
    names = list(dict.fromkeys(_tool_names(env, "LR_GATE_TOOLS")))
    if not names:
        # Set but empty would allow no tool, and every session would fail at check_init.
        raise ValueError("LR_GATE_TOOLS is set but names no tool: list the server's tools, or unset it")
    return names


def session_tools(env=os.environ):
    """The base list plus the declared extras, each once."""
    base = base_tools(env)
    return base + [t for t in dict.fromkeys(extra_tools(env)) if t not in base]


BUILTIN_PLUGINS = {"cc-plugin-agents-md", "cc-plugin-telemetry", "cc-plugin-plugin-authoring"}
# The only variables a session inherits. Claude Code reads ANTHROPIC_* and CLAUDE_CODE_USE_* to pick
# an API key or a cloud provider over the subscription login, and a deny list misses the next one.
ENV_ALLOW = ("HOME", "PATH", "USER", "LANG", "LC_ALL", "TMPDIR", "TERM")
# Set on every session whatever the caller holds. Auto memory is on by default in Claude Code
# 2.1.296 and adds its instructions to the system prompt, so a session would read text no arm chose.
ENV_SET = ("CLAUDE_CODE_DISABLE_AUTO_MEMORY=1",)

# The registry rides along so a fenced `registry_set` reaches the fence rule like a fenced row does.
SNAPSHOT_SQL = (
    "SELECT json_build_object("
    "'memory', (SELECT coalesce(json_agg(t), '[]'::json) FROM (SELECT id::text, namespace, "
    "source_client, supersedes::text, superseded_by::text, occurred_at::text, content "
    "FROM memory) t), "
    "'registry', (SELECT coalesce(json_agg(g), '[]'::json) FROM (SELECT id::text, namespace, kind, "
    "key, value::text AS value, version FROM registry) g))"
)


class Rejected(Exception):
    """The session broke an isolation condition. It records as incomplete with reason bad_init."""


def build_argv(*, claude, prompt, model, mcp_config=None, server=SERVER, tools=None, judge=False,
               env=None):
    """The exact command line. A pure function so a test can pin every flag.

    `env -i` with the allowlist keeps the session on the subscription login, and `ENV_SET` turns
    auto memory off. `--tools ""` removes
    the built-in tools. `--strict-mcp-config` with no `--mcp-config` (the judge) loads no MCP
    server at all."""
    env = os.environ if env is None else env
    argv = ["env", "-i", *(f"{k}={env[k]}" for k in ENV_ALLOW if k in env), *ENV_SET, claude, "-p", prompt,
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


def check_init(ev, server=SERVER, expected_tools=LUMBERROOM_TOOLS):
    """Reject a session whose `init` event breaks an isolation condition: an API key, a server
    other than the slot's or one that did not connect, auto memory on, a tool outside the slot's
    server, a server tool nobody declared, a declared tool that never loaded, or a plugin outside
    the builtin set."""
    if ev.get("apiKeySource") != "none":
        raise Rejected(f"apiKeySource is {ev.get('apiKeySource')!r}, not 'none'")
    servers = ev.get("mcp_servers", []) or []
    names = [s.get("name") for s in servers]
    if names != [server]:
        raise Rejected(f"mcp servers are {names}, not [{server!r}]")
    # A server that failed to connect loads no tools, and the agent would then answer from nothing.
    if servers[0].get("status") != "connected":
        raise Rejected(f"mcp server {server!r} is {servers[0].get('status')!r}, not 'connected'")
    if (ev.get("memory_paths") or {}).get("auto"):
        raise Rejected("auto memory is on; CLAUDE_CODE_DISABLE_AUTO_MEMORY did not hold")
    tools = ev.get("tools", []) or []
    for t in tools:
        if not t.startswith(f"mcp__{server}__"):
            raise Rejected(f"built-in or foreign tool {t!r} is loaded")
    # An undeclared tool would let one arm's agent reach for a tool the other arm lacks.
    undeclared = [t for t in tools if t[len(f"mcp__{server}__"):] not in expected_tools]
    if undeclared:
        raise Rejected(f"undeclared tools in init: {undeclared}; declare them in LR_GATE_EXTRA_TOOLS "
                       "or list the full set in LR_GATE_TOOLS")
    missing = [t for t in expected_tools if f"mcp__{server}__{t}" not in tools]
    if missing:
        raise Rejected(f"lumberroom tools missing from init: {missing}")
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
    """{"memory": id -> row, "registry": id -> entry} for the slot's live database."""
    out = subprocess.run(
        ["docker", "exec", slot["db_container"], "psql", "-U", slot["db_user"], "-d",
         slot["db_name"], "-At", "-c", SNAPSHOT_SQL],
        capture_output=True, text=True, check=True, timeout=120).stdout.strip()
    doc = json.loads(out)
    return {"memory": {r["id"]: r for r in doc["memory"]},
            "registry": {r["id"]: r for r in doc["registry"]}}


def record_path(root: Path, stage, arm, channel, task, rep, suffix=".json", sub="runs"):
    return root / sub / stage / arm / channel / task / f"{rep}{suffix}"


def write_record(path: Path, rec: dict):
    """Write through a temporary file and a rename. A record exists only once it is whole, so
    resume can trust any file it finds."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(rec, indent=1))
    tmp.replace(path)


def run_session(*, slot, task, stage, arm, channel, rep, model, root=None, claude=CLAUDE):
    """Run one session and write its record and transcript. Returns the record."""
    root = gate_root() if root is None else root
    arm_root = Path(slot["checkout"])
    snippet = (arm_root / "client" / "CLAUDE.md.snippet").read_text()
    skill = (arm_root / task["skill"]).read_text() if task.get("skill") else None
    prompt = build_prompt(snippet, task["prompt"], skill)
    tools = session_tools()
    argv = build_argv(claude=claude, prompt=prompt, model=model, mcp_config=slot["mcp_config"],
                      tools=tools)
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
            check_init(parsed["init"], slot.get("server", SERVER), tools)
            for ev in parsed["events"]:
                check_event(ev, slot.get("server", SERVER))
        except Rejected:
            reason = "bad_init"
    if reason is None:
        reason = outcomes.incomplete_reason(parsed) or outcomes.channel_reason(parsed, channel)
    after = snapshot(slot)
    verdicts = outcomes.verdicts_from_calls(parsed["calls"])
    end_state, ignored = outcomes.diff_state(before["memory"], after["memory"], slot["client"],
                                             verdicts)
    end_state["registry_written"] = outcomes.registry_changes(before["registry"], after["registry"])
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
        "file_stubbed": parsed["file_stubbed"], "rate_limit": parsed["rate_limit"],
        "transcript": str(tpath.relative_to(root)),
        "finished_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }
    write_record(record_path(root, stage, arm, channel, task["id"], rep), rec)
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
    ap.add_argument("--root", type=Path, default=None, help="default: LR_GATE_ROOT")
    a = ap.parse_args(argv)
    slot = json.loads(a.slot.read_text())
    tasks = {t["id"]: t for t in (json.loads(l) for l in a.tasks.read_text().splitlines() if l.strip())}
    rec = run_session(slot=slot, task=tasks[a.task], stage=a.stage, arm=a.arm, channel=a.channel,
                      rep=a.rep, model=a.model, root=a.root)
    print(json.dumps({k: rec[k] for k in ("task", "rep", "status", "incomplete_reason")}))
    return 0 if rec["status"] == "complete" else 2


if __name__ == "__main__":
    sys.exit(main())
