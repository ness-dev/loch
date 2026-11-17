"""Shared machinery for holding the Rust build to the Python reference.

Every scenario is built twice at the same path with pinned git dates and file
times, so commit hashes, note objects, the notes ref and session mtimes come
out byte-identical; one copy is driven by `python -m loch.cli`, the other by
the binary named in LOCH_RUST, and everything observable is compared: stdout,
stderr, exit status, and every file the run left behind.

Two values are random by design — the id `restore --new-id` mints and the
fork `ask` creates — so v4 UUIDs are renamed in order of first appearance
before comparing. Fixture ids are deliberately not v4, so they never are.
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

RUST = os.environ.get("LOCH_RUST")

SESSION = "11111111-2222-3333-4444-555555555555"
# In the future, so the hook's "committed after the call began" bound admits
# commits made during a call; fixed, so their hashes are too.
NOW = "2031-01-01T00:00:00Z"
PAST = "2020-01-01T00:00:00Z"
MTIME = 1_800_000_000.123456
EPOCH = "1800000000"

GENERATED = re.compile(
    rb"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}"
)


def implementation(name: str) -> list[str]:
    if name == "py":
        return [sys.executable, "-m", "loch.cli"]
    assert RUST is not None
    return [os.path.abspath(RUST)]


@dataclass
class Run:
    stdout: bytes
    stderr: bytes
    status: int


class World:
    """One scratch machine: a HOME, a repository, a store, an environment."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.home = root / "home"
        self.repo = root / "repo"
        self.store = root / "store"
        self.home.mkdir(parents=True)
        bin_dir = root / "bin"
        bin_dir.mkdir()
        # `install --local` records `shutil.which("loch")`; both builds must
        # find the same one.
        (bin_dir / "loch").write_text("#!/bin/sh\n")
        (bin_dir / "loch").chmod(0o755)
        self.env = {
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "HOME": str(self.home),
            "LOCH_STORE": str(self.store),
            "TMPDIR": str(root),
            "GIT_CONFIG_GLOBAL": str(root / "gitconfig"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Test",
            "GIT_AUTHOR_EMAIL": "test@example.com",
            "GIT_COMMITTER_NAME": "Test",
            "GIT_COMMITTER_EMAIL": "test@example.com",
            "GIT_AUTHOR_DATE": NOW,
            "GIT_COMMITTER_DATE": NOW,
            "SOURCE_DATE_EPOCH": EPOCH,
            "PYTHONPATH": str(Path(__file__).resolve().parent.parent),
        }

    # -- git ---------------------------------------------------------------

    def git(self, *args: str, cwd: Path | None = None, date: str = NOW) -> str:
        env = {**self.env, "GIT_AUTHOR_DATE": date, "GIT_COMMITTER_DATE": date}
        return subprocess.run(
            ["git", "-C", str(cwd or self.repo), *args],
            capture_output=True,
            text=True,
            check=True,
            env=env,
        ).stdout.strip()

    def init(self, *extra: str) -> None:
        self.repo.mkdir(exist_ok=True)
        self.git("init", "-q", "-b", "main", *extra)

    def commit(self, message: str, cwd: Path | None = None, date: str = NOW) -> str:
        self.git("commit", "-q", "--allow-empty", "-m", message, cwd=cwd, date=date)
        return self.git("rev-parse", "HEAD", cwd=cwd)

    def commit_file(self, name: str, text: str, message: str, date: str = NOW) -> str:
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        self.git("add", name)
        self.git("commit", "-q", "-m", message, date=date)
        return self.git("rev-parse", "HEAD")

    def note(self, commit: str, session: str, call: str) -> None:
        body = json.dumps(
            {
                "loch": 1,
                "agent": "claude-code",
                "session": session,
                "call": call,
                "captured": "hook",
            },
            sort_keys=True,
            separators=(",", ":"),
        )
        subprocess.run(
            ["git", "-C", str(self.repo), "notes", "--ref", "refs/notes/loch"]
            + ["add", "-f", "-m", body, commit],
            check=True,
            capture_output=True,
            env=self.env,
        )

    # -- sessions ------------------------------------------------------------

    def projects(self) -> Path:
        return self.home / ".claude" / "projects"

    def session(
        self,
        cwd: str,
        session_id: str,
        records: list[Any],
        directory: str | None = None,
        raw: bytes = b"",
    ) -> Path:
        """A transcript filed as Claude files it, with a pinned mtime. Records
        that are strings are written as-is, so a scenario can include lines
        that aren't JSON."""
        encoded = directory or re.sub(r"[^a-zA-Z0-9]", "-", cwd)
        folder = self.projects() / encoded
        folder.mkdir(parents=True, exist_ok=True)
        path = folder / f"{session_id}.jsonl"
        lines = [r if isinstance(r, str) else json.dumps(r) for r in records]
        path.write_bytes("\n".join(lines).encode() + b"\n" + raw)
        os.utime(path, (MTIME, MTIME))
        return path

    # -- running -------------------------------------------------------------

    def loch(
        self,
        impl: str,
        *args: str,
        stdin: Any = None,
        env: dict[str, str] | None = None,
        cwd: Path | None = None,
    ) -> Run:
        data = (
            stdin
            if isinstance(stdin, bytes)
            else b""
            if stdin is None
            else json.dumps(stdin).encode()
        )
        merged = {**self.env, **(env or {})}
        merged = {k: v for k, v in merged.items() if v is not None}
        result = subprocess.run(
            [*implementation(impl), *args],
            input=data,
            capture_output=True,
            env=merged,
            cwd=str(cwd or self.root),
        )
        return Run(result.stdout, result.stderr, result.returncode)

    def event(self, call: str | None = "toolu_1", **fields: Any) -> dict[str, Any]:
        body: dict[str, Any] = {
            "session_id": SESSION,
            "cwd": str(self.repo),
            "tool_name": "Bash",
            "tool_input": {"command": "git commit -m work"},
        }
        if call is not None:
            body["tool_use_id"] = call
        body.update(fields)
        return body

    # -- observing -------------------------------------------------------------

    def observe(self) -> dict[str, Any]:
        """Every file the run can have left behind, keyed by path, with git's
        own churn (objects, index, logs) left out in favour of the refs that
        matter. The pending state's timestamp is reduced to its type, being
        the one value that is the clock and nothing else."""
        seen: dict[str, Any] = {}
        for path in sorted(self.root.rglob("*")):
            relative = path.relative_to(self.root).as_posix()
            parts = relative.split("/")
            if ".git" in parts:
                at = parts.index(".git")
                inside = parts[at + 1 :]
                keep = inside[:1] == ["loch"] or (
                    inside[:1] == ["worktrees"] and "loch" in inside
                )
                if not keep:
                    continue
            if path.is_dir():
                seen[relative] = "dir"
                continue
            if "/loch/pending/" in f"/{relative}":
                try:
                    state = json.loads(path.read_text())
                except ValueError:
                    state = path.read_text()
                seen[relative] = (
                    {"head": state.get("head"), "at": type(state.get("at")).__name__}
                    if isinstance(state, dict)
                    else state
                )
                continue
            seen[relative] = path.read_bytes()
        for repo in {self.repo, *self.root.glob("wt*")}:
            if (repo / ".git").exists():
                for ref in ("refs/notes/loch", "HEAD"):
                    seen[f"{repo.name}:{ref}"] = self._git_quiet(
                        repo, "rev-parse", "-q", "--verify", ref
                    )
                seen[f"{repo.name}:rewriteRef"] = self._git_quiet(
                    repo, "config", "--get", "notes.rewriteRef"
                )
        return seen

    def _git_quiet(self, repo: Path, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            env=self.env,
        ).stdout.strip()


Scenario = Callable[[World, Callable[..., Run]], list[Run]]


class Renamer:
    """Generated v4 UUIDs → `UUID-n`, numbered by first appearance."""

    def __init__(self) -> None:
        self.names: dict[bytes, bytes] = {}

    def __call__(self, data: bytes) -> bytes:
        def name(match: re.Match[bytes]) -> bytes:
            found = match.group(0)
            if found not in self.names:
                self.names[found] = f"UUID-{len(self.names) + 1}".encode()
            return self.names[found]

        return GENERATED.sub(name, data)


def normalized(
    runs: list[Run], seen: dict[str, Any]
) -> tuple[list[Any], dict[str, Any]]:
    rename = Renamer()
    out_runs = [
        {"stdout": rename(r.stdout), "stderr": rename(r.stderr), "status": r.status}
        for r in runs
    ]
    out_seen: dict[str, Any] = {}
    for key in sorted(seen, key=lambda k: rename(k.encode())):
        value = seen[key]
        if isinstance(value, bytes):
            value = rename(value)
        out_seen[rename(key.encode()).decode()] = value
    return out_runs, out_seen


def digest_of(value: Any) -> str:
    """What a failure may show: a hash, never transcript content."""
    if isinstance(value, bytes):
        return hashlib.sha256(value).hexdigest()[:16]
    return hashlib.sha256(repr(value).encode()).hexdigest()[:16]


def first_difference(py: Any, rs: Any) -> str:
    """Where two observations part, named by run index or file path. File
    contents are reported by hash: a scenario's files are synthetic, but the
    corpus checks share this and theirs are not."""
    py_runs, py_seen = py
    rs_runs, rs_seen = rs
    for index, (a, b) in enumerate(zip(py_runs, rs_runs)):
        for stream in ("stdout", "stderr", "status"):
            if a[stream] != b[stream]:
                return f"run {index} {stream} differs:\n  py: {a[stream]!r}\n  rs: {b[stream]!r}"
    if len(py_runs) != len(rs_runs):
        return f"{len(py_runs)} runs against {len(rs_runs)}"
    only_py = sorted(set(py_seen) - set(rs_seen))
    only_rs = sorted(set(rs_seen) - set(py_seen))
    if only_py or only_rs:
        return f"files only in py: {only_py}; only in rs: {only_rs}"
    for key in py_seen:
        if py_seen[key] != rs_seen[key]:
            a, b = py_seen[key], rs_seen[key]
            if isinstance(a, bytes) and isinstance(b, bytes) and len(a) < 4000:
                return f"{key} differs:\n  py: {a!r}\n  rs: {b!r}"
            return f"{key} differs: py {digest_of(a)} rs {digest_of(b)}"
    return "no difference"


def twin(
    tmp_path: Path,
    scenario: Scenario,
    mix: tuple[str, str] | None = None,
    quiet: bool = False,
) -> dict[str, Any]:
    """Runs `scenario` once per implementation in identical worlds at the same
    path, and asserts nothing observable differs. With `mix`, the second run
    alternates implementations call by call — pre under one, post under the
    other — which is what a machine switching over mid-session does. `quiet`
    reports only hashes, for scenarios over real transcripts."""
    root = tmp_path / "world"
    outcomes = []
    plans: list[tuple[str, ...]] = [("py",), ("rs",)] if mix is None else [("py",), mix]
    raw_seen: list[dict[str, Any]] = []
    for plan in plans:
        if root.exists():
            shutil.rmtree(root)
        world = World(root)
        calls = iter(range(10_000))

        def loch(
            *args: str, _w: World = world, _p: tuple[str, ...] = plan, **kw: Any
        ) -> Run:
            return _w.loch(_p[next(calls) % len(_p)], *args, **kw)

        runs = scenario(world, loch)
        seen = world.observe()
        raw_seen.append(seen)
        outcomes.append(normalized(runs, seen))
    if outcomes[0] != outcomes[1]:
        if quiet:
            raise AssertionError("builds differ (details withheld: real transcripts)")
        raise AssertionError(first_difference(outcomes[0], outcomes[1]))
    return raw_seen[0]
