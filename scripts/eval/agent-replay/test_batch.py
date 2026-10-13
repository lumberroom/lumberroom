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


def info(status="allowed", five_hour=0.2):
    return {"status": status, "rateLimitType": "five_hour",
            "unifiedWindows": {"five_hour": {"utilization": five_hour}}}


def test_a_rate_limit_other_than_allowed_stops_the_batch():
    from batch import rate_limit_stop
    assert rate_limit_stop(info("allowed_warning"), 0.6)
    assert rate_limit_stop(info("rejected"), 0.6)
    assert not rate_limit_stop(info("allowed"), 0.6)


def test_five_hour_utilization_past_the_threshold_stops_the_batch():
    from batch import rate_limit_stop
    assert rate_limit_stop(info(five_hour=0.61), 0.6)
    assert not rate_limit_stop(info(five_hour=0.6), 0.6)
    assert not rate_limit_stop(info(five_hour=0.61), 0.8)


def test_a_flat_utilization_on_a_five_hour_event_counts_too():
    from batch import rate_limit_stop
    assert rate_limit_stop({"status": "allowed", "rateLimitType": "five_hour", "utilization": 0.9}, 0.6)
    assert not rate_limit_stop({"status": "allowed", "rateLimitType": "seven_day", "utilization": 0.9}, 0.6)


def test_no_rate_limit_event_never_stops_the_batch():
    from batch import rate_limit_stop
    assert not rate_limit_stop(None, 0.6)
    assert not rate_limit_stop({}, 0.6)


def test_the_disk_stop_fires_below_the_free_space_floor():
    from batch import disk_stop
    assert disk_stop(free_bytes=4 * 2**30, min_free_gb=5)
    assert not disk_stop(free_bytes=6 * 2**30, min_free_gb=5)


def test_the_utilization_threshold_and_disk_floor_are_flags_with_defaults():
    from batch import build_parser
    a = build_parser().parse_args(["run", "--batch", "b", "--tasks", "t"])
    assert a.max_five_hour == 0.6 and a.min_free_gb == 5.0 and a.keep_order is False
    a = build_parser().parse_args(["run", "--batch", "b", "--tasks", "t", "--max-five-hour", "0.4",
                                   "--min-free-gb", "20", "--keep-order"])
    assert a.max_five_hour == 0.4 and a.min_free_gb == 20.0 and a.keep_order is True


def test_an_arm_with_its_own_build_gets_its_own_slot_by_flag():
    from batch import slot_name, slot_map
    m = slot_map(["A1b=A1b"])
    assert slot_name({"arm": "A1b", "channel": "text"}, m) == "A1b-text"
    assert slot_name({"arm": "A0b", "channel": "text"}, m) == "A0-text"
    assert slot_name({"arm": "A2", "channel": "structured"}, slot_map(["A2=A2-rebuilt"])) == "A2-rebuilt-structured"


def test_a_malformed_slot_mapping_is_refused():
    import pytest
    from batch import slot_map
    with pytest.raises(ValueError):
        slot_map(["A1b"])


def test_keep_order_runs_jobs_in_the_listed_order():
    from batch import order_jobs
    jobs = expand(TASKS[:1], stage="sonnet", arms=["A1", "A0"], model="m")
    assert order_jobs(jobs, keep_order=True) == jobs
    assert order_jobs(jobs, keep_order=False) == interleave(jobs)
    assert order_jobs(jobs, keep_order=False) != jobs


# ---- the run loop, with the session, the disk and the slot check injected --------------------

def batch_files(tmp_path, n=4):
    task = {"id": "o0", "group": "registry", "decision": False, "channels": ["structured"],
            "prompt": "p", "items": []}
    (tmp_path / "tasks.jsonl").write_text(json.dumps(task) + "\n")
    jobs = [{"stage": "sonnet", "arm": "A0", "channel": "structured", "task": "o0", "rep": r,
             "model": "m"} for r in range(1, n + 1)]
    (tmp_path / "b.jsonl").write_text("".join(json.dumps(j) + "\n" for j in jobs))
    return ["run", "--batch", str(tmp_path / "b.jsonl"), "--tasks", str(tmp_path / "tasks.jsonl"),
            "--root", str(tmp_path), "--concurrency", "1"]


def fake_job(infos, seen):
    def run_job(job, *, root, task, prior, slot_of=None):
        seen.append(job["rep"])
        return {"status": "complete", "incomplete_reason": None,
                "rate_limit": infos[len(seen) - 1] if len(seen) <= len(infos) else None}
    return run_job


def test_the_run_stops_starting_sessions_after_a_rate_limit_warning(tmp_path):
    from batch import build_parser, run
    seen = []
    a = build_parser().parse_args(batch_files(tmp_path))
    rc = run(a, run_job=fake_job([info(), info("allowed_warning")], seen),
             disk_free=lambda p: 10**12, check_slots=lambda names, root: [])
    assert seen == [1, 2] and rc == 3


def test_the_run_stops_past_the_five_hour_threshold(tmp_path):
    from batch import build_parser, run
    seen = []
    a = build_parser().parse_args(batch_files(tmp_path) + ["--max-five-hour", "0.5"])
    rc = run(a, run_job=fake_job([info(five_hour=0.55)], seen),
             disk_free=lambda p: 10**12, check_slots=lambda names, root: [])
    assert seen == [1] and rc == 3


def test_the_run_stops_when_free_disk_falls_below_the_floor(tmp_path):
    from batch import build_parser, run
    seen, free = [], iter([10**12, 10**12, 10**6, 10**6])
    a = build_parser().parse_args(batch_files(tmp_path))
    rc = run(a, run_job=fake_job([], seen), disk_free=lambda p: next(free),
             check_slots=lambda names, root: [])
    assert seen == [1, 2] and rc == 4


def test_a_failed_slot_check_starts_no_session(tmp_path):
    from batch import build_parser, run
    seen = []
    a = build_parser().parse_args(batch_files(tmp_path))
    rc = run(a, run_job=fake_job([], seen), disk_free=lambda p: 10**12,
             check_slots=lambda names, root: ["A0-structured: memory_search lacks the declaration"])
    assert seen == [] and rc == 5


def test_the_slot_check_reads_every_slot_the_queue_uses(tmp_path):
    from batch import build_parser, run
    asked = []
    a = build_parser().parse_args(batch_files(tmp_path, n=1))
    run(a, run_job=fake_job([], []), disk_free=lambda p: 10**12,
        check_slots=lambda names, root: asked.extend(names) or [])
    assert asked == ["A0-structured"]


def test_a_missing_slot_directory_fails_the_preflight(tmp_path):
    from batch import preflight
    problems = preflight(["A1b-text"], tmp_path, fetch=lambda slot_dir: [])
    assert problems and "A1b-text" in problems[0] and "no slot" in problems[0]


def test_a_slot_whose_memory_search_lacks_the_declaration_fails_the_preflight(tmp_path):
    from batch import preflight
    (tmp_path / "slots" / "A0-text").mkdir(parents=True)
    bare = [{"name": "memory_search"}]
    declared = [{"name": "memory_search", "_meta": {"anthropic/maxResultSizeChars": 500000}}]
    assert "maxResultSizeChars" in preflight(["A0-text"], tmp_path, fetch=lambda d: bare)[0]
    assert preflight(["A0-text"], tmp_path, fetch=lambda d: declared) == []
