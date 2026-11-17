"""The Rust build against the Python reference, on the same inputs.

Opt-in: set LOCH_RUST to a built binary (`cargo build --release` in rust/, then
LOCH_RUST=rust/target/release/loch). Every scenario is built twice at the same
path with pinned git dates, so commit hashes, note objects and the notes ref
come out byte-identical; one copy is driven by `python -m loch.cli`, the other
by the binary, and everything observable is compared: stdout, stderr, exit
status, the notes ref, the pending-state directory and the store.

With LOCH_CORPUS=1 as well, the transcript-reading path is run against this
machine's real transcripts. Their content only ever exists in a temporary
directory and in memory; failures report session ids and counts, never text.
"""

import hashlib
import json
import os
import random
import shutil
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import pytest

RUST = os.environ.get("LOCH_RUST")

pytestmark = pytest.mark.skipif(not RUST, reason="set LOCH_RUST to a Rust build")

SESSION = "11111111-2222-3333-4444-555555555555"
# In the future, so the hook's "committed after the call began" bound admits
# commits made during a call; fixed, so their hashes are too.
NOW = "2031-01-01T00:00:00Z"
PAST = "2020-01-01T00:00:00Z"


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
        self.env = {
            "PATH": os.environ["PATH"],
            "HOME": str(self.home),
            "LOCH_STORE": str(self.store),
            "GIT_CONFIG_GLOBAL": str(root / "gitconfig"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Test",
            "GIT_AUTHOR_EMAIL": "test@example.com",
            "GIT_COMMITTER_NAME": "Test",
            "GIT_COMMITTER_EMAIL": "test@example.com",
            "GIT_AUTHOR_DATE": NOW,
            "GIT_COMMITTER_DATE": NOW,
            "PYTHONPATH": str(Path(__file__).resolve().parent.parent),
        }

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
        result = subprocess.run(
            [*implementation(impl), *args],
            input=data,
            capture_output=True,
            env={**self.env, **(env or {})},
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

    def observe(self) -> dict[str, Any]:
        """Everything a hook can leave behind, in a form two runs can be
        compared on. The one nondeterministic value — the pending state's
        timestamp — is reduced to its type."""
        seen: dict[str, Any] = {}
        for path in sorted(self.root.rglob("*")):
            relative = path.relative_to(self.root).as_posix()
            if "/.git/objects/" in f"/{relative}" or relative.startswith("home/.loch/"):
                continue
            if "/loch/pending/" in relative and path.is_file():
                try:
                    state = json.loads(path.read_text())
                except ValueError:
                    state = path.read_text()
                seen[relative] = (
                    {"head": state.get("head"), "at": type(state.get("at")).__name__}
                    if isinstance(state, dict)
                    else state
                )
            elif path.is_file() and (
                relative.startswith("store/") or relative.endswith("/NOTES_EDITMSG")
            ):
                seen[relative] = hashlib.sha256(path.read_bytes()).hexdigest()
            elif path.is_dir() and "/loch" in relative:
                seen[relative] = "dir"
        for repo in {self.repo, *self.root.glob("wt*")}:
            if (repo / ".git").exists():
                seen[f"notes:{repo.name}"] = subprocess.run(
                    [
                        "git",
                        "-C",
                        str(repo),
                        "rev-parse",
                        "-q",
                        "--verify",
                        "refs/notes/loch",
                    ],
                    capture_output=True,
                    text=True,
                    env=self.env,
                ).stdout.strip()
        return seen


Scenario = Callable[[World, Callable[..., Run]], list[Run]]


def twin(
    tmp_path: Path, scenario: Scenario, mix: tuple[str, str] | None = None
) -> dict[str, Any]:
    """Runs `scenario` once per implementation in identical worlds at the same
    path, and asserts nothing observable differs. With `mix`, the second run
    alternates implementations call by call — pre under one, post under the
    other — which is what a machine switching over mid-session does."""
    root = tmp_path / "world"
    outcomes = []
    plans: list[tuple[str, ...]] = [("py",), ("rs",)] if mix is None else [("py",), mix]
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
        outcomes.append(([vars(run) for run in runs], world.observe()))
    (py_runs, py_seen), (rs_runs, rs_seen) = outcomes
    assert rs_runs == py_runs
    assert rs_seen == py_seen
    return py_seen


# -- scenarios ----------------------------------------------------------------


def no_commit(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    return [
        loch("hook", "pre-tool", stdin=w.event()),
        loch("hook", "post-tool", stdin=w.event()),
    ]


def one_commit(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    pre = loch("hook", "pre-tool", stdin=w.event())
    w.commit("agent work")
    return [pre, loch("hook", "post-tool", stdin=w.event())]


def two_commits(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    pre = loch("hook", "pre-tool", stdin=w.event())
    w.commit("one")
    w.commit("two")
    return [pre, loch("hook", "post-tool", stdin=w.event())]


def checkout_is_not_committing(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("base", date=PAST)
    w.git("checkout", "-q", "-b", "side")
    w.commit("someone else", date=PAST)
    w.git("checkout", "-q", "main")
    pre = loch("hook", "pre-tool", stdin=w.event())
    w.git("checkout", "-q", "side")
    return [pre, loch("hook", "post-tool", stdin=w.event())]


def unborn_head(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    pre = loch("hook", "pre-tool", stdin=w.event())
    w.commit("root")
    return [pre, loch("hook", "post-tool", stdin=w.event())]


def call_from_transcript(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    transcript = w.root / "t.jsonl"
    lines = [
        {
            "type": "assistant",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "toolu_old",
                        "name": "Bash",
                        "input": {"command": "git commit -m work"},
                    }
                ]
            },
        },
        "not json",
        [1, 2],
        {"type": "assistant", "message": {"content": "plain"}},
        {
            "type": "assistant",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "toolu_new",
                        "name": "Bash",
                        "input": {"command": "git commit -m work"},
                    },
                    {
                        "type": "tool_use",
                        "id": "toolu_other",
                        "name": "Bash",
                        "input": {"command": "ls"},
                    },
                ]
            },
        },
    ]
    transcript.write_text(
        "\n".join(x if isinstance(x, str) else json.dumps(x) for x in lines) + "\n"
    )
    event = w.event(call=None, transcript_path=str(transcript))
    pre = loch("hook", "pre-tool", stdin=event)
    w.commit("agent work")
    return [pre, loch("hook", "post-tool", stdin=event)]


def no_call_anywhere(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    event = w.event(call=None)
    pre = loch("hook", "pre-tool", stdin=event)
    w.commit("agent work")
    return [pre, loch("hook", "post-tool", stdin=event)]


def worktree(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    wt = w.root / "wt1"
    w.git("worktree", "add", "-q", "-b", "feature", str(wt))
    event = w.event(cwd=str(wt / "."))
    pre = loch("hook", "pre-tool", stdin=event)
    w.commit("in the worktree", cwd=wt)
    return [pre, loch("hook", "post-tool", stdin=event)]


def packed_refs(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    w.git("pack-refs", "--all")
    pre = loch("hook", "pre-tool", stdin=w.event())
    w.commit("after packing")
    w.git("pack-refs", "--all")
    return [pre, loch("hook", "post-tool", stdin=w.event())]


def detached(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    w.git("checkout", "-q", "--detach")
    pre = loch("hook", "pre-tool", stdin=w.event())
    w.commit("detached work")
    return [pre, loch("hook", "post-tool", stdin=w.event())]


def subdirectory_and_symlink(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    (w.repo / "sub" / "deeper").mkdir(parents=True)
    os.symlink(w.repo / "sub", w.root / "link")
    event = w.event(cwd=str(w.root / "link" / "deeper"))
    pre = loch("hook", "pre-tool", stdin=event)
    w.commit("from a subdirectory")
    return [pre, loch("hook", "post-tool", stdin=event)]


def reftable(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init("--ref-format=reftable")
    w.commit("first")
    pre = loch("hook", "pre-tool", stdin=w.event())
    w.commit("agent work")
    return [pre, loch("hook", "post-tool", stdin=w.event())]


def git_dir_variable(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    other = w.root / "elsewhere"
    other.mkdir()
    env = {"GIT_DIR": str(w.repo / ".git")}
    event = w.event(cwd=str(other))
    pre = loch("hook", "pre-tool", stdin=event, env=env)
    w.commit("agent work")
    return [pre, loch("hook", "post-tool", stdin=event, env=env)]


def malformed(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    runs = []
    for stdin in [
        b"not json",
        b"[1, 2]",
        b"\xff\xfe",
        b"",
        {**w.event(), "tool_input": "a string"},
        {**w.event(call=None), "tool_input": "a string"},
        {**w.event(call=None), "tool_input": {"command": 5}},
        {k: v for k, v in w.event().items() if k != "session_id"},
        {**w.event(), "session_id": 7},
        {**w.event(), "cwd": str(w.root / "missing")},
        {**w.event(), "cwd": str(w.home)},
        {**w.event(), "cwd": 12},
    ]:
        for name in ("pre-tool", "post-tool"):
            runs.append(loch("hook", name, stdin=stdin))
    runs.append(loch("hook", "bogus", stdin=w.event()))
    runs.append(loch("hook", stdin=w.event()))
    return runs


def stale_state(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    pending = w.repo / ".git" / "loch" / "pending"
    pending.mkdir(parents=True)
    (pending / "toolu_a").write_text("[]")
    (pending / "toolu_b").write_text('{"head": "x"}')
    (pending / "toolu_c").write_text("garbage")
    runs = []
    w.commit("moved")
    for call in ("toolu_a", "toolu_b", "toolu_c", "toolu_missing"):
        runs.append(loch("hook", "post-tool", stdin=w.event(call=call)))
    return runs


def json_mode(w: World, loch: Callable[..., Run]) -> list[Run]:
    w.init()
    w.commit("first")
    return [
        loch("hook", "pre-tool", "--json", stdin=w.event()),
        loch("--json", "hook", "post-tool", stdin=w.event()),
        loch("--version"),
        loch("--version", "--json"),
        loch("version", "--json"),
        loch("--json", "-V"),
        loch("nonsense"),
        loch("nonsense", "--json"),
    ]


def stop(w: World, loch: Callable[..., Run]) -> list[Run]:
    project = w.home / ".claude" / "projects" / "-repo"
    project.mkdir(parents=True)
    transcript = project / f"{SESSION}.jsonl"
    transcript.write_bytes(b'{"type": "user"}\n\xff not utf-8\n')
    event = {"session_id": SESSION, "transcript_path": str(transcript)}
    uri = "file://" + str(w.root / "file store").replace(" ", "%20")
    return [
        loch("hook", "stop", stdin=event),
        loch("hook", "stop", stdin=event, env={"LOCH_STORE": uri}),
        loch("hook", "stop", stdin=event, env={"LOCH_STORE": "relative/store"}),
        loch("hook", "stop", stdin={**event, "transcript_path": str(w.root / "gone")}),
        loch("hook", "stop", stdin={"session_id": SESSION}),
        loch("hook", "stop", stdin={**event, "session_id": 3}),
    ]


def stop_default_store(w: World, loch: Callable[..., Run]) -> list[Run]:
    project = w.home / ".claude" / "projects" / "-repo"
    project.mkdir(parents=True)
    transcript = project / f"{SESSION}.jsonl"
    transcript.write_text('{"type": "user"}\n')
    event = {"session_id": SESSION, "transcript_path": str(transcript)}
    run = loch("hook", "stop", stdin=event, env={"LOCH_STORE": ""})
    # Moved where observe() compares stores.
    snapshot = w.home / ".loch" / "store"
    if snapshot.exists():
        shutil.copytree(snapshot, w.store)
    return [run]


SCENARIOS = [
    no_commit,
    one_commit,
    two_commits,
    checkout_is_not_committing,
    unborn_head,
    call_from_transcript,
    no_call_anywhere,
    worktree,
    packed_refs,
    detached,
    subdirectory_and_symlink,
    reftable,
    git_dir_variable,
    malformed,
    stale_state,
    json_mode,
    stop,
    stop_default_store,
]


@pytest.mark.parametrize("scenario", SCENARIOS, ids=lambda s: s.__name__)
def test_hook_matches_python(tmp_path: Path, scenario: Scenario) -> None:
    twin(tmp_path, scenario)


@pytest.mark.parametrize(
    "mix", [("py", "rs"), ("rs", "py")], ids=["py-then-rs", "rs-then-py"]
)
@pytest.mark.parametrize(
    "scenario",
    [one_commit, two_commits, unborn_head, call_from_transcript, worktree],
    ids=lambda s: s.__name__,
)
def test_switching_over_mid_call(
    tmp_path: Path, scenario: Scenario, mix: tuple[str, str]
) -> None:
    """A Bash call whose pre hook ran under one implementation and whose post
    hook ran under the other, as happens the moment a machine switches."""
    twin(tmp_path, scenario, mix=mix)


@dataclass
class S3:
    signed: bool
    endpoint: str
    access_key: str
    secret_key: str
    client: Any


@pytest.fixture(params=["signed", "unsigned"])
def s3(request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch) -> Any:
    """A real HTTP endpoint rather than boto3's in-process mock, since the Rust
    build signs its own requests. "signed" turns moto's IAM auth on, so a
    wrong signature is refused rather than waved through; the first four
    actions, which create the user who signs everything after, go unchecked.

    "unsigned" exists because moto's verifier rejects boto3's own correct
    signature on a key holding a space or `+`, so key encoding — which real
    project directories exercise — can only be compared with auth off."""
    import urllib.request

    import boto3
    import moto.settings
    from moto.core.responses import ActionAuthenticatorMixin
    from moto.server import ThreadedMotoServer

    server = ThreadedMotoServer(ip_address="127.0.0.1", port=0)
    server.start()
    host, port = server.get_host_and_port()
    endpoint = f"http://{host}:{port}"
    # moto's state belongs to the process, not the server; start clean.
    urllib.request.urlopen(urllib.request.Request(f"{endpoint}/moto-api/reset", b""))
    if request.param == "signed":
        monkeypatch.setattr(moto.settings, "INITIAL_NO_AUTH_ACTION_COUNT", 4)
        monkeypatch.setattr(ActionAuthenticatorMixin, "request_count", 0)
    iam = boto3.client(
        "iam",
        endpoint_url=endpoint,
        region_name="us-east-1",
        aws_access_key_id="bootstrap",
        aws_secret_access_key="bootstrap",
    )
    iam.create_user(UserName="loch")
    key = iam.create_access_key(UserName="loch")["AccessKey"]
    iam.put_user_policy(
        UserName="loch",
        PolicyName="s3",
        PolicyDocument=json.dumps(
            {
                "Version": "2012-10-17",
                "Statement": [{"Effect": "Allow", "Action": "s3:*", "Resource": "*"}],
            }
        ),
    )
    client = boto3.client(
        "s3",
        endpoint_url=endpoint,
        region_name="us-east-1",
        aws_access_key_id=key["AccessKeyId"],
        aws_secret_access_key=key["SecretAccessKey"],
    )
    client.create_bucket(Bucket="loch-test")
    yield S3(
        request.param == "signed",
        endpoint,
        key["AccessKeyId"],
        key["SecretAccessKey"],
        client,
    )
    server.stop()


def test_stop_snapshots_into_s3_identically(tmp_path: Path, s3: S3) -> None:
    """Each implementation snapshots under its own prefix of one bucket; the
    keys and bytes beneath must match, and a bad secret must write nothing."""
    world = World(tmp_path / "world")
    name = "-Users-x-repo" if s3.signed else "-a b+c%"
    project = world.home / ".claude" / "projects" / name
    project.mkdir(parents=True)
    transcript = project / f"{SESSION}.jsonl"
    transcript.write_bytes(b'{"type": "user"}\n\xff raw bytes \xe2\x9c\x93\n')
    event = {"session_id": SESSION, "transcript_path": str(transcript)}
    signed = {
        "AWS_ACCESS_KEY_ID": s3.access_key,
        "LOCH_S3_ENDPOINT": s3.endpoint,
        "LOCH_S3_REGION": "us-east-1",
        "AWS_SHARED_CREDENTIALS_FILE": os.devnull,
        "AWS_CONFIG_FILE": os.devnull,
    }

    contents: dict[str, dict[str, bytes]] = {}
    for impl in ("py", "rs"):
        runs = [
            world.loch(
                impl,
                "hook",
                "stop",
                stdin=event,
                env={
                    **signed,
                    "AWS_SECRET_ACCESS_KEY": s3.secret_key,
                    "LOCH_STORE": f"s3://loch-test/{impl}/",
                },
            ),
            world.loch(
                impl,
                "hook",
                "stop",
                stdin=event,
                env={
                    **signed,
                    "AWS_SECRET_ACCESS_KEY": "wrong",
                    "LOCH_STORE": f"s3://loch-test/{impl}-badly-signed",
                },
            ),
            world.loch(
                impl,
                "hook",
                "stop",
                stdin=event,
                env={
                    **{k: v for k, v in signed.items() if k != "AWS_ACCESS_KEY_ID"},
                    "LOCH_STORE": f"s3://loch-test/{impl}-anonymous",
                },
            ),
        ]
        # A hook may never fail its tool call, whatever the store says.
        assert [(r.stdout, r.stderr, r.status) for r in runs] == [(b"", b"", 0)] * len(
            runs
        )
        # Unprefixed: moto's verifier also rejects boto3's own signature on a
        # listing whose prefix holds a `/`.
        listed = s3.client.list_objects_v2(Bucket="loch-test")
        contents[impl] = {
            item["Key"]: s3.client.get_object(Bucket="loch-test", Key=item["Key"])[
                "Body"
            ].read()
            for item in listed.get("Contents", [])
            if item["Key"].startswith(f"{impl}/")
        }
    assert list(contents["py"]) == [
        f"py/transcripts/claude-code/{name}/{SESSION}.jsonl"
    ]
    assert list(contents["py"].values()) == [transcript.read_bytes()]
    assert {k.replace("rs/", "py/", 1): v for k, v in contents["rs"].items()} == (
        contents["py"]
    )
    if s3.signed:
        # Neither the bad secret nor the absent key got anything in.
        every = s3.client.list_objects_v2(Bucket="loch-test").get("Contents", [])
        assert sorted(item["Key"].split("/")[0] for item in every) == ["py", "rs"]


# -- corpus -------------------------------------------------------------------


def _corpus_cases(limit: int) -> list[tuple[str, str]]:
    from loch.claude import projects_dir

    root = Path(projects_dir())
    transcripts = sorted(root.glob("*/*.jsonl")) if root.is_dir() else []
    random.Random(0).shuffle(transcripts)
    cases = []
    for path in transcripts:
        commands = []
        for line in path.read_bytes().split(b"\n"):
            if b'"Bash"' not in line:
                continue
            try:
                record = json.loads(line)
            except ValueError:
                continue
            content = (record.get("message") or {}).get("content")
            for block in content if isinstance(content, list) else []:
                if (
                    isinstance(block, dict)
                    and block.get("type") == "tool_use"
                    and block.get("name") == "Bash"
                    and isinstance((block.get("input") or {}).get("command"), str)
                ):
                    commands.append(block["input"]["command"])
        if commands:
            cases.append((str(path), random.Random(path.name).choice(commands)))
        if len(cases) >= limit:
            break
    return cases


@pytest.mark.skipif(os.environ.get("LOCH_CORPUS") != "1", reason="set LOCH_CORPUS=1")
def test_call_recovery_matches_on_real_transcripts(tmp_path: Path) -> None:
    """The one hook path that reads a transcript, on real ones: a commit made
    by a call whose payload lacks tool_use_id must be noted with the same call
    id by both implementations."""
    cases = _corpus_cases(int(os.environ.get("LOCH_CORPUS_LIMIT", "150")))
    assert cases, "no transcripts with Bash calls on this machine"
    mismatched = []
    unnoted = 0
    for index, (path, command) in enumerate(cases):
        session = Path(path).stem

        def scenario(
            w: World, loch: Callable[..., Run], _p: str = path, _c: str = command
        ) -> list[Run]:
            w.init()
            w.commit("first")
            event = w.event(call=None, transcript_path=_p, tool_input={"command": _c})
            pre = loch("hook", "pre-tool", stdin=event)
            w.commit("agent work")
            return [pre, loch("hook", "post-tool", stdin=event)]

        try:
            seen = twin(tmp_path / str(index), scenario)
        except AssertionError:
            mismatched.append(session)
            continue
        # Agreeing that nothing happened would pass vacuously.
        unnoted += not seen["notes:repo"]
    assert not unnoted, f"{unnoted}/{len(cases)} cases wrote no note"
    assert not mismatched, (
        f"{len(mismatched)}/{len(cases)} sessions differ: {mismatched}"
    )
