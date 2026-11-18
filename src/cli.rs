//! Every command: each builds one `Outcome` — payload
//! and prose together — and `main` renders it.

use crate::argparse::{Exit, Nargs, Parser};
use crate::blame_html;
use crate::bundle::{self, Bundle, BundleOrigin, BundleSession, BUNDLE_VERSION};
use crate::claude::{
    self, home_dir, list_sessions, resolve_cwd, transcript_path_for, SessionMeta, AGENT,
};
use crate::format;
use crate::index::{self, IndexEntry, IndexError, BRANCH};
use crate::ingest::{self, Origin};
use crate::json::{Map, Value};
use crate::object;
use crate::output::{fail, CommandError, Outcome, JSON_FLAG};
use crate::py;
use crate::redact;
use crate::rewrite::{self, ForeignCwd, RewriteSpec};
use crate::store::{create_store, resolve_store_uri, Store, StoreError};
use crate::sys;
use crate::trace::{self, Link, Resolver, TraceError, UNCOMMITTED};
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

fn open_store(uri: Option<&str>) -> std::result::Result<Box<dyn Store>, Failure> {
    create_store(&resolve_store_uri(uri)).map_err(|e| Failure::Internal(e.0))
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

fn left(text: &str, width: usize) -> String {
    let len = py::len(text);
    format!("{}{}", text, " ".repeat(width.saturating_sub(len)))
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
    let version = match &entry.claude_version {
        Some(v) if !v.is_empty() => format!("  claude {}", v),
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

fn collect(
    cwd: Option<&str>,
    branch: Option<&str>,
    session: Option<&str>,
) -> (String, Vec<SessionMeta>) {
    let resolved = resolve_cwd(&cwd.map(str::to_string).unwrap_or_else(sys::getcwd));
    let sessions = list_sessions(&resolved, branch, session, None);
    (resolved, sessions)
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
        "path" => meta.path.as_str(),
        "cwd" => meta.cwd.as_str(),
        "otherCwds" => strings(&meta.other_cwds),
        "branch" => Value::opt_str(meta.git_branch.as_deref()),
        "claudeVersion" => Value::opt_str(meta.claude_version.as_deref()),
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
    })
}

// -- list ---------------------------------------------------------------------

pub fn command_list(argv: &[String]) -> Run {
    let mut p = parser("list");
    p.option("--cwd")
        .option("--branch")
        .flag("--remote", None)
        .flag("--all", Some("every"))
        .option("--store");
    let args = p.parse(argv)?;
    let (cwd, branch, store) = (args.str("cwd"), args.str("branch"), args.str("store"));
    if args.flag("remote") || args.flag("every") || store.is_some() {
        return list_remote(cwd, branch, store, args.flag("every"));
    }
    let (cwd, sessions) = collect(cwd.as_deref(), branch.as_deref(), None);
    let text = if sessions.is_empty() {
        format!("no {} sessions found for {}\n", AGENT, cwd)
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

fn list_remote(
    cwd: Option<String>,
    branch: Option<String>,
    uri: Option<String>,
    every: bool,
) -> Run {
    let store = open_store(uri.as_deref())?;
    let cwd = resolve_cwd(&cwd.unwrap_or_else(sys::getcwd));
    let branch = branch.or_else(|| current_branch(&cwd));
    let prefix = if every {
        index::dimension_prefix(BRANCH)
    } else {
        index::value_prefix(BRANCH, branch.as_deref())
    };
    let described = store.describe();
    let entries = index::load_entries(store.as_ref(), &prefix)
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
    Ok(Outcome::with(
        "list",
        object! {
            "remote" => true,
            "store" => described.as_str(),
            "all" => every,
            "branch" => if every { Value::Null } else { Value::opt_str(branch.as_deref()) },
            "bundles" => refs.len(),
            "entries" => entries.iter().map(entry_payload).collect::<Vec<_>>(),
        },
        text,
    ))
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
        .option("--store");
    let args = p.parse(argv)?;
    let (cwd, sessions) = collect(
        args.str("cwd").as_deref(),
        args.str("branch").as_deref(),
        args.str("session").as_deref(),
    );
    if sessions.is_empty() {
        return err(
            "no-sessions",
            format!("no {} sessions found for {}", AGENT, cwd),
        );
    }
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
        packed_sessions.push(BundleSession {
            session_id: meta.session_id.clone(),
            cwd: meta.cwd.clone(),
            git_branch: meta.git_branch.clone(),
            claude_version: meta.claude_version.clone(),
            modified_at: meta.modified_at.clone(),
            transcript: scrubbed.text,
        });
        redactions.push(scrubbed.counts);
    }
    let bundle = Bundle {
        version: BUNDLE_VERSION,
        agent: AGENT.into(),
        created_at: sys::iso_timestamp(sys::creation_time()),
        origin: BundleOrigin {
            home: home_dir(),
            platform: sys::platform_name().into(),
            hostname: sys::hostname(),
        },
        sessions: packed_sessions,
    };
    let packed = bundle::pack_bundle(&bundle).map_err(|e| Failure::Internal(e.0))?;
    let ref_ = bundle::bundle_ref(&packed);
    let store = open_store(args.str("store").as_deref())?;
    let described = format!("could not write to {}", store.describe());
    // Bundle first: an index entry is derived from it.
    store
        .put(&index::bundle_key(&ref_), &packed)
        .map_err(|e| store_error(&described, e))?;
    let entries = index::index_bundle(store.as_ref(), &bundle, &ref_, packed.len())
        .map_err(|e| store_error(&described, e))?;

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
    text.push_str(&format!("ref {}\n", ref_));

    Ok(Outcome::with(
        "save",
        object! {
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
                "branch" => Value::opt_str(s.git_branch.as_deref()),
                "cwd" => s.cwd.as_str(),
                "claudeVersion" => Value::opt_str(s.claude_version.as_deref()),
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
    ))
}

// -- restore ------------------------------------------------------------------

struct Restored {
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
        .option("--store")
        .flag("--force", None)
        .flag("--new-id", Some("new_id"));
    let args = p.parse(argv)?;
    let Some(ref_) = args.str("ref").filter(|r| !r.is_empty()) else {
        return err("missing-ref", "restore requires a bundle ref");
    };
    let store = open_store(args.str("store").as_deref())?;
    let described = format!("could not read {} from {}", ref_, store.describe());
    let packed = store
        .get(&index::bundle_key(&ref_))
        .map_err(|e| store_error(&described, e))?;
    let bundle = match bundle::unpack_bundle(&packed) {
        Ok(bundle) => bundle,
        // Fetched fine and still unusable: neither absent nor unreachable.
        Err(error) => return err("bundle-unreadable", format!("{}: {}", described, error)),
    };
    let target_cwd = resolve_cwd(&args.str("into").unwrap_or_else(sys::getcwd));
    let target_home = home_dir();
    let mut restored = Vec::new();
    for session in &bundle.sessions {
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
        let destination = transcript_path_for(&target_cwd, &target_session_id, None);
        if Path::new(&destination).exists() && !args.flag("force") {
            restored.push(Restored {
                session_id: target_session_id,
                origin_session_id: session.session_id.clone(),
                branch: session.git_branch.clone(),
                status: "skipped",
                path: destination,
                residual: Vec::new(),
                resume_command: None,
                foreign: Vec::new(),
            });
            continue;
        }
        let parent = sys::dirname(&destination);
        fs::create_dir_all(&parent).map_err(|e| Failure::Internal(sys::os_error(&e, &parent)))?;
        fs::write(&destination, transcript.as_bytes())
            .map_err(|e| Failure::Internal(sys::os_error(&e, &destination)))?;
        restored.push(Restored {
            session_id: target_session_id.clone(),
            origin_session_id: session.session_id.clone(),
            branch: session.git_branch.clone(),
            status: "restored",
            path: destination,
            residual: rewrite::residual_origin_paths(&transcript, &spec),
            resume_command: Some(format!(
                "cd {} && claude --resume {}",
                target_cwd, target_session_id
            )),
            // Pre-rewrite: the origin path only exists losslessly here.
            foreign: rewrite::foreign_cwds(&session.transcript, &spec),
        });
    }

    let payload = object! {
        "ref" => ref_.as_str(),
        "store" => store.describe(),
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
        .filter(|s| s.status == "skipped")
        .map(|s| {
            format!(
                "  skipped {} (already exists; --force to overwrite)\n",
                s.session_id
            )
        })
        .collect();
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
    p.positional("ref", Nargs::Optional).option("--store");
    let args = p.parse(argv)?;
    let Some(ref_) = args.str("ref").filter(|r| !r.is_empty()) else {
        return err("missing-ref", "delete requires a bundle ref");
    };
    let store = open_store(args.str("store").as_deref())?;
    let removed = index::delete_bundle(store.as_ref(), &ref_).map_err(|e| {
        index_error(
            &format!("could not delete {} from {}", ref_, store.describe()),
            e,
        )
    })?;
    let mut text: String = removed
        .iter()
        .map(|k| format!("  removed {}\n", k))
        .collect();
    text.push_str(&format!("\ndeleted {} from {}\n", ref_, store.describe()));
    Ok(Outcome::with(
        "delete",
        object! {"ref" => ref_.as_str(), "store" => store.describe(), "removed" => strings(&removed)},
        text,
    ))
}

pub fn command_reindex(argv: &[String]) -> Run {
    let mut p = parser("reindex");
    p.option("--store");
    let args = p.parse(argv)?;
    let store = open_store(args.str("store").as_deref())?;
    let result = index::reindex(store.as_ref())
        .map_err(|e| index_error(&format!("could not reindex {}", store.describe()), e))?;
    Ok(Outcome::with(
        "reindex",
        object! {
            "store" => store.describe(),
            "bundles" => result.bundles,
            "entries" => result.entries,
            "removed" => result.removed,
        },
        format!(
            "scanned {} bundle(s) in {}\nwrote {} index entries, removed {} stale\n",
            result.bundles,
            store.describe(),
            result.entries,
            result.removed
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
    let (cwd, sessions) = collect(
        args.str("cwd").as_deref(),
        args.str("branch").as_deref(),
        args.str("session").as_deref(),
    );
    if sessions.is_empty() {
        return err(
            "no-sessions",
            format!("no {} sessions found for {}", AGENT, cwd),
        );
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
    let mut stdin = Vec::new();
    if std::io::stdin().read_to_end(&mut stdin).is_ok() {
        crate::hook::run(name, &stdin);
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
    p.option("--cwd").flag("--local", None).option("--command");
    let args = p.parse(argv)?;
    let root = trace::repo_root(
        &args
            .str("cwd")
            .filter(|c| !c.is_empty())
            .unwrap_or_else(sys::getcwd),
    )?;
    let command = match args.str("command") {
        Some(command) => command,
        // A machine-specific path, which is why it only goes in the
        // uncommitted settings file.
        None if args.flag("local") => which("loch")
            .unwrap_or_else(|| sys::abspath(&std::env::args().next().unwrap_or_default())),
        None => "loch".into(),
    };
    let name = if args.flag("local") {
        "settings.local.json"
    } else {
        "settings.json"
    };
    let settings = sys::join(&sys::join(&root, ".claude"), name);
    let added = trace::install_hooks(&settings, &command)
        .map_err(|trace::InstallError::Internal(m)| Failure::Internal(m))?;
    // Amend and rebase copy a commit's note to its replacement only when told
    // to, and an amended commit is still the one the conversation made.
    trace::git(&root, &["config", "notes.rewriteRef", trace::NOTES_REF])?;
    let mut text = if added.is_empty() {
        format!("loch hooks already installed in {}\n", settings)
    } else {
        format!(
            "installed loch hooks ({}) in {}\n",
            added.join(", "),
            settings
        )
    };
    text.push_str(&format!(
        "restart Claude Code sessions in this repository to pick them up\nshare links with: git push <remote> {}\n",
        trace::NOTES_REF
    ));
    Ok(Outcome::with(
        "install",
        object! {"settings" => settings.as_str(), "command" => command.as_str(), "added" => strings(&added)},
        text,
    ))
}

// -- blame / log / ask -------------------------------------------------------------

fn store_or_none(uri: Option<&str>) -> Option<Box<dyn Store>> {
    create_store(&resolve_store_uri(uri)).ok()
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
        return "no conversation recorded".into();
    };
    if link.transcript.is_none() {
        return format!(
            "session {} (transcript not available)",
            py::truncate(&link.session_id, 8)
        );
    }
    let title = match &link.title {
        Some(t) if !t.is_empty() => format!(" \"{}\"", t),
        _ => String::new(),
    };
    let prompt = match &link.prompt {
        Some(p) if !p.is_empty() => format!(" — asked: \"{}\"", flat(p, 72)),
        _ => String::new(),
    };
    format!(
        "session {}{}{}",
        py::truncate(&link.session_id, 8),
        title,
        prompt
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

fn resolve(resolver: &mut Resolver, commit: &str) -> std::result::Result<Option<Link>, Failure> {
    resolver.resolve(commit).map_err(Failure::Internal)
}

pub fn command_blame(argv: &[String]) -> Run {
    let mut p = parser("blame");
    p.positional("file", Nargs::One)
        .option_optional("--html", "", "PATH")
        .option_metavar("--lines", "RANGES")
        .flag("--no-open", None)
        .option("--store");
    let args = p.parse(argv)?;
    let file = args.str("file").unwrap_or_default();
    let absolute = sys::abspath(&file);
    let cwd = {
        let dir = sys::dirname(&absolute);
        if dir.is_empty() {
            sys::getcwd()
        } else {
            dir
        }
    };
    let root = trace::repo_root(&cwd)?;
    let hunks = trace::blame_hunks(&root, &sys::relpath(&absolute, &root))?;
    let store = store_or_none(args.str("store").as_deref());
    let mut resolver = Resolver::new(&root, store.as_deref());
    let mut links: indexmap::IndexMap<String, Option<Link>> = indexmap::IndexMap::new();
    for hunk in &hunks {
        if !links.contains_key(&hunk.commit) && hunk.commit != UNCOMMITTED {
            let link = resolve(&mut resolver, &hunk.commit)?;
            links.insert(hunk.commit.clone(), link);
        }
    }

    // One mark per request rather than per session, handed out in order of
    // first appearance, so A is always the first conversation you read.
    let mut marks: HashMap<(String, String), String> = HashMap::new();
    let mut legend: Vec<Map> = Vec::new();
    let mut mark_of: HashMap<String, String> = HashMap::new();
    for hunk in &hunks {
        if hunk.commit == UNCOMMITTED {
            mark_of.insert(hunk.commit.clone(), "+".into());
            continue;
        }
        let Some(Some(link)) = links.get(&hunk.commit) else {
            mark_of.insert(hunk.commit.clone(), "·".into());
            continue;
        };
        let key = (
            link.session_id.clone(),
            link.prompt
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| link.call.clone()),
        );
        if !marks.contains_key(&key) {
            let letter = mark(marks.len());
            marks.insert(key.clone(), letter.clone());
            let mut entry = object! {"mark" => letter};
            entry.extend(link.payload());
            entry.insert("commits".into(), Value::Array(Vec::new()));
            legend.push(entry);
        }
        let letter = marks[&key].clone();
        let entry = legend
            .iter_mut()
            .find(|e| e.get("mark").is_some_and(|m| m.is_str(&letter)))
            .expect("the mark was just added");
        if let Some(Value::Array(commits)) = entry.get_mut("commits") {
            if !commits.iter().any(|c| c.is_str(&hunk.commit)) {
                commits.push(hunk.commit.as_str().into());
            }
        }
        mark_of.insert(hunk.commit.clone(), letter);
    }

    let rows: Vec<Value> = hunks
        .iter()
        .map(|hunk| {
            let link = links.get(&hunk.commit).and_then(Option::as_ref);
            Value::Object(object! {
                "start" => hunk.start,
                "end" => hunk.end,
                "commit" => if hunk.commit == UNCOMMITTED { Value::Null } else { hunk.commit.as_str().into() },
                "summary" => hunk.summary.as_str(),
                "mark" => mark_of[&hunk.commit].as_str(),
                "conversation" => link.map_or(Value::Null, |l| Value::Object(l.payload())),
            })
        })
        .collect();

    let mut text = format!("{}\n", file);
    for hunk in &hunks {
        let span = if hunk.end > hunk.start {
            format!("{}-{}", hunk.start, hunk.end)
        } else {
            hunk.start.to_string()
        };
        if hunk.commit == UNCOMMITTED {
            text.push_str(&format!("  {}  uncommitted\n", right(&span, 9)));
            continue;
        }
        let link = links.get(&hunk.commit).and_then(Option::as_ref);
        text.push_str(&format!(
            "  {}  {}  {}\n",
            right(&span, 9),
            py::truncate(&hunk.commit, 7),
            py::truncate(&hunk.summary, 60)
        ));
        text.push_str(&format!(
            "  {}  {}  {}\n",
            right("", 9),
            left("", 7),
            link_text(link)
        ));
    }
    let linked = links.values().filter(|l| l.is_some()).count();
    text.push_str(&format!(
        "\n{} of {} commit(s) linked to a conversation; follow one up with: loch ask {}:<line>\n",
        linked,
        links.len(),
        file
    ));

    let legend_values: Vec<Value> = legend.into_iter().map(Value::Object).collect();
    let mut payload = object! {
        "file" => file.as_str(),
        "conversations" => legend_values.clone(),
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
        let mut lines: HashMap<usize, String> = HashMap::new();
        for hunk in &hunks {
            for (offset, line) in hunk.lines.iter().enumerate() {
                lines.insert(hunk.start + offset, line.clone());
            }
        }
        let page = blame_html::render(&file, &legend_values, &rows, &lines, visible.as_deref());
        let target = if html.is_empty() {
            sys::join(
                &sys::gettempdir(),
                &format!("loch-blame-{}.html", sys::basename(&file)),
            )
        } else {
            html
        };
        let bytes =
            py::encode_strict(&page).map_err(|_| Failure::Internal("unencodable page".into()))?;
        fs::write(&target, bytes).map_err(|e| Failure::Internal(sys::os_error(&e, &target)))?;
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
fn history_target(target: &str) -> std::result::Result<(String, Option<(usize, usize)>), Failure> {
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
    p.positional("target", Nargs::One).option("--store");
    let args = p.parse(argv)?;
    let (file, lines) = history_target(&args.str("target").unwrap_or_default())?;
    let absolute = sys::abspath(&file);
    let cwd = {
        let dir = sys::dirname(&absolute);
        if dir.is_empty() {
            sys::getcwd()
        } else {
            dir
        }
    };
    let root = trace::repo_root(&cwd)?;
    let relative = sys::relpath(&absolute, &root);
    if let Some((_, last)) = lines {
        // -L past the end is a fatal git error with a worse message.
        let length = trace::blame_hunks(&root, &relative)?
            .iter()
            .map(|h| h.end)
            .max()
            .unwrap_or(0);
        if last > length {
            return err(
                "usage",
                format!("{} has {} lines, not {}", file, length, last),
            );
        }
    }
    let changes = trace::history(&root, &relative, lines)?;
    let store = store_or_none(args.str("store").as_deref());
    let mut resolver = Resolver::new(&root, store.as_deref());

    let mut conversations: Vec<Map> = Vec::new();
    let mut keyed: HashMap<(String, String), usize> = HashMap::new();
    let mut entries = Vec::new();
    let mut text = String::new();
    for change in &changes {
        let link = resolve(&mut resolver, &change.commit)?;
        let when = sys::utc_seconds(change.time);
        entries.push(Value::Object(object! {
            "commit" => change.commit.as_str(),
            "time" => when.as_str(),
            "summary" => change.summary.as_str(),
            "conversation" => link.as_ref().map_or(Value::Null, |l| Value::Object(l.payload())),
        }));
        text.push_str(&format!(
            "  {}  {}  {}\n  {}  {}  {}\n",
            py::truncate(&change.commit, 7),
            &when[..10],
            py::truncate(&change.summary, 60),
            left("", 7),
            left("", 10),
            link_text(link.as_ref())
        ));
        let Some(link) = link else { continue };
        let key = (
            link.session_id.clone(),
            link.prompt
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| link.call.clone()),
        );
        let index = *keyed.entry(key).or_insert_with(|| {
            let mut entry = link.payload();
            entry.insert("commits".into(), Value::Array(Vec::new()));
            conversations.push(entry);
            conversations.len() - 1
        });
        if let Some(Value::Array(commits)) = conversations[index].get_mut("commits") {
            commits.push(change.commit.as_str().into());
        }
    }
    let span = match lines {
        None => String::new(),
        Some((a, b)) if a == b => format!(":{}", a),
        Some((a, b)) => format!(":{}-{}", a, b),
    };
    let header = format!(
        "{}{}: {} commit(s) from {} conversation(s), newest first\n",
        file,
        span,
        changes.len(),
        conversations.len()
    );
    let footer = if conversations.is_empty() {
        ""
    } else {
        "\nfollow one up with: loch ask <commit>\n"
    };
    Ok(Outcome::with(
        "log",
        object! {
            "file" => file.as_str(),
            "lines" => lines.map_or(Value::Null, |(a, b)| Value::Object(object! {"start" => a, "end" => b})),
            "conversations" => conversations.into_iter().map(Value::Object).collect::<Vec<_>>(),
            "commits" => entries,
        },
        format!("{}{}{}", header, text, footer),
    ))
}

fn resolve_commit(root: &str, target: &str) -> std::result::Result<String, Failure> {
    if let Some((file, line)) = target.rsplit_once(':') {
        if !file.is_empty() && py::is_digits(line) && Path::new(file).exists() {
            let relative = sys::relpath(&sys::abspath(file), root);
            let number: usize = line.parse().unwrap_or(0);
            for hunk in trace::blame_hunks(root, &relative)? {
                if hunk.start <= number && number <= hunk.end {
                    if hunk.commit == UNCOMMITTED {
                        return err("not-linked", format!("{} is not committed yet", target));
                    }
                    return Ok(hunk.commit);
                }
            }
            return err("usage", format!("{} has no line {}", file, line));
        }
    }
    let output = trace::git_unchecked(
        root,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{}^{{commit}}", target),
        ],
    );
    let commit = py::strip(&output);
    if commit.is_empty() {
        return err(
            "usage",
            format!("{} is neither a commit nor <file>:<line>", target),
        );
    }
    Ok(commit.into())
}

pub fn command_ask(argv: &[String]) -> Run {
    let mut p = parser("ask");
    p.positional("target", Nargs::One)
        .positional("question", Nargs::Optional)
        .flag("--at-commit", None)
        .flag("--no-launch", None)
        .option("--store");
    let args = p.parse(argv)?;
    let as_json = json_requested();
    let root = trace::repo_root(&sys::getcwd())?;
    let target = args.str("target").unwrap_or_default();
    let commit = resolve_commit(&root, &target)?;
    let store = store_or_none(args.str("store").as_deref());
    let mut resolver = Resolver::new(&root, store.as_deref());
    let Some(link) = resolve(&mut resolver, &commit)? else {
        return err(
            "not-linked",
            format!(
                "no conversation is recorded for {}",
                py::truncate(&commit, 7)
            ),
        );
    };
    let Some(transcript) = &link.transcript else {
        return err(
            "transcript-missing",
            format!(
                "session {} is linked but its transcript is neither in {} nor in the store",
                link.session_id,
                claude::projects_dir(None)
            ),
        );
    };
    let fork = trace::fork_session(
        transcript,
        &root,
        if args.flag("at_commit") {
            Some(link.call.as_str())
        } else {
            None
        },
        Some(&format!("loch ask {}", py::truncate(&commit, 7))),
    )
    .map_err(|e| match e {
        trace::ForkError::Trace(t) => Failure::from(t),
        trace::ForkError::Internal(m) => Failure::Internal(m),
    })?;

    let mut command = vec![
        "claude".to_string(),
        "--resume".into(),
        fork.session_id.clone(),
    ];
    if let Some(question) = args.str("question").filter(|q| !q.is_empty()) {
        command.push(question);
    }
    let payload = object! {
        "commit" => commit.as_str(),
        "conversation" => link.payload(),
        "fork" => object! {
            "sessionId" => fork.session_id.as_str(),
            "path" => fork.path.as_str(),
            "cwd" => fork.cwd.as_str(),
            "records" => fork.records,
        },
        "at" => if args.flag("at_commit") { "commit" } else { "end" },
        "launch" => strings(&command),
    };
    let text = format!(
        "forked session {}, which made {}, {}: {} records, as {}\n",
        py::truncate(&fork.origin_session, 8),
        py::truncate(&commit, 7),
        if args.flag("at_commit") {
            "just after it"
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
                root,
                command.join(" ")
            ),
        ));
    }
    eprint!("{}", text);
    exec_claude(&root, &command)
}

#[cfg(unix)]
fn exec_claude(root: &str, command: &[String]) -> Run {
    use std::os::unix::process::CommandExt;
    let _ = std::env::set_current_dir(root);
    let error = std::process::Command::new(&command[0])
        .args(&command[1..])
        .exec();
    Err(Failure::Internal(format!(
        "[Errno {}] {}: '{}'",
        error.raw_os_error().unwrap_or(0),
        sys::strerror(error.raw_os_error().unwrap_or(0)),
        command[0]
    )))
}

#[cfg(not(unix))]
fn exec_claude(root: &str, command: &[String]) -> Run {
    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .current_dir(root)
        .status()
        .map_err(|e| Failure::Internal(e.to_string()))?;
    std::process::exit(status.code().unwrap_or(1));
}

// -- backfill -------------------------------------------------------------------------

fn stderr_is_tty() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: isatty only inspects the descriptor.
        unsafe { libc::isatty(2) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub fn command_backfill(argv: &[String]) -> Run {
    let mut p = parser("backfill");
    // Branches rather than --all, which would walk refs/notes/loch too.
    p.option("--cwd").option_default("--revision", "--branches");
    let args = p.parse(argv)?;
    let root = trace::repo_root(
        &args
            .str("cwd")
            .filter(|c| !c.is_empty())
            .unwrap_or_else(sys::getcwd),
    )?;
    let revision = args.str("revision").unwrap_or_default();

    // A one-line counter for a person watching a terminal only; JSON mode and
    // a pipe both get nothing.
    let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let mut meter = |done: usize, total: usize, read: u64, size: u64| {
        let now = std::time::Instant::now();
        if done < total && now.duration_since(last).as_secs_f64() < 0.1 {
            return;
        }
        last = now;
        let percent = if size > 0 { 100 * read / size } else { 100 };
        eprint!(
            "\rscanning transcripts {}/{} ({} of {}, {}%)\x1b[K",
            done,
            total,
            format_bytes(read),
            format_bytes(size),
            percent
        );
        if done == total {
            eprintln!();
        }
    };
    let progress: Option<trace::Progress> = if json_requested() || !stderr_is_tty() {
        None
    } else {
        Some(&mut meter)
    };
    let linked = trace::backfill(&root, &revision, progress)?;
    let rows: Vec<Value> = linked
        .iter()
        .map(|(sha, call)| {
            Value::Object(object! {
                "commit" => sha.as_str(),
                "session" => call.session_id.as_str(),
                "call" => call.call.as_str(),
            })
        })
        .collect();
    let mut text: String = linked
        .iter()
        .map(|(sha, call)| {
            format!(
                "  {}  session {}\n",
                py::truncate(sha, 7),
                py::truncate(&call.session_id, 8)
            )
        })
        .collect();
    text.push_str(&format!("linked {} commit(s)\n", linked.len()));
    Ok(Outcome::with("backfill", object! {"linked" => rows}, text))
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
        "backfill" => command_backfill(argv),
        "hook" => command_hook(argv),
        "help" => command_help(argv),
        "version" => command_version(argv),
        _ => return None,
    })
}
