#!/usr/bin/env python3
"""The probe that runs before batch 1 (spec G2, Probe).

    probe.py --structured-slot D1 --text-slot D2 --tasks tasks.example.jsonl --fixture fixtures/review.archive.json

One session per channel against an A0 slot. Two questions.

1. Which instructions does the model hold? `--setting-sources project` may still load
   `~/.claude/CLAUDE.md`. If it does, the gate accepts it and the report says so: every session of
   every arm loads the same file, so it cannot separate the arms.
2. Does Claude Code read `structuredContent`? The base build's structured review answer carries no
   row text; the text block does. A structured session that cannot quote a stale row shows Claude
   Code fed the model the structured payload. A text session through the proxy that can quote it
   shows the proxy works. Both must hold.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
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


def loaded_user_instructions(answer, owner_claude_md):
    """True when the answer repeats a distinctive line (40+ characters) of the owner's file."""
    a = " ".join(_words(answer))
    for line in owner_claude_md.splitlines():
        line = line.strip()
        if len(line) >= 40 and " ".join(_words(line)) in a:
            return True
    return False


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
    return session.run_session(slot=slot, task=task, stage="probe", arm="A0", channel=channel,
                               rep=1, model=model, root=root)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--structured-slot", required=True)
    ap.add_argument("--text-slot", required=True)
    ap.add_argument("--fixture", type=Path, required=True)
    ap.add_argument("--model", default="claude-sonnet-5-5")
    ap.add_argument("--root", type=Path, default=session.GATE_ROOT)
    a = ap.parse_args(argv)
    fx = json.loads(a.fixture.read_text())
    stale = [m["content"] for m in fx["memories"] if "stale" in m.get("tags", [])]
    owner = Path.home() / ".claude" / "CLAUDE.md"
    owner_text = owner.read_text() if owner.exists() else ""
    out = {}
    for channel, slot in (("structured", a.structured_slot), ("text", a.text_slot)):
        listing = run_one(slot, channel, INSTRUCTIONS, "probe-instructions", a.model, a.root)
        quote = run_one(slot, channel, QUOTE, "probe-quote", a.model, a.root)
        out[channel] = {"status": [listing["status"], quote["status"]],
                        "loads_user_claude_md": loaded_user_instructions(listing["answer"], owner_text),
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
