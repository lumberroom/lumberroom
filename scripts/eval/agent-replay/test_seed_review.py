import gzip
import json

from seed_review import build_archive, foreign_items, GATE_NS

FIXTURE = {
    "namespace": GATE_NS, "source_client": "gate-seed",
    "memories": [
        {"id": "00000000-0000-4000-8000-000000000001", "content": "older", "tags": ["a"],
         "created_at": "2026-03-01T00:00:00Z"},
        {"id": "00000000-0000-4000-8000-000000000002", "content": "newer", "tags": [],
         "created_at": "2026-03-02T00:00:00Z", "supersedes": None},
    ],
}


def lines_of(blob):
    return [json.loads(l) for l in gzip.decompress(blob).decode().splitlines()]


def test_the_archive_is_gzipped_ndjson_with_a_header_whose_counts_match_the_body():
    recs = lines_of(build_archive(FIXTURE))
    assert recs[0]["section"] == "header" and recs[0]["format"] == "lumberroom.archive"
    assert recs[0]["version"] == 1 and recs[0]["counts"]["memory"] == 2
    assert [r["section"] for r in recs[1:]] == ["memory", "memory"]


def test_every_memory_record_carries_the_fields_the_reader_requires():
    rec = lines_of(build_archive(FIXTURE))[1]
    for f in ("id", "namespace", "content", "tags", "source_client", "sensitivity", "supersedes",
              "superseded_by", "superseded_at", "occurred_at", "occurred_until", "access_count",
              "last_accessed_at", "last_confirmed_at", "created_at", "embedding_model"):
        assert f in rec, f
    assert rec["namespace"] == GATE_NS and rec["source_client"] == "gate-seed"
    assert rec["sensitivity"] == "open" and rec["last_confirmed_at"] is None


def test_the_old_created_at_survives_into_the_archive():
    assert lines_of(build_archive(FIXTURE))[1]["created_at"] == "2026-03-01T00:00:00Z"


def test_foreign_items_lists_every_queue_item_outside_the_gate_namespace():
    q = {"items": [{"key": "stale:1", "namespace": GATE_NS},
                   {"key": "stale:2", "namespace": "project:real"},
                   {"key": "conflict:3:4", "namespace": "global"}]}
    assert foreign_items(q) == ["stale:2", "conflict:3:4"]
    assert foreign_items({"items": []}) == []


def source_archive():
    source = {"namespace": "project:real", "source_client": "source",
             "memories": [{"id": "11111111-0000-4000-8000-000000000001", "content": "real",
                           "tags": [], "created_at": "2026-01-01T00:00:00Z"}]}
    return build_archive(source)


def test_injection_adds_the_fixture_rows_and_corrects_the_header_counts():
    from seed_review import inject_archive
    out = lines_of(inject_archive(source_archive(), FIXTURE))
    assert out[0]["counts"]["memory"] == 3
    assert [r["namespace"] for r in out[1:]] == ["project:real", GATE_NS, GATE_NS]


def test_injection_keeps_every_source_row_and_the_fixture_ids_and_dates():
    from seed_review import inject_archive
    out = lines_of(inject_archive(source_archive(), FIXTURE))
    assert out[1]["content"] == "real"
    assert out[2]["id"] == FIXTURE["memories"][0]["id"] and out[2]["created_at"] == "2026-03-01T00:00:00Z"


def test_injecting_twice_does_not_duplicate_fixture_rows():
    from seed_review import inject_archive
    once = inject_archive(source_archive(), FIXTURE)
    twice = inject_archive(once, FIXTURE)
    assert lines_of(twice)[0]["counts"]["memory"] == 3


def test_live_keys_map_to_fixture_keys_by_the_contents_of_their_rows():
    from seed_review import key_map
    fx = {"tasks": {"review-1": ["conflict:A:B", "stale:C"]},
          "memories": [{"id": "A", "content": "old text"}, {"id": "B", "content": "new text"},
                       {"id": "C", "content": "lone text"}]}
    live_contents = {"x1": "new text", "x2": "old text", "x3": "lone text"}
    live_keys = ["conflict:x2:x1", "stale:x3"]
    assert key_map(fx, live_keys, live_contents) == {"conflict:A:B": "conflict:x2:x1", "stale:C": "stale:x3"}


def test_a_fixture_key_with_no_live_match_is_reported_missing():
    from seed_review import key_map
    fx = {"tasks": {"t": ["stale:C"]}, "memories": [{"id": "C", "content": "lone"}]}
    assert key_map(fx, [], {}) == {}


# Each review task runs on a template that holds its own fixture rows and no other task's.

REAL_FX = json.loads((__import__("pathlib").Path(__file__).parent / "fixtures" / "review.archive.json").read_text())


def test_a_task_fixture_holds_only_that_tasks_rows_and_keys():
    from seed_review import fixture_for_task
    sub = fixture_for_task(REAL_FX, "review-2")
    assert list(sub["tasks"]) == ["review-2"]
    assert sub["memories"] and all(m["task"] == "review-2" for m in sub["memories"])
    ids = {m["id"] for m in sub["memories"]}
    for k in sub["tasks"]["review-2"]:
        assert set(k.split(":")[1:]) <= ids


def test_an_unknown_task_is_refused():
    import pytest
    from seed_review import fixture_for_task
    with pytest.raises(SystemExit):
        fixture_for_task(REAL_FX, "review-9")


def item(key, ns=GATE_NS):
    return {"key": key, "namespace": ns}


def test_queue_problems_name_another_tasks_key_a_foreign_item_and_a_missing_key():
    from seed_review import queue_problems
    mine = ["stale:a", "conflict:b:c"]
    assert queue_problems([item("stale:a"), item("conflict:b:c")], mine) == []
    got = queue_problems([item("stale:a"), item("stale:other-task"), item("undated:r", "project:real")], mine)
    assert any("outside" in p and "undated:r" in p for p in got)
    assert any("not this task's" in p and "stale:other-task" in p for p in got)
    assert any("conflict:b:c" in p and "not in the queue" in p for p in got)


# The queue an agent on a review template sees: the stale source reads rows over a year old, the
# undated source reads rows with no start date whose text writes out a day, and the conflict source
# reads the pairs the sweeper records. The sweeper is an embedding call and stays a live check
# (`seed_review.py check --task`); the other two are modelled here from the rows themselves.

DAY_RE = __import__("re").compile(
    r"\b\d{4}-\d{2}-\d{2}\b"
    r"|\b\d{1,2} (?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]* \d{4}\b"
    r"|\b(?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]* \d{1,2}, \d{4}\b", __import__("re").I)


def seen_queue(fx, task, today):
    from datetime import datetime, timedelta, timezone
    from seed_review import fixture_for_task
    sub = fixture_for_task(fx, task)
    cutoff = today - timedelta(days=365)
    keys = [k for k in sub["tasks"][task] if k.startswith("conflict:")]
    for m in sub["memories"]:
        created = datetime.fromisoformat(m["created_at"].replace("Z", "+00:00"))
        if created < cutoff:
            keys.append(f"stale:{m['id']}")
        if not m.get("occurred_at") and DAY_RE.search(m["content"]):
            keys.append(f"undated:{m['id']}")
    return keys


def test_every_review_task_faces_its_own_fifteen_to_twenty_keys_and_no_more():
    from datetime import datetime, timezone
    # Today, not a fixed day: the fixture dates age, and this test is the first place that shows it.
    today = datetime.now(timezone.utc)
    for task, listed in REAL_FX["tasks"].items():
        seen = seen_queue(REAL_FX, task, today)
        assert 15 <= len(seen) <= 20, (task, len(seen))
        assert sorted(seen) == sorted(listed), task


class FakeMcp:
    """review_queue and review_decide over an in-memory queue."""

    def __init__(self, items):
        self.items = {s: [i for i in items if i["key"].split(":", 1)[0] == s]
                      for s in ("conflict", "stale", "undated", "proposal")}
        self.decided, self.statements = [], []

    def call(self, tool, args):
        if tool == "review_queue":
            src = args["source"][0]
            off, lim = args.get("offset", 0), args.get("limit", 50)
            page = self.items[src][off:off + lim]
            return {"structuredContent": {"items": page, "conflicts_pending": 0,
                                          "has_more": off + lim < len(self.items[src])}}
        if tool == "review_decide":
            self.decided.append(args)
            src = args["key"].split(":", 1)[0]
            self.items[src] = [i for i in self.items[src] if i["key"] != args["key"]]
            return {"structuredContent": {}}
        raise AssertionError(tool)

    def sql(self, q):
        self.statements.append(q)
        for src in ("stale",):
            self.items[src] = [i for i in self.items[src] if i["key"].split(":", 1)[1] not in q]
        return []


def test_emptying_the_queue_fills_the_date_on_every_real_undated_item_and_spares_the_fixtures():
    from seed_review import empty_queue
    real = {"key": "undated:r1", "namespace": "project:real", "dates": ["2026-03-04", "2026-03-09"]}
    fixture = {"key": "undated:f1", "namespace": GATE_NS, "dates": ["2026-04-06"]}
    m = FakeMcp([real, fixture])
    empty_queue(m, fork=False)
    assert m.decided == [{"key": "undated:r1", "verdict": "fill_date", "occurred_at": "2026-03-04"}]
    assert [i["key"] for i in m.items["undated"]] == ["undated:f1"]


def test_check_reads_the_undated_source():
    from seed_review import check
    fx = {"tasks": {"review-1": ["stale:a"]}, "memories": []}
    m = FakeMcp([item("stale:a"), {"key": "undated:r1", "namespace": "project:real"}])
    problems = check(m, fx)
    assert any("undated:r1" in p for p in problems)


def test_the_cli_waits_longer_than_an_import_takes():
    # The CLI gives up after LUMBERROOM_TIMEOUT_MS, 15 s by default, while the server keeps going
    # and commits. Seeding 100 rows took longer than that on the gate host on 12 October 2026, so
    # the default reports a failure for an import that landed.
    from types import SimpleNamespace
    from seed_review import cli_env
    env = cli_env(SimpleNamespace(slot={"port": 8851}, token="t"))
    assert int(env["LUMBERROOM_TIMEOUT_MS"]) >= 600_000


class FakeSlot:
    """Answers backdate's SQL and queue reads. `landed` is the set of fixture contents present."""

    def __init__(self, landed):
        self.slot, self.token = {"port": 1}, "t"
        self.landed = landed
        self.statements = []

    def sql(self, q):
        self.statements.append(q)
        if q.startswith("SELECT json_build_object"):
            return [json.dumps({"id": f"live-{i}", "content": c}) for i, c in enumerate(sorted(self.landed))]
        return []

    def call(self, tool, args):
        return {"structuredContent": {"conflicts_pending": 0, "items": []}}


def failing_cli(tmp_path):
    cli = tmp_path / "cli"
    cli.write_text("#!/bin/sh\necho 'archive_import_failed: statement timeout' >&2\nexit 1\n")
    cli.chmod(0o755)
    return str(cli)


def test_backdate_goes_on_when_the_import_fails_after_every_row_landed(tmp_path):
    # The import's last step, graph::rebuild, can time out on a large corpus after the rows are
    # written. Every fixture row is present, so the backdate still runs.
    from seed_review import backdate
    m = FakeSlot({x["content"] for x in FIXTURE["memories"]})
    backdate(m, failing_cli(tmp_path), FIXTURE)
    assert any(q.startswith("UPDATE memory SET created_at") for q in m.statements)


def test_backdate_stops_when_the_import_fails_and_a_row_is_missing(tmp_path):
    import pytest
    from seed_review import backdate
    m = FakeSlot({FIXTURE["memories"][0]["content"]})
    with pytest.raises(SystemExit, match="did not land"):
        backdate(m, failing_cli(tmp_path), FIXTURE)
