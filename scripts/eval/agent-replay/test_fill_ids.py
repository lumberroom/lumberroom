from fill_ids import fill


def test_an_id_token_becomes_the_server_id_of_that_seed_key():
    line = '{"target_id": "@id:project-x-0001", "n": 1}'
    assert fill(line, {"project-x-0001": "uuid-1"}) == '{"target_id": "uuid-1", "n": 1}'


def test_an_unknown_seed_key_is_an_error_not_a_silent_blank():
    import pytest
    with pytest.raises(KeyError):
        fill('{"target_id": "@id:missing"}', {})
