import base64
import hashlib
import json
from pathlib import Path
from typing import Any

import pytest

from loch.format import pack_derived, read_derived
from loch.ingest import (
    ESCAPE,
    HOME,
    ROOT,
    ROOT_SLUG,
    SESSION,
    AuthorKeyError,
    Origin,
    convert,
    ingest,
    ssh_author_id,
)
from loch.rewrite import RewriteSpec, rewrite_transcript

HOME_DIR = "/Users/origin"
CWD = "/Users/origin/code/repo"
SESSION_ID = "11111111-1111-1111-1111-111111111111"
ORIGIN = Origin(CWD, HOME_DIR, SESSION_ID)
HUMAN = {"kind": "human"}


def record(uuid: str, parent: str | None, kind: str, **fields: Any) -> dict[str, Any]:
    base: dict[str, Any] = {
        "type": kind,
        "uuid": uuid,
        "parentUuid": parent,
        "sessionId": SESSION_ID,
        "cwd": CWD,
        "gitBranch": "main",
        "version": "2.0.0",
        "timestamp": f"2026-01-01T00:00:{uuid[-2:]}Z",
    }
    base.update(fields)
    return base


def user(uuid: str, parent: str | None, content: Any, **fields: Any) -> dict[str, Any]:
    return record(
        uuid, parent, "user", message={"role": "user", "content": content}, **fields
    )


def assistant(
    uuid: str, parent: str | None, blocks: list[Any], message_id: str = "msg_1"
) -> dict[str, Any]:
    message = {"id": message_id, "role": "assistant", "model": "m", "content": blocks}
    return record(uuid, parent, "assistant", message=message)


def jsonl(*records: dict[str, Any]) -> str:
    return "".join(json.dumps(r) + "\n" for r in records)


def session() -> str:
    """A prompt, an assistant message split across two records (thinking,
    then a tool call), the tool result, and a reply — the shape Claude Code
    writes for one tool-using exchange."""
    return jsonl(
        {"type": "custom-title", "customTitle": "demo", "sessionId": SESSION_ID},
        user("u-01", None, f"look at {CWD}/src/main.py"),
        assistant("a-02", "u-01", [{"type": "thinking", "thinking": "hmm"}]),
        assistant(
            "a-03",
            "a-02",
            [
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "Read",
                    "input": {"file_path": f"{CWD}/src/main.py"},
                }
            ],
        ),
        user(
            "u-04",
            "a-03",
            [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "print()"}],
            toolUseResult={"file": {"filePath": f"{CWD}/src/main.py"}},
        ),
        assistant("a-05", "u-04", [{"type": "text", "text": "done"}], "msg_2"),
        {"type": "last-prompt", "leafUuid": "a-05", "sessionId": SESSION_ID},
    )


def test_one_turn_per_chain_record_linked_by_parent() -> None:
    turns, heads, _ = convert(session(), ORIGIN, HUMAN)

    assert [turn.core["kind"] for turn in turns] == [
        "message",
        "message",
        "message",
        "tool-result",
        "message",
    ]
    for parent, child in zip(turns, turns[1:]):
        assert child.core["parents"] == [parent.id]
    assert turns[0].core["parents"] == []
    assert heads == {"current": turns[-1].id, "leaves": [turns[-1].id]}


def test_authors_follow_who_produced_the_record() -> None:
    turns, _, _ = convert(session(), ORIGIN, {"kind": "human", "id": "alice"})

    assert [turn.core["author"]["kind"] for turn in turns] == [
        "human",
        "agent",
        "agent",
        "tool",
        "agent",
    ]
    assert turns[0].core["author"] == {"kind": "human", "id": "alice"}


def test_split_assistant_message_is_regrouped_by_message_id_fact() -> None:
    turns, _, _ = convert(session(), ORIGIN, HUMAN)

    assert [turn.facts.get("messageId") for turn in turns] == [
        None,
        "msg_1",
        "msg_1",
        None,
        "msg_2",
    ]


def test_blocks_are_normalized_to_loch_shapes() -> None:
    turns, _, _ = convert(session(), ORIGIN, HUMAN)

    assert turns[1].blocks == [{"type": "thinking", "text": "hmm"}]
    assert turns[2].blocks == [
        {
            "type": "tool-call",
            "call": "toolu_1",
            "name": "Read",
            "input": {"file_path": f"{ROOT}/src/main.py"},
        }
    ]
    assert turns[3].blocks == [
        {
            "type": "tool-result",
            "call": "toolu_1",
            "content": [{"type": "text", "text": "print()"}],
        }
    ]


def test_machine_specific_strings_become_placeholders() -> None:
    raw = jsonl(
        user(
            "u-01",
            None,
            f"{CWD}/a {HOME_DIR}/.zshrc "
            f"{HOME_DIR}/.claude/projects/-Users-origin-code-repo/{SESSION_ID}.jsonl "
            "literal {{loch:root}}",
        )
    )
    turns, _, _ = convert(raw, ORIGIN, HUMAN)

    assert turns[0].blocks == [
        {
            "type": "text",
            "text": f"{ROOT}/a {HOME}/.zshrc "
            f"{HOME}/.claude/projects/{ROOT_SLUG}/{SESSION}.jsonl "
            f"literal {ESCAPE}root}}}}",
        }
    ]
    assert turns[0].facts["cwd"] == ROOT


def test_sibling_of_the_root_is_not_taken_for_it() -> None:
    raw = jsonl(user("u-01", None, f"{CWD}-worktrees/feature/x"))
    turns, _, _ = convert(raw, ORIGIN, HUMAN)

    assert turns[0].blocks == [
        {"type": "text", "text": f"{HOME}/code/repo-worktrees/feature/x"}
    ]


def test_turn_ids_survive_a_restore_to_another_machine() -> None:
    target = Origin(
        "/home/bob/src/repo", "/home/bob", "2" * 8 + "-2222-2222-2222-" + "2" * 12
    )
    spec = RewriteSpec(
        origin_cwd=CWD,
        origin_home=HOME_DIR,
        target_cwd=target.cwd,
        target_home=target.home,
        origin_session_id=SESSION_ID,
        target_session_id=target.session_id,
    )
    original = convert(session(), ORIGIN, HUMAN)
    copied = convert(rewrite_transcript(session(), spec), target, HUMAN)

    assert [t.id for t in copied[0]] == [t.id for t in original[0]]
    assert copied[1] == original[1]


def test_timestamps_and_record_uuids_are_facts_not_identity() -> None:
    renamed = session().replace("u-01", "x-99").replace("00:01Z", "09:09Z")
    original, _, _ = convert(session(), ORIGIN, HUMAN)
    changed, _, _ = convert(renamed, ORIGIN, HUMAN)

    assert [t.id for t in changed] == [t.id for t in original]
    assert changed[0].facts["uuid"] == "x-99"


def test_a_rewind_leaves_two_heads() -> None:
    raw = jsonl(
        user("u-01", None, "start"),
        assistant("a-02", "u-01", [{"type": "text", "text": "first try"}], "m1"),
        assistant("a-03", "u-01", [{"type": "text", "text": "second try"}], "m2"),
        {"type": "last-prompt", "leafUuid": "a-02", "sessionId": SESSION_ID},
    )
    turns, heads, _ = convert(raw, ORIGIN, HUMAN)

    assert heads["leaves"] == [turns[1].id, turns[2].id]
    assert heads["current"] == turns[1].id


def test_identical_resubmission_from_the_same_point_is_one_turn() -> None:
    raw = jsonl(
        user("u-01", None, "start"),
        user("u-02", "u-01", "again"),
        user("u-03", "u-01", "again"),
    )
    turns, _, _ = convert(raw, ORIGIN, HUMAN)

    assert len(turns) == 2
    assert turns[1].facts["alsoUuids"] == ["u-03"]


def test_compaction_boundary_follows_its_logical_parent() -> None:
    raw = jsonl(
        user("u-01", None, "long conversation"),
        record(
            "s-02",
            None,
            "system",
            subtype="compact_boundary",
            logicalParentUuid="u-01",
            content="Conversation compacted",
        ),
        user("u-03", "s-02", "summary of before", isCompactSummary=True),
    )
    turns, heads, _ = convert(raw, ORIGIN, HUMAN)

    assert [t.core["kind"] for t in turns] == ["message", "compaction", "context"]
    assert turns[1].core["parents"] == [turns[0].id]
    assert heads["leaves"] == [turns[2].id]


def test_dangling_parent_becomes_a_root_and_says_so() -> None:
    raw = jsonl(user("u-02", "u-gone", "orphan"))
    turns, _, _ = convert(raw, ORIGIN, HUMAN)

    assert turns[0].core["parents"] == []
    assert turns[0].facts["danglingParent"] == "u-gone"


def test_a_child_written_before_its_parent_still_links() -> None:
    raw = jsonl(user("u-02", "u-01", "child"), user("u-01", None, "parent"))
    turns, _, _ = convert(raw, ORIGIN, HUMAN)

    assert turns[0].facts["uuid"] == "u-01"
    assert turns[1].core["parents"] == [turns[0].id]


def test_unknown_blocks_and_records_pass_through_or_stay_out() -> None:
    raw = jsonl(
        {"type": "cost-state", "totalCostUSD": 1.0, "sessionId": SESSION_ID},
        assistant("a-01", None, [{"type": "future_block", "x": 1}]),
    )
    turns, _, _ = convert(raw, ORIGIN, HUMAN)

    assert len(turns) == 1
    assert turns[0].blocks == [
        {"type": "x-claude-code:block", "block": {"type": "future_block", "x": 1}}
    ]


def test_attachment_payload_excludes_the_record_envelope() -> None:
    raw = jsonl(
        record("t-01", None, "attachment", attachment={"type": "todo", "items": []})
    )
    turns, _, _ = convert(raw, ORIGIN, HUMAN)

    assert turns[0].core["kind"] == "context"
    assert turns[0].blocks == [
        {
            "type": "x-claude-code:attachment",
            "attachment": {"attachment": {"type": "todo", "items": []}},
        }
    ]


def write(tmp_path: Path, raw: str) -> Path:
    path = tmp_path / f"{SESSION_ID}.jsonl"
    path.write_text(raw, encoding="utf-8")
    return path


def test_source_manifest_hashes_the_transcript_and_its_sidecar(tmp_path: Path) -> None:
    path = write(tmp_path, session())
    sidecar = tmp_path / SESSION_ID / "subagents"
    sidecar.mkdir(parents=True)
    (sidecar / "agent-a.jsonl").write_text("{}\n", encoding="utf-8")

    derived = ingest(str(path), ORIGIN)

    files = derived.header["source"]["files"]
    assert [f["path"] for f in files] == [
        "sidecar/subagents/agent-a.jsonl",
        "transcript.jsonl",
    ]
    assert derived.header["source"]["id"].startswith("sha256:")
    assert derived.header["session"] == {"title": "demo"}


def test_ingest_is_idempotent_and_reads_back_verified(tmp_path: Path) -> None:
    path = write(tmp_path, session())

    first = pack_derived(ingest(str(path), ORIGIN, "alice"))
    second = pack_derived(ingest(str(path), ORIGIN, "alice"))

    assert first == second
    assert pack_derived(read_derived(first.decode().split("\n"))) == first


def test_appending_to_a_session_keeps_every_earlier_turn_id(tmp_path: Path) -> None:
    before = convert(session(), ORIGIN, HUMAN)[0]
    grown = session() + jsonl(user("u-06", "a-05", "and then?"))
    after = convert(grown, ORIGIN, HUMAN)[0]

    assert [t.id for t in after[: len(before)]] == [t.id for t in before]


def test_events_fold_into_the_turn_they_follow() -> None:
    raw = jsonl(
        user("u-01", None, "start"),
        record("s-02", "u-01", "system", subtype="stop_hook_summary", hookCount=1),
        assistant("a-03", "s-02", [{"type": "text", "text": "reply"}]),
        {"type": "last-prompt", "leafUuid": "s-02", "sessionId": SESSION_ID},
    )
    turns, heads, _ = convert(raw, ORIGIN, HUMAN)

    assert [t.core["kind"] for t in turns] == ["message", "message"]
    assert turns[1].core["parents"] == [turns[0].id]
    [event] = turns[0].facts["events"]
    assert event["type"] == "x-claude-code:system"
    assert event["system"] == {"subtype": "stop_hook_summary", "hookCount": 1}
    assert heads["current"] == turns[0].id


def test_events_do_not_change_the_ids_around_them() -> None:
    plain = jsonl(
        user("u-01", None, "start"),
        assistant("a-03", "u-01", [{"type": "text", "text": "reply"}]),
    )
    with_event = jsonl(
        user("u-01", None, "start"),
        record("s-02", "u-01", "system", subtype="turn_duration", durationMs=5),
        assistant("a-03", "s-02", [{"type": "text", "text": "reply"}]),
    )

    assert [t.id for t in convert(with_event, ORIGIN, HUMAN)[0]] == [
        t.id for t in convert(plain, ORIGIN, HUMAN)[0]
    ]


def test_an_event_with_no_turn_before_it_is_kept_at_session_level() -> None:
    raw = jsonl(
        record("s-01", None, "system", subtype="informational", content="hi"),
        user("u-02", "s-01", "start"),
    )
    turns, _, session = convert(raw, ORIGIN, HUMAN)

    assert len(turns) == 1
    assert session["events"][0]["system"]["subtype"] == "informational"


def test_ssh_author_id_is_the_ssh_keygen_fingerprint_of_the_key_blob() -> None:
    """ssh-keygen -l hashes the decoded key blob with SHA-256 and prints it as
    unpadded base64; checked against ssh-keygen by hand on a real key."""
    blob = b"\x00\x00\x00\x0bssh-ed25519" + bytes(range(32))
    line = "ssh-ed25519 " + base64.b64encode(blob).decode() + " me@host"
    expected = base64.b64encode(hashlib.sha256(blob).digest()).decode().rstrip("=")

    assert ssh_author_id(line) == f"ssh:SHA256:{expected}"
    assert ssh_author_id(line.rsplit(" ", 1)[0]) == ssh_author_id(line)


def test_ssh_author_id_rejects_something_that_is_not_a_key() -> None:
    with pytest.raises(AuthorKeyError):
        ssh_author_id("not-a-key")
