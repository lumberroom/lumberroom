"""Batch planning: expansion to the spec's session counts, resume, interleave, rerun cap."""
import json

from batch import expand, interleave, pending, MAX_ATTEMPTS

TASKS = [
    {"id": f"d{i}", "group": "review", "decision": True, "channels": ["structured", "text"]}
    for i in range(2)
] + [
    {"id": "o0", "group": "registry", "decision": False, "channels": ["structured"]},
    {"id": "o1", "group": "recall", "decision": False, "channels": ["structured", "text"]},
]


def test_sonnet_expansion_runs_five_repeats_on_decision_tasks_and_three_on_the_rest():
    jobs = expand(TASKS, stage="sonnet", arms=["A0", "A0b", "A1"], model="m")
    structured = [j for j in jobs if j["channel"] == "structured"]
    # 2 decision x 5 + 2 other x 3 = 16 per arm
    assert len(structured) == 16 * 3
    text = [j for j in jobs if j["channel"] == "text"]
    # 2 decision x 5 + 1 other x 3 = 13 per arm
    assert len(text) == 13 * 3


def test_opus_expansion_runs_decision_tasks_on_the_structured_channel_three_times():
    jobs = expand(TASKS, stage="opus", arms=["A0", "A0b", "A1"], model="m")
    assert len(jobs) == 2 * 3 * 3
    assert {j["channel"] for j in jobs} == {"structured"}
    assert {j["task"] for j in jobs} == {"d0", "d1"}


def test_interleave_alternates_arms_over_the_same_task_and_repeat():
    jobs = expand(TASKS[:1], stage="sonnet", arms=["A0", "A0b", "A1"], model="m")
    order = [(j["rep"], j["arm"]) for j in interleave(jobs) if j["channel"] == "structured"]
    assert order[:6] == [(1, "A0"), (1, "A0b"), (1, "A1"), (2, "A0"), (2, "A0b"), (2, "A1")]


def rec(job, status, attempts=1):
    return {**{k: job[k] for k in ("stage", "arm", "channel", "task", "rep")},
            "status": status, "attempts": attempts}


def test_a_complete_record_is_skipped_and_an_incomplete_one_is_rerun():
    jobs = expand(TASKS[:1], stage="sonnet", arms=["A0"], model="m")
    existing = {("sonnet", "A0", "structured", "d0", 1): rec(jobs[0], "complete"),
                ("sonnet", "A0", "structured", "d0", 2): rec(jobs[1], "incomplete")}
    todo, given_up = pending(jobs, existing)
    reps = {(j["channel"], j["rep"]) for j in todo}
    assert ("structured", 1) not in reps and ("structured", 2) in reps
    assert given_up == []


def test_a_run_incomplete_after_three_reruns_is_reported_by_name_and_not_queued():
    jobs = expand(TASKS[:1], stage="sonnet", arms=["A0"], model="m")
    existing = {("sonnet", "A0", "structured", "d0", 1): rec(jobs[0], "incomplete", attempts=MAX_ATTEMPTS)}
    todo, given_up = pending(jobs, existing)
    assert ("structured", 1) not in {(j["channel"], j["rep"]) for j in todo}
    assert given_up == [("sonnet", "A0", "structured", "d0", 1)]
