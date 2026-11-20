//! From a turn back to its conversation: installing the hooks that record
//! turns, finding a session's transcript wherever it survives, describing a
//! turn for blame, and forking the conversation for `ask`.
//!
//! What a turn changed is recorded by the hooks (`hook.rs`) and matched by
//! content (`attribution.rs`); nothing here reads or writes git history.

use crate::agent::{self, Agent, Scope};
use crate::attribution::{EditRecord, TurnRecord};
use crate::claude::{self, home_dir};
use crate::format::Turn;
use crate::git as gitio;
use crate::ingest::{self, Origin};
use crate::json::{self, Map, Value};
use crate::object;
use crate::py;
use crate::store::Store;
use crate::sys;
use std::collections::HashMap;
use std::fs;

pub fn transcripts_prefix(agent: &str) -> String {
    format!("transcripts/{}/", agent)
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

/// What earlier versions of `install` wrote around every Bash call, for the
/// git-notes design. Removed on install, so they stop costing a process each.
const RETIRED: &[&str] = &["pre-tool", "post-tool"];

/// How every line `install` writes ends.
const LINE_END: &str = " 2>/dev/null || true";

#[derive(Debug)]
pub enum InstallError {
    Internal(String),
}

pub struct Installed {
    pub added: Vec<String>,
    /// Events whose loch line was there in another form (another path to
    /// loch, another scope) and was rewritten where it stood.
    pub replaced: Vec<String>,
    /// Old loch hooks no longer used, and second loch lines for an event.
    pub removed: Vec<String>,
    /// Second loch lines for an event that were left where they are, since
    /// the agent keys trust by position.
    pub duplicates: Vec<String>,
}

/// The line a hook runs. `agent` is passed only for agents other than Claude
/// Code, and the scope only for the user's, so the lines earlier versions
/// wrote still match.
pub fn hook_line(command: &str, name: &str, agent: &dyn Agent, scope: Scope) -> String {
    let flag = if agent.id() == claude::AGENT {
        String::new()
    } else {
        format!(" --agent {}", agent.id())
    };
    // Only the user's lines name a scope, the one `hook::applies` acts on, so
    // a project's committed lines and Ness's local ones stay as they were.
    let scope = match scope {
        Scope::User => format!(" --scope {}", Scope::User.as_str()),
        Scope::Project | Scope::Local => String::new(),
    };
    // A machine without loch gets a silent no-op rather than an error on every
    // turn, since a project's settings file is committed and a user's applies
    // everywhere.
    format!("{} hook {}{}{}{}", command, name, flag, scope, LINE_END)
}

/// A loch hook line, read back: which hook, for which agent, at which scope
/// (`None` for a line that names none, as local ones and older ones don't).
pub struct HookLine {
    pub name: String,
    pub agent: String,
    pub scope: Option<Scope>,
}

/// Whether `line` is one `hook_line` wrote, whatever the path to loch. Only
/// these are ever rewritten or removed; every other hook is left alone.
pub fn parse_hook_line(line: &str) -> Option<HookLine> {
    let body = line.strip_suffix(LINE_END)?;
    let at = body.rfind(" hook ")?;
    let mut words = body[at + " hook ".len()..].split(' ');
    let name = words.next()?.to_string();
    let known = RETIRED.contains(&name.as_str())
        || agent::all()
            .iter()
            .any(|a| a.hook_events().iter().any(|(_, n)| *n == name));
    if !known || body[..at].is_empty() {
        return None;
    }
    let mut parsed = HookLine {
        name,
        agent: claude::AGENT.into(),
        scope: None,
    };
    while let Some(flag) = words.next() {
        let value = words.next()?;
        match flag {
            "--agent" => parsed.agent = value.into(),
            "--scope" => parsed.scope = Some(Scope::parse(value)?),
            _ => return None,
        }
    }
    Some(parsed)
}

fn is_retired(line: &str) -> bool {
    parse_hook_line(line).is_some_and(|l| RETIRED.contains(&l.name.as_str()))
}

/// Adds an agent's loch hooks to its hooks file, keeping whatever else is
/// there, and takes out the ones loch no longer uses. Re-running changes
/// nothing. A loch line already there in another form is rewritten in place,
/// and a second one for the same event removed, so no other hook moves;
/// where the agent keys trust by position (Codex), the second is left and
/// reported instead. Claude Code's settings and Codex's
/// hooks.json share the shape
/// `{"hooks": {"<Event>": [{"hooks": [{"type": "command", "command": ...}]}]}}`.
pub fn install_hooks(
    settings_path: &str,
    command: &str,
    agent: &dyn Agent,
    scope: Scope,
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
    let mut replaced = Vec::new();
    let mut duplicates = Vec::new();
    // An agent whose trust is keyed by position keeps every group where it
    // is: removing one would shift the keys of all the groups after it.
    let positional = agent.hooks_need_review();
    for (event, name) in agent.hook_events() {
        let line = hook_line(command, name, agent, scope);
        let groups = hooks
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(groups) = groups else {
            return Err(not_a_dict());
        };
        let ours = |entry: &Value| {
            entry
                .get("command")
                .and_then(Value::as_str)
                .and_then(parse_hook_line)
                .is_some_and(|l| l.name == *name && l.agent == agent.id())
        };
        let mut seen = 0;
        for group in groups.iter_mut() {
            let Some(Value::Array(entries)) =
                group.as_object_mut().and_then(|g| g.get_mut("hooks"))
            else {
                continue;
            };
            let mut index = 0;
            while index < entries.len() {
                if !ours(&entries[index]) {
                    index += 1;
                    continue;
                }
                seen += 1;
                if seen == 1 {
                    if !entries[index]
                        .get("command")
                        .is_some_and(|c| c.is_str(&line))
                    {
                        if let Some(entry) = entries[index].as_object_mut() {
                            entry.insert("command".into(), Value::str(line.as_str()));
                        }
                        replaced.push(event.to_string());
                    }
                    index += 1;
                } else if positional {
                    // Harmless beside the first, since an event is handled
                    // once (`hook::claim`), and only the person can retire it.
                    duplicates.push(event.to_string());
                    index += 1;
                } else {
                    // Another path to loch, appended by an older install.
                    entries.remove(index);
                    removed.push(event.to_string());
                }
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|entries| !entries.is_empty())
        });
        if seen == 0 {
            groups.push(Value::Object(object! {
                "hooks" => vec![Value::Object(object! {"type" => "command", "command" => line.as_str()})],
            }));
            added.push(event.to_string());
        }
    }
    let parent = sys::dirname(settings_path);
    fs::create_dir_all(&parent).map_err(|e| InstallError::Internal(sys::os_error(&e, &parent)))?;
    let text = format!("{}\n", json::dumps(&settings, json::INDENTED));
    fs::write(settings_path, text)
        .map_err(|e| InstallError::Internal(sys::os_error(&e, settings_path)))?;
    let mut unique = std::collections::HashSet::new();
    removed.retain(|event| unique.insert(event.clone()));
    duplicates.dedup();
    Ok(Installed {
        added,
        replaced,
        removed,
        duplicates,
    })
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

/// Where transcripts fetched from the store are kept.
pub fn cache_dir() -> String {
    sys::join(&sys::join(&home_dir(), ".loch"), "cache")
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
    pub backfilled: bool,
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
            "backfilled" => self.backfilled,
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
        agent_id: &str,
        session_id: &str,
    ) -> std::result::Result<&(Option<String>, Vec<Turn>, Map), String> {
        if !self.sessions.contains_key(session_id) {
            let path = match agent::by_id(agent_id) {
                Some(agent) => agent
                    .find_transcript(session_id, self.store)
                    .map_err(|e| e.to_string())?,
                None => None,
            };
            let mut turns = Vec::new();
            let mut header = Map::new();
            // The loch converter reads Claude Code's format alone; another
            // agent's turns are described without loch ids.
            if let (Some(path), true) = (&path, agent_id == claude::AGENT) {
                if let Some(meta) = claude::read_session_meta(path) {
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
        let (path, turns, header) = self.session(&record.agent, &record.session)?;
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
        let kept = || agent::by_id(&record.agent).and_then(|a| a.title(&record.session));
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
            title: title_of("title")
                .or_else(|| title_of("aiTitle"))
                .or_else(kept),
            prompt: turn.prompt.clone(),
            backfilled: turn.backfilled,
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

#[derive(Debug)]
pub enum ForkError {
    Trace(TraceError),
    Internal(String),
    /// The agent's adapter can't fork a conversation.
    Unsupported(&'static str),
}
