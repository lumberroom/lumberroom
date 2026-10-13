"""Stream parsing, the database diff and the per-item outcomes."""
import json

from outcomes import parse_stream, diff_state, derive_outcomes, incomplete_reason

A, B, C = ("00000000-0000-4000-8000-00000000000%d" % i for i in (1, 2, 3))


def lines(*events):
    return [json.dumps(e) for e in events]


INIT = {"type": "system", "subtype": "init", "apiKeySource": "none"}


def call(i, name, args):
    return {"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": i, "name": f"mcp__lumberroom__{name}", "input": args}]}}


def result(i, text, error=False):
    return {"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": i, "is_error": error,
         "content": [{"type": "text", "text": text}]}]}}


def final(text="done", **over):
    ev = {"type": "result", "subtype": "success", "is_error": False, "result": text,
          "usage": {"input_tokens": 10, "cache_creation_input_tokens": 5,
                    "cache_read_input_tokens": 100, "output_tokens": 7}}
    ev.update(over)
    return ev


def test_calls_come_back_without_the_server_prefix_and_in_order():
    p = parse_stream(lines(INIT, call("1", "review_queue", {}), result("1", "ok"),
                           call("2", "memory_history", {"id": A}), result("2", "ok"), final()))
    assert [c["tool"] for c in p["calls"]] == ["review_queue", "memory_history"]
    assert p["calls"][1]["args"] == {"id": A}


def test_received_keys_come_from_every_review_queue_result_in_first_seen_order():
    q1 = f'items: "key":"conflict:{A}:{B}" and "key":"stale:{C}"'
    q2 = f'again "key":"stale:{C}"'
    p = parse_stream(lines(INIT, call("1", "review_queue", {}), result("1", q1),
                           call("2", "review_queue", {"offset": 2}), result("2", q2), final()))
    assert p["received_keys"] == [f"conflict:{A}:{B}", f"stale:{C}"]


def test_keys_in_other_tool_results_do_not_count_as_received():
    p = parse_stream(lines(INIT, call("1", "memory_search", {"query": "x"}),
                           result("1", f"stale:{A}"), final()))
    assert p["received_keys"] == []


def test_usage_maps_cache_creation_into_input():
    p = parse_stream(lines(INIT, final()))
    assert p["usage"] == {"input": 15, "cache_read": 100, "output": 7}
    assert p["answer"] == "done"


def test_a_failed_review_decide_is_marked_not_ok():
    p = parse_stream(lines(INIT, call("1", "review_decide", {"key": "k", "verdict": "keep_both"}),
                           result("1", "proposal_moved", error=True), final()))
    assert p["calls"][0]["ok"] is False


def test_a_stream_without_a_result_event_is_bad_output():
    p = parse_stream(lines(INIT, call("1", "review_queue", {})))
    assert incomplete_reason(p) == "bad_output"


def test_a_line_that_is_not_json_is_bad_output():
    p = parse_stream(["not json"])
    assert incomplete_reason(p) == "bad_output"


def test_a_usage_limit_message_is_a_usage_limit():
    p = parse_stream(lines(INIT, final("You've hit your usage limit. Resets at 5pm.", is_error=True)))
    assert incomplete_reason(p) == "usage_limit"


def test_a_refusal_stop_reason_is_a_refusal():
    refusal = {"type": "assistant", "message": {"stop_reason": "refusal", "content": []}}
    p = parse_stream(lines(INIT, refusal, final()))
    assert incomplete_reason(p) == "refusal"


def test_a_connection_error_in_a_tool_result_is_an_mcp_error():
    p = parse_stream(lines(INIT, call("1", "memory_search", {}),
                           result("1", "MCP error: connection refused", error=True), final()))
    assert incomplete_reason(p) == "mcp_error"


def test_a_server_refusal_such_as_proposal_moved_is_not_an_mcp_error():
    p = parse_stream(lines(INIT, call("1", "review_decide", {}),
                           result("1", "proposal_moved", error=True), final()))
    assert incomplete_reason(p) is None


def row(i, **kw):
    r = {"id": i, "namespace": "project:gate-review", "source_client": "seed", "supersedes": None,
         "superseded_by": None, "occurred_at": None, "content": "c"}
    r.update(kw)
    return r


def test_diff_state_counts_only_rows_the_gate_client_wrote():
    before = {A: row(A), B: row(B)}
    after = {A: row(A, superseded_by="N"), B: row(B),
             "N": row("N", source_client="gate", supersedes=A, content="merged"),
             "Z": row("Z", source_client="sweeper")}
    st, ignored = diff_state(before, after, "gate", verdicts={})
    assert st["written"] == [{"namespace": "project:gate-review", "supersedes": A,
                              "content": "merged", "occurred_at": None}]
    assert st["superseded"] == [A] and st["deleted"] == []
    assert ignored == ["Z"]


def test_diff_state_ignores_a_supersession_by_another_client():
    before = {A: row(A)}
    after = {A: row(A, superseded_by="Z"), "Z": row("Z", source_client="sweeper")}
    st, _ = diff_state(before, after, "gate", verdicts={})
    assert st["superseded"] == []


def test_diff_state_reports_a_vanished_row_as_deleted():
    st, _ = diff_state({A: row(A), B: row(B)}, {B: row(B)}, "gate", verdicts={})
    assert st["deleted"] == [A]


def verdict_task(*keys):
    return {"id": "review-1", "items": [{"item": k, "kind": "verdict"} for k in keys]}


def ok_call(tool, **args):
    return {"tool": tool, "args": args, "ok": True}


def test_a_decided_key_maps_to_its_verdict_and_keep_target():
    key = f"conflict:{A}:{B}"
    out = derive_outcomes(verdict_task(key), [ok_call("review_decide", key=key, verdict="supersede", keep=B)],
                          [key], {"written": [], "superseded": [A], "deleted": [], "verdicts": {}})
    assert out == {key: f"supersede:{B}"}


def test_an_undecided_key_whose_rows_another_verdict_retired_is_resolved_elsewhere():
    key = f"stale:{C}"
    out = derive_outcomes(verdict_task(key), [], [key],
                          {"written": [], "superseded": [C], "deleted": [], "verdicts": {}})
    assert out == {key: "resolved_elsewhere"}


def test_a_key_the_agent_never_received_and_nobody_touched_is_unseen():
    key = f"stale:{C}"
    out = derive_outcomes(verdict_task(key), [], [],
                          {"written": [], "superseded": [], "deleted": [], "verdicts": {}})
    assert out == {key: "unseen"}


def test_a_received_key_left_alone_is_left():
    key = f"stale:{C}"
    out = derive_outcomes(verdict_task(key), [], [key],
                          {"written": [], "superseded": [], "deleted": [], "verdicts": {}})
    assert out == {key: "left"}


def test_a_failed_verdict_call_is_not_a_decision():
    key = f"stale:{C}"
    bad = {"tool": "review_decide", "args": {"key": key, "verdict": "delete"}, "ok": False}
    out = derive_outcomes(verdict_task(key), [bad], [key],
                          {"written": [], "superseded": [], "deleted": [], "verdicts": {}})
    assert out == {key: "left"}


def test_write_items_match_every_pattern_and_the_stated_date():
    task = {"id": "w", "items": [{"item": "pg17", "kind": "write", "match": ["postgres 17", "3 october"],
                                  "occurred_at": "2026-10-03"}]}
    good = {"written": [{"namespace": "global", "supersedes": None,
                         "content": "Switched to Postgres 17 on 3 October 2026.",
                         "occurred_at": "2026-10-03T00:00:00+00:00"}],
            "superseded": [], "deleted": [], "verdicts": {}}
    assert derive_outcomes(task, [], [], good) == {"pg17": "written"}
    wrong = json.loads(json.dumps(good))
    wrong["written"][0]["occurred_at"] = "2026-10-12T00:00:00+00:00"
    assert derive_outcomes(task, [], [], wrong) == {"pg17": "written:date_mismatch"}
    none = {"written": [], "superseded": [], "deleted": [], "verdicts": {}}
    assert derive_outcomes(task, [], [], none) == {"pg17": "not_written"}


def test_supersede_and_forget_items_read_the_end_state():
    task = {"id": "s", "items": [{"item": "old", "kind": "supersede", "target_id": A},
                                 {"item": "gone", "kind": "forget", "target_id": B}]}
    st = {"written": [], "superseded": [A], "deleted": [], "verdicts": {}}
    assert derive_outcomes(task, [], [], st) == {"old": "superseded", "gone": "left"}
    st2 = {"written": [], "superseded": [], "deleted": [B], "verdicts": {}}
    assert derive_outcomes(task, [], [], st2) == {"old": "left", "gone": "deleted"}


FORGET = {"id": "forget-1", "items": [{"item": "gone", "kind": "forget", "target_id": B}]}


def end(deleted=(), superseded=()):
    return {"written": [], "superseded": list(superseded), "deleted": list(deleted), "verdicts": {}}


def test_a_forget_that_deletes_the_target_alone_reads_deleted():
    assert derive_outcomes(FORGET, [], [], end(deleted=[B])) == {"gone": "deleted"}


def test_a_forget_that_also_deletes_or_supersedes_another_row_reads_deleted_extra():
    assert derive_outcomes(FORGET, [], [], end(deleted=[B, C])) == {"gone": "deleted:extra"}
    assert derive_outcomes(FORGET, [], [], end(deleted=[B], superseded=[A])) == {"gone": "deleted:extra"}


def test_a_forget_that_deletes_another_row_and_spares_the_target_reads_wrong_row():
    assert derive_outcomes(FORGET, [], [], end(deleted=[C])) == {"gone": "wrong_row"}


def test_a_forget_that_deletes_nothing_reads_left():
    assert derive_outcomes(FORGET, [], [], end()) == {"gone": "left"}


# The text channel: a structuredContent that reaches the session means the strip proxy failed.

def structured_result(i, where):
    block = {"type": "tool_result", "tool_use_id": i, "is_error": False,
             "content": [{"type": "text", "text": "ok"}]}
    ev = {"type": "user", "message": {"content": [block]}}
    if where == "block":
        block["structuredContent"] = {"hits": []}
    else:
        ev["tool_use_result"] = {"content": [], "structuredContent": {"hits": []}}
    return ev


def test_a_tool_result_carrying_structured_content_is_counted():
    from outcomes import channel_reason
    for where in ("block", "event"):
        p = parse_stream(lines(INIT, call("1", "memory_search", {}), structured_result("1", where),
                               final()))
        assert p["structured_results"] == 1
        assert channel_reason(p, "text") == "structured_on_text"
        assert channel_reason(p, "structured") is None


def test_a_text_only_tool_result_passes_the_text_channel():
    from outcomes import channel_reason
    p = parse_stream(lines(INIT, call("1", "memory_search", {}), result("1", "ok"), final()))
    assert p["structured_results"] == 0 and channel_reason(p, "text") is None


def test_row_text_that_mentions_structured_content_is_not_counted():
    p = parse_stream(lines(INIT, call("1", "memory_search", {}),
                           result("1", '{"note": "structuredContent is stripped"}'), final()))
    assert p["structured_results"] == 0


# The registry half of the end state, so a fenced registry_set reaches rule 6.

def reg(i, value, version=1, **kw):
    r = {"id": i, "namespace": "global", "kind": "host", "key": "db", "value": value,
         "version": version}
    r.update(kw)
    return r


def test_registry_changes_list_new_and_rewritten_entries_only():
    from outcomes import registry_changes
    before = {A: reg(A, '"old"'), B: reg(B, '"same"', key="cache")}
    after = {A: reg(A, '"----- data 1 -----"', version=2), B: reg(B, '"same"', key="cache"),
             C: reg(C, '"new"', key="queue")}
    got = registry_changes(before, after)
    assert [(g["key"], g["content"]) for g in got] == [("db", '"----- data 1 -----"'), ("queue", '"new"')]


def test_the_snapshot_query_reads_the_registry_table_too():
    from session import SNAPSHOT_SQL
    assert "FROM memory" in SNAPSHOT_SQL and "FROM registry" in SNAPSHOT_SQL


def test_the_structured_channel_shape_from_claude_code_2_1_296_parses():
    # Captured on the gate host on 12 October 2026. On the structured channel the tool_result
    # content is one string, the structuredContent as JSON; the text channel sends a list of text
    # blocks. The stream also carries system/thinking_tokens and rate_limit_event, which name no tool.
    body = json.dumps({"items": [{"key": f"stale:{C}", "namespace": "project:gate-review"}], "has_more": True})
    p = parse_stream(lines(
        INIT, {"type": "system", "subtype": "thinking_tokens"}, call("1", "review_queue", {"limit": 1}),
        {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "1", "content": body}]},
         "tool_use_result": {"content": [], "structuredContent": {}}},
        {"type": "rate_limit_event"}, final("NO TEXT")))
    assert p["received_keys"] == [f"stale:{C}"]
    assert [c["tool"] for c in p["calls"]] == ["review_queue"] and p["calls"][0]["ok"]
    assert incomplete_reason(p) is None and p["answer"] == "NO TEXT"


def test_the_text_channel_shape_parses_from_a_list_of_text_blocks():
    body = json.dumps({"items": [{"key": f"stale:{C}"}]})
    p = parse_stream(lines(INIT, call("1", "review_queue", {}), result("1", body), final()))
    assert p["received_keys"] == [f"stale:{C}"] and p["structured_results"] == 0


# A tool answer Claude Code saved to a file. The model gets a stub with a path under tool-results/,
# then tries the built-in Read on it, which check_event rejects.
STUB_PATH = "~/.claude/projects/-tmp-lr-gate-abc/s1/tool-results/mcp-lumberroom-memory_search-1.txt"


def read_call(i, path):
    return {"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": i, "name": "Read", "input": {"file_path": path}}]}}


def test_a_lumberroom_answer_saved_to_a_file_marks_the_session_file_stubbed():
    stub = f"Output too large (61.2KB). Full output saved to: {STUB_PATH}\n\nPreview (first 2KB): ..."
    p = parse_stream(lines(INIT, call("1", "memory_search", {"query": "q", "limit": 50}),
                           result("1", stub), final()))
    assert p["file_stubbed"]


def test_a_read_of_a_saved_answer_marks_the_session_file_stubbed():
    p = parse_stream(lines(INIT, call("1", "memory_search", {"query": "q"}), result("1", "ok"),
                           read_call("2", STUB_PATH), final()))
    assert p["file_stubbed"]


def test_an_inline_answer_is_not_file_stubbed():
    body = json.dumps({"hits": [{"id": A, "content": "the tool-results table lists outputs"}]})
    p = parse_stream(lines(INIT, call("1", "memory_search", {"query": "q"}), result("1", body), final()))
    assert not p["file_stubbed"]


def rate_event(status="allowed", five_hour=0.25):
    # The shape Claude Code 2.1.x prints, with synthetic values.
    return {"type": "rate_limit_event", "rate_limit_info": {
        "status": status, "resetsAt": 1700000000, "rateLimitType": "five_hour",
        "isUsingOverage": False,
        "unifiedWindows": {"five_hour": {"utilization": five_hour, "resetsAt": 1700000000},
                           "seven_day": {"utilization": 0.1, "resetsAt": 1700500000}}},
        "uuid": "u", "session_id": "s"}


def test_the_last_rate_limit_event_lands_in_the_parse():
    p = parse_stream(lines(INIT, rate_event(five_hour=0.2), call("1", "memory_search", {}),
                           result("1", "ok"), rate_event("allowed_warning", 0.7), final()))
    assert p["rate_limit"]["status"] == "allowed_warning"
    assert p["rate_limit"]["unifiedWindows"]["five_hour"]["utilization"] == 0.7


def test_a_stream_with_no_rate_limit_event_parses_to_none():
    assert parse_stream(lines(INIT, final()))["rate_limit"] is None


def test_a_forget_that_supersedes_the_target_and_deletes_nothing_reads_superseded():
    assert derive_outcomes(FORGET, [], [], end(superseded=[B])) == {"gone": "superseded"}
    assert derive_outcomes(FORGET, [], [], end(superseded=[A])) == {"gone": "left"}
