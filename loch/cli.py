import argparse
import importlib.metadata
import json
import os
import platform
import re
import shutil
import socket
import sys
import tempfile
import time
import uuid
import webbrowser
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, NoReturn

from .blame_html import parse_ranges, render
from .bundle import (
    BUNDLE_VERSION,
    Bundle,
    BundleOrigin,
    BundleSession,
    bundle_ref,
    pack_bundle,
    unpack_bundle,
)
from .claude import (
    AGENT,
    SessionMeta,
    home_dir,
    iso_timestamp,
    list_sessions,
    projects_dir,
    resolve_cwd,
    transcript_path_for,
)
from .format import pack_derived
from .git import current_branch
from .index import (
    BRANCH,
    IndexEntry,
    bundle_key,
    delete_bundle,
    dimension_prefix,
    index_bundle,
    index_key,
    load_entries,
    reindex,
    value_prefix,
)
from .ingest import (
    CONVERTER,
    AuthorKeyError,
    Origin,
    derived_name,
    ingest,
    ssh_author_id,
)
from .output import JSON_FLAG, CommandError, Result, emit, emit_error
from .redact import RedactionError, redact, summarize
from .rewrite import (
    ForeignCwd,
    RewriteSpec,
    foreign_cwds,
    residual_origin_paths,
    rewrite_transcript,
)
from .store import MissingDependency, MissingObject, create_store, resolve_store_uri
from .trace import (
    NOTES_REF,
    UNCOMMITTED,
    Link,
    Resolver,
    TraceError,
    backfill,
    blame_hunks,
    fork_session,
    git,
    history,
    hook_post_tool,
    hook_pre_tool,
    install_hooks,
    repo_root,
    snapshot_transcript,
)

DEFAULT_DERIVED = os.path.join(str(Path.home()), ".loch", "derived")

USAGE = """loch — save and restore agent coding sessions across machines

Usage:
  loch list    [--cwd <path>] [--branch <name>]
  loch list    --remote [--branch <name>] [--all] [--store <uri>]
  loch save    [--cwd <path>] [--branch <name>] [--session <id>] [--store <uri>]
  loch restore <ref> [--into <path>] [--store <uri>] [--force] [--new-id]
  loch delete  <ref> [--store <uri>]
  loch reindex [--store <uri>]
  loch ingest  [--cwd <path>] [--branch <name>] [--session <id>]
               [--author <id> | --author-key <key.pub>]
               [--out <dir>]
  loch install  [--local] [--command <loch>]
  loch blame   <file> [--html [<path>] [--lines <ranges>] [--no-open]]
               [--store <uri>]
  loch log     <file>[:<line>[-<end>]] [--store <uri>]
  loch ask     <commit | file:line> [<question>] [--at-commit] [--no-launch]
               [--store <uri>]
  loch backfill [--revision <rev>]   (default: every branch)
  loch hook    pre-tool | post-tool | stop      (run by Claude Code)
  loch --version

Every command takes --json, which replaces the text below with one machine-
readable document on stdout.

Stores:
  <path>              local directory
  s3://bucket/prefix  any S3-compatible endpoint

Environment:
  LOCH_STORE        default store URI
  LOCH_S3_ENDPOINT  custom endpoint (R2, MinIO, ...)
  LOCH_S3_REGION    region override
  CLAUDE_CONFIG_DIR Claude Code's state directory (default ~/.claude)

Derived conversation files go to ~/.loch/derived unless --out says otherwise.

`install` adds Claude Code hooks to a repository that note, on every commit an
agent makes, the conversation and tool call that made it (git notes on
refs/notes/loch), and snapshot each session's transcript into the store.
`blame` shows which conversation wrote each line, `log` every conversation
that changed a file or line range over its history; `ask` forks that
conversation and resumes it in Claude Code — from its end, or with
--at-commit from just after the commit.
"""


def fail(code: str, message: str) -> NoReturn:
    raise CommandError(code, message)


@contextmanager
def store_errors(description: str) -> Iterator[None]:
    """The three failures a caller can actually act on: an object that isn't
    there is terminal, a store that didn't answer is worth retrying, and a
    backend whose SDK is absent is fixed by an install and by nothing else.
    Keep the block around the store call alone — anything wider mislabels a bug
    in this process as a failure of the store."""
    try:
        yield
    except MissingObject as error:
        fail("bundle-not-found", f"{description}: {error}")
    except MissingDependency as error:
        fail("s3-extra-missing", f"{description}: {error}")
    except Exception as error:
        fail("store-unreachable", f"{description}: {error}")


def format_bytes(count: int) -> str:
    if count < 1024:
        return f"{count}B"
    if count < 1024 * 1024:
        return f"{count / 1024:.0f}K"
    return f"{count / (1024 * 1024):.1f}M"


def describe(meta: SessionMeta) -> str:
    branch = f"[{meta.git_branch}]" if meta.git_branch else "[no branch]"
    # Hoisted out of the f-string: a backslash inside an interpolation is
    # PEP 701 syntax and won't parse below 3.12.
    collapsed = re.sub(r"\s+", " ", meta.summary) if meta.summary else ""
    summary = f" {collapsed}" if collapsed else ""
    when = meta.modified_at[:16].replace("T", " ")
    return (
        f"{meta.session_id}  {when}  {format_bytes(meta.bytes):>5}  {branch}{summary}"
    )


def describe_entry(entry: IndexEntry) -> str:
    branch = f"[{entry.value}]" if entry.value else "[no branch]"
    when = entry.created_at[:16].replace("T", " ")
    version = f"  claude {entry.claude_version}" if entry.claude_version else ""
    return (
        f"{when}  {format_bytes(entry.bytes):>5}  {entry.sessions} session(s)  "
        f"{branch}{version}  {entry.cwd}"
    )


def collect(
    cwd: str | None, branch: str | None, session: str | None = None
) -> tuple[str, list[SessionMeta]]:
    resolved = resolve_cwd(cwd if cwd is not None else os.getcwd())
    return resolved, list_sessions(cwd=resolved, branch=branch, session_id=session)


class HelpRequested(Exception):
    def __init__(self, text: str) -> None:
        super().__init__(text)
        self.text = text


class _Parser(argparse.ArgumentParser):
    # argparse answers -h and a bad flag itself, writing prose and exiting
    # before a command ever runs. Both are routed back to main() so that
    # "one document per invocation" holds in JSON mode; the text each carries
    # is argparse's own, so nothing changes without --json.
    def error(self, message: str) -> NoReturn:
        raise CommandError(
            "usage",
            message,
            prose=f"{self.format_usage()}{self.prog}: error: {message}\n",
            exit_status=2,
        )

    def print_help(self, file: Any = None) -> NoReturn:
        raise HelpRequested(self.format_help())


def _parser(command: str) -> argparse.ArgumentParser:
    parser = _Parser(prog=f"loch {command}")
    # Registered so the flag parses after the subcommand; main() reads the mode
    # off argv itself, because a failure before or during parsing has to render
    # in the mode the caller asked for.
    parser.add_argument(JSON_FLAG, action="store_true")
    return parser


def _session_payload(meta: SessionMeta) -> dict[str, Any]:
    return {
        "sessionId": meta.session_id,
        "path": meta.path,
        "cwd": meta.cwd,
        "otherCwds": list(meta.other_cwds),
        "branch": meta.git_branch,
        "claudeVersion": meta.claude_version,
        "bytes": meta.bytes,
        "modifiedAt": meta.modified_at,
        "summary": meta.summary,
    }


def _entry_payload(entry: IndexEntry) -> dict[str, Any]:
    """Deliberately not index.py's stored shape: that one is a storage format,
    this one is a versioned wire contract, and they change for different
    reasons."""
    return {
        "ref": entry.ref,
        "dimension": entry.dimension,
        "value": entry.value,
        "agent": entry.agent,
        "createdAt": entry.created_at,
        "bytes": entry.bytes,
        "sessions": entry.sessions,
        "cwd": entry.cwd,
        "claudeVersion": entry.claude_version,
    }


def command_list(argv: list[str]) -> Result:
    parser = _parser("list")
    parser.add_argument("--cwd")
    parser.add_argument("--branch")
    parser.add_argument("--remote", action="store_true")
    parser.add_argument("--all", action="store_true", dest="every")
    parser.add_argument("--store")
    args = parser.parse_args(argv)

    if args.remote or args.every or args.store is not None:
        return list_remote(args)

    cwd, sessions = collect(args.cwd, args.branch)
    return Result(
        command="list",
        payload={
            "remote": False,
            "cwd": cwd,
            "branch": args.branch,
            "sessions": [_session_payload(meta) for meta in sessions],
        },
        text=_list_text(cwd, sessions),
    )


def _list_text(cwd: str, sessions: list[SessionMeta]) -> str:
    if not sessions:
        return f"no {AGENT} sessions found for {cwd}\n"
    lines = [f"{len(sessions)} session(s) for {cwd}\n"]
    lines += [f"  {describe(meta)}\n" for meta in sessions]
    return "".join(lines)


def list_remote(args: argparse.Namespace) -> Result:
    store = create_store(resolve_store_uri(args.store))
    cwd = resolve_cwd(args.cwd if args.cwd is not None else os.getcwd())
    branch = args.branch if args.branch is not None else current_branch(cwd)

    prefix = dimension_prefix(BRANCH) if args.every else value_prefix(BRANCH, branch)
    with store_errors(f"could not list {store.describe()}"):
        entries = load_entries(store, prefix)
    refs = {entry.ref for entry in entries}

    return Result(
        command="list",
        payload={
            "remote": True,
            "store": store.describe(),
            "all": bool(args.every),
            "branch": None if args.every else branch,
            "bundles": len(refs),
            "entries": [_entry_payload(entry) for entry in entries],
        },
        text=_list_remote_text(store.describe(), args.every, branch, entries, refs),
    )


def _list_remote_text(
    store: str,
    every: bool,
    branch: str | None,
    entries: list[IndexEntry],
    refs: set[str],
) -> str:
    scope = "" if every else f" for [{branch or 'no branch'}]"
    if not entries:
        return f"no bundles{scope} in {store}\n"
    lines = [f"{len(refs)} bundle(s){scope} in {store}\n"]
    lines += [f"  {entry.ref}\n    {describe_entry(entry)}\n" for entry in entries]
    return "".join(lines)


def command_save(argv: list[str]) -> Result:
    parser = _parser("save")
    parser.add_argument("--cwd")
    parser.add_argument("--branch")
    parser.add_argument("--session")
    parser.add_argument("--store")
    args = parser.parse_args(argv)

    cwd, sessions = collect(args.cwd, args.branch, args.session)
    if not sessions:
        fail("no-sessions", f"no {AGENT} sessions found for {cwd}")

    packed_sessions: list[BundleSession] = []
    redactions: list[dict[str, int]] = []
    for meta in sessions:
        # In memory only — the agent's own transcript on disk stays verbatim.
        try:
            scrubbed = redact(Path(meta.path).read_text(encoding="utf-8"))
        except RedactionError as error:
            fail("redaction-failed", f"refusing to pack {meta.session_id}: {error}")
        packed_sessions.append(
            BundleSession(
                session_id=meta.session_id,
                cwd=meta.cwd,
                git_branch=meta.git_branch,
                claude_version=meta.claude_version,
                modified_at=meta.modified_at,
                transcript=scrubbed.text,
            )
        )
        redactions.append(scrubbed.counts)

    bundle = Bundle(
        version=BUNDLE_VERSION,
        agent=AGENT,
        created_at=iso_timestamp(time.time()),
        origin=BundleOrigin(
            home=home_dir(), platform=_platform_name(), hostname=socket.gethostname()
        ),
        sessions=packed_sessions,
    )

    packed = pack_bundle(bundle)
    ref = bundle_ref(packed)
    store = create_store(resolve_store_uri(args.store))
    with store_errors(f"could not write to {store.describe()}"):
        # Bundle first: an index entry is derived from it, so a pointer to a
        # bundle that isn't there yet is the worse half of a failed save.
        store.put(bundle_key(ref), packed)
        entries = index_bundle(store, bundle, ref, len(packed))

    return Result(
        command="save",
        payload={
            "ref": ref,
            "bytes": len(packed),
            "store": store.describe(),
            "cwd": cwd,
            "createdAt": bundle.created_at,
            # Counts by rule name only. The matched text never leaves this
            # process: save's stdout lands in the next agent's transcript.
            "redacted": sum(sum(counts.values()) for counts in redactions),
            "sessions": [
                {
                    "sessionId": session.session_id,
                    "branch": session.git_branch,
                    "cwd": session.cwd,
                    "claudeVersion": session.claude_version,
                    "modifiedAt": session.modified_at,
                    "redactions": counts,
                }
                for session, counts in zip(bundle.sessions, redactions)
            ],
            "indexed": [
                {
                    "dimension": entry.dimension,
                    "value": entry.value,
                    "key": index_key(entry.dimension, entry.value, entry.ref),
                }
                for entry in entries
            ],
        },
        text=_save_text(
            bundle, redactions, entries, store.describe(), len(packed), ref
        ),
    )


def _save_text(
    bundle: Bundle,
    redactions: list[dict[str, int]],
    entries: list[IndexEntry],
    store: str,
    size: int,
    ref: str,
) -> str:
    lines = []
    for session, counts in zip(bundle.sessions, redactions):
        note = f" — {summarize(counts)}" if counts else ""
        lines.append(
            f"  packed {session.session_id} [{session.git_branch or 'no branch'}]{note}\n"
        )
    indexed = ", ".join(f"[{entry.value or 'no branch'}]" for entry in entries)
    lines.append(f"\nstored {format_bytes(size)} in {store}\n")
    lines.append(f"indexed under {indexed}\n")
    lines.append(f"ref {ref}\n")
    return "".join(lines)


@dataclass(frozen=True)
class RestoredSession:
    session_id: str
    origin_session_id: str
    branch: str | None
    status: str
    path: str
    residual_origin_paths: tuple[str, ...]
    resume_command: str | None
    foreign_cwds: dict[str, ForeignCwd] = field(default_factory=dict)


def command_restore(argv: list[str]) -> Result:
    parser = _parser("restore")
    parser.add_argument("ref", nargs="?")
    parser.add_argument("--into")
    parser.add_argument("--store")
    parser.add_argument("--force", action="store_true")
    parser.add_argument("--new-id", action="store_true", dest="new_id")
    args = parser.parse_args(argv)

    if not args.ref:
        fail("missing-ref", "restore requires a bundle ref")

    store = create_store(resolve_store_uri(args.store))
    described = f"could not read {args.ref} from {store.describe()}"
    with store_errors(described):
        packed = store.get(bundle_key(args.ref))
    try:
        bundle = unpack_bundle(packed)
    except Exception as error:
        # Fetched fine and still unusable: neither absent nor unreachable.
        fail("bundle-unreadable", f"{described}: {error}")

    target_cwd = resolve_cwd(args.into if args.into is not None else os.getcwd())
    target_home = home_dir()
    restored: list[RestoredSession] = []

    for session in bundle.sessions:
        target_session_id = str(uuid.uuid4()) if args.new_id else session.session_id
        spec = RewriteSpec(
            origin_cwd=session.cwd,
            origin_home=bundle.origin.home,
            target_cwd=target_cwd,
            target_home=target_home,
            origin_session_id=session.session_id,
            target_session_id=target_session_id,
        )

        transcript = rewrite_transcript(session.transcript, spec)
        destination = Path(transcript_path_for(target_cwd, target_session_id))

        if destination.exists() and not args.force:
            restored.append(
                RestoredSession(
                    session_id=target_session_id,
                    origin_session_id=session.session_id,
                    branch=session.git_branch,
                    status="skipped",
                    path=str(destination),
                    residual_origin_paths=(),
                    resume_command=None,
                )
            )
            continue

        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(transcript, encoding="utf-8")

        restored.append(
            RestoredSession(
                session_id=target_session_id,
                origin_session_id=session.session_id,
                branch=session.git_branch,
                status="restored",
                path=str(destination),
                residual_origin_paths=tuple(residual_origin_paths(transcript, spec)),
                resume_command=(
                    f"cd {target_cwd} && claude --resume {target_session_id}"
                ),
                # Pre-rewrite: the origin path only exists losslessly here.
                foreign_cwds=foreign_cwds(session.transcript, spec),
            )
        )

    payload = {
        "ref": args.ref,
        "store": store.describe(),
        "cwd": target_cwd,
        "origin": {
            "hostname": bundle.origin.hostname,
            "home": bundle.origin.home,
            "platform": bundle.origin.platform,
        },
        "sessions": [_restored_payload(session) for session in restored],
    }
    warnings = tuple(
        f"  skipped {session.session_id} (already exists; --force to overwrite)\n"
        for session in restored
        if session.status == "skipped"
    ) + tuple(
        f"  {session.session_id} worked in {origin} for {foreign.count} record(s), "
        f"which no rewrite into {target_cwd} can satisfy\n"
        for session in restored
        for origin, foreign in session.foreign_cwds.items()
    )

    if not any(session.status == "restored" for session in restored):
        raise CommandError("nothing-restored", "nothing restored", payload, warnings)

    return Result(
        command="restore",
        payload=payload,
        text=_restore_text(bundle, restored),
        warnings=warnings,
    )


def _restored_payload(session: RestoredSession) -> dict[str, Any]:
    return {
        "sessionId": session.session_id,
        "originSessionId": session.origin_session_id,
        "branch": session.branch,
        "status": session.status,
        "path": session.path,
        "residualOriginPaths": list(session.residual_origin_paths),
        # Keyed by the origin path, which is the left-hand side a caller states
        # a mapping with; `restored` is what the file on disk actually says.
        "foreignCwds": {
            origin: {"count": foreign.count, "restored": foreign.restored}
            for origin, foreign in session.foreign_cwds.items()
        },
        "resumeCommand": session.resume_command,
    }


def _restore_text(bundle: Bundle, restored: list[RestoredSession]) -> str:
    lines = []
    for session in restored:
        if session.status != "restored":
            continue
        residual = session.residual_origin_paths
        note = f"  (residual origin paths: {', '.join(residual)})" if residual else ""
        lines.append(
            f"  restored {session.session_id} [{session.branch or 'no branch'}]{note}\n"
        )
        for origin, foreign in session.foreign_cwds.items():
            lines.append(
                f"    {foreign.count} record(s) worked in {origin}, "
                f"now {foreign.restored}\n"
            )
    lines.append(
        f"\nfrom {bundle.origin.hostname} ({bundle.origin.home})\nresume with:\n"
    )
    lines += [
        f"  {session.resume_command}\n"
        for session in restored
        if session.resume_command
    ]
    return "".join(lines)


def command_delete(argv: list[str]) -> Result:
    parser = _parser("delete")
    parser.add_argument("ref", nargs="?")
    parser.add_argument("--store")
    args = parser.parse_args(argv)

    if not args.ref:
        fail("missing-ref", "delete requires a bundle ref")

    store = create_store(resolve_store_uri(args.store))
    with store_errors(f"could not delete {args.ref} from {store.describe()}"):
        removed = delete_bundle(store, args.ref)

    text = "".join(f"  removed {key}\n" for key in removed)
    text += f"\ndeleted {args.ref} from {store.describe()}\n"
    return Result(
        command="delete",
        payload={"ref": args.ref, "store": store.describe(), "removed": removed},
        text=text,
    )


def command_reindex(argv: list[str]) -> Result:
    parser = _parser("reindex")
    parser.add_argument("--store")
    args = parser.parse_args(argv)

    store = create_store(resolve_store_uri(args.store))
    with store_errors(f"could not reindex {store.describe()}"):
        result = reindex(store)

    return Result(
        command="reindex",
        payload={
            "store": store.describe(),
            "bundles": result.bundles,
            "entries": result.entries,
            "removed": result.removed,
        },
        text=(
            f"scanned {result.bundles} bundle(s) in {store.describe()}\n"
            f"wrote {result.entries} index entries, removed {result.removed} stale\n"
        ),
    )


def command_ingest(argv: list[str]) -> Result:
    parser = _parser("ingest")
    parser.add_argument("--cwd")
    parser.add_argument("--branch")
    parser.add_argument("--session")
    parser.add_argument("--author")
    parser.add_argument("--author-key")
    parser.add_argument("--out")
    args = parser.parse_args(argv)

    author = args.author
    if args.author_key is not None:
        if author is not None:
            fail("usage", "pass --author or --author-key, not both")
        try:
            author = ssh_author_id(Path(args.author_key).read_text(encoding="utf-8"))
        except (OSError, AuthorKeyError) as error:
            fail("author-key-unreadable", f"{args.author_key}: {error}")

    cwd, sessions = collect(args.cwd, args.branch, args.session)
    if not sessions:
        fail("no-sessions", f"no {AGENT} sessions found for {cwd}")

    out = args.out if args.out is not None else DEFAULT_DERIVED
    os.makedirs(out, exist_ok=True)
    home = home_dir()

    results: list[dict[str, Any]] = []
    lines: list[str] = []
    for meta in sessions:
        derived = ingest(meta.path, Origin(meta.cwd, home, meta.session_id), author)
        path = os.path.join(out, derived_name(derived))
        Path(path).write_bytes(pack_derived(derived))
        results.append(
            {
                "sessionId": meta.session_id,
                "source": derived.header["source"]["id"],
                "path": path,
                "turns": len(derived.turns),
                "heads": derived.heads,
            }
        )
        leaves = len(derived.heads["leaves"])
        lines.append(
            f"  {meta.session_id}  {len(derived.turns)} turns  {leaves} head(s)\n"
        )

    text = "".join(lines) + f"\ningested {len(sessions)} session(s) into {out}\n"
    return Result(
        command="ingest",
        payload={
            "cwd": cwd,
            "out": out,
            "converter": CONVERTER,
            "sessions": results,
        },
        text=text,
    )


@contextmanager
def trace_errors() -> Iterator[None]:
    try:
        yield
    except TraceError as error:
        fail(error.code, str(error))


def command_hook(argv: list[str]) -> Result:
    """Run by Claude Code with the event as JSON on stdin. It must never fail
    the tool call it wraps, so every error is swallowed and nothing is
    written: a hook that breaks commits gets uninstalled."""
    name = argv[0] if argv else ""
    try:
        event = json.loads(sys.stdin.read() or "{}")
        if not isinstance(event, dict):
            event = {}
        if name == "pre-tool":
            hook_pre_tool(event)
        elif name == "post-tool":
            hook_post_tool(event)
        elif name == "stop":
            snapshot_transcript(event, create_store(resolve_store_uri()))
    except Exception:
        pass
    return Result(command="hook")


def command_install(argv: list[str]) -> Result:
    parser = _parser("install")
    parser.add_argument("--cwd")
    parser.add_argument("--local", action="store_true")
    parser.add_argument("--command")
    args = parser.parse_args(argv)

    with trace_errors():
        root = repo_root(args.cwd or os.getcwd())
    if args.command is not None:
        command = args.command
    elif args.local:
        # A machine-specific path, which is why it only goes in the
        # uncommitted settings file.
        command = shutil.which("loch") or os.path.abspath(sys.argv[0])
    else:
        command = "loch"
    name = "settings.local.json" if args.local else "settings.json"
    settings = os.path.join(root, ".claude", name)
    added = install_hooks(settings, command)
    # Amend and rebase copy a commit's note to its replacement only when told
    # to, and an amended commit is still the one the conversation made.
    git(root, "config", "notes.rewriteRef", NOTES_REF)

    text = (
        f"installed loch hooks ({', '.join(added)}) in {settings}\n"
        if added
        else f"loch hooks already installed in {settings}\n"
    )
    text += (
        "restart Claude Code sessions in this repository to pick them up\n"
        f"share links with: git push <remote> {NOTES_REF}\n"
    )
    return Result(
        command="install",
        payload={"settings": settings, "command": command, "added": added},
        text=text,
    )


def _store_or_none(uri: str | None) -> Any:
    try:
        return create_store(resolve_store_uri(uri))
    except Exception:
        return None


def _flat(text: str, limit: int) -> str:
    flat = " ".join(text.split())
    return flat if len(flat) <= limit else flat[: limit - 1] + "…"


def _link_text(link: Link | None) -> str:
    if link is None:
        return "no conversation recorded"
    if link.transcript is None:
        return f"session {link.session_id[:8]} (transcript not available)"
    title = f' "{link.title}"' if link.title else ""
    prompt = f' — asked: "{_flat(link.prompt, 72)}"' if link.prompt else ""
    return f"session {link.session_id[:8]}{title}{prompt}"


UNLINKED_MARK = "·"
UNCOMMITTED_MARK = "+"


def _mark(index: int) -> str:
    letters = ""
    index += 1
    while index:
        index, remainder = divmod(index - 1, 26)
        letters = chr(ord("A") + remainder) + letters
    return letters


def command_blame(argv: list[str]) -> Result:
    parser = _parser("blame")
    parser.add_argument("file")
    parser.add_argument("--html", nargs="?", const="", metavar="PATH")
    parser.add_argument("--lines", metavar="RANGES")
    parser.add_argument("--no-open", action="store_true")
    parser.add_argument("--store")
    args = parser.parse_args(argv)

    cwd = os.path.dirname(os.path.abspath(args.file)) or os.getcwd()
    with trace_errors():
        root = repo_root(cwd)
        hunks = blame_hunks(root, os.path.relpath(os.path.abspath(args.file), root))
    resolver = Resolver(root, _store_or_none(args.store))
    links: dict[str, Link | None] = {}
    for hunk in hunks:
        if hunk.commit not in links and hunk.commit != UNCOMMITTED:
            links[hunk.commit] = resolver.resolve(hunk.commit)

    # One mark per request rather than per session: a long session makes many
    # commits for many asks, and marking by session would paint the whole file
    # one letter. Marks are handed out in order of first appearance in the
    # file, so A is always the first conversation you read.
    marks: dict[tuple[str, str], str] = {}
    legend: list[dict[str, Any]] = []
    mark_of: dict[str, str] = {}
    for hunk in hunks:
        link = links.get(hunk.commit)
        if hunk.commit == UNCOMMITTED:
            mark_of[hunk.commit] = UNCOMMITTED_MARK
            continue
        if link is None:
            mark_of[hunk.commit] = UNLINKED_MARK
            continue
        key = (link.session_id, link.prompt or link.call)
        if key not in marks:
            marks[key] = _mark(len(marks))
            legend.append({"mark": marks[key], **link.payload(), "commits": []})
        entry = next(item for item in legend if item["mark"] == marks[key])
        if hunk.commit not in entry["commits"]:
            entry["commits"].append(hunk.commit)
        mark_of[hunk.commit] = marks[key]

    rows: list[dict[str, Any]] = []
    for hunk in hunks:
        link = links.get(hunk.commit)
        rows.append(
            {
                "start": hunk.start,
                "end": hunk.end,
                "commit": None if hunk.commit == UNCOMMITTED else hunk.commit,
                "summary": hunk.summary,
                "mark": mark_of[hunk.commit],
                "conversation": link.payload() if link is not None else None,
            }
        )
    text = _hunks_text(args.file, hunks, links)
    payload: dict[str, Any] = {
        "file": args.file,
        "conversations": legend,
        "hunks": rows,
    }
    if args.html is not None:
        try:
            visible = parse_ranges(args.lines) if args.lines else None
        except ValueError:
            fail("usage", f"--lines takes ranges like 1-12,99-110, not {args.lines}")
        page = render(
            args.file,
            legend,
            rows,
            {
                hunk.start + offset: line
                for hunk in hunks
                for offset, line in enumerate(hunk.lines)
            },
            visible,
        )
        target = args.html or os.path.join(
            tempfile.gettempdir(), f"loch-blame-{os.path.basename(args.file)}.html"
        )
        Path(target).write_text(page, encoding="utf-8")
        payload["html"] = os.path.abspath(target)
        text = f"wrote {payload['html']}\n"
        if not args.no_open and JSON_FLAG not in sys.argv:
            webbrowser.open(Path(target).resolve().as_uri())
    return Result(command="blame", payload=payload, text=text)


def _hunks_text(file: str, hunks: list[Any], links: dict[str, Link | None]) -> str:
    lines = [f"{file}\n"]
    for hunk in hunks:
        link = links.get(hunk.commit)
        span = f"{hunk.start}-{hunk.end}" if hunk.end > hunk.start else str(hunk.start)
        if hunk.commit == UNCOMMITTED:
            lines.append(f"  {span:>9}  uncommitted\n")
            continue
        lines.append(f"  {span:>9}  {hunk.commit[:7]}  {hunk.summary[:60]}\n")
        lines.append(f"  {'':>9}  {'':7}  {_link_text(link)}\n")
    linked = sum(1 for link in links.values() if link is not None)
    lines.append(
        f"\n{linked} of {len(links)} commit(s) linked to a conversation; "
        f"follow one up with: loch ask {file}:<line>\n"
    )
    return "".join(lines)


def _history_target(target: str) -> tuple[str, tuple[int, int] | None]:
    """`file`, `file:105` or `file:100-120`; a path that exists wins outright."""
    if os.path.exists(target):
        return target, None
    file, _, span = target.rpartition(":")
    start, _, end = span.partition("-")
    if not file or not start.isdigit() or (end and not end.isdigit()):
        fail("usage", f"{target} is neither a file nor <file>:<line>[-<end>]")
    first, last = int(start), int(end) if end else int(start)
    if first < 1 or last < first:
        fail("usage", f"{span} is not a line range")
    return file, (first, last)


def command_log(argv: list[str]) -> Result:
    parser = _parser("log")
    parser.add_argument("target")
    parser.add_argument("--store")
    args = parser.parse_args(argv)

    file, lines = _history_target(args.target)
    cwd = os.path.dirname(os.path.abspath(file)) or os.getcwd()
    with trace_errors():
        root = repo_root(cwd)
        relative = os.path.relpath(os.path.abspath(file), root)
        if lines is not None:
            # -L past the end is a fatal git error with a worse message.
            length = max((hunk.end for hunk in blame_hunks(root, relative)), default=0)
            if lines[1] > length:
                fail("usage", f"{file} has {length} lines, not {lines[1]}")
        changes = history(root, relative, lines)
    resolver = Resolver(root, _store_or_none(args.store))

    # Grouped the way blame marks them — per request, not per session — and
    # in order of most recent change, since "who touched this last" is the
    # question a history is usually opened to answer.
    conversations: list[dict[str, Any]] = []
    keyed: dict[tuple[str, str], dict[str, Any]] = {}
    entries: list[dict[str, Any]] = []
    text = []
    for change in changes:
        link = resolver.resolve(change.commit)
        when = datetime.fromtimestamp(change.time, timezone.utc)
        entries.append(
            {
                "commit": change.commit,
                "time": when.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "summary": change.summary,
                "conversation": link.payload() if link is not None else None,
            }
        )
        text.append(
            f"  {change.commit[:7]}  {when:%Y-%m-%d}  {change.summary[:60]}\n"
            f"  {'':7}  {'':10}  {_link_text(link)}\n"
        )
        if link is None:
            continue
        key = (link.session_id, link.prompt or link.call)
        if key not in keyed:
            keyed[key] = {**link.payload(), "commits": []}
            conversations.append(keyed[key])
        keyed[key]["commits"].append(change.commit)

    span = (
        ""
        if lines is None
        else (f":{lines[0]}" if lines[0] == lines[1] else f":{lines[0]}-{lines[1]}")
    )
    header = (
        f"{file}{span}: {len(changes)} commit(s) from "
        f"{len(conversations)} conversation(s), newest first\n"
    )
    footer = "\nfollow one up with: loch ask <commit>\n" if conversations else ""
    return Result(
        command="log",
        payload={
            "file": file,
            "lines": None if lines is None else {"start": lines[0], "end": lines[1]},
            "conversations": conversations,
            "commits": entries,
        },
        text=header + "".join(text) + footer,
    )


def _resolve_commit(root: str, target: str) -> str:
    file, _, line = target.rpartition(":")
    if file and line.isdigit() and os.path.exists(file):
        relative = os.path.relpath(os.path.abspath(file), root)
        for hunk in blame_hunks(root, relative):
            if hunk.start <= int(line) <= hunk.end:
                if hunk.commit == UNCOMMITTED:
                    fail("not-linked", f"{target} is not committed yet")
                return hunk.commit
        fail("usage", f"{file} has no line {line}")
    output = git(
        root, "rev-parse", "--verify", "-q", f"{target}^{{commit}}", check=False
    )
    if not output.strip():
        fail("usage", f"{target} is neither a commit nor <file>:<line>")
    return output.strip()


def command_ask(argv: list[str]) -> Result:
    parser = _parser("ask")
    parser.add_argument("target")
    parser.add_argument("question", nargs="?")
    parser.add_argument("--at-commit", action="store_true")
    parser.add_argument("--no-launch", action="store_true")
    parser.add_argument("--store")
    args = parser.parse_args(argv)
    as_json = JSON_FLAG in sys.argv

    with trace_errors():
        root = repo_root(os.getcwd())
        commit = _resolve_commit(root, args.target)
        link = Resolver(root, _store_or_none(args.store)).resolve(commit)
        if link is None:
            fail("not-linked", f"no conversation is recorded for {commit[:7]}")
        if link.transcript is None:
            fail(
                "transcript-missing",
                f"session {link.session_id} is linked but its transcript is "
                f"neither in {projects_dir()} nor in the store",
            )
        fork = fork_session(
            link.transcript,
            root,
            call=link.call if args.at_commit else None,
            title=f"loch ask {commit[:7]}",
        )

    command = ["claude", "--resume", fork.session_id]
    if args.question:
        command.append(args.question)
    payload = {
        "commit": commit,
        "conversation": link.payload(),
        "fork": {
            "sessionId": fork.session_id,
            "path": fork.path,
            "cwd": fork.cwd,
            "records": fork.records,
        },
        "at": "commit" if args.at_commit else "end",
        "launch": command,
    }
    where = "just after it" if args.at_commit else "to its end"
    text = (
        f"forked session {fork.origin_session[:8]}, which made {commit[:7]}, "
        f"{where}: {fork.records} records, as {fork.session_id}\n"
    )
    if args.no_launch or as_json:
        return Result(
            command="ask",
            payload=payload,
            text=text + f"resume with: cd {root} && {' '.join(command)}\n",
        )
    sys.stderr.write(text)
    os.chdir(root)
    os.execvp("claude", command)


def _progress_meter() -> Callable[[int, int, int, int], None] | None:
    """A one-line counter on stderr, for a person watching a terminal only.

    JSON mode keeps stderr empty, and a pipe or log file would get every
    redraw as garbage, so both get nothing.
    """
    if JSON_FLAG in sys.argv or not sys.stderr.isatty():
        return None
    last = [0.0]

    def meter(done: int, total: int, read: int, size: int) -> None:
        now = time.monotonic()
        if done < total and now - last[0] < 0.1:
            return
        last[0] = now
        percent = 100 * read // size if size else 100
        sys.stderr.write(
            f"\rscanning transcripts {done}/{total} "
            f"({format_bytes(read)} of {format_bytes(size)}, {percent}%)\033[K"
        )
        if done == total:
            sys.stderr.write("\n")
        sys.stderr.flush()

    return meter


def command_backfill(argv: list[str]) -> Result:
    parser = _parser("backfill")
    parser.add_argument("--cwd")
    # Branches rather than --all, which would walk refs/notes/loch too.
    parser.add_argument("--revision", default="--branches")
    args = parser.parse_args(argv)

    with trace_errors():
        root = repo_root(args.cwd or os.getcwd())
        linked = backfill(root, args.revision, _progress_meter())
    rows = [
        {"commit": sha, "session": call.session_id, "call": call.call}
        for sha, call in linked
    ]
    text = "".join(
        f"  {sha[:7]}  session {call.session_id[:8]}\n" for sha, call in linked
    )
    text += f"linked {len(linked)} commit(s)\n"
    return Result(command="backfill", payload={"linked": rows}, text=text)


def command_help(argv: list[str]) -> Result:
    return Result(command="help", payload={"usage": USAGE}, text=USAGE)


def installed_version() -> str:
    """Read from package metadata rather than held here, so there is one copy
    of the number and pyproject stays the one that sets it. A source checkout
    that was never installed has no metadata to read, and pyproject can't be
    parsed for it either — tomllib arrived in 3.11 and the floor is 3.10."""
    try:
        return importlib.metadata.version("loch")
    except importlib.metadata.PackageNotFoundError:
        return "unknown"


def command_version(argv: list[str]) -> Result:
    version = installed_version()
    return Result(
        command="version", payload={"version": version}, text=f"loch {version}\n"
    )


def _platform_name() -> str:
    system = platform.system().lower()
    return {"darwin": "darwin", "linux": "linux", "windows": "win32"}.get(
        system, system
    )


COMMANDS: dict[str, Callable[[list[str]], Result]] = {
    "list": command_list,
    "save": command_save,
    "restore": command_restore,
    "delete": command_delete,
    "reindex": command_reindex,
    "ingest": command_ingest,
    "install": command_install,
    "blame": command_blame,
    "ask": command_ask,
    "log": command_log,
    "backfill": command_backfill,
    "hook": command_hook,
    "help": command_help,
    "version": command_version,
}


def main() -> None:
    argv = sys.argv[1:]
    # Read off argv rather than from a parsed namespace: an unknown command, or
    # a subparser rejecting a flag, has to render in the mode the caller asked
    # for, and neither of those has a namespace by the time it fails.
    as_json = JSON_FLAG in argv
    if argv and argv[0] == JSON_FLAG:
        argv = argv[1:]

    command = argv[0] if argv else "help"
    if command in ("--help", "-h"):
        command = "help"
    if command in ("--version", "-V"):
        command = "version"

    try:
        handler = COMMANDS.get(command)
        if handler is None:
            fail("unknown-command", f"unknown command '{command}' (try: loch help)")
        result = handler(argv[1:])
    except HelpRequested as help_text:
        result = Result(command, {"usage": help_text.text}, text=help_text.text)
    except CommandError as error:
        emit_error(command, error, as_json)
    except Exception as error:
        # A traceback is the most useful thing a human can get, and the one
        # thing a JSON consumer can't parse.
        if not as_json:
            raise
        emit_error(command, CommandError("internal", str(error)), True)

    emit(result, as_json)


if __name__ == "__main__":
    main()
