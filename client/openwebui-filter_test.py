"""
Regression tests for openwebui-filter.py: the single-owner warning, the write rule, fit_digest and
the request the filter sends as context_bootstrap's `query`.

httpx and pydantic (the filter's two runtime dependencies, both supplied by OpenWebUI's own
backend) are not guaranteed to be installed wherever this repo is checked out. The warning,
write-rule and fit_digest tests read the file as text through ast for that reason. The warning
tests guard documentation staying in place: the filter has exactly one token valve shared by every
account it runs for, and the warning is the only thing standing between an operator and enabling
it on a multi-account instance.

RequestQueryPicksThePersonsRequest and FilterSendsTheRequest import the module itself, with
stand-ins for httpx and pydantic in sys.modules, because the query and the cache are runtime
behaviour on a Filter instance.

Run: python3 -I client/openwebui-filter_test.py
"""

import ast
import asyncio
import importlib.util
import pathlib
import sys
import types
import unittest

HERE = pathlib.Path(__file__).parent
SOURCE = (HERE / "openwebui-filter.py").read_text()
SNIPPET = (HERE / "AGENTS.md.snippet").read_text()


def module_constant(name: str) -> str:
    """Read a module-level string constant out of the filter without importing it. httpx and
    pydantic are not guaranteed to be installed here, so an import would fail before the
    assertion ran."""
    for node in ast.parse(SOURCE).body:
        if isinstance(node, ast.Assign) and any(
            isinstance(target, ast.Name) and target.id == name for target in node.targets
        ):
            return ast.literal_eval(node.value)
    raise AssertionError(f"{name} is gone from openwebui-filter.py")


def first_two_sentences_of_the_write_paragraph() -> str:
    lines = SNIPPET.splitlines()
    start = next(i for i, line in enumerate(lines) if line.startswith("**Write.**"))
    end = next(i for i in range(start, len(lines)) if not lines[i].strip())
    paragraph = " ".join(lines[start:end]).removeprefix("**Write.** ")
    head, _, _ = paragraph.partition("Without announcing it.")
    return head.strip()


class SingleOwnerWarningStaysInThePlace(unittest.TestCase):
    def test_the_module_docstring_names_the_single_owner_restriction(self):
        self.assertIn("SINGLE-OWNER ONLY", SOURCE)
        self.assertIn("DO NOT ENABLE THIS ON A MULTI-ACCOUNT OPENWEBUI INSTANCE", SOURCE)

    def test_the_module_docstring_explains_why_the_cache_key_is_not_an_access_boundary(self):
        self.assertIn("cache-locality choice, not an access boundary", SOURCE)

    def test_the_token_valve_description_points_back_at_the_warning(self):
        self.assertIn("single-owner OpenWebUI deployments only", SOURCE)


class WriteRuleReachesTheModel(unittest.TestCase):
    """OpenWebUI reads no CLAUDE.md and no AGENTS.md, so the injected system message is the only
    route the write rule has to a model behind this filter. These assertions catch a dropped
    constant, and a snippet that moved on without the filter following it."""

    def test_the_write_rule_matches_the_snippet_word_for_word(self):
        self.assertEqual(module_constant("WRITE_RULE"), first_two_sentences_of_the_write_paragraph())

    def test_the_injected_block_carries_the_write_rule(self):
        block = SOURCE.split("block = (", 1)[1].split("\n        )", 1)[0]
        self.assertIn("{WRITE_RULE}", block)

def module_function(name: str):
    """Compile one module-level function out of the filter without importing the module, for the
    same reason module_constant reads text. The function must use builtins only."""
    for node in ast.parse(SOURCE).body:
        if isinstance(node, ast.FunctionDef) and node.name == name:
            scope: dict = {}
            exec(compile(ast.Module(body=[node], type_ignores=[]), "openwebui-filter.py", "exec"), scope)
            return scope[name]
    raise AssertionError(f"{name} is gone from openwebui-filter.py")


NOTE = "lumberroom stores more standing preferences than this list, and memory_search finds the one that covers a convention."
DIGEST = "\n".join(
    ["## Memory digest", "Store: 9 memories.", "", "### About the user and standing preferences", NOTE]
    + [f"- Standing rule {i} says something worth keeping _(user:me, 2026-10-06, via mac)_" for i in range(9)]
    + ["", "_(3 more recent entries left out at 6,000 chars; use memory_search for the rest)_"]
)


class FilterCutsAtWholeLines(unittest.TestCase):
    def setUp(self):
        self.fit = module_function("fit_digest")

    def test_a_digest_within_the_cap_is_unchanged(self):
        self.assertEqual(self.fit(DIGEST, len(DIGEST)), DIGEST)

    def test_every_cut_keeps_whole_lines_within_the_cap(self):
        whole = set(DIGEST.splitlines())
        for cap in range(0, len(DIGEST), 5):
            out = self.fit(DIGEST, cap)
            self.assertLessEqual(len(out), cap, cap)
            for line in out.splitlines():
                self.assertTrue(line in whole or line.startswith("[digest truncated"), (cap, line))

    def test_the_server_footer_survives_when_it_fits(self):
        out = self.fit(DIGEST, 400)
        self.assertIn("left out at 6,000 chars", out)
        self.assertIn("[digest truncated by the OpenWebUI filter]", out)
        self.assertNotIn("Standing rule 8", out)

    # The review named caps 240, 200 and 120; every cap up to the digest's length covers them.
    def test_a_cut_leaves_no_run_of_blank_lines(self):
        for cap in [240, *range(len(DIGEST))]:
            self.assertNotIn("\n\n\n", self.fit(DIGEST, cap), cap)

    def test_a_cut_leaves_no_heading_without_a_bullet_under_it(self):
        for cap in [200, 120, *range(len(DIGEST))]:
            lines = self.fit(DIGEST, cap).splitlines()
            for i, line in enumerate(lines):
                if line.startswith("###"):
                    rest = lines[i + 1 : i + 3]
                    # One line of text may sit under a heading, as the profile note does, when a
                    # bullet follows it.
                    if rest and not rest[0].startswith("- "):
                        rest = rest[1:]
                    self.assertTrue(rest and rest[0].startswith("- "), (cap, lines))

    def test_a_cut_keeps_a_text_block_that_never_had_bullets(self):
        # The Not shown block carries one line of text and no bullets, and the cut lands right
        # after it once the Sealed block goes. That line is no note over dropped bullets.
        not_shown = ["", "### Not shown above", "These namespaces also hold memories: project:orbit (3)."]
        sealed = ["", "### Sealed", "Encrypted by the client and unreadable by this server: credentials:aws (2)."]
        footer = DIGEST.splitlines()[-1]
        body = DIGEST.splitlines()[:-2] + not_shown
        digest = "\n".join(body + sealed + ["", footer])
        cap = len("\n".join(body + ["", footer]) + "\n\n[digest truncated by the OpenWebUI filter]")
        out = self.fit(digest, cap)
        self.assertIn("project:orbit (3).", out)
        self.assertNotIn("credentials:aws", out)

    def test_a_cut_never_ends_on_a_heading_and_its_note(self):
        for cap in range(len(DIGEST)):
            out = self.fit(DIGEST, cap)
            if NOTE in out:
                self.assertIn(NOTE + "\n- ", out, cap)


class FakeHttpx(types.ModuleType):
    """httpx as the filter uses it. AsyncClient records every JSON body it posts and answers a
    tools/call with a digest that names the query it got, so a test can see which request a
    digest answered."""

    class HTTPError(Exception):
        pass

    def __init__(self):
        super().__init__("httpx")
        self.calls: list[dict] = []
        self.down = False
        fake = self

        class Response:
            status_code = 200

            def __init__(self, payload):
                self._payload = payload

            def json(self):
                return self._payload

        class AsyncClient:
            def __init__(self, timeout=None):
                pass

            async def __aenter__(self):
                return self

            async def __aexit__(self, *exc):
                return False

            async def post(self, url, headers=None, json=None):
                if fake.down:
                    raise fake.HTTPError("connection refused")
                fake.calls.append(json)
                if json.get("method") != "tools/call":
                    return Response({"result": {}})
                query = json["params"]["arguments"].get("query", "-")
                return Response({"result": {"content": [{"type": "text", "text": f"digest for {query}"}]}})

        self.AsyncClient = AsyncClient
        self.Timeout = lambda seconds: seconds

    def arguments(self) -> list[dict]:
        return [c["params"]["arguments"] for c in self.calls if c.get("method") == "tools/call"]


def load_filter(fake_httpx: FakeHttpx):
    """Import the filter with fake httpx and pydantic modules, then put sys.modules back. Field
    returns its default, so each valve reads as a plain class attribute."""
    pydantic = types.ModuleType("pydantic")
    pydantic.BaseModel = type("BaseModel", (), {})
    pydantic.Field = lambda default=None, **_: default
    saved = {name: sys.modules.get(name) for name in ("httpx", "pydantic")}
    sys.modules.update(httpx=fake_httpx, pydantic=pydantic)
    # The import would otherwise leave client/__pycache__ in the checkout.
    writes_bytecode = sys.dont_write_bytecode
    sys.dont_write_bytecode = True
    try:
        spec = importlib.util.spec_from_file_location("lumberroom_openwebui_filter", HERE / "openwebui-filter.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
    finally:
        sys.dont_write_bytecode = writes_bytecode
        for name, previous in saved.items():
            if previous is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = previous
    return module


DEPLOY = "how do we deploy the staging box"
BACKUPS = "which bucket holds the nightly backups"


def chat(*user_texts: str) -> dict:
    messages: list[dict] = []
    for text in user_texts:
        messages += [{"role": "user", "content": text}, {"role": "assistant", "content": "ok"}]
    return {"messages": messages[:-1]}


def injected(body: dict) -> str | None:
    first = body["messages"][0]
    return first["content"] if first["role"] == "system" else None


class RequestQueryPicksThePersonsRequest(unittest.TestCase):
    def setUp(self):
        module = load_filter(FakeHttpx())
        self.query = lambda messages: module.request_query(
            messages, module.MIN_QUERY_CHARS, module.QUERY_MAX_CHARS
        )
        self.module = module

    def test_the_constants_match_the_claude_code_plugin(self):
        self.assertEqual((self.module.MIN_QUERY_CHARS, self.module.QUERY_MAX_CHARS), (12, 1000))

    def test_the_last_user_message_is_the_query(self):
        self.assertEqual(self.query(chat(DEPLOY, BACKUPS)["messages"]), BACKUPS)

    def test_list_content_keeps_text_parts_and_drops_images(self):
        content = [
            {"type": "text", "text": BACKUPS},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        ]
        self.assertEqual(self.query([{"role": "user", "content": content}]), BACKUPS)

    def test_the_query_is_trimmed_and_cut_at_the_cap(self):
        out = self.query([{"role": "user", "content": "  " + "x" * 1500 + "  "}])
        self.assertEqual(out, "x" * 1000)

    def test_a_short_reply_falls_back_to_the_previous_request(self):
        self.assertEqual(self.query(chat(DEPLOY, "ok do it")["messages"]), DEPLOY)

    def test_nothing_long_enough_gives_no_query(self):
        self.assertIsNone(self.query(chat("ok", "yes")["messages"]))
        self.assertIsNone(self.query([]))
        self.assertIsNone(self.query(["not a dict", {"role": "user", "content": None}, {"role": "user"}]))
        self.assertIsNone(self.query([{"role": "user", "content": "   " + "a" * 11 + "   "}]))
        self.assertEqual(self.query([{"role": "user", "content": "a" * 12}]), "a" * 12)


class FilterSendsTheRequest(unittest.TestCase):
    def setUp(self):
        self.fake = FakeHttpx()
        self.module = load_filter(self.fake)
        self.clock = [100.0]
        self.module.time = types.SimpleNamespace(monotonic=lambda: self.clock[0])
        self.filter = self.module.Filter()
        self.filter.valves.lumberroom_token = "token"

    def inlet(self, body: dict, user: str = "u1") -> dict:
        return asyncio.run(self.filter.inlet(body, {"id": user}))

    def test_the_tools_call_carries_the_latest_request_as_query(self):
        self.inlet(chat(DEPLOY))
        self.assertEqual(self.fake.arguments(), [{"query": DEPLOY}])

    def test_two_messages_fetch_twice_and_each_gets_its_own_digest(self):
        first = self.inlet(chat(DEPLOY))
        second = self.inlet(chat(DEPLOY, BACKUPS))
        self.assertEqual(self.fake.arguments(), [{"query": DEPLOY}, {"query": BACKUPS}])
        self.assertIn(f"digest for {DEPLOY}", injected(first))
        self.assertIn(f"digest for {BACKUPS}", injected(second))
        self.assertNotIn(f"digest for {DEPLOY}", injected(second))

    def test_a_repeated_request_inside_the_window_reuses_the_digest(self):
        self.inlet(chat(DEPLOY))
        self.clock[0] += 10
        self.inlet(chat(DEPLOY))
        self.assertEqual(len(self.fake.arguments()), 1)

    def test_the_send_query_valve_off_sends_empty_arguments(self):
        self.filter.valves.send_query = False
        self.inlet(chat(DEPLOY))
        self.assertEqual(self.fake.arguments(), [{}])

    def test_short_replies_alone_send_empty_arguments(self):
        self.inlet(chat("hi", "ok"))
        self.assertEqual(self.fake.arguments(), [{}])

    def test_an_outage_on_a_new_request_serves_the_users_last_digest(self):
        self.inlet(chat(DEPLOY))
        self.fake.down = True
        during = self.inlet(chat(DEPLOY, BACKUPS))
        self.assertIn(f"digest for {DEPLOY}", injected(during))
        other = self.inlet(chat(BACKUPS), user="u2")
        self.assertIsNone(injected(other))

    def test_expired_entries_leave_the_cache_when_a_new_one_lands(self):
        self.inlet(chat(DEPLOY))
        self.clock[0] += 100
        self.inlet(chat(BACKUPS))
        self.assertEqual(set(self.filter._cache), {("u1", BACKUPS)})

    def test_an_outage_with_nothing_cached_returns_the_body_unchanged(self):
        self.fake.down = True
        body = chat(DEPLOY)
        before = [dict(m) for m in body["messages"]]
        self.assertEqual(self.inlet(body)["messages"], before)


if __name__ == "__main__":
    unittest.main()
