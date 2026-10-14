import json
from pathlib import Path

from probe import quotes_row, loaded_user_instructions, verdict

ROW = "The harbor-api service listens on port 9100 behind nginx on host quay-01 and restarts under systemd."


def test_a_verbatim_run_of_six_words_counts_as_a_quote():
    assert quotes_row('The row says: "listens on port 9100 behind nginx on host quay-01".', [ROW])


def test_a_paraphrase_or_a_no_text_answer_is_not_a_quote():
    assert not quotes_row("NO TEXT: the answer carried only keys and counts.", [ROW])
    assert not quotes_row("harbor-api is served on 9100 through nginx", [ROW])


def test_user_instructions_are_detected_by_a_distinctive_line_of_the_user_file():
    user_file = "# House rules\nAlways water the fern on the third shelf before the kettle boils.\n"
    assert loaded_user_instructions("I was told: Always water the fern on the third shelf before the kettle boils.", user_file)
    assert not loaded_user_instructions("I was given only the task text.", user_file)


def test_the_probe_passes_when_structured_cannot_quote_and_text_can():
    assert verdict(structured_quotes=False, text_quotes=True)["pass"]
    v = verdict(structured_quotes=True, text_quotes=True)
    assert not v["pass"] and "structured" in v["reason"]
    v = verdict(structured_quotes=False, text_quotes=False)
    assert not v["pass"] and "proxy" in v["reason"]


def test_a_tools_list_without_the_size_declaration_on_memory_search_is_flagged():
    from probe import missing_result_size
    declared = {"name": "memory_search", "_meta": {"anthropic/maxResultSizeChars": 500000}}
    assert missing_result_size([declared, {"name": "memory_write"}]) == []
    assert missing_result_size([{"name": "memory_search"}]) == ["memory_search"]
    assert missing_result_size([{"name": "memory_search", "_meta": {}}]) == ["memory_search"]
    # A tool that never listed is missing too: the check cannot pass on an empty list.
    assert missing_result_size([]) == ["memory_search"]


def test_a_declaration_at_or_under_the_default_threshold_counts_as_missing():
    from probe import missing_result_size
    low = {"name": "memory_search", "_meta": {"anthropic/maxResultSizeChars": 50000}}
    assert missing_result_size([low]) == ["memory_search"]


def test_tools_list_reads_the_slot_mcp_config(tmp_path):
    import json as _j
    from probe import mcp_endpoint
    (tmp_path / "mcp.json").write_text(_j.dumps({"mcpServers": {"lumberroom": {
        "type": "http", "url": "http://127.0.0.1:8941/mcp", "headers": {"Authorization": "Bearer tok"}}}}))
    assert mcp_endpoint(tmp_path) == ("http://127.0.0.1:8941/mcp", "tok")


def test_fetch_tools_follows_next_cursor_across_pages(tmp_path, monkeypatch):
    import io
    import json as _j
    import probe
    (tmp_path / "mcp.json").write_text(_j.dumps({"mcpServers": {"lumberroom": {
        "type": "http", "url": "http://127.0.0.1:8941/mcp", "headers": {"Authorization": "Bearer tok"}}}}))
    pages = {None: {"tools": [{"name": "memory_write"}], "nextCursor": "p2"},
             "p2": {"tools": [{"name": "memory_search"}]}}
    sent = []

    class Resp(io.BytesIO):
        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

    def urlopen(req, timeout=None):
        body = _j.loads(req.data)
        sent.append(body["params"].get("cursor"))
        msg = {"jsonrpc": "2.0", "id": body["id"], "result": pages[body["params"].get("cursor")]}
        return Resp(("event: message\ndata: " + _j.dumps(msg) + "\n\n").encode())

    monkeypatch.setattr(probe.urllib.request, "urlopen", urlopen)
    assert [t["name"] for t in probe.fetch_tools(tmp_path)] == ["memory_write", "memory_search"]
    assert sent == [None, "p2"]
