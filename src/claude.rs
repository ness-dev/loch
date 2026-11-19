//! The Claude Code adapter: where sessions live, how their directory is
//! named, what a session's metadata is, how a turn reads in the transcript,
//! and how a conversation is forked.

pub use crate::agent::SessionMeta;
use crate::agent::{Agent, HookEvent, TurnInfo};
use crate::json::{self, Map, Value};
use crate::object;
use crate::py;
use crate::redact;
use crate::rewrite::{rewrite_transcript, RewriteSpec};
use crate::store::{Store, StoreError};
use crate::sys;
use crate::trace::{self, content, records_of, Fork, ForkError, TraceError};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

pub const AGENT: &str = "claude-code";

pub const SUMMARY_LIMIT: usize = 120;

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

/// Each line stripped, then parsed; dicts only.
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
    // Replacing rather than strict: one undecodable
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
        agent: AGENT,
        session_id,
        path: path.into(),
        git_branch: branches.get(&cwd).cloned(),
        version: versions
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

// -- reading a turn --------------------------------------------------------------

/// A record a person typed: a user message that is neither a tool's result
/// nor Claude Code's own bookkeeping.
pub fn is_human_prompt(record: &Map) -> bool {
    if !record.get("type").is_some_and(|t| t.is_str("user")) {
        return false;
    }
    if record.get("isMeta").is_some_and(Value::truthy)
        || record.get("isCompactSummary").is_some_and(Value::truthy)
    {
        return false;
    }
    match record.get("message").and_then(|m| m.get("content")) {
        Some(Value::Str(_)) => true,
        Some(Value::Array(_)) => !content(record)
            .iter()
            .any(|b| b.get("type").is_some_and(|t| t.is_str("tool_result"))),
        _ => false,
    }
}

pub fn prompt_text(record: &Map) -> Option<String> {
    match record.get("message")?.get("content")? {
        Value::Str(text) => Some(text.clone()),
        Value::Array(_) => content(record)
            .iter()
            .find_map(|block| match block.get("text") {
                Some(Value::Str(text)) if !text.is_empty() => Some(text.clone()),
                _ => None,
            }),
        _ => None,
    }
}

/// The turn the transcript ends with. When the hook closing it belongs to the
/// *next* prompt, that prompt may already be written; `excluding` names it,
/// and it and everything after are left out.
pub fn current_turn(transcript: &str, excluding: Option<&str>) -> TurnInfo {
    let records: Vec<Map> = trace::read_records(transcript)
        .into_iter()
        .filter(|r| matches!(r.get("uuid"), Some(Value::Str(_))))
        .collect();
    let mut end = records.len();
    if let Some(next) = excluding {
        if let Some(at) = records.iter().rposition(is_human_prompt) {
            if prompt_text(&records[at]).as_deref() == Some(next) {
                end = at;
            }
        }
    }
    let mut info = TurnInfo::default();
    for record in &records[..end] {
        let uuid = record
            .get("uuid")
            .and_then(Value::as_str)
            .map(str::to_string);
        let stamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        if is_human_prompt(record) {
            info.prompt_uuid = uuid.clone();
            info.prompt = display_summary(prompt_text(record).as_deref());
            info.started = stamp.clone();
        }
        info.end_uuid = uuid;
        info.ended = stamp;
    }
    info
}

// -- finding a transcript ------------------------------------------------------------

fn list_dir(path: &str) -> Vec<String> {
    fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Claude's own copy if it still exists, else the store's snapshot, fetched
/// into a cache laid out like Claude's project directory so the session's cwd
/// is chosen the same way.
pub fn find_transcript(
    session_id: &str,
    store: Option<&dyn Store>,
) -> Result<Option<String>, StoreError> {
    let root = projects_dir(None);
    if Path::new(&root).is_dir() {
        for project in list_dir(&root) {
            let path = sys::join(
                &sys::join(&root, &project),
                &format!("{}.jsonl", session_id),
            );
            if Path::new(&path).exists() {
                return Ok(Some(path));
            }
        }
    }
    let Some(store) = store else {
        return Ok(None);
    };
    let prefix = trace::transcripts_prefix(AGENT);
    let Ok(keys) = store.list(&prefix) else {
        return Ok(None);
    };
    let suffix = format!("/{}.jsonl", session_id);
    for key in keys {
        if key.ends_with(&suffix) {
            let project = key[prefix.len()..]
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            let data = match store.get(&key) {
                Ok(data) => data,
                Err(StoreError::Missing(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
            let cache = sys::join(&trace::cache_dir(), &project);
            let cached = sys::join(&cache, &format!("{}.jsonl", session_id));
            let _ = fs::create_dir_all(&cache);
            let _ = fs::write(&cached, data);
            return Ok(Some(cached));
        }
    }
    Ok(None)
}

// -- forking -------------------------------------------------------------------------

fn parent(record: &Map) -> Option<String> {
    let parent = match record.get("parentUuid") {
        None | Some(Value::Null) => record.get("logicalParentUuid"),
        other => other,
    };
    parent.and_then(Value::as_str).map(str::to_string)
}

fn uuid(record: &Map) -> &str {
    record.get("uuid").and_then(Value::as_str).unwrap_or("")
}

/// A new Claude session holding the conversation, filed under `target_cwd`.
/// With no `cut` it holds the conversation to its end — the last record
/// written, which after a rewind is the branch the session was left on. With
/// one, it ends at that record: the last of the turn being asked about.
pub fn fork_session(
    transcript: &str,
    target_cwd: &str,
    cut: Option<&str>,
    title: Option<&str>,
) -> Result<Fork, ForkError> {
    let bytes =
        fs::read(transcript).map_err(|e| ForkError::Internal(sys::os_error(&e, transcript)))?;
    let raw = py::decode_surrogateescape(&bytes);
    let records: Vec<Map> = records_of(&raw)
        .into_iter()
        .filter(|r| matches!(r.get("uuid"), Some(Value::Str(_))))
        .collect();
    if records.is_empty() {
        return Err(ForkError::Trace(TraceError::new(
            "turn-not-found",
            "the transcript holds no records",
        )));
    }
    let leaf = match cut {
        None => records.len() - 1,
        Some(cut) => match records.iter().rposition(|r| uuid(r) == cut) {
            Some(leaf) => leaf,
            None => {
                return Err(ForkError::Trace(TraceError::new(
                    "turn-not-found",
                    format!("the transcript has no record {}", cut),
                )))
            }
        },
    };
    write_fork(transcript, &raw, &records, leaf, target_cwd, title)
}

fn write_fork(
    transcript: &str,
    raw: &str,
    records: &[Map],
    leaf: usize,
    target_cwd: &str,
    title: Option<&str>,
) -> Result<Fork, ForkError> {
    let mut by_uuid: HashMap<&str, usize> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        by_uuid.insert(uuid(record), index);
    }
    let mut keep: HashSet<String> = HashSet::new();
    let mut walker = Some(leaf);
    while let Some(index) = walker {
        let id = uuid(&records[index]).to_string();
        if keep.contains(&id) {
            break;
        }
        keep.insert(id);
        walker = parent(&records[index]).and_then(|p| by_uuid.get(p.as_str()).copied());
    }

    let origin_cwd = match read_session_meta(transcript) {
        Some(meta) => meta.cwd,
        None => match transcript_cwds(raw).into_iter().next() {
            Some((cwd, _)) => cwd,
            None => return Err(ForkError::Internal("the transcript names no cwd".into())),
        },
    };
    let file = sys::basename(transcript);
    let origin_session = file[..file.len().saturating_sub(".jsonl".len())].to_string();
    let new_id = sys::uuid4();

    let mut kept: Vec<String> = records
        .iter()
        .filter(|r| keep.contains(uuid(r)))
        .map(|r| json::dumps(&Value::Object(r.clone()), json::UNICODE))
        .collect();
    let default_title = format!("loch ask: {}", py::truncate(&origin_session, 8));
    kept.push(json::dumps(
        &Value::Object(object! {
            "type" => "custom-title",
            "customTitle" => title.filter(|t| !t.is_empty()).unwrap_or(&default_title),
            "sessionId" => origin_session.as_str(),
        }),
        json::DEFAULT,
    ));
    let home = home_dir();
    let spec = RewriteSpec {
        origin_cwd,
        origin_home: home.clone(),
        target_cwd: target_cwd.into(),
        target_home: home,
        origin_session_id: Some(origin_session.clone()),
        target_session_id: Some(new_id.clone()),
    };
    let text = rewrite_transcript(&format!("{}\n", kept.join("\n")), &spec);
    let path = transcript_path_for(target_cwd, &new_id, None);
    let parent_dir = sys::dirname(&path);
    fs::create_dir_all(&parent_dir)
        .map_err(|e| ForkError::Internal(sys::os_error(&e, &parent_dir)))?;
    let bytes = py::encode_surrogateescape(&text)
        .map_err(|_| ForkError::Internal("the fork holds an unencodable surrogate".into()))?;
    fs::write(&path, bytes).map_err(|e| ForkError::Internal(sys::os_error(&e, &path)))?;
    Ok(Fork {
        session_id: new_id,
        path,
        cwd: target_cwd.into(),
        records: keep.len(),
        origin_session,
    })
}

// -- the adapter -----------------------------------------------------------------------

pub struct ClaudeCode;

pub static CLAUDE_CODE: ClaudeCode = ClaudeCode;

/// A turn's two moments, which is all attribution needs, and the session's
/// end, which closes a turn a cancel left open.
const HOOK_EVENTS: &[HookEvent] = &[
    ("UserPromptSubmit", "prompt"),
    ("Stop", "stop"),
    ("SessionEnd", "end"),
];

impl Agent for ClaudeCode {
    fn id(&self) -> &'static str {
        AGENT
    }

    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn hook_events(&self) -> &'static [HookEvent] {
        HOOK_EVENTS
    }

    /// The project's settings file, or with `local` the uncommitted one, since
    /// a `--local` command is a machine-specific path.
    fn hooks_file(&self, root: &str, local: bool) -> String {
        let name = if local {
            "settings.local.json"
        } else {
            "settings.json"
        };
        sys::join(&sys::join(root, ".claude"), name)
    }

    fn install_note(&self) -> &'static str {
        "restart Claude Code sessions here to pick them up"
    }

    fn current_turn(&self, transcript: &str, event: &Map, for_next: bool) -> TurnInfo {
        let next = match event.get("prompt") {
            Some(Value::Str(text)) if for_next => Some(text.as_str()),
            _ => None,
        };
        current_turn(transcript, next)
    }

    /// Keyed by Claude's project directory, so a fetched copy can be filed
    /// back under it and its cwd chosen the same way.
    fn transcript_key(&self, path: &str, session_id: &str) -> Option<String> {
        let project = sys::basename(&sys::dirname(path));
        Some(format!(
            "{}{}/{}.jsonl",
            trace::transcripts_prefix(AGENT),
            project,
            session_id
        ))
    }

    fn sessions_dir(&self) -> String {
        projects_dir(None)
    }

    fn list_sessions(
        &self,
        cwd: &str,
        branch: Option<&str>,
        session_id: Option<&str>,
    ) -> Vec<SessionMeta> {
        list_sessions(cwd, branch, session_id, None)
    }

    fn read_session_meta(&self, path: &str) -> Option<SessionMeta> {
        read_session_meta(path)
    }

    fn transcript_cwds(&self, raw: &str) -> Vec<(String, usize)> {
        transcript_cwds(raw)
    }

    fn restore_path(&self, _raw: &str, cwd: &str, session_id: &str) -> Option<String> {
        Some(transcript_path_for(cwd, session_id, None))
    }

    fn existing_transcript(&self, cwd: &str, session_id: &str) -> Option<String> {
        let path = transcript_path_for(cwd, session_id, None);
        Path::new(&path).exists().then_some(path)
    }

    fn resume_command(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec!["claude".into(), "--resume".into(), session_id.into()])
    }

    fn find_transcript(
        &self,
        session_id: &str,
        store: Option<&dyn Store>,
    ) -> Result<Option<String>, StoreError> {
        find_transcript(session_id, store)
    }

    fn fork(
        &self,
        transcript: &str,
        target_cwd: &str,
        cut: Option<&str>,
        title: Option<&str>,
    ) -> Result<Fork, ForkError> {
        fork_session(transcript, target_cwd, cut, title)
    }
}
