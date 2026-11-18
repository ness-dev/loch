//! The Claude Code adapter, as in `loch/claude.py`: where sessions live, how
//! their directory is named, and what a session's metadata is.

use crate::json::{self, Value};
use crate::py;
use crate::redact;
use crate::sys;
use std::fs;

pub const AGENT: &str = "claude-code";

pub const SUMMARY_LIMIT: usize = 120;

#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub session_id: String,
    pub path: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub claude_version: Option<String>,
    pub bytes: u64,
    pub modified_at: String,
    pub summary: Option<String>,
    /// Directories the session also worked in before it moved. A restore has
    /// one target cwd, so records naming these cannot be made correct on the
    /// far side; surfacing them is the only honest option.
    pub other_cwds: Vec<String>,
}

/// `Path.home()`: `HOME` on Unix, `USERPROFILE` on Windows.
pub fn home_dir() -> String {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(name).unwrap_or_default()
}

/// `CLAUDE_CONFIG_DIR` when set, which is how one machine runs several
/// accounts, else `~/.claude`. Only the variable is consulted, never both
/// places, because that is the rule Claude itself follows — a restore filed
/// anywhere else is one `--resume` can't find. An explicit `home` names
/// another machine's layout and ignores it.
pub fn config_dir(home: Option<&str>) -> String {
    if home.is_none() {
        if let Ok(configured) = std::env::var("CLAUDE_CONFIG_DIR") {
            if !configured.is_empty() {
                return configured;
            }
        }
    }
    let home = home.map(str::to_string).unwrap_or_else(home_dir);
    sys::join(&home, ".claude")
}

pub fn projects_dir(home: Option<&str>) -> String {
    sys::join(&config_dir(home), "projects")
}

/// Claude resolves symlinks before deriving the directory name, so /tmp
/// becomes /private/tmp on macOS. Resolving here keeps lookups matching.
pub fn resolve_cwd(cwd: &str) -> String {
    sys::realpath_strict(cwd).unwrap_or_else(|| cwd.to_string())
}

/// `re.sub(r"[^a-zA-Z0-9]", "-", path)`: every code point, a surrogate
/// included, that isn't an ASCII letter or digit.
pub fn encode_path(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn encode_cwd(cwd: &str) -> String {
    encode_path(&resolve_cwd(cwd))
}

pub fn session_dir_for(cwd: &str, home: Option<&str>) -> String {
    sys::join(&projects_dir(home), &encode_cwd(cwd))
}

pub fn transcript_path_for(cwd: &str, session_id: &str, home: Option<&str>) -> String {
    sys::join(
        &session_dir_for(cwd, home),
        &format!("{}.jsonl", session_id),
    )
}

fn first_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Str(text)) if !text.is_empty() && text != "-" => Some(text.clone()),
        _ => None,
    }
}

/// claude.py's `_records`: each line stripped, then parsed; dicts only.
pub fn records(raw: &str) -> impl Iterator<Item = json::Map> + '_ {
    raw.split('\n').filter_map(|line| {
        let line = py::strip(line);
        if line.is_empty() {
            return None;
        }
        match json::loads(line) {
            Ok(Value::Object(map)) => Some(map),
            _ => None,
        }
    })
}

/// Every cwd a transcript's records name, with how many name it, in order of
/// first appearance. Only the top-level field counts: `"cwd"` also occurs
/// inside tool inputs and results — loch's own --json output among them —
/// where it names some other process's directory.
pub fn transcript_cwds(raw: &str) -> Vec<(String, usize)> {
    let mut counts: indexmap::IndexMap<String, usize> = indexmap::IndexMap::new();
    for record in records(raw) {
        if let Some(cwd) = first_string(record.get("cwd")) {
            *counts.entry(cwd).or_insert(0) += 1;
        }
    }
    counts.into_iter().collect()
}

/// Which of a session's cwd values the transcript is actually filed under.
/// The encoding is only ever compared here, never decoded: a match is strong
/// evidence, a mismatch is conclusive. Nothing matching falls back to the
/// first cwd.
pub fn cwd_for_directory(cwds: &[String], directory: &str) -> String {
    cwds.iter()
        .find(|cwd| encode_path(cwd) == directory)
        .unwrap_or(&cwds[0])
        .clone()
}

fn extract_summary(record: &json::Map) -> Option<String> {
    if !record.get("type").is_some_and(|t| t.is_str("user")) {
        return None;
    }
    let content = record.get("message")?.get("content")?;
    match content {
        Value::Str(text) => Some(text.clone()),
        Value::Array(blocks) => blocks.iter().find_map(|block| match block.get("text") {
            Some(Value::Str(text)) if !text.is_empty() => Some(text.clone()),
            _ => None,
        }),
        _ => None,
    }
}

/// The one piece of transcript content that travels as metadata, so it is
/// scrubbed here, before truncation, since a cut can leave a secret's tail
/// matching no rule. A summary that won't come clean is dropped whole.
pub fn display_summary(text: Option<&str>) -> Option<String> {
    let text = text?;
    Some(match redact::redact(text) {
        Ok(redaction) => py::truncate(&redaction.text, SUMMARY_LIMIT).to_string(),
        Err(_) => "[REDACTED]".into(),
    })
}

pub fn read_session_meta(path: &str) -> Option<SessionMeta> {
    let meta = fs::metadata(path).ok()?;
    // Replacing rather than strict, as claude.py now does: one undecodable
    // byte must not take down a listing.
    let raw = py::decode_replace(&fs::read(path).ok()?);
    let file = path.rsplit('/').next().unwrap_or(path);
    let session_id = file.strip_suffix(".jsonl").unwrap_or(file).to_string();

    let mut counts: indexmap::IndexMap<String, usize> = indexmap::IndexMap::new();
    let mut branches: std::collections::HashMap<String, String> = Default::default();
    let mut versions: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
    let mut summary: Option<String> = None;
    // Whole file rather than a bounded window: the cwd the session is filed
    // under can first appear hundreds of records in.
    for record in records(&raw) {
        if let Some(cwd) = first_string(record.get("cwd")) {
            *counts.entry(cwd.clone()).or_insert(0) += 1;
            if let Some(branch) = first_string(record.get("gitBranch")) {
                branches.entry(cwd.clone()).or_insert(branch);
            }
            if let Some(version) = first_string(record.get("version")) {
                versions.entry(cwd).or_insert(version);
            }
        }
        if summary.is_none() {
            summary = extract_summary(&record);
        }
    }
    if counts.is_empty() {
        return None;
    }
    let ordered: Vec<String> = counts.keys().cloned().collect();
    let cwd = cwd_for_directory(&ordered, &sys::basename(&sys::dirname(path)));
    Some(SessionMeta {
        session_id,
        path: path.into(),
        git_branch: branches.get(&cwd).cloned(),
        claude_version: versions
            .get(&cwd)
            .or_else(|| versions.values().next())
            .cloned(),
        bytes: meta.len(),
        modified_at: sys::iso_timestamp(sys::mtime(&meta)),
        summary: display_summary(summary.as_deref()),
        other_cwds: ordered.iter().filter(|o| **o != cwd).cloned().collect(),
        cwd,
    })
}

pub fn list_sessions(
    cwd: &str,
    branch: Option<&str>,
    session_id: Option<&str>,
    home: Option<&str>,
) -> Vec<SessionMeta> {
    let directory = session_dir_for(cwd, home);
    let Ok(entries) = fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut metas = Vec::new();
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if !file.ends_with(".jsonl") {
            continue;
        }
        if let Some(id) = session_id.filter(|s| !s.is_empty()) {
            if file != format!("{}.jsonl", id) {
                continue;
            }
        }
        let Some(meta) = read_session_meta(&sys::join(&directory, &file)) else {
            continue;
        };
        if let Some(branch) = branch.filter(|b| !b.is_empty()) {
            if meta.git_branch.as_deref() != Some(branch) {
                continue;
            }
        }
        metas.push(meta);
    }
    // sorted(..., reverse=True) is stable, as sort_by is.
    metas.sort_by(|a, b| py::cmp(&b.modified_at, &a.modified_at));
    metas
}
