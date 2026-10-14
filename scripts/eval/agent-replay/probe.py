#!/usr/bin/env python3
"""The probe that runs before batch 1.

    probe.py --structured-slot SLOT_S --text-slot SLOT_T --fixture fixtures/review.archive.json

One session per channel against an A0 slot. Two questions.

1. Which instructions does the model hold? `--setting-sources project` may still load
   `~/.claude/CLAUDE.md`. If it does, the gate accepts it and the report says so: every session of
   every arm loads the same file, so it cannot separate the arms.
2. Does Claude Code read `structuredContent`? The base build's structured review answer carries no
   row text; the text block does. A structured session that cannot quote a stale row shows Claude
   Code fed the model the structured payload. A text session through the proxy that can quote it
   shows the proxy works. Both must hold.

Before either session it reads each slot's `tools/list` through the slot's `mcp.json`, the URL
Claude Code reads, and fails when `memory_search` lacks the `anthropic/maxResultSizeChars`
declaration (decision 0032). Without it Claude Code saves a large answer to a file, and the arm
searches differently for a reason outside the change under test.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # `python3 -I` drops the script dir

import session

QUOTE_WORDS = 6
INSTRUCTIONS = ("List every instruction you were given before this message, word for word, "
                "including anything loaded from files such as CLAUDE.md or AGENTS.md.")
QUOTE = ("Call review_queue with source [\"stale\"] and limit 1. Quote, word for word, the text of "
         "the row it lists. If the answer carries no row text, reply exactly: NO TEXT.")


def _words(s):
    return re.findall(r"[a-z0-9_.:/-]+", s.lower())


def quotes_row(answer, contents):
    """True when the answer holds QUOTE_WORDS consecutive words of any stale row's content."""
    a = " ".join(_words(answer))
    for c in contents:
        w = _words(c)
        for i in range(0, max(1, len(w) - QUOTE_WORDS + 1)):
            if len(w) >= QUOTE_WORDS and " ".join(w[i:i + QUOTE_WORDS]) in a:
                return True
    return False


def loaded_user_instructions(answer, user_claude_md):
    """True when the answer repeats a distinctive line (40+ characters) of the user's file."""
    a = " ".join(_words(answer))
    for line in user_claude_md.splitlines():
        line = line.strip()
        if len(line) >= 40 and " ".join(_words(line)) in a:
            return True
    return False


RESULT_SIZE_KEY = "anthropic/maxResultSizeChars"
# Claude Code's own threshold. A declaration at or under it changes nothing.
CLIENT_DEFAULT_CHARS = 50_000
# The read tools whose large answers must reach the model inline. Decision 0032 declares six;
# memory_search is the one every task calls and the one a limit-50 answer pushes past 50,000.
NEEDS_INLINE = ("memory_search",)


def missing_result_size(tools):
    """Names in NEEDS_INLINE whose `tools/list` entry lacks a size declaration above the client's
    default. A tool absent from the list counts as missing."""
    by_name = {t.get("name"): t for t in tools}
    out = []
    for name in NEEDS_INLINE:
        meta = (by_name.get(name) or {}).get("_meta") or {}
        v = meta.get(RESULT_SIZE_KEY)
        if not isinstance(v, (int, float)) or v <= CLIENT_DEFAULT_CHARS:
            out.append(name)
    return out


def mcp_endpoint(slot_dir):
    """(url, token) from the slot's mcp.json: the proxy on the text channel, the arm on the other."""
    cfg = json.loads((Path(slot_dir) / "mcp.json").read_text())["mcpServers"][session.SERVER]
    return cfg["url"], cfg["headers"]["Authorization"].split(" ", 1)[1]


def _body(text):
    text = text.strip()
    if text.startswith("{"):
        return json.loads(text)
    for line in text.splitlines():
        if line.startswith("data:"):
            return json.loads(line[5:].strip())
    raise ValueError("no JSON in the tools/list answer")


def fetch_tools(slot_dir):
    """Every `tools/list` entry the slot serves, following `nextCursor`."""
    url, token = mcp_endpoint(slot_dir)
    tools, cursor, n = [], None, 0
    while True:
        n += 1
        params = {"cursor": cursor} if cursor else {}
        req = urllib.request.Request(
            url, method="POST",
            data=json.dumps({"jsonrpc": "2.0", "id": n, "method": "tools/list", "params": params}).encode(),
            headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json",
                     "Accept": "application/json, text/event-stream"})
        with urllib.request.urlopen(req, timeout=60) as r:
            msg = _body(r.read().decode())
        if "error" in msg:
            raise RuntimeError(f"tools/list: {msg['error']}")
        tools += msg["result"].get("tools", [])
        cursor = msg["result"].get("nextCursor")
        if not cursor:
            return tools


def declaration_problems(slot_dirs, fetch=fetch_tools):
    """One line per slot whose tools/list cannot be read or lacks the declaration."""
    out = []
    for d in slot_dirs:
        try:
            missing = missing_result_size(fetch(d))
        except (OSError, ValueError, KeyError, RuntimeError) as e:
            out.append(f"{Path(d).name}: tools/list unreadable ({e})")
            continue
        if missing:
            out.append(f"{Path(d).name}: {', '.join(missing)} lacks _meta {RESULT_SIZE_KEY} above "
                       f"{CLIENT_DEFAULT_CHARS}; build the arm at or after decision 0032, or put a "
                       "proxy that adds it in front")
    return out


def verdict(structured_quotes, text_quotes):
    if structured_quotes:
        return {"pass": False, "reason": "the structured session quoted row text the structured "
                "payload does not carry; the isolation or the base build is not what the gate assumes"}
    if not text_quotes:
        return {"pass": False, "reason": "the text session could not quote the row; the proxy is not "
                "delivering the text block"}
    return {"pass": True, "reason": "structured cannot quote, text can"}


def run_one(slot_dir, channel, prompt, task_id, model, root):
    slot = json.loads((Path(slot_dir) / "slot.json").read_text())
    task = {"id": task_id, "prompt": prompt, "items": []}
    # Under probe/, so the probe's records never land in the runs tree that batch.py resumes from
    # and compare.py scores.
    return session.run_session(slot=slot, task=task, stage="probe", arm="A0", channel=channel,
                               rep=1, model=model, root=root / "probe")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--structured-slot", required=True)
    ap.add_argument("--text-slot", required=True)
    ap.add_argument("--fixture", type=Path, required=True)
    ap.add_argument("--model", default="claude-sonnet-5-5")
    ap.add_argument("--root", type=Path, default=session.gate_root())
    a = ap.parse_args(argv)
    try:
        session.session_tools()
    except ValueError as e:
        # Every session would fail as bad_init, so stop before reading a slot.
        print("FAIL:", e, file=sys.stderr)
        return 2
    problems = declaration_problems([a.structured_slot, a.text_slot])
    if problems:
        for line in problems:
            print("FAIL:", line, file=sys.stderr)
        return 1
    fx = json.loads(a.fixture.read_text())
    stale = [m["content"] for m in fx["memories"] if "stale" in m.get("tags", [])]
    user_file = Path.home() / ".claude" / "CLAUDE.md"
    user_text = user_file.read_text() if user_file.exists() else ""
    out = {}
    for channel, slot in (("structured", a.structured_slot), ("text", a.text_slot)):
        listing = run_one(slot, channel, INSTRUCTIONS, "probe-instructions", a.model, a.root)
        quote = run_one(slot, channel, QUOTE, "probe-quote", a.model, a.root)
        out[channel] = {"status": [listing["status"], quote["status"]],
                        "loads_user_claude_md": loaded_user_instructions(listing["answer"], user_text),
                        "quotes_row": quote["status"] == "complete" and quotes_row(quote["answer"], stale),
                        "instructions_answer": listing["answer"], "quote_answer": quote["answer"]}
    v = verdict(out["structured"]["quotes_row"], out["text"]["quotes_row"])
    complete = all(s == "complete" for c in out.values() for s in c["status"])
    result = {"verdict": v, "all_sessions_complete": complete,
              "user_claude_md_loaded": out["structured"]["loads_user_claude_md"] or out["text"]["loads_user_claude_md"],
              "channels": out}
    path = a.root / "probe.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(result, indent=1))
    print(json.dumps({"pass": v["pass"] and complete, "reason": v["reason"],
                      "user_claude_md_loaded": result["user_claude_md_loaded"], "record": str(path)}))
    return 0 if v["pass"] and complete else 1


if __name__ == "__main__":
    sys.exit(main())
