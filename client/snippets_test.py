"""
Guards the sentence each rules file and note carries about context_bootstrap's `query` argument,
and the connector snippet's length.

The snippets end up in rules files and connector settings a model reads, and the MCP directory's
review rejects text that orders a model about. tests/it/mcp_tool_annotations.rs keeps that
denylist for the tool descriptions; this test reads it from there and applies it to the sentences
that describe `query`. The rest of each snippet is the owner's own rule text and keeps its
imperatives.

docs/connect-claude-ai.md tells a reader the connector snippet fits every paste field it lists,
and gives the ceiling and ChatGPT's cap as numbers. The length test reads both from that page, so
a longer snippet fails here before the page goes false.

Run: python3 -I client/snippets_test.py
"""

import pathlib
import re
import unittest

HERE = pathlib.Path(__file__).parent
REPO = HERE.parent
FILES = [
    "connector-instructions.md.snippet",
    "AGENTS.md.snippet",
    "CLAUDE.md.snippet",
    "hermes-notes.md",
    "skills/lumberroom-memory/SKILL.md",
]


def orders() -> list[str]:
    source = (REPO / "tests" / "it" / "mcp_tool_annotations.rs").read_text()
    body = source.split("const ORDERS", 1)[1].split("];", 1)[0]
    phrases = re.findall(r'"([^"]+)"', body)
    if not phrases:
        raise AssertionError("ORDERS is gone from tests/it/mcp_tool_annotations.rs")
    return phrases


def query_sentences(text: str) -> list[str]:
    flat = " ".join(text.split())
    return [s for s in re.split(r"(?<=[.!?])\s+", flat) if "`query`" in s]


def stated_number(page: str, pattern: str) -> int:
    found = re.search(pattern, page)
    if not found:
        raise AssertionError(f"docs/connect-claude-ai.md no longer states {pattern!r}")
    return int(found.group(1).replace(",", ""))


class QueryArgumentIsDescribed(unittest.TestCase):
    def test_every_file_describes_the_query_argument(self):
        for name in FILES:
            with self.subTest(name):
                self.assertTrue(query_sentences((HERE / name).read_text()), name)

    def test_no_query_sentence_carries_an_order_phrase(self):
        phrases = orders()
        for name in FILES:
            for sentence in query_sentences((HERE / name).read_text()):
                for phrase in phrases:
                    self.assertNotIn(phrase, sentence.lower(), (name, sentence))

    def test_the_snippets_carry_no_em_dash(self):
        for path in sorted(HERE.glob("*.snippet")):
            self.assertNotIn("\u2014", path.read_text(), path.name)

    def test_the_connector_snippet_fits_the_length_its_doc_states(self):
        page = (REPO / "docs" / "connect-claude-ai.md").read_text()
        ceiling = stated_number(page, r"under ([\d,]+) characters")
        cap = stated_number(page, r"cap a field at ([\d,]+) characters")
        length = len((HERE / "connector-instructions.md.snippet").read_text())
        self.assertLess(length, ceiling)
        self.assertLessEqual(ceiling, cap)


if __name__ == "__main__":
    unittest.main()
