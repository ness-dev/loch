//! `loch hook`, run by the agent's harness at a turn's boundaries: `prompt`
//! when one is submitted, `stop` when the agent finishes, `end` when the
//! session ends.
//!
//! `prompt` snapshots the workspace; `stop` snapshots it again and records the
//! lines the turn added (see `attribution.rs`) in the session's edit record,
//! and copies the transcript into the store, since Claude Code deletes
//! transcripts after `cleanupPeriodDays` (30 by default).
//!
//! **Every turn gets closed by someone.** Claude Code runs no `Stop` when a
//! turn is cancelled, and a crash runs nothing at all. Left alone, the next
//! prompt's snapshot would take the unfinished turn's edits as its baseline
//! and they would belong to nobody. So a session's state says whether its turn
//! is still open, and whichever hook comes next — the next `prompt`, or `end`
//! on exit — closes it first, marked interrupted, ending where the workspace
//! stands then. A crash that is never resumed is the one case nothing closes;
//! its start snapshot stops being kept after `ABANDONED`.
//!
//! A hook must never fail the turn it belongs to, so every error is swallowed
//! and nothing is written to stdout: a hook that breaks the agent gets
//! uninstalled.

use crate::agent::{self, Agent, Scope, TurnInfo};
use crate::attribution::{self, EditRecord, TurnRecord};
use crate::json::{self, Map, Value};
use crate::object;
use crate::snapshot;
use crate::store::{self, Store};
use crate::sys;
use crate::trace;
use std::fs;
use std::path::PathBuf;

/// How long an open turn's start snapshot is kept for a session that never
/// came back to close it.
pub const ABANDONED: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

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

fn transcript_of(event: &Map) -> Option<String> {
    match event.get("transcript_path") {
        Some(Value::Str(path)) => Some(path.clone()),
        _ => None,
    }
}

// -- session state --------------------------------------------------------------

fn sessions_dir() -> PathBuf {
    snapshot::loch_home().join("sessions")
}

fn state_path(session: &str) -> PathBuf {
    sessions_dir().join(format!("{}.json", session))
}

/// What a session's hooks hand each other between processes.
struct State {
    root: String,
    /// A turn has begun and not been closed.
    open: bool,
    /// Where the open turn began, or where the last closed one did — kept so
    /// a second `Stop` for the same prompt can be measured from the start.
    turn_start: String,
}

fn read_state(session: &str) -> Option<State> {
    let value = json::loads(&fs::read_to_string(state_path(session)).ok()?).ok()?;
    Some(State {
        root: value.get("root")?.as_str()?.to_string(),
        open: value.get("open").is_some_and(Value::truthy),
        turn_start: value.get("turnStart")?.as_str()?.to_string(),
    })
}

fn write_state(session: &str, state: &State) -> Result<(), Stop> {
    let body = Value::Object(object! {
        "root" => state.root.as_str(),
        "open" => state.open,
        "turnStart" => state.turn_start.as_str(),
    });
    snapshot::write_atomically(
        &state_path(session),
        json::dumps(&body, json::DEFAULT).as_bytes(),
    )?;
    Ok(())
}

// -- closing a turn ------------------------------------------------------------------

/// Records the turn that began at `start` and ends now, in the session's edit
/// record, and returns the snapshot it ended at.
fn close_turn(
    agent: &dyn Agent,
    session: &str,
    root: &str,
    start: Option<&str>,
    info: TurnInfo,
    interrupted: bool,
    store: Option<&dyn Store>,
) -> Result<String, Stop> {
    let end = snapshot::snapshot(root)?;
    let mut record = attribution::load_local(agent.id(), session).unwrap_or_else(|| EditRecord {
        agent: agent.id().into(),
        session: session.into(),
        project: attribution::project_id(root),
        root: root.into(),
        turns: Vec::new(),
    });
    // With no baseline at all, everything in the workspace would look new; the
    // turn is recorded with nothing attributed, as the baseline for the next.
    let files = match start {
        Some(start) => attribution::diff(Some(start), &end)?,
        None => Vec::new(),
    };
    // Claude Code can stop more than once for one prompt; that is still one
    // turn, and the caller measured it from where it began.
    let same_turn = info.prompt_uuid.is_some()
        && record.turns.last().map(|t| &t.prompt_uuid) == Some(&info.prompt_uuid);
    if same_turn {
        record.turns.pop();
    }
    record.turns.push(TurnRecord {
        prompt_uuid: info.prompt_uuid,
        end_uuid: info.end_uuid,
        started: info.started,
        ended: info.ended.unwrap_or_else(|| sys::iso_timestamp(sys::now())),
        prompt: info.prompt,
        interrupted,
        backfilled: false,
        files,
    });
    attribution::save_local(&record)?;
    if let Some(store) = store {
        let _ = attribution::publish(store, &record);
    }
    Ok(end)
}

/// The session's last closed turn's prompt, to recognise a repeated `Stop`.
fn last_prompt(agent: &dyn Agent, session: &str) -> Option<String> {
    attribution::load_local(agent.id(), session)?
        .turns
        .last()?
        .prompt_uuid
        .clone()
}

/// A turn begins. A turn still open from before — cancelled, or a crash then
/// `--resume` — is closed first, so its edits are its own and not this
/// turn's baseline.
pub fn prompt(agent: &dyn Agent, event: &Map, store: Option<&dyn Store>) -> Result<(), Stop> {
    let session = session_of(event)?;
    let root = snapshot::workspace_root(&event_cwd(event));
    if let Some(state) = read_state(&session).filter(|s| s.open) {
        if snapshot::snapshottable(&state.root) {
            let info = transcript_of(event)
                .map(|t| agent.current_turn(&t, event, true))
                .unwrap_or_default();
            close_turn(
                agent,
                &session,
                &state.root,
                Some(&state.turn_start),
                info,
                true,
                store,
            )?;
        }
    }
    if !snapshot::snapshottable(&root) {
        return Ok(());
    }
    let start = snapshot::snapshot(&root)?;
    write_state(
        &session,
        &State {
            root,
            open: true,
            turn_start: start,
        },
    )
}

/// The agent finished: record what the turn changed, and keep the transcript.
pub fn stop(agent: &dyn Agent, event: &Map, store: Option<&dyn Store>) -> Result<(), Stop> {
    if let Some(store) = store {
        let _ = snapshot_transcript(agent, event, store);
    }
    let session = session_of(event)?;
    let state = read_state(&session);
    let root = match &state {
        Some(state) => state.root.clone(),
        None => snapshot::workspace_root(&event_cwd(event)),
    };
    if !snapshot::snapshottable(&root) {
        return Ok(());
    }
    let info = transcript_of(event)
        .map(|t| agent.current_turn(&t, event, false))
        .unwrap_or_default();
    let repeated = info.prompt_uuid.is_some() && last_prompt(agent, &session) == info.prompt_uuid;
    // The turn's own start if it has one — an open turn, or a repeated stop of
    // the last one. Otherwise no prompt hook ran (installed mid-session, or a
    // harness without one), and the workspace's last snapshot is the best
    // baseline there is; it is read before taking the next, which replaces it.
    let start = match &state {
        Some(state) if state.open || repeated => Some(state.turn_start.clone()),
        _ => snapshot::latest(&root),
    };
    close_turn(agent, &session, &root, start.as_deref(), info, false, store)?;
    if let Some(start) = start {
        write_state(
            &session,
            &State {
                root,
                open: false,
                turn_start: start,
            },
        )?;
    }
    Ok(())
}

/// The session is over. A turn still open was cut short; close it, and let
/// the session's state go, so nothing it held is kept any longer.
pub fn end(agent: &dyn Agent, event: &Map, store: Option<&dyn Store>) -> Result<(), Stop> {
    let session = session_of(event)?;
    if let Some(state) = read_state(&session).filter(|s| s.open) {
        if snapshot::snapshottable(&state.root) {
            let info = transcript_of(event)
                .map(|t| agent.current_turn(&t, event, false))
                .unwrap_or_default();
            close_turn(
                agent,
                &session,
                &state.root,
                Some(&state.turn_start),
                info,
                true,
                store,
            )?;
        }
    }
    if let Some(store) = store {
        let _ = snapshot_transcript(agent, event, store);
    }
    let _ = fs::remove_file(state_path(&session));
    Ok(())
}

// -- collection ------------------------------------------------------------------------

/// Every snapshot something still needs: each workspace's latest, and each
/// live session's turn start. A session's state that has sat untouched past
/// `ABANDONED` belongs to a crash nobody resumed; it is deleted, and what it
/// held goes with the next collection.
pub fn roots() -> Vec<String> {
    let mut roots = snapshot::workspace_roots();
    let now = std::time::SystemTime::now();
    let Ok(entries) = fs::read_dir(sessions_dir()) else {
        return roots;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let age = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok());
        if age.is_some_and(|age| age > ABANDONED) {
            let _ = fs::remove_file(&path);
            continue;
        }
        let session = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(state) = read_state(&session) {
            roots.push(state.turn_start);
        }
    }
    roots
}

pub fn collect() -> Result<snapshot::Collected, snapshot::SnapshotError> {
    snapshot::collect(&roots())
}

/// Copies the live transcript into the store. The file is append-only, so the
/// latest snapshot covers every earlier one and one key per session suffices.
pub fn snapshot_transcript(
    agent: &dyn Agent,
    event: &Map,
    store: &dyn Store,
) -> Result<Option<String>, Stop> {
    let (Some(Value::Str(path)), Some(Value::Str(session_id))) =
        (event.get("transcript_path"), event.get("session_id"))
    else {
        return Ok(None);
    };
    let Some(key) = agent.transcript_key(path, session_id) else {
        return Ok(None);
    };
    let Ok(data) = fs::read(path) else {
        return Ok(None);
    };
    store.put(&key, &data).map_err(|_| Stop)?;
    Ok(Some(key))
}

/// Whether a hook installed at `scope` records this event, decided before
/// anything is opened, since a user-wide hook runs in every directory the
/// agent does.
///
/// - **A user-wide hook records only in a git repository.** Anywhere else it
///   would snapshot whatever directory a session happened to start in
///   (`~/Downloads`, a scratch dir), which nobody asked for by installing
///   loch once. A project-level install still records a plain directory:
///   that was asked for.
/// - **A wider scope's hook defers to a narrower one's** in the same project,
///   when that file holds loch's hook for the same event and the agent will
///   run it. Every scope's hooks run, in parallel, so two would snapshot one
///   turn twice and race over its state.
///
/// A line naming no scope (local ones, and every line before scopes) records
/// as it always did.
pub fn applies(agent: &dyn Agent, scope: Option<Scope>, name: &str, event: &Map) -> bool {
    let Some(scope) = scope else {
        return true;
    };
    let root = snapshot::workspace_root(&event_cwd(event));
    // A home directory that is itself a repository (dotfiles) is the root of
    // every directory under it, and is refused before any store is opened.
    if scope == Scope::User && !(snapshot::has_git_marker(&root) && snapshot::snapshottable(&root))
    {
        return false;
    }
    let Some((event_name, _)) = agent.hook_events().iter().find(|(_, n)| *n == name) else {
        return true;
    };
    let own = agent.hooks_file(scope, &root);
    !scope.narrower().iter().any(|narrower| {
        let file = agent.hooks_file(*narrower, &root);
        file != own
            && trace::holds_hook(&file, event_name, name, agent)
            && agent.hooks_active(&file, &root, event_name)
    })
}

/// Run by the harness with the event as JSON on stdin. An agent this build
/// doesn't know does nothing, like every other failure here.
pub fn run(name: &str, agent_id: &str, scope: Option<Scope>, stdin: &[u8]) {
    let Some(agent) = agent::by_id(agent_id) else {
        return;
    };
    let Ok(text) = std::str::from_utf8(stdin) else {
        return;
    };
    let text = if text.is_empty() { "{}" } else { text };
    let Ok(Value::Object(event)) = json::loads(text) else {
        return;
    };
    if !applies(agent, scope, name, &event) {
        return;
    }
    let store = || store::create_store(&store::resolve_store_uri(None)).ok();
    let _ = match name {
        "prompt" => prompt(agent, &event, store().as_deref()),
        "stop" => stop(agent, &event, store().as_deref()),
        "end" => end(agent, &event, store().as_deref()),
        // The tool-call hooks the git-notes design used. A settings file can
        // still name them until `install` is re-run; they do nothing.
        _ => return,
    };
    // Collection rides on the hooks, as `git gc --auto` rides on commands:
    // the check is a counter read, and the work runs once in many turns.
    if snapshot::collection_due() {
        let _ = collect();
    }
}
