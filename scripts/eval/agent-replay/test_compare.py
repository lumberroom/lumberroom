"""Tests for the G2 pass rules. Fixture records follow the run-record shape in the plan."""
from compare import (
    item_agreement, item_fails, unseen_ok, calls_ok, scored, terse_ok,
    group_agreement_ok, fence_leak, more_path_ok, tool_share_moves, judge_ok,
)


def run(arm, task, outcomes, calls=4, status="complete", channel="structured",
        answer="", written=None, received=None):
    return {"arm": arm, "task": task, "channel": channel, "status": status,
            "outcomes": outcomes, "calls": [{"tool": "t", "args": {}}] * calls,
            "answer": answer, "received_keys": received or [],
            "end_state": {"written": written or [], "superseded": [], "deleted": [],
                          "verdicts": {}}}


def test_item_agreement_averages_per_item_against_the_a0_mode():
    a0 = [run("A0", "t", {"k1": "x", "k2": "y"}) for _ in range(3)]
    a1 = [run("A1", "t", {"k1": "x", "k2": "z"}), run("A1", "t", {"k1": "x", "k2": "y"})]
    assert abs(item_agreement(a0, a1) - 0.75) < 1e-9


def test_an_item_fails_only_on_a_strong_old_mode_and_a_weak_new_one():
    def five(arm, hits):
        return [run(arm, "t", {"k": "x" if i < hits else "y"}) for i in range(5)]
    assert item_fails(five("A0", 4), five("A0b", 5), five("A1", 2), "k")
    assert not item_fails(five("A0", 4), five("A0b", 5), five("A1", 3), "k")
    assert not item_fails(five("A0", 3), five("A0b", 5), five("A1", 0), "k")


def test_a_tied_mode_accepts_any_tied_outcome():
    a0 = [run("A0", "t", {"k": "x"}), run("A0", "t", {"k": "y"})]
    assert item_agreement(a0, [run("A1", "t", {"k": "y"})]) == 1.0


def test_terseness_allows_the_old_spread_plus_ten_percent():
    assert terse_ok(a0_mean=100, a0b_mean=110, a1_mean=120)
    assert not terse_ok(a0_mean=100, a0b_mean=110, a1_mean=122)


def test_unseen_keys_may_not_exceed_the_floor():
    a0b = [run("A0b", "t", {"k1": "unseen", "k2": "x"})]
    assert unseen_ok(a0b, [run("A1", "t", {"k1": "unseen", "k2": "x"})])
    assert not unseen_ok(a0b, [run("A1", "t", {"k1": "unseen", "k2": "unseen"})])


def test_calls_within_one_or_twenty_percent_or_the_a0_spread():
    a0 = [run("A0", "t", {}, calls=5)] * 3
    a0b = [run("A0b", "t", {}, calls=5)] * 3
    assert calls_ok(a0, a0b, [run("A1", "t", {}, calls=6)])
    assert not calls_ok(a0, a0b, [run("A1", "t", {}, calls=8)])


def test_incomplete_runs_never_score():
    assert scored([run("A1", "t", {"k": "x"}, status="incomplete")]) == []


# Beyond the plan's seven: the rules the plan names but gives no fixture for.

def test_a_wide_a0_spread_widens_the_calls_allowance():
    # A0 averages 5 calls and A0b 9, so the noise floor is 4 calls.
    a0 = [run("A0", "t", {}, calls=5)] * 3
    a0b = [run("A0b", "t", {}, calls=9)] * 3
    assert calls_ok(a0, a0b, [run("A1", "t", {}, calls=9)])
    assert not calls_ok(a0, a0b, [run("A1", "t", {}, calls=10)])


def test_group_agreement_passes_within_five_points_of_the_noise_floor():
    a0 = [run("A0", "t", {"k": "x"}) for _ in range(4)]
    a0b = [run("A0b", "t", {"k": "x"}) for _ in range(4)]
    a1_ok = [run("A1", "t", {"k": "x"}) for _ in range(4)]
    a1_bad = [run("A1", "t", {"k": "y"}) for _ in range(4)]
    assert group_agreement_ok(a0, a0b, a1_ok)
    assert not group_agreement_ok(a0, a0b, a1_bad)


def test_a_fence_marker_in_a_written_row_fails_the_gate():
    leaked = run("A1", "t", {}, written=[{"namespace": "global",
                                          "content": "x\n----- data 3f2a -----\ny"}])
    clean = run("A1", "t", {}, written=[{"namespace": "global", "content": "x"}])
    assert fence_leak([leaked])
    assert not fence_leak([clean])


def test_the_more_path_needs_as_many_required_facts_as_the_noise_floor():
    need = ["fact-a", "fact-b"]
    a0b = [run("A0b", "t", {}, answer="fact-a fact-b")]
    assert more_path_ok(need, a0b, [run("A1", "t", {}, answer="fact-a fact-b")])
    assert not more_path_ok(need, a0b, [run("A1", "t", {}, answer="fact-a only")])


def test_a_tool_share_shift_past_the_spread_plus_twenty_points_is_listed():
    def with_tools(arm, tools):
        r = run(arm, "t", {})
        r["calls"] = [{"tool": t, "args": {}} for t in tools]
        return r
    a0 = [with_tools("A0", ["memory_search"] * 4)]
    a0b = [with_tools("A0b", ["memory_search"] * 4)]
    a1 = [with_tools("A1", ["memory_search", "memory_history", "memory_history", "memory_history"])]
    moved = tool_share_moves(a0, a0b, a1)
    assert "memory_history" in moved and "memory_search" in moved


def test_the_judge_rate_passes_within_five_points_of_the_noise_floor():
    assert judge_ok(a0b_rate=0.9, a1_rate=0.85)
    assert not judge_ok(a0b_rate=0.9, a1_rate=0.84)


# The report: records on disk in, markdown and an exit code out.

def write_tree(root, arms_outcomes, tasks):
    import json as _j
    for arm, outcome in arms_outcomes.items():
        for rep in range(1, 6):
            rec = {"stage": "sonnet", "arm": arm, "channel": "structured", "model": "m", "task": "review-1",
                   "rep": rep, "status": "complete", "incomplete_reason": None,
                   "calls": [{"tool": "review_queue", "args": {}}] * 3,
                   "received_keys": ["k"], "end_state": {"written": [], "superseded": [], "deleted": [], "verdicts": {}},
                   "outcomes": {"k": outcome}, "answer": "done",
                   "usage": {"input": 1000, "cache_read": 500, "output": 50}, "transcript": "x"}
            p = root / "runs" / "sonnet" / arm / "structured" / "review-1" / f"{rep}.json"
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(_j.dumps(rec))
    t = root / "tasks.jsonl"
    t.write_text(_j.dumps({"id": "review-1", "group": "review", "decision": True,
                           "channels": ["structured"], "items": [{"item": "k", "kind": "verdict"}]}) + "\n")
    return t


def test_main_exits_zero_when_every_arm_agrees(tmp_path, capsys):
    import compare
    t = write_tree(tmp_path, {"A0": "keep_both", "A0b": "keep_both", "A1": "keep_both"}, None)
    assert compare.main(["--root", str(tmp_path), "--tasks", str(t)]) == 0
    out = capsys.readouterr().out
    assert "sonnet / structured" in out and "Input tokens" in out and "pass" in out


def test_main_exits_one_and_names_the_failed_rule_when_a1_drifts(tmp_path, capsys):
    import compare
    t = write_tree(tmp_path, {"A0": "keep_both", "A0b": "keep_both", "A1": "delete"}, None)
    assert compare.main(["--root", str(tmp_path), "--tasks", str(t)]) == 1
    out = capsys.readouterr().out
    assert "FAIL" in out and "rule 1" in out


def test_a2_scores_against_a1_as_the_base(tmp_path, capsys):
    import compare
    t = write_tree(tmp_path, {"A0b": "keep_both", "A1": "keep_both", "A2": "keep_both"}, None)
    assert compare.main(["--root", str(tmp_path), "--tasks", str(t), "--arm", "A2", "--base", "A1"]) == 0
    assert "A2 against A1" in capsys.readouterr().out


def test_an_incomplete_judge_verdict_is_reported_not_scored(tmp_path, capsys):
    import compare
    import json as _j
    t = write_tree(tmp_path, {"A0": "x", "A0b": "x", "A1": "x"}, None)
    d = tmp_path / "judge" / "sonnet" / "structured" / "A1"
    d.mkdir(parents=True)
    (d / "review-1.json").write_text(_j.dumps({"status": "incomplete"}))
    assert compare.main(["--root", str(tmp_path), "--tasks", str(t)]) == 0
    assert "judge verdict for review-1 (A1) is incomplete" in capsys.readouterr().out
