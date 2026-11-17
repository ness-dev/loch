//! `loch hook`, run by Claude Code around every Bash call and at every stop.
//! A port of the hook half of `loch/trace.py`; the Python is the reference
//! and `test/test_equivalence.py` holds the two to the same side effects.
//!
//! The pending-state file is shared across implementations — a session can
//! start its Bash call under one and finish it under the other while a machine
//! switches over — so its name and JSON shape must stay exactly Python's.

use crate::git::{self, GitError};
use crate::output::{python_dumps, python_dumps_compact_sorted};
use crate::store::{self, Store};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const NOTES_REF: &str = "refs/notes/loch";
pub const NOTE_VERSION: i64 = 1;
pub const AGENT: &str = "claude-code";

/// Any failure: the hook swallows them all, so the only thing that matters
/// about one is where it stopped.
pub struct Stop;

impl From<GitError> for Stop {
    fn from(_: GitError) -> Self {
        Stop
    }
}

impl From<std::io::Error> for Stop {
    fn from(_: std::io::Error) -> Self {
        Stop
    }
}

fn event_cwd(event: &Map<String, Value>) -> Result<String, Stop> {
    // Python's `str(event.get("cwd") or os.getcwd())`. A non-string cwd would
    // be str()-ed into something git can't enter, so it stops here instead.
    match event.get("cwd") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => current_dir(),
        Some(Value::String(text)) if text.is_empty() => current_dir(),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(Stop),
    }
}

fn current_dir() -> Result<String, Stop> {
    Ok(std::env::current_dir()?.to_string_lossy().into_owned())
}

fn pending_dir(cwd: &str) -> Result<PathBuf, Stop> {
    let git_dir = git::absolute_git_dir(cwd)?;
    Ok(Path::new(&git_dir).join("loch").join("pending"))
}

/// `(event.get("tool_input") or {}).get("command", "")`, stringified. A truthy
/// non-object tool_input raises in Python, so it stops here.
fn tool_command(event: &Map<String, Value>) -> Result<String, Stop> {
    let input = match event.get("tool_input") {
        None | Some(Value::Null) => return Ok(String::new()),
        Some(Value::Object(map)) => map,
        Some(value) if !truthy(value) => return Ok(String::new()),
        Some(_) => return Err(Stop),
    };
    match input.get("command") {
        None => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        // str() of anything else; commands are strings in every real event.
        Some(_) => Err(Stop),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn tool_use_id(event: &Map<String, Value>) -> Option<String> {
    match event.get("tool_use_id") {
        Some(Value::String(call)) if !call.is_empty() => Some(call.clone()),
        _ => None,
    }
}

fn call_key(event: &Map<String, Value>) -> Result<String, Stop> {
    if let Some(call) = tool_use_id(event) {
        return Ok(call);
    }
    let digest = Sha256::digest(tool_command(event)?.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .concat();
    Ok(format!("cmd-{}", hex))
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Remembers HEAD before a Bash call, so the matching post hook can tell
/// exactly which commits the call made rather than guessing from history.
pub fn pre_tool(event: &Map<String, Value>) -> Result<(), Stop> {
    let cwd = event_cwd(event)?;
    let before = git::head(&cwd)?.unwrap_or_default();
    let directory = pending_dir(&cwd)?;
    fs::create_dir_all(&directory)?;
    let key = call_key(event)?;
    let state = json!({"head": before, "at": now()});
    fs::write(directory.join(key), python_dumps(&state))?;
    Ok(())
}

pub fn post_tool(event: &Map<String, Value>) -> Result<Vec<String>, Stop> {
    let cwd = event_cwd(event)?;
    let Some(Value::String(session_id)) = event.get("session_id") else {
        return Ok(Vec::new());
    };
    let state_path = pending_dir(&cwd)?.join(call_key(event)?);
    let state: Value = serde_json::from_slice(&fs::read(&state_path)?).map_err(|_| Stop)?;
    fs::remove_file(&state_path)?;
    let before = match state.get("head") {
        Some(Value::String(head)) => head.clone(),
        _ => return Err(Stop),
    };
    let since = match state.get("at") {
        Some(Value::Number(at)) => at.as_f64().ok_or(Stop)?,
        _ => return Err(Stop),
    };
    let after = match git::head(&cwd)? {
        Some(after) if after != before => after,
        _ => return Ok(Vec::new()),
    };

    let call = match tool_use_id(event) {
        Some(call) => call,
        None => {
            let command = tool_command(event)?;
            let found = match event.get("transcript_path") {
                Some(Value::String(path)) => call_from_transcript(path, &command),
                _ => None,
            };
            match found {
                Some(call) => call,
                None => return Ok(Vec::new()),
            }
        }
    };

    let made = commits_made(&cwd, &before, &after, since)?;
    for commit in &made {
        write_note(&cwd, commit, &make_note(session_id, &call, "hook"))?;
    }
    Ok(made)
}

/// Commits reachable from `after` and not from `before`, and committed after
/// the call began. The time bound is what keeps a `git checkout` or a
/// fast-forward pull — which move HEAD onto commits someone else made — from
/// being attributed to this call.
fn commits_made(cwd: &str, before: &str, after: &str, since: f64) -> Result<Vec<String>, Stop> {
    let span = if before.is_empty() {
        after.to_string()
    } else {
        format!("{}..{}", before, after)
    };
    let listing = git::git(cwd, &["log", "--format=%H %ct", &span], None)?;
    // Python's int() truncates toward zero, as `as` does.
    let floor = since.trunc() as i64 - 1;
    let mut made = Vec::new();
    for line in listing.lines() {
        let (sha, stamp) = line.split_once(' ').unwrap_or((line, ""));
        if stamp.is_empty() {
            continue;
        }
        let stamp: i64 = stamp.trim().parse().map_err(|_| Stop)?;
        if stamp >= floor {
            made.push(sha.to_string());
        }
    }
    Ok(made)
}

/// The tool call id, for a hook payload that didn't carry one: the last Bash
/// call in the transcript that ran exactly this command.
fn call_from_transcript(path: &str, command: &str) -> Option<String> {
    let raw = fs::read(path).ok()?;
    let mut found: Option<Value> = None;
    for line in raw.split(|&b| b == b'\n') {
        let Ok(Value::Object(record)) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(Value::Object(message)) = record.get("message") else {
            continue;
        };
        let Some(Value::Array(content)) = message.get("content") else {
            continue;
        };
        for block in content {
            let Value::Object(block) = block else {
                continue;
            };
            let input_command = match block.get("input") {
                Some(Value::Object(input)) => input.get("command"),
                _ => None,
            };
            if block.get("type") == Some(&Value::from("tool_use"))
                && block.get("name") == Some(&Value::from("Bash"))
                && input_command == Some(&Value::from(command))
            {
                found = Some(block.get("id").cloned().unwrap_or(Value::Null));
            }
        }
    }
    match found {
        Some(Value::String(id)) => Some(id),
        _ => None,
    }
}

pub fn make_note(session_id: &str, call: &str, captured: &str) -> Value {
    json!({
        "loch": NOTE_VERSION,
        "agent": AGENT,
        "session": session_id,
        "call": call,
        "captured": captured,
    })
}

fn write_note(cwd: &str, commit: &str, note: &Value) -> Result<(), Stop> {
    let body = python_dumps_compact_sorted(note);
    git::git(
        cwd,
        &["notes", "--ref", NOTES_REF, "add", "-f", "-F", "-", commit],
        Some(&body),
    )?;
    Ok(())
}

/// Copies the live transcript into the store. Claude Code deletes transcripts
/// after `cleanupPeriodDays` (30 by default), which would leave every note
/// pointing at nothing; this is the copy that outlives it. The file is
/// append-only, so the latest snapshot covers every earlier one and one key
/// per session suffices.
pub fn snapshot_transcript(
    event: &Map<String, Value>,
    store: &dyn Store,
) -> Result<Option<String>, Stop> {
    let (Some(Value::String(path)), Some(Value::String(session_id))) =
        (event.get("transcript_path"), event.get("session_id"))
    else {
        return Ok(None);
    };
    let Ok(data) = fs::read(path) else {
        return Ok(None);
    };
    let key = transcript_key(&project_of(path), session_id);
    store.put(&key, &data).map_err(|_| Stop)?;
    Ok(Some(key))
}

/// `os.path.basename(os.path.dirname(path))`.
fn project_of(path: &str) -> String {
    let dir = match path.rfind('/') {
        Some(slash) => path[..slash].trim_end_matches('/'),
        None => "",
    };
    match dir.rfind('/') {
        Some(slash) => dir[slash + 1..].to_string(),
        None => dir.to_string(),
    }
}

pub fn transcript_key(project: &str, session_id: &str) -> String {
    format!("transcripts/{}/{}/{}.jsonl", AGENT, project, session_id)
}

/// Run by Claude Code with the event as JSON on stdin. It must never fail the
/// tool call it wraps, so every error is swallowed and nothing is written: a
/// hook that breaks commits gets uninstalled.
pub fn run(name: &str, stdin: &[u8]) {
    // Strict, as Python's text-mode stdin is: undecodable input raises there.
    let Ok(text) = std::str::from_utf8(stdin) else {
        return;
    };
    let text = if text.is_empty() { "{}" } else { text };
    let Ok(parsed) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let event = match parsed {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let _ = match name {
        "pre-tool" => pre_tool(&event),
        "post-tool" => post_tool(&event).map(|_| ()),
        "stop" => store::create_store(&store::resolve_store_uri(None))
            .map_err(|_| Stop)
            .and_then(|store| snapshot_transcript(&event, store.as_ref()).map(|_| ())),
        _ => Ok(()),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_is_the_containing_directory_name() {
        assert_eq!(project_of("/h/.claude/projects/-a-b/s.jsonl"), "-a-b");
        assert_eq!(project_of("s.jsonl"), "");
        assert_eq!(project_of("/s.jsonl"), "");
    }

    #[test]
    fn command_key_matches_python() {
        let event: Map<String, Value> =
            serde_json::from_str(r#"{"tool_input": {"command": "git commit"}}"#).unwrap();
        // hashlib.sha256(b"git commit").hexdigest()[:32]
        assert_eq!(
            call_key(&event).ok().unwrap(),
            "cmd-993c9eb42f6efa07b7501be8a0378748"
        );
    }
}
