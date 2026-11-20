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
    for entry in fs::read_dir(events_dir()).into_iter().flatten().flatten() {
        let age = fs::metadata(entry.path())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok());
        if age.is_some_and(|age| age > ABANDONED) {
            let _ = fs::remove_file(entry.path());
        }
    }
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
/// anything is opened. A user-wide hook runs in every directory the agent
/// does, and records only in a git repository: anywhere else it would
/// snapshot whatever directory a session happened to start in
/// (`~/Downloads`, a scratch dir), which nobody asked for by installing loch
/// once. A project-level install still records a plain directory, since that
/// was asked for. A line naming no scope (local ones, and every line before
/// scopes) records as it always did.
pub fn applies(scope: Option<Scope>, event: &Map) -> bool {
    if scope != Some(Scope::User) {
        return true;
    }
    let root = snapshot::workspace_root(&event_cwd(event));
    // A home directory that is itself a repository (dotfiles) is the root of
    // every directory under it, and is refused before any store is opened.
    snapshot::has_git_marker(&root) && snapshot::snapshottable(&root)
}

/// How long after one hook has handled an event a byte-identical one is taken
/// to be the same event, seen by another hook. Those start within
/// milliseconds of each other and, having waited on the lock, read the time
/// the first wrote as it let go. A genuine repeat follows a model round trip,
/// which is seconds, so a wider window could only drop real events.
const DUPLICATE_WINDOW_MS: u64 = 2_000;

fn now_ms() -> u64 {
    (sys::now() * 1000.0) as u64
}

fn events_dir() -> PathBuf {
    snapshot::loch_home().join("events")
}

/// The right to handle one event, held for as long as the hook runs.
///
/// **An event is handled once, however many of loch's hooks it reaches.** An
/// agent runs every scope's hooks for an event (a user-wide one, a project's,
/// an older line beside a newer one), at once, and each gets the same bytes
/// on stdin: verified on Claude Code 2.1 and Codex 0.166. Two hooks handling
/// one prompt would snapshot it twice and race over the session's state, the
/// second closing the first's turn as an empty interruption. So the hooks of
/// a session take turns under a lock, and one finding that the event it was
/// handed was handled moments ago does nothing. An event is its hook's name,
/// its payload and the length of its transcript: Claude Code's prompts carry
/// a `prompt_id` and Codex's events a `turn_id`, and a second `Stop` for one
/// prompt comes after more of the transcript was written.
///
/// Deciding instead which hook should step aside means knowing which files
/// the agent runs: where it reads a project's settings from, whether it
/// trusts them, whether they changed after it started. A wrong answer loses
/// the turn. This needs none of it.
pub struct Claim {
    file: fs::File,
    digest: String,
}

pub enum Claimed {
    /// This hook handles the event; `finish` once done.
    Mine(Claim),
    /// Another hook already has.
    Handled,
    /// The lock couldn't be taken. The event is handled anyway, since a
    /// duplicate record beats a lost turn.
    Unguarded,
}

pub fn claim(session: &str, name: &str, stdin: &[u8], transcript: Option<&str>) -> Claimed {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update(b"\n");
    hasher.update(stdin);
    // The payload alone can repeat: Claude Code stopping twice for one prompt
    // sends the same bytes both times. The agent waits on these hooks, so
    // every hook for one event sees the same transcript, and a genuine second
    // event follows the output that grew it. Read before waiting on the lock,
    // as the hooks start, so nothing the agent writes once another hook has
    // exited is counted.
    let length = transcript
        .and_then(|t| fs::metadata(t).ok())
        .map(|m| m.len());
    hasher.update(format!("\n{:?}", length).as_bytes());
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    let Ok(()) = fs::create_dir_all(events_dir()) else {
        return Claimed::Unguarded;
    };
    let Ok(mut file) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(events_dir().join(format!("{}.last", session)))
    else {
        return Claimed::Unguarded;
    };
    if file.lock().is_err() {
        return Claimed::Unguarded;
    }
    let mut seen = String::new();
    let _ = file.read_to_string(&mut seen);
    let recent = seen
        .split_once(' ')
        .filter(|(last, _)| *last == digest)
        .and_then(|(_, at)| at.trim().parse::<u64>().ok())
        .is_some_and(|at| now_ms().saturating_sub(at) < DUPLICATE_WINDOW_MS);
    if recent {
        Claimed::Handled
    } else {
        Claimed::Mine(Claim { file, digest })
    }
}

impl Claim {
    /// Records the event as handled, timed from when it finished, and lets
    /// the session's next hook in. A write cut short leaves something that
    /// matches nothing, so the next hook handles the event again.
    pub fn finish(mut self) {
        use std::io::{Seek, Write};
        let _ = self.file.set_len(0);
        let _ = self.file.seek(std::io::SeekFrom::Start(0));
        let _ = write!(self.file, "{} {}", self.digest, now_ms());
    }
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
    if !applies(scope, &event) {
        return;
    }
    let claim = match session_of(&event) {
        Ok(session) => match claim(&session, name, stdin, transcript_of(&event).as_deref()) {
            Claimed::Handled => return,
            Claimed::Mine(claim) => Some(claim),
            Claimed::Unguarded => None,
        },
        Err(Stop) => None,
    };
    let store = || store::create_store(&store::resolve_store_uri(None)).ok();
    let _ = match name {
        "prompt" => prompt(agent, &event, store().as_deref()),
        "stop" => stop(agent, &event, store().as_deref()),
        "end" => end(agent, &event, store().as_deref()),
        // The tool-call hooks the git-notes design used. A settings file can
        // still name them until `install` is re-run; they do nothing.
        _ => return,
    };
    if let Some(claim) = claim {
        claim.finish();
    }
    // Collection rides on the hooks, as `git gc --auto` rides on commands:
    // the check is a counter read, and the work runs once in many turns.
    if snapshot::collection_due() {
        let _ = collect();
    }
}
