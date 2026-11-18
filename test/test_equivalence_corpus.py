"""The Rust build against the Python reference on this machine's real
transcripts: every project directory is listed, saved, restored onto a
different home and ingested by both builds, and everything they write is
compared.

Opt-in twice over — LOCH_RUST and LOCH_CORPUS=1 — and read-only towards
`~/.claude`: restores go to a temporary HOME, stores and derived files to a
temporary directory. Content never leaves memory; a failure names the project
directory, the command, and a byte offset or a hash.
"""

import os
import re
from pathlib import Path

import pytest
from equivalence import RUST, Run, World, twin

from loch.claude import projects_dir, read_session_meta

pytestmark = pytest.mark.skipif(
    not RUST or os.environ.get("LOCH_CORPUS") != "1",
    reason="set LOCH_RUST and LOCH_CORPUS=1",
)

REAL_HOME = str(Path.home())


def project_cwds() -> list[tuple[str, str]]:
    """(directory name, the cwd its sessions are filed under), as list would
    be asked for it. A directory whose name no session's cwd encodes to — a
    hand-copied one — can't be reached by --cwd and is skipped."""
    root = Path(projects_dir())
    found = []
    for directory in sorted(root.iterdir()) if root.is_dir() else []:
        for transcript in sorted(directory.glob("*.jsonl")):
            meta = read_session_meta(str(transcript))
            if meta and re.sub(r"[^a-zA-Z0-9]", "-", meta.cwd) == directory.name:
                found.append((directory.name, meta.cwd))
                break
    limit = int(os.environ.get("LOCH_CORPUS_DIRS", "0"))
    return found[:limit] if limit else found


def test_no_transcript_uses_the_surrogate_mapping_range() -> None:
    """The Rust build carries lone surrogates in U+10F800..U+10FFFF (py.rs);
    a transcript holding those code points literally would be misread."""
    reserved = re.compile("[\U0010f800-\U0010ffff]")
    hits = [
        path.parent.name
        for path in Path(projects_dir()).glob("*/*.jsonl")
        if reserved.search(path.read_bytes().decode("utf-8", "surrogateescape"))
    ]
    assert not hits, f"{len(hits)} transcripts use the reserved range: {hits[:5]}"


def test_every_project_directory_matches(tmp_path: Path) -> None:
    cases = project_cwds()
    assert cases, "no Claude Code sessions on this machine"
    mismatched: list[str] = []
    real = {"HOME": REAL_HOME, "CLAUDE_CONFIG_DIR": os.environ.get("CLAUDE_CONFIG_DIR")}
    for index, (directory, cwd) in enumerate(cases):

        def scenario(w: World, loch, _cwd: str = cwd) -> list[Run]:
            store = str(w.root / "store")
            runs = [
                loch("list", "--cwd", _cwd, env=real),
                loch("list", "--cwd", _cwd, "--json", env=real),
                loch("save", "--cwd", _cwd, "--store", store, "--json", env=real),
                loch(
                    "ingest",
                    "--cwd",
                    _cwd,
                    "--out",
                    str(w.root / "derived"),
                    "--json",
                    env=real,
                ),
            ]
            bundles = sorted((w.root / "store" / "bundles").glob("*.loch"))
            if bundles:
                target = w.root / "target"
                target.mkdir()
                # The temporary home: a different machine, and nowhere real.
                runs.append(
                    loch(
                        "restore",
                        bundles[0].stem,
                        "--store",
                        store,
                        "--into",
                        str(target),
                        "--json",
                    )
                )
            return runs

        try:
            twin(tmp_path / str(index), scenario, quiet=True)
        except AssertionError as error:
            mismatched.append(f"{directory}: {error}")
    assert not mismatched, (
        f"{len(mismatched)}/{len(cases)} directories differ:\n" + "\n".join(mismatched)
    )
