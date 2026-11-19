//! Attribution for history made before the hooks: edit records reconstructed
//! from what survives — the transcripts, and the commits the agent made.
//!
//! The hooks see every edit because they watch the files. Afterwards the files
//! are long past, so this works from two weaker sources and says which turns
//! it built (`backfilled`):
//!
//! 1. **The transcript's own edit calls.** An `Edit`, `Write` or `MultiEdit`
//!    carries the exact text it wrote, so its lines are its turn's exactly.
//!    What a transcript can't show is what a shell command did to a file.
//! 2. **The commits the agent made.** A commit's diff holds every line it
//!    added, however written, which is what catches shell edits. A commit is
//!    credited to a turn only on an exact match: exactly one Bash call in the
//!    transcript contains the commit's subject and brackets its author or
//!    committer time — or a git note from the old design names the call. A
//!    commit can hold several earlier turns' work, so its lines that a tool
//!    edit in the same session already explains are left to that edit.
//!
//! Turns the hooks recorded are never touched, and re-running replaces only
//! what an earlier backfill wrote.

use crate::attribution::{self, EditRecord, FileChange, Hunk, TurnRecord};
use crate::claude::{self, display_summary, AGENT};
use crate::git;
use crate::hook::{is_human_prompt, prompt_text};
use crate::json::{self, Map, Value};
use crate::snapshot;
use crate::store::Store;
use crate::sys;
use crate::trace::{self, content, py_str};
use std::collections::{HashMap, HashSet};
use std::fs;

/// How far a commit's time may sit outside the Bash call that made it: git
/// records whole seconds and transcripts milliseconds, and a hook is slow.
const CLOCK_SLACK: f64 = 2.0;

/// The tools whose inputs hold the text they wrote.
const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit"];

/// One turn of a transcript, as backfill reads it.
#[derive(Default)]
struct Turn {
    prompt_uuid: Option<String>,
    prompt: Option<String>,
    started: Option<String>,
    end_uuid: Option<String>,
    ended: Option<String>,
    /// (absolute path, text written) for each successful edit call.
    edits: Vec<(String, String)>,
    /// Bash calls that could have committed: (call id, command, start, end).
    calls: Vec<(String, String, f64, f64)>,
}

fn seconds(stamp: Option<&str>) -> Option<f64> {
    sys::iso_seconds(stamp?)
}

/// A transcript split at its human prompts, each turn holding what it wrote.
fn turns_of(path: &str) -> Vec<Turn> {
    let records = trace::read_records(path);
    // Results first, so an edit that failed contributes nothing.
    let mut failed: HashSet<String> = HashSet::new();
    let mut finished: HashMap<String, f64> = HashMap::new();
    for record in &records {
        let stamp = seconds(record.get("timestamp").and_then(Value::as_str));
        for block in content(record) {
            if !block.get("type").is_some_and(|t| t.is_str("tool_result")) {
                continue;
            }
            let id = py_str(block.get("tool_use_id"));
            if block.get("is_error").is_some_and(Value::truthy) {
                failed.insert(id.clone());
            }
            if let Some(stamp) = stamp {
                finished.insert(id, stamp);
            }
        }
    }

    let mut turns: Vec<Turn> = Vec::new();
    for record in &records {
        let Some(Value::Str(uuid)) = record.get("uuid") else {
            continue;
        };
        let stamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        if is_human_prompt(record) {
            turns.push(Turn {
                prompt_uuid: Some(uuid.clone()),
                prompt: display_summary(prompt_text(record).as_deref()),
                started: stamp.clone(),
                ..Turn::default()
            });
        }
        let Some(turn) = turns.last_mut() else {
            continue;
        };
        turn.end_uuid = Some(uuid.clone());
        turn.ended = stamp.clone();
        for block in content(record) {
            if !block.get("type").is_some_and(|t| t.is_str("tool_use")) {
                continue;
            }
            let id = py_str(block.get("id"));
            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
            let input = block.get("input").and_then(Value::as_object);
            if EDIT_TOOLS.contains(&name) && !failed.contains(&id) {
                if let Some(input) = input {
                    collect_edits(name, input, &mut turn.edits);
                }
            } else if name == "Bash" {
                let command = input.map(|i| py_str(i.get("command"))).unwrap_or_default();
                let started = seconds(stamp.as_deref());
                if let (true, Some(started), Some(&ended)) =
                    (command.contains("commit"), started, finished.get(&id))
                {
                    turn.calls.push((id, command, started, ended));
                }
            }
        }
    }
    turns
}

fn collect_edits(tool: &str, input: &Map, into: &mut Vec<(String, String)>) {
    let Some(path) = input.get("file_path").and_then(Value::as_str) else {
        return;
    };
    let text = |key: &str, from: &Map| from.get(key).and_then(Value::as_str).map(str::to_string);
    match tool {
        "Write" => into.extend(text("content", input).map(|t| (path.to_string(), t))),
        "Edit" => into.extend(text("new_string", input).map(|t| (path.to_string(), t))),
        "MultiEdit" => {
            for edit in input
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(edit) = edit.as_object() {
                    into.extend(text("new_string", edit).map(|t| (path.to_string(), t)));
                }
            }
        }
        _ => {}
    }
}

/// `path` relative to `root`, if it lies inside it.
fn inside(path: &str, root: &str) -> Option<String> {
    let rest = path.strip_prefix(root)?.strip_prefix('/')?;
    (!rest.is_empty()).then(|| rest.to_string())
}

/// The lines a commit added, per file, as hunks of line hashes.
fn commit_hunks(root: &str, commit: &str) -> Vec<FileChange> {
    let Ok(diff) = git::git(
        root,
        &[
            "show",
            "--format=",
            "--unified=0",
            "--no-color",
            "--no-renames",
            commit,
        ],
        None,
    ) else {
        return Vec::new();
    };
    let mut changes: Vec<FileChange> = Vec::new();
    let mut file: Option<String> = None;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").map(str::to_string);
            continue;
        }
        if line.starts_with("@@") {
            let start = line
                .split_whitespace()
                .find(|part| part.starts_with('+'))
                .and_then(|part| part[1..].split(',').next())
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0);
            if let Some(path) = &file {
                match changes.iter_mut().find(|c| &c.path == path) {
                    Some(change) => change.hunks.push(Hunk {
                        start,
                        lines: Vec::new(),
                    }),
                    None => changes.push(FileChange {
                        path: path.clone(),
                        hunks: vec![Hunk {
                            start,
                            lines: Vec::new(),
                        }],
                    }),
                }
            }
            continue;
        }
        if let (Some(added), Some(path)) = (line.strip_prefix('+'), &file) {
            if let Some(hunk) = changes
                .iter_mut()
                .find(|c| &c.path == path)
                .and_then(|c| c.hunks.last_mut())
            {
                hunk.lines.push(attribution::line_hash(added));
            }
        }
    }
    changes.retain(|c| c.hunks.iter().any(|h| !h.lines.is_empty()));
    changes
}

/// A commit in the repository, with what it was called and when.
struct Commit {
    sha: String,
    times: [f64; 2],
    subject: String,
}

fn commits(root: &str) -> Vec<Commit> {
    let listing = git::git(
        root,
        &["log", "--all", "--format=%H%x00%at%x00%ct%x00%s"],
        None,
    )
    .unwrap_or_default();
    listing
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, '\0').collect();
            let time = |s: &str| s.parse::<f64>().ok();
            if parts.len() != 4 || parts[3].is_empty() {
                return None;
            }
            Some(Commit {
                sha: parts[0].into(),
                times: [time(parts[1])?, time(parts[2])?],
                subject: parts[3].into(),
            })
        })
        .collect()
}

/// `(session, call) → commit` from the old design's notes, which named the
/// call exactly and so need no matching.
fn old_notes(root: &str) -> HashMap<(String, String), Vec<String>> {
    let mut found: HashMap<(String, String), Vec<String>> = HashMap::new();
    let listing = git::git_unchecked(root, &["notes", "--ref", "refs/notes/loch", "list"])
        .unwrap_or_default();
    for line in listing.lines() {
        let Some(commit) = line.split_whitespace().nth(1) else {
            continue;
        };
        let text = git::git_unchecked(root, &["notes", "--ref", "refs/notes/loch", "show", commit])
            .unwrap_or_default();
        if let Ok(note) = json::loads(text.trim()) {
            let (Some(session), Some(call)) = (
                note.get("session").and_then(Value::as_str),
                note.get("call").and_then(Value::as_str),
            ) else {
                continue;
            };
            found
                .entry((session.into(), call.into()))
                .or_default()
                .push(commit.into());
        }
    }
    found
}

/// The transcripts that might hold this project's history: every local
/// session whose working directory lay inside `root`.
fn transcripts_for(root: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let projects = claude::projects_dir(None);
    let Ok(dirs) = fs::read_dir(&projects) else {
        return found;
    };
    for dir in dirs.flatten() {
        let Ok(files) = fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path().to_string_lossy().into_owned();
            if !path.ends_with(".jsonl") {
                continue;
            }
            let Some(meta) = claude::read_session_meta(&path) else {
                continue;
            };
            let within = std::iter::once(&meta.cwd)
                .chain(&meta.other_cwds)
                .any(|cwd| cwd == root || inside(cwd, root).is_some());
            if within {
                found.push((meta.session_id, path));
            }
        }
    }
    found.sort();
    found
}

#[derive(Default)]
pub struct Backfilled {
    pub session: String,
    pub turns: usize,
    pub edit_lines: usize,
    pub commit_lines: usize,
    pub commits: usize,
}

/// Which commit each Bash call made, by the exact-match rule: the old
/// design's note if one names the call; otherwise every commit whose subject
/// the command contains and whose time the call brackets, credited only if no
/// other call — in any session — matches it too. A wrong link would send
/// `ask` into the wrong conversation and look exactly like a right one, so an
/// ambiguous commit stays unlinked.
fn credit(
    sessions: &[(String, Vec<Turn>)],
    history: &[Commit],
    notes: &HashMap<(String, String), Vec<String>>,
) -> HashMap<(String, String), Vec<String>> {
    let mut claims: HashMap<&str, HashSet<(String, String)>> = HashMap::new();
    let mut credited: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (session, turns) in sessions {
        for (call, command, started, ended) in turns.iter().flat_map(|t| t.calls.iter()) {
            let key = (session.clone(), call.clone());
            if let Some(noted) = notes.get(&key) {
                credited.insert(key, noted.clone());
                continue;
            }
            for commit in history {
                let brackets = commit
                    .times
                    .iter()
                    .any(|&t| started - CLOCK_SLACK <= t && t <= ended + CLOCK_SLACK);
                if brackets && command.contains(&commit.subject) {
                    claims.entry(&commit.sha).or_default().insert(key.clone());
                }
            }
        }
    }
    let noted: HashSet<&String> = credited.values().flatten().collect();
    let mut matched: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (sha, claimants) in claims {
        if claimants.len() == 1 && !noted.contains(&sha.to_string()) {
            let key = claimants.into_iter().next().expect("one claimant");
            matched.entry(key).or_default().push(sha.to_string());
        }
    }
    for (key, shas) in matched {
        credited.entry(key).or_default().extend(shas);
    }
    credited
}

/// Rebuilds the backfilled part of every session that worked in the
/// workspace at `cwd`, and publishes the records.
pub fn backfill(cwd: &str, store: Option<&dyn Store>) -> Vec<Backfilled> {
    let root = snapshot::workspace_root(cwd);
    let project = attribution::project_id(&root);
    let in_git = snapshot::has_git_marker(&root);
    let history = if in_git { commits(&root) } else { Vec::new() };
    let notes = if in_git {
        old_notes(&root)
    } else {
        HashMap::new()
    };
    let sessions: Vec<(String, Vec<Turn>)> = transcripts_for(&root)
        .into_iter()
        .map(|(session, path)| (session, turns_of(&path)))
        .collect();
    let credited = credit(&sessions, &history, &notes);

    let mut results = Vec::new();
    for (session, turns) in sessions {
        // Every line a tool edit in this session wrote: a commit's share is
        // what these don't already explain.
        let tool_lines: HashSet<String> = turns
            .iter()
            .flat_map(|t| t.edits.iter())
            .flat_map(|(_, text)| text.lines().map(attribution::line_hash).collect::<Vec<_>>())
            .collect();

        let mut built = Vec::new();
        let mut result = Backfilled {
            session: session.clone(),
            ..Backfilled::default()
        };
        for turn in turns {
            let mut files: Vec<FileChange> = Vec::new();
            for (file, text) in &turn.edits {
                let Some(relative) = inside(file, &root) else {
                    continue;
                };
                let lines: Vec<String> = text.lines().map(attribution::line_hash).collect();
                result.edit_lines += lines.len();
                push(&mut files, &relative, Hunk { start: 0, lines });
            }
            for (call, ..) in &turn.calls {
                let made = credited
                    .get(&(session.clone(), call.clone()))
                    .cloned()
                    .unwrap_or_default();
                for sha in made {
                    result.commits += 1;
                    for change in commit_hunks(&root, &sha) {
                        for hunk in change.hunks {
                            let lines: Vec<String> = hunk
                                .lines
                                .into_iter()
                                .filter(|h| !tool_lines.contains(h))
                                .collect();
                            if lines.is_empty() {
                                continue;
                            }
                            result.commit_lines += lines.len();
                            push(
                                &mut files,
                                &change.path,
                                Hunk {
                                    start: hunk.start,
                                    lines,
                                },
                            );
                        }
                    }
                }
            }
            if files.is_empty() {
                continue;
            }
            result.turns += 1;
            built.push(TurnRecord {
                prompt_uuid: turn.prompt_uuid,
                end_uuid: turn.end_uuid,
                started: turn.started,
                ended: turn.ended.unwrap_or_default(),
                prompt: turn.prompt,
                interrupted: false,
                backfilled: true,
                files,
            });
        }
        if built.is_empty() {
            continue;
        }
        let mut record = attribution::load_local(AGENT, &session).unwrap_or_else(|| EditRecord {
            agent: AGENT.into(),
            session: session.clone(),
            project: project.clone(),
            root: root.clone(),
            turns: Vec::new(),
        });
        // The hooks' own turns stand; an earlier backfill's are replaced.
        record.turns.retain(|t| !t.backfilled);
        let recorded: HashSet<Option<String>> =
            record.turns.iter().map(|t| t.prompt_uuid.clone()).collect();
        record.turns.extend(
            built
                .into_iter()
                .filter(|t| !recorded.contains(&t.prompt_uuid)),
        );
        record.turns.sort_by(|a, b| a.ended.cmp(&b.ended));
        if attribution::save_local(&record).is_ok() {
            if let Some(store) = store {
                let _ = attribution::publish(store, &record);
            }
        }
        results.push(result);
    }
    results
}

fn push(files: &mut Vec<FileChange>, path: &str, hunk: Hunk) {
    match files.iter_mut().find(|f| f.path == path) {
        Some(file) => file.hunks.push(hunk),
        None => files.push(FileChange {
            path: path.into(),
            hunks: vec![hunk],
        }),
    }
}
