import gzip
import hashlib
import json
import struct
import zlib
from dataclasses import dataclass
from typing import Any

BUNDLE_VERSION = 1


@dataclass(frozen=True)
class BundleOrigin:
    home: str
    platform: str
    hostname: str


@dataclass(frozen=True)
class BundleSession:
    session_id: str
    cwd: str
    git_branch: str | None
    claude_version: str | None
    modified_at: str
    transcript: str


@dataclass(frozen=True)
class Bundle:
    version: int
    agent: str
    created_at: str
    origin: BundleOrigin
    sessions: list[BundleSession]


def _session_to_json(session: BundleSession) -> dict[str, Any]:
    record: dict[str, Any] = {"sessionId": session.session_id, "cwd": session.cwd}
    # Omitted rather than null: the ref is a hash of these bytes, so the
    # encoding of "unknown" has to stay stable or identical sessions diverge.
    if session.git_branch is not None:
        record["gitBranch"] = session.git_branch
    if session.claude_version is not None:
        record["claudeVersion"] = session.claude_version
    record["modifiedAt"] = session.modified_at
    record["transcript"] = session.transcript
    return record


def _bundle_to_json(bundle: Bundle) -> dict[str, Any]:
    return {
        "version": bundle.version,
        "agent": bundle.agent,
        "createdAt": bundle.created_at,
        "origin": {
            "home": bundle.origin.home,
            "platform": bundle.origin.platform,
            "hostname": bundle.origin.hostname,
        },
        "sessions": [_session_to_json(session) for session in bundle.sessions],
    }


# The gzip header, written out rather than left to gzip.compress: from 3.11
# that delegates to zlib, which stamps its own OS byte (19 on macOS, 3 on
# Linux) where 3.10 wrote 255. The ref is a hash of these bytes, so the same
# sessions got a different ref per platform and per Python. Fixed at 255
# ("unknown"), which is what 3.10 always produced; XFL 2 is level 9.
GZIP_HEADER = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x02\xff"


def gzip_deterministic(data: bytes) -> bytes:
    compressor = zlib.compressobj(9, zlib.DEFLATED, -zlib.MAX_WBITS, 8, 0)
    body = compressor.compress(data) + compressor.flush()
    trailer = struct.pack("<II", zlib.crc32(data), len(data) & 0xFFFFFFFF)
    return GZIP_HEADER + body + trailer


def pack_bundle(bundle: Bundle) -> bytes:
    encoded = json.dumps(
        _bundle_to_json(bundle), separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")
    return gzip_deterministic(encoded)


def unpack_bundle(data: bytes) -> Bundle:
    raw = json.loads(gzip.decompress(data).decode("utf-8"))
    version = raw.get("version")
    if version != BUNDLE_VERSION:
        raise ValueError(
            f"unsupported bundle version {version} (this build reads {BUNDLE_VERSION})"
        )

    origin = raw.get("origin", {})
    return Bundle(
        version=version,
        agent=raw.get("agent", ""),
        created_at=raw.get("createdAt", ""),
        origin=BundleOrigin(
            home=origin.get("home", ""),
            platform=origin.get("platform", ""),
            hostname=origin.get("hostname", ""),
        ),
        sessions=[
            BundleSession(
                session_id=session["sessionId"],
                cwd=session["cwd"],
                git_branch=session.get("gitBranch"),
                claude_version=session.get("claudeVersion"),
                modified_at=session.get("modifiedAt", ""),
                transcript=session["transcript"],
            )
            for session in raw.get("sessions", [])
        ],
    )


def bundle_ref(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()
