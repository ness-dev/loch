"""Every command but `hook` in the Rust build against the Python reference.

Opt-in with LOCH_RUST, like test_equivalence.py; the machinery is in
equivalence.py. Each scenario runs its commands in both renderings where the
two differ, since prose and payload are built separately and either can drift.
"""

import gzip
import json
import os
import re
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest
from equivalence import MTIME, PAST, RUST, SESSION, Run, Scenario, World, twin

pytestmark = pytest.mark.skipif(not RUST, reason="set LOCH_RUST to a Rust build")

Loch = Callable[..., Run]

OTHER = "22222222-3333-4444-5555-666666666666"
THIRD = "33333333-4444-5555-6666-777777777777"
SECRET = "ghp_" + "a1B2c3D4e5" * 4


def both(loch: Loch, *args: str, **kw: Any) -> list[Run]:
    """A command in text mode and in JSON mode."""
    return [loch(*args, **kw), loch(*args, "--json", **kw)]


def user(cwd: str, text: Any, branch: str | None = "main", **extra: Any) -> dict:
    record: dict[str, Any] = {
        "type": "user",
        "uuid": extra.pop("uuid", "u1"),
        "parentUuid": extra.pop("parent", None),
        "sessionId": extra.pop("session", SESSION),
        "cwd": cwd,
        "version": "2.1.0",
        "timestamp": "2026-01-01T00:00:00.000Z",
        "message": {"role": "user", "content": text},
    }
    if branch is not None:
        record["gitBranch"] = branch
    record.update(extra)
    return record


def assistant(
    cwd: str, blocks: list[Any], uuid: str, parent: str, **extra: Any
) -> dict:
    record = {
        "type": "assistant",
        "uuid": uuid,
        "parentUuid": parent,
        "sessionId": extra.pop("session", SESSION),
        "cwd": cwd,
        "gitBranch": extra.pop("branch", "main"),
        "version": "2.1.0",
        "timestamp": extra.pop("timestamp", "2026-01-01T00:00:05.000Z"),
        "message": {
            "id": "msg_1",
            "model": "claude-x",
            "role": "assistant",
            "content": blocks,
        },
    }
    record.update(extra)
    return record


def project(w: World) -> str:
    path = w.root / "proj"
    path.mkdir(exist_ok=True)
    return str(path)


def rich_sessions(w: World) -> str:
    """Sessions with what list, save and ingest each have to get right: a
    secret, odd whitespace, a non-ASCII summary, two branches, a session that
    moved directories, a lone surrogate escape and a line that isn't JSON."""
    cwd = project(w)
    w.session(
        cwd,
        SESSION,
        [
            user(cwd, f"deploy with  {SECRET}\tand then é 😀", uuid="a1"),
            assistant(
                cwd, [{"type": "text", "text": f"ok {cwd}/src and ~"}], "a2", "a1"
            ),
            "not json {",
            {"type": "ai-title", "aiTitle": "Deploying"},
            user(
                cwd,
                [{"type": "tool_result", "tool_use_id": "t1", "content": "done"}],
                uuid="a3",
                parent="a2",
            ),
        ],
    )
    w.session(
        cwd,
        OTHER,
        [
            user(
                f"{cwd}-old", "before the move", branch="old", uuid="b1", session=OTHER
            ),
            user(
                cwd,
                [{"type": "text", "text": "after \\ud83d the move"}],
                branch="feature/x",
                uuid="b2",
                parent="b1",
                session=OTHER,
            ),
            '{"type": "user", "uuid": "b3", "parentUuid": "b2", "cwd": "'
            + cwd
            + '", "message": {"content": "lone \\ud83d"}}',
            {
                "type": "system",
                "subtype": "turn_duration",
                "uuid": "b4",
                "parentUuid": "b3",
                "cwd": cwd,
                "durationMs": 5,
            },
            {"type": "custom-title", "customTitle": "Moved"},
        ],
    )
    w.session(
        cwd, THIRD, [user(cwd, "no branch here", branch=None, uuid="c1", session=THIRD)]
    )
    return cwd


# -- help, version, usage -------------------------------------------------------

COMMANDS = [
    "list",
    "save",
    "restore",
    "delete",
    "reindex",
    "ingest",
    "install",
    "blame",
    "log",
    "ask",
    "backfill",
]

BAD_ARGUMENTS = [
    ["-h"],
    ["--help"],
    ["--bogus"],
    ["--cwd"],
    ["--store"],
    ["--s", "x"],
    ["--st=x"],
    ["--json=1"],
    ["--force=yes"],
    ["-x"],
    ["-hx"],
    ["--", "-h"],
    ["a", "b", "c"],
    ["-5"],
    ["--into", "-5"],
]


def usage(w: World, loch: Loch) -> list[Run]:
    runs = []
    for command in COMMANDS:
        for argv in BAD_ARGUMENTS:
            runs += both(loch, command, *argv)
    return runs


def usage_narrow(w: World, loch: Loch) -> list[Run]:
    """Usage lines wrap at the terminal width, which COLUMNS sets."""
    runs = []
    for columns in ("40", "60", "12"):
        for command in COMMANDS:
            runs.append(loch(command, "-h", env={"COLUMNS": columns}))
            runs.append(loch(command, "--bogus", env={"COLUMNS": columns}))
    return runs


def top_level(w: World, loch: Loch) -> list[Run]:
    return [
        *both(loch),
        *both(loch, "help"),
        *both(loch, "--help"),
        *both(loch, "-h"),
        *both(loch, "--version"),
        *both(loch, "-V"),
        *both(loch, "version"),
        *both(loch, "nonsense"),
        loch("--json"),
        loch("--json", "--json"),
        loch("list", "--json", "--json", "--cwd", project(w)),
    ]


# -- list -----------------------------------------------------------------------


def listing(w: World, loch: Loch) -> list[Run]:
    cwd = rich_sessions(w)
    (w.root / "empty").mkdir()
    return [
        *both(loch, "list", "--cwd", cwd),
        *both(loch, "list", "--cwd", cwd, "--branch", "main"),
        *both(loch, "list", "--cwd", cwd, "--branch", "feature/x"),
        *both(loch, "list", "--cwd", cwd, "--branch", "none-such"),
        *both(loch, "list", "--cwd", str(w.root / "empty")),
        *both(loch, "list", "--cwd", str(w.root / "missing")),
        *both(loch, "list", cwd=Path(cwd)),
        *both(
            loch,
            "list",
            "--cwd",
            cwd,
            env={"CLAUDE_CONFIG_DIR": str(w.root / "other-config")},
        ),
    ]


def listing_via_symlink(w: World, loch: Loch) -> list[Run]:
    cwd = rich_sessions(w)
    os.symlink(cwd, w.root / "link")
    return both(loch, "list", "--cwd", str(w.root / "link"))


def listing_config_dir(w: World, loch: Loch) -> list[Run]:
    cwd = project(w)
    config = w.root / "work-account"
    folder = config / "projects" / re.sub(r"[^a-zA-Z0-9]", "-", cwd)
    folder.mkdir(parents=True)
    (folder / f"{SESSION}.jsonl").write_text(
        json.dumps(user(cwd, "in another account")) + "\n"
    )
    os.utime(folder / f"{SESSION}.jsonl", (MTIME, MTIME))
    return both(loch, "list", "--cwd", cwd, env={"CLAUDE_CONFIG_DIR": str(config)})


def listing_bad_bytes(w: World, loch: Loch) -> list[Run]:
    cwd = project(w)
    w.session(cwd, SESSION, [user(cwd, "fine")], raw=b'{"type": "user", "x": "\xff"}\n')
    return [
        *both(loch, "list", "--cwd", cwd),
        *both(loch, "save", "--cwd", cwd),
    ]


# -- save, list --remote, restore, delete, reindex --------------------------------------


def save_and_list_remote(w: World, loch: Loch) -> list[Run]:
    cwd = rich_sessions(w)
    runs = [
        *both(loch, "save", "--cwd", cwd),
        loch("save", "--cwd", cwd, "--branch", "feature/x", "--json"),
        loch("save", "--cwd", cwd, "--session", THIRD),
        *both(loch, "save", "--cwd", cwd, "--branch", "none-such"),
        *both(loch, "save", "--cwd", str(w.root / "missing")),
        loch("save", "--cwd", cwd, "--store", "relative-store", "--json", cwd=w.root),
        loch("save", "--cwd", cwd, "--store", f"file://{w.root}/uri%20store"),
    ]
    os.makedirs(w.repo, exist_ok=True)
    w.init()
    w.commit("first")
    w.git("checkout", "-q", "-b", "feature/x")
    for args in (
        ["--remote"],
        ["--remote", "--cwd", str(w.repo)],
        ["--remote", "--branch", "main"],
        ["--remote", "--branch", "none-such"],
        ["--all"],
        ["--store", str(w.store), "--branch", "feature/x"],
        ["--remote", "--store", str(w.root / "nowhere")],
    ):
        runs += both(loch, "list", *args, cwd=w.repo)
    runs += both(loch, "list", "--remote", "--cwd", str(w.root))
    return runs


def bundle_ref(w: World) -> str:
    bundles = sorted((w.store / "bundles").glob("*.loch"))
    return bundles[0].stem


def restore(w: World, loch: Loch) -> list[Run]:
    cwd = rich_sessions(w)
    runs = [loch("save", "--cwd", cwd)]
    ref = bundle_ref(w)
    target = w.root / "elsewhere"
    target.mkdir()
    runs += [
        *both(loch, "restore", ref, "--into", str(target)),
        # Everything is there now: skipped, so nothing restored.
        *both(loch, "restore", ref, "--into", str(target)),
        *both(loch, "restore", ref, "--into", str(target), "--force"),
        *both(loch, "restore", ref, "--into", str(target), "--new-id"),
        *both(loch, "restore"),
        *both(loch, "restore", "0" * 64),
        *both(loch, "restore", ref, "--store", str(w.root / "nowhere")),
    ]
    # Another machine: a different home, so the home pair rewrites too.
    other_home = w.root / "other-home"
    other_home.mkdir()
    runs += both(
        loch,
        "restore",
        ref,
        "--into",
        str(target),
        "--force",
        env={"HOME": str(other_home)},
    )
    return runs


def restore_unreadable(w: World, loch: Loch) -> list[Run]:
    bundles = w.store / "bundles"
    bundles.mkdir(parents=True)
    (bundles / f"{'a' * 64}.loch").write_bytes(b"garbage")
    (bundles / f"{'b' * 64}.loch").write_bytes(b"")
    (bundles / f"{'c' * 64}.loch").write_bytes(
        gzip.compress(json.dumps({"version": 2}).encode(), mtime=0)
    )
    (bundles / f"{'d' * 64}.loch").write_bytes(
        gzip.compress(json.dumps({"version": "1"}).encode(), mtime=0)
    )
    runs = []
    for letter in "abcd":
        runs += both(loch, "restore", letter * 64)
    return runs


def delete_and_reindex(w: World, loch: Loch) -> list[Run]:
    cwd = rich_sessions(w)
    runs = [loch("save", "--cwd", cwd), loch("save", "--cwd", cwd, "--branch", "main")]
    refs = sorted(p.stem for p in (w.store / "bundles").glob("*.loch"))
    stale = w.store / "index" / "branch" / "gone" / ("f" * 64)
    stale.parent.mkdir(parents=True)
    stale.write_text("{}")
    (w.store / "bundles" / "notes.txt").write_text("not a bundle")
    runs += [
        *both(loch, "reindex"),
        *both(loch, "reindex"),
        *both(loch, "delete", refs[0]),
        *both(loch, "delete", refs[0]),
        *both(loch, "delete"),
        *both(loch, "list", "--all"),
        *both(loch, "reindex", "--store", str(w.root / "empty-store")),
    ]
    return runs


# -- ingest ----------------------------------------------------------------------


def ingest(w: World, loch: Loch) -> list[Run]:
    cwd = rich_sessions(w)
    key = w.root / "id_ed25519.pub"
    key.write_text(
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGq7k5Pi0VGa1sYhVPLvlXHjjhVQrDsWlvS4sUVJmPB0 me\n"
    )
    bad = w.root / "bad.pub"
    bad.write_text("ssh-ed25519 not*base64\n")
    one = w.root / "one.pub"
    one.write_text("just-one-field\n")
    padding = w.root / "padding.pub"
    padding.write_text("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5\n")
    sidecar = w.projects() / re.sub(r"[^a-zA-Z0-9]", "-", cwd) / SESSION / "subagents"
    sidecar.mkdir(parents=True)
    (sidecar / "agent-1.jsonl").write_text('{"type": "user"}\n')
    return [
        *both(loch, "ingest", "--cwd", cwd),
        *both(
            loch,
            "ingest",
            "--cwd",
            cwd,
            "--out",
            str(w.root / "out"),
            "--author",
            "me@example.com",
        ),
        *both(
            loch,
            "ingest",
            "--cwd",
            cwd,
            "--out",
            str(w.root / "keyed"),
            "--author-key",
            str(key),
        ),
        *both(loch, "ingest", "--cwd", cwd, "--author-key", str(bad)),
        *both(loch, "ingest", "--cwd", cwd, "--author-key", str(one)),
        *both(loch, "ingest", "--cwd", cwd, "--author-key", str(padding)),
        *both(
            loch, "ingest", "--cwd", cwd, "--author-key", str(w.root / "missing.pub")
        ),
        *both(loch, "ingest", "--cwd", cwd, "--author", "x", "--author-key", str(key)),
        *both(
            loch,
            "ingest",
            "--cwd",
            cwd,
            "--session",
            OTHER,
            "--out",
            str(w.root / "one"),
        ),
        *both(loch, "ingest", "--cwd", str(w.root / "missing")),
    ]


def ingest_graph(w: World, loch: Loch) -> list[Run]:
    """The converter's harder cases: a rewind that leaves a branch, a
    resubmitted identical turn, a compaction boundary, a dangling parent,
    records out of file order, a cycle, and every block type."""
    cwd = project(w)
    blocks = [
        {"type": "thinking", "thinking": "hm", "signature": "sig"},
        {"type": "redacted_thinking", "data": "x"},
        {
            "type": "tool_use",
            "id": "t1",
            "name": "Bash",
            "input": {"command": f"ls {cwd}"},
        },
        {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
        {"type": "server_tool_use", "id": "s1"},
        "a bare string",
        {"type": "text", "text": "literal {{loch:root}} placeholder"},
    ]
    records = [
        user(cwd, "start", uuid="r1"),
        assistant(cwd, blocks, "r2", "r1"),
        user(
            cwd,
            [
                {
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [{"type": "text", "text": f"{cwd}/a"}],
                    "is_error": True,
                }
            ],
            uuid="r3",
            parent="r2",
        ),
        user(cwd, "branch A", uuid="r4", parent="r3"),
        user(cwd, "branch B", uuid="r5", parent="r3"),
        user(cwd, "branch A", uuid="r6", parent="r3"),
        {
            "type": "system",
            "subtype": "compact_boundary",
            "uuid": "r7",
            "parentUuid": None,
            "logicalParentUuid": "r5",
            "cwd": cwd,
            "content": "compacted",
        },
        user(cwd, "summary", uuid="r8", parent="r7", isCompactSummary=True),
        user(cwd, "orphan", uuid="r9", parent="never-seen"),
        user(cwd, "child before parent", uuid="r10", parent="r11"),
        user(cwd, "parent after child", uuid="r11", parent="r8"),
        user(cwd, "cycle one", uuid="r12", parent="r13"),
        user(cwd, "cycle two", uuid="r13", parent="r12"),
        {
            "type": "attachment",
            "uuid": "r14",
            "parentUuid": "r11",
            "cwd": cwd,
            "attachment": {"path": f"{cwd}/f", "keyed": {f"{cwd}/k": 1}},
        },
        {
            "type": "system",
            "subtype": "api_error",
            "uuid": "r15",
            "parentUuid": "r14",
            "cwd": cwd,
            "timestamp": "t",
        },
        {
            "type": "system",
            "subtype": "stray",
            "uuid": "r16",
            "parentUuid": "nowhere",
            "cwd": cwd,
        },
        user(cwd, "meta", uuid="r17", parent="r15", isMeta=True),
        {"type": "last-prompt", "leafUuid": "r4"},
        {"type": "user", "uuid": 5, "message": {"content": "non-string uuid"}},
        {
            "type": "user",
            "uuid": "r18",
            "parentUuid": "r17",
            "cwd": cwd,
            "message": {
                "content": [
                    {"type": "text", "text": 1.5e300},
                    {"type": "text", "text": 123456789012345678901234567890},
                ]
            },
        },
    ]
    w.session(cwd, SESSION, records)
    return both(loch, "ingest", "--cwd", cwd, "--out", str(w.root / "derived"))


def ingest_from_home(w: World, loch: Loch) -> list[Run]:
    """A session run from the home directory itself is a root, not a home."""
    cwd = str(w.home)
    w.session(cwd, SESSION, [user(cwd, f"in {cwd}/x")])
    return both(loch, "ingest", "--cwd", cwd, "--out", str(w.root / "derived"))


# -- install -------------------------------------------------------------------------


def install(w: World, loch: Loch) -> list[Run]:
    w.init()
    w.commit("first")
    settings = w.repo / ".claude"
    settings.mkdir()
    (settings / "settings.json").write_text(
        json.dumps(
            {"model": "x", "n": 1.50, "é": "ü", "hooks": {"Stop": [{"hooks": []}]}}
        )
    )
    return [
        *both(loch, "install", cwd=w.repo),
        *both(loch, "install", cwd=w.repo),
        *both(loch, "install", "--local", cwd=w.repo),
        loch("install", "--command", "/opt/loch", "--cwd", str(w.repo)),
        *both(loch, "install", cwd=w.root),
    ]


# -- blame, log, ask, backfill ------------------------------------------------------


def traced_repo(w: World) -> dict[str, str]:
    """A repository whose commits link to a real-shaped transcript: two
    requests in one session, one commit each, plus one unlinked commit and an
    uncommitted edit."""
    w.init()
    repo = str(w.repo)
    base = w.commit_file("app.py", "one\ntwo\nthree\n", "unlinked start", date=PAST)
    first = w.commit_file(
        "app.py", "one\nTWO\nthree\nfour\n", "agent: two", date="2026-01-01T00:00:12Z"
    )
    second = w.commit_file(
        "app.py",
        "one\nTWO\nthree\nfour\nfive\n",
        "agent: five",
        date="2026-01-01T00:01:12Z",
    )
    w.commit_file("other.txt", "x\n", "agent: other", date="2026-01-01T00:02:12Z")
    (w.repo / "app.py").write_text("one\nTWO\nthree\nfour\nfive\nsix\n")
    records = [
        user(repo, "please change two <and> “quote”", uuid="q1"),
        assistant(
            repo,
            [
                {
                    "type": "tool_use",
                    "id": "toolu_a",
                    "name": "Bash",
                    "input": {"command": "git commit -m 'agent: two'"},
                }
            ],
            "q2",
            "q1",
            timestamp="2026-01-01T00:00:10.000Z",
        ),
        user(
            repo,
            [{"type": "tool_result", "tool_use_id": "toolu_a", "content": "ok"}],
            uuid="q3",
            parent="q2",
            timestamp="2026-01-01T00:00:14.000Z",
        ),
        user(repo, "<command>ignored</command>", uuid="q4", parent="q3"),
        user(repo, "now add five", uuid="q5", parent="q4"),
        assistant(
            repo,
            [
                {
                    "type": "tool_use",
                    "id": "toolu_b",
                    "name": "Bash",
                    "input": {"command": "git commit -m 'agent: five'"},
                },
                {
                    "type": "tool_use",
                    "id": "toolu_c",
                    "name": "Bash",
                    "input": {"command": "git commit -m 'agent: other'"},
                },
            ],
            "q6",
            "q5",
            timestamp="2026-01-01T00:01:10.000Z",
        ),
        user(
            repo,
            [{"type": "tool_result", "tool_use_id": "toolu_b", "content": "ok"}],
            uuid="q7",
            parent="q6",
            timestamp="2026-01-01T00:01:14.000Z",
        ),
        user(
            repo,
            [{"type": "tool_result", "tool_use_id": "toolu_c", "content": "ok"}],
            uuid="q8",
            parent="q7",
            timestamp="2026-01-01T00:02:14.000Z",
        ),
        {"type": "ai-title", "aiTitle": "Edit app"},
        {
            "type": "user",
            "uuid": "q9",
            "parentUuid": "q8",
            "cwd": repo,
            "costs": [1e-05, 1e16, 0.5, 1e22, -0.0],
            "message": {"content": "floats"},
        },
    ]
    w.session(repo, SESSION, records)
    return {"base": base, "first": first, "second": second}


def blame_and_log(w: World, loch: Loch) -> list[Run]:
    commits = traced_repo(w)
    w.note(commits["first"], SESSION, "toolu_a")
    w.note(commits["second"], SESSION, "toolu_b")
    w.note(commits["base"], "99999999-0000-0000-0000-000000000000", "toolu_gone")
    runs = []
    for args in (
        ["blame", "app.py"],
        ["blame", "other.txt"],
        ["blame", "missing.py"],
        ["blame", "app.py", "--html", str(w.root / "page.html"), "--no-open"],
        ["blame", "app.py", "--html", "--no-open"],
        [
            "blame",
            "app.py",
            "--html",
            str(w.root / "lines.html"),
            "--lines",
            "1-2, 5",
            "--no-open",
        ],
        [
            "blame",
            "app.py",
            "--html",
            str(w.root / "bad.html"),
            "--lines",
            "x-1",
            "--no-open",
        ],
        ["log", "app.py"],
        ["log", "app.py:2"],
        ["log", "app.py:2-4"],
        ["log", "app.py:9"],
        ["log", "app.py:0"],
        ["log", "app.py:3-2"],
        ["log", "app.py:x"],
        ["log", "nope"],
        ["log", "other.txt"],
    ):
        runs += both(loch, *args, cwd=w.repo)
    runs += both(loch, "blame", str(w.root / "outside.txt"), cwd=w.root)
    return runs


def ask(w: World, loch: Loch) -> list[Run]:
    commits = traced_repo(w)
    w.note(commits["first"], SESSION, "toolu_a")
    w.note(commits["second"], SESSION, "toolu_b")
    w.note(commits["base"], "99999999-0000-0000-0000-000000000000", "toolu_gone")
    runs = []
    for args in (
        ["ask", commits["first"], "--no-launch"],
        ["ask", "app.py:5", "why five?", "--no-launch", "--at-commit"],
        ["ask", "app.py:2", "--at-commit", "--no-launch"],
        ["ask", "app.py:6", "--no-launch"],
        ["ask", "app.py:1", "--no-launch"],
        ["ask", "app.py:99", "--no-launch"],
        ["ask", "HEAD~3", "--no-launch"],
        ["ask", "nonsense-rev", "--no-launch"],
        ["ask", commits["first"], "--json"],
    ):
        runs += both(loch, *args, cwd=w.repo)
    return runs


def ask_from_store(w: World, loch: Loch) -> list[Run]:
    """A transcript Claude has deleted is found in the store's snapshot."""
    commits = traced_repo(w)
    w.note(commits["second"], SESSION, "toolu_b")
    transcript = next(w.projects().glob(f"*/{SESSION}.jsonl"))
    runs = [
        loch(
            "hook",
            "stop",
            stdin={"session_id": SESSION, "transcript_path": str(transcript)},
        )
    ]
    transcript.unlink()
    runs += both(loch, "ask", "app.py:5", "--no-launch", cwd=w.repo)
    runs += both(loch, "blame", "app.py", cwd=w.repo)
    return runs


def backfill(w: World, loch: Loch) -> list[Run]:
    traced_repo(w)
    w.git("checkout", "-q", "-b", "side")
    # Two calls in different sessions bracket this one: ambiguous, so unlinked.
    w.commit_file("x.txt", "x\n", "agent: two", date="2026-01-01T00:00:12Z")
    runs = both(loch, "backfill", cwd=w.repo)
    runs += both(loch, "backfill", "--revision", "main", cwd=w.repo)
    runs += both(loch, "backfill", "--revision", "no-such-rev", cwd=w.repo)
    runs += both(loch, "backfill", "--cwd", str(w.root))
    return runs


SCENARIOS: list[Scenario] = [
    usage,
    usage_narrow,
    top_level,
    listing,
    listing_via_symlink,
    listing_config_dir,
    listing_bad_bytes,
    save_and_list_remote,
    restore,
    restore_unreadable,
    delete_and_reindex,
    ingest,
    ingest_graph,
    ingest_from_home,
    install,
    blame_and_log,
    ask,
    ask_from_store,
    backfill,
]


@pytest.mark.parametrize("scenario", SCENARIOS, ids=lambda s: s.__name__)
def test_command_matches_python(tmp_path: Path, scenario: Scenario) -> None:
    twin(tmp_path, scenario)


def test_s3_store_matches_python(tmp_path: Path) -> None:
    """The bundle commands against an S3 endpoint rather than a directory:
    both builds' requests go to one moto server, reset between them. Auth is
    off, because moto's verifier rejects boto3's own correct signatures on
    the prefixed listings these commands make; signing is pinned to AWS's
    published examples in s3.rs instead, and checked with auth on by the
    stop-hook test."""
    import urllib.request

    import boto3
    from equivalence import first_difference, normalized
    from moto.server import ThreadedMotoServer

    server = ThreadedMotoServer(ip_address="127.0.0.1", port=0)
    server.start()
    host, port = server.get_host_and_port()
    endpoint = f"http://{host}:{port}"
    credentials = {
        "AWS_ACCESS_KEY_ID": "testing",
        "AWS_SECRET_ACCESS_KEY": "testing",
        "AWS_SHARED_CREDENTIALS_FILE": os.devnull,
        "AWS_CONFIG_FILE": os.devnull,
        "LOCH_S3_ENDPOINT": endpoint,
        "LOCH_S3_REGION": "us-east-1",
        "LOCH_STORE": "s3://loch-test/team/",
    }
    outcomes = []
    try:
        for impl in ("py", "rs"):
            urllib.request.urlopen(
                urllib.request.Request(f"{endpoint}/moto-api/reset", b"")
            )
            boto3.client(
                "s3",
                endpoint_url=endpoint,
                region_name="us-east-1",
                aws_access_key_id="testing",
                aws_secret_access_key="testing",
            ).create_bucket(Bucket="loch-test")
            root = tmp_path / "world"
            if root.exists():
                import shutil

                shutil.rmtree(root)
            w = World(root)
            cwd = rich_sessions(w)

            def loch(*args: str, _w: World = w, _impl: str = impl, **kw: Any) -> Run:
                env = {**credentials, **kw.pop("env", {})}
                return _w.loch(_impl, *args, env=env, **kw)

            runs = [*both(loch, "save", "--cwd", cwd)]
            listed = json.loads(loch("list", "--all", "--json").stdout)
            ref = listed["entries"][0]["ref"]
            target = w.root / "target"
            target.mkdir()
            runs += [
                *both(loch, "list", "--all"),
                *both(loch, "list", "--remote", "--branch", "feature/x"),
                *both(loch, "restore", ref, "--into", str(target)),
                *both(loch, "restore", "0" * 64),
                *both(loch, "reindex"),
                *both(loch, "delete", ref),
                *both(loch, "delete", ref),
                *both(loch, "list", "--all"),
                *both(loch, "save", "--cwd", cwd, env={"AWS_ACCESS_KEY_ID": ""}),
                *both(
                    loch,
                    "list",
                    "--all",
                    env={"LOCH_S3_ENDPOINT": "http://127.0.0.1:9"},
                ),
            ]
            outcomes.append(normalized(runs, w.observe()))
    finally:
        server.stop()
    assert outcomes[0] == outcomes[1], first_difference(outcomes[0], outcomes[1])
