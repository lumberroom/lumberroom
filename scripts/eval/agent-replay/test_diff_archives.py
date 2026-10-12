import gzip
import json

from diff_archives import load_rows, diff_rows

FIELDS = ("id", "namespace", "content", "tags", "created_at", "occurred_at", "occurred_until", "supersedes")


def row(i, **kw):
    r = {"section": "memory", "id": i, "namespace": "g", "content": "c", "tags": ["t"],
         "created_at": "2026-01-01T00:00:00Z", "occurred_at": None, "occurred_until": None,
         "supersedes": None, "access_count": 3, "source_client": "x"}
    r.update(kw)
    return r


def blob(*rows):
    head = {"section": "header", "format": "lumberroom.archive", "version": 1}
    return gzip.compress("".join(json.dumps(r) + "\n" for r in (head, *rows)).encode())


def test_equal_exports_have_no_difference_even_when_bookkeeping_fields_differ():
    a = load_rows(blob(row("1"), row("2")))
    b = load_rows(blob(row("2", access_count=9, source_client="y"), row("1")))
    assert diff_rows(a, b) == []


def test_a_changed_field_a_missing_row_and_an_extra_row_each_show():
    a = load_rows(blob(row("1"), row("2")))
    b = load_rows(blob(row("1", content="changed"), row("3")))
    d = diff_rows(a, b)
    assert "1: content differs" in d and "2: missing from the second export" in d and "3: extra in the second export" in d


def test_each_compared_field_is_checked():
    for f, v in (("namespace", "z"), ("tags", []), ("created_at", "2025-01-01T00:00:00Z"),
                 ("occurred_at", "2026-02-02T00:00:00Z"), ("occurred_until", "2026-02-03T00:00:00Z"),
                 ("supersedes", "9")):
        assert diff_rows(load_rows(blob(row("1"))), load_rows(blob(row("1", **{f: v})))) == [f"1: {f} differs"]
