import heapq
import json
import os
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .claude import AGENT, encode_path
from .format import FORMAT_VERSION, Derived, Turn, digest, digest_bytes
from .format import make_turn as build_turn
from .rewrite import (
    ENCODED_COMPONENT_BOUNDARY,
    PATH_BOUNDARY,
    UNGUARDED,
    compile_replacements,
)

CONVERTER = {"id": AGENT, "version": 1}

PLACEHOLDER_PREFIX = "{{loch:"
ESCAPE = "{{loch:esc}}"
ROOT = "{{loch:root}}"
HOME = "{{loch:home}}"
ROOT_SLUG = "{{loch:root-slug}}"
SESSION = "{{loch:session}}"

AGENT_AUTHOR = {"kind": "agent", "agent": AGENT}
TOOL_AUTHOR = {"kind": "tool", "agent": AGENT}
SYSTEM_AUTHOR = {"kind": "system", "agent": AGENT}

# Record fields that place a record (in time, on a machine, in Claude's own
# bookkeeping) rather than say anything. Excluded from the hashed payload of
# system and attachment turns; the identity of a user or assistant turn comes
# from message content alone, so this list doesn't reach those.
ENVELOPE_FIELDS = frozenset(
    {
        "type",
        "uuid",
        "parentUuid",
        "logicalParentUuid",
        "sessionId",
        "session_id",
        "timestamp",
        "cwd",
        "gitBranch",
        "version",
        "entrypoint",
        "userType",
        "slug",
        "isSidechain",
        "agentId",
        "requestId",
        "promptId",
    }
)


@dataclass(frozen=True)
class Origin:
    """Where the source lived when it was ingested. These are the only
    machine-specific strings a derived file carries, and they live in its
    header alone: every turn refers to them through placeholders."""

    cwd: str
    home: str
    session_id: str


def normalizer(origin: Origin) -> Callable[[str], str]:
    """Maps a string to its machine-neutral form, with the same single-pass,
    boundary-guarded machinery restore rewrites with — so a session copied
    elsewhere by `restore` normalizes back to the same bytes.

    The escape pair comes first in meaning though not in order: a literal
    `{{loch:` already in a transcript becomes `{{loch:esc}}`, which keeps
    every placeholder in the output unambiguous and the mapping reversible."""
    pairs = [
        (PLACEHOLDER_PREFIX, ESCAPE, UNGUARDED),
        (origin.cwd, ROOT, PATH_BOUNDARY),
        (encode_path(origin.cwd), ROOT_SLUG, ENCODED_COMPONENT_BOUNDARY),
        (origin.session_id, SESSION, UNGUARDED),
    ]
    # A session run from the home directory itself is a root, not a home.
    if origin.home != origin.cwd:
        pairs.append((origin.home, HOME, PATH_BOUNDARY))
    return compile_replacements(pairs)


def normalize_value(value: Any, text: Callable[[str], str]) -> Any:
    """Keys as well as values: a tool result can key a map by file path, and
    `restore` rewrites the text of the whole record, keys included."""
    if isinstance(value, str):
        return text(value)
    if isinstance(value, list):
        return [normalize_value(item, text) for item in value]
    if isinstance(value, dict):
        return {text(key): normalize_value(item, text) for key, item in value.items()}
    return value


def _blocks_from_content(content: Any) -> list[dict[str, Any]]:
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    if not isinstance(content, list):
        return []
    return [_block(block) for block in content]


def _block(block: Any) -> dict[str, Any]:
    if not isinstance(block, dict):
        return {"type": "x-claude-code:block", "block": block}
    kind = block.get("type")
    if kind == "text":
        return {"type": "text", "text": block.get("text", "")}
    if kind == "thinking":
        # The signature is Anthropic's attestation for resuming, not content,
        # and it lives on in the source.
        return {"type": "thinking", "text": block.get("thinking", "")}
    if kind == "redacted_thinking":
        return {"type": "redacted-thinking"}
    if kind == "tool_use":
        return {
            "type": "tool-call",
            "call": block.get("id"),
            "name": block.get("name"),
            "input": block.get("input"),
        }
    if kind == "tool_result":
        normalized: dict[str, Any] = {
            "type": "tool-result",
            "call": block.get("tool_use_id"),
            "content": _blocks_from_content(block.get("content")),
        }
        if block.get("is_error"):
            normalized["error"] = True
        return normalized
    if kind == "image":
        return {"type": "image", "source": block.get("source")}
    # Unknown block types pass through whole rather than being dropped, so a
    # later converter version can learn them without the source being re-read
    # by anyone who only has the derived file.
    return {"type": "x-claude-code:block", "block": block}


def _payload(record: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in record.items() if key not in ENVELOPE_FIELDS}


def classify(record: dict[str, Any]) -> tuple[str, dict[str, Any], list[Any]] | None:
    """(kind, author, blocks) for a chain record, or None for one that isn't
    part of the conversation graph. The human author is filled in by the
    caller, since only the ingest knows who it is."""
    kind = record.get("type")
    message = record.get("message")
    content = message.get("content") if isinstance(message, dict) else None

    if kind == "assistant":
        return "message", AGENT_AUTHOR, _blocks_from_content(content)
    if kind == "user":
        blocks = _blocks_from_content(content)
        if record.get("isCompactSummary") or record.get("isMeta"):
            return "context", SYSTEM_AUTHOR, blocks
        if any(block["type"] == "tool-result" for block in blocks):
            return "tool-result", TOOL_AUTHOR, blocks
        return "message", {"kind": "human"}, blocks
    if kind == "attachment":
        block = {"type": "x-claude-code:attachment", "attachment": _payload(record)}
        return "context", SYSTEM_AUTHOR, [block]
    if kind == "system":
        block = {"type": "x-claude-code:system", "system": _payload(record)}
        if record.get("subtype") == "compact_boundary":
            return "compaction", SYSTEM_AUTHOR, [block]
        return "event", SYSTEM_AUTHOR, [block]
    return None


def _parent_uuid(record: dict[str, Any]) -> str | None:
    """A compaction boundary has no parent by design and names the record it
    logically follows instead. Following that link is what keeps a compacted
    session one connected graph rather than a stack of disjoint roots."""
    parent = record.get("parentUuid")
    if parent is None:
        parent = record.get("logicalParentUuid")
    return parent if isinstance(parent, str) else None


def _parse(raw: str) -> list[tuple[int, dict[str, Any]]]:
    records: list[tuple[int, dict[str, Any]]] = []
    for number, line in enumerate(raw.split("\n"), start=1):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if isinstance(record, dict):
            records.append((number, record))
    return records


def _topological(
    chain: list[tuple[int, dict[str, Any]]],
) -> tuple[list[tuple[int, dict[str, Any]]], set[str]]:
    """File order wherever it already respects parentage, which for Claude's
    append-only files is everywhere. Parents that never appear are dangling
    (the record becomes a root); a cycle, which a well-formed file can't hold,
    is broken the same way rather than looping."""
    present = {record["uuid"] for _, record in chain}
    children: dict[str, list[int]] = {}
    waiting = [0] * len(chain)
    for index, (_, record) in enumerate(chain):
        parent = _parent_uuid(record)
        if parent is not None and parent in present and parent != record["uuid"]:
            children.setdefault(parent, []).append(index)
            waiting[index] = 1

    ready = [index for index, count in enumerate(waiting) if count == 0]
    heapq.heapify(ready)
    ordered: list[tuple[int, dict[str, Any]]] = []
    done = [False] * len(chain)
    while len(ordered) < len(chain):
        if not ready:
            stuck = next(index for index, finished in enumerate(done) if not finished)
            waiting[stuck] = 0
            heapq.heappush(ready, stuck)
        index = heapq.heappop(ready)
        if done[index]:
            continue
        done[index] = True
        ordered.append(chain[index])
        for child in children.get(chain[index][1]["uuid"], []):
            if not done[child]:
                waiting[child] -= 1
                if waiting[child] <= 0:
                    heapq.heappush(ready, child)
    return ordered, present


def _facts(
    number: int, record: dict[str, Any], text: Callable[[str], str]
) -> dict[str, Any]:
    facts: dict[str, Any] = {"line": number, "uuid": record["uuid"]}
    message = record.get("message")
    if isinstance(message, dict):
        for source, name in (("id", "messageId"), ("model", "model")):
            if isinstance(message.get(source), str):
                facts[name] = message[source]
    for name in ("timestamp", "gitBranch", "version", "subtype"):
        if isinstance(record.get(name), str):
            facts[name] = record[name]
    if isinstance(record.get("cwd"), str):
        facts["cwd"] = text(record["cwd"])
    facts["record"] = record.get("type")
    return facts


def convert(
    raw: str, origin: Origin, human: dict[str, Any]
) -> tuple[list[Turn], dict[str, Any], dict[str, Any]]:
    """Claude Code JSONL to loch turns, plus heads and session-level facts.

    One chain record is one turn, and its parents are its `parentUuid`'s turn.
    Assistant messages are therefore several turns when Claude split one API
    message across records (one per content block) — `facts.messageId` is what
    groups them back. Keeping records whole is what keeps Claude's own tree,
    including the branches a rewind leaves in the file, intact as the graph.

    Two records whose cores are identical are one turn. That happens when a
    rewind resubmits exactly what was said before from the same point, which
    is the same turn by any reasonable reading; the later uuids are kept in
    the first one's facts.
    """
    text = normalizer(origin)
    records = _parse(raw)
    chain = [
        (number, record)
        for number, record in records
        if isinstance(record.get("uuid"), str)
    ]
    ordered, present = _topological(chain)

    turn_of: dict[str, str] = {}
    turns: dict[str, Turn] = {}
    has_children: set[str] = set()
    last_turn: str | None = None

    for number, record in ordered:
        classified = classify(record)
        if classified is None:
            continue
        kind, author, blocks = classified
        if author.get("kind") == "human":
            author = human
        parent_uuid = _parent_uuid(record)
        facts = _facts(number, record, text)
        parents: list[str] = []
        if parent_uuid is not None:
            if parent_uuid in turn_of:
                parents = [turn_of[parent_uuid]]
            elif parent_uuid not in present:
                facts["danglingParent"] = parent_uuid

        turn = build_turn(kind, author, parents, normalize_value(blocks, text), facts)
        turn_of[record["uuid"]] = turn.id
        last_turn = turn.id
        if turn.id in turns:
            turns[turn.id].facts.setdefault("alsoUuids", []).append(record["uuid"])
        else:
            turns[turn.id] = turn
        has_children.update(parents)

    session: dict[str, Any] = {}
    current = last_turn
    for _, record in records:
        record_type = record.get("type")
        if record_type == "last-prompt" and record.get("leafUuid") in turn_of:
            current = turn_of[record["leafUuid"]]
        elif record_type == "ai-title" and isinstance(record.get("aiTitle"), str):
            session["aiTitle"] = record["aiTitle"]
        elif record_type == "custom-title" and isinstance(
            record.get("customTitle"), str
        ):
            session["title"] = record["customTitle"]

    heads: dict[str, Any] = {
        "leaves": [turn_id for turn_id in turns if turn_id not in has_children]
    }
    if current is not None:
        heads["current"] = current
    return list(turns.values()), heads, normalize_value(session, text)


def source_files(transcript_path: str) -> list[tuple[str, str]]:
    """The transcript and everything Claude files beside it for the session:
    subagent transcripts and spilled tool output under `<session-id>/`. All of
    it is the source, and a source that dropped the sidecar would silently
    lose every subagent conversation."""
    files = [("transcript.jsonl", transcript_path)]
    sidecar = transcript_path[: -len(".jsonl")]
    if os.path.isdir(sidecar):
        for directory, _, names in os.walk(sidecar):
            for name in names:
                path = os.path.join(directory, name)
                relative = os.path.relpath(path, sidecar).replace(os.sep, "/")
                files.append((f"sidecar/{relative}", path))
    return sorted(files)


def source_manifest(transcript_path: str) -> tuple[dict[str, Any], str]:
    """The content-addressed identity of the raw source: every file's hash,
    in path order. Returns the manifest and the transcript text."""
    entries = []
    transcript = ""
    for relative, path in source_files(transcript_path):
        data = Path(path).read_bytes()
        entries.append(
            {"path": relative, "sha256": digest_bytes(data), "bytes": len(data)}
        )
        if relative == "transcript.jsonl":
            transcript = data.decode("utf-8", "surrogateescape")
    manifest = {"agent": AGENT, "files": entries}
    return {"id": digest(manifest), **manifest}, transcript


def ingest(
    transcript_path: str,
    origin: Origin,
    author_id: str | None = None,
) -> Derived:
    human: dict[str, Any] = {"kind": "human"}
    # Omitted rather than null, for the same reason the bundle format omits
    # absent metadata: the encoding of "unknown" is part of every id.
    if author_id is not None:
        human["id"] = author_id

    source, transcript = source_manifest(transcript_path)
    turns, heads, session = convert(transcript, origin, human)
    header: dict[str, Any] = {
        "loch": FORMAT_VERSION,
        "converter": CONVERTER,
        "source": source,
        "origin": {
            "cwd": origin.cwd,
            "home": origin.home,
            "sessionId": origin.session_id,
        },
        "author": human,
    }
    if session:
        header["session"] = session
    return Derived(header=header, turns=turns, heads=heads)


def derived_name(derived: Derived) -> str:
    source = derived.header["source"]["id"].split(":", 1)[1]
    converter = derived.header["converter"]
    return f"{source}.{converter['id']}-v{converter['version']}.jsonl"
