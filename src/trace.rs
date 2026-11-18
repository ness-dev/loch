//! Which conversation made which commit, and getting back into it — as in
//! less the hooks, which live in `hook.rs`.
//!
//! A commit is linked to the turn that made it by a git note on
//! `refs/notes/loch` naming the session and the tool call that ran
//! `git commit`. It holds no transcript text.

use crate::claude::{self, display_summary, home_dir, projects_dir, read_session_meta, AGENT};
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

pub const NOTES_REF: &str = "refs/notes/loch";
pub const NOTE_VERSION: i64 = 1;
pub const UNCOMMITTED: &str = "0000000000000000000000000000000000000000";

/// How far a commit's committer time may sit outside the tool call that made
/// it: one second for git's truncation, the rest for a slow hook process.
const CLOCK_SLACK: f64 = 2.0;

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

pub type Result<T> = std::result::Result<T, TraceError>;

/// trace.git: stdout, or `git-failed` with git's stderr.
pub fn git(cwd: &str, args: &[&str]) -> Result<String> {
    gitio::git(cwd, args, None).map_err(|e| TraceError::new("git-failed", e.0))
}

fn git_stdin(cwd: &str, args: &[&str], stdin: &str) -> Result<String> {
    gitio::git(cwd, args, Some(stdin)).map_err(|e| TraceError::new("git-failed", e.0))
}

pub fn git_unchecked(cwd: &str, args: &[&str]) -> String {
    gitio::git_unchecked(cwd, args).unwrap_or_default()
}

pub fn repo_root(cwd: &str) -> Result<String> {
    git(cwd, &["rev-parse", "--show-toplevel"])
        .map(|out| out.trim().to_string())
        .map_err(|_| {
            TraceError::new(
                "not-a-repository",
                format!("{} is not in a git repository", cwd),
            )
        })
}

// -- notes --------------------------------------------------------------------

pub fn make_note(session_id: &str, call: &str, captured: &str) -> Value {
    Value::Object(object! {
        "loch" => NOTE_VERSION,
        "agent" => AGENT,
        "session" => session_id,
        "call" => call,
        "captured" => captured,
    })
}

pub fn write_note(cwd: &str, commit: &str, note: &Value) -> Result<()> {
    let body = json::dumps(note, json::COMPACT_SORTED);
    git_stdin(
        cwd,
        &["notes", "--ref", NOTES_REF, "add", "-f", "-F", "-", commit],
        &body,
    )?;
    Ok(())
}

/// `x == 1` as Python means it: 1, 1.0 and True all qualify.
fn is_one(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Int(d)) => d == "1",
        Some(Value::Float(f)) => *f == 1.0,
        Some(Value::Bool(b)) => *b,
        _ => false,
    }
}

pub fn read_note(cwd: &str, commit: &str) -> Option<Map> {
    let text = git_unchecked(cwd, &["notes", "--ref", NOTES_REF, "show", commit]);
    match json::loads(&text) {
        Ok(Value::Object(note)) if is_one(note.get("loch")) => Some(note),
        _ => None,
    }
}

pub fn noted_commits(cwd: &str) -> HashSet<String> {
    git_unchecked(cwd, &["notes", "--ref", NOTES_REF, "list"])
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = py::split(line);
            (parts.len() == 2).then(|| parts[1].to_string())
        })
        .collect()
}

/// `str(value)` for a value read out of JSON.
pub fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(Value::Int(d)) => d.clone(),
        Some(Value::Float(f)) => {
            let repr = json::float_repr(*f);
            match repr.as_str() {
                "NaN" => "nan".into(),
                "Infinity" => "inf".into(),
                "-Infinity" => "-inf".into(),
                _ => repr,
            }
        }
        Some(Value::Str(s)) => s.clone(),
        Some(other) => json::dumps(other, json::DEFAULT),
    }
}

// -- install ------------------------------------------------------------------

const HOOK_EVENTS: &[(&str, Option<&str>, &str)] = &[
    ("PreToolUse", Some("Bash"), "pre-tool"),
    ("PostToolUse", Some("Bash"), "post-tool"),
    ("Stop", None, "stop"),
];

#[derive(Debug)]
pub enum InstallError {
    /// What Python's exception would have said; `internal` in the envelope.
    Internal(String),
}

/// Adds loch's hooks to a Claude Code settings file, keeping whatever else is
/// there. Returns the events it added; re-running adds nothing.
pub fn install_hooks(
    settings_path: &str,
    command: &str,
) -> std::result::Result<Vec<String>, InstallError> {
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
    let mut added = Vec::new();
    for (event, matcher, name) in HOOK_EVENTS {
        // A checkout whose machine lacks loch gets a silent no-op rather than
        // an error on every Bash call, since the settings file is committed.
        let line = format!("{} hook {} 2>/dev/null || true", command, name);
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
        let mut group = object! {"hooks" => vec![Value::Object(object! {"type" => "command", "command" => line.as_str()})]};
        if let Some(matcher) = matcher {
            group.insert("matcher".into(), (*matcher).into());
        }
        groups.push(Value::Object(group));
        added.push(event.to_string());
    }
    let parent = sys::dirname(settings_path);
    fs::create_dir_all(&parent).map_err(|e| InstallError::Internal(sys::os_error(&e, &parent)))?;
    let text = format!("{}\n", json::dumps(&settings, json::INDENTED));
    fs::write(settings_path, text)
        .map_err(|e| InstallError::Internal(sys::os_error(&e, settings_path)))?;
    Ok(added)
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

// -- blame ------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Hunk {
    pub start: usize,
    pub end: usize,
    pub commit: String,
    pub summary: String,
    pub lines: Vec<String>,
}

pub fn blame_hunks(cwd: &str, path: &str) -> Result<Vec<Hunk>> {
    let porcelain = git(cwd, &["blame", "--porcelain", "--", path])?;
    let mut summaries: HashMap<String, String> = HashMap::new();
    let mut lines: Vec<(usize, String, String)> = Vec::new();
    let mut current = String::new();
    let mut number = 0usize;
    for line in porcelain.split('\n') {
        if let Some(text) = line.strip_prefix('\t') {
            lines.push((number, current.clone(), text.to_string()));
            continue;
        }
        let parts: Vec<&str> = line.split(' ').collect();
        if parts.len() >= 3 && parts[0].chars().count() == 40 && py::is_digits(parts[1]) {
            current = parts[0].to_string();
            number = parts[2].parse().unwrap_or(0);
        } else if let Some(summary) = line.strip_prefix("summary ") {
            summaries.insert(current.clone(), summary.to_string());
        }
    }
    lines.sort();
    let mut hunks: Vec<Hunk> = Vec::new();
    for (number, commit, text) in lines {
        match hunks.last_mut() {
            Some(last) if last.commit == commit && last.end + 1 == number => {
                last.end = number;
                last.lines.push(text);
            }
            _ => hunks.push(Hunk {
                start: number,
                end: number,
                summary: summaries.get(&commit).cloned().unwrap_or_default(),
                commit,
                lines: vec![text],
            }),
        }
    }
    Ok(hunks)
}

pub struct Change {
    pub commit: String,
    pub time: i64,
    pub summary: String,
}

/// Every commit that touched `path` (or just `lines` of it), newest first,
/// by git's own tracking: `-L` for a range, `--follow` for a whole file.
pub fn history(cwd: &str, path: &str, lines: Option<(usize, usize)>) -> Result<Vec<Change>> {
    let range = lines.map(|(a, b)| format!("{},{}:{}", a, b, path));
    let mut args = vec!["log", "-s", "--format=%x1e%H%x1f%ct%x1f%s"];
    match &range {
        Some(range) => args.extend(["-L", range.as_str()]),
        None => args.extend(["--follow", "--", path]),
    }
    let output = git(cwd, &args)?;
    let mut changes = Vec::new();
    for record in output.split('\x1e').skip(1) {
        let mut parts = record.trim_matches('\n').splitn(3, '\x1f');
        let commit = parts.next().unwrap_or("").to_string();
        let time = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let summary = parts
            .next()
            .unwrap_or("")
            .split('\n')
            .next()
            .unwrap_or("")
            .to_string();
        changes.push(Change {
            commit,
            time,
            summary,
        });
    }
    Ok(changes)
}

#[derive(Clone)]
pub struct Link {
    pub session_id: String,
    pub call: String,
    pub captured: String,
    pub transcript: Option<String>,
    pub turn: Option<String>,
    pub conversation: Option<String>,
    pub title: Option<String>,
    pub prompt: Option<String>,
}

impl Link {
    pub fn payload(&self) -> Map {
        object! {
            "session" => self.session_id.as_str(),
            "call" => self.call.as_str(),
            "captured" => self.captured.as_str(),
            "available" => self.transcript.is_some(),
            "turn" => Value::opt_str(self.turn.as_deref()),
            "conversation" => Value::opt_str(self.conversation.as_deref()),
            "title" => Value::opt_str(self.title.as_deref()),
            "prompt" => Value::opt_str(self.prompt.as_deref()),
        }
    }
}

fn prompt_text(turn: &Turn) -> Option<String> {
    for block in turn.blocks.iter().flatten() {
        if block.get("type").is_some_and(|t| t.is_str("text")) {
            if let Some(Value::Str(text)) = block.get("text") {
                if !py::strip(text).is_empty() && !text.starts_with('<') {
                    return Some(text.clone());
                }
            }
        }
    }
    None
}

/// Resolves commits to turns, ingesting each session once however many
/// commits and lines point into it.
pub struct Resolver<'a> {
    cwd: String,
    store: Option<&'a dyn Store>,
    sessions: HashMap<String, (Option<String>, Vec<Turn>, Map)>,
}

impl<'a> Resolver<'a> {
    pub fn new(cwd: &str, store: Option<&'a dyn Store>) -> Self {
        Resolver {
            cwd: cwd.into(),
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
                    let derived =
                        ingest::ingest(path, &origin, None).map_err(|e| format!("{:?}", e))?;
                    turns = derived.turns;
                    header = derived.header;
                }
            }
            self.sessions
                .insert(session_id.into(), (path, turns, header));
        }
        Ok(&self.sessions[session_id])
    }

    pub fn resolve(&mut self, commit: &str) -> std::result::Result<Option<Link>, String> {
        let Some(note) = read_note(&self.cwd, commit) else {
            return Ok(None);
        };
        let session_id = py_str(note.get("session"));
        let call = py_str(note.get("call"));
        let (path, turns, header) = self.session(&session_id)?;
        let by_id: HashMap<&str, &Turn> = turns.iter().map(|t| (t.id.as_str(), t)).collect();
        let turn = turns.iter().find(|candidate| {
            candidate.blocks.iter().flatten().any(|block| {
                block.get("type").is_some_and(|t| t.is_str("tool-call"))
                    && block.get("call").is_some_and(|c| c.is_str(&call))
            })
        });
        let mut prompt = None;
        let mut conversation = None;
        let mut walker = turn;
        while let Some(current) = walker {
            if prompt.is_none()
                && current.kind() == "message"
                && current.author_kind() == Some("human")
            {
                prompt = prompt_text(current);
            }
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
        Ok(Some(Link {
            session_id: session_id.clone(),
            call,
            captured: py_str(note.get("captured")),
            transcript: path.clone(),
            turn: turn.map(|t| t.id.clone()),
            conversation,
            title: title_of("title").or_else(|| title_of("aiTitle")),
            prompt: display_summary(prompt.as_deref()),
        }))
    }
}

// -- backfill ---------------------------------------------------------------------

#[derive(Clone)]
pub struct Call {
    pub session_id: String,
    pub call: String,
    pub command: String,
    pub started: f64,
    pub finished: f64,
}

fn seconds(stamp: Option<&Value>) -> Option<f64> {
    sys::iso_seconds(stamp?.as_str()?)
}

/// `(block.get("input") or {}).get("command", "")`, stringified.
pub fn input_command(block: &Map) -> Option<String> {
    match block.get("input") {
        None => Some(String::new()),
        Some(v) if !v.truthy() => Some(String::new()),
        Some(Value::Object(input)) => Some(match input.get("command") {
            None => String::new(),
            other => py_str(other),
        }),
        Some(_) => None,
    }
}

/// Every Bash call in a transcript that could have made a commit, with the
/// window between the call being issued and its result coming back.
pub fn commit_calls(path: &str) -> Vec<Call> {
    let file = sys::basename(path);
    let session_id = file[..file.len().saturating_sub(".jsonl".len())].to_string();
    let mut issued: HashMap<String, (String, f64)> = HashMap::new();
    let mut calls = Vec::new();
    for record in read_records(path) {
        let stamp = seconds(record.get("timestamp"));
        for block in content(&record) {
            let kind = block.get("type").and_then(Value::as_str);
            if kind == Some("tool_use") && block.get("name").is_some_and(|n| n.is_str("Bash")) {
                let Some(command) = input_command(block) else {
                    continue;
                };
                if let (true, Some(stamp)) = (command.contains("commit"), stamp) {
                    issued.insert(py_str(block.get("id")), (command, stamp));
                }
            } else if kind == Some("tool_result") {
                let call = py_str(block.get("tool_use_id"));
                if let Some(stamp) = stamp {
                    if let Some((command, started)) = issued.remove(&call) {
                        calls.push(Call {
                            session_id: session_id.clone(),
                            call,
                            command,
                            started,
                            finished: stamp,
                        });
                    }
                }
            }
        }
    }
    calls
}

/// `str.splitlines()`, which splits on more than newlines.
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let breaks = matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if breaks {
            lines.push(&text[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(_, '\n')) = chars.peek() {
                    chars.next();
                    end += 1;
                }
            }
            start = end;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

pub type Progress<'p> = &'p mut dyn FnMut(usize, usize, u64, u64);

/// Links commits made before the hooks existed, accepting only an exact
/// match: one Bash call whose command contains the commit's subject and whose
/// run brackets the commit's author or committer time.
pub fn backfill(
    cwd: &str,
    revision: &str,
    mut progress: Option<Progress>,
) -> Result<Vec<(String, Call)>> {
    let listing = git(cwd, &["log", "--format=%H%x00%at%x00%ct%x00%s", revision])?;
    let already = noted_commits(cwd);
    let mut pending: Vec<(String, [i64; 2], String)> = Vec::new();
    for line in splitlines(&listing) {
        let parts: Vec<&str> = line.splitn(4, '\x00').collect();
        if parts.len() < 4 {
            return Err(TraceError::new("git-failed", "unexpected git log output"));
        }
        let (sha, subject) = (parts[0], parts[3]);
        if !already.contains(sha) && !subject.is_empty() {
            let stamp = |s: &str| s.trim().parse::<i64>().unwrap_or(0);
            pending.push((
                sha.into(),
                [stamp(parts[1]), stamp(parts[2])],
                subject.into(),
            ));
        }
    }
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let subjects: HashSet<&str> = pending.iter().map(|(_, _, s)| s.as_str()).collect();

    let mut transcripts: Vec<(String, u64)> = Vec::new();
    let root = projects_dir(None);
    let mut projects = list_dir(&root);
    projects.sort_by(|a, b| py::cmp(a, b));
    for project in projects {
        let directory = sys::join(&root, &project);
        if !Path::new(&directory).is_dir() {
            continue;
        }
        let mut names = list_dir(&directory);
        names.sort_by(|a, b| py::cmp(a, b));
        for name in names {
            if name.ends_with(".jsonl") {
                let path = sys::join(&directory, &name);
                if let Ok(meta) = fs::metadata(&path) {
                    transcripts.push((path, meta.len()));
                }
            }
        }
    }
    let total: u64 = transcripts.iter().map(|(_, size)| size).sum();
    let mut calls: Vec<Call> = Vec::new();
    let mut read = 0u64;
    let count = transcripts.len();
    for (done, (path, size)) in transcripts.iter().enumerate() {
        read += size;
        let raw = fs::read(path)
            .map(|b| py::decode_replace(&b))
            .unwrap_or_default();
        if raw.contains("git commit") && subjects.iter().any(|s| raw.contains(s)) {
            calls.extend(commit_calls(path));
        }
        if let Some(progress) = progress.as_mut() {
            progress(done + 1, count, read, total);
        }
    }

    let mut linked = Vec::new();
    for (sha, stamps, subject) in pending {
        let matches: Vec<&Call> = calls
            .iter()
            .filter(|call| {
                call.command.contains(&subject)
                    && stamps.iter().any(|&stamp| {
                        let stamp = stamp as f64;
                        call.started - CLOCK_SLACK <= stamp && stamp <= call.finished + CLOCK_SLACK
                    })
            })
            .collect();
        let distinct: HashSet<(&str, &str)> = matches
            .iter()
            .map(|c| (c.session_id.as_str(), c.call.as_str()))
            .collect();
        if distinct.len() == 1 {
            write_note(
                cwd,
                &sha,
                &make_note(&matches[0].session_id, &matches[0].call, "backfill"),
            )?;
            linked.push((sha, matches[0].clone()));
        }
    }
    Ok(linked)
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
/// With no `call` it holds the conversation to its end; with one, it is cut
/// after the whole round of tool results that call belongs to.
pub fn fork_session(
    transcript: &str,
    target_cwd: &str,
    call: Option<&str>,
    title: Option<&str>,
) -> std::result::Result<Fork, ForkError> {
    let bytes =
        fs::read(transcript).map_err(|e| ForkError::Internal(sys::os_error(&e, transcript)))?;
    let raw = py::decode_surrogateescape(&bytes);
    let records: Vec<Map> = records_of(&raw)
        .into_iter()
        .filter(|r| matches!(r.get("uuid"), Some(Value::Str(_))))
        .collect();
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if let Some(parent) = parent(record) {
            children.entry(parent).or_default().push(index);
        }
    }
    let Some(call) = call else {
        if records.is_empty() {
            return Err(ForkError::Trace(TraceError::new(
                "call-not-found",
                "the transcript holds no records",
            )));
        }
        let leaf = records.len() - 1;
        return write_fork(transcript, &raw, &records, leaf, target_cwd, title);
    };
    let is_result_for = |record: &Map| {
        content(record).iter().any(|block| {
            block.get("type").is_some_and(|t| t.is_str("tool_result"))
                && block.get("tool_use_id").is_some_and(|id| id.is_str(call))
        })
    };
    let Some(mut leaf) = records.iter().position(is_result_for) else {
        return Err(ForkError::Trace(TraceError::new(
            "call-not-found",
            format!("no result for tool call {}", call),
        )));
    };
    loop {
        let following: Vec<usize> = children
            .get(uuid(&records[leaf]))
            .into_iter()
            .flatten()
            .copied()
            .filter(|&child| {
                records[child].get("type").is_some_and(|t| t.is_str("user"))
                    && content(&records[child])
                        .iter()
                        .any(|b| b.get("type").is_some_and(|t| t.is_str("tool_result")))
            })
            .collect();
        match following.first() {
            Some(&next) => leaf = next,
            None => break,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(
            splitlines("a\nb\r\nc\rd\u{1c}e\n"),
            vec!["a", "b", "c", "d", "e"]
        );
        assert_eq!(splitlines(""), Vec::<&str>::new());
    }
}
