"""
Regression tests for the single-owner warning and the write rule in openwebui-filter.py.

httpx and pydantic (the filter's two runtime dependencies, both supplied by OpenWebUI's own
backend) are not guaranteed to be installed wherever this repo is checked out, so this test reads
the file as text rather than importing the Filter class. What it guards is documentation staying
in place, not runtime behaviour: the filter has exactly one token valve shared by every account it
runs for, and the warning is the only thing standing between an operator and enabling it on a
multi-account instance.

Run: python3 client/openwebui-filter_test.py
"""

import ast
import pathlib
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

if __name__ == "__main__":
    unittest.main()
