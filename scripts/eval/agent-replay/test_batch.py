"""Batch planning: expansion to the stage session counts, resume, interleave, rerun cap."""
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


def test_a_usage_limit_does_not_spend_an_attempt():
    from batch import next_attempts
    assert next_attempts(0, "usage_limit") == 0
    assert next_attempts(2, "usage_limit") == 2
    assert next_attempts(2, "mcp_error") == 3
    assert next_attempts(0, None) == 1


def test_a_noise_arm_shares_the_slot_of_its_build():
    from batch import slot_name
    assert slot_name({"arm": "A0b", "channel": "text"}) == "A0-text"
    assert slot_name({"arm": "A1b", "channel": "structured"}) == "A1-structured"
    assert slot_name({"arm": "A2", "channel": "structured"}) == "A2-structured"


JOB = {"stage": "sonnet", "arm": "A1", "channel": "structured", "task": "review-1", "rep": 2, "model": "m"}
TASK = {"id": "review-1", "group": "review", "template": "review-1", "items": []}


def test_a_failed_slot_reset_writes_an_incomplete_record_instead_of_raising(tmp_path):
    import subprocess
    from batch import run_job

    def reset(slot_dir, template):
        raise subprocess.CalledProcessError(1, ["slot.sh", "reset"])

    def never(**kw):
        raise AssertionError("no session may run on a slot that did not reset")

    rec = run_job(JOB, root=tmp_path, task=TASK, prior=1, reset=reset, run_session=never)
    assert rec["status"] == "incomplete" and rec["incomplete_reason"] == "slot_reset"
    assert rec["attempts"] == 2
    on_disk = json.loads((tmp_path / "runs/sonnet/A1/structured/review-1/2.json").read_text())
    assert on_disk["incomplete_reason"] == "slot_reset" and on_disk["outcomes"] == {}


def test_a_session_runs_on_the_task_template_and_its_record_carries_attempts(tmp_path):
    from batch import run_job
    seen = {}

    def reset(slot_dir, template):
        seen["slot"], seen["template"] = slot_dir.name, template
        return {"name": "A1-structured"}

    def session(**kw):
        return {**{k: kw[k] for k in ("stage", "arm", "channel", "rep")}, "task": kw["task"]["id"],
                "status": "incomplete", "incomplete_reason": "usage_limit"}

    rec = run_job(JOB, root=tmp_path, task=TASK, prior=1, reset=reset, run_session=session)
    assert seen == {"slot": "A1-structured", "template": "review-1"}
    assert rec["attempts"] == 1
    on_disk = json.loads((tmp_path / "runs/sonnet/A1/structured/review-1/2.json").read_text())
    assert on_disk["attempts"] == 1
