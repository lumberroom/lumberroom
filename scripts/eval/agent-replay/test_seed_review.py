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


def owner_archive():
    owner = {"namespace": "project:real", "source_client": "owner",
             "memories": [{"id": "11111111-0000-4000-8000-000000000001", "content": "real",
                           "tags": [], "created_at": "2026-01-01T00:00:00Z"}]}
    return build_archive(owner)


def test_injection_adds_the_fixture_rows_and_corrects_the_header_counts():
    from seed_review import inject_archive
    out = lines_of(inject_archive(owner_archive(), FIXTURE))
    assert out[0]["counts"]["memory"] == 3
    assert [r["namespace"] for r in out[1:]] == ["project:real", GATE_NS, GATE_NS]


def test_injection_keeps_every_owner_row_and_the_fixture_ids_and_dates():
    from seed_review import inject_archive
    out = lines_of(inject_archive(owner_archive(), FIXTURE))
    assert out[1]["content"] == "real"
    assert out[2]["id"] == FIXTURE["memories"][0]["id"] and out[2]["created_at"] == "2026-03-01T00:00:00Z"


def test_injecting_twice_does_not_duplicate_fixture_rows():
    from seed_review import inject_archive
    once = inject_archive(owner_archive(), FIXTURE)
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
