"""The shipped example task file reproduces the spec's session counts."""
import json
from pathlib import Path

from batch import expand, ARMS

TASKS = [json.loads(l) for l in (Path(__file__).parent / "tasks.example.jsonl").read_text().splitlines() if l.strip()]


def test_there_are_22_tasks_and_11_decision_tasks():
    assert len(TASKS) == 22 and sum(t["decision"] for t in TASKS) == 11


def test_sonnet_stage_runs_264_structured_and_162_text_sessions():
    jobs = expand(TASKS, stage="sonnet", arms=ARMS, model="m")
    assert sum(j["channel"] == "structured" for j in jobs) == 264
    assert sum(j["channel"] == "text" for j in jobs) == 162


def test_opus_stage_runs_99_sessions():
    assert len(expand(TASKS, stage="opus", arms=ARMS, model="m")) == 99


def test_every_review_task_owns_between_15_and_20_keys():
    for t in TASKS:
        if t["group"] == "review":
            assert 15 <= len(t["seeded_keys"]) <= 20


def test_item_kinds_are_the_ones_the_runner_understands():
    kinds = {i["kind"] for t in TASKS for i in t["items"]}
    assert kinds <= {"verdict", "supersede", "forget", "write"}
