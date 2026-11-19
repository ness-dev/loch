//! The agent adapter boundary: everything loch needs to know about one agent
//! harness, behind one trait, so the rest of loch — snapshots, attribution,
//! bundles, the store, the JSON contract — is written once.
//!
//! What an adapter has to provide comes in tiers, and each tier is useful on
//! its own:
//!
//! 1. **Attribution** — `id`, `hook_events`, `hooks_file`. Blame and log need
//!    nothing else: a hook at a turn's start and end, carrying a session id
//!    and a cwd. `current_turn` (what the transcript says the turn was) only
//!    sharpens the record, and the default says nothing.
//! 2. **Transport** — `list_sessions`, `transcript_cwds`, `restore_path`,
//!    `existing_transcript`, `resume_command`. `save`, `list` and `restore`.
//! 3. **Conversation** — `find_transcript`, `fork`. `ask`.
//!
//! Everything past tier 1 has a default that declines, so a harness with turn
//! hooks and nothing else is still an adapter.

use crate::json::Map;
use crate::store::{Store, StoreError};
use crate::trace::{Fork, ForkError};

#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub agent: &'static str,
    pub session_id: String,
    pub path: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    /// The harness's own version, as its transcript records it.
    pub version: Option<String>,
    pub bytes: u64,
    pub modified_at: String,
    pub summary: Option<String>,
    /// Directories the session also worked in before it moved. A restore has
    /// one target cwd, so records naming these cannot be made correct on the
    /// far side; surfacing them is the only honest option.
    pub other_cwds: Vec<String>,
}

/// What the transcript says about a turn: the record that began it and the
/// last record written. Each id is whatever the agent's transcript uses to
/// name a position in itself — a record uuid for Claude Code, a turn id for
/// Codex — and only that agent's adapter ever interprets it.
#[derive(Default)]
pub struct TurnInfo {
    pub prompt_uuid: Option<String>,
    pub prompt: Option<String>,
    pub started: Option<String>,
    pub end_uuid: Option<String>,
    pub ended: Option<String>,
}

/// A loch hook subcommand an agent event runs: `(event, name)`, e.g.
/// `("UserPromptSubmit", "prompt")`.
pub type HookEvent = (&'static str, &'static str);

pub trait Agent: Sync {
    /// Stable identifier: edit records, store keys, bundles, `--agent`.
    fn id(&self) -> &'static str;
    /// For prose.
    fn name(&self) -> &'static str;

    // -- tier 1: attribution ---------------------------------------------------

    /// The events loch hooks, and the subcommand each runs.
    fn hook_events(&self) -> &'static [HookEvent];
    /// The hooks file `install` writes, for a workspace root.
    fn hooks_file(&self, root: &str, local: bool) -> String;
    /// What `install` says after writing, beyond the file it wrote.
    fn install_note(&self) -> &'static str;
    /// Hooks this agent won't run until a person approves them.
    fn hooks_need_review(&self) -> bool {
        false
    }
    /// The turn `event` closes, as the transcript tells it. When the closing
    /// hook belongs to the *next* prompt (`for_next`), that prompt may already
    /// be written and is left out.
    fn current_turn(&self, _transcript: &str, _event: &Map, _for_next: bool) -> TurnInfo {
        TurnInfo::default()
    }
    /// Where the store keeps this session's transcript, under
    /// `transcripts/<id>/`. `None` keeps none.
    fn transcript_key(&self, _path: &str, _session_id: &str) -> Option<String> {
        None
    }

    // -- tier 2: transport ------------------------------------------------------

    /// Where this agent keeps transcripts, for messages.
    fn sessions_dir(&self) -> String;
    fn list_sessions(
        &self,
        _cwd: &str,
        _branch: Option<&str>,
        _session_id: Option<&str>,
    ) -> Vec<SessionMeta> {
        Vec::new()
    }
    fn read_session_meta(&self, _path: &str) -> Option<SessionMeta> {
        None
    }
    /// Every working directory the transcript's records name, with how many
    /// name it, in order of first appearance.
    fn transcript_cwds(&self, _raw: &str) -> Vec<(String, usize)> {
        Vec::new()
    }
    /// Whether `restore` can file a session of this agent's: the transport
    /// tier is implemented.
    fn can_restore(&self) -> bool {
        false
    }
    /// Where a restored transcript goes. `raw` is the rewritten transcript.
    fn restore_path(&self, _raw: &str, _cwd: &str, _session_id: &str) -> Option<String> {
        None
    }
    /// A transcript this machine already holds for the session, which a
    /// restore must not silently duplicate or overwrite.
    fn existing_transcript(&self, _cwd: &str, _session_id: &str) -> Option<String> {
        None
    }
    /// argv that resumes the session.
    fn resume_command(&self, _session_id: &str) -> Option<Vec<String>> {
        None
    }

    // -- tier 3: conversation -----------------------------------------------------

    /// The agent's own copy if it still exists, else the store's.
    fn find_transcript(
        &self,
        _session_id: &str,
        _store: Option<&dyn Store>,
    ) -> Result<Option<String>, StoreError> {
        Ok(None)
    }
    /// The session's title, where the agent keeps one outside the
    /// transcript.
    fn title(&self, _session_id: &str) -> Option<String> {
        None
    }
    /// A new session holding the conversation up to `cut` (the end, if none),
    /// filed under `target_cwd`.
    fn fork(
        &self,
        _transcript: &str,
        _target_cwd: &str,
        _cut: Option<&str>,
        _title: Option<&str>,
    ) -> Result<Fork, ForkError> {
        Err(ForkError::Unsupported(self.name()))
    }
}

static ALL: &[&dyn Agent] = &[&crate::claude::CLAUDE_CODE, &crate::codex::CODEX];

pub fn all() -> &'static [&'static dyn Agent] {
    ALL
}

pub fn by_id(id: &str) -> Option<&'static dyn Agent> {
    all().iter().copied().find(|agent| agent.id() == id)
}

/// The agents `--agent` names, or every one.
/// `all` names every agent too.
pub fn selected(id: Option<&str>) -> Result<Vec<&'static dyn Agent>, String> {
    match id.filter(|id| !id.is_empty()) {
        None | Some("all") => Ok(all().to_vec()),
        Some(id) => by_id(id).map(|agent| vec![agent]).ok_or_else(|| {
            let known: Vec<&str> = all().iter().map(|a| a.id()).collect();
            format!("unknown agent {} (known: {})", id, known.join(", "))
        }),
    }
}
