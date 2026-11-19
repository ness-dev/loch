//! The Codex CLI adapter.
//!
//! Codex files a session by date, not by directory:
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, one JSON
//! record per line, each `{"timestamp", "type", "payload"}`. The first record
//! is `session_meta` (id, cwd, cli_version, git); every turn writes a
//! `turn_context` carrying that turn's cwd and `turn_id`. `codex resume <id>`
//! needs the rollout file and nothing else: the sqlite files beside it are
//! projections of the rollouts, rebuilt when missing (see CLAUDE.md).
//!
//! Three things differ from Claude Code and shape this file:
//! - **The live cwd is the last `turn_context`'s.** A session resumed with
//!   `-C` elsewhere keeps its `session_meta` naming where it began, and no
//!   directory name says where it is now. Codex stores `-C` as given, not
//!   symlink-resolved, so cwds are compared resolved.
//! - **The branch is recorded once,** in `session_meta`, for the directory
//!   the session began in. A session that has moved has no recorded branch
//!   for where it is, and gets none rather than its old one.
//! - **A turn has an id.** Hooks carry `turn_id` and records name it, so a
//!   turn's start and end in the transcript are that id, not a record.

use crate::agent::{Agent, HookEvent, SessionMeta, TurnInfo};
use crate::claude::{display_summary, home_dir, resolve_cwd};
use crate::json::{self, Map, Value};
use crate::py;
use crate::rewrite::{rewrite_transcript, RewriteSpec};
use crate::store::{Store, StoreError};
use crate::sys;
use crate::trace::{self, Fork, ForkError, TraceError};
use std::fs;
use std::path::Path;

pub const AGENT: &str = "codex";

/// `CODEX_HOME` when set, else `~/.codex` — the rule Codex follows, and the
/// only place consulted, for the reason `claude::config_dir` gives.
pub fn codex_home() -> String {
    match std::env::var("CODEX_HOME") {
        Ok(configured) if !configured.is_empty() => configured,
        _ => sys::join(&home_dir(), ".codex"),
    }
}

pub fn sessions_dir() -> String {
    sys::join(&codex_home(), "sessions")
}

/// Every rollout under `sessions/`, in path order.
pub fn rollouts() -> Vec<String> {
    let mut found = Vec::new();
    walk(&sessions_dir(), &mut found);
    found.sort();
    found
}

fn walk(dir: &str, into: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = sys::join(dir, &name);
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(&path, into),
            Ok(kind) if kind.is_file() => {
                if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                    into.push(path);
                }
            }
            _ => {}
        }
    }
}

/// The id a rollout's file name ends with: `rollout-<time>-<uuid>.jsonl`.
fn file_session_id(path: &str) -> Option<String> {
    let stem = sys::basename(path);
    let stem = stem.strip_suffix(".jsonl")?;
    (stem.len() >= 36).then(|| stem[stem.len() - 36..].to_string())
}

fn rollout_for(session_id: &str) -> Option<String> {
    rollouts()
        .into_iter()
        .find(|path| file_session_id(path).as_deref() == Some(session_id))
}

fn kind(record: &Map) -> &str {
    record.get("type").and_then(Value::as_str).unwrap_or("")
}

fn payload(record: &Map) -> Option<&Map> {
    record.get("payload").and_then(Value::as_object)
}

fn text(map: &Map, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Value::Str(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// The working directory a record says it is in: `session_meta` and
/// `turn_context` carry one at the top of their payload. `"cwd"` occurs inside
/// tool calls and `world_state` too, naming other things, and is not read.
fn record_cwd(record: &Map) -> Option<String> {
    match kind(record) {
        "session_meta" | "turn_context" => text(payload(record)?, "cwd"),
        _ => None,
    }
}

/// Every cwd the rollout's records name, with how many name it, in order of
/// first appearance.
pub fn transcript_cwds(raw: &str) -> Vec<(String, usize)> {
    let mut counts: indexmap::IndexMap<String, usize> = indexmap::IndexMap::new();
    for record in trace::records_of(raw) {
        if let Some(cwd) = record_cwd(&record) {
            *counts.entry(cwd).or_insert(0) += 1;
        }
    }
    counts.into_iter().collect()
}

/// What a person typed, from the event Codex records for it: `user_message`
/// in older rollouts, a completed `UserMessage` item in newer ones. The
/// `response_item` user messages are not read, since Codex injects its own
/// context (`<environment_context>`, AGENTS.md) as user messages too.
fn user_prompt(record: &Map) -> Option<String> {
    if kind(record) != "event_msg" {
        return None;
    }
    let event = payload(record)?;
    match event.get("type").and_then(Value::as_str)? {
        "user_message" => text(event, "message"),
        "item_completed" => {
            let item = event.get("item")?.as_object()?;
            if !item.get("type").is_some_and(|t| t.is_str("UserMessage")) {
                return None;
            }
            item.get("content")?
                .as_array()?
                .iter()
                .filter_map(Value::as_object)
                .find_map(|block| text(block, "text"))
        }
        _ => None,
    }
}

/// The turn a record names: events and contexts carry `turn_id`, response
/// items carry it in their metadata passthrough. Bookkeeping between turns —
/// the settings a `resume` writes before its first turn — names none.
fn named_turn(record: &Map) -> Option<String> {
    let body = payload(record)?;
    text(body, "turn_id").or_else(|| {
        body.get("internal_chat_message_metadata_passthrough")
            .and_then(Value::as_object)
            .and_then(|meta| text(meta, "turn_id"))
    })
}

/// Turn ids in order of first appearance.
fn turn_ids(records: &[Map]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for id in records.iter().filter_map(named_turn) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// The first and last records naming `turn`: everything between belongs to
/// it, and nothing after the last does.
fn span(records: &[Map], turn: &str) -> Option<(usize, usize)> {
    let first = records
        .iter()
        .position(|r| named_turn(r).as_deref() == Some(turn))?;
    let last = records
        .iter()
        .rposition(|r| named_turn(r).as_deref() == Some(turn))?;
    Some((first, last))
}

pub fn read_session_meta(path: &str) -> Option<SessionMeta> {
    let meta = fs::metadata(path).ok()?;
    let raw = py::decode_replace(&fs::read(path).ok()?);
    let mut session_id = None;
    let mut first_cwd = None;
    let mut last_cwd = None;
    let mut branch = None;
    let mut version = None;
    let mut summary = None;
    let mut ordered: Vec<String> = Vec::new();
    for record in trace::records_of(&raw) {
        if let Some(cwd) = record_cwd(&record) {
            if !ordered.contains(&cwd) {
                ordered.push(cwd.clone());
            }
            if kind(&record) == "session_meta" && first_cwd.is_none() {
                let session = payload(&record)?;
                session_id = text(session, "id");
                version = text(session, "cli_version");
                branch = session
                    .get("git")
                    .and_then(Value::as_object)
                    .and_then(|git| text(git, "branch"));
                first_cwd = Some(cwd.clone());
            }
            last_cwd = Some(cwd);
        }
        if summary.is_none() {
            summary = user_prompt(&record);
        }
    }
    let cwd = last_cwd?;
    // The branch was recorded for where the session began; after a move it
    // says nothing about where the session is.
    let moved = first_cwd.as_deref().map(resolve_cwd) != Some(resolve_cwd(&cwd));
    Some(SessionMeta {
        agent: AGENT,
        session_id: session_id
            .or_else(|| file_session_id(path))
            .unwrap_or_default(),
        path: path.into(),
        git_branch: if moved { None } else { branch },
        version,
        bytes: meta.len(),
        modified_at: sys::iso_timestamp(sys::mtime(&meta)),
        summary: display_summary(summary.as_deref()),
        other_cwds: ordered.iter().filter(|o| **o != cwd).cloned().collect(),
        cwd,
    })
}

/// Sessions whose live cwd is `cwd` (already resolved). There is no
/// directory to look in, so every rollout is read: the same whole-file cost
/// `claude::list_sessions` pays, for the same reason.
pub fn list_sessions(
    cwd: &str,
    branch: Option<&str>,
    session_id: Option<&str>,
) -> Vec<SessionMeta> {
    let mut metas = Vec::new();
    for path in rollouts() {
        if let Some(id) = session_id.filter(|s| !s.is_empty()) {
            if file_session_id(&path).as_deref() != Some(id) {
                continue;
            }
        }
        let Some(meta) = read_session_meta(&path) else {
            continue;
        };
        if resolve_cwd(&meta.cwd) != cwd {
            continue;
        }
        if let Some(branch) = branch.filter(|b| !b.is_empty()) {
            if meta.git_branch.as_deref() != Some(branch) {
                continue;
            }
        }
        metas.push(meta);
    }
    metas.sort_by(|a, b| py::cmp(&b.modified_at, &a.modified_at));
    metas
}

/// The turn the transcript ends with — or, for a prompt hook closing the turn
/// before it, the last turn that isn't the new prompt's own.
pub fn current_turn(transcript: &str, event: &Map, for_next: bool) -> TurnInfo {
    let records = trace::read_records(transcript);
    let turns = turn_ids(&records);
    let own = text(event, "turn_id");
    let wanted = if for_next {
        turns
            .iter()
            .rev()
            .find(|id| Some(*id) != own.as_ref())
            .cloned()
    } else {
        own.filter(|id| turns.contains(id))
            .or_else(|| turns.last().cloned())
    };
    let Some((first, last)) = wanted.as_deref().and_then(|w| span(&records, w)) else {
        return TurnInfo::default();
    };
    let mut info = TurnInfo {
        prompt_uuid: wanted.clone(),
        end_uuid: wanted,
        ..TurnInfo::default()
    };
    for record in &records[first..=last] {
        let stamp = text(record, "timestamp");
        if info.started.is_none() {
            info.started = stamp.clone();
        }
        if info.prompt.is_none() {
            info.prompt = display_summary(user_prompt(record).as_deref());
        }
        info.ended = stamp;
    }
    info
}

/// `sessions/YYYY/MM/DD/rollout-YYYY-MM-DDTHH-MM-SS-<id>.jsonl`, from an ISO
/// timestamp. Codex names its own files in local time; the name is a label,
/// and `resume` finds a session by the id it ends with.
fn rollout_path(stamp: &str, session_id: &str) -> String {
    // The prefix is checked as ASCII before anything slices it: the stamp
    // comes from a bundle, and a cut inside a character would panic.
    let valid = stamp.get(..19).is_some_and(|prefix| {
        prefix.as_bytes()[4] == b'-'
            && prefix.as_bytes()[10] == b'T'
            && prefix
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'T' | b':'))
    });
    let stamp = if valid {
        stamp.to_string()
    } else {
        sys::iso_timestamp(sys::now())
    };
    let day = sys::join(
        &sys::join(&sys::join(&sessions_dir(), &stamp[..4]), &stamp[5..7]),
        &stamp[8..10],
    );
    sys::join(
        &day,
        &format!(
            "rollout-{}-{}.jsonl",
            stamp[..19].replace(':', "-"),
            session_id
        ),
    )
}

fn started_at(raw: &str) -> Option<String> {
    trace::records_of(raw)
        .iter()
        .find(|r| kind(r) == "session_meta")
        .and_then(|r| payload(r).and_then(|p| text(p, "timestamp")))
}

pub fn find_transcript(
    session_id: &str,
    store: Option<&dyn Store>,
) -> Result<Option<String>, StoreError> {
    if let Some(path) = rollout_for(session_id) {
        return Ok(Some(path));
    }
    let Some(store) = store else {
        return Ok(None);
    };
    let prefix = trace::transcripts_prefix(AGENT);
    let Ok(keys) = store.list(&prefix) else {
        return Ok(None);
    };
    let suffix = format!("-{}.jsonl", session_id);
    let Some(key) = keys.into_iter().find(|k| k.ends_with(&suffix)) else {
        return Ok(None);
    };
    let data = match store.get(&key) {
        Ok(data) => data,
        Err(StoreError::Missing(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let cache = sys::join(&trace::cache_dir(), AGENT);
    let cached = sys::join(&cache, &sys::basename(&key));
    let _ = fs::create_dir_all(&cache);
    let _ = fs::write(&cached, data);
    Ok(Some(cached))
}

/// A new Codex session holding the conversation to its end, or with `cut` to
/// the end of that turn: a prefix of the rollout's lines, kept byte for byte,
/// under a new id and rewritten into `target_cwd`. `codex fork` would copy a
/// whole session, and only one Codex can already find.
pub fn fork_session(
    transcript: &str,
    target_cwd: &str,
    cut: Option<&str>,
) -> Result<Fork, ForkError> {
    let bytes =
        fs::read(transcript).map_err(|e| ForkError::Internal(sys::os_error(&e, transcript)))?;
    let raw = py::decode_surrogateescape(&bytes);
    let lines: Vec<&str> = raw
        .split('\n')
        .filter(|line| !py::strip(line).is_empty())
        .collect();
    let records: Vec<Option<Map>> = lines
        .iter()
        .map(|line| match json::loads(line) {
            Ok(Value::Object(record)) => Some(record),
            _ => None,
        })
        .collect();
    let parsed: Vec<Map> = records.iter().flatten().cloned().collect();
    if parsed.is_empty() {
        return Err(ForkError::Trace(TraceError::new(
            "turn-not-found",
            "the transcript holds no records",
        )));
    }
    let keep = match cut {
        None => lines.len(),
        Some(cut) => {
            // Back from parsed records to lines, which unparseable ones offset.
            let indices: Vec<usize> = records
                .iter()
                .enumerate()
                .filter(|(_, r)| r.is_some())
                .map(|(i, _)| i)
                .collect();
            match span(&parsed, cut) {
                Some((_, last)) => indices[last] + 1,
                None => {
                    return Err(ForkError::Trace(TraceError::new(
                        "turn-not-found",
                        format!("the transcript has no turn {}", cut),
                    )))
                }
            }
        }
    };
    let meta = read_session_meta(transcript);
    let origin_cwd = match &meta {
        Some(meta) => meta.cwd.clone(),
        None => return Err(ForkError::Internal("the transcript names no cwd".into())),
    };
    let origin_session = meta
        .map(|m| m.session_id)
        .filter(|id| !id.is_empty())
        .or_else(|| file_session_id(transcript))
        .unwrap_or_default();
    let new_id = sys::uuid4();
    let home = home_dir();
    let spec = RewriteSpec {
        origin_cwd,
        origin_home: home.clone(),
        target_cwd: target_cwd.into(),
        target_home: home,
        origin_session_id: Some(origin_session.clone()),
        target_session_id: Some(new_id.clone()),
    };
    let text = rewrite_transcript(&format!("{}\n", lines[..keep].join("\n")), &spec);
    let path = rollout_path(&sys::iso_timestamp(sys::now()), &new_id);
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
        records: records[..keep].iter().flatten().count(),
        origin_session,
    })
}

pub struct Codex;

pub static CODEX: Codex = Codex;

/// Codex 0.166 has `SessionEnd`, so an open turn is closed on exit as Claude
/// Code's is. It clamps that hook's timeout to 3s.
const HOOK_EVENTS: &[HookEvent] = &[
    ("UserPromptSubmit", "prompt"),
    ("Stop", "stop"),
    ("SessionEnd", "end"),
];

impl Agent for Codex {
    fn id(&self) -> &'static str {
        AGENT
    }

    fn name(&self) -> &'static str {
        "Codex"
    }

    fn hook_events(&self) -> &'static [HookEvent] {
        HOOK_EVENTS
    }

    /// The project's `.codex/hooks.json`, or with `local` the user's
    /// `$CODEX_HOME/hooks.json`: Codex has no uncommitted project file, and a
    /// `--local` command is a machine-specific path. The user's file applies
    /// to every project, which attribution handles — it refuses `/` and home.
    fn hooks_file(&self, root: &str, local: bool) -> String {
        if local {
            sys::join(&codex_home(), "hooks.json")
        } else {
            sys::join(&sys::join(root, ".codex"), "hooks.json")
        }
    }

    fn install_note(&self) -> &'static str {
        "Codex runs new hooks only once trusted: open /hooks in Codex and trust loch's"
    }

    fn hooks_need_review(&self) -> bool {
        true
    }

    fn current_turn(&self, transcript: &str, event: &Map, for_next: bool) -> TurnInfo {
        current_turn(transcript, event, for_next)
    }

    /// The rollout's file name, which ends with the id.
    fn transcript_key(&self, path: &str, _session_id: &str) -> Option<String> {
        Some(format!(
            "{}{}",
            trace::transcripts_prefix(AGENT),
            sys::basename(path)
        ))
    }

    fn sessions_dir(&self) -> String {
        sessions_dir()
    }

    fn list_sessions(
        &self,
        cwd: &str,
        branch: Option<&str>,
        session_id: Option<&str>,
    ) -> Vec<SessionMeta> {
        list_sessions(cwd, branch, session_id)
    }

    fn read_session_meta(&self, path: &str) -> Option<SessionMeta> {
        read_session_meta(path)
    }

    fn transcript_cwds(&self, raw: &str) -> Vec<(String, usize)> {
        transcript_cwds(raw)
    }

    /// Filed under the day it began, as Codex would have.
    fn can_restore(&self) -> bool {
        true
    }

    fn restore_path(&self, raw: &str, _cwd: &str, session_id: &str) -> Option<String> {
        Some(rollout_path(
            &started_at(raw).unwrap_or_default(),
            session_id,
        ))
    }

    /// By id, wherever it is filed: two rollouts with one id would leave
    /// `resume` to pick either.
    fn existing_transcript(&self, _cwd: &str, session_id: &str) -> Option<String> {
        rollout_for(session_id).filter(|p| Path::new(p).exists())
    }

    fn resume_command(&self, session_id: &str) -> Option<Vec<String>> {
        Some(vec!["codex".into(), "resume".into(), session_id.into()])
    }

    fn find_transcript(
        &self,
        session_id: &str,
        store: Option<&dyn Store>,
    ) -> Result<Option<String>, StoreError> {
        find_transcript(session_id, store)
    }

    /// `session_index.jsonl`, where Codex appends a thread's name each time
    /// it changes, so the last line for an id is current. Read, never written.
    fn title(&self, session_id: &str) -> Option<String> {
        let raw = fs::read(sys::join(&codex_home(), "session_index.jsonl")).ok()?;
        trace::records_of(&py::decode_replace(&raw))
            .into_iter()
            .filter(|entry| entry.get("id").is_some_and(|id| id.is_str(session_id)))
            .filter_map(|entry| text(&entry, "thread_name"))
            .next_back()
    }

    /// Codex keeps titles in `session_index.jsonl`, which loch leaves to
    /// Codex, so a fork has none.
    fn fork(
        &self,
        transcript: &str,
        target_cwd: &str,
        cut: Option<&str>,
        _title: Option<&str>,
    ) -> Result<Fork, ForkError> {
        fork_session(transcript, target_cwd, cut)
    }
}
