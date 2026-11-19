//! From a turn back to its conversation: installing the hooks that record
//! turns, finding a session's transcript wherever it survives, describing a
//! turn for blame, and forking the conversation for `ask`.
//!
//! What a turn changed is recorded by the hooks (`hook.rs`) and matched by
//! content (`attribution.rs`); nothing here reads or writes git history.

use crate::attribution::{EditRecord, TurnRecord};
use crate::claude::{self, home_dir, projects_dir, read_session_meta, AGENT};
use crate::format::Turn;
use crate::git as gitio;
use crate::ingest::{self, Origin};
use crate::json::{self, Map, Value};
use crate::object;
use crate::py;
use crate::rewrite::{rewrite_transcript, RewriteSpec};
use crate::store::{Store, StoreError};
use crate::sys;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

pub fn transcripts_prefix() -> String {
    format!("transcripts/{}/", AGENT)
}

#[derive(Debug)]
pub struct TraceError {
    pub code: &'static str,
    pub message: String,
}

impl TraceError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        TraceError {
            code,
            message: message.into(),
        }
    }
}

/// Git's stdout whatever its exit status; empty if git can't run.
pub fn git_unchecked(cwd: &str, args: &[&str]) -> String {
    gitio::git_unchecked(cwd, args).unwrap_or_default()
}

/// `str(value)` for a value read out of JSON.
pub fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(Value::Int(d)) => d.clone(),
        Some(Value::Float(f)) => json::float_repr(*f),
        Some(Value::Str(s)) => s.clone(),
        Some(other) => json::dumps(other, json::DEFAULT),
    }
}

// -- install ------------------------------------------------------------------

/// A turn's two moments, which is all attribution needs, and the session's
/// end, which closes a turn a cancel left open.
const HOOK_EVENTS: &[(&str, &str)] = &[
    ("UserPromptSubmit", "prompt"),
    ("Stop", "stop"),
    ("SessionEnd", "end"),
];

/// What earlier versions of `install` wrote around every Bash call, for the
/// git-notes design. Removed on install, so they stop costing a process each.
const RETIRED: &[&str] = &["pre-tool", "post-tool"];

#[derive(Debug)]
pub enum InstallError {
    Internal(String),
}

pub struct Installed {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

fn hook_line(command: &str, name: &str) -> String {
    // A checkout whose machine lacks loch gets a silent no-op rather than an
    // error on every turn, since the settings file is committed.
    format!("{} hook {} 2>/dev/null || true", command, name)
}

fn is_retired(line: &str) -> bool {
    RETIRED
        .iter()
        .any(|name| line.ends_with(&format!(" hook {} 2>/dev/null || true", name)))
}

/// Adds loch's hooks to a Claude Code settings file, keeping whatever else is
/// there, and takes out the ones it no longer uses. Re-running changes
/// nothing.
pub fn install_hooks(
    settings_path: &str,
    command: &str,
) -> std::result::Result<Installed, InstallError> {
    let mut settings = match fs::read(settings_path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes)
                .map_err(|_| InstallError::Internal(format!("{} is not UTF-8", settings_path)))?;
            json::loads(&text).map_err(|_| {
                InstallError::Internal(format!("{} is not valid JSON", settings_path))
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(error) => return Err(InstallError::Internal(sys::os_error(&error, settings_path))),
    };
    let not_a_dict = || InstallError::Internal("the settings are not a JSON object".into());
    let Value::Object(root) = &mut settings else {
        return Err(not_a_dict());
    };
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks) = hooks else {
        return Err(not_a_dict());
    };

    let mut removed = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        let Value::Array(groups) = groups else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(Value::Array(entries)) =
                group.as_object_mut().and_then(|g| g.get_mut("hooks"))
            {
                let before = entries.len();
                entries.retain(|entry| {
                    !entry
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(is_retired)
                });
                if entries.len() < before {
                    removed.push(event.clone());
                }
            }
        }
        // A group the retired line was alone in would be an empty husk.
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|entries| !entries.is_empty())
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));

    let mut added = Vec::new();
    for (event, name) in HOOK_EVENTS {
        let line = hook_line(command, name);
        let groups = hooks
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(groups) = groups else {
            return Err(not_a_dict());
        };
        let present = groups.iter().any(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|entry| entry.get("command").is_some_and(|c| c.is_str(&line)))
                })
        });
        if present {
            continue;
        }
        groups.push(Value::Object(object! {
            "hooks" => vec![Value::Object(object! {"type" => "command", "command" => line.as_str()})],
        }));
        added.push(event.to_string());
    }
    let parent = sys::dirname(settings_path);
    fs::create_dir_all(&parent).map_err(|e| InstallError::Internal(sys::os_error(&e, &parent)))?;
    let text = format!("{}\n", json::dumps(&settings, json::INDENTED));
    fs::write(settings_path, text)
        .map_err(|e| InstallError::Internal(sys::os_error(&e, settings_path)))?;
    removed.dedup();
    Ok(Installed { added, removed })
}

// -- transcripts ----------------------------------------------------------------

/// A line is skipped when blank but parsed unstripped — unlike
/// `claude::records`, which strips first.
pub fn records_of(raw: &str) -> Vec<Map> {
    raw.split('\n')
        .filter(|line| !py::strip(line).is_empty())
        .filter_map(|line| match json::loads(line) {
            Ok(Value::Object(record)) => Some(record),
            _ => None,
        })
        .collect()
}

pub fn read_records(path: &str) -> Vec<Map> {
    match fs::read(path) {
        Ok(bytes) => records_of(&py::decode_surrogateescape(&bytes)),
        Err(_) => Vec::new(),
    }
}

pub fn content(record: &Map) -> Vec<&Map> {
    match record.get("message") {
        Some(Value::Object(message)) => match message.get("content") {
            Some(Value::Array(blocks)) => blocks.iter().filter_map(Value::as_object).collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

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
) -> std::result::Result<Option<String>, StoreError> {
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
    let prefix = transcripts_prefix();
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
            let cache = sys::join(
                &sys::join(&sys::join(&home_dir(), ".loch"), "cache"),
                &project,
            );
            let cached = sys::join(&cache, &format!("{}.jsonl", session_id));
            let _ = fs::create_dir_all(&cache);
            let _ = fs::write(&cached, data);
            return Ok(Some(cached));
        }
    }
    Ok(None)
}

// -- describing a turn -------------------------------------------------------------

/// A turn blame pointed at, with what can be said about its conversation.
#[derive(Clone)]
pub struct Link {
    pub session_id: String,
    pub agent: String,
    pub index: usize,
    pub ended: String,
    pub prompt_uuid: Option<String>,
    pub end_uuid: Option<String>,
    pub transcript: Option<String>,
    /// The loch-format id of the turn holding the prompt, and of the root of
    /// its conversation, when the transcript could be ingested.
    pub turn: Option<String>,
    pub conversation: Option<String>,
    pub title: Option<String>,
    pub prompt: Option<String>,
}

impl Link {
    pub fn payload(&self) -> Map {
        object! {
            "session" => self.session_id.as_str(),
            "agent" => self.agent.as_str(),
            "index" => self.index,
            "ended" => self.ended.as_str(),
            "available" => self.transcript.is_some(),
            "turn" => Value::opt_str(self.turn.as_deref()),
            "conversation" => Value::opt_str(self.conversation.as_deref()),
            "title" => Value::opt_str(self.title.as_deref()),
            "prompt" => Value::opt_str(self.prompt.as_deref()),
        }
    }
}

/// Describes turns, ingesting each session's transcript once however many
/// lines point into it.
pub struct Resolver<'a> {
    store: Option<&'a dyn Store>,
    sessions: HashMap<String, (Option<String>, Vec<Turn>, Map)>,
}

impl<'a> Resolver<'a> {
    pub fn new(store: Option<&'a dyn Store>) -> Self {
        Resolver {
            store,
            sessions: HashMap::new(),
        }
    }

    fn session(
        &mut self,
        session_id: &str,
    ) -> std::result::Result<&(Option<String>, Vec<Turn>, Map), String> {
        if !self.sessions.contains_key(session_id) {
            let path = find_transcript(session_id, self.store).map_err(|e| e.to_string())?;
            let mut turns = Vec::new();
            let mut header = Map::new();
            if let Some(path) = &path {
                if let Some(meta) = read_session_meta(path) {
                    let origin = Origin {
                        cwd: meta.cwd,
                        home: home_dir(),
                        session_id: session_id.into(),
                    };
                    if let Ok(derived) = ingest::ingest(path, &origin, None) {
                        turns = derived.turns;
                        header = derived.header;
                    }
                }
            }
            self.sessions
                .insert(session_id.into(), (path, turns, header));
        }
        Ok(&self.sessions[session_id])
    }

    pub fn describe(
        &mut self,
        record: &EditRecord,
        index: usize,
    ) -> std::result::Result<Link, String> {
        let turn: &TurnRecord = &record.turns[index];
        let (path, turns, header) = self.session(&record.session)?;
        // The loch turn whose source record is the prompt that began this
        // turn — or that a resubmitted identical prompt folded into.
        let holds = |candidate: &&Turn, uuid: &str| {
            candidate.facts.get("uuid").is_some_and(|u| u.is_str(uuid))
                || candidate
                    .facts
                    .get("alsoUuids")
                    .and_then(Value::as_array)
                    .is_some_and(|also| also.iter().any(|u| u.is_str(uuid)))
        };
        let found = turn
            .prompt_uuid
            .as_deref()
            .and_then(|uuid| turns.iter().find(|candidate| holds(candidate, uuid)));
        let by_id: HashMap<&str, &Turn> = turns.iter().map(|t| (t.id.as_str(), t)).collect();
        let mut conversation = None;
        let mut walker = found;
        while let Some(current) = walker {
            let parents = current.parents();
            if parents.is_empty() {
                conversation = Some(current.id.clone());
            }
            walker = parents.first().and_then(|p| by_id.get(p.as_str()).copied());
        }
        let empty = Map::new();
        let session = header
            .get("session")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let title_of = |key: &str| match session.get(key) {
            Some(v) if v.truthy() => v.as_str().map(str::to_string),
            _ => None,
        };
        Ok(Link {
            session_id: record.session.clone(),
            agent: record.agent.clone(),
            index,
            ended: turn.ended.clone(),
            prompt_uuid: turn.prompt_uuid.clone(),
            end_uuid: turn.end_uuid.clone(),
            transcript: path.clone(),
            turn: found.map(|t| t.id.clone()),
            conversation,
            title: title_of("title").or_else(|| title_of("aiTitle")),
            prompt: turn.prompt.clone(),
        })
    }
}

// -- ask ------------------------------------------------------------------------------

pub struct Fork {
    pub session_id: String,
    pub path: String,
    pub cwd: String,
    pub records: usize,
    pub origin_session: String,
}

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
) -> std::result::Result<Fork, ForkError> {
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

#[derive(Debug)]
pub enum ForkError {
    Trace(TraceError),
    Internal(String),
}

fn write_fork(
    transcript: &str,
    raw: &str,
    records: &[Map],
    leaf: usize,
    target_cwd: &str,
    title: Option<&str>,
) -> std::result::Result<Fork, ForkError> {
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
        None => match claude::transcript_cwds(raw).into_iter().next() {
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
    let path = claude::transcript_path_for(target_cwd, &new_id, None);
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
