import pytest

from loch.format import (
    Derived,
    FormatError,
    Turn,
    canonical,
    content_id,
    digest,
    make_turn,
    pack_derived,
    read_derived,
)

HUMAN = {"kind": "human", "id": "alice"}


def header() -> dict[str, object]:
    return {"loch": 1, "converter": {"id": "test", "version": 1}}


def test_canonical_is_independent_of_key_order() -> None:
    assert canonical({"b": 1, "a": [1, {"d": 2, "c": 3}]}) == canonical(
        {"a": [1, {"c": 3, "d": 2}], "b": 1}
    )


def test_canonical_is_compact_and_keeps_non_ascii() -> None:
    assert canonical({"a": "é", "b": [1, 2]}) == '{"a":"é","b":[1,2]}'.encode()


def test_canonical_hashes_a_lone_surrogate_rather_than_failing() -> None:
    assert digest({"text": "\ud83d"}).startswith("sha256:")


def test_canonical_refuses_nan() -> None:
    with pytest.raises(ValueError):
        canonical({"x": float("nan")})


def test_turn_id_covers_the_content_hash_not_the_content() -> None:
    turn = make_turn("message", HUMAN, [], [{"type": "text", "text": "hi"}])

    assert turn.core["content"] == content_id([{"type": "text", "text": "hi"}])
    assert "blocks" not in turn.core
    assert turn.id == digest(turn.core)


def test_facts_do_not_change_identity() -> None:
    blocks = [{"type": "text", "text": "hi"}]
    plain = make_turn("message", HUMAN, [], blocks)
    annotated = make_turn("message", HUMAN, [], blocks, {"timestamp": "2026"})

    assert plain.id == annotated.id


def test_parents_and_author_change_identity() -> None:
    blocks = [{"type": "text", "text": "hi"}]
    root = make_turn("message", HUMAN, [], blocks)

    assert make_turn("message", HUMAN, [root.id], blocks).id != root.id
    assert make_turn("message", {"kind": "human"}, [], blocks).id != root.id


def chain() -> Derived:
    first = make_turn("message", HUMAN, [], [{"type": "text", "text": "a"}])
    second = make_turn("message", HUMAN, [first.id], [{"type": "text", "text": "b"}])
    return Derived(
        header=header(),
        turns=[first, second],
        heads={"current": second.id, "leaves": [second.id]},
    )


def test_round_trips_byte_for_byte() -> None:
    packed = pack_derived(chain())
    assert pack_derived(read_derived(packed.decode().split("\n"))) == packed


def test_reads_a_tombstoned_turn_and_still_verifies_its_id() -> None:
    derived = chain()
    tombstoned = Turn(
        id=derived.turns[0].id, core=derived.turns[0].core, blocks=None, facts={}
    )
    packed = pack_derived(
        Derived(derived.header, [tombstoned, derived.turns[1]], derived.heads)
    )

    back = read_derived(packed.decode().split("\n"))

    assert back.turns[0].blocks is None
    assert back.turns[0].id == derived.turns[0].id


def test_rejects_content_that_does_not_match_its_hash() -> None:
    derived = chain()
    forged = Turn(
        id=derived.turns[0].id,
        core=derived.turns[0].core,
        blocks=[{"type": "text", "text": "forged"}],
    )
    packed = pack_derived(Derived(derived.header, [forged], {}))

    with pytest.raises(FormatError, match="content does not match"):
        read_derived(packed.decode().split("\n"))


def test_rejects_an_id_that_does_not_match_its_core() -> None:
    derived = chain()
    turn = derived.turns[0]
    forged = Turn(id=digest({"other": 1}), core=turn.core, blocks=turn.blocks)
    packed = pack_derived(Derived(derived.header, [forged], {}))

    with pytest.raises(FormatError, match="does not hash to its id"):
        read_derived(packed.decode().split("\n"))


def test_rejects_a_turn_before_its_parent() -> None:
    derived = chain()
    packed = pack_derived(
        Derived(derived.header, list(reversed(derived.turns)), derived.heads)
    )

    with pytest.raises(FormatError, match="precedes its parent"):
        read_derived(packed.decode().split("\n"))


def test_rejects_an_unknown_format_version() -> None:
    with pytest.raises(FormatError, match="unsupported loch format 2"):
        read_derived(['{"type":"header","loch":2}'])
