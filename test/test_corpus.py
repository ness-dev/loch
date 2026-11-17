"""Checks against this machine's real transcripts. Opt-in, since the corpus is
personal and its size is unbounded: `LOCH_CORPUS=1 poetry run pytest
test/test_corpus.py`, optionally with `LOCH_CORPUS_LIMIT=<n>`.

Nothing from a transcript is ever asserted on or printed — only session ids
and counts, so a failure report is safe to paste."""

import os
import uuid
from collections.abc import Iterator
from pathlib import Path

import pytest

from loch.claude import SessionMeta, home_dir, projects_dir, read_session_meta
from loch.format import pack_derived, read_derived
from loch.ingest import Origin, convert, ingest
from loch.rewrite import RewriteSpec, rewrite_transcript

pytestmark = pytest.mark.skipif(
    not os.environ.get("LOCH_CORPUS"), reason="set LOCH_CORPUS=1 to run"
)


def corpus() -> Iterator[SessionMeta]:
    limit = int(os.environ.get("LOCH_CORPUS_LIMIT", "0")) or None
    root = projects_dir()
    count = 0
    for directory in sorted(os.listdir(root)):
        path = os.path.join(root, directory)
        if not os.path.isdir(path):
            continue
        for name in sorted(os.listdir(path)):
            if not name.endswith(".jsonl"):
                continue
            meta = read_session_meta(os.path.join(path, name))
            if meta is None:
                continue
            yield meta
            count += 1
            if limit is not None and count >= limit:
                return


def test_every_session_ingests_and_reads_back_verified() -> None:
    failures = []
    for meta in corpus():
        origin = Origin(meta.cwd, home_dir(), meta.session_id)
        packed = pack_derived(ingest(meta.path, origin))
        if pack_derived(ingest(meta.path, origin)) != packed:
            failures.append((meta.session_id, "not idempotent"))
            continue
        lines = packed.decode("utf-8", "surrogatepass").split("\n")
        if pack_derived(read_derived(lines)) != packed:
            failures.append((meta.session_id, "does not round-trip"))
    assert failures == []


def test_turn_ids_survive_a_restore_to_another_machine() -> None:
    """A transcript that already names the destination paths can't pass: its
    content is indistinguishable from a rewritten root. Those are reported by
    id, to check by hand, rather than failing the run."""
    target_cwd, target_home = "/srv/loch-corpus/checkout", "/home/loch-corpus"
    unstable = []
    for meta in corpus():
        raw = Path(meta.path).read_text(encoding="utf-8")
        new_id = str(uuid.uuid5(uuid.NAMESPACE_URL, meta.session_id))
        spec = RewriteSpec(
            meta.cwd, home_dir(), target_cwd, target_home, meta.session_id, new_id
        )
        before = convert(raw, Origin(meta.cwd, home_dir(), meta.session_id), {})
        after = convert(
            rewrite_transcript(raw, spec), Origin(target_cwd, target_home, new_id), {}
        )
        if [t.id for t in before[0]] != [t.id for t in after[0]]:
            if target_cwd not in raw and target_home not in raw:
                unstable.append(meta.session_id)
    assert unstable == []
