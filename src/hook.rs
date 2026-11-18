//! `loch hook`, run by the agent's harness at the two moments a turn has: when
//! a prompt is submitted and when the agent stops.
//!
//! `prompt` snapshots the workspace. `stop` snapshots it again, records the
//! lines the turn added (see `attribution.rs`) in the session's edit record,
//! and copies the transcript into the store, since Claude Code deletes
//! transcripts after `cleanupPeriodDays` (30 by default) and every link would
//! otherwise end up pointing at nothing.
//!
//! A hook must never fail the turn it belongs to, so every error is swallowed
//! and nothing is written to stdout: a hook that breaks the agent gets
//! uninstalled.

use crate::attribution::{self, EditRecord, TurnRecord};
use crate::claude::{display_summary, AGENT};
use crate::json::{self, Map, Value};
use crate::object;
use crate::snapshot;
use crate::store::{self, Store};
use crate::sys;
use crate::trace::{self, content};
use std::fs;
use std::path::PathBuf;

/// Any failure: the hook swallows them all, so all that matters about one is
/// where it stopped.
pub struct Stop;

impl From<std::io::Error> for Stop {
    fn from(_: std::io::Error) -> Self {
        Stop
    }
}

impl From<snapshot::SnapshotError> for Stop {
    fn from(_: snapshot::SnapshotError) -> Self {
        Stop
    }
}

fn event_cwd(event: &Map) -> String {
    match event.get("cwd") {
        Some(Value::Str(text)) if !text.is_empty() => text.clone(),
        _ => sys::getcwd(),
    }
}

fn session_of(event: &Map) -> Result<String, Stop> {
    match event.get("session_id") {
        Some(Value::Str(id)) if !id.is_empty() && !id.contains('/') => Ok(id.clone()),
        _ => Err(Stop),
    }
}

/// Between a turn's two hooks: which workspace, and its snapshot at the start.
fn state_path(session: &str) -> PathBuf {
    snapshot::loch_home()
        .join("sessions")
        .join(format!("{}.json", session))
}

fn read_state(session: &str) -> Option<(String, String)> {
    let value = json::loads(&fs::read_to_string(state_path(session)).ok()?).ok()?;
    Some((
        value.get("root")?.as_str()?.to_string(),
        value.get("start")?.as_str()?.to_string(),
    ))
}

fn write_state(session: &str, root: &str, start: &str) -> Result<(), Stop> {
    let path = state_path(session);
    fs::create_dir_all(path.parent().expect("has a parent"))?;
    let state = Value::Object(object! {"root" => root, "start" => start});
    fs::write(path, json::dumps(&state, json::DEFAULT))?;
    Ok(())
}

/// A turn begins: remember what the workspace looked like.
pub fn prompt(event: &Map) -> Result<(), Stop> {
    let session = session_of(event)?;
    let root = snapshot::workspace_root(&event_cwd(event));
    if !snapshot::snapshottable(&root) {
        return Ok(());
    }
    let start = snapshot::snapshot(&root)?;
    write_state(&session, &root, &start)
}

/// What the transcript says about the turn that just ended: the prompt that
/// began it and the last record written.
#[derive(Default)]
pub struct TurnInfo {
    pub prompt_uuid: Option<String>,
    pub prompt: Option<String>,
    pub started: Option<String>,
    pub end_uuid: Option<String>,
    pub ended: Option<String>,
}

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

pub fn current_turn(transcript: &str) -> TurnInfo {
    let mut info = TurnInfo::default();
    for record in trace::read_records(transcript) {
        let Some(Value::Str(uuid)) = record.get("uuid") else {
            continue;
        };
        let stamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        if is_human_prompt(&record) {
            info.prompt_uuid = Some(uuid.clone());
            info.prompt = display_summary(prompt_text(&record).as_deref());
            info.started = stamp.clone();
        }
        info.end_uuid = Some(uuid.clone());
        info.ended = stamp;
    }
    info
}

/// A turn ends: record what it changed, and keep the transcript.
pub fn stop(event: &Map, store: Option<&dyn Store>) -> Result<(), Stop> {
    if let Some(store) = store {
        let _ = snapshot_transcript(event, store);
    }
    let session = session_of(event)?;
    let state = read_state(&session);
    let root = match &state {
        Some((root, _)) => root.clone(),
        None => snapshot::workspace_root(&event_cwd(event)),
    };
    if !snapshot::snapshottable(&root) {
        return Ok(());
    }
    // Without this turn's prompt snapshot — hooks installed mid-turn, a
    // harness with no prompt hook — the workspace's last snapshot is the best
    // baseline there is. Read before taking the next one, which replaces it.
    let start = state
        .map(|(_, start)| start)
        .or_else(|| snapshot::latest(&root));
    let end = snapshot::snapshot(&root)?;

    let info = match event.get("transcript_path") {
        Some(Value::Str(path)) => current_turn(path),
        _ => TurnInfo::default(),
    };
    let mut record = attribution::load_local(AGENT, &session).unwrap_or_else(|| EditRecord {
        agent: AGENT.into(),
        session: session.clone(),
        project: attribution::project_id(&root),
        root: root.clone(),
        turns: Vec::new(),
    });
    // Claude Code can stop more than once for one prompt; that is still one
    // turn, measured from where it began.
    let same_turn = info.prompt_uuid.is_some()
        && record.turns.last().map(|t| &t.prompt_uuid) == Some(&info.prompt_uuid);
    let start = if same_turn {
        record.turns.last().and_then(|t| t.start_snapshot.clone())
    } else {
        start
    };
    // With no baseline at all, everything in the workspace would look new;
    // the turn is recorded with nothing attributed, as the baseline for the
    // next.
    let files = match &start {
        Some(start) => attribution::diff(Some(start), &end)?,
        None => Vec::new(),
    };
    let turn = TurnRecord {
        prompt_uuid: info.prompt_uuid,
        end_uuid: info.end_uuid,
        started: info.started,
        ended: info.ended.unwrap_or_else(|| sys::iso_timestamp(sys::now())),
        prompt: info.prompt,
        start_snapshot: start,
        end_snapshot: end.clone(),
        files,
    };
    if same_turn {
        record.turns.pop();
    }
    record.turns.push(turn);
    attribution::save_local(&record)?;
    write_state(&session, &root, &end)?;
    if let Some(store) = store {
        let _ = attribution::publish(store, &record);
    }
    Ok(())
}

/// Copies the live transcript into the store. The file is append-only, so the
/// latest snapshot covers every earlier one and one key per session suffices.
pub fn snapshot_transcript(event: &Map, store: &dyn Store) -> Result<Option<String>, Stop> {
    let (Some(Value::Str(path)), Some(Value::Str(session_id))) =
        (event.get("transcript_path"), event.get("session_id"))
    else {
        return Ok(None);
    };
    let Ok(data) = fs::read(path) else {
        return Ok(None);
    };
    let project = sys::basename(&sys::dirname(path));
    let key = format!(
        "{}{}/{}.jsonl",
        trace::transcripts_prefix(),
        project,
        session_id
    );
    store.put(&key, &data).map_err(|_| Stop)?;
    Ok(Some(key))
}

/// Run by the harness with the event as JSON on stdin.
pub fn run(name: &str, stdin: &[u8]) {
    let Ok(text) = std::str::from_utf8(stdin) else {
        return;
    };
    let text = if text.is_empty() { "{}" } else { text };
    let Ok(Value::Object(event)) = json::loads(text) else {
        return;
    };
    let _ = match name {
        "prompt" => prompt(&event),
        "stop" => {
            let store = store::create_store(&store::resolve_store_uri(None)).ok();
            stop(&event, store.as_deref())
        }
        // The tool-call hooks the git-notes design used. A settings file can
        // still name them until `install` is re-run; they do nothing.
        _ => Ok(()),
    };
}
