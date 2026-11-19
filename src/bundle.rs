//! The bundle format. The ref is the SHA-256 of the
//! packed bytes, so two things here must never drift: the JSON is compact and
//! unescaped (`separators=(",", ":")`, `ensure_ascii=False`) with absent
//! metadata omitted rather than null, and the gzip stream is zlib's own
//! output at level 9 behind a fixed header.

use crate::json::{self, Map, Value};
use crate::object;
use crate::py;
use crate::sys;
use sha2::{Digest, Sha256};
use std::io::Read;

/// A bundle of Claude Code sessions alone: every bundle made before Codex,
/// and every one since that holds only Claude sessions, keeps this, and its
/// bytes.
pub const BUNDLE_VERSION: i64 = 1;

/// A bundle holding any other agent's session. A build that predates agents
/// ignores `agent` and would restore a Codex session as a Claude transcript
/// while reporting success; it refuses any version but 1, so this makes it
/// fail loudly instead.
pub const AGENTS_VERSION: i64 = 2;

#[derive(Clone)]
pub struct BundleOrigin {
    pub home: String,
    pub platform: String,
    pub hostname: String,
}

#[derive(Clone, Default)]
pub struct BundleSession {
    pub session_id: String,
    /// Set only where the session's agent differs from the bundle's, so a
    /// bundle of one agent's sessions — every Claude bundle before Codex —
    /// keeps its bytes, and its ref.
    pub agent: Option<String>,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub claude_version: Option<String>,
    /// Any other agent's version: `claudeVersion` keeps meaning Claude's.
    pub agent_version: Option<String>,
    pub modified_at: String,
    pub transcript: String,
}

impl BundleSession {
    pub fn agent_of<'a>(&'a self, bundle: &'a Bundle) -> &'a str {
        self.agent.as_deref().unwrap_or(&bundle.agent)
    }

    pub fn version(&self) -> Option<&str> {
        self.claude_version
            .as_deref()
            .or(self.agent_version.as_deref())
    }
}

impl Bundle {
    /// The version these sessions need: 1 unless any isn't Claude Code's.
    pub fn required_version(&self) -> i64 {
        let claude_only = self
            .sessions
            .iter()
            .all(|s| s.agent_of(self) == crate::claude::AGENT);
        if claude_only {
            BUNDLE_VERSION
        } else {
            AGENTS_VERSION
        }
    }
}

#[derive(Clone)]
pub struct Bundle {
    pub version: i64,
    pub agent: String,
    pub created_at: String,
    pub origin: BundleOrigin,
    pub sessions: Vec<BundleSession>,
}

fn session_json(session: &BundleSession) -> Value {
    let mut record = object! {"sessionId" => session.session_id.as_str()};
    // Omitted rather than null: the ref is a hash of these bytes.
    if let Some(agent) = &session.agent {
        record.insert("agent".into(), agent.as_str().into());
    }
    record.insert("cwd".into(), session.cwd.as_str().into());
    if let Some(branch) = &session.git_branch {
        record.insert("gitBranch".into(), branch.as_str().into());
    }
    if let Some(version) = &session.claude_version {
        record.insert("claudeVersion".into(), version.as_str().into());
    }
    if let Some(version) = &session.agent_version {
        record.insert("agentVersion".into(), version.as_str().into());
    }
    record.insert("modifiedAt".into(), session.modified_at.as_str().into());
    record.insert("transcript".into(), session.transcript.as_str().into());
    Value::Object(record)
}

fn bundle_json(bundle: &Bundle) -> Value {
    Value::Object(object! {
        "version" => bundle.version,
        "agent" => bundle.agent.as_str(),
        "createdAt" => bundle.created_at.as_str(),
        "origin" => object! {
            "home" => bundle.origin.home.as_str(),
            "platform" => bundle.origin.platform.as_str(),
            "hostname" => bundle.origin.hostname.as_str(),
        },
        "sessions" => bundle.sessions.iter().map(session_json).collect::<Vec<_>>(),
    })
}

/// A fixed header: OS byte 255 whatever the platform, XFL 2 for
/// level 9, mtime 0.
const GZIP_HEADER: [u8; 10] = [0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x02, 0xff];

/// Raw deflate at level 9, window 15, memLevel 8, default strategy — the
/// `zlib.compressobj(9, DEFLATED, -15, 8, 0)` the Python build used — through zlib
/// itself (flate2's zlib backend initialises exactly that), since no other
/// deflate reproduces zlib's bytes. zlib's output doesn't depend on how the
/// input is chunked, so streaming it is the same as one call.
fn deflate_raw(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(9));
    encoder.write_all(data).expect("writing to a Vec");
    encoder.finish().expect("finishing into a Vec")
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum()
}

pub fn gzip_deterministic(data: &[u8]) -> Vec<u8> {
    let mut out = GZIP_HEADER.to_vec();
    out.extend(deflate_raw(data));
    out.extend(crc32(data).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

#[derive(Debug)]
pub struct PackError(pub String);

pub fn pack_bundle(bundle: &Bundle) -> Result<Vec<u8>, PackError> {
    let encoded = json::dumps(&bundle_json(bundle), json::COMPACT_UNICODE);
    let bytes = py::encode_strict(&encoded)
        .map_err(|_| PackError("'utf-8' codec can't encode a surrogate".into()))?;
    Ok(gzip_deterministic(&bytes))
}

pub fn bundle_ref(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// `str()` of a JSON value as Python would print it in an f-string.
fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(Value::Int(digits)) => digits.clone(),
        Some(Value::Float(f)) => json::float_repr(*f),
        Some(Value::Str(text)) => text.clone(),
        Some(other) => json::dumps(other, json::DEFAULT),
    }
}

/// What `gzip.decompress` raises for the inputs worth telling apart. An empty
/// input is not an error to it: it decompresses to nothing, and the JSON
/// parse then fails.
fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let magic = &data[..data.len().min(2)];
    if magic != [0x1f, 0x8b] {
        return Err(format!("Not a gzipped file ({})", sys::bytes_repr(magic)));
    }
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|_| {
            "Compressed file ended before the end-of-stream marker was reached".to_string()
        })?;
    Ok(out)
}

/// The error string is what the Python build's exception printed, for the
/// failures the harness exercises; anything stranger reads generically.
pub fn unpack_bundle(data: &[u8]) -> Result<Bundle, String> {
    let raw = gunzip(data)?;
    let text = std::str::from_utf8(&raw).map_err(|e| {
        format!(
            "'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte",
            raw[e.valid_up_to()],
            e.valid_up_to()
        )
    })?;
    let value = json::loads(text).map_err(|_| {
        if py::strip(text).is_empty() {
            "Expecting value: line 1 column 1 (char 0)".to_string()
        } else {
            "the bundle is not valid JSON".to_string()
        }
    })?;
    let root = value.as_object().ok_or("the bundle is not a JSON object")?;
    let version = root.get("version");
    let read = if matches!(version, Some(Value::Int(d)) if d == "1")
        || matches!(version, Some(Value::Bool(true)))
        || matches!(version, Some(Value::Float(f)) if *f == 1.0)
    {
        BUNDLE_VERSION
    } else if matches!(version, Some(Value::Int(d)) if d == "2") {
        AGENTS_VERSION
    } else {
        return Err(format!(
            "unsupported bundle version {} (this build reads {} and {})",
            py_str(version),
            BUNDLE_VERSION,
            AGENTS_VERSION
        ));
    };
    let empty = Map::new();
    let origin = match root.get("origin") {
        None => &empty,
        Some(Value::Object(map)) => map,
        Some(_) => return Err("the bundle's origin is not an object".into()),
    };
    let text_of = |map: &Map, key: &str| -> String {
        map.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let mut sessions = Vec::new();
    for session in root
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let session = session.as_object().ok_or("a session is not an object")?;
        let required = |key: &str| -> Result<String, String> {
            session
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("'{}'", key))
        };
        let optional = |key: &str| session.get(key).and_then(Value::as_str).map(str::to_string);
        sessions.push(BundleSession {
            session_id: required("sessionId")?,
            agent: optional("agent"),
            agent_version: optional("agentVersion"),
            cwd: required("cwd")?,
            git_branch: session
                .get("gitBranch")
                .and_then(Value::as_str)
                .map(str::to_string),
            claude_version: session
                .get("claudeVersion")
                .and_then(Value::as_str)
                .map(str::to_string),
            modified_at: text_of(session, "modifiedAt"),
            transcript: required("transcript")?,
        });
    }
    Ok(Bundle {
        version: read,
        agent: text_of(root, "agent"),
        created_at: text_of(root, "createdAt"),
        origin: BundleOrigin {
            home: text_of(origin, "home"),
            platform: text_of(origin, "platform"),
            hostname: text_of(origin, "hostname"),
        },
        sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Python 3.10's gzip.compress output, which refs were defined by.
    #[test]
    fn packing_matches_python_310() {
        let mut data = b"{\"a\":\"".to_vec();
        data.extend(b"xyz".repeat(5000));
        data.extend(b"\"}");
        let packed = gzip_deterministic(&data);
        assert_eq!(&bundle_ref(&packed)[..16], "405552bff938ef58");
    }
}
