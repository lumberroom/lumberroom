import json
from pathlib import Path

from probe import quotes_row, loaded_user_instructions, verdict

ROW = "The harbor-api service listens on port 9100 behind nginx on host quay-01 and restarts under systemd."


def test_a_verbatim_run_of_six_words_counts_as_a_quote():
    assert quotes_row('The row says: "listens on port 9100 behind nginx on host quay-01".', [ROW])


def test_a_paraphrase_or_a_no_text_answer_is_not_a_quote():
    assert not quotes_row("NO TEXT: the answer carried only keys and counts.", [ROW])
    assert not quotes_row("harbor-api is served on 9100 through nginx", [ROW])


def test_user_instructions_are_detected_by_a_distinctive_line_of_the_owner_file():
    owner = "# Global instructions\nWhen started up in a project, check the CLAUDE.md file for that project.\n"
    assert loaded_user_instructions("I was told: When started up in a project, check the CLAUDE.md file for that project.", owner)
    assert not loaded_user_instructions("I was given only the task text.", owner)


def test_the_probe_passes_when_structured_cannot_quote_and_text_can():
    assert verdict(structured_quotes=False, text_quotes=True)["pass"]
    v = verdict(structured_quotes=True, text_quotes=True)
    assert not v["pass"] and "structured" in v["reason"]
    v = verdict(structured_quotes=False, text_quotes=False)
    assert not v["pass"] and "proxy" in v["reason"]
