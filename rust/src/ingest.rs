//! Claude Code JSONL → loch turns, as in `loch/ingest.py` (the converter).
//!
//! Normalizing goes through `rewrite::compile`, the same single pass with the
//! same guards restore uses, fed placeholders instead of another machine's
//! paths: that sharing is what makes a turn id survive `restore`.

use crate::claude::{encode_path, AGENT};
use crate::format::{self, digest, digest_bytes, Derived, NotJson, Turn, FORMAT_VERSION};
use crate::json::{self, Map, Value};
use crate::object;
use crate::py;
use crate::rewrite::{self, Guard, Replacer};
use crate::sys;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;

pub const CONVERTER_VERSION: i64 = 1;

pub fn converter() -> Map {
    object! {"id" => AGENT, "version" => CONVERTER_VERSION}
}

const PLACEHOLDER_PREFIX: &str = "{{loch:";
const ESCAPE: &str = "{{loch:esc}}";
const ROOT: &str = "{{loch:root}}";
const HOME: &str = "{{loch:home}}";
const ROOT_SLUG: &str = "{{loch:root-slug}}";
const SESSION: &str = "{{loch:session}}";

/// Record fields that place a record rather than say anything; excluded from
/// the hashed payload of system and attachment turns.
const ENVELOPE_FIELDS: &[&str] = &[
    "type",
    "uuid",
    "parentUuid",
    "logicalParentUuid",
    "sessionId",
    "session_id",
    "timestamp",
    "cwd",
    "gitBranch",
    "version",
    "entrypoint",
    "userType",
    "slug",
    "isSidechain",
    "agentId",
    "requestId",
    "promptId",
];

fn author(kind: &str) -> Map {
    object! {"kind" => kind, "agent" => AGENT}
}

pub struct Origin {
    pub cwd: String,
    pub home: String,
    pub session_id: String,
}

pub fn normalizer(origin: &Origin) -> Replacer {
    let mut pairs = vec![
        (
            PLACEHOLDER_PREFIX.to_string(),
            ESCAPE.to_string(),
            Guard::None,
        ),
        (origin.cwd.clone(), ROOT.to_string(), Guard::Path),
        (
            encode_path(&origin.cwd),
            ROOT_SLUG.to_string(),
            Guard::EncodedComponent,
        ),
        (origin.session_id.clone(), SESSION.to_string(), Guard::None),
    ];
    // A session run from the home directory itself is a root, not a home.
    if origin.home != origin.cwd {
        pairs.push((origin.home.clone(), HOME.to_string(), Guard::Path));
    }
    rewrite::compile(&pairs)
}

/// Keys as well as values: a tool result can key a map by file path, and
/// `restore` rewrites the text of the whole record, keys included.
pub fn normalize_value(value: &Value, text: &Replacer) -> Value {
    match value {
        Value::Str(s) => Value::Str(text.apply(s)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| normalize_value(v, text)).collect())
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                out.insert(text.apply(key), normalize_value(item, text));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `block.get(key, default)`: present-with-null stays null.
fn get_or(block: &Map, key: &str, default: Value) -> Value {
    block.get(key).cloned().unwrap_or(default)
}

fn get_none(block: &Map, key: &str) -> Value {
    get_or(block, key, Value::Null)
}

fn blocks_from_content(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::Str(text)) => vec![Value::Object(
            object! {"type" => "text", "text" => text.as_str()},
        )],
        Some(Value::Array(items)) => items.iter().map(block).collect(),
        _ => Vec::new(),
    }
}

fn passthrough(value: &Value) -> Value {
    Value::Object(object! {"type" => "x-claude-code:block", "block" => value.clone()})
}

fn block(value: &Value) -> Value {
    let Value::Object(b) = value else {
        return passthrough(value);
    };
    let kind = b.get("type").and_then(Value::as_str);
    match kind {
        Some("text") => {
            Value::Object(object! {"type" => "text", "text" => get_or(b, "text", "".into())})
        }
        // The signature is Anthropic's attestation for resuming, not content.
        Some("thinking") => Value::Object(
            object! {"type" => "thinking", "text" => get_or(b, "thinking", "".into())},
        ),
        Some("redacted_thinking") => Value::Object(object! {"type" => "redacted-thinking"}),
        Some("tool_use") => Value::Object(object! {
            "type" => "tool-call",
            "call" => get_none(b, "id"),
            "name" => get_none(b, "name"),
            "input" => get_none(b, "input"),
        }),
        Some("tool_result") => {
            let mut normalized = object! {
                "type" => "tool-result",
                "call" => get_none(b, "tool_use_id"),
                "content" => blocks_from_content(b.get("content")),
            };
            if b.get("is_error").is_some_and(Value::truthy) {
                normalized.insert("error".into(), Value::Bool(true));
            }
            Value::Object(normalized)
        }
        Some("image") => {
            Value::Object(object! {"type" => "image", "source" => get_none(b, "source")})
        }
        // Unknown block types pass through whole rather than being dropped.
        _ => passthrough(value),
    }
}

fn payload(record: &Map) -> Map {
    record
        .iter()
        .filter(|(key, _)| !ENVELOPE_FIELDS.contains(&key.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// (kind, author, blocks) for a chain record, or None for one outside the
/// conversation graph. The human author is filled in by the caller.
fn classify(record: &Map) -> Option<(&'static str, Map, Vec<Value>)> {
    let kind = record.get("type").and_then(Value::as_str);
    let content = match record.get("message") {
        Some(Value::Object(message)) => message.get("content"),
        _ => None,
    };
    match kind {
        Some("assistant") => Some(("message", author("agent"), blocks_from_content(content))),
        Some("user") => {
            let blocks = blocks_from_content(content);
            if record.get("isCompactSummary").is_some_and(Value::truthy)
                || record.get("isMeta").is_some_and(Value::truthy)
            {
                return Some(("context", author("system"), blocks));
            }
            if blocks
                .iter()
                .any(|b| b.get("type").is_some_and(|t| t.is_str("tool-result")))
            {
                return Some(("tool-result", author("tool"), blocks));
            }
            Some(("message", object! {"kind" => "human"}, blocks))
        }
        Some("attachment") => {
            let b = object! {"type" => "x-claude-code:attachment", "attachment" => payload(record)};
            Some(("context", author("system"), vec![Value::Object(b)]))
        }
        Some("system") => {
            let b = Value::Object(
                object! {"type" => "x-claude-code:system", "system" => payload(record)},
            );
            if record
                .get("subtype")
                .is_some_and(|s| s.is_str("compact_boundary"))
            {
                Some(("compaction", author("system"), vec![b]))
            } else {
                Some(("event", author("system"), vec![b]))
            }
        }
        _ => None,
    }
}

/// A compaction boundary has no parent by design and names the record it
/// logically follows instead; following that link keeps a compacted session
/// one connected graph.
fn parent_uuid(record: &Map) -> Option<String> {
    let parent = match record.get("parentUuid") {
        None | Some(Value::Null) => record.get("logicalParentUuid"),
        other => other,
    };
    parent.and_then(Value::as_str).map(str::to_string)
}

fn parse(raw: &str) -> Vec<(usize, Map)> {
    let mut records = Vec::new();
    for (index, line) in raw.split('\n').enumerate() {
        if py::strip(line).is_empty() {
            continue;
        }
        if let Ok(Value::Object(record)) = json::loads(line) {
            records.push((index + 1, record));
        }
    }
    records
}

fn uuid_of(record: &Map) -> &str {
    record
        .get("uuid")
        .and_then(Value::as_str)
        .expect("chain records have a uuid")
}

/// File order wherever it already respects parentage. Dangling parents make
/// a root; a cycle is broken the same way rather than looping.
fn topological(chain: Vec<(usize, Map)>) -> (Vec<(usize, Map)>, HashSet<String>) {
    let present: HashSet<String> = chain.iter().map(|(_, r)| uuid_of(r).to_string()).collect();
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut waiting: Vec<i64> = vec![0; chain.len()];
    for (index, (_, record)) in chain.iter().enumerate() {
        if let Some(parent) = parent_uuid(record) {
            if present.contains(&parent) && parent != uuid_of(record) {
                children.entry(parent).or_default().push(index);
                waiting[index] = 1;
            }
        }
    }
    let mut ready: BinaryHeap<Reverse<usize>> = waiting
        .iter()
        .enumerate()
        .filter(|(_, &w)| w == 0)
        .map(|(i, _)| Reverse(i))
        .collect();
    let mut done = vec![false; chain.len()];
    let mut order: Vec<usize> = Vec::with_capacity(chain.len());
    while order.len() < chain.len() {
        if ready.is_empty() {
            let stuck = done
                .iter()
                .position(|d| !d)
                .expect("something is unfinished");
            waiting[stuck] = 0;
            ready.push(Reverse(stuck));
        }
        let Reverse(index) = ready.pop().expect("ready is non-empty");
        if done[index] {
            continue;
        }
        done[index] = true;
        order.push(index);
        if let Some(kids) = children.get(uuid_of(&chain[index].1)) {
            for &child in kids {
                if !done[child] {
                    waiting[child] -= 1;
                    if waiting[child] <= 0 {
                        ready.push(Reverse(child));
                    }
                }
            }
        }
    }
    let mut slots: Vec<Option<(usize, Map)>> = chain.into_iter().map(Some).collect();
    let ordered = order
        .into_iter()
        .map(|i| slots[i].take().expect("each once"))
        .collect();
    (ordered, present)
}

fn facts(number: usize, record: &Map, text: &Replacer) -> Map {
    let mut facts = object! {"line" => number, "uuid" => uuid_of(record)};
    if let Some(Value::Object(message)) = record.get("message") {
        for (source, name) in [("id", "messageId"), ("model", "model")] {
            if let Some(Value::Str(value)) = message.get(source) {
                facts.insert(name.into(), value.as_str().into());
            }
        }
    }
    for name in ["timestamp", "gitBranch", "version", "subtype"] {
        if let Some(Value::Str(value)) = record.get(name) {
            facts.insert(name.into(), value.as_str().into());
        }
    }
    if let Some(Value::Str(cwd)) = record.get("cwd") {
        facts.insert("cwd".into(), text.apply(cwd).into());
    }
    facts.insert("record".into(), get_none(record, "type"));
    facts
}

fn append(map: &mut Map, key: &str, item: Value) {
    match map
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
    {
        Value::Array(items) => items.push(item),
        _ => unreachable!("only ever an array"),
    }
}

pub struct Converted {
    pub turns: Vec<Turn>,
    pub heads: Map,
    pub session: Map,
}

pub fn convert(raw: &str, origin: &Origin, human: &Map) -> Result<Converted, NotJson> {
    let text = normalizer(origin);
    let records = parse(raw);
    let chain: Vec<(usize, Map)> = records
        .iter()
        .filter(|(_, r)| matches!(r.get("uuid"), Some(Value::Str(_))))
        .cloned()
        .collect();
    let (ordered, present) = topological(chain);

    let mut turn_of: HashMap<String, String> = HashMap::new();
    let mut turns: indexmap::IndexMap<String, Turn> = indexmap::IndexMap::new();
    let mut has_children: HashSet<String> = HashSet::new();
    let mut last_turn: Option<String> = None;
    let mut session = Map::new();

    for (number, record) in &ordered {
        let Some((kind, mut who, blocks)) = classify(record) else {
            continue;
        };
        let uuid = uuid_of(record).to_string();
        if kind == "event" {
            // Claude Code's own bookkeeping, folded into the facts of the
            // turn it follows rather than made a turn.
            let mut event = object! {"line" => *number};
            if let Value::Object(normalized) = normalize_value(&blocks[0], &text) {
                event.extend(normalized);
            }
            if let Some(Value::Str(stamp)) = record.get("timestamp") {
                event.insert("timestamp".into(), stamp.as_str().into());
            }
            let parent = parent_uuid(record).and_then(|p| turn_of.get(&p).cloned());
            match parent {
                None => append(&mut session, "events", Value::Object(event)),
                Some(parent) => {
                    turn_of.insert(uuid, parent.clone());
                    append(&mut turns[&parent].facts, "events", Value::Object(event));
                }
            }
            continue;
        }
        if who.get("kind").is_some_and(|k| k.is_str("human")) {
            who = human.clone();
        }
        let parent = parent_uuid(record);
        let mut turn_facts = facts(*number, record, &text);
        let mut parents = Vec::new();
        if let Some(parent) = parent {
            if let Some(turn) = turn_of.get(&parent) {
                parents.push(turn.clone());
            } else if !present.contains(&parent) {
                turn_facts.insert("danglingParent".into(), parent.into());
            }
        }
        let normalized: Vec<Value> = blocks.iter().map(|b| normalize_value(b, &text)).collect();
        let turn = format::make_turn(kind, &who, parents.clone(), normalized, turn_facts)?;
        turn_of.insert(uuid.clone(), turn.id.clone());
        last_turn = Some(turn.id.clone());
        match turns.get_mut(&turn.id) {
            Some(existing) => append(&mut existing.facts, "alsoUuids", uuid.into()),
            None => {
                turns.insert(turn.id.clone(), turn);
            }
        }
        has_children.extend(parents);
    }

    let mut current = last_turn;
    for (_, record) in &records {
        match record.get("type").and_then(Value::as_str) {
            Some("last-prompt") => {
                if let Some(Value::Str(leaf)) = record.get("leafUuid") {
                    if let Some(turn) = turn_of.get(leaf) {
                        current = Some(turn.clone());
                    }
                }
            }
            Some("ai-title") => {
                if let Some(Value::Str(title)) = record.get("aiTitle") {
                    session.insert("aiTitle".into(), title.as_str().into());
                }
            }
            Some("custom-title") => {
                if let Some(Value::Str(title)) = record.get("customTitle") {
                    session.insert("title".into(), title.as_str().into());
                }
            }
            _ => {}
        }
    }

    let leaves: Vec<Value> = turns
        .keys()
        .filter(|id| !has_children.contains(*id))
        .map(|id| Value::str(id.as_str()))
        .collect();
    let mut heads = object! {"leaves" => leaves};
    if let Some(current) = current {
        heads.insert("current".into(), current.into());
    }
    let session = match normalize_value(&Value::Object(session), &text) {
        Value::Object(map) => map,
        _ => unreachable!("an object normalizes to an object"),
    };
    Ok(Converted {
        turns: turns.into_values().collect(),
        heads,
        session,
    })
}

/// The transcript and everything Claude files beside it for the session:
/// subagent transcripts and spilled tool output under `<session-id>/`.
pub fn source_files(transcript_path: &str) -> Vec<(String, String)> {
    let mut files = vec![("transcript.jsonl".to_string(), transcript_path.to_string())];
    let sidecar = &transcript_path[..transcript_path.len().saturating_sub(".jsonl".len())];
    if std::path::Path::new(sidecar).is_dir() {
        let mut stack = vec![sidecar.to_string()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = sys::join(&dir, &entry.file_name().to_string_lossy());
                // os.walk files a symlink to a directory with the directories
                // and, without followlinks, never descends it: it is in
                // neither list.
                let real_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if real_dir {
                    stack.push(path);
                } else if std::path::Path::new(&path).is_dir() {
                    continue;
                } else {
                    let relative = sys::relpath(&path, sidecar);
                    files.push((format!("sidecar/{}", relative), path));
                }
            }
        }
    }
    files.sort_by(|a, b| py::cmp(&a.0, &b.0).then_with(|| py::cmp(&a.1, &b.1)));
    files
}

#[derive(Debug)]
pub enum IngestError {
    /// str(OSError), which Debug carries into the `internal` message.
    #[allow(dead_code)]
    Io(String),
    NotJson,
}

impl From<NotJson> for IngestError {
    fn from(_: NotJson) -> Self {
        IngestError::NotJson
    }
}

/// The content-addressed identity of the raw source, and the transcript text.
pub fn source_manifest(transcript_path: &str) -> Result<(Map, String), IngestError> {
    let mut entries = Vec::new();
    let mut transcript = String::new();
    for (relative, path) in source_files(transcript_path) {
        let data = fs::read(&path).map_err(|e| IngestError::Io(sys::os_error(&e, &path)))?;
        entries.push(Value::Object(object! {
            "path" => relative.as_str(),
            "sha256" => digest_bytes(&data),
            "bytes" => data.len(),
        }));
        if relative == "transcript.jsonl" {
            transcript = py::decode_surrogateescape(&data);
        }
    }
    let manifest = object! {"agent" => AGENT, "files" => entries};
    let mut source = object! {"id" => digest(&Value::Object(manifest.clone()))?};
    source.extend(manifest);
    Ok((source, transcript))
}

pub fn ingest(
    transcript_path: &str,
    origin: &Origin,
    author_id: Option<&str>,
) -> Result<Derived, IngestError> {
    let mut human = object! {"kind" => "human"};
    // Omitted rather than null: the encoding of "unknown" is part of every id.
    if let Some(id) = author_id {
        human.insert("id".into(), id.into());
    }
    let (source, transcript) = source_manifest(transcript_path)?;
    let converted = convert(&transcript, origin, &human)?;
    let mut header = object! {
        "loch" => FORMAT_VERSION,
        "converter" => converter(),
        "source" => source,
        "origin" => object! {
            "cwd" => origin.cwd.as_str(),
            "home" => origin.home.as_str(),
            "sessionId" => origin.session_id.as_str(),
        },
        "author" => human,
    };
    if !converted.session.is_empty() {
        header.insert("session".into(), Value::Object(converted.session));
    }
    Ok(Derived {
        header,
        turns: converted.turns,
        heads: converted.heads,
    })
}

pub fn derived_name(derived: &Derived) -> String {
    let id = derived.header["source"]
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let source = id.split_once(':').map(|(_, hex)| hex).unwrap_or(id);
    format!("{}.{}-v{}.jsonl", source, AGENT, CONVERTER_VERSION)
}

#[derive(Debug)]
pub struct AuthorKeyError(pub String);

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `base64.b64decode(text, validate=True)`: binascii's `a2b_base64` in
/// strict mode, which checks in this order and words each failure its way.
fn b64decode_strict(text: &str) -> Result<Vec<u8>, String> {
    if !text.is_ascii() {
        return Err("string argument should contain only ASCII characters".into());
    }
    let bytes = text.as_bytes();
    if bytes.first() == Some(&b'=') {
        return Err("Leading padding not allowed".into());
    }
    let mut out = Vec::new();
    let (mut quad, mut pads, mut left, mut padding) = (0usize, 0usize, 0u32, false);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'=' {
            padding = true;
            if quad == 0 {
                return Err("Excess padding not allowed".into());
            }
            if quad >= 2 {
                pads += 1;
                if quad + pads >= 4 {
                    if i + 1 < bytes.len() {
                        return Err("Excess data after padding".into());
                    }
                    return Ok(out);
                }
            }
            continue;
        }
        let Some(value) = B64.iter().position(|&c| c == b) else {
            return Err("Only base64 data is allowed".into());
        };
        if padding {
            return Err("Discontinuous padding not allowed".into());
        }
        pads = 0;
        let value = value as u32;
        match quad {
            0 => {
                left = value;
                quad = 1;
            }
            1 => {
                out.push((left << 2 | value >> 4) as u8);
                left = value & 0x0f;
                quad = 2;
            }
            2 => {
                out.push((left << 4 | value >> 2) as u8);
                left = value & 0x03;
                quad = 3;
            }
            _ => {
                out.push((left << 6 | value) as u8);
                quad = 0;
            }
        }
    }
    match quad {
        0 => Ok(out),
        1 => Err(format!(
            "Invalid base64-encoded string: number of data characters ({}) cannot be 1 more than a multiple of 4",
            out.len() / 3 * 4 + 1
        )),
        _ => Err("Incorrect padding".into()),
    }
}

/// `ssh:SHA256:<base64>`, the fingerprint `ssh-keygen -l` prints.
pub fn ssh_author_id(public_key: &str) -> Result<String, AuthorKeyError> {
    let fields = py::split(public_key);
    if fields.len() < 2 {
        return Err(AuthorKeyError("not an OpenSSH public key line".into()));
    }
    let blob = b64decode_strict(fields[1])
        .map_err(|e| AuthorKeyError(format!("key body is not base64: {}", e)))?;
    use sha2::{Digest, Sha256};
    let fingerprint = b64encode(&Sha256::digest(&blob));
    Ok(format!("ssh:SHA256:{}", fingerprint.trim_end_matches('=')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_binascii() {
        assert_eq!(b64encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64decode_strict("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(
            b64decode_strict("aGVsbG8").unwrap_err(),
            "Incorrect padding"
        );
        for (input, error) in [
            ("not*base64", "Only base64 data is allowed"),
            ("ab=c", "Discontinuous padding not allowed"),
            ("aGVs====", "Excess padding not allowed"),
            ("=aGVs", "Leading padding not allowed"),
            ("aGVsbA==x", "Excess data after padding"),
            ("aGVsbA===", "Excess data after padding"),
            ("a===", "Invalid base64-encoded string: number of data characters (1) cannot be 1 more than a multiple of 4"),
            ("aGVsb", "Invalid base64-encoded string: number of data characters (5) cannot be 1 more than a multiple of 4"),
        ] {
            assert_eq!(b64decode_strict(input).unwrap_err(), error, "{input}");
        }
        assert_eq!(b64decode_strict("abc=").unwrap(), b"i\xb7");
    }
}
