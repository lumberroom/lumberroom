from judge import build_judge_prompt, parse_reply


def test_the_prompt_holds_the_task_every_reference_answer_and_every_answer_under_test():
    p = build_judge_prompt("Which port?", ["ref one", "ref two"], ["test one", "test two", "test three"])
    for s in ("Which port?", "ref one", "ref two", "test one", "test two", "test three"):
        assert s in p
    assert "equivalent" in p and "one line" in p


def test_a_reply_with_one_flag_per_answer_parses():
    got = parse_reply('{"equivalent": [true, false], "reasons": ["same", "port differs"]}', 2)
    assert got == {"equivalent": [True, False], "reasons": ["same", "port differs"]}


def test_prose_before_the_json_line_is_tolerated():
    got = parse_reply('Here is my verdict.\n{"equivalent": [true], "reasons": ["same"]}', 1)
    assert got["equivalent"] == [True]


def test_a_reply_with_the_wrong_count_does_not_parse():
    assert parse_reply('{"equivalent": [true], "reasons": ["x"]}', 2) is None


def test_a_reply_with_no_json_does_not_parse():
    assert parse_reply("they look the same to me", 1) is None
