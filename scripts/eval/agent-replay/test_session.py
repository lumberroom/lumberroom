"""The runner's command construction is a pure function. These tests pin the exact argv."""
import pytest

from pathlib import Path

from session import (
    build_argv, check_event, check_init, claude_bin, gate_root, Rejected, LUMBERROOM_TOOLS,
)

CLAUDE = "/opt/tools/bin/claude"
ENV = {"HOME": "/h", "PATH": "/opt/tools/bin:/bin", "USER": "gate", "LANG": "C.UTF-8"}


def test_the_claude_binary_comes_from_lr_gate_claude_and_defaults_to_path_lookup():
    assert claude_bin({"LR_GATE_CLAUDE": CLAUDE}) == CLAUDE
    assert claude_bin({}) == "claude"
    assert claude_bin({"LR_GATE_CLAUDE": ""}) == "claude"


def test_the_run_root_comes_from_lr_gate_root():
    assert gate_root({"LR_GATE_ROOT": "/data/runs"}) == Path("/data/runs")


def test_the_default_run_root_sits_under_home_and_outside_any_checkout():
    # Run records hold store content. A default relative to the working directory lands them in
    # the checkout whenever the operator runs a script from the repository root.
    assert gate_root({"HOME": "/h"}) == Path("/h/.local/share/lumberroom-gate")
    assert gate_root({"HOME": "/h", "LR_GATE_ROOT": ""}) == Path("/h/.local/share/lumberroom-gate")


def argv(**kw):
    base = dict(claude=CLAUDE, prompt="Work my review queue.", mcp_config="/slot/mcp.json",
                model="claude-sonnet-5-5", server="lumberroom", tools=LUMBERROOM_TOOLS, env=ENV)
    base.update(kw)
    return build_argv(**base)


def test_the_argv_is_exactly_the_gate_command():
    tools = "mcp__lumberroom__context_bootstrap,mcp__lumberroom__memory_search"
    got = argv(tools=["context_bootstrap", "memory_search"])
    assert got == [
        "env", "-i", "HOME=/h", "PATH=/opt/tools/bin:/bin", "USER=gate", "LANG=C.UTF-8",
        "CLAUDE_CODE_DISABLE_AUTO_MEMORY=1", CLAUDE, "-p", "Work my review queue.",
        "--setting-sources", "project",
        "--strict-mcp-config", "--mcp-config", "/slot/mcp.json",
        "--tools", "",
        "--allowedTools", tools,
        "--no-session-persistence", "--no-chrome", "--disable-slash-commands",
        "--include-hook-events",
        "--output-format", "stream-json", "--verbose",
        "--model", "claude-sonnet-5-5",
    ]


def test_the_session_environment_carries_only_the_allowlist():
    noisy = {**ENV, "ANTHROPIC_API_KEY": "k", "ANTHROPIC_BASE_URL": "u", "ANTHROPIC_AUTH_TOKEN": "t",
             "CLAUDE_CODE_USE_BEDROCK": "1", "CLAUDE_CODE_USE_VERTEX": "1", "LR_GATE_ROOT": "/r",
             "TMPDIR": "/tmp/x", "TERM": "dumb", "LC_ALL": "C", "AWS_PROFILE": "p"}
    got = argv(env=noisy)
    assert got[:2] == ["env", "-i"]
    passed = got[2:got.index(CLAUDE)]
    assert {p.split("=", 1)[0] for p in passed} == {
        "HOME", "PATH", "USER", "LANG", "LC_ALL", "TMPDIR", "TERM", "CLAUDE_CODE_DISABLE_AUTO_MEMORY"}
    joined = " ".join(got)
    for banned in ("ANTHROPIC_", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "AWS_PROFILE"):
        assert banned not in joined


@pytest.mark.parametrize("flag", [
    "--setting-sources", "--strict-mcp-config", "--mcp-config", "--tools", "--allowedTools",
    "--no-session-persistence", "--no-chrome", "--disable-slash-commands",
    "--include-hook-events", "--output-format", "--verbose", "--model",
])
def test_every_isolation_flag_is_present(flag):
    assert flag in argv()


def test_the_runner_never_uses_bare_or_a_separate_config_dir():
    got = " ".join(argv())
    assert "--bare" not in got and "CLAUDE_CONFIG_DIR" not in got and "setup-token" not in got


def test_the_tools_flag_is_an_empty_string_so_no_builtin_loads():
    got = argv()
    assert got[got.index("--tools") + 1] == ""


def test_allowed_tools_are_server_prefixed():
    got = argv()
    allowed = got[got.index("--allowedTools") + 1].split(",")
    assert allowed and all(t.startswith("mcp__lumberroom__") for t in allowed)


def test_a_judge_session_carries_no_mcp_flags():
    got = build_argv(claude=CLAUDE, prompt="p", model="claude-opus-5-5", judge=True, env=ENV)
    assert "--mcp-config" not in got and "--allowedTools" not in got
    assert got[got.index("--tools") + 1] == ""
    assert "--strict-mcp-config" in got
    assert got[:2] == ["env", "-i"]


def good_init(**over):
    ev = {"type": "system", "subtype": "init", "apiKeySource": "none",
          "mcp_servers": [{"name": "lumberroom", "status": "connected"}],
          "tools": [f"mcp__lumberroom__{t}" for t in LUMBERROOM_TOOLS],
          "plugins": [{"name": "cc-plugin-agents-md"}, {"name": "cc-plugin-telemetry"}]}
    ev.update(over)
    return ev


def test_a_clean_init_event_passes():
    check_init(good_init(), server="lumberroom")


ALL_TOOLS = [f"mcp__lumberroom__{t}" for t in LUMBERROOM_TOOLS]


@pytest.mark.parametrize("over,why", [
    ({"apiKeySource": "ANTHROPIC_API_KEY"}, "api key"),
    ({"mcp_servers": [{"name": "lumberroom", "status": "connected"},
                      {"name": "sentry", "status": "connected"}]}, "other mcp server"),
    ({"mcp_servers": [{"name": "other", "status": "connected"}]}, "wrong mcp server"),
    ({"mcp_servers": [{"name": "lumberroom", "status": "failed"}]}, "server failed to connect"),
    ({"mcp_servers": [{"name": "lumberroom", "status": "pending"}]}, "server still pending"),
    ({"mcp_servers": [{"name": "lumberroom"}]}, "no status at all"),
    ({"tools": ALL_TOOLS + ["Bash"]}, "builtin tool"),
    ({"tools": [t for t in ALL_TOOLS if not t.endswith("review_queue")]}, "a lumberroom tool is missing"),
    ({"tools": []}, "no tools at all"),
    ({"plugins": [{"name": "lumberroom-memory"}]}, "foreign plugin"),
])
def test_an_init_event_that_breaks_a_condition_is_rejected(over, why):
    with pytest.raises(Rejected):
        check_init(good_init(**over), server="lumberroom")


def test_auto_memory_is_off_for_every_session_and_the_judge():
    # Claude Code 2.1.296 turns auto memory on by default and adds its instructions to the system
    # prompt. CLAUDE_CODE_DISABLE_AUTO_MEMORY=1 is the documented switch
    # (code.claude.com/docs/en/memory). A caller's own value never reaches the session.
    for got in (argv(), argv(env={**ENV, "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "0"}),
                build_argv(claude=CLAUDE, prompt="p", model="m", judge=True, env=ENV)):
        passed = got[2:got.index(CLAUDE)]
        assert [p for p in passed if p.startswith("CLAUDE_CODE_DISABLE_AUTO_MEMORY=")] == [
            "CLAUDE_CODE_DISABLE_AUTO_MEMORY=1"]


def test_the_real_init_shape_from_claude_code_2_1_296_passes():
    # Captured on the gate host on 12 October 2026: each server carries name, status and source,
    # each plugin carries name, path and source, and with auto memory off memory_paths is empty.
    check_init(good_init(
        mcp_servers=[{"name": "lumberroom", "status": "connected", "source": "dynamic"}],
        plugins=[{"name": n, "path": "builtin", "source": f"{n}@builtin"} for n in
                 ("cc-plugin-agents-md", "cc-plugin-telemetry", "cc-plugin-plugin-authoring")],
        memory_paths={}), server="lumberroom")


def test_an_init_that_shows_auto_memory_on_is_rejected():
    # With auto memory on, 2.1.296 reports its directory under memory_paths.auto.
    with pytest.raises(Rejected, match="auto memory"):
        check_init(good_init(memory_paths={"auto": "/var/tmp/u/.claude/projects/-tmp-lr-gate-x/memory/"}),
                   server="lumberroom")


def test_a_hook_event_rejects_the_session():
    with pytest.raises(Rejected):
        check_event({"type": "system", "subtype": "hook_started", "hook_name": "SessionStart"})


def tool_use(name):
    return {"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": "t1", "name": name, "input": {}}]}}


def test_a_tool_use_outside_the_slot_server_rejects_the_session():
    with pytest.raises(Rejected):
        check_event(tool_use("Bash"))
    with pytest.raises(Rejected):
        check_event(tool_use("mcp__other__memory_search"))


def test_a_tool_use_on_the_slot_server_passes():
    check_event(tool_use("mcp__lumberroom__memory_search"))


def test_the_prompt_is_the_snippet_then_the_skill_body_then_the_task():
    from session import build_prompt
    skill = "---\nname: lr-review\ndescription: x\n---\n# Working the queue\nDecide."
    got = build_prompt(snippet="SNIPPET", task_prompt="Work my review queue.", skill_text=skill)
    assert got.index("SNIPPET") < got.index("# Working the queue") < got.index("Work my review queue.")
    assert "name: lr-review" not in got


def test_a_task_with_no_skill_gets_the_snippet_and_the_task_only():
    from session import build_prompt
    assert build_prompt("SNIPPET", "Which port?", None) == "SNIPPET\n\nWhich port?"


def test_a_record_is_written_whole_with_no_temporary_file_left(tmp_path):
    from session import write_record
    p = tmp_path / "runs" / "r.json"
    write_record(p, {"a": 1})
    assert p.read_text().startswith("{") and not list(p.parent.glob("*.tmp"))


def test_an_interrupted_write_leaves_the_old_record_intact(tmp_path, monkeypatch):
    from pathlib import Path
    from session import write_record
    p = tmp_path / "runs" / "r.json"
    write_record(p, {"a": 1})

    def boom(self, target):
        raise OSError("killed between the temporary write and the rename")
    monkeypatch.setattr(Path, "replace", boom)
    try:
        write_record(p, {"a": 2})
    except OSError:
        pass
    assert p.read_text().count('"a": 1') == 1


# ---- run_session over a stand-in binary that prints a recorded stream -----------------------

def fake_claude(tmp_path, events):
    import json as _j
    stream = tmp_path / "stream.jsonl"
    stream.write_text("".join(_j.dumps(e) + "\n" for e in events))
    exe = tmp_path / "claude"
    exe.write_text(f"#!/bin/sh\ncat '{stream}'\n")
    exe.chmod(0o755)
    return str(exe)


def stub_session(tmp_path, monkeypatch, events):
    import session
    monkeypatch.setattr(session, "snapshot", lambda slot: {"memory": {}, "registry": {}})
    (tmp_path / "arm" / "client").mkdir(parents=True)
    (tmp_path / "arm" / "client" / "CLAUDE.md.snippet").write_text("Use lumberroom.")
    slot = {"checkout": str(tmp_path / "arm"), "mcp_config": "/slot/mcp.json", "client": "gate-A0"}
    task = {"id": "recall-1", "prompt": "What port?", "items": []}
    return session.run_session(slot=slot, task=task, stage="sonnet", arm="A0", channel="structured",
                               rep=1, model="m", root=tmp_path / "root",
                               claude=fake_claude(tmp_path, events))


INIT_OK = {"type": "system", "subtype": "init", "apiKeySource": "none",
           "mcp_servers": [{"name": "lumberroom", "status": "connected"}],
           "tools": [f"mcp__lumberroom__{t}" for t in LUMBERROOM_TOOLS], "plugins": []}
SAVED = "~/.claude/projects/-tmp-lr-gate-abc/s1/tool-results/mcp-lumberroom-memory_search-1.txt"


def test_a_read_of_a_saved_answer_records_bad_init_and_file_stubbed(tmp_path, monkeypatch):
    events = [
        INIT_OK,
        {"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "1", "name": "mcp__lumberroom__memory_search", "input": {"query": "q"}}]}},
        {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "1",
            "content": f"Output too large. Full output saved to: {SAVED}"}]}},
        {"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "2", "name": "Read", "input": {"file_path": SAVED}}]}},
        {"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "rateLimitType": "five_hour",
                                                         "unifiedWindows": {"five_hour": {"utilization": 0.3}}}},
        {"type": "result", "subtype": "success", "is_error": False, "result": "9100", "usage": {}},
    ]
    rec = stub_session(tmp_path, monkeypatch, events)
    assert rec["incomplete_reason"] == "bad_init" and rec["file_stubbed"] is True
    assert rec["rate_limit"]["unifiedWindows"]["five_hour"]["utilization"] == 0.3


def test_an_inline_session_records_complete_and_not_file_stubbed(tmp_path, monkeypatch):
    events = [
        INIT_OK,
        {"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "1", "name": "mcp__lumberroom__memory_search", "input": {"query": "q"}}]}},
        {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "1",
                                                  "content": '{"hits": []}'}]}},
        {"type": "result", "subtype": "success", "is_error": False, "result": "none", "usage": {}},
    ]
    rec = stub_session(tmp_path, monkeypatch, events)
    assert rec["status"] == "complete" and rec["file_stubbed"] is False and rec["rate_limit"] is None


# ---- extra tools a run declares through LR_GATE_EXTRA_TOOLS ---------------------------------

def test_extra_tools_come_from_lr_gate_extra_tools_comma_separated():
    from session import extra_tools
    assert extra_tools({"LR_GATE_EXTRA_TOOLS": "extra_queue, extra_release,,"}) == [
        "extra_queue", "extra_release"]
    assert extra_tools({}) == [] and extra_tools({"LR_GATE_EXTRA_TOOLS": ""}) == []


def test_session_tools_add_the_declared_extras_to_the_engine_twelve_once():
    from session import session_tools
    got = session_tools({"LR_GATE_EXTRA_TOOLS": "extra_lookup,memory_search"})
    assert got == LUMBERROOM_TOOLS + ["extra_lookup"]


def test_an_extra_tool_name_that_is_not_a_bare_tool_name_is_refused():
    # A prefixed or comma-mangled name would never match an init entry and fail every session.
    from session import extra_tools
    for bad in ("mcp__lumberroom__extra_lookup", "memory why", "Extra_Lookup"):
        with pytest.raises(ValueError, match="LR_GATE_EXTRA_TOOLS"):
            extra_tools({"LR_GATE_EXTRA_TOOLS": bad})


def test_an_undeclared_tool_on_the_slot_server_is_rejected():
    with pytest.raises(Rejected, match="undeclared"):
        check_init(good_init(tools=ALL_TOOLS + ["mcp__lumberroom__extra_release"]),
                   server="lumberroom")


def test_a_declared_extra_tool_on_the_slot_server_passes():
    expected = LUMBERROOM_TOOLS + ["extra_release"]
    check_init(good_init(tools=ALL_TOOLS + ["mcp__lumberroom__extra_release"]),
               server="lumberroom", expected_tools=expected)


def test_a_declared_extra_tool_the_server_never_loaded_is_rejected():
    with pytest.raises(Rejected, match="missing"):
        check_init(good_init(), server="lumberroom",
                   expected_tools=LUMBERROOM_TOOLS + ["extra_release"])


def test_lr_gate_extra_tools_never_reaches_the_session_environment():
    got = argv(env={**ENV, "LR_GATE_EXTRA_TOOLS": "extra_release"})
    assert "LR_GATE_EXTRA_TOOLS" not in " ".join(got[:got.index(CLAUDE)])


def arg_logging_claude(tmp_path, events):
    """A stand-in binary that prints the stream and keeps its own argv, one argument per line."""
    import json as _j
    stream = tmp_path / "stream.jsonl"
    stream.write_text("".join(_j.dumps(e) + "\n" for e in events))
    exe = tmp_path / "claude"
    exe.write_text(f"#!/bin/sh\nprintf '%s\\n' \"$@\" > '{tmp_path / 'args.txt'}'\ncat '{stream}'\n")
    exe.chmod(0o755)
    return str(exe)


def run_stub(tmp_path, monkeypatch, events, task=None):
    import session
    monkeypatch.setattr(session, "snapshot", lambda slot: {"memory": {}, "registry": {}})
    (tmp_path / "arm" / "client").mkdir(parents=True)
    (tmp_path / "arm" / "client" / "CLAUDE.md.snippet").write_text("Use lumberroom.")
    slot = {"checkout": str(tmp_path / "arm"), "mcp_config": "/slot/mcp.json", "client": "gate-A0"}
    task = task or {"id": "recall-1", "prompt": "What port?", "items": []}
    return session.run_session(slot=slot, task=task, stage="sonnet", arm="A0", channel="structured",
                               rep=1, model="m", root=tmp_path / "root",
                               claude=arg_logging_claude(tmp_path, events))


CLEAN_END = {"type": "result", "subtype": "success", "is_error": False, "result": "ok", "usage": {}}


def test_a_run_with_declared_extras_allows_them_and_accepts_an_init_that_lists_them(tmp_path, monkeypatch):
    monkeypatch.setenv("LR_GATE_EXTRA_TOOLS", "extra_release")
    init = {**INIT_OK, "tools": INIT_OK["tools"] + ["mcp__lumberroom__extra_release"]}
    rec = run_stub(tmp_path, monkeypatch, [init, CLEAN_END])
    assert rec["status"] == "complete"
    args = (tmp_path / "args.txt").read_text().splitlines()
    assert "mcp__lumberroom__extra_release" in args[args.index("--allowedTools") + 1].split(",")


def test_a_run_without_the_declaration_rejects_an_init_that_lists_an_extra(tmp_path, monkeypatch):
    monkeypatch.delenv("LR_GATE_EXTRA_TOOLS", raising=False)
    init = {**INIT_OK, "tools": INIT_OK["tools"] + ["mcp__lumberroom__extra_release"]}
    rec = run_stub(tmp_path, monkeypatch, [init, CLEAN_END])
    assert rec["incomplete_reason"] == "bad_init"


def test_an_unknown_item_kind_records_unscored_instead_of_losing_the_session(tmp_path, monkeypatch):
    task = {"id": "x-1", "prompt": "p", "items": [{"item": "odd", "kind": "teleport"}]}
    rec = run_stub(tmp_path, monkeypatch, [INIT_OK, CLEAN_END], task=task)
    assert rec["status"] == "complete" and rec["outcomes"] == {"odd": "unscored"}


# ---- LR_GATE_TOOLS replaces the engine's twelve ---------------------------------------------

def test_lr_gate_tools_replaces_the_engine_list_and_extras_still_add_on_top():
    from session import session_tools
    env = {"LR_GATE_TOOLS": "context_bootstrap, memory_search,tool_x", "LR_GATE_EXTRA_TOOLS": "extra_lookup,tool_x"}
    assert session_tools(env) == ["context_bootstrap", "memory_search", "tool_x", "extra_lookup"]
    assert session_tools({"LR_GATE_EXTRA_TOOLS": "extra_lookup"}) == LUMBERROOM_TOOLS + ["extra_lookup"]


@pytest.mark.parametrize("value", ["", " , ", "mcp__lumberroom__memory_search", "memory_search,Tool_X"])
def test_an_empty_or_malformed_lr_gate_tools_is_refused(value):
    # Set but empty would allow no tool at all, so every session would fail at check_init.
    from session import session_tools
    with pytest.raises(ValueError, match="LR_GATE_TOOLS"):
        session_tools({"LR_GATE_TOOLS": value})


REPLACED = ["context_bootstrap", "memory_search", "tool_x"]


def test_a_replaced_set_drops_the_missing_engine_tool_from_allowed_tools(tmp_path, monkeypatch):
    monkeypatch.setenv("LR_GATE_TOOLS", ",".join(REPLACED))
    monkeypatch.delenv("LR_GATE_EXTRA_TOOLS", raising=False)
    init = {**INIT_OK, "tools": [f"mcp__lumberroom__{t}" for t in REPLACED]}
    rec = run_stub(tmp_path, monkeypatch, [init, CLEAN_END])
    assert rec["status"] == "complete"
    args = (tmp_path / "args.txt").read_text().splitlines()
    allowed = args[args.index("--allowedTools") + 1].split(",")
    assert allowed == [f"mcp__lumberroom__{t}" for t in REPLACED]


def test_a_replaced_set_refuses_an_init_that_lists_a_dropped_engine_tool(tmp_path, monkeypatch):
    monkeypatch.setenv("LR_GATE_TOOLS", ",".join(REPLACED))
    monkeypatch.delenv("LR_GATE_EXTRA_TOOLS", raising=False)
    init = {**INIT_OK, "tools": [f"mcp__lumberroom__{t}" for t in REPLACED + ["review_decide"]]}
    rec = run_stub(tmp_path, monkeypatch, [init, CLEAN_END])
    assert rec["incomplete_reason"] == "bad_init"


def test_check_init_names_a_dropped_engine_tool_as_undeclared():
    with pytest.raises(Rejected, match="undeclared.*review_decide"):
        check_init(good_init(tools=[f"mcp__lumberroom__{t}" for t in REPLACED + ["review_decide"]]),
                   server="lumberroom", expected_tools=REPLACED)
