"""Pure parts of the build replay and the consumer check."""
from retrieval_diff import (
    collect_ids, collect_keys, DEFAULT_PAGE, default_page_diffs, diff_lists, left_out_stats,
    check_search_fields, check_bootstrap_fields, split_by_source, sse_or_json,
)

A, B, C = "id-a", "id-b", "id-c"


def test_hit_ids_come_back_in_list_order():
    s = {"hits": [{"id": A, "content": "x"}, {"id": B, "content": "y"}], "namespaces": ["global"]}
    assert collect_ids(s) == [A, B]


def test_review_keys_come_back_in_document_order():
    s = {"items": [{"key": "conflict:1:2", "rows": [{"id": "r1"}]}, {"key": "stale:3"}]}
    assert collect_keys(s) == ["conflict:1:2", "stale:3"]


def test_a_call_with_no_limit_reads_up_to_50_per_source():
    # Decision 0031: an MCP call with no limit keeps the service default of 50 per source.
    assert DEFAULT_PAGE == 50


def keys(src, n):
    return [f"{src}:{i}" for i in range(n)]


def test_a_default_page_matching_the_first_50_per_source_with_no_more_passes():
    full = {"conflict": keys("conflict", 12), "stale": keys("stale", 50)}
    assert default_page_diffs(full, dict(full), has_more_hint=False) == []


def test_a_default_page_cut_short_of_50_is_a_difference():
    full = {"conflict": keys("conflict", 12), "stale": keys("stale", 30)}
    short = {"conflict": full["conflict"], "stale": full["stale"][:10]}
    names = [d["name"] for d in default_page_diffs(full, short, has_more_hint=False)]
    assert names == ["review stale default page"]


def test_a_default_page_past_50_in_one_source_is_a_size_difference():
    full = {"conflict": keys("conflict", 60), "stale": keys("stale", 3)}
    long = {"conflict": full["conflict"], "stale": full["stale"]}
    diffs = default_page_diffs(full, long, has_more_hint=True)
    assert [d["name"] for d in diffs] == ["review conflict default page size"]
    assert diffs[0]["base"] == 50 and diffs[0]["branch"] == 60


def test_more_must_appear_past_50_in_a_source_and_only_then():
    over = {"conflict": keys("conflict", 51), "stale": []}
    page = {"conflict": over["conflict"][:50], "stale": []}
    assert default_page_diffs(over, page, has_more_hint=True) == []
    missing = default_page_diffs(over, page, has_more_hint=False)
    assert [d["name"] for d in missing] == ["review default page more"]
    at = {"conflict": keys("conflict", 50), "stale": []}
    extra = default_page_diffs(at, dict(at), has_more_hint=True)
    assert [d["name"] for d in extra] == ["review default page more"]


def test_diff_lists_reports_the_first_difference_and_nothing_for_equal_lists():
    assert diff_lists("q", [A, B], [A, B]) is None
    d = diff_lists("q", [A, B], [B, A])
    assert d["name"] == "q" and d["base"] == [A, B] and d["branch"] == [B, A]


def test_keys_split_by_source_prefix_keeping_order():
    keys = ["conflict:1:2", "stale:3", "conflict:4:5", "stale:6"]
    assert split_by_source(keys) == {"conflict": ["conflict:1:2", "conflict:4:5"],
                                     "stale": ["stale:3", "stale:6"]}


def test_left_out_stats_count_entries_and_bytes():
    s = {"left_out": [{"id": A, "content": "abc"}], "left_out_registry": [{"key": "k", "value": 1}]}
    st = left_out_stats(s)
    assert st["left_out"] == 1 and st["left_out_registry"] == 1 and st["bytes"] > 0
    assert left_out_stats({}) == {"left_out": 0, "left_out_registry": 0, "bytes": 0}


BASE_HIT = {"id": A, "namespace": "global", "content": "c", "source": "app", "occurred_at": "2026-01-01T00:00:00+00:00",
            "similarity": 0.7, "score": 0.03, "created_at": "2026-01-02T00:00:00+00:00"}


def test_hermes_fields_must_match_the_base_value_for_value():
    base = {"hits": [BASE_HIT]}
    brief = {"hits": [{k: v for k, v in BASE_HIT.items() if k != "score"}]}
    assert check_search_fields(base, brief, consumer="hermes") == []
    broken = {"hits": [{**brief["hits"][0], "content": "changed"}]}
    assert check_search_fields(base, broken, consumer="hermes") == ["hits[0].content differs"]


def test_the_mod_needs_similarity_and_the_cli_needs_score():
    base = {"hits": [BASE_HIT]}
    no_sim = {"hits": [{k: v for k, v in BASE_HIT.items() if k != "similarity"}]}
    assert "hits[0].similarity is missing" in check_search_fields(base, no_sim, consumer="mod")
    no_score = {"hits": [{k: v for k, v in BASE_HIT.items() if k != "score"}]}
    assert "hits[0].score is missing" in check_search_fields(base, no_score, consumer="cli")
    assert check_search_fields(base, base, consumer="cli") == []


def test_bootstrap_fields_text_counts_and_inventories_must_survive():
    base = {"text": "t", "counts": {"memories": 3, "by_namespace": {"g": 3}}, "inventory": {"g": 3},
            "sealed_inventory": {}}
    brief = {"text": "t", "counts": {"memories": 3}, "inventory": {"g": 3}, "sealed_inventory": {}}
    assert check_bootstrap_fields(base, brief, rows=False) == []
    # by_namespace leaves the brief payload and returns on rows: true.
    assert check_bootstrap_fields(base, brief, rows=True) == ["counts.by_namespace is missing"]
    assert check_bootstrap_fields(base, {**brief, "text": "u"}, rows=False) == ["text differs"]


def test_sse_bodies_and_json_bodies_both_parse():
    assert sse_or_json('{"a": 1}') == {"a": 1}
    assert sse_or_json('event: message\ndata: {"a": 2}\n\n') == {"a": 2}


def test_frozen_queries_and_the_report_default_to_the_gate_root(monkeypatch, tmp_path):
    # Both files carry store text, so they live with the run records and never in the checkout.
    from retrieval_diff import build_parser, resolve_outputs
    monkeypatch.setenv("LR_GATE_ROOT", str(tmp_path))
    f = resolve_outputs(build_parser().parse_args(["freeze", "--slot", "s"]))
    assert f.out == tmp_path / "frozen-queries.jsonl"
    r = resolve_outputs(build_parser().parse_args(
        ["run", "--base-slot", "a", "--branch-slot", "b", "--cli-fixture", "c"]))
    assert r.frozen == tmp_path / "frozen-queries.jsonl" and r.out == tmp_path / "build-replay.md"
