"""Which conversation made which commit, and getting back into it.

A commit is linked to the turn that made it by a git note on `refs/notes/loch`:
a small JSON document naming the agent session and the tool call that ran
`git commit`. It holds no transcript text, so it travels with the repository
like any other envelope (SPEC.md §9, §10). The transcript it points at lives in
Claude's project directory or, once a hook has snapshotted it, in the store.

Links are captured live, by Claude Code hooks around every Bash call, because
recovering them afterwards is unreliable: `git commit -q` prints no hash, and
commits made from a script never appear in any tool output. `backfill` exists
for history made before the hooks, and accepts only an exact match.
"""

import hashlib
import json
import os
import subprocess
import time
import uuid
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

from .claude import (
    AGENT,
    _display_summary,
    home_dir,
    projects_dir,
    read_session_meta,
    transcript_cwds,
    transcript_path_for,
)
from .format import Turn
from .ingest import Origin, ingest
from .rewrite import RewriteSpec, rewrite_transcript
from .store import MissingObject, Store

NOTES_REF = "refs/notes/loch"

NOTE_VERSION = 1

TRANSCRIPTS_PREFIX = f"transcripts/{AGENT}/"

UNCOMMITTED = "0" * 40

# How far a commit's committer time may sit outside the tool call that made it.
# Git records whole seconds and the transcript milliseconds, so one second of
# slack covers truncation; the rest covers a slow hook process.
CLOCK_SLACK = 2.0


class TraceError(Exception):
    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


def git(cwd: str, *args: str, check: bool = True, stdin: str | None = None) -> str:
    result = subprocess.run(
        ["git", "-C", cwd, *args],
        capture_output=True,
        text=True,
        check=False,
        input=stdin,
    )
    if check and result.returncode != 0:
        raise TraceError("git-failed", result.stderr.strip() or "git failed")
    return result.stdout


def repo_root(cwd: str) -> str:
    try:
        return git(cwd, "rev-parse", "--show-toplevel").strip()
    except (TraceError, OSError) as error:
        raise TraceError("not-a-repository", f"{cwd} is not in a git repository") from (
            error
        )


def head(cwd: str) -> str | None:
    return git(cwd, "rev-parse", "--verify", "-q", "HEAD", check=False).strip() or None


# -- notes ------------------------------------------------------------------


def make_note(session_id: str, call: str, captured: str) -> dict[str, Any]:
    return {
        "loch": NOTE_VERSION,
        "agent": AGENT,
        "session": session_id,
        "call": call,
        "captured": captured,
    }


def write_note(cwd: str, commit: str, note: dict[str, Any]) -> None:
    body = json.dumps(note, sort_keys=True, separators=(",", ":"))
    git(cwd, "notes", "--ref", NOTES_REF, "add", "-f", "-F", "-", commit, stdin=body)


def read_note(cwd: str, commit: str) -> dict[str, Any] | None:
    text = git(cwd, "notes", "--ref", NOTES_REF, "show", commit, check=False)
    try:
        note = json.loads(text)
    except ValueError:
        return None
    return note if isinstance(note, dict) and note.get("loch") == NOTE_VERSION else None


def noted_commits(cwd: str) -> set[str]:
    listing = git(cwd, "notes", "--ref", NOTES_REF, "list", check=False)
    return {line.split()[1] for line in listing.splitlines() if len(line.split()) == 2}


# -- hooks ------------------------------------------------------------------


def _pending_dir(cwd: str) -> str:
    git_dir = git(cwd, "rev-parse", "--absolute-git-dir").strip()
    return os.path.join(git_dir, "loch", "pending")


def _call_key(event: dict[str, Any]) -> str:
    call = event.get("tool_use_id")
    if isinstance(call, str) and call:
        return call
    command = str((event.get("tool_input") or {}).get("command", ""))
    return "cmd-" + hashlib.sha256(command.encode()).hexdigest()[:32]


def hook_pre_tool(event: dict[str, Any]) -> None:
    """Remembers HEAD before a Bash call, so the matching post hook can tell
    exactly which commits the call made rather than guessing from history."""
    cwd = str(event.get("cwd") or os.getcwd())
    try:
        before = head(cwd) or ""
        directory = _pending_dir(cwd)
    except (TraceError, OSError):
        return
    os.makedirs(directory, exist_ok=True)
    state = {"head": before, "at": time.time()}
    Path(directory, _call_key(event)).write_text(json.dumps(state))


def commits_made(cwd: str, before: str, after: str, since: float) -> list[str]:
    """Commits reachable from `after` and not from `before`, and committed
    after the call began. The time bound is what keeps a `git checkout` or a
    fast-forward pull — which move HEAD onto commits someone else made — from
    being attributed to this call."""
    span = f"{before}..{after}" if before else after
    listing = git(cwd, "log", "--format=%H %ct", span)
    made = []
    for line in listing.splitlines():
        sha, _, stamp = line.partition(" ")
        if stamp and int(stamp) >= int(since) - 1:
            made.append(sha)
    return made


def _call_from_transcript(path: str, command: str) -> str | None:
    """The tool call id, for a hook payload that didn't carry one: the last
    Bash call in the transcript that ran exactly this command."""
    found = None
    for record in _read_records(path):
        for block in _content(record):
            if (
                block.get("type") == "tool_use"
                and block.get("name") == "Bash"
                and (block.get("input") or {}).get("command") == command
            ):
                found = block.get("id")
    return found if isinstance(found, str) else None


def hook_post_tool(event: dict[str, Any]) -> list[str]:
    cwd = str(event.get("cwd") or os.getcwd())
    session_id = event.get("session_id")
    if not isinstance(session_id, str):
        return []
    try:
        state_path = Path(_pending_dir(cwd), _call_key(event))
        state = json.loads(state_path.read_text())
        state_path.unlink()
    except (TraceError, OSError, ValueError):
        return []
    after = head(cwd)
    if after is None or after == state["head"]:
        return []

    call = event.get("tool_use_id")
    if not isinstance(call, str) or not call:
        transcript = event.get("transcript_path")
        command = str((event.get("tool_input") or {}).get("command", ""))
        call = (
            _call_from_transcript(transcript, command)
            if isinstance(transcript, str)
            else None
        )
        if call is None:
            return []

    made = commits_made(cwd, state["head"], after, state["at"])
    for commit in made:
        write_note(cwd, commit, make_note(session_id, call, "hook"))
    return made


def snapshot_transcript(event: dict[str, Any], store: Store) -> str | None:
    """Copies the live transcript into the store. Claude Code deletes
    transcripts after `cleanupPeriodDays` (30 by default), which would leave
    every note pointing at nothing; this is the copy that outlives it. The
    file is append-only, so the latest snapshot covers every earlier one and
    one key per session suffices."""
    path = event.get("transcript_path")
    session_id = event.get("session_id")
    if not isinstance(path, str) or not isinstance(session_id, str):
        return None
    try:
        data = Path(path).read_bytes()
    except OSError:
        return None
    key = transcript_key(os.path.basename(os.path.dirname(path)), session_id)
    store.put(key, data)
    return key


def transcript_key(project: str, session_id: str) -> str:
    return f"{TRANSCRIPTS_PREFIX}{project}/{session_id}.jsonl"


HOOK_EVENTS = {
    "PreToolUse": ("Bash", "pre-tool"),
    "PostToolUse": ("Bash", "post-tool"),
    "Stop": (None, "stop"),
}


def install_hooks(settings_path: str, command: str) -> list[str]:
    """Adds loch's hooks to a Claude Code settings file, keeping whatever else
    is there. Returns the events it added; re-running adds nothing."""
    try:
        settings = json.loads(Path(settings_path).read_text())
    except FileNotFoundError:
        settings = {}
    hooks = settings.setdefault("hooks", {})
    added = []
    for event, (matcher, name) in HOOK_EVENTS.items():
        # A checkout whose machine lacks loch gets a silent no-op rather than
        # an error on every Bash call, since the settings file is committed.
        line = f"{command} hook {name} 2>/dev/null || true"
        groups = hooks.setdefault(event, [])
        present = any(
            entry.get("command") == line
            for group in groups
            for entry in group.get("hooks", [])
        )
        if present:
            continue
        group: dict[str, Any] = {"hooks": [{"type": "command", "command": line}]}
        if matcher is not None:
            group["matcher"] = matcher
        groups.append(group)
        added.append(event)
    os.makedirs(os.path.dirname(settings_path), exist_ok=True)
    Path(settings_path).write_text(json.dumps(settings, indent=2) + "\n")
    return added


# -- transcripts --------------------------------------------------------------


def _records_of(raw: str) -> Iterator[dict[str, Any]]:
    for line in raw.split("\n"):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if isinstance(record, dict):
            yield record


def _read_records(path: str) -> Iterator[dict[str, Any]]:
    try:
        raw = Path(path).read_text(encoding="utf-8", errors="surrogateescape")
    except OSError:
        return
    yield from _records_of(raw)


def _content(record: dict[str, Any]) -> list[dict[str, Any]]:
    message = record.get("message")
    content = message.get("content") if isinstance(message, dict) else None
    if not isinstance(content, list):
        return []
    return [block for block in content if isinstance(block, dict)]


def find_transcript(session_id: str, store: Store | None) -> str | None:
    """Claude's own copy if it still exists, else the store's snapshot,
    fetched into a cache laid out like Claude's project directory so the
    session's cwd is chosen the same way (claude.cwd_for_directory)."""
    root = projects_dir()
    if os.path.isdir(root):
        for project in os.listdir(root):
            path = os.path.join(root, project, f"{session_id}.jsonl")
            if os.path.exists(path):
                return path
    if store is None:
        return None
    try:
        keys = store.list(TRANSCRIPTS_PREFIX)
    except Exception:
        return None
    for key in keys:
        if key.endswith(f"/{session_id}.jsonl"):
            project = key[len(TRANSCRIPTS_PREFIX) :].split("/")[0]
            try:
                data = store.get(key)
            except MissingObject:
                return None
            cached = os.path.join(
                home_dir(), ".loch", "cache", project, f"{session_id}.jsonl"
            )
            os.makedirs(os.path.dirname(cached), exist_ok=True)
            Path(cached).write_bytes(data)
            return cached
    return None


# -- blame --------------------------------------------------------------------


@dataclass(frozen=True)
class Hunk:
    start: int
    end: int
    commit: str
    summary: str
    lines: tuple[str, ...] = ()


def blame_hunks(cwd: str, path: str) -> list[Hunk]:
    porcelain = git(cwd, "blame", "--porcelain", "--", path)
    summaries: dict[str, str] = {}
    lines: list[tuple[int, str, str]] = []
    current = ""
    number = 0
    for line in porcelain.split("\n"):
        if line.startswith("\t"):
            # Each header is followed by the line's text, tab-prefixed.
            lines.append((number, current, line[1:]))
            continue
        parts = line.split(" ")
        if len(parts) >= 3 and len(parts[0]) == 40 and parts[1].isdigit():
            current, number = parts[0], int(parts[2])
        elif line.startswith("summary "):
            summaries[current] = line[len("summary ") :]

    hunks: list[Hunk] = []
    for number, commit, text in sorted(lines):
        last = hunks[-1] if hunks else None
        if last is not None and last.commit == commit and last.end == number - 1:
            hunks[-1] = Hunk(
                last.start, number, commit, last.summary, (*last.lines, text)
            )
        else:
            hunks.append(
                Hunk(number, number, commit, summaries.get(commit, ""), (text,))
            )
    return hunks


@dataclass(frozen=True)
class Change:
    commit: str
    time: int
    summary: str


def history(cwd: str, path: str, lines: tuple[int, int] | None = None) -> list[Change]:
    """Every commit that touched `path` (or just `lines` of it), newest first.

    `-L` follows the range through edits above it and through moves; a whole
    file follows renames. Both are git's own tracking, so this answers "which
    commits shaped these lines" exactly as `git log` would.
    """
    scope = (
        ["-L", f"{lines[0]},{lines[1]}:{path}"]
        if lines is not None
        else ["--follow", "--", path]
    )
    output = git(cwd, "log", "-s", "--format=%x1e%H%x1f%ct%x1f%s", *scope)
    changes = []
    for record in output.split("\x1e")[1:]:
        commit, time, summary = record.strip("\n").split("\x1f", 2)
        changes.append(Change(commit, int(time), summary.split("\n", 1)[0]))
    return changes


@dataclass(frozen=True)
class Link:
    """A commit resolved to the conversation turn that made it."""

    commit: str
    session_id: str
    call: str
    captured: str
    transcript: str | None
    turn: str | None
    conversation: str | None
    title: str | None
    prompt: str | None

    def payload(self) -> dict[str, Any]:
        return {
            "session": self.session_id,
            "call": self.call,
            "captured": self.captured,
            "available": self.transcript is not None,
            "turn": self.turn,
            "conversation": self.conversation,
            "title": self.title,
            "prompt": self.prompt,
        }


def _prompt_text(turn: Turn) -> str | None:
    for block in turn.blocks or []:
        text = block.get("text") if block.get("type") == "text" else None
        if isinstance(text, str) and text.strip() and not text.startswith("<"):
            return text
    return None


class Resolver:
    """Resolves commits to turns, ingesting each session once however many
    commits and lines point into it."""

    def __init__(self, cwd: str, store: Store | None) -> None:
        self.cwd = cwd
        self.store = store
        self._sessions: dict[str, tuple[str | None, list[Turn], dict[str, Any]]] = {}

    def _session(
        self, session_id: str
    ) -> tuple[str | None, list[Turn], dict[str, Any]]:
        if session_id not in self._sessions:
            path = find_transcript(session_id, self.store)
            turns: list[Turn] = []
            header: dict[str, Any] = {}
            if path is not None:
                meta = read_session_meta(path)
                if meta is not None:
                    derived = ingest(path, Origin(meta.cwd, home_dir(), session_id))
                    turns, header = derived.turns, derived.header
            self._sessions[session_id] = (path, turns, header)
        return self._sessions[session_id]

    def resolve(self, commit: str) -> Link | None:
        note = read_note(self.cwd, commit)
        if note is None:
            return None
        session_id, call = str(note.get("session")), str(note.get("call"))
        path, turns, header = self._session(session_id)
        by_id = {turn.id: turn for turn in turns}

        turn = next(
            (
                candidate
                for candidate in turns
                if any(
                    block.get("type") == "tool-call" and block.get("call") == call
                    for block in candidate.blocks or []
                )
            ),
            None,
        )
        prompt = None
        conversation = None
        walker = turn
        while walker is not None:
            if (
                prompt is None
                and walker.core["kind"] == "message"
                and walker.core["author"].get("kind") == "human"
            ):
                prompt = _prompt_text(walker)
            parents = walker.core["parents"]
            if not parents:
                conversation = walker.id
            walker = by_id.get(parents[0]) if parents else None

        session = header.get("session") or {}
        return Link(
            commit=commit,
            session_id=session_id,
            call=call,
            captured=str(note.get("captured")),
            transcript=path,
            turn=turn.id if turn is not None else None,
            conversation=conversation,
            title=session.get("title") or session.get("aiTitle"),
            prompt=_display_summary(prompt),
        )


# -- backfill -----------------------------------------------------------------


@dataclass(frozen=True)
class Call:
    session_id: str
    call: str
    command: str
    started: float
    finished: float


def _seconds(stamp: Any) -> float | None:
    if not isinstance(stamp, str):
        return None
    try:
        return datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def commit_calls(path: str) -> list[Call]:
    """Every Bash call in a transcript that could have made a commit, with the
    window between the call being issued and its result coming back."""
    session_id = os.path.basename(path)[: -len(".jsonl")]
    issued: dict[str, tuple[str, float]] = {}
    calls: list[Call] = []
    for record in _read_records(path):
        stamp = _seconds(record.get("timestamp"))
        for block in _content(record):
            if block.get("type") == "tool_use" and block.get("name") == "Bash":
                command = str((block.get("input") or {}).get("command", ""))
                if "commit" in command and stamp is not None:
                    issued[str(block.get("id"))] = (command, stamp)
            elif block.get("type") == "tool_result":
                call = str(block.get("tool_use_id"))
                if call in issued and stamp is not None:
                    command, started = issued.pop(call)
                    calls.append(Call(session_id, call, command, started, stamp))
    return calls


def backfill(
    cwd: str,
    revision: str = "HEAD",
    progress: Callable[[int, int, int, int], None] | None = None,
) -> list[tuple[str, Call]]:
    """Links commits made before the hooks existed, accepting only an exact
    match: one Bash call whose command contains the commit's subject line and
    whose run brackets the commit's timestamp. A commit with no such call, or
    with two, stays unlinked — a wrong link would send `ask` into the wrong
    conversation and look exactly like a right one.

    Either timestamp may fall in the window: a rebase or cherry-pick rewrites
    the committer time and keeps the author time, and the conversation that
    wrote the change is still the one that made the original commit."""
    listing = git(cwd, "log", "--format=%H%x00%at%x00%ct%x00%s", revision)
    already = noted_commits(cwd)
    pending: list[tuple[str, tuple[int, int], str]] = []
    for line in listing.splitlines():
        sha, authored, committed, subject = line.split("\x00", 3)
        if sha not in already and subject:
            pending.append((sha, (int(authored), int(committed)), subject))
    if not pending:
        return []

    subjects = {subject for _, _, subject in pending}
    # Listed and sized up front so progress has a real total: reading every
    # transcript on the machine is the whole cost (~25s on 750MB).
    transcripts: list[tuple[str, int]] = []
    root = projects_dir()
    for project in sorted(os.listdir(root)) if os.path.isdir(root) else []:
        directory = os.path.join(root, project)
        if not os.path.isdir(directory):
            continue
        for name in sorted(os.listdir(directory)):
            if name.endswith(".jsonl"):
                path = os.path.join(directory, name)
                try:
                    transcripts.append((path, os.path.getsize(path)))
                except OSError:
                    continue
    total_bytes = sum(size for _, size in transcripts)

    calls: list[Call] = []
    read = 0
    for done, (path, size) in enumerate(transcripts, 1):
        read += size
        try:
            raw = Path(path).read_text(encoding="utf-8", errors="replace")
        except OSError:
            raw = ""
        if "git commit" in raw and any(subject in raw for subject in subjects):
            calls.extend(commit_calls(path))
        if progress is not None:
            progress(done, len(transcripts), read, total_bytes)

    linked: list[tuple[str, Call]] = []
    for sha, stamps, subject in pending:
        matches = [
            call
            for call in calls
            if subject in call.command
            and any(
                call.started - CLOCK_SLACK <= stamp <= call.finished + CLOCK_SLACK
                for stamp in stamps
            )
        ]
        if len({(call.session_id, call.call) for call in matches}) == 1:
            write_note(
                cwd, sha, make_note(matches[0].session_id, matches[0].call, "backfill")
            )
            linked.append((sha, matches[0]))
    return linked


# -- ask ----------------------------------------------------------------------


@dataclass(frozen=True)
class Fork:
    session_id: str
    path: str
    cwd: str
    records: int
    origin_session: str


def _parent(record: dict[str, Any]) -> str | None:
    parent = record.get("parentUuid")
    if parent is None:
        parent = record.get("logicalParentUuid")
    return parent if isinstance(parent, str) else None


def fork_session(
    transcript: str,
    target_cwd: str,
    call: str | None = None,
    title: str | None = None,
) -> Fork:
    """A new Claude session holding the conversation, filed under
    `target_cwd` so `claude --resume` finds it. The original session is never
    modified: a follow-up is a branch of the conversation, not more of it.

    With no `call` it holds the conversation to its end — the last record
    written, which after a rewind is the branch the session was left on.
    With one, it is cut after the whole round of tool results that call
    belongs to, not after the call alone: an assistant message that issued
    parallel calls is one API message, and resuming with any of its results
    missing is a malformed conversation."""
    raw = Path(transcript).read_text(encoding="utf-8", errors="surrogateescape")
    records = [
        record for record in _records_of(raw) if isinstance(record.get("uuid"), str)
    ]
    children: dict[str, list[dict[str, Any]]] = {}
    for record in records:
        parent = _parent(record)
        if parent is not None:
            children.setdefault(parent, []).append(record)

    if call is None:
        if not records:
            raise TraceError("call-not-found", "the transcript holds no records")
        return _write_fork(transcript, raw, records, records[-1], target_cwd, title)

    result = next(
        (
            record
            for record in records
            if any(
                block.get("type") == "tool_result" and block.get("tool_use_id") == call
                for block in _content(record)
            )
        ),
        None,
    )
    if result is None:
        raise TraceError("call-not-found", f"no result for tool call {call}")

    leaf = result
    while True:
        following = [
            child
            for child in children.get(leaf["uuid"], [])
            if child.get("type") == "user"
            and any(block.get("type") == "tool_result" for block in _content(child))
        ]
        if not following:
            break
        leaf = following[0]
    return _write_fork(transcript, raw, records, leaf, target_cwd, title)


def _write_fork(
    transcript: str,
    raw: str,
    records: list[dict[str, Any]],
    leaf: dict[str, Any],
    target_cwd: str,
    title: str | None,
) -> Fork:
    by_uuid = {record["uuid"]: record for record in records}
    keep: set[str] = set()
    walker: dict[str, Any] | None = leaf
    while walker is not None and walker["uuid"] not in keep:
        keep.add(walker["uuid"])
        parent = _parent(walker)
        walker = by_uuid.get(parent) if parent is not None else None

    meta = read_session_meta(transcript)
    origin_cwd = meta.cwd if meta is not None else next(iter(transcript_cwds(raw)))
    origin_session = os.path.basename(transcript)[: -len(".jsonl")]
    new_id = str(uuid.uuid4())

    kept = [
        json.dumps(record, ensure_ascii=False)
        for record in records
        if record["uuid"] in keep
    ]
    kept.append(
        json.dumps(
            {
                "type": "custom-title",
                "customTitle": title or f"loch ask: {origin_session[:8]}",
                "sessionId": origin_session,
            }
        )
    )
    spec = RewriteSpec(
        origin_cwd, home_dir(), target_cwd, home_dir(), origin_session, new_id
    )
    text = rewrite_transcript("\n".join(kept) + "\n", spec)

    path = transcript_path_for(target_cwd, new_id)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    Path(path).write_text(text, encoding="utf-8", errors="surrogateescape")
    return Fork(new_id, path, target_cwd, len(keep), origin_session)
