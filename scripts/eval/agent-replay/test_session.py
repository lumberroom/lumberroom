"""The runner's command construction is a pure function. These tests pin the exact argv."""
import pytest

from session import build_argv, check_init, Rejected, LUMBERROOM_TOOLS

CLAUDE = "/home/cybersapien/.local/bin/claude"


def argv(**kw):
    base = dict(claude=CLAUDE, prompt="Work my review queue.", mcp_config="/slot/mcp.json",
                model="claude-sonnet-5-5", server="lumberroom", tools=LUMBERROOM_TOOLS)
    base.update(kw)
    return build_argv(**base)


def test_the_argv_is_exactly_the_spec_command():
    tools = "mcp__lumberroom__context_bootstrap,mcp__lumberroom__memory_search"
    got = argv(tools=["context_bootstrap", "memory_search"])
    assert got == [
        "env", "-u", "ANTHROPIC_API_KEY", CLAUDE, "-p", "Work my review queue.",
        "--setting-sources", "project",
        "--strict-mcp-config", "--mcp-config", "/slot/mcp.json",
        "--tools", "",
        "--allowedTools", tools,
        "--no-session-persistence", "--no-chrome", "--disable-slash-commands",
        "--include-hook-events",
        "--output-format", "stream-json", "--verbose",
        "--model", "claude-sonnet-5-5",
    ]


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
    got = build_argv(claude=CLAUDE, prompt="p", model="claude-opus-5-5", judge=True)
    assert "--mcp-config" not in got and "--allowedTools" not in got
    assert got[got.index("--tools") + 1] == ""
    assert "--strict-mcp-config" in got


def good_init(**over):
    ev = {"type": "system", "subtype": "init", "apiKeySource": "none",
          "mcp_servers": [{"name": "lumberroom", "status": "connected"}],
          "tools": ["mcp__lumberroom__memory_search"],
          "plugins": [{"name": "cc-plugin-agents-md"}, {"name": "cc-plugin-telemetry"}]}
    ev.update(over)
    return ev


def test_a_clean_init_event_passes():
    check_init(good_init(), server="lumberroom")


@pytest.mark.parametrize("over,why", [
    ({"apiKeySource": "ANTHROPIC_API_KEY"}, "api key"),
    ({"mcp_servers": [{"name": "lumberroom"}, {"name": "sentry"}]}, "other mcp server"),
    ({"mcp_servers": [{"name": "other"}]}, "wrong mcp server"),
    ({"tools": ["mcp__lumberroom__memory_search", "Bash"]}, "builtin tool"),
    ({"plugins": [{"name": "lumberroom-memory"}]}, "foreign plugin"),
])
def test_an_init_event_that_breaks_a_condition_is_rejected(over, why):
    with pytest.raises(Rejected):
        check_init(good_init(**over), server="lumberroom")


def test_a_hook_event_rejects_the_session():
    from session import check_event
    with pytest.raises(Rejected):
        check_event({"type": "system", "subtype": "hook_started", "hook_name": "SessionStart"})


def test_the_prompt_is_the_snippet_then_the_skill_body_then_the_task():
    from session import build_prompt
    skill = "---\nname: lr-review\ndescription: x\n---\n# Working the queue\nDecide."
    got = build_prompt(snippet="SNIPPET", task_prompt="Work my review queue.", skill_text=skill)
    assert got.index("SNIPPET") < got.index("# Working the queue") < got.index("Work my review queue.")
    assert "name: lr-review" not in got


def test_a_task_with_no_skill_gets_the_snippet_and_the_task_only():
    from session import build_prompt
    assert build_prompt("SNIPPET", "Which port?", None) == "SNIPPET\n\nWhich port?"
