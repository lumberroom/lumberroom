"""Tests for the agent replay pass rules, over fixture run records."""
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


# The remaining rules.

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


def test_an_item_does_not_fail_alone_when_the_noise_arm_is_weak():
    # A0 holds the outcome in 5 of 5 runs but A0b in only 2 of 5: the old build is not stable on
    # this item, so A1 missing it is noise and the item rule stays quiet.
    def five(arm, hits):
        return [run(arm, "t", {"k": "x" if i < hits else "y"}) for i in range(5)]
    assert not item_fails(five("A0", 5), five("A0b", 2), five("A1", 0), "k")


def test_the_fence_rule_reads_incomplete_runs_too():
    leaked = run("A1", "t", {}, status="incomplete",
                 written=[{"namespace": "global", "content": "----- data 3f2a -----"}])
    assert fence_leak([leaked])


def test_the_fence_rule_reads_registry_writes():
    r = run("A1", "t", {})
    r["end_state"]["registry_written"] = [{"namespace": "global", "kind": "host", "key": "db",
                                           "content": '"x ----- data 3f2a ----- y"'}]
    assert fence_leak([r])


def test_a_task_needs_three_of_five_complete_runs_or_two_of_three():
    from compare import min_complete
    assert min_complete(5) == 3 and min_complete(3) == 2


# The report: records on disk in, markdown and an exit code out. review-1 is a decision task, so
# the sonnet stage plans 5 runs per arm and the opus stage 3, structured channel only.

PLANNED = {"sonnet": 5, "opus": 3}


def write_tree(root, arms, judge=True, base="A0", noise="A0b", test="A1", stages=("sonnet", "opus")):
    """arms maps an arm to (outcome, complete runs out of 5). An arm left out has no records. Each
    stage in `stages` gets records; on opus an arm completes min(n, 3) of its 3 planned runs."""
    import json as _j
    for stage in stages:
        reps = PLANNED[stage]
        for arm, (outcome, n_complete) in arms.items():
            for rep in range(1, reps + 1):
                done = rep <= n_complete
                rec = {"stage": stage, "arm": arm, "channel": "structured", "model": "m", "task": "review-1",
                       "rep": rep, "status": "complete" if done else "incomplete",
                       "incomplete_reason": None if done else "mcp_error",
                       "calls": [{"tool": "review_queue", "args": {}}] * 3,
                       "received_keys": ["k"], "end_state": {"written": [], "superseded": [], "deleted": [], "verdicts": {}},
                       "outcomes": {"k": outcome}, "answer": "done",
                       "usage": {"input": 1000, "cache_read": 500, "output": 50}, "transcript": "x"}
                p = root / "runs" / stage / arm / "structured" / "review-1" / f"{rep}.json"
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(_j.dumps(rec))
        if judge:
            from compare import judge_path
            for arm in (noise, test):
                p = judge_path(root, stage, "structured", base, arm, "review-1")
                p.parent.mkdir(parents=True, exist_ok=True)
                n = min(arms[arm][1], reps) if arm in arms else reps
                p.write_text(_j.dumps({"equivalent": [True] * n, "reasons": ["same"] * n}))
    t = root / "tasks.jsonl"
    t.write_text(_j.dumps({"id": "review-1", "group": "review", "decision": True,
                           "channels": ["structured"], "items": [{"item": "k", "kind": "verdict"}]}) + "\n")
    return t


def main_of(tmp_path, *extra):
    import compare
    return compare.main(["--root", str(tmp_path), "--tasks", str(tmp_path / "tasks.jsonl"), *extra])


AGREE = {"A0": ("keep_both", 5), "A0b": ("keep_both", 5), "A1": ("keep_both", 5)}


def test_main_exits_zero_when_every_arm_agrees(tmp_path, capsys):
    write_tree(tmp_path, AGREE)
    assert main_of(tmp_path) == 0
    out = capsys.readouterr().out
    assert "sonnet / structured" in out and "Input tokens" in out and "pass" in out


def test_main_exits_one_and_names_the_failed_rule_when_a1_drifts(tmp_path, capsys):
    write_tree(tmp_path, {**AGREE, "A1": ("delete", 5)})
    assert main_of(tmp_path) == 1
    out = capsys.readouterr().out
    assert "FAIL" in out and "rule 1" in out


def test_a2_scores_against_a1_with_its_own_noise_arm(tmp_path, capsys):
    write_tree(tmp_path, {"A1": ("keep_both", 5), "A1b": ("keep_both", 5), "A2": ("keep_both", 5)},
               base="A1", noise="A1b", test="A2")
    assert main_of(tmp_path, "--arm", "A2", "--base", "A1", "--noise", "A1b") == 0
    out = capsys.readouterr().out
    assert "A2 against A1" in out and "noise floor A1b" in out


def test_an_incomplete_judge_verdict_fails_rule_4(tmp_path, capsys):
    import json as _j
    write_tree(tmp_path, AGREE)
    from compare import judge_path
    judge_path(tmp_path, "sonnet", "structured", "A0", "A1", "review-1").write_text(
        _j.dumps({"status": "incomplete"}))
    assert main_of(tmp_path) == 1
    assert "rule 4: task review-1 judge verdict for A1 is incomplete" in capsys.readouterr().out


def test_a_missing_judge_file_fails_rule_4(tmp_path, capsys):
    write_tree(tmp_path, AGREE, judge=False)
    assert main_of(tmp_path) == 1
    assert "rule 4: task review-1 has no judge verdict for A1" in capsys.readouterr().out


def test_every_a1_run_incomplete_fails(tmp_path, capsys):
    write_tree(tmp_path, {**AGREE, "A1": ("keep_both", 0)})
    assert main_of(tmp_path) == 1
    out = capsys.readouterr().out
    assert "coverage: task review-1 A1 has 0 of 5 complete runs" in out
    assert "group review has no complete A1 run" in out


def test_no_a1_records_at_all_fails(tmp_path, capsys):
    write_tree(tmp_path, {"A0": ("keep_both", 5), "A0b": ("keep_both", 5)})
    assert main_of(tmp_path) == 1
    assert "coverage: task review-1 A1 has 0 of 5 complete runs" in capsys.readouterr().out


def test_a_missing_noise_arm_fails(tmp_path, capsys):
    write_tree(tmp_path, {"A0": ("keep_both", 5), "A1": ("keep_both", 5)})
    assert main_of(tmp_path) == 1
    assert "coverage: task review-1 A0b has 0 of 5 complete runs" in capsys.readouterr().out


def test_one_complete_run_of_five_fails(tmp_path, capsys):
    write_tree(tmp_path, {**AGREE, "A1": ("keep_both", 1)})
    assert main_of(tmp_path) == 1
    assert "coverage: task review-1 A1 has 1 of 5 complete runs, needs 3" in capsys.readouterr().out


def test_three_complete_runs_of_five_meet_the_minimum(tmp_path):
    write_tree(tmp_path, {**AGREE, "A1": ("keep_both", 3)})
    assert main_of(tmp_path) == 0


def test_a_root_with_no_records_fails(tmp_path, capsys):
    write_tree(tmp_path, {}, judge=False)
    assert main_of(tmp_path) == 1
    assert "no run records" in capsys.readouterr().out


def test_a_planned_channel_with_no_records_fails(tmp_path, capsys):
    import json as _j
    t = write_tree(tmp_path, AGREE)
    t.write_text(_j.dumps({"id": "review-1", "group": "review", "decision": True,
                           "channels": ["structured", "text"],
                           "items": [{"item": "k", "kind": "verdict"}]}) + "\n")
    assert main_of(tmp_path) == 1
    out = capsys.readouterr().out
    assert "sonnet / text" in out
    assert "coverage: task review-1 A1 has 0 of 5 complete runs" in out


def test_a_judge_verdict_needs_one_flag_per_complete_run(tmp_path, capsys):
    import json as _j
    from compare import judge_path, verdict_complete
    assert verdict_complete({"equivalent": [True] * 3}, 3)
    assert not verdict_complete({"equivalent": [True] * 2}, 3)
    assert not verdict_complete({"equivalent": [True] * 4}, 3)
    write_tree(tmp_path, AGREE)
    judge_path(tmp_path, "sonnet", "structured", "A0", "A1", "review-1").write_text(
        _j.dumps({"equivalent": [True] * 4}))
    assert main_of(tmp_path) == 1
    assert "rule 4: task review-1 judge verdict for A1 is incomplete" in capsys.readouterr().out


def test_a_planned_stage_with_no_records_fails_on_coverage(tmp_path, capsys):
    # review-1 is a decision task, so the plan schedules an opus stage. A root holding only the
    # sonnet stage must not pass as if opus had nothing to run.
    write_tree(tmp_path, AGREE, stages=("sonnet",))
    assert main_of(tmp_path) == 1
    out = capsys.readouterr().out
    assert "## opus / structured" in out
    assert "coverage: task review-1 A1 has 0 of 3 complete runs, needs 2" in out


def test_a_stage_the_plan_never_schedules_adds_no_section(tmp_path, capsys):
    # A task with no decision flag runs on sonnet only, so the report carries no opus section.
    import json as _j
    write_tree(tmp_path, AGREE, stages=("sonnet",))
    (tmp_path / "tasks.jsonl").write_text(_j.dumps({
        "id": "review-1", "group": "review", "decision": False, "channels": ["structured"],
        "items": [{"item": "k", "kind": "verdict"}]}) + "\n")
    main_of(tmp_path)
    assert "opus" not in capsys.readouterr().out


def forget_run(arm, deleted, stored="deleted"):
    r = run(arm, "forget-1", {"gone": stored})
    r["end_state"]["deleted"] = list(deleted)
    return r


FORGET_TASKS = {"forget-1": {"id": "forget-1", "group": "forget", "items": [
    {"item": "gone", "kind": "forget", "target_id": "t"}]}}


def test_forget_agreement_scores_the_whole_end_state_and_not_the_stored_outcome():
    # A record written before the forget outcome read the whole end state says `deleted` for a run
    # that also deleted another row. Rescored, it disagrees with a run that deleted the target alone.
    from compare import rescore
    a0 = rescore([forget_run("A0", ["t"]) for _ in range(3)], FORGET_TASKS)
    a1 = rescore([forget_run("A1", ["t", "other"]) for _ in range(3)], FORGET_TASKS)
    assert {r["outcomes"]["gone"] for r in a1} == {"deleted:extra"}
    assert item_agreement(a0, a1) == 0.0
    assert item_agreement(a0, a0) == 1.0


def test_rescore_leaves_other_items_and_unknown_tasks_alone():
    from compare import rescore
    r = run("A0", "review-1", {"k": "keep_both"})
    assert rescore([r], FORGET_TASKS)[0]["outcomes"] == {"k": "keep_both"}


def test_the_report_scores_forget_items_on_the_rescored_outcome():
    from compare import report
    tasks = {"forget-1": {**FORGET_TASKS["forget-1"], "decision": True, "channels": ["structured"]}}
    recs = []
    for arm, deleted in (("A0", ["t"]), ("A0b", ["t"]), ("A1", ["t", "other"])):
        for rep in range(1, 6):
            r = forget_run(arm, deleted)
            recs.append({**r, "stage": "sonnet", "rep": rep, "usage": {}})
    text, failed = report(recs, tasks, {})
    assert failed and "rule 1: item forget-1/gone fails alone" in text


def test_stubbed_sessions_count_per_arm_from_the_flag_or_the_transcript(tmp_path):
    import json as _j
    from compare import stub_counts
    saved = "~/.claude/projects/-tmp-lr-gate-abc/s/tool-results/r.txt"
    t = tmp_path / "transcripts" / "a.jsonl"
    t.parent.mkdir(parents=True)
    t.write_text(_j.dumps({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": "2", "name": "Read", "input": {"file_path": saved}}]}}) + "\n")
    recs = [
        {**run("A0", "t", {}), "stage": "sonnet", "file_stubbed": True},
        {**run("A0", "t", {}, status="incomplete"), "stage": "sonnet", "file_stubbed": True},
        {**run("A0", "t", {}), "stage": "sonnet", "file_stubbed": False},
        # An older record with no flag: the transcript decides.
        {**run("A1", "t", {}), "stage": "sonnet", "transcript": "transcripts/a.jsonl"},
        # No flag and no transcript: it counts as unknown and never as clean.
        {**run("A1", "t", {}), "stage": "sonnet", "transcript": "transcripts/missing.jsonl"},
    ]
    assert stub_counts(recs, tmp_path, "sonnet", "structured") == {
        "A0": {"stubbed": 2, "runs": 3, "unknown": 0}, "A1": {"stubbed": 1, "runs": 2, "unknown": 1}}


def test_the_report_prints_the_stub_count_and_fails_nothing_on_it(tmp_path, capsys):
    import json as _j
    write_tree(tmp_path, AGREE)
    p = tmp_path / "runs" / "sonnet" / "A0" / "structured" / "review-1" / "1.json"
    rec = _j.loads(p.read_text())
    p.write_text(_j.dumps({**rec, "file_stubbed": True}))
    assert main_of(tmp_path) == 0
    out = capsys.readouterr().out
    assert "File-stubbed sessions" in out and "A0 1 of 5" in out


def test_a_task_file_with_unfilled_ids_is_refused_by_name(tmp_path):
    import json as _j
    import pytest
    from compare import load_tasks
    f = tmp_path / "tasks.example.jsonl"
    f.write_text(_j.dumps({"id": "forget-1", "group": "forget", "items": [
        {"item": "gone", "kind": "forget", "target_id": "@id:project-x-0016"}]}) + "\n")
    with pytest.raises(ValueError, match="tasks.example.jsonl"):
        load_tasks(f)


def test_rescore_refuses_an_unfilled_target_id():
    import pytest
    from compare import rescore
    tasks = {"forget-1": {"id": "forget-1", "items": [{"item": "gone", "kind": "forget", "target_id": "@id:k"}]}}
    with pytest.raises(ValueError, match="@id:"):
        rescore([forget_run("A0", ["t"])], tasks)


def test_main_exits_two_on_an_unfilled_task_file(tmp_path, capsys):
    import json as _j
    import compare
    write_tree(tmp_path, AGREE)
    (tmp_path / "tasks.jsonl").write_text(_j.dumps({"id": "forget-1", "group": "forget", "decision": True,
        "channels": ["structured"], "items": [{"item": "gone", "kind": "forget", "target_id": "@id:k"}]}) + "\n")
    assert compare.main(["--root", str(tmp_path), "--tasks", str(tmp_path / "tasks.jsonl")]) == 2
    assert "fill_ids.py" in capsys.readouterr().err
