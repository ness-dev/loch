import hashlib
import json
from collections.abc import Iterable, Iterator
from dataclasses import dataclass, field
from typing import Any

FORMAT_VERSION = 1

HASH_PREFIX = "sha256:"


class FormatError(ValueError):
    """A derived file that doesn't parse, names a format this build can't
    read, or carries an id its own contents don't hash to."""


def canonical(value: Any) -> bytes:
    """The one encoding every id is computed over. Sorted keys and compact
    separators make it independent of construction order. `surrogatepass`
    because a transcript can hold a lone surrogate (a `\\ud83d` cut from its
    pair), and refusing to hash it would drop a real record.

    Floats go through Python's shortest round-trip repr, which agrees with
    RFC 8785 for every value that has no exponent. See SPEC.md."""
    return json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
        allow_nan=False,
    ).encode("utf-8", "surrogatepass")


def digest(value: Any) -> str:
    return HASH_PREFIX + hashlib.sha256(canonical(value)).hexdigest()


def digest_bytes(data: bytes) -> str:
    return HASH_PREFIX + hashlib.sha256(data).hexdigest()


def hex_of(object_id: str) -> str:
    if not object_id.startswith(HASH_PREFIX):
        raise FormatError(f"not a {HASH_PREFIX} id: {object_id}")
    return object_id[len(HASH_PREFIX) :]


@dataclass(frozen=True)
class Turn:
    """One node of the conversation graph, in two halves.

    `core` is the hashed envelope — format version, kind, author, parents, and
    the *hash* of the content, never the content itself. That indirection is
    what lets content be deleted while every id that depends on it stays
    verifiable: a tombstoned turn still hashes to its id.

    `facts` are what the converter knew about the turn that is not part of its
    identity — timestamps, the source record's uuid and line, model, branch.
    They are regenerable and may differ between two derivations of the same
    turn without the turn being a different one."""

    id: str
    core: dict[str, Any]
    blocks: list[dict[str, Any]] | None
    facts: dict[str, Any] = field(default_factory=dict)


def content_id(blocks: list[dict[str, Any]]) -> str:
    return digest({"blocks": blocks})


def make_core(
    kind: str,
    author: dict[str, Any],
    parents: list[str],
    blocks: list[dict[str, Any]],
) -> dict[str, Any]:
    return {
        "loch": FORMAT_VERSION,
        "kind": kind,
        "author": author,
        "parents": parents,
        "content": content_id(blocks),
    }


def make_turn(
    kind: str,
    author: dict[str, Any],
    parents: list[str],
    blocks: list[dict[str, Any]],
    facts: dict[str, Any] | None = None,
) -> Turn:
    core = make_core(kind, author, parents, blocks)
    return Turn(id=digest(core), core=core, blocks=blocks, facts=facts or {})


@dataclass(frozen=True)
class Derived:
    """A converter's understanding of one source, at the time it ran. Throw it
    away and re-derive whenever the converter improves; the source is what
    has to survive."""

    header: dict[str, Any]
    turns: list[Turn]
    heads: dict[str, Any]


def derived_lines(derived: Derived) -> Iterator[bytes]:
    yield canonical({"type": "header", **derived.header})
    for turn in derived.turns:
        record: dict[str, Any] = {"type": "turn", "id": turn.id, "turn": turn.core}
        if turn.blocks is not None:
            record["content"] = {"blocks": turn.blocks}
        if turn.facts:
            record["facts"] = turn.facts
        yield canonical(record)
    yield canonical({"type": "heads", **derived.heads})


def pack_derived(derived: Derived) -> bytes:
    return b"".join(line + b"\n" for line in derived_lines(derived))


def verify_turn(turn: Turn) -> None:
    if digest(turn.core) != turn.id:
        raise FormatError(f"turn {turn.id} does not hash to its id")
    if turn.blocks is not None and content_id(turn.blocks) != turn.core["content"]:
        raise FormatError(f"turn {turn.id} content does not match its hash")


def read_derived(lines: Iterable[str]) -> Derived:
    """Parses and verifies. Every id is recomputed, so a file that reads back
    without raising is internally consistent, not merely well-formed."""
    header: dict[str, Any] | None = None
    heads: dict[str, Any] = {}
    turns: list[Turn] = []
    seen: set[str] = set()

    for number, line in enumerate(lines, start=1):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except ValueError as error:
            raise FormatError(f"line {number}: {error}") from error
        kind = record.pop("type", None)
        if kind == "header":
            if record.get("loch") != FORMAT_VERSION:
                raise FormatError(
                    f"unsupported loch format {record.get('loch')} "
                    f"(this build reads {FORMAT_VERSION})"
                )
            header = record
        elif kind == "turn":
            content = record.get("content")
            turn = Turn(
                id=record["id"],
                core=record["turn"],
                blocks=content["blocks"] if content is not None else None,
                facts=record.get("facts", {}),
            )
            verify_turn(turn)
            for parent in turn.core["parents"]:
                if parent not in seen:
                    raise FormatError(f"turn {turn.id} precedes its parent {parent}")
            seen.add(turn.id)
            turns.append(turn)
        elif kind == "heads":
            heads = record
        else:
            raise FormatError(f"line {number}: unknown record type {kind!r}")

    if header is None:
        raise FormatError("no header")
    return Derived(header=header, turns=turns, heads=heads)
