//! `loch hook`, run by Claude Code around every Bash call and at every stop —
//! the half of the commit links that runs live.
//!
//! The pending-state file is the one piece of state a hook call leaves between
//! its two halves. A session can start a Bash call under one loch and finish it
//! under another while a machine upgrades, so keep its name and shape stable.

use crate::git::{self, GitError};
use crate::json::{self, Map, Value};
use crate::object;
use crate::store::{self, Store};
use crate::trace::{self, content, make_note, read_records, write_note};
use sha2::{Digest, Sha256};
use std::fs;

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

impl From<trace::TraceError> for Stop {
    fn from(_: trace::TraceError) -> Self {
        Stop
    }
}

fn event_cwd(event: &Map) -> Result<String, Stop> {
    // `str(event.get("cwd") or os.getcwd())`. A non-string cwd would be
    // str()-ed into something git can't enter, so it stops here instead.
    match event.get("cwd") {
        None => Ok(crate::sys::getcwd()),
        Some(v) if !v.truthy() => Ok(crate::sys::getcwd()),
        Some(Value::Str(text)) => Ok(text.clone()),
        Some(_) => Err(Stop),
    }
}

fn pending_dir(cwd: &str) -> Result<String, Stop> {
    let git_dir = git::absolute_git_dir(cwd)?;
    Ok(crate::sys::join(
        &crate::sys::join(&git_dir, "loch"),
        "pending",
    ))
}

/// `(event.get("tool_input") or {}).get("command", "")`, stringified. A
/// truthy non-object raises in Python, so it stops here.
fn tool_command(event: &Map) -> Result<String, Stop> {
    match event.get("tool_input") {
        None => Ok(String::new()),
        Some(v) if !v.truthy() => Ok(String::new()),
        Some(Value::Object(input)) => match input.get("command") {
            None => Ok(String::new()),
            Some(Value::Str(text)) => Ok(text.clone()),
            other => Ok(trace::py_str(other)),
        },
        Some(_) => Err(Stop),
    }
}

fn tool_use_id(event: &Map) -> Option<String> {
    match event.get("tool_use_id") {
        Some(Value::Str(call)) if !call.is_empty() => Some(call.clone()),
        _ => None,
    }
}

fn call_key(event: &Map) -> Result<String, Stop> {
    if let Some(call) = tool_use_id(event) {
        return Ok(call);
    }
    let digest = Sha256::digest(tool_command(event)?.as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{:02x}", b)).collect();
    Ok(format!("cmd-{}", hex))
}

/// Remembers HEAD before a Bash call, so the matching post hook can tell
/// exactly which commits the call made rather than guessing from history.
pub fn pre_tool(event: &Map) -> Result<(), Stop> {
    let cwd = event_cwd(event)?;
    let before = git::head(&cwd)?.unwrap_or_default();
    let directory = pending_dir(&cwd)?;
    fs::create_dir_all(&directory)?;
    let key = call_key(event)?;
    let state = Value::Object(object! {"head" => before, "at" => Value::Float(crate::sys::now())});
    fs::write(
        crate::sys::join(&directory, &key),
        json::dumps(&state, json::DEFAULT),
    )?;
    Ok(())
}

pub fn post_tool(event: &Map) -> Result<Vec<String>, Stop> {
    let cwd = event_cwd(event)?;
    let Some(Value::Str(session_id)) = event.get("session_id") else {
        return Ok(Vec::new());
    };
    let state_path = crate::sys::join(&pending_dir(&cwd)?, &call_key(event)?);
    let text = String::from_utf8(fs::read(&state_path)?).map_err(|_| Stop)?;
    let state = json::loads(&text).map_err(|_| Stop)?;
    fs::remove_file(&state_path)?;
    let before = match state.get("head") {
        Some(Value::Str(head)) => head.clone(),
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
                Some(Value::Str(path)) => call_from_transcript(path, &command)?,
                _ => None,
            };
            match found {
                Some(call) => call,
                None => return Ok(Vec::new()),
            }
        }
    };

    let since = match state.get("at") {
        Some(Value::Float(at)) => *at,
        Some(Value::Int(at)) => at.parse::<f64>().map_err(|_| Stop)?,
        _ => return Err(Stop),
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
    let listing = trace::git(cwd, &["log", "--format=%H %ct", &span])?;
    // Python's int() truncates toward zero, as `as` does.
    let floor = since.trunc() as i64 - 1;
    let mut made = Vec::new();
    for line in trace::splitlines(&listing) {
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
fn call_from_transcript(path: &str, command: &str) -> Result<Option<String>, Stop> {
    let mut found: Option<Value> = None;
    for record in read_records(path) {
        for block in content(&record) {
            if !(block.get("type").is_some_and(|t| t.is_str("tool_use"))
                && block.get("name").is_some_and(|n| n.is_str("Bash")))
            {
                continue;
            }
            // `(block.get("input") or {}).get("command")`, which raises on a
            // truthy non-object and so ends the hook.
            let input_command = match block.get("input") {
                None => None,
                Some(v) if !v.truthy() => None,
                Some(Value::Object(input)) => input.get("command"),
                Some(_) => return Err(Stop),
            };
            if input_command.is_some_and(|c| c.is_str(command)) {
                found = Some(block.get("id").cloned().unwrap_or(Value::Null));
            }
        }
    }
    Ok(match found {
        Some(Value::Str(id)) => Some(id),
        _ => None,
    })
}

/// Copies the live transcript into the store. Claude Code deletes transcripts
/// after `cleanupPeriodDays` (30 by default), which would leave every note
/// pointing at nothing; this is the copy that outlives it. The file is
/// append-only, so the latest snapshot covers every earlier one and one key
/// per session suffices.
pub fn snapshot_transcript(event: &Map, store: &dyn Store) -> Result<Option<String>, Stop> {
    let (Some(Value::Str(path)), Some(Value::Str(session_id))) =
        (event.get("transcript_path"), event.get("session_id"))
    else {
        return Ok(None);
    };
    let Ok(data) = fs::read(path) else {
        return Ok(None);
    };
    let project = crate::sys::basename(&crate::sys::dirname(path));
    let key = format!(
        "{}{}/{}.jsonl",
        trace::transcripts_prefix(),
        project,
        session_id
    );
    store.put(&key, &data).map_err(|_| Stop)?;
    Ok(Some(key))
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
    let Ok(parsed) = json::loads(text) else {
        return;
    };
    let event = match parsed {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let _ = match name {
        "pre-tool" => pre_tool(&event),
        "post-tool" => post_tool(&event).map(|_| ()),
        "stop" => match store::create_store(&store::resolve_store_uri(None)) {
            Ok(store) => snapshot_transcript(&event, store.as_ref()).map(|_| ()),
            Err(_) => Err(Stop),
        },
        _ => Ok(()),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_key_matches_python() {
        let event = json::loads(r#"{"tool_input": {"command": "git commit"}}"#).unwrap();
        // hashlib.sha256(b"git commit").hexdigest()[:32]
        assert_eq!(
            call_key(event.as_object().unwrap()).ok().unwrap(),
            "cmd-993c9eb42f6efa07b7501be8a0378748"
        );
    }
}
