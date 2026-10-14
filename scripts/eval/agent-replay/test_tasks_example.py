"""The shipped example task file reproduces the stage session counts."""
import json
from pathlib import Path

from batch import expand, ARMS
from outcomes import KINDS

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
    assert kinds <= KINDS


def test_every_review_task_runs_on_a_template_of_its_own():
    review = [t for t in TASKS if t["group"] == "review"]
    assert review and all(t.get("template") == t["id"] for t in review)
    assert all("template" not in t for t in TASKS if t["group"] != "review")


def test_the_forget_task_asks_for_the_delete_with_the_go_ahead_given():
    # A headless session never confirms, so a prompt that asks to confirm first measures a dry run.
    forget = [t for t in TASKS if t["group"] == "forget"]
    assert forget and all(
        t["prompt"] == "Delete the memory about the dead letter queue replay CLI stub. You have my "
                       "go-ahead: delete it now, without asking me to confirm." for t in forget)
