import json
import os
import subprocess
from pathlib import Path
from typing import Any

import pytest

from loch.claude import transcript_path_for
from loch.cli import main
from loch.store import FileStore
from loch.trace import (
    Resolver,
    backfill,
    fork_at_call,
    hook_post_tool,
    hook_pre_tool,
    install_hooks,
    read_note,
    snapshot_transcript,
)

SESSION = "11111111-2222-3333-4444-555555555555"
CALL = "toolu_commit"


@pytest.fixture
def env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> dict[str, Path]:
    home = tmp_path / "home"
    repo = tmp_path / "repo"
    home.mkdir()
    repo.mkdir()
    monkeypatch.setenv("HOME", str(home))
    for name in ("AUTHOR", "COMMITTER"):
        monkeypatch.setenv(f"GIT_{name}_NAME", "Test")
        monkeypatch.setenv(f"GIT_{name}_EMAIL", "test@example.com")
    monkeypatch.setenv("GIT_CONFIG_GLOBAL", str(tmp_path / "gitconfig"))
    run(repo, "init", "-q", "-b", "main")
    return {"home": home, "repo": repo, "tmp": tmp_path}


def run(repo: Path, *args: str, date: str | None = None) -> str:
    extra = {"GIT_AUTHOR_DATE": date, "GIT_COMMITTER_DATE": date} if date else {}
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        check=True,
        env={**os.environ, **extra},
    ).stdout.strip()


def commit(
    repo: Path, name: str, text: str, message: str, date: str | None = None
) -> str:
    (repo / name).write_text(text)
    run(repo, "add", name)
    run(repo, "commit", "-q", "-m", message, date=date)
    return run(repo, "rev-parse", "HEAD")


def transcript(repo: Path, command: str, prompt: str = "please add the file") -> str:
    """human prompt -> assistant Bash call -> its result, as Claude writes it."""
    records: list[dict[str, Any]] = [
        {
            "type": "user",
            "uuid": "u1",
            "parentUuid": None,
            "sessionId": SESSION,
            "cwd": str(repo),
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {"role": "user", "content": prompt},
        },
        {
            "type": "assistant",
            "uuid": "a1",
            "parentUuid": "u1",
            "sessionId": SESSION,
            "cwd": str(repo),
            "timestamp": "2026-01-01T00:00:10.000Z",
            "message": {
                "id": "msg1",
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": CALL,
                        "name": "Bash",
                        "input": {"command": command},
                    }
                ],
            },
        },
        {
            "type": "user",
            "uuid": "r1",
            "parentUuid": "a1",
            "sessionId": SESSION,
            "cwd": str(repo),
            "timestamp": "2026-01-01T00:00:20.000Z",
            "message": {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": CALL, "content": "ok"}
                ],
            },
        },
    ]
    path = transcript_path_for(str(repo), SESSION)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    Path(path).write_text("".join(json.dumps(record) + "\n" for record in records))
    return path


def event(repo: Path, **fields: Any) -> dict[str, Any]:
    return {
        "session_id": SESSION,
        "cwd": str(repo),
        "tool_name": "Bash",
        "tool_use_id": CALL,
        "tool_input": {"command": "git commit"},
        **fields,
    }


def test_hooks_note_exactly_the_commits_a_call_made(env: dict[str, Path]) -> None:
    repo = env["repo"]
    old = commit(repo, "a.txt", "a\n", "old work", date="2020-01-01T00:00:00Z")
    run(repo, "checkout", "-q", "-b", "side")
    other = commit(repo, "b.txt", "b\n", "someone else", date="2020-01-02T00:00:00Z")
    run(repo, "checkout", "-q", "main")

    hook_pre_tool(event(repo))
    made = commit(repo, "c.txt", "c\n", "agent work")
    assert hook_post_tool(event(repo)) == [made]
    assert read_note(str(repo), made) == {
        "loch": 1,
        "agent": "claude-code",
        "session": SESSION,
        "call": CALL,
        "captured": "hook",
    }

    # Moving HEAD onto commits someone else made is not making them.
    hook_pre_tool(event(repo, tool_use_id="toolu_checkout"))
    run(repo, "checkout", "-q", "side")
    assert hook_post_tool(event(repo, tool_use_id="toolu_checkout")) == []
    assert read_note(str(repo), other) is None
    assert read_note(str(repo), old) is None


def test_a_call_that_leaves_head_alone_notes_nothing(env: dict[str, Path]) -> None:
    repo = env["repo"]
    commit(repo, "a.txt", "a\n", "first")
    hook_pre_tool(event(repo))
    assert hook_post_tool(event(repo)) == []


def test_hook_command_never_fails(
    env: dict[str, Path],
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    import io

    monkeypatch.setattr("sys.stdin", io.StringIO("not json"))
    monkeypatch.setattr("sys.argv", ["loch", "hook", "post-tool"])
    main()
    assert capsys.readouterr().out == ""


def test_install_is_idempotent_and_keeps_existing_settings(tmp_path: Path) -> None:
    settings = tmp_path / ".claude" / "settings.json"
    settings.parent.mkdir()
    settings.write_text(json.dumps({"model": "x", "hooks": {"Stop": [{"hooks": []}]}}))
    assert install_hooks(str(settings), "loch") == [
        "PreToolUse",
        "PostToolUse",
        "Stop",
    ]
    assert install_hooks(str(settings), "loch") == []
    written = json.loads(settings.read_text())
    assert written["model"] == "x"
    assert written["hooks"]["PreToolUse"][0]["matcher"] == "Bash"
    assert len(written["hooks"]["Stop"]) == 2


def test_stop_snapshots_the_transcript(env: dict[str, Path]) -> None:
    path = transcript(env["repo"], "git commit -m 'x'")
    store = FileStore(str(env["tmp"] / "store"))
    key = snapshot_transcript({"session_id": SESSION, "transcript_path": path}, store)
    assert key is not None
    assert store.get(key) == Path(path).read_bytes()


def test_a_deleted_transcript_is_found_in_the_store(env: dict[str, Path]) -> None:
    repo = env["repo"]
    path = transcript(repo, "git commit -m 'add a'")
    hook_pre_tool(event(repo))
    sha = commit(repo, "a.txt", "a\n", "add a")
    hook_post_tool(event(repo))
    store = FileStore(str(env["tmp"] / "store"))
    snapshot_transcript({"session_id": SESSION, "transcript_path": path}, store)
    os.remove(path)

    link = Resolver(str(repo), store).resolve(sha)
    assert link is not None and link.transcript is not None
    assert link.prompt == "please add the file"


def test_backfill_links_only_an_exact_match(env: dict[str, Path]) -> None:
    repo = env["repo"]
    # The transcript's call ran 00:00:10-00:00:20; these straddle it.
    inside = commit(repo, "a.txt", "a\n", "add a", date="2026-01-01T00:00:15Z")
    outside = commit(repo, "b.txt", "b\n", "add a", date="2026-01-01T05:00:00Z")
    unnamed = commit(repo, "c.txt", "c\n", "add c", date="2026-01-01T00:00:15Z")
    transcript(repo, 'git add a.txt && git commit -m "add a"')

    linked = backfill(str(repo), "HEAD")
    assert [sha for sha, _ in linked] == [inside]
    note = read_note(str(repo), inside)
    assert note is not None and note["captured"] == "backfill"
    assert read_note(str(repo), outside) is None
    assert read_note(str(repo), unnamed) is None
    assert backfill(str(repo), "HEAD") == []


def test_blame_names_the_prompt_behind_each_line(
    env: dict[str, Path],
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    repo = env["repo"]
    commit(repo, "f.txt", "human line\n", "by hand")
    transcript(repo, "git commit -am 'agent'", prompt="make it two lines")
    hook_pre_tool(event(repo))
    sha = commit(repo, "f.txt", "human line\nagent line\n", "agent")
    hook_post_tool(event(repo))

    monkeypatch.chdir(repo)
    monkeypatch.setattr("sys.argv", ["loch", "blame", "f.txt", "--json"])
    main()
    hunks = json.loads(capsys.readouterr().out)["hunks"]
    assert [(h["start"], h["conversation"] is None) for h in hunks] == [
        (1, True),
        (2, False),
    ]
    assert hunks[1]["commit"] == sha
    assert hunks[1]["conversation"]["prompt"] == "make it two lines"
    assert hunks[1]["conversation"]["turn"].startswith("sha256:")


def test_fork_cuts_after_the_whole_round_and_leaves_the_original(
    env: dict[str, Path],
) -> None:
    repo = env["repo"]
    path = transcript(repo, "git commit")
    records = [json.loads(line) for line in Path(path).read_text().splitlines()]
    # A parallel second call in the same message, with its result after ours,
    # then the conversation carrying on past the commit.
    second = json.loads(json.dumps(records[1]))
    second.update(uuid="a2", parentUuid="a1")
    second["message"]["content"][0]["id"] = "toolu_other"
    result_one = dict(records[2], parentUuid="a2")
    result_two = json.loads(json.dumps(records[2]))
    result_two.update(uuid="r2", parentUuid="r1")
    result_two["message"]["content"][0]["tool_use_id"] = "toolu_other"
    later = dict(records[0], uuid="u2", parentUuid="r2", message={"content": "more"})
    lines = [records[0], records[1], second, result_one, result_two, later]
    original = "".join(json.dumps(record) + "\n" for record in lines)
    Path(path).write_text(original)

    target = env["tmp"] / "elsewhere"
    target.mkdir()
    fork = fork_at_call(path, CALL, str(target))

    assert Path(path).read_text() == original
    forked = [json.loads(line) for line in Path(fork.path).read_text().splitlines()]
    assert [r.get("uuid") for r in forked] == ["u1", "a1", "a2", "r1", "r2", None]
    assert {r["sessionId"] for r in forked} == {fork.session_id}
    assert {r["cwd"] for r in forked if "cwd" in r} == {str(target)}
    assert forked[-1]["type"] == "custom-title"
    assert fork.path == transcript_path_for(str(target), fork.session_id)
