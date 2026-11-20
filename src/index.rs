//! Store key layout and derived index entries.

use crate::bundle::{unpack_bundle, Bundle, BundleSession};
use crate::json::{self, Value};
use crate::object;
use crate::store::{Store, StoreError};
use std::collections::BTreeSet;

pub const BUNDLES_PREFIX: &str = "bundles/";
pub const INDEX_PREFIX: &str = "index/";
pub const BRANCH: &str = "branch";

/// Percent-encoding rather than Claude's lossy scheme, which would file
/// feature/foo and feature-foo together. A value can't hold a control
/// character, so this can't collide with one.
pub const UNSET_SEGMENT: &str = "%00";

#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub ref_: String,
    pub dimension: String,
    pub value: Option<String>,
    pub agent: String,
    pub created_at: String,
    pub bytes: i64,
    pub sessions: i64,
    pub cwd: String,
    pub claude_version: Option<String>,
    /// The version of a non-Claude agent; `claudeVersion` keeps meaning Claude's.
    pub agent_version: Option<String>,
}

pub struct ReindexResult {
    pub bundles: usize,
    pub entries: usize,
    pub removed: usize,
}

pub fn bundle_key(ref_: &str) -> String {
    format!("{}{}.loch", BUNDLES_PREFIX, ref_)
}

pub fn ref_from_key(key: &str) -> String {
    key[BUNDLES_PREFIX.len()..key.len() - ".loch".len()].into()
}

/// `urllib.parse.quote(value, safe="")`.
pub fn quote(value: &str) -> String {
    let mut out = String::new();
    for byte in
        crate::py::encode_surrogateescape(value).unwrap_or_else(|_| value.as_bytes().to_vec())
    {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

/// `urllib.parse.unquote`: valid `%XX` escapes become bytes, decoded as UTF-8
/// with replacement in runs broken by any literal character.
pub fn unquote(value: &str) -> String {
    if !value.contains('%') {
        return value.into();
    }
    let bytes = value.as_bytes();
    let mut out = String::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("ASCII hex");
            pending.push(u8::from_str_radix(hex, 16).expect("checked hex"));
            i += 3;
            continue;
        }
        out.push_str(&String::from_utf8_lossy(&pending));
        pending.clear();
        let c = value[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    out.push_str(&String::from_utf8_lossy(&pending));
    out
}

pub fn encode_segment(value: Option<&str>) -> String {
    match value {
        None => UNSET_SEGMENT.into(),
        Some(value) => quote(value),
    }
}

pub fn dimension_prefix(dimension: &str) -> String {
    format!("{}{}/", INDEX_PREFIX, dimension)
}

pub fn value_prefix(dimension: &str, value: Option<&str>) -> String {
    format!("{}{}/", dimension_prefix(dimension), encode_segment(value))
}

pub fn index_key(dimension: &str, value: Option<&str>, ref_: &str) -> String {
    format!("{}{}", value_prefix(dimension, value), ref_)
}

fn entry_json(entry: &IndexEntry) -> Value {
    Value::Object(object! {
        "ref" => entry.ref_.as_str(),
        "dimension" => entry.dimension.as_str(),
        "value" => Value::opt_str(entry.value.as_deref()),
        "agent" => entry.agent.as_str(),
        "createdAt" => entry.created_at.as_str(),
        "bytes" => entry.bytes,
        "sessions" => entry.sessions,
        "cwd" => entry.cwd.as_str(),
        "claudeVersion" => Value::opt_str(entry.claude_version.as_deref()),
        "agentVersion" => Value::opt_str(entry.agent_version.as_deref()),
    })
}

fn entry_from_json(raw: &Value) -> Option<IndexEntry> {
    let text = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_string);
    let number = |key: &str| raw.get(key).and_then(Value::as_i64).unwrap_or(0);
    Some(IndexEntry {
        ref_: text("ref")?,
        dimension: text("dimension")?,
        value: text("value"),
        agent: text("agent").unwrap_or_default(),
        created_at: text("createdAt").unwrap_or_default(),
        bytes: number("bytes"),
        sessions: number("sessions"),
        cwd: text("cwd").unwrap_or_default(),
        claude_version: text("claudeVersion"),
        agent_version: text("agentVersion"),
    })
}

/// One entry per (branch, ref): a save without --branch can pack sessions
/// from several branches, so a bundle may be indexed under each of them.
pub fn entries_for(bundle: &Bundle, ref_: &str, size: usize) -> Vec<IndexEntry> {
    let mut grouped: indexmap::IndexMap<Option<String>, Vec<&BundleSession>> = Default::default();
    for session in &bundle.sessions {
        grouped
            .entry(session.git_branch.clone())
            .or_default()
            .push(session);
    }
    let mut groups: Vec<(Option<String>, Vec<&BundleSession>)> = grouped.into_iter().collect();
    groups.sort_by(|a, b| {
        crate::py::cmp(
            &encode_segment(a.0.as_deref()),
            &encode_segment(b.0.as_deref()),
        )
    });
    groups
        .into_iter()
        .map(|(branch, sessions)| IndexEntry {
            ref_: ref_.into(),
            dimension: BRANCH.into(),
            value: branch,
            // A bundle can hold several agents' sessions; each entry names
            // its first session's.
            agent: sessions[0].agent_of(bundle).to_string(),
            created_at: bundle.created_at.clone(),
            bytes: size as i64,
            sessions: sessions.len() as i64,
            cwd: sessions[0].cwd.clone(),
            claude_version: sessions
                .iter()
                .find_map(|s| s.claude_version.clone().filter(|v| !v.is_empty())),
            agent_version: sessions
                .iter()
                .find_map(|s| s.agent_version.clone().filter(|v| !v.is_empty())),
        })
        .collect()
}

pub fn write_entries(store: &dyn Store, entries: &[IndexEntry]) -> Result<Vec<String>, StoreError> {
    let mut keys = Vec::new();
    for entry in entries {
        let key = index_key(&entry.dimension, entry.value.as_deref(), &entry.ref_);
        store.put(
            &key,
            json::dumps(&entry_json(entry), json::DEFAULT).as_bytes(),
        )?;
        keys.push(key);
    }
    Ok(keys)
}

pub fn index_bundle(
    store: &dyn Store,
    bundle: &Bundle,
    ref_: &str,
    size: usize,
) -> Result<Vec<IndexEntry>, StoreError> {
    let entries = entries_for(bundle, ref_, size);
    write_entries(store, &entries)?;
    Ok(entries)
}

#[derive(Debug)]
pub enum IndexError {
    Store(StoreError),
    /// Bytes that arrived and won't parse: an entry or a bundle.
    Unreadable(String),
}

impl From<StoreError> for IndexError {
    fn from(error: StoreError) -> Self {
        IndexError::Store(error)
    }
}

pub fn load_entries(store: &dyn Store, prefix: &str) -> Result<Vec<IndexEntry>, IndexError> {
    let mut entries = Vec::new();
    for key in store.list(prefix)? {
        let data = store.get(&key)?;
        let entry = std::str::from_utf8(&data)
            .ok()
            .and_then(|text| json::loads(text).ok())
            .and_then(|value| entry_from_json(&value))
            .ok_or_else(|| IndexError::Unreadable(format!("unreadable index entry {}", key)))?;
        entries.push(entry);
    }
    entries.sort_by(|a, b| crate::py::cmp(&b.created_at, &a.created_at));
    Ok(entries)
}

/// Entries first: they are derived from the bundle, so removing it before
/// them would strand any that a failure here left behind.
pub fn delete_bundle(store: &dyn Store, ref_: &str) -> Result<Vec<String>, IndexError> {
    let packed = store.get(&bundle_key(ref_))?;
    let bundle = unpack_bundle(&packed).map_err(IndexError::Unreadable)?;
    let mut removed = Vec::new();
    for entry in entries_for(&bundle, ref_, packed.len()) {
        let key = index_key(&entry.dimension, entry.value.as_deref(), &entry.ref_);
        store.delete(&key)?;
        removed.push(key);
    }
    store.delete(&bundle_key(ref_))?;
    removed.push(bundle_key(ref_));
    Ok(removed)
}

/// Rebuilds the branch dimension from the bundles. Other dimensions are
/// derived from other sources (`file`, from edit records) and are left alone.
pub fn reindex(store: &dyn Store) -> Result<ReindexResult, IndexError> {
    let stale: BTreeSet<String> = store.list(&dimension_prefix(BRANCH))?.into_iter().collect();
    let mut bundles = 0;
    let mut written: BTreeSet<String> = BTreeSet::new();
    for key in store.list(BUNDLES_PREFIX)? {
        if !key.ends_with(".loch") {
            continue;
        }
        let packed = store.get(&key)?;
        let bundle = unpack_bundle(&packed).map_err(IndexError::Unreadable)?;
        let entries = entries_for(&bundle, &ref_from_key(&key), packed.len());
        written.extend(write_entries(store, &entries)?);
        bundles += 1;
    }
    let mut removed: Vec<&String> = stale.difference(&written).collect();
    removed.sort_by(|a, b| crate::py::cmp(a, b));
    for key in &removed {
        store.delete(key)?;
    }
    Ok(ReindexResult {
        bundles,
        entries: written.len(),
        removed: removed.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_round_trip_like_urllib() {
        assert_eq!(quote("feature/foo bar~é"), "feature%2Ffoo%20bar~%C3%A9");
        assert_eq!(unquote("feature%2Ffoo%20bar~%C3%A9"), "feature/foo bar~é");
        assert_eq!(unquote("%E2x%zz%4"), "\u{fffd}x%zz%4");
    }
}
