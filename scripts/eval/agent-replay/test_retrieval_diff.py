"""Pure parts of G1 and the consumer check."""
from retrieval_diff import (
    collect_ids, collect_keys, default_page_size, diff_lists, left_out_stats,
    check_search_fields, check_bootstrap_fields, split_by_source, sse_or_json,
)

A, B, C = "id-a", "id-b", "id-c"


def test_hit_ids_come_back_in_list_order():
    s = {"hits": [{"id": A, "content": "x"}, {"id": B, "content": "y"}], "namespaces": ["global"]}
    assert collect_ids(s) == [A, B]


def test_review_keys_come_back_in_document_order():
    s = {"items": [{"key": "conflict:1:2", "rows": [{"id": "r1"}]}, {"key": "stale:3"}]}
    assert collect_keys(s) == ["conflict:1:2", "stale:3"]


def test_the_default_page_is_ten_over_n_rounded_up_with_a_floor_of_two():
    assert default_page_size(1) == 10
    assert default_page_size(2) == 5
    assert default_page_size(3) == 4
    assert default_page_size(10) == 2
    assert default_page_size(20) == 2


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
