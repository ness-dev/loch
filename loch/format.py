import hashlib
import json
import re
from collections.abc import Iterable, Iterator
from dataclasses import dataclass, field
from decimal import Decimal
from typing import Any

FORMAT_VERSION = 1

HASH_PREFIX = "sha256:"


class FormatError(ValueError):
    """A derived file that doesn't parse, names a format this build can't
    read, or carries an id its own contents don't hash to."""


_LONE_SURROGATE = re.compile("[\ud800-\udfff]")


def _string(text: str) -> str:
    """JSON.stringify's escaping, which Python's matches except for a lone
    surrogate: Python writes it raw, which isn't valid UTF-8, where
    ECMAScript escapes it. Transcripts do hold them — a `\\ud83d` cut from its
    pair by truncation — so they are escaped rather than refused."""
    encoded = json.dumps(text, ensure_ascii=False)
    if _LONE_SURROGATE.search(encoded):
        encoded = _LONE_SURROGATE.sub(
            lambda match: f"\\u{ord(match.group(0)):04x}", encoded
        )
    return encoded


def _number(value: float) -> str:
    """ECMAScript Number-to-String, as RFC 8785 requires. Python's repr finds
    the same shortest round-trip digits but places the exponent differently
    (`1e+16` and `1e-05` where ECMAScript writes `10000000000000000` and
    `0.00001`), so only the digits are taken from it."""
    if value != value or value in (float("inf"), float("-inf")):
        raise ValueError(f"{value} has no JSON representation")
    if value == 0:
        return "0"
    sign = "-" if value < 0 else ""
    parts = Decimal(repr(abs(value))).normalize().as_tuple()
    digits = "".join(str(digit) for digit in parts.digits)
    exponent = parts.exponent
    assert isinstance(exponent, int)
    k = len(digits)
    n = k + exponent
    if k <= n <= 21:
        return sign + digits + "0" * (n - k)
    if 0 < n <= 21:
        return sign + digits[:n] + "." + digits[n:]
    if -6 < n <= 0:
        return sign + "0." + "0" * -n + digits
    mantissa = digits[0] + ("." + digits[1:] if k > 1 else "")
    power = n - 1
    return f"{sign}{mantissa}e{'+' if power >= 0 else '-'}{abs(power)}"


def _utf16_order(key: str) -> bytes:
    return key.encode("utf-16-be", "surrogatepass")


def _encode(value: Any, out: list[str]) -> None:
    if isinstance(value, str):
        out.append(_string(value))
    elif value is None:
        out.append("null")
    elif value is True:
        out.append("true")
    elif value is False:
        out.append("false")
    elif isinstance(value, int):
        out.append(str(value))
    elif isinstance(value, float):
        out.append(_number(value))
    elif isinstance(value, dict):
        out.append("{")
        for index, key in enumerate(sorted(value, key=_utf16_order)):
            if not isinstance(key, str):
                raise TypeError(f"object key {key!r} is not a string")
            if index:
                out.append(",")
            out.append(_string(key))
            out.append(":")
            _encode(value[key], out)
        out.append("}")
    elif isinstance(value, (list, tuple)):
        out.append("[")
        for index, item in enumerate(value):
            if index:
                out.append(",")
            _encode(item, out)
        out.append("]")
    else:
        raise TypeError(f"{type(value).__name__} is not JSON")


def canonical(value: Any) -> bytes:
    """RFC 8785 (JSON Canonicalization Scheme): the one encoding every id is
    computed over, so a second implementation in any language can reproduce
    an id byte for byte. Keys sort by UTF-16 code units, numbers print as
    ECMAScript does, and strings escape as JSON.stringify does.

    One deliberate departure: an integer prints exactly, at any size. RFC
    8785 treats every number as a double, which would silently round a
    63-bit id inside a tool result; exact is what the transcript said."""
    out: list[str] = []
    _encode(value, out)
    return "".join(out).encode("utf-8")


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
