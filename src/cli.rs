//! Every command: each builds one `Outcome` — payload
//! and prose together — and `main` renders it.

use crate::agent::{self, Agent, Scope};
use crate::argparse::{Exit, Nargs, Parser};
use crate::attribution::{self, EditRecord};
use crate::blame_html;
use crate::bundle::{self, Bundle, BundleOrigin, BundleSession, BUNDLE_VERSION};
use crate::claude::{self, home_dir, resolve_cwd, SessionMeta, AGENT};
use crate::format;
use crate::index::{self, IndexEntry, IndexError, BRANCH};
use crate::ingest::{self, Origin};
use crate::json::Value;
use crate::object;
use crate::output::{fail, CommandError, Outcome, JSON_FLAG};
use crate::py;
use crate::redact;
use crate::remote::{self, Remote, RemoteError};
use crate::rewrite::{self, ForeignCwd, RewriteSpec};
use crate::snapshot;
use crate::store::{Store, StoreError};
use crate::sync::{self, SyncError};
use crate::sys;
use crate::trace::{self, Link, Resolver, TraceError};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub const USAGE: &str = include_str!("usage.txt");

/// Whether `--json` appears anywhere in argv — `JSON_FLAG in sys.argv`.
pub fn json_requested() -> bool {
    std::env::args().skip(1).any(|a| a == JSON_FLAG)
}

pub enum Failure {
    Error(CommandError),
    Help(String),
    /// A bug rather than a failure a caller can act on: `internal`.
    Internal(String),
}

impl From<CommandError> for Failure {
    fn from(error: CommandError) -> Self {
        Failure::Error(error)
    }
}

impl From<Exit> for Failure {
    fn from(exit: Exit) -> Self {
        match exit {
            Exit::Help(text) => Failure::Help(text),
            Exit::Usage { message, prose } => {
                let mut error = CommandError::new("usage", message);
                error.prose = Some(prose);
                error.exit_status = 2;
                Failure::Error(error)
            }
        }
    }
}

impl From<TraceError> for Failure {
    fn from(error: TraceError) -> Self {
        Failure::Error(CommandError::new(error.code, error.message))
    }
}

type Run = std::result::Result<Outcome, Failure>;

fn err<T>(code: &'static str, message: impl Into<String>) -> std::result::Result<T, Failure> {
    fail(code, message).map_err(Failure::Error)
}

/// `store_errors`: absent is terminal, unreachable is worth retrying.
fn store_error(description: &str, error: StoreError) -> Failure {
    match error {
        StoreError::Missing(m) => Failure::Error(CommandError::new(
            "bundle-not-found",
            format!("{}: {}", description, m),
        )),
        StoreError::Unreachable(m) => Failure::Error(CommandError::new(
            "store-unreachable",
            format!("{}: {}", description, m),
        )),
    }
}

/// Inside store_errors in Python a parse failure is just another exception,
/// so it lands as store-unreachable; kept, since it is the reference's answer.
fn index_error(description: &str, error: IndexError) -> Failure {
    match error {
        IndexError::Store(e) => store_error(description, e),
        IndexError::Unreadable(m) => Failure::Error(CommandError::new(
            "store-unreachable",
            format!("{}: {}", description, m),
        )),
    }
}

pub fn format_bytes(count: u64) -> String {
    if count < 1024 {
        format!("{}B", count)
    } else if count < 1024 * 1024 {
        format!("{:.0}K", count as f64 / 1024.0)
    } else {
        format!("{:.1}M", count as f64 / (1024.0 * 1024.0))
    }
}

/// `f"{text:>width}"`.
fn right(text: &str, width: usize) -> String {
    let len = py::len(text);
    format!("{}{}", " ".repeat(width.saturating_sub(len)), text)
}

fn describe(meta: &SessionMeta) -> String {
    let branch = match &meta.git_branch {
        Some(b) if !b.is_empty() => format!("[{}]", b),
        _ => "[no branch]".into(),
    };
    let collapsed = meta
        .summary
        .as_deref()
        .map(collapse_whitespace)
        .unwrap_or_default();
    let summary = if collapsed.is_empty() {
        String::new()
    } else {
        format!(" {}", collapsed)
    };
    let when = py::truncate(&meta.modified_at, 16).replace('T', " ");
    format!(
        "{}  {}  {}  {}{}",
        meta.session_id,
        when,
        right(&format_bytes(meta.bytes), 5),
        branch,
        summary
    )
}

/// `re.sub(r"\s+", " ", text)`.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::new();
    let mut in_space = false;
    for c in text.chars() {
        if py::is_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

fn describe_entry(entry: &IndexEntry) -> String {
    let branch = match &entry.value {
        Some(v) if !v.is_empty() => format!("[{}]", v),
        _ => "[no branch]".into(),
    };
    let when = py::truncate(&entry.created_at, 16).replace('T', " ");
    let version = match (&entry.claude_version, &entry.agent_version) {
        (Some(v), _) if !v.is_empty() => format!("  claude {}", v),
        (_, Some(v)) if !v.is_empty() => format!("  {} {}", entry.agent, v),
        _ => String::new(),
    };
    format!(
        "{}  {}  {} session(s)  {}{}  {}",
        when,
        right(&format_bytes(entry.bytes.max(0) as u64), 5),
        entry.sessions,
        branch,
        version,
        entry.cwd
    )
}

/// Every selected agent's sessions for a cwd, newest first.
fn collect(
    agents: &[&dyn Agent],
    cwd: Option<&str>,
    branch: Option<&str>,
    session: Option<&str>,
) -> (String, Vec<SessionMeta>) {
    let resolved = resolve_cwd(&cwd.map(str::to_string).unwrap_or_else(sys::getcwd));
    let mut sessions: Vec<SessionMeta> = agents
        .iter()
        .flat_map(|agent| agent.list_sessions(&resolved, branch, session))
        .collect();
    // Stable, so one agent's sessions keep their own order on a tie.
    sessions.sort_by(|a, b| py::cmp(&b.modified_at, &a.modified_at));
    (resolved, sessions)
}

fn agents_arg(id: Option<&str>) -> std::result::Result<Vec<&'static dyn Agent>, Failure> {
    agent::selected(id)
        .map_err(|message| Failure::Error(CommandError::new("unknown-agent", message)))
}

fn none_found(agents: &[&dyn Agent], cwd: &str) -> String {
    let ids: Vec<&str> = agents.iter().map(|a| a.id()).collect();
    format!("no {} sessions found for {}", ids.join(" or "), cwd)
}

fn parser(command: &str) -> Parser {
    Parser::new(&format!("loch {}", command))
}

fn strings(items: &[String]) -> Value {
    Value::Array(items.iter().map(|s| Value::str(s.as_str())).collect())
}

fn session_payload(meta: &SessionMeta) -> Value {
    Value::Object(object! {
        "sessionId" => meta.session_id.as_str(),
        "agent" => meta.agent,
        "path" => meta.path.as_str(),
        "cwd" => meta.cwd.as_str(),
        "otherCwds" => strings(&meta.other_cwds),
        "branch" => Value::opt_str(meta.git_branch.as_deref()),
        "claudeVersion" => Value::opt_str(meta.version.as_deref().filter(|_| meta.agent == AGENT)),
        "agentVersion" => Value::opt_str(meta.version.as_deref()),
        "bytes" => meta.bytes as i64,
        "modifiedAt" => meta.modified_at.as_str(),
        "summary" => Value::opt_str(meta.summary.as_deref()),
    })
}

/// Deliberately not index.rs's stored shape: a storage format and a wire
/// contract change for different reasons.
fn entry_payload(entry: &IndexEntry) -> Value {
    Value::Object(object! {
        "ref" => entry.ref_.as_str(),
        "dimension" => entry.dimension.as_str(),
        "value" => Value::opt_str(entry.value.as_deref()),
        "agent" => entry.agent.as_str(),
        "createdAt" => entry.created_at.as_str(),
        "bytes" => entry.bytes,
        "sessions" => entry.sessions,
        "cwd" => entry.cwd.as_str(),
        "claudeVersion" => Value::opt_str(entry.claude_version.as_deref()),
        "agentVersion" => Value::opt_str(entry.claude_version.as_deref().or(entry.agent_version.as_deref())),
    })
}

// -- list ---------------------------------------------------------------------

pub fn command_list(argv: &[String]) -> Run {
    let mut p = parser("list");
    p.option("--cwd")
        .option("--branch")
        .option("--agent")
        .option_optional("--remote", "", "NAME")
        .flag("--saved", None)
        .flag("--all", Some("every"))
        .option("--store");
    let args = p.parse(argv)?;
    let (cwd, branch) = (args.str("cwd"), args.str("branch"));
    let every = args.flag("every");
    if let Some(asked) = remote_asked(&args) {
        let (remote, store) = open_remote(asked.as_deref())?;
        return list_bundles(cwd, branch, store.as_ref(), Some(&remote), every);
    }
    if args.flag("saved") || every {
        let local = crate::store::local_store();
        return list_bundles(cwd, branch, &local, None, every);
    }
    let agents = agents_arg(args.str("agent").as_deref())?;
    let (cwd, sessions) = collect(&agents, cwd.as_deref(), branch.as_deref(), None);
    let text = if sessions.is_empty() {
        format!("{}\n", none_found(&agents, &cwd))
    } else {
        let mut text = format!("{} session(s) for {}\n", sessions.len(), cwd);
        for meta in &sessions {
            text.push_str(&format!("  {}\n", describe(meta)));
        }
        text
    };
    Ok(Outcome::with(
        "list",
        object! {
            "remote" => false,
            "saved" => false,
            "cwd" => cwd.as_str(),
            "branch" => Value::opt_str(branch.as_deref()),
            "sessions" => sessions.iter().map(session_payload).collect::<Vec<_>>(),
        },
        text,
    ))
}

pub fn current_branch(cwd: &str) -> Option<String> {
    let out = trace::git_unchecked(cwd, &["symbolic-ref", "--short", "HEAD"]);
    let branch = py::strip(&out);
    (!branch.is_empty()).then(|| branch.to_string())
}

/// Saved bundles, by their branch entries: this machine's (`--saved`), or a
/// remote's (`--remote`, git's `ls-remote`).
fn list_bundles(
    cwd: Option<String>,
    branch: Option<String>,
    store: &dyn Store,
    remote: Option<&Remote>,
    every: bool,
) -> Run {
    let cwd = resolve_cwd(&cwd.unwrap_or_else(sys::getcwd));
    let branch = branch.or_else(|| current_branch(&cwd));
    let prefix = if every {
        index::dimension_prefix(BRANCH)
    } else {
        index::value_prefix(BRANCH, branch.as_deref())
    };
    let described = remote.map_or_else(|| store.describe(), Remote::label);
    let entries = index::load_entries(store, &prefix)
        .map_err(|e| index_error(&format!("could not list {}", described), e))?;
    let mut refs: Vec<&str> = entries.iter().map(|e| e.ref_.as_str()).collect();
    refs.sort();
    refs.dedup();
    let scope = if every {
        String::new()
    } else {
        format!(
            " for [{}]",
            branch
                .as_deref()
                .filter(|b| !b.is_empty())
                .unwrap_or("no branch")
        )
    };
    let text = if entries.is_empty() {
        format!("no bundles{} in {}\n", scope, described)
    } else {
        let mut text = format!("{} bundle(s){} in {}\n", refs.len(), scope, described);
        for entry in &entries {
            text.push_str(&format!(
                "  {}\n    {}\n",
                entry.ref_,
                describe_entry(entry)
            ));
        }
        text
    };
    let mut payload = object! {
        "remote" => remote.is_some(),
        "saved" => remote.is_none(),
        "store" => store.describe(),
        "all" => every,
        "branch" => if every { Value::Null } else { Value::opt_str(branch.as_deref()) },
        "bundles" => refs.len(),
        "entries" => entries.iter().map(entry_payload).collect::<Vec<_>>(),
    };
    let mut outcome_warnings = Vec::new();
    if let Some(remote) = remote {
        payload.insert("remoteName".into(), Value::opt_str(remote.name.as_deref()));
        if remote.deprecated {
            payload.insert("deprecated".into(), Value::str("LOCH_STORE"));
        }
        outcome_warnings.extend(remote.warning());
    }
    let mut outcome = Outcome::with("list", payload, text);
    outcome.warnings = outcome_warnings;
    Ok(outcome)
}

// -- save ---------------------------------------------------------------------

fn counts_value(counts: &indexmap::IndexMap<String, usize>) -> Value {
    Value::Object(
        counts
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(*v)))
            .collect(),
    )
}

pub fn command_save(argv: &[String]) -> Run {
    let mut p = parser("save");
    p.option("--cwd")
        .option("--branch")
        .option("--session")
        .option("--agent")
        .option_optional("--push", "", "REMOTE")
        .option("--store");
    let args = p.parse(argv)?;
    // `--store <uri>` is the older spelling of `--push <uri>`.
    let push_to: Option<Option<String>> = match args.str("store").filter(|s| !s.is_empty()) {
        Some(uri) => Some(Some(uri)),
        None => args.str("push").map(|r| Some(r).filter(|r| !r.is_empty())),
    };
    let agents = agents_arg(args.str("agent").as_deref())?;
    let (cwd, sessions) = collect(
        &agents,
        args.str("cwd").as_deref(),
        args.str("branch").as_deref(),
        args.str("session").as_deref(),
    );
    if sessions.is_empty() {
        return err("no-sessions", none_found(&agents, &cwd));
    }
    // The bundle is its first session's agent's; any other session names its
    // own, so a bundle of one agent's sessions has the bytes it always had.
    let bundle_agent = sessions[0].agent;
    let mut packed_sessions = Vec::new();
    let mut redactions = Vec::new();
    for meta in &sessions {
        // In memory only — the agent's own transcript on disk stays verbatim.
        let bytes =
            fs::read(&meta.path).map_err(|e| Failure::Internal(sys::os_error(&e, &meta.path)))?;
        let raw = match std::str::from_utf8(&bytes) {
            Ok(raw) => raw,
            Err(error) => {
                return err(
                    "transcript-unreadable",
                    format!(
                        "refusing to pack {}: not UTF-8 at byte {}",
                        meta.session_id,
                        error.valid_up_to()
                    ),
                )
            }
        };
        let scrubbed = match redact::redact(raw) {
            Ok(scrubbed) => scrubbed,
            Err(error) => {
                return err(
                    "redaction-failed",
                    format!("refusing to pack {}: {}", meta.session_id, error.0),
                )
            }
        };
        let is_claude = meta.agent == AGENT;
        packed_sessions.push(BundleSession {
            session_id: meta.session_id.clone(),
            agent: (meta.agent != bundle_agent).then(|| meta.agent.to_string()),
            cwd: meta.cwd.clone(),
            git_branch: meta.git_branch.clone(),
            claude_version: meta.version.clone().filter(|_| is_claude),
            agent_version: meta.version.clone().filter(|_| !is_claude),
            modified_at: meta.modified_at.clone(),
            transcript: scrubbed.text,
        });
        redactions.push(scrubbed.counts);
    }
    let mut bundle = Bundle {
        version: BUNDLE_VERSION,
        agent: bundle_agent.into(),
        created_at: sys::iso_timestamp(sys::creation_time()),
        origin: BundleOrigin {
            home: home_dir(),
            platform: sys::platform_name().into(),
            hostname: sys::hostname(),
        },
        sessions: packed_sessions,
    };
    bundle.version = bundle.required_version();
    let packed = bundle::pack_bundle(&bundle).map_err(|e| Failure::Internal(e.0))?;
    let ref_ = bundle::bundle_ref(&packed);
    // Resolved before anything is written, so a remote that can't be named
    // fails the save rather than leaving a bundle it didn't ask for.
    let remote = match &push_to {
        Some(asked) => Some(open_remote(asked.as_deref())?),
        None => None,
    };
    let store = crate::store::local_store();
    let described = format!("could not write to {}", store.describe());
    // Bundle first: an index entry is derived from it.
    store
        .put(&index::bundle_key(&ref_), &packed)
        .map_err(|e| store_error(&described, e))?;
    let entries = index::index_bundle(&store, &bundle, &ref_, packed.len())
        .map_err(|e| store_error(&described, e))?;
    let pushed = match &remote {
        Some((remote, opened)) => {
            let mut report = sync::PushReport::new(false);
            let result = sync::push(
                &store,
                opened.as_ref(),
                &remote.uri,
                sync::Scope::Bundle(&ref_),
                &mut report,
            );
            let summary = with_remote(report.payload(), remote);
            if let Err(error) = result {
                return Err(match sync_error(remote, error) {
                    Failure::Error(mut failure) => {
                        failure.message =
                            format!("saved {} on this machine, but {}", ref_, failure.message);
                        failure.payload = object! {
                            "ref" => ref_.as_str(),
                            "store" => store.describe(),
                            "pushed" => Value::Object(summary),
                        };
                        Failure::Error(failure)
                    }
                    other => other,
                });
            }
            Some((remote, report, summary))
        }
        None => None,
    };

    let total: usize = redactions.iter().map(|c| c.values().sum::<usize>()).sum();
    let mut text = String::new();
    for (session, counts) in bundle.sessions.iter().zip(&redactions) {
        let note = if counts.is_empty() {
            String::new()
        } else {
            format!(" — {}", redact::summarize(counts))
        };
        text.push_str(&format!(
            "  packed {} [{}]{}\n",
            session.session_id,
            session
                .git_branch
                .as_deref()
                .filter(|b| !b.is_empty())
                .unwrap_or("no branch"),
            note
        ));
    }
    let indexed: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "[{}]",
                e.value
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .unwrap_or("no branch")
            )
        })
        .collect();
    text.push_str(&format!(
        "\nstored {} in {}\n",
        format_bytes(packed.len() as u64),
        store.describe()
    ));
    text.push_str(&format!("indexed under {}\n", indexed.join(", ")));
    match &pushed {
        Some((remote, report, _)) if report.tombstoned > 0 => text.push_str(&format!(
            "not pushed: {} has deleted this bundle\n",
            remote.label()
        )),
        Some((remote, report, _)) if report.total() == 0 => {
            text.push_str(&format!("{} already has it\n", remote.label()))
        }
        Some((remote, _, _)) => text.push_str(&format!("pushed to {}\n", remote.label())),
        None => {}
    }
    text.push_str(&format!("ref {}\n", ref_));
    // A caller that set LOCH_STORE expected `save` to upload, as it did
    // before remotes; answering `ok` and keeping the bundle here would be the
    // silent failure, so it is told.
    let mut warnings = Vec::new();
    let stale_store = push_to.is_none() && std::env::var_os("LOCH_STORE").is_some();
    if stale_store {
        warnings.push(
            "LOCH_STORE is set, but save keeps bundles on this machine now; `loch save --push` or `loch push` sends them\n"
                .to_string(),
        );
    }

    let mut outcome = Outcome::with(
        "save",
        object! {
            "pushed" => match &pushed {
                Some((_, _, summary)) => Value::Object(summary.clone()),
                None => Value::Null,
            },
            "ref" => ref_.as_str(),
            "bytes" => packed.len(),
            "store" => store.describe(),
            "cwd" => cwd.as_str(),
            "createdAt" => bundle.created_at.as_str(),
            // Counts by rule name only. The matched text never leaves this
            // process: save's stdout lands in the next agent's transcript.
            "redacted" => total,
            "sessions" => bundle.sessions.iter().zip(&redactions).map(|(s, counts)| Value::Object(object! {
                "sessionId" => s.session_id.as_str(),
                "agent" => s.agent_of(&bundle),
                "branch" => Value::opt_str(s.git_branch.as_deref()),
                "cwd" => s.cwd.as_str(),
                "claudeVersion" => Value::opt_str(s.claude_version.as_deref()),
                "agentVersion" => Value::opt_str(s.version()),
                "modifiedAt" => s.modified_at.as_str(),
                "redactions" => counts_value(counts),
            })).collect::<Vec<_>>(),
            "indexed" => entries.iter().map(|e| Value::Object(object! {
                "dimension" => e.dimension.as_str(),
                "value" => Value::opt_str(e.value.as_deref()),
                "key" => index::index_key(&e.dimension, e.value.as_deref(), &e.ref_),
            })).collect::<Vec<_>>(),
        },
        text,
    );
    if stale_store {
        outcome
            .payload
            .insert("deprecated".into(), Value::str("LOCH_STORE"));
    }
    if let Some((remote, _, _)) = &pushed {
        warnings.extend(remote.warning());
    }
    outcome.warnings = warnings;
    Ok(outcome)
}

// -- restore ------------------------------------------------------------------

struct Restored {
    agent: String,
    session_id: String,
    origin_session_id: String,
    branch: Option<String>,
    status: &'static str,
    path: String,
    residual: Vec<String>,
    resume_command: Option<String>,
    foreign: Vec<(String, ForeignCwd)>,
}

fn restored_payload(session: &Restored) -> Value {
    Value::Object(object! {
        "sessionId" => session.session_id.as_str(),
        "agent" => session.agent.as_str(),
        "originSessionId" => session.origin_session_id.as_str(),
        "branch" => Value::opt_str(session.branch.as_deref()),
        "status" => session.status,
        "path" => session.path.as_str(),
        "residualOriginPaths" => strings(&session.residual),
        // Keyed by the origin path, the left-hand side a caller states a
        // mapping with; `restored` is what the file on disk actually says.
        "foreignCwds" => Value::Object(session.foreign.iter().map(|(origin, f)| (origin.clone(), Value::Object(object! {
            "count" => f.count,
            "restored" => f.restored.as_str(),
        }))).collect()),
        "resumeCommand" => Value::opt_str(session.resume_command.as_deref()),
    })
}

pub fn command_restore(argv: &[String]) -> Run {
    let mut p = parser("restore");
    p.positional("ref", Nargs::Optional)
        .option("--into")
        .option_optional("--remote", "", "NAME")
        .option("--store")
        .flag("--force", None)
        .flag("--new-id", Some("new_id"));
    let args = p.parse(argv)?;
    let (ref_, asked) = ref_and_remote(&args);
    let Some(ref_) = ref_ else {
        return err("missing-ref", "restore requires a bundle ref");
    };
    let local = crate::store::local_store();
    let (packed, fetched) = match local.get(&index::bundle_key(&ref_)) {
        Ok(packed) => (packed, None),
        Err(StoreError::Missing(_)) => {
            let (remote, packed) = fetch_bundle(&ref_, asked)?;
            (packed, Some(remote))
        }
        Err(e) => {
            return Err(store_error(
                &format!("could not read {} from {}", ref_, local.describe()),
                e,
            ))
        }
    };
    let described = format!(
        "could not read {} from {}",
        ref_,
        fetched
            .as_ref()
            .map_or_else(|| local.describe(), Remote::label)
    );
    let bundle = match bundle::unpack_bundle(&packed) {
        Ok(bundle) => bundle,
        // Fetched fine and still unusable: neither absent nor unreachable.
        Err(error) => return err("bundle-unreadable", format!("{}: {}", described, error)),
    };
    // Kept, as git keeps what it fetched: the bytes hash to the ref, so this
    // copy is the bundle, whichever remote it came from.
    if fetched.is_some() {
        let cached = local
            .put(&index::bundle_key(&ref_), &packed)
            .and_then(|()| index::index_bundle(&local, &bundle, &ref_, packed.len()).map(|_| ()));
        if let Err(e) = cached {
            return Err(store_error(
                &format!("could not keep {} in {}", ref_, local.describe()),
                e,
            ));
        }
    }
    let target_cwd = resolve_cwd(&args.str("into").unwrap_or_else(sys::getcwd));
    let target_home = home_dir();
    let mut restored = Vec::new();
    for session in &bundle.sessions {
        let agent_id = session.agent_of(&bundle).to_string();
        // A bundle from a build that knows agents this one doesn't, or one
        // whose adapter can't restore.
        let Some(adapter) = agent::by_id(&agent_id).filter(|a| a.can_restore()) else {
            restored.push(Restored {
                agent: agent_id,
                session_id: session.session_id.clone(),
                origin_session_id: session.session_id.clone(),
                branch: session.git_branch.clone(),
                status: "unsupported",
                path: String::new(),
                residual: Vec::new(),
                resume_command: None,
                foreign: Vec::new(),
            });
            continue;
        };
        let target_session_id = if args.flag("new_id") {
            sys::uuid4()
        } else {
            session.session_id.clone()
        };
        let spec = RewriteSpec {
            origin_cwd: session.cwd.clone(),
            origin_home: bundle.origin.home.clone(),
            target_cwd: target_cwd.clone(),
            target_home: target_home.clone(),
            origin_session_id: Some(session.session_id.clone()),
            target_session_id: Some(target_session_id.clone()),
        };
        let transcript = rewrite::rewrite_transcript(&session.transcript, &spec);
        let mut destination = adapter
            .restore_path(&transcript, &target_cwd, &target_session_id)
            .unwrap_or_default();
        let existing = adapter.existing_transcript(&target_cwd, &target_session_id);
        if existing.is_some() && !args.flag("force") {
            restored.push(Restored {
                agent: agent_id.clone(),
                session_id: target_session_id,
                origin_session_id: session.session_id.clone(),
                branch: session.git_branch.clone(),
                status: "skipped",
                path: existing.unwrap_or(destination),
                residual: Vec::new(),
                resume_command: None,
                foreign: Vec::new(),
            });
            continue;
        }
        // Overwriting in place: a second copy of one session id is a
        // transcript the agent may pick either of.
        if let Some(existing) = existing {
            destination = existing;
        }
        let parent = sys::dirname(&destination);
        fs::create_dir_all(&parent).map_err(|e| Failure::Internal(sys::os_error(&e, &parent)))?;
        fs::write(&destination, transcript.as_bytes())
            .map_err(|e| Failure::Internal(sys::os_error(&e, &destination)))?;
        restored.push(Restored {
            agent: agent_id.clone(),
            session_id: target_session_id.clone(),
            origin_session_id: session.session_id.clone(),
            branch: session.git_branch.clone(),
            status: "restored",
            path: destination,
            residual: rewrite::residual_origin_paths(&transcript, &spec),
            resume_command: adapter
                .resume_command(&target_session_id)
                .map(|argv| format!("cd {} && {}", target_cwd, argv.join(" "))),
            // Pre-rewrite: the origin path only exists losslessly here.
            foreign: rewrite::foreign_cwds_of(adapter.transcript_cwds(&session.transcript), &spec),
        });
    }

    let payload = object! {
        "ref" => ref_.as_str(),
        "store" => local.describe(),
        // Where the bundle came from: here, or fetched from a remote.
        "fetchedFrom" => fetched.as_ref().map_or(Value::Null, Remote::payload),
        "cwd" => target_cwd.as_str(),
        "origin" => object! {
            "hostname" => bundle.origin.hostname.as_str(),
            "home" => bundle.origin.home.as_str(),
            "platform" => bundle.origin.platform.as_str(),
        },
        "sessions" => restored.iter().map(restored_payload).collect::<Vec<_>>(),
    };
    let mut warnings: Vec<String> = restored
        .iter()
        .filter(|s| s.status != "restored")
        .map(|s| {
            if s.status == "unsupported" {
                format!(
                    "  skipped {} ({} sessions aren't supported by this build)\n",
                    s.session_id, s.agent
                )
            } else {
                format!(
                    "  skipped {} (already exists; --force to overwrite)\n",
                    s.session_id
                )
            }
        })
        .collect();
    if let Some(remote) = &fetched {
        warnings.extend(remote.warning());
    }
    for session in &restored {
        for (origin, foreign) in &session.foreign {
            warnings.push(format!(
                "  {} worked in {} for {} record(s), which no rewrite into {} can satisfy\n",
                session.session_id, origin, foreign.count, target_cwd
            ));
        }
    }
    if !restored.iter().any(|s| s.status == "restored") {
        let mut error = CommandError::new("nothing-restored", "nothing restored");
        error.payload = payload;
        error.warnings = warnings;
        return Err(Failure::Error(error));
    }

    let mut text = String::new();
    for session in restored.iter().filter(|s| s.status == "restored") {
        let note = if session.residual.is_empty() {
            String::new()
        } else {
            format!("  (residual origin paths: {})", session.residual.join(", "))
        };
        text.push_str(&format!(
            "  restored {} [{}]{}\n",
            session.session_id,
            session
                .branch
                .as_deref()
                .filter(|b| !b.is_empty())
                .unwrap_or("no branch"),
            note
        ));
        for (origin, foreign) in &session.foreign {
            text.push_str(&format!(
                "    {} record(s) worked in {}, now {}\n",
                foreign.count, origin, foreign.restored
            ));
        }
    }
    text.push_str(&format!(
        "\nfrom {} ({})\nresume with:\n",
        bundle.origin.hostname, bundle.origin.home
    ));
    for session in &restored {
        if let Some(command) = &session.resume_command {
            text.push_str(&format!("  {}\n", command));
        }
    }
    let mut outcome = Outcome::with("restore", payload, text);
    outcome.warnings = warnings;
    Ok(outcome)
}

// -- delete / reindex -----------------------------------------------------------

pub fn command_delete(argv: &[String]) -> Run {
    let mut p = parser("delete");
    p.positional("ref", Nargs::Optional)
        .option("--session")
        .option("--agent")
        .option_optional("--remote", "", "NAME")
        .option("--store");
    let args = p.parse(argv)?;
    let (ref_, asked) = ref_and_remote(&args);
    let session = args.str("session").filter(|s| !s.is_empty());
    let local = crate::store::local_store();
    let remote = match &asked {
        Some(asked) => Some(open_remote(asked.as_deref())?),
        None => None,
    };
    let deleted = match (ref_, session) {
        (Some(_), Some(_)) => {
            let mut error = CommandError::new("usage", "delete takes a ref or --session, not both");
            error.exit_status = 2;
            return Err(Failure::Error(error));
        }
        (None, None) => return err("missing-ref", "delete requires a bundle ref or --session"),
        (Some(ref_), None) => delete_bundle_everywhere(&local, remote.as_ref(), &ref_)?,
        (None, Some(session)) => {
            delete_session_everywhere(&local, remote.as_ref(), &session, args.str("agent"))?
        }
    };
    let mut text: String = deleted
        .removed
        .iter()
        .map(|k| format!("  removed {}\n", k))
        .collect();
    for key in &deleted.removed_remote {
        text.push_str(&format!("  removed {} from the remote\n", key));
    }
    let what = deleted.subject.clone();
    match &remote {
        Some((remote, _)) => text.push_str(&format!(
            "\ndeleted {} from {} and this machine; a tombstone keeps it deleted\n",
            what,
            remote.label()
        )),
        None => text.push_str(&format!("\ndeleted {} from {}\n", what, local.describe())),
    }
    let mut payload = object! {
        "ref" => Value::opt_str(deleted.ref_.as_deref()),
        "session" => Value::opt_str(deleted.session.as_deref()),
        "store" => local.describe(),
        "removed" => strings(&deleted.removed),
        "removedRemote" => strings(&deleted.removed_remote),
        "tombstones" => strings(&deleted.tombstones),
    };
    let mut outcome_warnings = Vec::new();
    if let Some((remote, _)) = &remote {
        payload = with_remote(payload, remote);
        outcome_warnings.extend(remote.warning());
    } else {
        payload.insert("remote".into(), Value::Null);
    }
    let mut outcome = Outcome::with("delete", payload, text);
    outcome.warnings = outcome_warnings;
    Ok(outcome)
}

struct Deleted {
    subject: String,
    ref_: Option<String>,
    session: Option<String>,
    removed: Vec<String>,
    removed_remote: Vec<String>,
    tombstones: Vec<String>,
}

/// A bundle, from this machine and, with a remote, from it too. On a remote
/// the tombstone is written first, so a push racing the delete sees it before
/// the bundle is gone; here it is written as well, so this machine never
/// pushes the bundle anywhere again.
fn delete_bundle_everywhere(
    local: &crate::store::FileStore,
    remote: Option<&(Remote, Box<dyn Store>)>,
    ref_: &str,
) -> std::result::Result<Deleted, Failure> {
    let mut deleted = Deleted {
        subject: ref_.to_string(),
        ref_: Some(ref_.to_string()),
        session: None,
        removed: Vec::new(),
        removed_remote: Vec::new(),
        tombstones: Vec::new(),
    };
    let local_failure = |e| {
        index_error(
            &format!("could not delete {} from {}", ref_, local.describe()),
            e,
        )
    };
    let Some((remote, opened)) = remote else {
        deleted.removed = index::delete_bundle(local, ref_).map_err(local_failure)?;
        return Ok(deleted);
    };
    let described = format!("could not delete {} from {}", ref_, remote.label());
    // A ref in neither place is a typo, not a delete: refused before any
    // tombstone is written, as a local delete of it is.
    let key = index::bundle_key(ref_);
    let on_remote = opened
        .list(&key)
        .map_err(|e| store_error(&described, e))?
        .contains(&key);
    if !on_remote && !local.exists(&key) {
        return Err(store_error(
            &described,
            StoreError::Missing(format!("no object at {}", key)),
        ));
    }
    let tombstone = sync::bundle_tombstone(ref_);
    opened
        .put(&tombstone, &sync::tombstone_body())
        .map_err(|e| store_error(&described, e))?;
    deleted.tombstones.push(tombstone.clone());
    deleted.removed_remote = match index::delete_bundle(opened.as_ref(), ref_) {
        Ok(removed) => removed,
        Err(IndexError::Store(StoreError::Missing(_))) => Vec::new(),
        // A bundle that won't unpack still goes; its entries wait for reindex.
        Err(IndexError::Unreadable(_)) => {
            let key = index::bundle_key(ref_);
            opened
                .delete(&key)
                .map_err(|e| store_error(&described, e))?;
            vec![key]
        }
        Err(e) => return Err(index_error(&described, e)),
    };
    local
        .put(&tombstone, &sync::tombstone_body())
        .map_err(|e| store_error(&format!("could not write {}", tombstone), e))?;
    deleted.removed = match index::delete_bundle(local, ref_) {
        Ok(removed) => removed,
        Err(IndexError::Store(StoreError::Missing(_))) => Vec::new(),
        Err(e) => return Err(local_failure(e)),
    };
    Ok(deleted)
}

/// A session's edit record, file index and transcripts, from this machine
/// and, with a remote, from it too, tombstoned as a bundle is. The agent's
/// own transcript in its own directory is never touched.
fn delete_session_everywhere(
    local: &crate::store::FileStore,
    remote: Option<&(Remote, Box<dyn Store>)>,
    session: &str,
    agent_id: Option<String>,
) -> std::result::Result<Deleted, Failure> {
    if !agent::is_safe_session_id(session) {
        let mut error = CommandError::new("usage", "--session is not a session id");
        error.exit_status = 2;
        return Err(Failure::Error(error));
    }
    let agents: Vec<String> = match agent_id {
        Some(id) => vec![agents_arg(Some(&id))?
            .first()
            .map(|a| a.id().to_string())
            .unwrap_or(id)],
        None => {
            let mut found = sync::agents_of_session(local, session)
                .map_err(|e| store_error(&format!("could not read {}", local.describe()), e))?;
            for id in agent::all().iter().map(|a| a.id()) {
                if attribution::load_local(id, session).is_some() && !found.iter().any(|f| f == id)
                {
                    found.push(id.to_string());
                }
            }
            if let Some((remote, opened)) = remote {
                for id in sync::agents_of_session(opened.as_ref(), session)
                    .map_err(|e| store_error(&format!("could not read {}", remote.label()), e))?
                {
                    if !found.contains(&id) {
                        found.push(id);
                    }
                }
            }
            found
        }
    };
    if agents.is_empty() {
        return err(
            "session-not-found",
            format!("no record of session {} here or on the remote", session),
        );
    }
    let mut deleted = Deleted {
        subject: format!("session {}", session),
        ref_: None,
        session: Some(session.to_string()),
        removed: Vec::new(),
        removed_remote: Vec::new(),
        tombstones: Vec::new(),
    };
    let local_failure = |e| store_error(&format!("could not delete from {}", local.describe()), e);
    for agent in &agents {
        let tombstone = sync::session_tombstone(agent, session);
        if let Some((remote, opened)) = remote {
            let described = format!(
                "could not delete session {} from {}",
                session,
                remote.label()
            );
            opened
                .put(&tombstone, &sync::tombstone_body())
                .map_err(|e| store_error(&described, e))?;
            deleted.tombstones.push(tombstone.clone());
            deleted.removed_remote.extend(
                sync::delete_session(opened.as_ref(), agent, session)
                    .map_err(|e| store_error(&described, e))?,
            );
            local
                .put(&tombstone, &sync::tombstone_body())
                .map_err(local_failure)?;
        }
        deleted
            .removed
            .extend(sync::delete_session(local, agent, session).map_err(local_failure)?);
        if attribution::forget_local(local, agent, session).map_err(local_failure)? {
            deleted
                .removed
                .push(format!("~/.loch/edits/{}/{}.json", agent, session));
        }
    }
    if remote.is_none() && deleted.removed.is_empty() {
        return err(
            "session-not-found",
            format!("no record of session {} on this machine", session),
        );
    }
    Ok(deleted)
}

pub fn command_reindex(argv: &[String]) -> Run {
    let mut p = parser("reindex");
    p.option_optional("--remote", "", "NAME").option("--store");
    let args = p.parse(argv)?;
    let store: Box<dyn Store> = match remote_asked(&args) {
        Some(asked) => open_remote(asked.as_deref())?.1,
        None => Box::new(crate::store::local_store()),
    };
    let described = format!("could not reindex {}", store.describe());
    let result = index::reindex(store.as_ref()).map_err(|e| index_error(&described, e))?;
    // The file index, re-derived from the edit records the same way, with
    // entries no record accounts for removed.
    let stale: std::collections::BTreeSet<String> = store
        .list(&index::dimension_prefix(attribution::FILE))
        .map_err(|e| store_error(&described, e))?
        .into_iter()
        .collect();
    let written: std::collections::BTreeSet<String> = attribution::reindex_files(store.as_ref())
        .map_err(|e| store_error(&described, e))?
        .into_iter()
        .collect();
    let gone: Vec<&String> = stale.difference(&written).collect();
    for key in &gone {
        store.delete(key).map_err(|e| store_error(&described, e))?;
    }
    Ok(Outcome::with(
        "reindex",
        object! {
            "store" => store.describe(),
            "bundles" => result.bundles,
            "entries" => result.entries,
            "removed" => result.removed,
            "fileEntries" => written.len(),
            "fileRemoved" => gone.len(),
        },
        format!(
            "scanned {} bundle(s) in {}\nwrote {} index entries, removed {} stale\nwrote {} file entries, removed {} stale\n",
            result.bundles,
            store.describe(),
            result.entries,
            result.removed,
            written.len(),
            gone.len()
        ),
    ))
}

// -- ingest -------------------------------------------------------------------------

fn default_derived() -> String {
    sys::join(&sys::join(&home_dir(), ".loch"), "derived")
}

pub fn command_ingest(argv: &[String]) -> Run {
    let mut p = parser("ingest");
    p.option("--cwd")
        .option("--branch")
        .option("--session")
        .option("--author")
        .option("--author-key")
        .option("--out");
    let args = p.parse(argv)?;
    let mut author = args.str("author");
    if let Some(key_path) = args.str("author_key") {
        if author.is_some() {
            return err("usage", "pass --author or --author-key, not both");
        }
        let text = match fs::read(&key_path) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| Failure::Internal(format!("{} is not UTF-8", key_path)))?,
            Err(error) => {
                return err(
                    "author-key-unreadable",
                    format!("{}: {}", key_path, sys::os_error(&error, &key_path)),
                )
            }
        };
        match ingest::ssh_author_id(&text) {
            Ok(id) => author = Some(id),
            Err(error) => {
                return err(
                    "author-key-unreadable",
                    format!("{}: {}", key_path, error.0),
                )
            }
        }
    }
    // The loch converter reads Claude Code's transcripts alone (SPEC.md §8).
    let agents: Vec<&dyn Agent> = vec![&claude::CLAUDE_CODE];
    let (cwd, sessions) = collect(
        &agents,
        args.str("cwd").as_deref(),
        args.str("branch").as_deref(),
        args.str("session").as_deref(),
    );
    if sessions.is_empty() {
        return err("no-sessions", none_found(&agents, &cwd));
    }
    let out = args.str("out").unwrap_or_else(default_derived);
    fs::create_dir_all(&out).map_err(|e| Failure::Internal(sys::os_error(&e, &out)))?;
    let home = home_dir();
    let mut results = Vec::new();
    let mut text = String::new();
    for meta in &sessions {
        let origin = Origin {
            cwd: meta.cwd.clone(),
            home: home.clone(),
            session_id: meta.session_id.clone(),
        };
        let derived = ingest::ingest(&meta.path, &origin, author.as_deref())
            .map_err(|e| Failure::Internal(format!("{:?}", e)))?;
        let path = sys::join(&out, &ingest::derived_name(&derived));
        let packed =
            format::pack_derived(&derived).map_err(|_| Failure::Internal("not JSON".into()))?;
        fs::write(&path, packed).map_err(|e| Failure::Internal(sys::os_error(&e, &path)))?;
        let leaves = derived
            .heads
            .get("leaves")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        results.push(Value::Object(object! {
            "sessionId" => meta.session_id.as_str(),
            "source" => derived.header["source"].get("id").cloned().unwrap_or(Value::Null),
            "path" => path.as_str(),
            "turns" => derived.turns.len(),
            "heads" => derived.heads.clone(),
        }));
        text.push_str(&format!(
            "  {}  {} turns  {} head(s)\n",
            meta.session_id,
            derived.turns.len(),
            leaves
        ));
    }
    text.push_str(&format!(
        "\ningested {} session(s) into {}\n",
        sessions.len(),
        out
    ));
    Ok(Outcome::with(
        "ingest",
        object! {
            "cwd" => cwd.as_str(),
            "out" => out.as_str(),
            "converter" => ingest::converter(),
            "sessions" => results,
        },
        text,
    ))
}

// -- hook / install -------------------------------------------------------------

pub fn command_hook(argv: &[String]) -> Run {
    use std::io::Read;
    let name = argv.first().map(String::as_str).unwrap_or("");
    // `hook <name> [--agent <id>]`, read by hand: a hook must never fail on
    // its arguments, and the lines earlier versions installed carry no flag.
    let agent_id = argv
        .iter()
        .position(|a| a == "--agent")
        .and_then(|i| argv.get(i + 1))
        .map(String::as_str)
        .unwrap_or(AGENT);
    let scope = argv
        .iter()
        .position(|a| a == "--scope")
        .and_then(|i| argv.get(i + 1))
        .and_then(|s| Scope::parse(s));
    let mut stdin = Vec::new();
    if std::io::stdin().read_to_end(&mut stdin).is_ok() {
        crate::hook::run(name, agent_id, scope, &stdin);
    }
    Ok(Outcome::new("hook"))
}

/// `shutil.which(name)`.
fn which(name: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let candidate = sys::join(dir, name);
        let Ok(meta) = fs::metadata(&candidate) else {
            continue;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Some(candidate);
            }
        }
        #[cfg(not(unix))]
        if meta.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn command_install(argv: &[String]) -> Run {
    let mut p = parser("install");
    p.option("--cwd")
        .flag("--local", None)
        .flag("--project", None)
        .option("--command")
        .option("--agent");
    let args = p.parse(argv)?;
    if args.flag("local") && args.flag("project") {
        return Err(p
            .usage_error("argument --project: not allowed with argument --local")
            .into());
    }
    // The user's hooks unless asked: one install covers every project, with
    // nothing written into any of them.
    let scope = if args.flag("local") {
        Scope::Local
    } else if args.flag("project") {
        Scope::Project
    } else {
        Scope::User
    };
    // Claude Code's alone unless asked: an install that also wrote Codex's
    // hooks would put loch in every Codex session on the machine, or drop a
    // .codex/ into repos that never use Codex, and leave a review prompt in
    // its next TUI either way.
    let agents = agents_arg(Some(args.str("agent").as_deref().unwrap_or(AGENT)))?;
    let cwd = args
        .str("cwd")
        .filter(|c| !c.is_empty())
        .unwrap_or_else(sys::getcwd);
    let root = snapshot::workspace_root(&cwd);
    let command = match args.str("command") {
        Some(command) => command,
        // A machine-specific path, which is why only the uncommitted settings
        // file gets one. The user's file names plain `loch` too, so it keeps
        // working when loch moves (an npm upgrade under another Node version),
        // at the price of depending on the agent's PATH.
        None if scope == Scope::Local => which("loch")
            .unwrap_or_else(|| sys::abspath(&std::env::args().next().unwrap_or_default())),
        None => "loch".into(),
    };
    let mut text = String::new();
    let mut results = Vec::new();
    for adapter in &agents {
        let settings = adapter.hooks_file(scope, &root);
        let installed = trace::install_hooks(&settings, &command, *adapter, scope)
            .map_err(|trace::InstallError::Internal(m)| Failure::Internal(m))?;
        if installed.added.is_empty() && installed.replaced.is_empty() {
            text.push_str(&format!("loch hooks already installed in {}\n", settings));
        } else {
            let mut events = installed.added.clone();
            events.extend(installed.replaced.iter().cloned());
            text.push_str(&format!(
                "installed loch hooks ({}) in {}\n",
                events.join(", "),
                settings
            ));
        }
        if !installed.removed.is_empty() {
            text.push_str(&format!(
                "removed old and duplicate loch hooks ({})\n",
                installed.removed.join(", ")
            ));
        }
        if !installed.duplicates.is_empty() {
            text.push_str(&format!(
                "left a second loch hook for {} where it is (each event is still handled once); remove it in {} if you like\n",
                installed.duplicates.join(", "),
                settings
            ));
        }
        text.push_str(&format!("{}\n", adapter.install_note(scope)));
        results.push((
            adapter.id(),
            settings,
            installed,
            adapter.hooks_need_review(),
        ));
    }
    text.push_str(match scope {
        Scope::User => "they run in every git repository\n",
        Scope::Project => "they run for everyone who works in this project\n",
        Scope::Local => "they run in this project, on this machine alone\n",
    });
    // The first agent's at the top level, as before there were several.
    let (_, settings, installed, _) = &results[0];
    let mut payload = object! {
        "scope" => scope.as_str(),
        "settings" => settings.as_str(),
        // Not `command`, which is the envelope's: the hooks' loch.
        "hookCommand" => command.as_str(),
        "added" => strings(&installed.added),
        "replaced" => strings(&installed.replaced),
        "removed" => strings(&installed.removed),
        "duplicates" => strings(&installed.duplicates),
    };
    payload.insert(
        "agents".into(),
        Value::Array(
            results
                .iter()
                .map(|(id, settings, installed, review)| {
                    let changed = !installed.added.is_empty() || !installed.replaced.is_empty();
                    Value::Object(object! {
                        "agent" => *id,
                        "scope" => scope.as_str(),
                        "settings" => settings.as_str(),
                        "added" => strings(&installed.added),
                        "replaced" => strings(&installed.replaced),
                        "removed" => strings(&installed.removed),
                        "duplicates" => strings(&installed.duplicates),
                        // The agent won't run a new or changed hook until a
                        // person approves it; loch never approves its own.
                        "needsReview" => *review && changed,
                    })
                })
                .collect(),
        ),
    );
    Ok(Outcome::with("install", payload, text))
}

// -- blame / log / ask -------------------------------------------------------------

/// The local store, which is all `blame`, `log`, `ask` and `backfill` read
/// or write: a remote's records arrive by `pull`, as git's do by `fetch`.
fn local() -> Option<Box<dyn Store>> {
    Some(Box::new(crate::store::local_store()))
}

fn flat(text: &str, limit: usize) -> String {
    let flat = py::split(text).join(" ");
    if py::len(&flat) <= limit {
        flat
    } else {
        format!("{}…", py::truncate(&flat, limit - 1))
    }
}

fn link_text(link: Option<&Link>) -> String {
    let Some(link) = link else {
        return "not written in a recorded turn".into();
    };
    let title = match &link.title {
        Some(t) if !t.is_empty() => format!(" \"{}\"", t),
        _ => String::new(),
    };
    let prompt = match &link.prompt {
        Some(p) if !p.is_empty() => format!(" — asked: \"{}\"", flat(p, 72)),
        _ => String::new(),
    };
    let missing = if link.transcript.is_none() {
        " (transcript not available)"
    } else {
        ""
    };
    format!(
        "{}  session {}{}{}{}",
        py::truncate(&link.ended, 10),
        py::truncate(&link.session_id, 8),
        title,
        prompt,
        missing
    )
}

fn mark(index: usize) -> String {
    let mut letters = String::new();
    let mut index = index + 1;
    while index > 0 {
        let remainder = (index - 1) % 26;
        index = (index - 1) / 26;
        letters.insert(0, (b'A' + remainder as u8) as char);
    }
    letters
}

/// A file as attribution sees it: its text, and the project and relative path
/// its turns were recorded under.
struct Located {
    text: String,
    project: String,
    relative: String,
    /// `relative` and every earlier name git knows the file by.
    names: Vec<String>,
    records: Vec<EditRecord>,
}

/// The names a file has had, newest first, by git's rename tracking — read
/// only, and only where there is a repository to ask.
fn past_names(root: &str, relative: &str) -> Vec<String> {
    let mut names = vec![relative.to_string()];
    if snapshot::has_git_marker(root) {
        let log = trace::git_unchecked(
            root,
            &[
                "log",
                "--follow",
                "--name-only",
                "--format=",
                "--",
                relative,
            ],
        );
        for name in log.lines().map(str::trim).filter(|n| !n.is_empty()) {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Every record that wrote the file under any of its names, one per session,
/// the one with more turns winning where two copies of a session meet.
fn records_named(store: Option<&dyn Store>, project: &str, names: &[String]) -> Vec<EditRecord> {
    let mut by_session: HashMap<String, EditRecord> = HashMap::new();
    for name in names {
        for record in attribution::records_for(store, project, name) {
            let better = by_session
                .get(&record.session)
                .is_none_or(|have| record.turns.len() > have.turns.len());
            if better {
                by_session.insert(record.session.clone(), record);
            }
        }
    }
    let mut records: Vec<EditRecord> = by_session.into_values().collect();
    records.sort_by(|a, b| a.session.cmp(&b.session));
    records
}

/// Finds the records that wrote `file`. In a git checkout the root is the
/// checkout and the project its root commit, so there is one answer. Anywhere
/// else a session's root was the directory it ran in, which may be any
/// ancestor of the file, so each is tried, nearest first, until one has
/// records; with none, the nearest is the answer.
fn locate(file: &str, store: Option<&dyn Store>) -> std::result::Result<Located, Failure> {
    let absolute = sys::abspath(file);
    let text = match fs::read(&absolute) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => {
            return err(
                "file-unreadable",
                format!("{}: {}", file, sys::os_error(&error, file)),
            )
        }
    };
    let real = sys::realpath_strict(&absolute).unwrap_or(absolute);
    let root = snapshot::workspace_root(&sys::dirname(&real));
    let mut candidates = vec![root.clone()];
    if !snapshot::has_git_marker(&root) {
        let mut dir = sys::dirname(&root);
        while dir.len() > 1 {
            candidates.push(dir.clone());
            dir = sys::dirname(&dir);
        }
    }
    let mut first = None;
    for candidate in candidates {
        let project = attribution::project_id(&candidate);
        let relative = sys::relpath(&real, &candidate);
        let names = past_names(&candidate, &relative);
        let records = records_named(store, &project, &names);
        if !records.is_empty() {
            return Ok(Located {
                text,
                project,
                relative,
                names,
                records,
            });
        }
        first.get_or_insert((project, relative, names));
    }
    let (project, relative, names) = first.expect("there is always one candidate");
    Ok(Located {
        text,
        project,
        relative,
        names,
        records: Vec::new(),
    })
}

struct Blamed {
    located: Located,
    /// Per line, an index into `links`.
    owners: Vec<Option<usize>>,
    links: Vec<Link>,
}

fn blame_file(file: &str, store: Option<&dyn Store>) -> std::result::Result<Blamed, Failure> {
    let located = locate(file, store)?;
    let owners = attribution::attribute(&located.text, &located.names, &located.records);
    let mut resolver = Resolver::new(store);
    let mut index_of: HashMap<attribution::TurnRef, usize> = HashMap::new();
    let mut links = Vec::new();
    let mut indices = Vec::with_capacity(owners.len());
    for owner in &owners {
        indices.push(match owner {
            None => None,
            Some(reference) => {
                if !index_of.contains_key(reference) {
                    let link = resolver
                        .describe(&located.records[reference.record], reference.turn)
                        .map_err(Failure::Internal)?;
                    index_of.insert(*reference, links.len());
                    links.push(link);
                }
                Some(index_of[reference])
            }
        });
    }
    Ok(Blamed {
        located,
        owners: indices,
        links,
    })
}

/// Consecutive lines with one owner, 1-based: (start, end, owner).
fn runs(owners: &[Option<usize>]) -> Vec<(usize, usize, Option<usize>)> {
    let mut runs: Vec<(usize, usize, Option<usize>)> = Vec::new();
    for (i, owner) in owners.iter().enumerate() {
        match runs.last_mut() {
            Some(last) if last.2 == *owner => last.1 = i + 1,
            _ => runs.push((i + 1, i + 1, *owner)),
        }
    }
    runs
}

pub fn command_blame(argv: &[String]) -> Run {
    let mut p = parser("blame");
    p.positional("file", Nargs::One)
        .option_optional("--html", "", "PATH")
        .option_metavar("--lines", "RANGES")
        .flag("--no-open", None);
    let args = p.parse(argv)?;
    let file = args.str("file").unwrap_or_default();
    let store = local();
    let blamed = blame_file(&file, store.as_deref())?;

    // One mark per turn, handed out in order of first appearance in the file,
    // so A is always the first conversation you read.
    let marks: Vec<String> = (0..blamed.links.len()).map(mark).collect();
    let legend: Vec<Value> = blamed
        .links
        .iter()
        .zip(&marks)
        .map(|(link, letter)| {
            let mut entry = object! {"mark" => letter.as_str()};
            entry.extend(link.payload());
            Value::Object(entry)
        })
        .collect();
    let spans = runs(&blamed.owners);
    let rows: Vec<Value> = spans
        .iter()
        .map(|&(start, end, owner)| {
            Value::Object(object! {
                "start" => start,
                "end" => end,
                "mark" => owner.map_or("·", |o| marks[o].as_str()),
                "conversation" => owner.map_or(Value::Null, |o| Value::Object(blamed.links[o].payload())),
            })
        })
        .collect();

    let mut text = format!("{}\n", file);
    for &(start, end, owner) in &spans {
        let span = if end > start {
            format!("{}-{}", start, end)
        } else {
            start.to_string()
        };
        text.push_str(&format!(
            "  {}  {}\n",
            right(&span, 9),
            link_text(owner.map(|o| &blamed.links[o]))
        ));
    }
    let attributed = blamed.owners.iter().filter(|o| o.is_some()).count();
    text.push_str(&format!(
        "\n{} of {} line(s) written in a recorded turn; follow one up with: loch ask {}:<line>\n",
        attributed,
        blamed.owners.len(),
        file
    ));

    let mut payload = object! {
        "file" => file.as_str(),
        "path" => blamed.located.relative.as_str(),
        "project" => blamed.located.project.as_str(),
        "conversations" => legend.clone(),
        "hunks" => rows.clone(),
    };
    if let Some(html) = args.str("html") {
        let visible = match args.str("lines").filter(|l| !l.is_empty()) {
            Some(spec) => match blame_html::parse_ranges(&spec) {
                Some(ranges) => Some(ranges),
                None => {
                    return err(
                        "usage",
                        format!("--lines takes ranges like 1-12,99-110, not {}", spec),
                    )
                }
            },
            None => None,
        };
        let lines: HashMap<usize, String> = blamed
            .located
            .text
            .lines()
            .enumerate()
            .map(|(i, line)| (i + 1, line.to_string()))
            .collect();
        let page = blame_html::render(&file, &legend, &rows, &lines, visible.as_deref());
        let target = if html.is_empty() {
            sys::join(
                &sys::gettempdir(),
                &format!("loch-blame-{}.html", sys::basename(&file)),
            )
        } else {
            html
        };
        fs::write(&target, page.as_bytes())
            .map_err(|e| Failure::Internal(sys::os_error(&e, &target)))?;
        let absolute = sys::abspath(&target);
        text = format!("wrote {}\n", absolute);
        payload.insert("html".into(), absolute.as_str().into());
        if !args.flag("no_open") && !json_requested() {
            open_in_browser(&sys::realpath_strict(&target).unwrap_or(absolute));
        }
    }
    Ok(Outcome::with("blame", payload, text))
}

fn open_in_browser(path: &str) {
    let uri = format!("file://{}", path.replace(' ', "%20"));
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(uri)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `file`, `file:105` or `file:100-120`; a path that exists wins outright.
fn line_target(target: &str) -> std::result::Result<(String, Option<(usize, usize)>), Failure> {
    if Path::new(target).exists() {
        return Ok((target.into(), None));
    }
    let (file, span) = target.rsplit_once(':').unwrap_or(("", target));
    let (start, end) = span.split_once('-').unwrap_or((span, ""));
    if file.is_empty() || !py::is_digits(start) || (!end.is_empty() && !py::is_digits(end)) {
        return err(
            "usage",
            format!("{} is neither a file nor <file>:<line>[-<end>]", target),
        );
    }
    let first: usize = start.parse().unwrap_or(0);
    let last: usize = if end.is_empty() {
        first
    } else {
        end.parse().unwrap_or(0)
    };
    if first < 1 || last < first {
        return err("usage", format!("{} is not a line range", span));
    }
    Ok((file.into(), Some((first, last))))
}

pub fn command_log(argv: &[String]) -> Run {
    let mut p = parser("log");
    p.positional("target", Nargs::One);
    let args = p.parse(argv)?;
    let (file, lines) = line_target(&args.str("target").unwrap_or_default())?;
    let store = local();
    let blamed = blame_file(&file, store.as_deref())?;
    if let Some((_, last)) = lines {
        if last > blamed.owners.len() {
            return err(
                "usage",
                format!("{} has {} lines, not {}", file, blamed.owners.len(), last),
            );
        }
    }

    // A range asks who wrote what is there now; a whole file asks every turn
    // that ever changed it, including ones whose lines are gone.
    let mut resolver = Resolver::new(store.as_deref());
    let mut links: Vec<Link> = match lines {
        Some((first, last)) => {
            let mut seen = Vec::new();
            for owner in blamed.owners[first - 1..last].iter().flatten() {
                if !seen.contains(owner) {
                    seen.push(*owner);
                }
            }
            seen.into_iter().map(|o| blamed.links[o].clone()).collect()
        }
        None => {
            let mut all = Vec::new();
            for record in &blamed.located.records {
                for (index, turn) in record.turns.iter().enumerate() {
                    if turn
                        .files
                        .iter()
                        .any(|f| blamed.located.names.contains(&f.path))
                    {
                        all.push(
                            resolver
                                .describe(record, index)
                                .map_err(Failure::Internal)?,
                        );
                    }
                }
            }
            all
        }
    };
    links.sort_by(|a, b| b.ended.cmp(&a.ended));

    let span = match lines {
        None => String::new(),
        Some((a, b)) if a == b => format!(":{}", a),
        Some((a, b)) => format!(":{}-{}", a, b),
    };
    let mut text = format!("{}{}: {} turn(s), newest first\n", file, span, links.len());
    for link in &links {
        text.push_str(&format!("  {}\n", link_text(Some(link))));
    }
    if !links.is_empty() {
        text.push_str(&format!("\nfollow one up with: loch ask {}:<line>\n", file));
    }
    Ok(Outcome::with(
        "log",
        object! {
            "file" => file.as_str(),
            "lines" => lines.map_or(Value::Null, |(a, b)| Value::Object(object! {"start" => a, "end" => b})),
            "conversations" => links.iter().map(|l| Value::Object(l.payload())).collect::<Vec<_>>(),
        },
        text,
    ))
}

pub fn command_ask(argv: &[String]) -> Run {
    let mut p = parser("ask");
    p.positional("target", Nargs::One)
        .positional("question", Nargs::Optional)
        .flag("--at-turn", None)
        .flag("--no-launch", None);
    let args = p.parse(argv)?;
    let as_json = json_requested();
    let target = args.str("target").unwrap_or_default();
    let (file, line) = match line_target(&target)? {
        (file, Some((line, _))) => (file, line),
        (_, None) => return err("usage", format!("{} is not <file>:<line>", target)),
    };
    let store = local();
    let blamed = blame_file(&file, store.as_deref())?;
    if line > blamed.owners.len() {
        return err(
            "usage",
            format!("{} has {} lines, not {}", file, blamed.owners.len(), line),
        );
    }
    let Some(owner) = blamed.owners[line - 1] else {
        return err(
            "not-linked",
            format!("{} was not written in a recorded turn", target),
        );
    };
    let link = blamed.links[owner].clone();
    let Some(adapter) = agent::by_id(&link.agent) else {
        return err(
            "unsupported-agent",
            format!(
                "session {} wrote it, but this build doesn't know its agent, {}",
                link.session_id, link.agent
            ),
        );
    };
    let Some(transcript) = &link.transcript else {
        return err(
            "transcript-missing",
            format!(
                "session {} wrote it, but its transcript is neither in {} nor in the local store",
                link.session_id,
                adapter.sessions_dir()
            ),
        );
    };
    let at_turn = args.flag("at_turn");
    let cwd = blamed
        .located
        .records
        .iter()
        .find(|r| r.session == link.session_id)
        .map(|r| r.root.clone())
        .filter(|root| Path::new(root).is_dir())
        .unwrap_or_else(sys::getcwd);
    let fork = adapter
        .fork(
            transcript,
            &cwd,
            if at_turn {
                link.end_uuid.as_deref()
            } else {
                None
            },
            Some(&format!("loch ask {}", target)),
        )
        .map_err(|e| match e {
            trace::ForkError::Trace(t) => Failure::from(t),
            trace::ForkError::Internal(m) => Failure::Internal(m),
            trace::ForkError::Unsupported(name) => Failure::Error(CommandError::new(
                "unsupported-agent",
                format!("{} conversations can't be forked", name),
            )),
        })?;

    let Some(mut command) = adapter.resume_command(&fork.session_id) else {
        return err(
            "unsupported-agent",
            format!("{} sessions can't be resumed", adapter.name()),
        );
    };
    if let Some(question) = args.str("question").filter(|q| !q.is_empty()) {
        command.push(question);
    }
    let payload = object! {
        "target" => target.as_str(),
        "conversation" => link.payload(),
        "fork" => object! {
            "sessionId" => fork.session_id.as_str(),
            "path" => fork.path.as_str(),
            "cwd" => fork.cwd.as_str(),
            "records" => fork.records,
        },
        "at" => if at_turn { "turn" } else { "end" },
        "launch" => strings(&command),
    };
    let text = format!(
        "forked session {}, which wrote {}, {}: {} records, as {}\n",
        py::truncate(&fork.origin_session, 8),
        target,
        if at_turn {
            "at the end of that turn"
        } else {
            "to its end"
        },
        fork.records,
        fork.session_id
    );
    if args.flag("no_launch") || as_json {
        return Ok(Outcome::with(
            "ask",
            payload,
            format!(
                "{}resume with: cd {} && {}\n",
                text,
                fork.cwd,
                command.join(" ")
            ),
        ));
    }
    eprint!("{}", text);
    exec_agent(&fork.cwd, &command)
}

#[cfg(unix)]
fn exec_agent(root: &str, command: &[String]) -> Run {
    use std::os::unix::process::CommandExt;
    let _ = std::env::set_current_dir(root);
    let error = std::process::Command::new(&command[0])
        .args(&command[1..])
        .exec();
    let code = error.raw_os_error().unwrap_or(0);
    Err(Failure::Internal(format!(
        "[Errno {}] {}: '{}'",
        code,
        sys::strerror(code),
        command[0]
    )))
}

#[cfg(not(unix))]
fn exec_agent(root: &str, command: &[String]) -> Run {
    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .current_dir(root)
        .status()
        .map_err(|e| Failure::Internal(e.to_string()))?;
    std::process::exit(status.code().unwrap_or(1));
}

// -- backfill ----------------------------------------------------------------------

/// Reconstructs attribution for turns from before the hooks, from the
/// transcripts' edit calls and the commits the agent made.
pub fn command_backfill(argv: &[String]) -> Run {
    let mut p = parser("backfill");
    p.option("--cwd");
    let args = p.parse(argv)?;
    let cwd = args
        .str("cwd")
        .filter(|c| !c.is_empty())
        .unwrap_or_else(sys::getcwd);
    let root = snapshot::workspace_root(&cwd);
    let store = local();
    let results = crate::backfill::backfill(&cwd, store.as_deref());
    let mut text = String::new();
    for r in &results {
        text.push_str(&format!(
            "  session {}  {} turn(s): {} line(s) from edits, {} from {} commit(s)\n",
            py::truncate(&r.session, 8),
            r.turns,
            r.edit_lines,
            r.commit_lines,
            r.commits
        ));
    }
    let turns: usize = results.iter().map(|r| r.turns).sum();
    text.push_str(&format!(
        "backfilled {} turn(s) across {} session(s) in {}\n",
        turns,
        results.len(),
        root
    ));
    Ok(Outcome::with(
        "backfill",
        object! {
            "root" => root.as_str(),
            "project" => attribution::project_id(&root),
            "turns" => turns,
            "sessions" => results.iter().map(|r| Value::Object(object! {
                "session" => r.session.as_str(),
                "root" => r.root.as_str(),
                "turns" => r.turns,
                "editLines" => r.edit_lines,
                "commitLines" => r.commit_lines,
                "commits" => r.commits,
            })).collect::<Vec<_>>(),
        },
        text,
    ))
}

// -- remotes, as the store commands name them ------------------------------------------

/// `--remote [<name>]`, or the older `--store <uri>`, which means the same:
/// `None` when neither was given, `Some(None)` for the default remote.
fn remote_asked(args: &crate::argparse::Args) -> Option<Option<String>> {
    if let Some(uri) = args.str("store").filter(|s| !s.is_empty()) {
        return Some(Some(uri));
    }
    args.str("remote")
        .map(|r| Some(r).filter(|r| !r.is_empty()))
}

/// The ref and the remote of `restore`/`delete`. `--remote` takes an
/// optional value, so `delete --remote <ref>` hands it the ref, as argparse
/// does; no remote can be named like a ref, so it is given back.
fn ref_and_remote(args: &crate::argparse::Args) -> (Option<String>, Option<Option<String>>) {
    let ref_ = args.str("ref").filter(|r| !r.is_empty());
    let asked = remote_asked(args);
    match (&ref_, &asked) {
        (None, Some(Some(name))) if remote::looks_like_ref(name) && args.str("store").is_none() => {
            (Some(name.clone()), Some(None))
        }
        _ => (ref_, asked),
    }
}

/// A bundle this machine doesn't have, from the remote `asked` names (or the
/// default one), checked against its ref.
fn fetch_bundle(
    ref_: &str,
    asked: Option<Option<String>>,
) -> std::result::Result<(Remote, Vec<u8>), Failure> {
    let resolved = remote::resolve(asked.clone().flatten().as_deref());
    let (remote, opened) = match resolved {
        // Nothing to fetch from, and nobody asked for a remote: the bundle
        // simply isn't here.
        Err(e) if asked.is_none() && e.code == "no-remote" => {
            return err(
                "bundle-not-found",
                format!(
                    "{} is not on this machine, and there is no remote to fetch it from",
                    ref_
                ),
            )
        }
        Err(e) => return Err(remote_error(e)),
        Ok(remote) => {
            let opened = remote.open().map_err(remote_error)?;
            (remote, opened)
        }
    };
    let described = format!("could not read {} from {}", ref_, remote.label());
    let packed = opened
        .get(&index::bundle_key(ref_))
        .map_err(|e| store_error(&described, e))?;
    if bundle::bundle_ref(&packed) != ref_ {
        return err(
            "bundle-unreadable",
            format!("{}: its bytes don't hash to the ref", described),
        );
    }
    Ok((remote, packed))
}

// -- push / pull ----------------------------------------------------------------------

fn sync_error(remote: &Remote, error: SyncError) -> Failure {
    match error {
        SyncError::Store(e) => store_error(&format!("could not reach {}", remote.label()), e),
        SyncError::Redaction(message) => {
            Failure::Error(CommandError::new("redaction-failed", message))
        }
    }
}

/// The remote a command named, opened, and refused if it is this machine's
/// own store: pushing there would mark everything sent and send nothing.
fn open_remote(asked: Option<&str>) -> std::result::Result<(Remote, Box<dyn Store>), Failure> {
    let remote = remote::resolve(asked).map_err(remote_error)?;
    let opened = remote.open().map_err(remote_error)?;
    if sys::abspath(remote.uri.trim_start_matches("file://"))
        == sys::abspath(&crate::store::default_store())
    {
        return err(
            "invalid-uri",
            format!("{} is this machine's own store, not a remote", remote.uri),
        );
    }
    Ok((remote, opened))
}

/// The project a sync covers: the one `--cwd` (or the current directory) is
/// in, or every project with `--all`.
fn sync_scope(args: &crate::argparse::Args) -> Option<String> {
    if args.flag("all") {
        return None;
    }
    let cwd = args
        .str("cwd")
        .filter(|c| !c.is_empty())
        .unwrap_or_else(sys::getcwd);
    Some(attribution::project_id(&snapshot::workspace_root(&cwd)))
}

fn with_remote(mut payload: crate::json::Map, remote: &Remote) -> crate::json::Map {
    payload.insert("remote".into(), remote.payload());
    if remote.deprecated {
        payload.insert("deprecated".into(), Value::str("LOCH_STORE"));
    }
    payload
}

pub fn command_push(argv: &[String]) -> Run {
    let mut p = parser("push");
    p.positional("remote", Nargs::Optional)
        .option("--cwd")
        .flag("--all", None)
        .flag("--dry-run", None);
    let args = p.parse(argv)?;
    let (remote, opened) = open_remote(args.str("remote").as_deref())?;
    let project = sync_scope(&args);
    let local = crate::store::local_store();
    let mut report = sync::PushReport::new(args.flag("dry_run"));
    let scope = match &project {
        Some(project) => sync::Scope::Project(project),
        None => sync::Scope::All,
    };
    let result = sync::push(&local, opened.as_ref(), &remote.uri, scope, &mut report);
    let mut payload = report.payload();
    payload.insert("project".into(), Value::opt_str(project.as_deref()));
    let payload = with_remote(payload, &remote);
    if let Err(error) = result {
        return Err(match sync_error(&remote, error) {
            Failure::Error(mut failure) => {
                failure.payload = payload;
                Failure::Error(failure)
            }
            other => other,
        });
    }
    let mut outcome = Outcome::with(
        "push",
        payload,
        format!("{}: {}", remote.label(), report.text()),
    );
    outcome.warnings.extend(remote.warning());
    Ok(outcome)
}

pub fn command_pull(argv: &[String]) -> Run {
    let mut p = parser("pull");
    p.positional("remote", Nargs::Optional)
        .option("--cwd")
        .flag("--all", None);
    let args = p.parse(argv)?;
    let (remote, opened) = open_remote(args.str("remote").as_deref())?;
    let project = sync_scope(&args);
    let local = crate::store::local_store();
    let mut report = sync::PullReport::default();
    let result = sync::pull(&local, opened.as_ref(), project.as_deref(), &mut report);
    let mut payload = report.payload();
    payload.insert("project".into(), Value::opt_str(project.as_deref()));
    let payload = with_remote(payload, &remote);
    if let Err(error) = result {
        return Err(match sync_error(&remote, error) {
            Failure::Error(mut failure) => {
                failure.payload = payload;
                Failure::Error(failure)
            }
            other => other,
        });
    }
    let mut outcome = Outcome::with(
        "pull",
        payload,
        format!("{}: {}", remote.label(), report.text()),
    );
    outcome.warnings.extend(remote.warning());
    Ok(outcome)
}

// -- remote ---------------------------------------------------------------------------

fn remote_error(error: RemoteError) -> Failure {
    let mut failure = CommandError::new(error.code, error.message);
    if error.code == "usage" {
        failure.exit_status = 2;
    }
    Failure::Error(failure)
}

/// `remote [list]`, `remote add <name> <uri>`, `remote remove <name>`.
pub fn command_remote(argv: &[String]) -> Run {
    let (sub, rest) = match argv.split_first() {
        Some((sub, rest)) if !sub.starts_with('-') => (sub.as_str(), rest),
        _ => ("list", argv),
    };
    match sub {
        "add" => {
            let mut p = parser("remote add");
            p.positional("name", Nargs::One)
                .positional("uri", Nargs::One);
            let args = p.parse(rest)?;
            let added = remote::add(
                &args.str("name").unwrap_or_default(),
                &args.str("uri").unwrap_or_default(),
            )
            .map_err(remote_error)?;
            Ok(Outcome::with(
                "remote",
                object! {"action" => "add", "remote" => added.payload()},
                format!("added remote {} {}\n", added.label(), added.uri),
            ))
        }
        "remove" => {
            let mut p = parser("remote remove");
            p.positional("name", Nargs::One);
            let args = p.parse(rest)?;
            let removed =
                remote::remove(&args.str("name").unwrap_or_default()).map_err(remote_error)?;
            Ok(Outcome::with(
                "remote",
                object! {"action" => "remove", "remote" => removed.payload()},
                format!("removed remote {} {}\n", removed.label(), removed.uri),
            ))
        }
        "list" => {
            parser("remote list").parse(rest)?;
            let remotes = remote::remotes().map_err(remote_error)?;
            let default = remote::default_name(&remotes);
            let mut text = String::new();
            for (name, uri) in &remotes {
                let marker = if default.as_deref() == Some(name) {
                    "  (default)"
                } else {
                    ""
                };
                text.push_str(&format!(
                    "{}  {}{}\n",
                    name,
                    uri.as_str().unwrap_or_default(),
                    marker
                ));
            }
            if remotes.is_empty() {
                text.push_str("no remotes; `loch remote add origin <uri>`\n");
            }
            Ok(Outcome::with(
                "remote",
                object! {
                    "action" => "list",
                    "remotes" => remotes.iter().map(|(name, uri)| Value::Object(object! {
                        "name" => name.as_str(),
                        "uri" => uri.clone(),
                    })).collect::<Vec<_>>(),
                    "default" => Value::opt_str(default.as_deref()),
                },
                text,
            ))
        }
        other => {
            let mut error = CommandError::new(
                "usage",
                format!("unknown remote action {:?} (add, remove, list)", other),
            );
            error.exit_status = 2;
            Err(Failure::Error(error))
        }
    }
}

// -- gc ------------------------------------------------------------------------------

/// Deletes the snapshots nothing needs: the hooks do this on their own every
/// `COLLECT_EVERY` snapshots, and this runs it now.
pub fn command_gc(argv: &[String]) -> Run {
    let p = parser("gc");
    p.parse(argv)?;
    let collected = crate::hook::collect().map_err(|e| Failure::Internal(e.0))?;
    Ok(Outcome::with(
        "gc",
        object! {
            "kept" => collected.kept,
            "removed" => collected.removed,
            "bytes" => collected.bytes as i64,
        },
        format!(
            "removed {} object(s), {}; kept {}\n",
            collected.removed,
            format_bytes(collected.bytes),
            collected.kept
        ),
    ))
}

// -- help / version -----------------------------------------------------------------

pub fn command_help(_argv: &[String]) -> Run {
    Ok(Outcome::with(
        "help",
        object! {"usage" => USAGE},
        USAGE.into(),
    ))
}

pub fn command_version(_argv: &[String]) -> Run {
    let version = env!("CARGO_PKG_VERSION");
    Ok(Outcome::with(
        "version",
        object! {"version" => version},
        format!("loch {}\n", version),
    ))
}

pub fn dispatch(command: &str, argv: &[String]) -> Option<Run> {
    Some(match command {
        "list" => command_list(argv),
        "save" => command_save(argv),
        "restore" => command_restore(argv),
        "delete" => command_delete(argv),
        "reindex" => command_reindex(argv),
        "ingest" => command_ingest(argv),
        "install" => command_install(argv),
        "blame" => command_blame(argv),
        "ask" => command_ask(argv),
        "log" => command_log(argv),
        "hook" => command_hook(argv),
        "gc" => command_gc(argv),
        "remote" => command_remote(argv),
        "push" => command_push(argv),
        "pull" => command_pull(argv),
        "backfill" => command_backfill(argv),
        "help" => command_help(argv),
        "version" => command_version(argv),
        _ => return None,
    })
}
