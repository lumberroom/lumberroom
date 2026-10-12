"""Turn one session's stream-json and database diff into the run record's fields.

Everything here is a pure function over parsed data, so `test_outcomes.py` covers it without a
model or a database.
"""
from __future__ import annotations

import json
import re

PREFIX = "mcp__lumberroom__"
# A review key. The class stops at quotes, commas and whitespace so a key inside JSON text ends
# where the JSON string ends.
KEY_RE = re.compile(r"\b(?:conflict|stale|undated|proposal):[A-Za-z0-9_.:-]+")
USAGE_LIMIT_RE = re.compile(r"usage limit|limit reached|hit your limit|rate.?limit", re.I)
# Transport failures only. A server's own refusal ("proposal_moved", "repair_refused") is a
# legitimate answer the agent must handle, so it never marks a run incomplete.
TRANSPORT_RE = re.compile(r"connection (refused|closed|reset)|ECONNREFUSED|failed to connect|"
                          r"mcp server .*(disconnected|not connected)|timed out", re.I)


def _result_text(block) -> str:
    c = block.get("content")
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "\n".join(b.get("text", "") for b in c if isinstance(b, dict))
    return ""


def parse_stream(raw_lines) -> dict:
    """Parse stream-json lines. `bad_lines` counts lines that were not JSON."""
    calls, by_id, keys = [], {}, []
    out = {"init": None, "calls": calls, "received_keys": keys, "answer": "", "usage": None,
           "result": None, "bad_lines": 0, "refusal": False, "transport_error": False,
           "events": []}
    for line in raw_lines:
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            out["bad_lines"] += 1
            continue
        out["events"].append(ev)
        t = ev.get("type")
        if t == "system" and ev.get("subtype") == "init":
            out["init"] = ev
        elif t == "assistant":
            msg = ev.get("message", {})
            if msg.get("stop_reason") == "refusal":
                out["refusal"] = True
            for b in msg.get("content", []) or []:
                if b.get("type") == "tool_use":
                    name = b.get("name", "")
                    rec = {"tool": name[len(PREFIX):] if name.startswith(PREFIX) else name,
                           "args": b.get("input", {}), "ok": True}
                    calls.append(rec)
                    by_id[b.get("id")] = rec
        elif t == "user":
            for b in ev.get("message", {}).get("content", []) or []:
                if not isinstance(b, dict) or b.get("type") != "tool_result":
                    continue
                rec = by_id.get(b.get("tool_use_id"))
                text = _result_text(b)
                if b.get("is_error"):
                    if rec is not None:
                        rec["ok"] = False
                    if TRANSPORT_RE.search(text):
                        out["transport_error"] = True
                if rec is not None and rec["tool"] == "review_queue":
                    for k in KEY_RE.findall(text):
                        if k not in keys:
                            keys.append(k)
        elif t == "result":
            out["result"] = ev
            out["answer"] = ev.get("result", "") or ""
            u = ev.get("usage") or {}
            out["usage"] = {
                "input": u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0),
                "cache_read": u.get("cache_read_input_tokens", 0),
                "output": u.get("output_tokens", 0)}
    return out


def incomplete_reason(parsed: dict):
    """The record's `incomplete_reason`, or None when the session finished cleanly."""
    res = parsed["result"]
    if res is not None and USAGE_LIMIT_RE.search(res.get("result", "") or "") and res.get("is_error"):
        return "usage_limit"
    if parsed["refusal"]:
        return "refusal"
    if parsed["transport_error"]:
        return "mcp_error"
    if parsed["bad_lines"] or res is None:
        return "bad_output"
    if res.get("is_error"):
        return "bad_output"
    return None


def diff_state(before: dict, after: dict, client: str, verdicts: dict):
    """Database diff between slot start and end, counting only what `client` changed.

    Returns (end_state, ignored). `ignored` lists ids of new rows another client wrote, which the
    report mentions and the rules skip. A vanished row counts as deleted: the memory table keeps no
    deleter, and the slot runs no cleanup job."""
    new_ids = [i for i in after if i not in before]
    mine = {i for i in new_ids if after[i]["source_client"] == client}
    ignored = [i for i in new_ids if i not in mine]
    written = [{"namespace": after[i]["namespace"], "supersedes": after[i].get("supersedes"),
                "content": after[i]["content"], "occurred_at": after[i].get("occurred_at")}
               for i in new_ids if i in mine]
    superseded = [i for i, r in before.items()
                  if i in after and r.get("superseded_by") is None
                  and after[i].get("superseded_by") in mine]
    deleted = [i for i in before if i not in after]
    return ({"written": written, "superseded": superseded, "deleted": deleted,
             "verdicts": verdicts}, ignored)


def verdicts_from_calls(calls) -> dict:
    out = {}
    for c in calls:
        if c["tool"] == "review_decide" and c.get("ok") and "key" in c["args"]:
            out[c["args"]["key"]] = c["args"].get("verdict")
    return out


def _ids_in_key(key: str):
    return re.findall(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", key) or \
        key.split(":")[1:]


def derive_outcomes(task: dict, calls, received_keys, end_state: dict) -> dict:
    """Per-item outcome: the verdict or write, `unseen`, `resolved_elsewhere`, or `left`."""
    touched = set(end_state["superseded"]) | set(end_state["deleted"])
    decided = {}
    for c in calls:
        if c["tool"] == "review_decide" and c.get("ok") and "key" in c["args"]:
            a = c["args"]
            decided[a["key"]] = a["verdict"] + (f":{a['keep']}" if a.get("keep") else "")
    out = {}
    for it in task["items"]:
        name, kind = it["item"], it["kind"]
        if kind == "verdict":
            if name in decided:
                out[name] = decided[name]
            elif touched & set(_ids_in_key(name)):
                out[name] = "resolved_elsewhere"
            elif name not in received_keys:
                out[name] = "unseen"
            else:
                out[name] = "left"
        elif kind == "supersede":
            tid = it["target_id"]
            out[name] = "superseded" if tid in end_state["superseded"] else (
                "deleted" if tid in end_state["deleted"] else "left")
        elif kind == "forget":
            out[name] = "deleted" if it["target_id"] in end_state["deleted"] else "left"
        elif kind == "write":
            pats = [re.compile(p, re.I) for p in it.get("match", [])]
            hit = [w for w in end_state["written"] if all(p.search(w["content"]) for p in pats)]
            if not hit:
                out[name] = "not_written"
            elif it.get("occurred_at") and not any(
                    (w.get("occurred_at") or "").startswith(it["occurred_at"]) for w in hit):
                out[name] = "written:date_mismatch"
            else:
                out[name] = "written"
        else:
            raise ValueError(f"unknown item kind {kind!r} on {name}")
    return out
