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
